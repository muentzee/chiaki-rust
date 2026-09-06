// SPDX-License-Identifier: AGPL-3.0-only
//! Audio-Ausgabe (Lautsprecher) und Mikrofon-Eingabe für den Client —
//! Port der Audio-Teile von `gui/src/streamsession.cpp` (chiaki-ng, SDL2)
//! auf cpal/WASAPI.
//!
//! ## Mapping C++ (SDL2) → Rust (cpal)
//!
//! | C++ (streamsession.cpp)                     | Rust                                    |
//! |---------------------------------------------|-----------------------------------------|
//! | `InitAudio` (Gerät öffnen, Ring anlegen)    | [`AudioOutput::new`]                    |
//! | `PushAudioFrame` (Volume-Mix, Threshold)    | [`AudioOutput::push`]                   |
//! | `QueueAudioOutData` (Ring, drop-oldest)     | `SampleRing::push` (in `output.rs`)     |
//! | `AudioOutDrainThreadMain` (1 ms Backpressure| entfällt — der cpal-Echtzeit-Callback   |
//! | laut REWORK.md Abschnitt 2.2)               | zieht direkt aus dem Ring; die          |
//! |                                             | 1 ms-Warteschleife existierte nur, um   |
//! |                                             | die SDL-Queue nachzufüllen. In Rust ist |
//! |                                             | jede angefallene Probe beim nächsten    |
//! |                                             | Callback-Periode sofort abholbar        |
//! |                                             | (wirksame Wartezeit: 0 ms).             |
//! | `SDL_MixAudioFormat(..., audio_volume)`     | Multiplikation im Callback (`set_volume`),
//! | (Mix zum Zeitpunkt des Queuens)             | dadurch sofort wirksam (statt eine      |
//! |                                             | Queue-Lang später)                      |
//! | `ToggleMute`/`muted`/`SDL_PauseAudioDevice` | [`AudioInput::set_muted`] (Capture-Gate)|
//! | `InitMic`/`QueueMicData`/`DrainMicRingBuffer`/`ReadMic` | [`AudioInput::new`] (Capture-Callback → Mic-Ring → Drain-Thread → Frame-Akkumulator → `OpusAudioEncoder` → Owner-Callback) |
//!
//! Die Ringpuffer-Kapazität entspricht exakt dem C++ (Ring = `8 * audio_buffer_size`
//! Bytes, Latenz-Threshold = `3 * audio_buffer_size`, SDL-Zielqueue = `2 *`
//! `audio_buffer_size`); `settings/audio_buffer_size` ist wie im C++ ein **Byte**-Wert
//! (S16-PCM), Default 9600 (Settings::GetAudioBufferSizeDefault) = 2400 Frames =
//! 50 ms @ 48 kHz stereo.
//!
//! ## Speech-Processing (Speex) — nicht portiert
//!
//! chiaki-ng filtert das Mikrofon optional per **SpeexDSP**
//! (`speex_echo_state_init`/`speex_preprocess_state_init`, Echokompensation +
//! Rauschunterdrückung, Settings `settings/enable_speech_processing`,
//! `noise_suppress_level`, `echo_suppress_level`) — ein optionales Build-Feature
//! (`CHIAKI_GUI_ENABLE_SPEEX`), keine SDL-Funktion. Für den Port fehlt eine
//! speexdsp-Anbindung in chiaki-media; implementiert ist deshalb exakt der
//! C-Pfad **ohne** `CHIAKI_GUI_ENABLE_SPEEX` (`InitMic(2, rate)` → direktes
//! Opus-Encoding). TODO(speech-processing): Noise-Gate als einfacher Ersatz
//! oder speexdsp-FFI, falls gewünscht.

pub mod input;
pub mod output;

pub use input::AudioInput;
pub use output::AudioOutput;

use chiaki_core::{ChiakiError, ChiakiResult};
use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{BufferSize, SampleFormat, SampleRate, StreamConfig};

/// Port von `MICROPHONE_SAMPLES` (streamsession.cpp): 480 Samples pro
/// Mikrofon-Frame = 10 ms @ 48 kHz.
pub(crate) const MICROPHONE_SAMPLES: u32 = 480;

