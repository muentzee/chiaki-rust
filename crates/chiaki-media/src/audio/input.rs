// SPDX-License-Identifier: AGPL-3.0-only
//! Mikrofon-Eingabe — Port der Mic-Teile von `gui/src/streamsession.cpp`
//! (`InitMic`, `QueueMicData`, `DrainMicRingBuffer`, `ReadMic` und die
//! `ToggleMute`-Mute-Pflege) auf cpal/WASAPI + den bereits vorhandenen
//! [`OpusAudioEncoder`](crate::opus::OpusAudioEncoder)
//! (`OPUS_APPLICATION_RESTRICTED_LOWDELAY`, 40-Byte-Frames, 48 kHz).
//!
//! Pipeline (wie im C++): Capture-Callback → Mic-Ring (drop-oldest,
//! `mic_ring_*`) → Drain-Thread (`DrainMicRingBuffer`, im C++ eine
//! Queue-Invocation auf dem Qt-Loop) → Frame-Akkumulator
//! (`mic_buf`/`ReadMic`) → `chiaki_opus_encoder_frame` → Callback an den
//! Besitzer (der C++-Client sendet darin via `chiaki_session_send_mic_data`).
//!
//! Header wie streamsession.cpp:576:
//! `chiaki_audio_header_set(&audio_header, 2, 16, MICROPHONE_SAMPLES * 100,
//! MICROPHONE_SAMPLES)` — 2 Kanäle, 16 Bit, 48 kHz, 480 Samples/Frame (10 ms).
//! Ohne Speech-Processing öffnet das C++ das Mikrofon direkt mit 2 Kanälen
//! (`InitMic(2, rate)`); kommt das Gerät mit nur einem Kanal, wird hier wie
//! im SDL-Converter mono → stereo gemischt (siehe `map_channels`).
//!
//! **Speech-Processing (Speex) ist nicht portiert** — siehe Moduldoku in
//! `super`. Der Mute-Zustand ist das `SDL_PauseAudioDevice(audio_in, muted)`-
//! Äquivalent: stummgeschaltet verwirft der Capture-Callback die Samples,
//! sodass kein Material angeschrieben wird (`ReadMic`: "Don't send mic data
//! if muted").

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use chiaki_core::audio::AudioHeader;
use chiaki_core::{ChiakiError, ChiakiResult};
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, Stream, StreamConfig};

use crate::opus::OpusAudioEncoder;

use super::output::DEFAULT_AUDIO_BUFFER_SIZE;
use super::{map_channels, pick_stream_config, resolve_device, Direction, MICROPHONE_SAMPLES};

/// Der Besitzer (Session) bekommt jedes fertige, encodierte Opus-Paket
/// (immer exakt 40 Bytes @ 48 kHz — der C++-Vertrag aus `opus_frame_buf`).
pub type MicFrameCallback = Box<dyn FnMut(&[u8]) + Send + 'static>;

// ---------------------------------------------------------------------------
// Mic-Ring (Port von mic_ring_* — Mutex-geschützt wie im C++: der
// Capture-Callback nimmt den Lock wie QueueMicData den QMutexLocker)
// ---------------------------------------------------------------------------

struct MicRing {
    buf: Vec<i16>,
    read: usize,
    write: usize,
    fill: usize,
    overflow_logged: bool,
}

impl MicRing {
    /// Feste Kapazität in Samples; die Produktion übergibt
    /// `frame_samples * 8` (C++: `mic_ring_buf.resize(mic_buf.size_bytes * 8)`
    /// — 8 Mikrofon-Frames).
    fn new(capacity_samples: usize) -> Self {
        MicRing {
            buf: vec![0i16; capacity_samples],
            read: 0,
            write: 0,
            fill: 0,
            overflow_logged: false,
        }
    }

    /// Port von `QueueMicData` (Ring-Teil): drop-oldest bei Überlauf,
    /// Warnung einmalig ("Mic ring buffer overflow, dropping stale captured
    /// audio"), `data >= capacity` → nur das Ende behalten.
    /// Rückgabe: `true`, wenn Samples angekommen sind (→ Drain wecken).
    fn push(&mut self, data: &[i16]) -> bool {
        let capacity = self.buf.len();
        if capacity == 0 || data.is_empty() {
            return false;
        }
        let mut data = data;
        if data.len() >= capacity {
            self.read = 0;
            self.write = 0;
            self.fill = 0;
            data = &data[data.len() - capacity..];
        } else if data.len() > capacity - self.fill {
            let drop = data.len() - (capacity - self.fill);
            self.read = (self.read + drop) % capacity;
            self.fill -= drop;
            if !self.overflow_logged {
                tracing::warn!("Mic ring buffer overflow, dropping stale captured audio");
                self.overflow_logged = true;
            }
        }
        let first = data.len().min(capacity - self.write);
        self.buf[self.write..self.write + first].copy_from_slice(&data[..first]);
        if data.len() > first {
            self.buf[..data.len() - first].copy_from_slice(&data[first..]);
        }
        self.write = (self.write + data.len()) % capacity;
        self.fill += data.len();
        true
    }

