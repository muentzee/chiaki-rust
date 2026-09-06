// SPDX-License-Identifier: AGPL-3.0-only
//! Audio-Ausgabe — Port der SDL-Audio-Teile von `gui/src/streamsession.cpp`
//! (`InitAudio`, `PushAudioFrame`, `QueueAudioOutData`,
//! `DrainAudioOutRingBuffer`, `AudioOutDrainThreadMain`) auf cpal/WASAPI.
//!
//! Architektur-Unterschied zum C++: SDL2 hatte zwei Stufen (Session-Thread
//! füllt einen Ring, ein Drain-Thread kopiert in die SDL-Queue und wartet bei
//! Rückstand 1 ms statt 4 ms — REWORK.md Abschnitt 2.2, "Backpressure"), und
//! SDLs interner Callback verbrauchte die Queue. cpal liefert uns den
//! Echtzeit-Callback selbst: die Session pusht dekodierte Frames in einen
//! festen Ring ([`SampleRing`]), der Audio-Thread zieht sie direkt heraus.
//! Der Drain-Thread samt Warteschleife entfällt damit; die wirksame
//! Backpressure-Wartezeit ist 0 ms (jede Probe ist spätestens nach einer
//! Callback-Periode verbraucht), das Überlaufverhalten bleibt identisch:
//! älteste Samples werden gedroppt (`QueueAudioOutData`), bei leerem Ring
//! spielt der Callback Stille (SDL-Unterrun-Verhalten).
//!
//! Alle von `settings` kommenden Größen sind wie im C++ **Bytes** von
//! S16-PCM: Ring = `8 * audio_buffer_size`, Latenzgrenze = `3 *
//! audio_buffer_size` ("Audio queue exceeded latency threshold"), Default
//! 9600 = 50 ms @ 48 kHz stereo.

use std::sync::atomic::{AtomicU64, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use chiaki_core::{ChiakiError, ChiakiResult};
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, Stream, StreamConfig};

use super::{map_channels, pick_stream_config, resolve_device, Direction};

/// C++: `Settings::GetAudioBufferSizeDefault()` (settings.cpp) — Default für
/// `settings/audio_buffer_size` (Bytes S16-PCM; 9600 B = 2400 Frames = 50 ms
/// @ 48 kHz stereo).
pub const DEFAULT_AUDIO_BUFFER_SIZE: u32 = 9600;

/// C++: `SDL_MIX_MAXVOLUME` — Volume-Skala der Einstellung `settings/audio_volume`
/// (0..=128, Default 128). 128 = unverstärkt (C++: memcpy-Zweig).
pub const SDL_MIX_MAXVOLUME: u32 = 128;

// ---------------------------------------------------------------------------
// SampleRing — fester Ring mit Drop-Oldest (Port von audio_out_ring_*)
// ---------------------------------------------------------------------------

/// Fester Ring aus interleaved i16-Samples. Genau **ein** Produzent
/// (Session-Thread, `push`) und **ein** Konsument (cpal-Audio-Callback,
/// `pull`). Überlauf verwirft die ältesten Samples (wie `QueueAudioOutData`:
/// `audio_out_ring_read_pos += bytes_to_drop`), die Warnung wird wie
/// `audio_out_overflow_logged` nur einmal bis zum nächsten Leerlauf
/// ausgegeben.
///
/// Der Zustand liegt — wie im C++ (`QMutexLocker locker(&audio_out_ring_mutex)`)
/// — hinter einem Mutex; die kritischen Abschnitte sind reine Kopier-
/// Operationen im Mikrosekundenbereich. Zähler fürs Stats-HUD laufen über
/// Atomics, damit sie ohne Lock lesbar sind.
struct SampleRing {
    state: Mutex<RingState>,
    /// In den Ring geschobene Samples (Producer-Gesamtmenge, Messung P1).
    pushed: AtomicU64,
    /// Vom Callback entnommene Samples (Konsument-Gesamtmenge, Messung P1).
    pulled: AtomicU64,
    /// Samples, die durch Überlauf/Latenz-Clear verworfen wurden (Stats-HUD).
    dropped: AtomicU64,
    /// Callbacks, bei denen der Ring leer war und Stille gespielt wurde.
    underflows: AtomicU64,
    /// Ausgelöste 3×-Latenz-Clears ("queue exceeded latency threshold").
    clears: AtomicU64,
}