/// Vom C++ übernommene naive Kanalwandlung (SDL macht das intern im
/// AudioConverter): mono ↔ stereo und Zuschnitt bei anderen Kanalzahlen.
/// `src`/`dst` sind interleaved i16; `src` beschreibt `src_frames` Frames mit
/// `src_channels` Kanälen, `dst` wird mit `dst_frames` Frames à `dst_channels`
/// gefüllt (Rest: Stille).
pub(crate) fn map_channels(
    src: &[i16],
    src_channels: u16,
    src_frames: usize,
    dst: &mut [i16],
    dst_channels: u16,
    dst_frames: usize,
) {    let (src_channels, dst_channels) = (src_channels as usize, dst_channels as usize);
    for frame in 0..dst_frames {
        let dst_base = frame * dst_channels;
        if frame >= src_frames {
            for s in &mut dst[dst_base..dst_base + dst_channels] {
                *s = 0;
            }
            continue;
        }
        let src_base = frame * src_channels;
        match (src_channels, dst_channels) {
            _ if src_channels == dst_channels => {
                dst[dst_base..dst_base + dst_channels]
                    .copy_from_slice(&src[src_base..src_base + dst_channels]);
            }
            // mono → stereo (wie im C++ SPEEX-Pfad: "Use 1 channel ... then mix to 2 channels")
            (1, 2) => {
                let s = src[src_base];
                dst[dst_base] = s;
                dst[dst_base + 1] = s;
            }
            // stereo → mono: Mittelwert (SDL-Downmix)
            (2, 1) => {
                dst[dst_base] = ((src[src_base] as i32 + src[src_base + 1] as i32) / 2) as i16;
            }
            // allgemein: gemeinsame Kanäle kopieren, Rest stumm
            (s, d) => {
                let n = s.min(d);
                dst[dst_base..dst_base + n].copy_from_slice(&src[src_base..src_base + n]);
                for s in &mut dst[dst_base + n..dst_base + d] {
                    *s = 0;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Geräte-/Format-Auswahl (gemeinsam für Output und Mic; die Fallback- und
// Konvertierungsregeln entsprechen SDL-Verhalten wie in InitAudio/InitMic)
// ---------------------------------------------------------------------------

/// Richtung des Streams (für Logtexte und Geräte-Enumeration).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Output,
    Input,
}

impl Direction {
    fn device_list(&self, host: &cpal::Host) -> Vec<String> {
        let mut names = Vec::new();
        let default = match self {
            Direction::Output => host.default_output_device(),
            Direction::Input => host.default_input_device(),
        };
        if let Some(d) = default {
            if let Ok(name) = d.name() {
                names.push(name);
            }
        }
        let devices = match self {
            Direction::Output => host.output_devices().map(|i| i.collect::<Vec<_>>()),
            Direction::Input => host.input_devices().map(|i| i.collect::<Vec<_>>()),
        };
        if let Ok(devices) = devices {
            for d in devices {
                if let Ok(name) = d.name() {
                    if !names.contains(&name) {
                        names.push(name);
                    }
                }
            }
        }
        names
    }

    fn resolve(&self, host: &cpal::Host, name: Option<&str>) -> ChiakiResult<(cpal::Device, String)> {
        // C++ InitAudio/InitMic: benanntes Gerät versuchen, bei Misserfolg auf
        // den Default fallen (Logtexte wörtlich übernommen).
        let (named_log, default_log) = match self {
            Direction::Output => (
                "Failed to open Audio Output Device",
                "Failed to open default Audio Output Device",
            ),
            Direction::Input => (
                "Failed to open Microphone",
                "Failed to open default Microphone",
            ),
        };
        if let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) {
            let devices = match self {
                Direction::Output => host.output_devices().map(|i| i.collect::<Vec<_>>()),
                Direction::Input => host.input_devices().map(|i| i.collect::<Vec<_>>()),
            };
            if let Ok(devices) = devices {
                for d in devices {
                    if d.name().ok().as_deref() == Some(name) {
                        return Ok((d, name.to_string()));
                    }
                }
            }
            tracing::error!("{named_log} '{name}'");
        }
        match match self {
            Direction::Output => host.default_output_device(),
            Direction::Input => host.default_input_device(),
        } {
            Some(d) => {
                let name = d.name().unwrap_or_else(|_| "default".to_string());
                Ok((d, name))
            }
            None => {
                tracing::error!("{default_log}");
                Err(ChiakiError::Unknown)
            }
        }
    }
}

/// Geräte-Liste für die Settings-UI: Standardgerät zuerst (wie die C++-UI
/// mit "Auto"), danach alle weiteren Geräte.
pub(crate) fn devices(direction: Direction) -> Vec<String> {
    direction.device_list(&cpal::default_host())
}

/// Öffnet das Gerät nach Name (mit Default-Fallback wie im C++).
pub(crate) fn resolve_device(
    direction: Direction,
    host: &cpal::Host,
    name: Option<&str>,
) -> ChiakiResult<(cpal::Device, String)> {
    direction.resolve(host, name)
}

/// Sucht aus den WASAPI-Angeboten eine Config mit der gewünschten Rate.
/// Bevorzugt exakt (rate, channels), dann die Rate mit abweichender
/// Kanalzahl (naive Wandlung wie im SDL-Converter, siehe
/// `map_channels`). Sample-Format: I16 direkt, sonst F32/U16/U8 mit
/// Konvertierung im Callback. Resampling wird nicht implementiert — die
/// PS-Remote-Play-Audio-Header sind immer 48 kHz.
pub(crate) fn pick_stream_config(
    device: &cpal::Device,
    direction: Direction,
    sample_rate: u32,
    channels: u16,
    requested_frames: u32,
) -> ChiakiResult<(StreamConfig, SampleFormat, bool)> {
    let mut ranges: Vec<cpal::SupportedStreamConfigRange> = Vec::new();
    match direction {
        Direction::Output => {
            ranges.extend(device.supported_output_configs().map_err(|_| ChiakiError::Unknown)?);
        }
        Direction::Input => {
            ranges.extend(device.supported_input_configs().map_err(|_| ChiakiError::Unknown)?);
        }
    }

    let rate_ok = |r: &cpal::SupportedStreamConfigRange| {
        r.min_sample_rate().0 <= sample_rate && sample_rate <= r.max_sample_rate().0
    };
    // I16 zuerst (der OpusDecoder liefert i16), danach F32/U16/U8.
    let format_rank = |f: SampleFormat| match f {
        SampleFormat::I16 => 0,
        SampleFormat::F32 => 1,
        SampleFormat::U16 => 2,
        SampleFormat::U8 => 3,
        _ => 4,
    };

    let mut best_exact: Option<&cpal::SupportedStreamConfigRange> = None;
    let mut best_other: Option<&cpal::SupportedStreamConfigRange> = None;
    for range in &ranges {
        if !rate_ok(range) || format_rank(range.sample_format()) >= 4 {
            continue;
        }
        if range.channels() == channels {
            if best_exact.is_none_or(|b| {
                format_rank(range.sample_format()) < format_rank(b.sample_format())
            }) {
                best_exact = Some(range);
            }
        } else if best_other.is_none_or(|b| {
            format_rank(range.sample_format()) < format_rank(b.sample_format())
        }) {
            best_other = Some(range);
        }
    }

    let range = best_exact.or(best_other).ok_or_else(|| {
        let kind = match direction {
            Direction::Output => "output",
            Direction::Input => "input",
        };
        tracing::error!(
            "Audio {kind} device does not support {sample_rate} Hz (no usable WASAPI format)"
        );
        ChiakiError::InvalidData
    })?;

    let converted = range.channels() != channels;
    let config = StreamConfig {
        channels: range.channels(),
        sample_rate: SampleRate(sample_rate),
        buffer_size: BufferSize::Fixed(requested_frames),
    };
    Ok((config, range.sample_format(), converted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_channels_identity() {
        let src = vec![1, 2, 3, 4, 5, 6];
        let mut dst = vec![0; 6];
        map_channels(&src, 2, 3, &mut dst, 2, 3);
        assert_eq!(dst, src);
    }

    #[test]
    fn map_channels_mono_to_stereo() {
        let src = vec![10, 20];
        let mut dst = vec![0; 4];
        map_channels(&src, 1, 2, &mut dst, 2, 2);
        assert_eq!(dst, vec![10, 10, 20, 20]);
    }

    #[test]
    fn map_channels_stereo_to_mono() {
        let src = vec![10, 20, 30, 40];
        let mut dst = vec![0; 2];
        map_channels(&src, 2, 2, &mut dst, 1, 2);
        assert_eq!(dst, vec![15, 35]);
    }

    #[test]
    fn map_channels_pads_silence_when_source_short() {
        let src = vec![7, 8];
        let mut dst = vec![1; 4];
        map_channels(&src, 2, 1, &mut dst, 2, 2);
        assert_eq!(dst, vec![7, 8, 0, 0]);
    }
}