    /// Port der Entnahme in `DrainMicRingBuffer`: nimmt höchstens
    /// `min(fill, capacity - read)` als ein Stück heraus; bei leerem Ring
    /// wird die Warn-Flagge zurückgesetzt (wie im C++).
    fn pop_chunk(&mut self) -> Option<Vec<i16>> {
        if self.fill == 0 {
            self.overflow_logged = false;
            return None;
        }
        let capacity = self.buf.len();
        let chunk_size = self.fill.min(capacity - self.read);
        let chunk = self.buf[self.read..self.read + chunk_size].to_vec();
        self.read = (self.read + chunk_size) % capacity;
        self.fill -= chunk_size;
        Some(chunk)
    }
}

/// Geteilter Zustand zwischen Capture-Callback und Drain-Thread.
struct MicShared {
    ring: Mutex<MicRing>,
    cond: Condvar,
    /// C++ `muted` (start: true; `settings/start_mic_unmuted` invertiert).
    muted: AtomicBool,
    /// C++ `mic_active` — Session-seitig ein-/ausgeschaltet.
    active: AtomicBool,
    /// Beendet den Drain-Thread (Drop).
    stop: AtomicBool,
}

impl MicShared {
    /// Wartet, bis Ring-Material anliegt; `None` beim Stop.
    fn pop_chunk_blocking(&self) -> Option<Vec<i16>> {
        let mut ring = self.ring.lock().ok()?;
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return None;
            }
            if let Some(chunk) = ring.pop_chunk() {
                return Some(chunk);
            }
            ring = self.cond.wait(ring).ok()?;
        }
    }
}

// ---------------------------------------------------------------------------
// Frame-Akkumulator (Port von mic_buf + ReadMic)
// ---------------------------------------------------------------------------

/// Akkumuliert Samples bis zur Frame-Größe und liefert jeden vollen Frame —
/// Struktur 1:1 wie `ReadMic` (mic_buf.buf / mic_buf.current_byte).
struct MicAccumulator {
    /// C++ `mic_buf.buf` — `frame_samples * channels` Samples.
    buf: Vec<i16>,
    /// C++ `mic_buf.current_byte`, hier in Samples.
    filled: usize,
}

impl MicAccumulator {
    fn new(frame_samples: usize, channels: u16) -> Self {
        MicAccumulator {
            buf: vec![0i16; frame_samples * channels as usize],
            filled: 0,
        }
    }

    /// Liefert alle vollständig akkumulierten Frames (eigene Kopien, damit
    /// der Akkumulator direkt weiterschreiben darf — der C++ kopiert die
    /// Frames ebenfalls in mic_buf, inhaltlich identisch).
    fn feed(&mut self, samples: &[i16]) -> Vec<Vec<i16>> {
        let frame = self.buf.len();
        if frame == 0 {
            return Vec::new();
        }
        let mut frames = Vec::new();
        let mut rest = samples;

        let left = frame - self.filled;
        if rest.len() < left {
            self.buf[self.filled..self.filled + rest.len()].copy_from_slice(rest);
            self.filled += rest.len();
            return frames;
        }
        self.buf[self.filled..].copy_from_slice(&rest[..left]);
        frames.push(self.buf.clone());
        rest = &rest[left..];

        // C++: "frames = bytes_read / mic_buf.size_bytes"-Schleife.
        let full = rest.len() / frame;
        for i in 0..full {
            frames.push(rest[i * frame..(i + 1) * frame].to_vec());
        }
        rest = &rest[full * frame..];

        self.filled = rest.len();
        if self.filled == 0 {
            return frames;
        }
        self.buf[..self.filled].copy_from_slice(rest);
        frames
    }
}

// ---------------------------------------------------------------------------
// AudioInput
// ---------------------------------------------------------------------------

