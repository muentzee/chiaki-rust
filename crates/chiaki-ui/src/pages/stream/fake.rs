//! FAKE-Stream (env `CHIAKI_UI_FAKE_STREAM=1` bzw. `=pin`): die komplette
//! Stream-UI ohne Konsole verifizierbar machen.
//!
//! Ein Producer-Thread erzeugt im Takt des echten Video-Pfads (60 fps,
//! 1-Frame-Queue über [`VideoPresenter::set_frame`]) ein synthetisches
//! NV12-Testpattern (animierter Diagonal-Gradient + laufender Weißbalken +
//! Chroma-Farbbalken — die Spike-S1-Idee aus chiaki-render) und füttert die
//! [`StreamTelemetry`] mit plausiblen Werten (Bitrate ~15 Mbit/s, RTT,
//! Audio-Füllstand). Der Connecting-Ablauf (Aufwecken → Anmelden →
//! Kalibrieren → Streamen) wird zeitgetaktet durchgespielt; mit `=pin`
//! fordert der Fake zusätzlich eine Login-PIN an, sodass das PIN-Overlay
//! testbar ist.
//!
//! Bewusst eine Abweichung zum echten Pfad: Der Fake treibt den UI-Zustand
//! über Atomics (connected/pin) statt über `UiEvent::Session` — der
//! Event-Pfad inkl. Quit-Handling ist nur mit echter Konsole bzw. über die
//! Quit-Tests der Session erreichbar.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chiaki_render::gpu_sink::GpuSinkHandle;
use chiaki_render::nv12::NV12Frame;
use chiaki_render::presenter::VideoPresenter;

use crate::backend::sessions::StreamTelemetry;

/// Fake-Stream-Auflösung (720p — klein genug für weiche 60 fps im
/// Testpattern-Thread, groß genug für sichtbare Details).
pub const WIDTH: u32 = 1280;
pub const HEIGHT: u32 = 720;
/// Fake-Quellrate (der Producer-Thread taktet hierauf).
pub const FPS: u32 = 60;
/// Ziel-Bitrate des Fake-Streams (Bytes/s) — ~15 Mbit/s wie ein lokales
/// 1080p-Profil, damit das HUD „echte" Werte zeigt.
const BYTES_PER_SEC: u64 = 15_000_000 / 8;

/// Startet den Fake-Producer-Thread (endet über `stop`). Mit `gpu` werden die
/// Frames in den D3D11-Sink hochgeladen (submit_cpu — GPU-Pfad-Test ohne
/// Konsole), sonst in den Presenter.
pub fn start(
    presenter: VideoPresenter,
    telemetry: Arc<StreamTelemetry>,
    stop: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    pin_requested: Option<Arc<AtomicBool>>,
    gpu: Option<GpuSinkHandle>,
) {
    let spawned = std::thread::Builder::new()
        .name("chiaki-ui-fake-stream".into())
        .spawn(move || run(presenter, telemetry, stop, connected, pin_requested, gpu));
    if let Err(err) = spawned {
        tracing::error!("Fake-Stream-Thread konnte nicht gestartet werden: {err}");
    }
}