struct RingState {
    buf: Vec<i16>,
    read: usize,
    write: usize,
    fill: usize,
    overflow_warned: bool,
}

impl SampleRing {
    fn new(capacity_samples: usize) -> Self {
        SampleRing {
            state: Mutex::new(RingState {
                buf: vec![0i16; capacity_samples],
                read: 0,
                write: 0,
                fill: 0,
                overflow_warned: false,
            }),
            pushed: AtomicU64::new(0),
            pulled: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            underflows: AtomicU64::new(0),
            clears: AtomicU64::new(0),
        }
    }

    /// Aktuelle Füllmenge in Samples.
    fn fill(&self) -> u64 {
        self.state
            .lock()
            .map(|state| state.fill as u64)
            .unwrap_or(0)
    }

    /// Port von `QueueAudioOutData` + dem 3×-Latenz-Guard aus
    /// `PushAudioFrame`/`DrainAudioOutRingBuffer`:
    ///
    /// 1. `fill > 3 * buffer` → den kompletten Rückstand verwerfen
    ///    ("Audio queue exceeded latency threshold, clearing queued audio"),
    /// 2. Überlauf: älteste Samples droppen, Warnung einmalig
    ///    ("Audio output ring overflow, dropping stale queued audio"),
    /// 3. `data >= capacity`: nur das Ende behalten, Ring neu beginnen.
    fn push(&self, data: &[i16], clear_threshold: u64) {
        if data.is_empty() {
            return;
        }
        self.pushed.fetch_add(data.len() as u64, Ordering::Relaxed);
        let Ok(mut state) = self.state.lock() else {
            return; // vergifteter Lock: Audio wegwerfen, nicht blockieren
        };
        let capacity = state.buf.len();
        if capacity == 0 {
            return;
        }

        // C++ PushAudioFrame/DrainAudioOutRingBuffer: Queue-Latenz
        // überschritten → sämtliche angestauten Samples wegwerfen und mit
        // dem aktuellen Frame neu ansetzen.
        if state.fill as u64 > clear_threshold {
            self.dropped
                .fetch_add(state.fill as u64, Ordering::Relaxed);
            state.read = 0;
            state.write = 0;
            state.fill = 0;
            state.overflow_warned = false;
            self.clears.fetch_add(1, Ordering::Relaxed);
            tracing::warn!("Audio queue exceeded latency threshold, clearing queued audio");
        }

        // C++ QueueAudioOutData: passt der neue Frame nicht, fliegt das
        // älteste Material raus ("dropping stale queued audio").
        let mut data = data;
        if data.len() >= capacity {
            self.dropped.fetch_add(state.fill as u64, Ordering::Relaxed);
            state.read = 0;
            state.write = 0;
            state.fill = 0;
            state.overflow_warned = false;
            data = &data[data.len() - capacity..];
        } else if data.len() > capacity - state.fill {
            let drop = data.len() - (capacity - state.fill);
            state.read = (state.read + drop) % capacity;
            state.fill -= drop;
            self.dropped.fetch_add(drop as u64, Ordering::Relaxed);
            if !state.overflow_warned {
                tracing::warn!("Audio output ring overflow, dropping stale queued audio");
                state.overflow_warned = true;
            }
        }

        let start = state.write;
        let first = data.len().min(capacity - start);
        state.buf[start..start + first].copy_from_slice(&data[..first]);
        if data.len() > first {
            state.buf[..data.len() - first].copy_from_slice(&data[first..]);
        }
        state.write = (start + data.len()) % capacity;
        state.fill += data.len();
    }