/// Mikrofon-Eingabe inkl. Opus-Encoding.
///
/// **Nicht `Send`** (cpal::Stream): Erzeugen und Droppen müssen auf demselben
/// Thread passieren. Der Drain-Thread (Encodierung + Besitzer-Callback) wird
/// im Konstruktor gestartet und im [`Drop`] wieder beigetreten.
pub struct AudioInput {
    // RAII: hält den cpal-Stream am Leben (Drop stoppt den Callback).
    #[allow(dead_code)]
    stream: Stream,
    shared: Arc<MicShared>,
    device_name: String,
    header: AudioHeader,
    drain: Option<JoinHandle<()>>,
}

impl AudioInput {
    /// Öffnet das Mikrofon und startet Capture + Encode-Pipeline.
    ///
    /// * `device` — Gerätename aus [`AudioInput::devices`], `None`/`""` =
    ///   Standardgerät (Fallback auf den Default wie in `InitMic`).
    /// * `buffer_size` — roher Wert von `settings/audio_buffer_size` (**Bytes**
    ///   S16-PCM wie im C++; `0` = [`DEFAULT_AUDIO_BUFFER_SIZE`]). Wie im C++
    ///   (`spec.samples = audio_buffer_size / 4`) wird daraus nur die
    ///   angeforderte Geräte-Puffergröße abgeleitet; die Ringkapazität ist
    ///   fest `8 * Mic-Frame` (C++: `mic_ring_buf.resize(mic_buf.size_bytes * 8)`).
    /// * `start_muted` — C++ `muted = true` plus `start_mic_unmuted`-Setting:
    ///   die Session übergibt hier `!settings.start_mic_unmuted`.
    /// * `on_frame` — wird aus dem Drain-Thread für jedes encodierte Paket
    ///   gerufen (40 Bytes; der Besitzer sendet es als Mic-Daten an die
    ///   Konsole).
    pub fn new(
        device: Option<&str>,
        buffer_size: u32,
        start_muted: bool,
        on_frame: impl FnMut(&[u8]) + Send + 'static,
    ) -> ChiakiResult<Self> {
        // streamsession.cpp:576 — 2ch/16bit/48kHz/480 Samples (10 ms).
        let header = AudioHeader {
            channels: 2,
            bits: 16,
            rate: 48_000,
            frame_size: MICROPHONE_SAMPLES,
            unknown: 0,
        };
        let frame_samples = header.frame_size as usize * header.channels as usize;

        // C++: chiaki_opus_encoder_header — schlägt das fehl, bricht der
        // Mic-Start ab ("Microphone initialization failed, leaving microphone
        // muted" macht die Session daraus).
        let mut encoder = OpusAudioEncoder::new();
        encoder.set_header(header).map_err(|_| ChiakiError::Unknown)?;

        let host = cpal::default_host();
        let (device, device_name) = resolve_device(Direction::Input, &host, device)?;

        // C++ InitMic: spec.samples = audio_buffer_size / 4 (stereo S16).
        let buffer_size = if buffer_size == 0 {
            DEFAULT_AUDIO_BUFFER_SIZE
        } else {
            buffer_size
        };
        let requested_frames = (buffer_size / 4).max(1);

        let (config, sample_format, converted) = pick_stream_config(
            &device,
            Direction::Input,
            header.rate,
            u16::from(header.channels),
            requested_frames,
        )?;

        if converted {
            // C++ InitMic-Logzeile, falls SDL konvertieren musste.
            tracing::warn!(
                "Microphone '{}' opened with converted format {:?}, {} channels @ {} Hz (requested {:?}, {} channels @ {} Hz)",
                device_name,
                sample_format,
                config.channels,
                config.sample_rate.0,
                SampleFormat::I16,
                header.channels,
                header.rate
            );
        }

        let shared = Arc::new(MicShared {
            ring: Mutex::new(MicRing::new(frame_samples * 8)),
            cond: Condvar::new(),
            muted: AtomicBool::new(start_muted),
            active: AtomicBool::new(true),
            stop: AtomicBool::new(false),
        });

        // Capture-Stream: Format → i16 → Ring (Port von QueueMicData als
        // SDL-Callback). Fixed Buffer Size ist im WASAPI-Shared-Mode nicht
        // überall möglich — dann der Engine-Default.
        let stream = build_capture_stream(
            &device,
            &config,
            sample_format,
            make_push_closure(Arc::clone(&shared), header, config.channels),
        )
        .or_else(|err| {
            tracing::warn!(
                "Microphone: requested buffer size {} frames rejected ({}), retrying with engine default",
                requested_frames,
                err
            );
            let config = StreamConfig {
                channels: config.channels,
                sample_rate: config.sample_rate,
                buffer_size: BufferSize::Default,
            };
            build_capture_stream(
                &device,
                &config,
                sample_format,
                make_push_closure(Arc::clone(&shared), header, config.channels),
            )
        })
        .map_err(|_| ChiakiError::Unknown)?;

        // Drain-Thread (Port von DrainMicRingBuffer + ReadMic + Encode).
        let drain_shared = Arc::clone(&shared);
        let drain = std::thread::Builder::new()
            .name("chiaki-mic-drain".to_string())
            .spawn(move || {
                let mut on_frame = on_frame;
                let mut accumulator =
                    MicAccumulator::new(header.frame_size as usize, u16::from(header.channels));
                while let Some(chunk) = drain_shared.pop_chunk_blocking() {
                    if !drain_shared.active.load(Ordering::Relaxed) {
                        continue; // C++: Ring-Inhalt verwerfen, nicht encodieren
                    }
                    if drain_shared.muted.load(Ordering::Relaxed) {
                        // C++ ReadMic: "Don't send mic data if muted"
                        continue;
                    }
                    for frame in accumulator.feed(&chunk) {
                        match encoder.encode_frame(&frame) {
                            Ok(packet) => on_frame(packet),
                            // Logtext/Verwerfen übernimmt OpusAudioEncoder.
                            Err(_) => continue,
                        }
                    }
                }
            })
            .map_err(|_| {
                tracing::error!("Could not start mic drain thread");
                ChiakiError::Thread
            })?;

        // Capture läuft sofort; stumm geschaltetes Material wird im Callback
        // verworfen (SDL_PauseAudioDevice(audio_in, muted)-Äquivalent).
        stream.play().map_err(|_| ChiakiError::Unknown)?;

        tracing::info!(
            "Microphone '{}' opened with {} channels @ {} Hz, buffer size {}",
            device_name,
            config.channels,
            config.sample_rate.0,
            requested_frames * u32::from(config.channels) * 2
        );

        Ok(AudioInput {
            stream,
            shared,
            device_name,
            header,
            drain: Some(drain),
        })
    }