fn run(
    presenter: VideoPresenter,
    telemetry: Arc<StreamTelemetry>,
    stop: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    pin_requested: Option<Arc<AtomicBool>>,
    gpu: Option<GpuSinkHandle>,
) {
    let started = Instant::now();
    let mut pin_sent = false;
    let mut frame_index: u64 = 0;

    // Connecting-Phase: Wakeup → Anmelden → Kalibrieren (Taktung wie im
    // UI-State: 0.8 s / 1.6 s / Connected).
    let connect_at = started + Duration::from_millis(2600);
    let frame_period = Duration::from_millis(1000 / u64::from(FPS));

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let now = Instant::now();

        if now >= connect_at && !connected.load(Ordering::Relaxed) {
            connected.store(true, Ordering::Relaxed);
            tracing::info!("Fake-Stream: Connected");
        }
        if !pin_sent && started.elapsed() >= Duration::from_millis(1200) {
            pin_sent = true;
            if let Some(pin) = &pin_requested {
                pin.store(true, Ordering::Relaxed);
                tracing::info!("Fake-Stream: LoginPinRequest (pin-Mode)");
            }
        }

        // Testpattern erzeugen und in den Presenter legen (Producer-Rolle
        // des echten Media-Threads). Die Frame-Erzeugungszeit dient als
        // FRAME-TIME (telemetry.media_frame_us), wenn der GPU-Pfad den
        // Presenter ungenutzt lässt.
        let t = (frame_index % (60 * 8)) as f32 / 60.0; // 8-s-Zyklus
        let frame_t0 = Instant::now();
        let frame = test_pattern(t, frame_index);
        // GPU-Sink vorhanden (settings/video_output) → Upload-Pfad testen.
        match &gpu {
            Some(g) if !g.is_lost() => g.submit_cpu(frame),
            _ => presenter.set_frame(frame),
        };
        let frame_us = frame_t0.elapsed().as_micros() as u64;
        let prev = telemetry.media_frame_us.load(Ordering::Relaxed) as u64;
        let ema = if prev == 0 { frame_us } else { prev * 9 / 10 + frame_us / 10 };
        telemetry.media_frame_us.store(ema.min(u32::MAX as u64) as u32, Ordering::Relaxed);

        // Telemetrie: gemessene Bitrate (bytes/s → Bytes pro Frame), Frames,
        // gelegentlich ein verlorener Frame, Audio-Füllstand, RTT, Decoder.
        let bytes_per_frame = BYTES_PER_SEC / 60;
        telemetry.video_bytes.fetch_add(bytes_per_frame, Ordering::Relaxed);
        telemetry.video_frames.fetch_add(1, Ordering::Relaxed);
        if frame_index % 600 == 42 {
            telemetry.video_frames_lost.fetch_add(1, Ordering::Relaxed);
        }
        let wobble = (t * std::f32::consts::TAU).sin() * 6.0;
        telemetry.audio_fill_ms_x10.store(((45.0 + wobble) * 10.0) as u32, Ordering::Relaxed);
        telemetry.rtt_ms_x10.store(42, Ordering::Relaxed); // 4.2 ms
        *telemetry.decoder_backend.lock().unwrap_or_else(|e| e.into_inner()) =
            Some("Software (Fake)".into());
        *telemetry.haptics_mode.lock().unwrap_or_else(|e| e.into_inner()) = "aus".into();

        frame_index += 1;
        std::thread::sleep(frame_period);
    }
    tracing::info!("Fake-Stream-Thread beendet");
}

/// Erzeugt einen NV12-Frame: animierter Diagonal-Luma-Gradient, ein
/// querlaufender Weißbalken und neutrale → farbige Chroma-Streifen.
fn test_pattern(t: f32, frame_index: u64) -> NV12Frame {
    let mut frame = NV12Frame::new(WIDTH, HEIGHT).expect("720p ist ein gültiges NV12-Format");
    let (w, h) = (WIDTH as usize, HEIGHT as usize);
    let (y_len, uv_len) = (w * h, w * h / 2);

    {
        let y = &mut frame.data[..y_len];
        let offset = (t * 80.0) as usize; // Gradient driftet mit der Zeit
        // Balkenposition (querlaufend, 80 px breit).
        let bar_x = ((t * 240.0) as usize) % (w + 80);
        for row in 0..h {
            let line = &mut y[row * w..(row + 1) * w];
            let row_term = (row / 3 + offset) % 256;
            for (col, px) in line.iter_mut().enumerate() {
                let mut v = (col / 3 + row_term) % 256;
                if col >= bar_x && col < bar_x + 80 {
                    v = 235; // Weißreferenz
                }
                *px = v as u8;
            }
        }
    }
    {
        // Chroma: 8 Farbbalken (U/V-Paare), über die Zeit rotierend.
        let uv = &mut frame.data[y_len..y_len + uv_len];
        const PALETTE: [(u8, u8); 8] = [
            (128, 128), // grau
            (160, 70),  // rotlich
            (120, 170), // gelblich
            (150, 40),  // grünlich
            (60, 200),  // cyanlich
            (200, 150), // bläulich
            (170, 40),  // violett
            (110, 110), // dunkler
        ];
        let shift = (frame_index / 30) as usize % PALETTE.len();
        for row in 0..h / 2 {
            let line = &mut uv[row * w..(row + 1) * w];
            for (i, pair) in line.chunks_exact_mut(2).enumerate() {
                let idx = (i / (w / 8) + shift) % PALETTE.len();
                pair[0] = PALETTE[idx].0;
                pair[1] = PALETTE[idx].1;
            }
        }
    }
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn testpattern_hat_gueltiges_layout() {
        let frame = test_pattern(0.5, 30);
        assert_eq!((frame.width, frame.height), (WIDTH, HEIGHT));
        assert_eq!(frame.data.len(), WIDTH as usize * HEIGHT as usize * 3 / 2);
        // Balken liefert Weißreferenz-Zeilen (Y=235 kommt vor).
        assert!(frame.data[..y_len_of(&frame)].contains(&235));
    }

    fn y_len_of(frame: &NV12Frame) -> usize {
        frame.y_stride * frame.height as usize
    }
}