    /// Port der Entnahme-Seite von `DrainAudioOutRingBuffer` (hier als
    /// Echtzeit-Callback): kopiert bis zu `out.len()` Samples heraus — in
    /// Stücken wie die C++-Drain-Schleife, inklusive Wrap-around — und lässt
    /// den Rest von `out` unberührt (der Aufrufer hat ihn mit Stille gefüllt
    /// — SDL-Verhalten bei Unterlauf). Setzt wie das C++
    /// `audio_out_overflow_logged = false` zurück, sobald der Ring leer ist.
    fn pull(&self, out: &mut [i16]) -> usize {
        let mut done = 0;
        if let Ok(mut state) = self.state.lock() {
            let capacity = state.buf.len();
            if capacity > 0 {
                while done < out.len() && state.fill > 0 {
                    let chunk = state.fill.min(out.len() - done).min(capacity - state.read);
                    out[done..done + chunk]
                        .copy_from_slice(&state.buf[state.read..state.read + chunk]);
                    state.read = (state.read + chunk) % capacity;
                    state.fill -= chunk;
                    done += chunk;
                }
                if state.fill == 0 {
                    state.overflow_warned = false;
                }
            }
        }
        if done < out.len() {
            // Underflow → der Rest von `out` bleibt Stille (SDL-Verhalten).
            self.underflows.fetch_add(1, Ordering::Relaxed);
        }
        self.pulled.fetch_add(done as u64, Ordering::Relaxed);
        done
    }