    /// C++ `muted`-Pflege aus `ToggleMute`: stumm → Capture-Material wird
    /// verworfen (SDL_PauseAudioDevice-Äquivalent) und keine Frames mehr
    /// encodiert/gesendet.
    pub fn set_muted(&self, muted: bool) {
        self.shared.muted.store(muted, Ordering::Relaxed);
    }

    pub fn is_muted(&self) -> bool {
        self.shared.muted.load(Ordering::Relaxed)
    }

    /// C++ `mic_active`: false verwirft Capture- und Drain-Material
    /// (Session-Start/-Stopp).
    pub fn set_active(&self, active: bool) {
        self.shared.active.store(active, Ordering::Relaxed);
    }

    pub fn is_active(&self) -> bool {
        self.shared.active.load(Ordering::Relaxed)
    }

    /// Der feste Mikrofon-AudioHeader (2ch/16bit/48kHz/480 Samples).
    pub fn header(&self) -> AudioHeader {
        self.header
    }

    /// Aufgelöster Gerätename (nach Fallback auf den Default).
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Mikrofon-Geräte-Liste für die Settings-UI (Default zuerst).
    pub fn devices() -> Vec<String> {
        super::devices(Direction::Input)
    }
}

impl std::fmt::Debug for AudioInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioInput")
            .field("device", &self.device_name)
            .field("muted", &self.is_muted())
            .field("active", &self.is_active())
            .finish()
    }
}

impl Drop for AudioInput {
    fn drop(&mut self) {
        // Drain-Thread beenden, bevor der Stream (und damit die Callbacks)
        // wegfällt.
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.cond.notify_all();
        if let Some(drain) = self.drain.take() {
            let _ = drain.join();
        }
        // self.stream fällt danach weg → cpal stoppt den Capture-Callback
        // (C++: SDL_CloseAudioDevice).
    }
}

/// Erzeugt den Capture-Callback: gated auf stop/active/muted
/// (SDL_PauseAudioDevice-Äquivalent), mischt die Geräte-Kanäle auf den
/// 2-Kanal-Mic-Header und schreibt in den Ring (QueueMicData).
fn make_push_closure(
    shared: Arc<MicShared>,
    header: AudioHeader,
    dev_channels: u16,
) -> impl FnMut(&[i16]) + Send + 'static {
    // Wiederverwendeter Puffer — keine Allokation im Echtzeit-Callback.
    let mut stereo: Vec<i16> = Vec::new();
    move |pcm: &[i16]| {
        if shared.stop.load(Ordering::Relaxed)
            || !shared.active.load(Ordering::Relaxed)
            || shared.muted.load(Ordering::Relaxed)
        {
            return;
        }
        let dev_ch = usize::from(dev_channels.max(1));
        let frames = pcm.len() / dev_ch;
        stereo.clear();
        stereo.resize(frames * header.channels as usize, 0);
        map_channels(
            pcm,
            dev_channels,
            frames,
            &mut stereo,
            u16::from(header.channels),
            frames,
        );
        let pushed = shared
            .ring
            .lock()
            .map(|mut ring| ring.push(&stereo))
            .unwrap_or(false);
        if pushed {
            shared.cond.notify_one();
        }
    }
}

// ---------------------------------------------------------------------------
// Capture-Stream-Bau (Geräteformat → i16)
// ---------------------------------------------------------------------------