    /// Warn-Flagge (Testbeobachtung von audio_out_overflow_logged).
    #[cfg(test)]
    fn overflow_warned(&self) -> bool {
        self.state.lock().map(|s| s.overflow_warned).unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// Volume (Port des SDL_MixAudioFormat-Aufrufs aus PushAudioFrame)
// ---------------------------------------------------------------------------

/// C++: `SDL_MixAudioFormat(..., AUDIO_S16SYS, volume)` rechnet
/// `sample * volume / 128` (ganzzahlig); bei `volume == SDL_MIX_MAXVOLUME`
/// macht das C++ stattdessen ein reines memcpy (Identität).
/// Läuft auf i32 und kann nicht überlaufen: |i16| * 127 / 128 < i16::MAX.
fn apply_volume(samples: &mut [i16], volume128: i32) {
    if volume128 == SDL_MIX_MAXVOLUME as i32 {
        return; // C++-memcpy-Zweig
    }
    for s in samples.iter_mut() {
        *s = ((*s as i32) * volume128 / SDL_MIX_MAXVOLUME as i32) as i16;
    }
}

// ---------------------------------------------------------------------------
// AudioOutput
// ---------------------------------------------------------------------------

/// Shared State zwischen Session-Thread (push/volume) und cpal-Callback.
struct OutShared {
    ring: SampleRing,
    /// `settings/audio_volume` 0..=128 (SDL_MIX_MAXVOLUME-Skala).
    volume128: AtomicU32,
    /// Latenzgrenze in Samples: `3 * audio_buffer_size` Bytes.
    clear_threshold: u64,
    /// Session-Format (das Format, das `push` liefert) — für fill_ms.
    sample_rate: u32,
    channels: u16,
}

/// Audio-Ausgabe (Lautsprecher). Port von `InitAudio`/`PushAudioFrame`.
///
/// **Nicht `Send`** (cpal::Stream): Erzeugen und Droppen müssen auf demselben
/// Thread passieren — die Session baut das Objekt beim Audio-Handshake
/// (`AudioSettingsCb` → `InitAudio`) ab und wirft es beim Stopp weg.
///
/// Der Ring hat exakt die C++-Kapazität `8 * audio_buffer_size` Bytes;
/// `push` darf vom Session-/Decoder-Thread gerufen werden, solange das
/// `AudioOutput`-Objekt lebt.
pub struct AudioOutput {
    // RAII: hält den cpal-Stream am Leben (Drop stoppt den Callback).
    #[allow(dead_code)]
    stream: Stream,
    shared: Arc<OutShared>,
    device_name: String,
    /// Tatsächlich ausgehandeltes Geräteformat (C++: `obtained`).
    obtained_sample_rate: u32,
    obtained_channels: u16,
    obtained_sample_format: SampleFormat,
    /// Angeforderte Puffergröße in Frames (C++: `spec.samples`).
    requested_frames: u32,
}

impl AudioOutput {
    /// Öffnet das Ausgabegerät und startet den Stream.
    ///
    /// * `device` — Gerätename aus [`AudioOutput::devices`], `None`/`""` = Standardgerät.
    ///   Bei nicht auffindbarem Namen wird — wie im C++ (`InitAudio`) — auf das
    ///   Standardgerät zurückgefallen (Fallback-Logtext übernommen).
    /// * `sample_rate`/`channels` — Session-Format aus dem AudioHeader
    ///   (`AudioSettingsCb(channels, rate)`, immer 48 kHz stereo bei Remote Play).
    /// * `buffer_size` — roher Wert von `settings/audio_buffer_size` (**Bytes**
    ///   S16-PCM, wie im C++; `0` = [`DEFAULT_AUDIO_BUFFER_SIZE`]). Daraus folgen
    ///   Ringkapazität (×8), Latenzgrenze (×3) und die angeforderte
    ///   Geräte-Puffergröße in Frames (`buffer_size / (2 * channels)`,
    ///   C++: `spec.samples = audio_buffer_size / audio_out_sample_size`).
    pub fn new(
        device: Option<&str>,
        sample_rate: u32,
        channels: u16,
        buffer_size: u32,
    ) -> ChiakiResult<Self> {
        let host = cpal::default_host();
        let (device, device_name) = resolve_device(Direction::Output, &host, device)?;

        // C++: audio_buffer_size == 0 → GetAudioBufferSizeDefault() = 9600.
        let buffer_size = if buffer_size == 0 {
            DEFAULT_AUDIO_BUFFER_SIZE
        } else {
            buffer_size
        };
        let buffer_samples = buffer_size as usize / 2; // S16: 2 Bytes je Sample
        let requested_frames = buffer_size / (2 * u32::from(channels)); // C++ spec.samples

        let (config, sample_format, converted) =
            pick_stream_config(&device, Direction::Output, sample_rate, channels, requested_frames)?;

        let shared = Arc::new(OutShared {
            // C++: ring_buf.resize(audio_buffer_size * 8) ist in BYTES — bei
            // S16 also 38400 Samples (nicht buffer_samples × 4: das wäre nur
            // die halbe C++-Kapazität).
            ring: SampleRing::new(buffer_samples * 8),
            volume128: AtomicU32::new(SDL_MIX_MAXVOLUME),
            clear_threshold: (buffer_samples * 3) as u64, // C++: SDL_GetQueuedAudioSize > 3 * audio_buffer_size
            sample_rate,
            channels,
        });

        // C++ InitAudio-Logzeile, falls SDL konvertieren musste.
        if converted {
            tracing::warn!(
                "Audio output '{}' opened with converted format {:?}, {} channels @ {} Hz (requested {:?}, {} channels @ {} Hz)",
                device_name,
                sample_format,
                config.channels,
                config.sample_rate.0,
                SampleFormat::I16,
                channels,
                sample_rate
            );
        }

        let stream = build_stream(&device, &config, sample_format, &shared)
            .or_else(|err| {
                // Fixed Buffer Size wird im WASAPI-Shared-Mode nicht von jedem
                // Treiber akzeptiert — dann der Engine-Default (wie SDL es bei
                // "obtained" auch tun durfte).
                tracing::warn!(
                    "Audio output: requested buffer size {} frames rejected ({}), retrying with engine default",
                    requested_frames,
                    err
                );
                let config = StreamConfig {
                    channels: config.channels,
                    sample_rate: config.sample_rate,
                    buffer_size: BufferSize::Default,
                };
                build_stream(&device, &config, sample_format, &shared)
            })
            .map_err(|_| ChiakiError::Unknown)?;

        // C++: SDL_PauseAudioDevice(audio_out, 0) — der Stream spielt sofort.
        stream.play().map_err(|_| ChiakiError::Unknown)?;

        tracing::info!(
            "Audio Device '{}' opened with {} channels @ {} Hz, buffer size {}",
            device_name,
            config.channels,
            config.sample_rate.0,
            requested_frames * u32::from(config.channels) * 2 // obtained.size-Äquivalent in Bytes
        );

        Ok(AudioOutput {
            obtained_sample_rate: config.sample_rate.0,
            obtained_channels: config.channels,
            obtained_sample_format: sample_format,
            requested_frames,
            device_name,
            stream,
            shared,
        })
    }

    /// Port von `PushAudioFrame` (ohne Speex-Echo-Referenz): hängt dekodierte
    /// Samples (interleaved i16 im Session-Format) an den Ring an.
    ///
    /// C++-Semantik übernommen: bei `audio_volume == 0` wird nichts mehr
    /// eingereiht (`if(!audio_out || !audio_volume) return;`), bei Rückstand
    /// über 3× Puffergröße wird der komplette Queue-Inhalt verworfen und bei
    /// Ringüberlauf die ältesten Samples gedroppt.
    pub fn push(&self, samples: &[i16]) {
        if samples.is_empty() {
            return;
        }
        // C++: if(!audio_out || !audio_volume) return;
        if self.shared.volume128.load(Ordering::Relaxed) == 0 {
            return;
        }
        self.shared.ring.push(samples, self.shared.clear_threshold);
    }

    /// Lautstärke 0.0..=1.0 (Einstellung `settings/audio_volume` 0..=128 →
    /// `volume / 128`). Wird im Audio-Callback angewendet und ist dadurch —
    /// anders als im C++, wo beim Reihen gemischt wurde — sofort wirksam.
    /// Die Rechnung ist identisch (`sample * volume128 / 128`).
    pub fn set_volume(&self, volume: f32) {
        let clamped = volume.clamp(0.0, 1.0);
        let volume128 = (clamped * SDL_MIX_MAXVOLUME as f32).round() as u32;
        self.shared
            .volume128
            .store(volume128.min(SDL_MIX_MAXVOLUME), Ordering::Relaxed);
    }

    /// Aktuelle Lautstärke auf der SDL-Skala 0..=128.
    pub fn volume128(&self) -> u32 {
        self.shared.volume128.load(Ordering::Relaxed)
    }

    /// Ring-Füllstand in Millisekunden Session-Audio — für das Stats-HUD
    /// (C++ hatte dafür nur ein auskommentiertes qDebug über die SDL-Queue).
    pub fn current_buffer_fill_ms(&self) -> f32 {
        let fill = self.shared.ring.fill() as f32;
        let samples_per_ms = f32::from(self.shared.channels) * self.shared.sample_rate as f32
            / 1000.0;
        if samples_per_ms <= 0.0 {
            0.0
        } else {
            fill / samples_per_ms
        }
    }

    /// Durch Überlauf verworfene Samples (kumulativ).
    pub fn dropped_samples(&self) -> u64 {
        self.shared.ring.dropped.load(Ordering::Relaxed)
    }

    /// In den Ring geschobene Samples (kumulativ) — Messung Producer-/Konsum-
    /// Bilanz (HANDOFF P1 "Audio queue exceeded").
    pub fn pushed_samples(&self) -> u64 {
        self.shared.ring.pushed.load(Ordering::Relaxed)
    }

    /// Vom Gerät-Callback entnommene Samples (kumulativ).
    pub fn pulled_samples(&self) -> u64 {
        self.shared.ring.pulled.load(Ordering::Relaxed)
    }

    /// Ausgelöste 3×-Latenz-Clears (kumulativ).
    pub fn clears(&self) -> u64 {
        self.shared.ring.clears.load(Ordering::Relaxed)
    }

    /// Callbacks, in denen Stille wegen leerem Ring nachgespielt wurde.
    pub fn underflows(&self) -> u64 {
        self.shared.ring.underflows.load(Ordering::Relaxed)
    }

    /// Aufgelöster Gerätename (nach Fallback auf den Default).
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Tatsächliche Geräte-Parameter (C++: `obtained`-Spec).
    pub fn obtained_config(&self) -> (u32, u16, SampleFormat) {
        (
            self.obtained_sample_rate,
            self.obtained_channels,
            self.obtained_sample_format,
        )
    }

    /// Angeforderte Geräte-Puffergröße in Frames (C++: `spec.samples`).
    pub fn requested_buffer_frames(&self) -> u32 {
        self.requested_frames
    }

    /// Geräte-Liste für die Settings-UI: Standardgerät zuerst (wie die
    /// C++-UI mit "Auto"), danach alle weiteren Ausgabegeräte.
    pub fn devices() -> Vec<String> {
        super::devices(Direction::Output)
    }
}

impl std::fmt::Debug for AudioOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioOutput")
            .field("device", &self.device_name)
            .field("sample_rate", &self.obtained_sample_rate)
            .field("channels", &self.obtained_channels)
            .field("sample_format", &self.obtained_sample_format)
            .finish()
    }
}