/// Baut den Eingabe-Stream im passenden Sample-Format; die Gerätedaten
/// werden nach i16 konvertiert und an `on_pcm` (interleaved, Geräte-Kanäle)
/// übergeben.
fn build_capture_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    sample_format: SampleFormat,
    on_pcm: impl FnMut(&[i16]) + Send + 'static,
) -> Result<Stream, cpal::BuildStreamError> {
    let error_callback = move |err: cpal::StreamError| {
        tracing::error!("Microphone stream error: {err}");
    };
    macro_rules! build {
        ($t:ty, $convert:expr) => {{
            // Wiederverwendeter Konvertierungspuffer (RT-Callback).
            let mut conv: Vec<i16> = Vec::new();
            let mut on_pcm = on_pcm;
            device.build_input_stream(
                config,
                move |pcm: &[$t], _info| {
                    conv.clear();
                    conv.extend(pcm.iter().map(|&s| $convert(s)));
                    on_pcm(&conv);
                },
                error_callback,
                None,
            )
        }};
    }
    match sample_format {
        SampleFormat::I16 => build!(i16, |s: i16| s),
        SampleFormat::F32 => build!(f32, |s: f32| (s * 32767.0).clamp(-32768.0, 32767.0) as i16),
        SampleFormat::U16 => build!(u16, |s: u16| (s ^ 0x8000) as i16),
        SampleFormat::U8 => build!(u8, |s: u8| (i16::from(s) - 128) << 8),
        other => {
            tracing::error!("Unsupported input sample format: {other:?}");
            Err(cpal::BuildStreamError::StreamConfigNotSupported)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn mic_header() -> AudioHeader {
        AudioHeader {
            channels: 2,
            bits: 16,
            rate: 48_000,
            frame_size: MICROPHONE_SAMPLES,
            unknown: 0,
        }
    }

    /// Opus-Vertrag aus dem C++ (opus_frame_buf): RESTRICTED_LOWDELAY,
    /// 48 kHz, 480 Samples/Frame → **immer exakt 40 Bytes**.
    #[test]
    fn opus_encoder_produces_40_byte_frames_at_48k() {
        crate::test_setup::reference_dlls();
        let header = mic_header();
        let mut encoder = OpusAudioEncoder::new();
        encoder.set_header(header).expect("encoder init");

        // 10 ms Stereo (480 Samples/Kanal = 960 interleaved).
        let pcm: Vec<i16> = (0..header.frame_size as usize)
            .flat_map(|n| {
                let s =
                    (2.0 * std::f64::consts::PI * 440.0 * n as f64 / 48_000.0).sin() * 8000.0;
                [s as i16, s as i16]
            })
            .collect();
        let packet = encoder.encode_frame(&pcm).expect("encode");
        assert_eq!(packet.len(), 40);
        assert_eq!(header.frame_size, 480);
        assert_eq!(header.rate, 48_000);
        assert_eq!(header.channels, 2);
    }

    #[test]
    fn mic_ring_roundtrip_across_wraparound() {
        let mut ring = MicRing::new(8);
        assert!(ring.push(&[1, 2, 3, 4]));
        assert!(ring.push(&[5, 6, 7, 8]));
        assert_eq!(ring.pop_chunk(), Some(vec![1, 2, 3, 4, 5, 6, 7, 8]));
        // write steht jetzt auf dem Wrap-Punkt — weiterfüllen und holen.
        assert!(ring.push(&[9, 10, 11, 12]));
        assert_eq!(ring.pop_chunk(), Some(vec![9, 10, 11, 12]));
        assert_eq!(ring.pop_chunk(), None);
    }

    #[test]
    fn mic_ring_overflow_drops_oldest_and_warns_once() {
        let mut ring = MicRing::new(8);
        ring.push(&[1, 2, 3, 4]);
        ring.push(&[5, 6, 7, 8]);
        ring.push(&[9, 10]); // passt nicht mehr → älteste (1,2) fallen raus
        assert_eq!(ring.pop_chunk(), Some(vec![3, 4, 5, 6, 7, 8]));
        assert_eq!(ring.pop_chunk(), Some(vec![9, 10]));
        assert!(ring.overflow_logged);
        // Nach Leerlauf setzt das C++ die Warn-Flagge zurück.
        assert!(ring.pop_chunk().is_none());
        assert!(!ring.overflow_logged);
    }

    #[test]
    fn mic_ring_data_larger_than_capacity_keeps_tail() {
        let mut ring = MicRing::new(4);
        ring.push(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(ring.pop_chunk(), Some(vec![6, 7, 8, 9]));
    }

    #[test]
    fn accumulator_frames_at_exact_boundaries() {
        // 480 Samples/Frame, 2 Kanäle → 960 Samples je Mic-Frame.
        let mut acc = MicAccumulator::new(480, 2);
        assert!(acc.feed(&vec![0i16; 500]).is_empty());
        assert_eq!(acc.filled, 500);
        let frames = acc.feed(&vec![1i16; 1420]); // 500 + 1420 = 1920 = 2 Frames
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().all(|f| f.len() == 960));
        assert_eq!(acc.filled, 0);

        let frames = acc.feed(&vec![2i16; 960]);
        assert_eq!(frames.len(), 1);

        assert!(acc.feed(&vec![3i16; 500]).is_empty());
        assert_eq!(acc.filled, 500);
        let frames = acc.feed(&vec![4i16; 460]); // füllt den Rest auf
        assert_eq!(frames.len(), 1);
        assert_eq!(acc.filled, 0);
    }

    #[test]
    fn accumulator_retains_partial_tail_like_readmic() {
        let mut acc = MicAccumulator::new(2, 1); // winziger Frame zum Testen
        assert!(acc.feed(&[1]).is_empty());
        let frames = acc.feed(&[2, 3, 4, 5]);
        assert_eq!(frames.len(), 2); // [1,2] und [3,4]
        assert_eq!(acc.filled, 1); // [5] liegt vor
        let frames = acc.feed(&[6]);
        assert_eq!(frames.len(), 1); // [5,6]
        assert_eq!(acc.filled, 0);
    }

    /// Geräte-Enumeration — `#[ignore]`, damit CI ohne Audio-Gerät grün
    /// bleibt (diese Maschine HAT Audio: lokal mit `--ignored` laufen).
    #[test]
    #[ignore = "benoetigt Audio-Geraete (Windows-Host)"]
    fn enumerates_output_and_input_devices() {
        let outputs = super::super::AudioOutput::devices();
        let inputs = AudioInput::devices();
        assert!(!inputs.is_empty(), "kein Eingabegeraet gefunden");
        println!("output devices: {outputs:?}");
        println!("input devices: {inputs:?}");
    }

    /// Mikrofon-Stream gegen das Standardgerät: Aufbau, kurzes Laufen,
    /// sauberer Stop. `#[ignore]` wie oben; prüft nur den sauberen
    /// Lebenszyklus (ein OS-seitig stummgeschaltetes Mikrofon liefert
    /// keine Frames — das zählt nicht als Fehler).
    #[test]
    #[ignore = "benoetigt ein Mikrofon (Windows-Host)"]
    fn input_stream_starts_and_stops_cleanly() {
        crate::test_setup::reference_dlls();
        let frames = Arc::new(AtomicUsize::new(0));
        let frames_cb = Arc::clone(&frames);
        let input = AudioInput::new(None, 0, false, move |packet| {
            assert_eq!(packet.len(), 40);
            frames_cb.fetch_add(1, Ordering::Relaxed);
        })
        .expect("AudioInput auf Standardgerät");
        input.set_muted(false);
        std::thread::sleep(std::time::Duration::from_millis(300));
        let received = frames.load(Ordering::Relaxed);
        println!("mic frames in 300 ms: {received} (Erwartung ~30 bei 10-ms-Frames)");
        drop(input); // sauberer Stop inkl. Drain-Join
        assert!(true);
    }
}