// Drop des Streams stoppt den cpal-Callback (C++: SDL_CloseAudioDevice).

// ---------------------------------------------------------------------------
// Geräte-/Format-Auswahl: gemeinsam in `super` (mod.rs)
// ---------------------------------------------------------------------------

/// Baut den cpal-Stream im passenden Sample-Format. Der Callback füllt aus
/// dem Ring (i16, Session-Kanäle), mischt die Lautstärke, wandelt ggf. die
/// Kanalzahl und dann das Sample-Format — die Arbeit, die im C++ SDL
/// (`SDL_MixAudioFormat` + AudioConverter) abgenommen hat.
fn build_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    sample_format: SampleFormat,
    shared: &Arc<OutShared>,
) -> Result<Stream, cpal::BuildStreamError> {
    // Callback-Scratch: eine Geräte-Periode reicht; größere Callbacks werden
    // Frame-weise durchlaufen.
    let max_frames = match config.buffer_size {
        BufferSize::Fixed(frames) => frames as usize,
        BufferSize::Default => (config.sample_rate.0 / 100) as usize, // typische 10-ms-Periode
    }
    .max(config.sample_rate.0 as usize / 200); // mind. 5 ms
    let sess_ch = shared.channels as usize;
    let dev_ch = config.channels as usize;
    let dev_channels = config.channels;
    let mut scratch = vec![0i16; max_frames * sess_ch];
    let mut converted = vec![0i16; max_frames * dev_ch];
    let shared = Arc::clone(shared);

    let error_callback = move |err: cpal::StreamError| {
        tracing::error!("Audio output stream error: {err}");
    };

    macro_rules! build {
        ($t:ty, $convert:expr) => {
            device.build_output_stream(
                config,
                move |out: &mut [$t], _info| {
                    // Größere Callbacks werden Frame-weise durch den Scratch
                    // gedreht; alle Chunks sind ganze Frames (der WASAPI-Host
                    // liefert ganzzahlige Frame-Anzahlen).
                    for chunk in out.chunks_mut(converted.len()) {
                        let frames = chunk.len() / dev_ch;
                        let usable = frames * dev_ch;
                        {
                            let pcm = &mut scratch[..frames * sess_ch];
                            pcm.fill(0); // Underflow → Stille (SDL-Unterrun-Verhalten)
                            shared.ring.pull(pcm);
                            apply_volume(pcm, shared.volume128.load(Ordering::Relaxed) as i32);
                        }
                        map_channels(
                            &scratch,
                            shared.channels,
                            frames,
                            &mut converted[..usable],
                            dev_channels,
                            frames,
                        );
                        for (dst, &src) in chunk[..usable].iter_mut().zip(converted[..usable].iter())
                        {
                            *dst = $convert(src);
                        }
                    }
                },
                error_callback,
                None,
            )
        };
    }

    match sample_format {
        SampleFormat::I16 => build!(i16, |s: i16| s),
        SampleFormat::F32 => build!(f32, |s: i16| s as f32 / 32768.0),
        SampleFormat::U16 => build!(u16, |s: i16| (s as u16) ^ 0x8000),
        SampleFormat::U8 => build!(u8, |s: i16| ((s >> 8) as i32 + 128) as u8),
        other => Err(cpal::BuildStreamError::StreamConfigNotSupported)
            .inspect_err(|_| tracing::error!("Unsupported output sample format: {other:?}")),
    }
}

// ---------------------------------------------------------------------------
// Tests (ohne Hardware)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(capacity: usize) -> SampleRing {
        SampleRing::new(capacity)
    }

    #[test]
    fn ring_push_pull_roundtrip_across_wraparound() {
        let r = ring(8);
        r.push(&[1, 2, 3, 4, 5, 6], 1000);
        let mut out = [0i16; 4];
        assert_eq!(r.pull(&mut out), 4);
        assert_eq!(out, [1, 2, 3, 4]);

        // Über die Kapazitätsgrenze hinweg weiterschreiben …
        r.push(&[7, 8, 9, 10, 11, 12], 1000);
        let mut out = [0i16; 8];
        assert_eq!(r.pull(&mut out), 8);
        assert_eq!(out, [5, 6, 7, 8, 9, 10, 11, 12]);
        assert_eq!(r.fill(), 0);
    }

    #[test]
    fn ring_pull_leaves_tail_untouched_on_underflow() {
        let r = ring(8);
        r.push(&[1, 2, 3], 1000);
        let mut out = [0x55u16 as i16; 8]; // Marker
        assert_eq!(r.pull(&mut out), 3);
        assert_eq!(&out[..3], &[1, 2, 3]);
        assert_eq!(&out[3..], &[0x55; 5]);
        assert_eq!(r.underflows.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn ring_overflow_drops_oldest_and_warns_once() {
        let r = ring(8);
        r.push(&[1, 2, 3, 4, 5, 6, 7, 8], 1000);
        // Volle Kapazität + 4 neue Samples → wie QueueAudioOutData: die
        // ältesten 4 (read_pos += bytes_to_drop) fallen raus.
        r.push(&[9, 10, 11, 12], 1000);
        assert_eq!(r.dropped.load(Ordering::Relaxed), 4);
        assert!(r.overflow_warned());
        let mut out = [0i16; 8];
        assert_eq!(r.pull(&mut out), 8);
        assert_eq!(out, [5, 6, 7, 8, 9, 10, 11, 12]);

        // Warnung nur einmal (audio_out_overflow_logged), Reset nach Leerlauf.
        r.push(&[13, 14, 15, 16], 1000);
        let mut out = [0i16; 4];
        r.pull(&mut out);
        assert!(!r.overflow_warned());
    }

    #[test]
    fn ring_data_larger_than_capacity_keeps_tail_and_resets() {
        let r = ring(4);
        r.push(&[1, 2, 3, 4, 5, 6], 1000);
        let mut out = [0i16; 4];
        assert_eq!(r.pull(&mut out), 4);
        assert_eq!(out, [3, 4, 5, 6]);
    }

    #[test]
    fn ring_clear_threshold_drops_backlog_like_cpp_queue_clear() {
        let r = ring(16);
        // Threshold 8 Samples: fill > 8 löst den C++-Queue-Clear aus.
        r.push(&[1, 2, 3, 4], 8);
        r.push(&[5, 6, 7, 8], 8); // fill=4 → ok
        r.push(&[9, 10, 11, 12], 8); // fill=8, 8 > 8 ist false → noch kein Clear
        let mut out = [0i16; 16];
        assert_eq!(r.pull(&mut out), 12);
        assert_eq!(&out[..12], &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);

        // Rückstand über dem Threshold → beim nächsten Push fliegt der
        // komplette Stau raus, nur die neuen Frames bleiben.
        r.push(&[1, 2, 3, 4], 8);
        r.push(&[5, 6, 7, 8], 8);
        r.push(&[9, 10, 11, 12], 8); // fill=12
        r.push(&[21, 22, 23, 24], 8); // fill=12 > 8 → Clear, dann push
        r.push(&[31, 32, 33, 34], 8); // fill=4 → ok
        let mut out2 = [0i16; 16];
        let got = r.pull(&mut out2);
        assert_eq!(got, 8);
        assert_eq!(&out2[..got], &[21, 22, 23, 24, 31, 32, 33, 34]);
    }

    #[test]
    fn volume_matches_sdl_mix_audio_format() {
        // SDL_MixAudioFormat: sample * volume / 128 (ganzzahlig)
        let mut s = vec![1000i16, -2000, i16::MAX, i16::MIN];
        apply_volume(&mut s, 128); // Identität (C++-memcpy-Zweig)
        assert_eq!(s, vec![1000, -2000, i16::MAX, i16::MIN]);

        let mut s = vec![1000i16, -2000];
        apply_volume(&mut s, 64);
        assert_eq!(s, vec![500, -1000]);

        let mut s = vec![1234i16];
        apply_volume(&mut s, 0);
        assert_eq!(s, vec![0]);

        // Lautstärken unter 128 können i16 nicht überlaufen lassen
        // (ganzzahlig wie SDL: 32767*127/128 = 32511).
        let mut s = vec![i16::MIN, i16::MAX];
        apply_volume(&mut s, 127);
        assert_eq!(s, vec![-32512, 32511]);
    }

    #[test]
    fn ring_is_quiet_when_empty_and_flags_reset() {
        let r = ring(4);
        let mut out = [0i16; 4];
        assert_eq!(r.pull(&mut out), 0);
        assert_eq!(r.underflows.load(Ordering::Relaxed), 1);
        r.push(&[1, 2], 1000);
        r.push(&[3, 4, 5], 1000); // 3 > 4-2 → das älteste Sample fliegt raus
        assert!(r.overflow_warned());
        let mut four = [0i16; 4];
        assert_eq!(r.pull(&mut four), 4);
        assert_eq!(four, [2, 3, 4, 5]);
        // Leerlauf setzt die Warn-Flagge zurück (C++-Verhalten).
        assert!(!r.overflow_warned());
    }

    /// Stream-Test gegen das Standardgerät: Aufbau, 100 ms Audio in den
    /// Ring, der cpal-Callback muss es abholen; danach sauberer Stop.
    /// `#[ignore]`, damit CI ohne Audio-Gerät grün bleibt (diese Maschine
    /// HAT Audio: lokal mit `--ignored` laufen lassen).
    #[test]
    #[ignore = "benoetigt ein Ausgabegeraet (Windows-Host)"]
    fn output_stream_plays_100ms_silence_and_stops_cleanly() {
        let out = AudioOutput::new(None, 48_000, 2, 0).expect("AudioOutput auf Standardgerät");
        out.set_volume(1.0);
        // 100 ms @ 48 kHz stereo = 4800 Frames = 9600 Samples (Stille).
        let silence = vec![0i16; 9600];
        out.push(&silence);
        assert!(out.current_buffer_fill_ms() > 90.0, "Ring sollte ~100 ms enthalten");
        std::thread::sleep(std::time::Duration::from_millis(300));
        // Der Echtzeit-Callback hat den Ring leer gezogen (Underflow zählt
        // ggf. die Restperiode — Hauptsache, nichts ist mehr angestaut).
        assert!(out.current_buffer_fill_ms() < 1.0);
        println!(
            "underflows: {}, dropped: {}",
            out.underflows(),
            out.dropped_samples()
        );
        drop(out); // sauberer Stop (C++: SDL_CloseAudioDevice)
    }

    /// Geräte-Enumeration gegen den WASAPI-Host.
    #[test]
    #[ignore = "benoetigt Audio-Geraete (Windows-Host)"]
    fn enumerates_output_devices() {
        let devices = AudioOutput::devices();
        assert!(!devices.is_empty(), "kein Ausgabegeraet gefunden");
        println!("output devices: {devices:?}");
    }
}
