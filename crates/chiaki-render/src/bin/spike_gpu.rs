//! Spike GPU (Spike-Variante b) — „GPU-residenter Videopfad“.
//!
//! Beweist die Pipeline `Quelle → D3D11-Sink (eigenes Fenster + Swapchain) →
//! Present` in drei Phasen mit transparentem GPUI-Overlay-Fenster darüber:
//!
//! 1. **cpu-upload** — NV12-Testpattern (60 fps) → [`GpuSink::submit_cpu`]
//!    (UpdateSubresource-Pfad; entspricht dem VSR-FrameBuf-Fallback).
//! 2. **d3d11va** — `live-test/stream.h265` via Decoder (D3D11VA, raw Output,
//!    externes Sink-Device) → [`GpuSinkHandle::submit_d3d11`] (GPU-GPU-Kopie).
//! 3. **cuda-vsr** — Decoder (CUDA, raw Output) → [`VsrUpscaler::process_frame_gpu`]
//!    → CUDA-D3D11-Interop-Schreibvorgang → [`GpuSinkHandle::submit_bgra_ready`].
//!
//! Gemessen pro Phase: präsentierte FPS (Sink-Statistik), Prozess-CPU-Last
//! (GetProcessTimes), Feed-/Decode-Zähler. Das Overlay (gpui 0.2.2,
//! DirectComposition, PREMULTIPLIED-Alpha) malt NUR HUD-Elemente — der Rest
//! bleibt transparent und zeigt das Video-Fenster darunter (Beweis der
//! Overlay-Variante). Ergebnis schreibt `spike-gpu-results.md`.
//!
//! Aufruf: `cargo run -p chiaki-render --bin spike-gpu --release [-- <sekunden>]`
//! Umgebungs-Defaults (Dev-Maschine): `CHIAKI_FFMPEG_DIR`, `CHIAKI_VSR_SDK_DIR`.

use gpui::{
    div, px, App, Application, Bounds, Context, Render, TitlebarOptions, Window, WindowBounds,
    WindowOptions, prelude::*, size,
};
use std::fmt::Write as _;
use std::os::raw::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::thread;

use chiaki_media::decoder::{Decoder, DecoderOpts, FrameMemory, HwBackend};
use chiaki_media::vsr::VsrUpscaler;
use chiaki_media::cuda_d3d11::CudaD3d11Interop;
use chiaki_media::DecodedFrame;
use chiaki_render::gpu_sink::{GpuSink, GpuSinkHandle, SinkZoom};
use chiaki_render::nv12::NV12Frame;

const OVERLAY_TITLE: &str = "chiaki-gpu-spike-overlay";

// ---------------------------------------------------------------------------
// Windows-FFI: Prozess-CPU-Zeit (GetProcessTimes) — nur für die Messung.
// ---------------------------------------------------------------------------

mod win {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    fn filetime_u64(ft: &FILETIME) -> u64 {
        ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
    }

    /// Verbrauchte CPU-Zeit des Prozesses (Kernel + User) in Sekunden.
    pub fn process_cpu_time() -> f64 {
        unsafe {
            let handle = GetCurrentProcess();
            let mut creation = FILETIME::default();
            let mut exit = FILETIME::default();
            let mut kernel = FILETIME::default();
            let mut user = FILETIME::default();
            if GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user).is_ok() {
                (filetime_u64(&kernel) + filetime_u64(&user)) as f64 / 10_000_000.0
            } else {
                0.0
            }
        }
    }
}

/// Dev-Defaults für die dynamischen DLLs (wie chiaki-media test_setup).
fn ensure_dev_env() {
    if std::env::var_os("CHIAKI_FFMPEG_DIR").is_none() {
        std::env::set_var(
            "CHIAKI_FFMPEG_DIR",
            r"F:\projekte\chiaki-rust-remaster\ffmpeg-n7.1-latest-win64-gpl-shared-7.1\bin",
        );
    }
    if std::env::var_os("CHIAKI_VSR_SDK_DIR").is_none() {
        std::env::set_var(
            "CHIAKI_VSR_SDK_DIR",
            r"F:\projekte\chiaki-rust-remaster\vfx_sdk\sdk\VideoFX\bin",
        );
    }
}

// ---------------------------------------------------------------------------
// NV12-Testpattern (identisch zu Spike S1)
// ---------------------------------------------------------------------------

const CHECKER_LIGHT: u8 = 110;
const CHECKER_DARK: u8 = 30;
const CIRCLE_LUMA: u8 = 220;
const CIRCLE_U: u8 = 90;
const CIRCLE_V: u8 = 220;

fn generate_test_frame(frame_no: u64, width: u32, height: u32) -> NV12Frame {
    let mut frame = NV12Frame::new(width, height).expect("pattern dimensions are valid");
    let w = width as usize;
    let h = height as usize;
    let y_stride = frame.y_stride;
    let uv_stride = frame.uv_stride;
    let y_len = y_stride * h;
    let (y, uv) = frame.data.split_at_mut(y_len);

    const BLOCK: usize = 32;
    let scroll = (frame_no % (2 * BLOCK as u64)) as usize;
    for row in 0..h {
        let row_start = row * y_stride;
        let row_buf = &mut y[row_start..row_start + w];
        let mut light = ((row / BLOCK) + scroll / BLOCK) % 2 == 0;
        let mut rest = row_buf;
        let first = BLOCK - (scroll % BLOCK);
        let n0 = first.min(rest.len());
        rest[..n0].fill(if light { CHECKER_LIGHT } else { CHECKER_DARK });
        rest = &mut rest[n0..];
        light = !light;
        while !rest.is_empty() {
            let n = BLOCK.min(rest.len());
            rest[..n].fill(if light { CHECKER_LIGHT } else { CHECKER_DARK });
            rest = &mut rest[n..];
            light = !light;
        }
    }

    for uv_row in 0..(8usize.min(h / 2)) {
        let uv_start = uv_row * uv_stride;
        for col in 0..w / 2 {
            uv[uv_start + col * 2] = 128;
            uv[uv_start + col * 2 + 1] = (frame_no % 256) as u8;
        }
    }

    let counter = (frame_no % 256) as u8;
    for row in h.saturating_sub(32)..h {
        let row_start = row * y_stride;
        y[row_start..row_start + w].fill(counter);
    }

    let t = frame_no as f64;
    let cx = w as f64 / 2.0 + (w as f64 / 4.0) * (t * 0.02).sin();
    let cy = h as f64 / 2.0 + (h as f64 / 4.0) * (t * 0.027).cos();
    let r = (h.min(w) as f64) / 6.0;
    let row0 = (cy - r).max(0.0) as usize;
    let row1 = (((cy + r) as usize) + 1).min(h);
    for row in row0..row1 {
        let dy = row as f64 + 0.5 - cy;
        let dx = ((r * r - dy * dy).max(0.0)).sqrt() as usize;
        let x0 = (cx as usize).saturating_sub(dx);
        let x1 = ((cx as usize) + dx + 1).min(w);
        let row_start = row * y_stride;
        y[row_start + x0..row_start + x1].fill(CIRCLE_LUMA);
        let uv_start = (row / 2) * uv_stride;
        let c0 = x0 / 2;
        let c1 = ((x1 / 2).max(c0 + 1)).min(w / 2);
        for col in c0..c1 {
            uv[uv_start + col * 2] = CIRCLE_U;
            uv[uv_start + col * 2 + 1] = CIRCLE_V;
        }
    }

    frame
}

// ---------------------------------------------------------------------------
// HEVC-AnnexB-Splitter (Access Units aus live-test/stream.h265)
// ---------------------------------------------------------------------------

/// HEVC-NAL-Typ einer NAL (Header-Byte 0: type = (b >> 1) & 0x3f).
fn hevc_nal_type(nal: &[u8]) -> u8 {
    (nal[0] >> 1) & 0x3f
}

/// VCL-NALs (Bildinhalt): TRAIL/TSA/STSA/RADL/RASL (0..=9) und BLA/IDR/CRA (16..=21).
fn is_vcl(t: u8) -> bool {
    t <= 9 || (16..=21).contains(&t)
}

/// Teilt einen AnnexB-Stream in Access Units auf: eine AU endet an der ersten
/// VCL-NAL, sobald die laufende AU schon eine VCL hat (Parameter-Sets/SEI
/// laufen der Slice-NAL voraus — genau das Layout der PS5-Streams).
fn split_access_units(data: &[u8]) -> Vec<Vec<u8>> {
    let mut starts: Vec<usize> = Vec::new();
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i);
            i += 3;
        } else if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1 {
            starts.push(i);
            i += 4;
        } else {
            i += 1;
        }
    }

    let mut aus: Vec<Vec<u8>> = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    let mut current_has_vcl = false;
    for (idx, &s) in starts.iter().enumerate() {
        let end = starts.get(idx + 1).copied().unwrap_or(data.len());
        let header = if data[s + 2] == 0 { 4 } else { 3 };
        let nal_at = (s + header).min(data.len());
        let t = hevc_nal_type(&data[nal_at..]);
        let vcl = is_vcl(t);
        if vcl && current_has_vcl {
            aus.push(std::mem::take(&mut current));
            current_has_vcl = false;
        }
        current.extend_from_slice(&data[s..end]);
        if vcl {
            current_has_vcl = true;
        }
    }
    if !current.is_empty() {
        aus.push(current);
    }
    aus
}

// ---------------------------------------------------------------------------
// Overlay-Fenster (gpui): nur HUD malen — Rest transparent
// ---------------------------------------------------------------------------

struct OverlayShared {
    phase: Mutex<String>,
    fps: Mutex<f64>,
    ui_frames: AtomicU64,
}

struct OverlayView {
    shared: Arc<OverlayShared>,
}

impl Render for OverlayView {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        window.request_animation_frame();
        self.shared.ui_frames.fetch_add(1, Ordering::Relaxed);
        let phase = self.shared.phase.lock().unwrap().clone();
        let fps = *self.shared.fps.lock().unwrap();

        // WICHTIG: KEIN Hintergrund auf dem Root — ungepixelte Bereiche sind
        // transparent (gpui: DComp-Swapchain, PREMULTIPLIED, Clear 0,0,0,0).
        div()
            .size_full()
            .child(
                div()
                    .absolute()
                    .top_2()
                    .left_2()
                    .p_2()
                    .rounded_sm()
                    .bg(gpui::black().opacity(0.65))
                    .text_size(px(16.0))
                    .text_color(gpui::white())
                    .child(format!("Spike GPU — Phase: {phase} — Overlay {fps:.0} fps")),
            )
            .child(
                div()
                    .absolute()
                    .top_2()
                    .right_2()
                    .p_2()
                    .rounded_sm()
                    .bg(gpui::rgb(0x1d2029))
                    .text_size(px(14.0))
                    .text_color(gpui::white())
                    .child("opakes HUD-Panel"),
            )
    }
}

fn overlay_fps_thread(shared: Arc<OverlayShared>, stop: Arc<AtomicBool>) {
    let mut last = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(500));
        let now = Instant::now();
        let elapsed = now.duration_since(last);
        if elapsed >= Duration::from_secs(1) {
            let frames = shared.ui_frames.swap(0, Ordering::Relaxed);
            *shared.fps.lock().unwrap() = frames as f64 / elapsed.as_secs_f64();
            last = now;
        }
    }
}

// ---------------------------------------------------------------------------
// Phasen
// ---------------------------------------------------------------------------

struct PhaseOutcome {
    name: &'static str,
    wall_secs: f64,
    cpu_seconds: f64,
    presented: u64,
    fed: u64,
    notes: String,
}

fn pace_60fps(next: &mut Instant) {
    let period = Duration::from_nanos(1_000_000_000 / 60);
    *next += period;
    let now = Instant::now();
    if *next > now {
        thread::sleep(*next - now);
    } else {
        *next = now;
    }
}

fn phase_cpu_upload(seconds: u64, shared: &Arc<OverlayShared>) -> PhaseOutcome {
    *shared.phase.lock().unwrap() = "cpu-upload (1080p Testpattern)".into();
    let sink = match GpuSink::new(OVERLAY_TITLE, (1920, 1080), chiaki_render::gpu_sink::GpuSinkConfig::default()) {
        Ok(s) => s,
        Err(err) => {
            return PhaseOutcome {
                name: "cpu-upload",
                wall_secs: 0.0,
                cpu_seconds: 0.0,
                presented: 0,
                fed: 0,
                notes: format!("SINK-FEHLER: {err}"),
            }
        }
    };
    let handle = sink.handle();
    handle.set_zoom(SinkZoom::Fit);
    eprintln!("[phase1] sink ready, loop start");

    let cpu0 = win::process_cpu_time();
    let t0 = Instant::now();
    let mut next = Instant::now();
    let mut frame_no: u64 = 0;
    while t0.elapsed() < Duration::from_secs(seconds) {
        let frame = generate_test_frame(frame_no, 1920, 1080);
        handle.submit_cpu(frame);
        frame_no += 1;
        pace_60fps(&mut next);
    }
    let wall = t0.elapsed().as_secs_f64();
    let stats = handle.stats_values();
    eprintln!("[phase1] done: presented={}", stats.frames_presented);
    drop(sink); // Render-Thread sauber beenden vor der nächsten Phase.
    eprintln!("[phase1] sink joined");

    PhaseOutcome {
        name: "cpu-upload",
        wall_secs: wall,
        cpu_seconds: win::process_cpu_time() - cpu0,
        presented: stats.frames_presented,
        fed: frame_no,
        notes: format!(
            "cpu-uploads {}, dropped {}, last_draw {} µs",
            stats.cpu_uploads, stats.frames_dropped, stats.last_draw_us
        ),
    }
}

fn find_stream_file() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("CHIAKI_SPIKE_H265") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    [
        PathBuf::from("live-test/stream.h265"),
        PathBuf::from("../live-test/stream.h265"),
        PathBuf::from("F:/projekte/chiaki-rs/live-test/stream.h265"),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

/// Dekodiert die stream.h265-Access-Units mit 60-fps-Wall-Clock-Pacing (Datei
/// wird geloopt). `on_frame` bekommt JEDEN dekodierten Frame + den Decoder
/// (für cuda_context etc.); Rückgabe true = Frame wurde zur Anzeige übergeben.
fn decode_loop(
    seconds: u64,
    backend: HwBackend,
    opts: DecoderOpts,
    mut on_frame: impl FnMut(&mut Decoder, &DecodedFrame) -> bool,
) -> (u64, u64, u64, String, Option<Decoder>) {
    let Some(path) = find_stream_file() else {
        return (0, 0, 0, "stream.h265 nicht gefunden (live-test/)".into(), None);
    };
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(err) => return (0, 0, 0, format!("Lesefehler {path:?}: {err}"), None),
    };
    let aus = split_access_units(&data);
    if aus.is_empty() {
        return (0, 0, 0, "keine Access Units erkannt".into(), None);
    }
    eprintln!("[decode-loop] Decoder::new start");
    let mut decoder = match Decoder::new_opts(chiaki_core::Codec::H265, backend, 60, opts) {
        Ok(d) => d,
        Err(err) => return (0, 0, 0, format!("Decoder-Init fehlgeschlagen: {err:?}"), None),
    };
    eprintln!("[decode-loop] Decoder ready");
    if backend != HwBackend::Cuda && decoder.used_hw_backend().is_none() {
        return (
            0,
            0,
            0,
            format!("{backend:?} nicht verfügbar (Software-Fallback) — Phase ohne GPU-Pfad"),
            None,
        );
    }

    let t0 = Instant::now();
    let mut next = Instant::now();
    let (mut fed, mut decoded, mut frames_seen): (u64, u64, u64) = (0, 0, 0);
    let mut note = String::new();
    'outer: while t0.elapsed() < Duration::from_secs(seconds) {
        for au in &aus {
            if t0.elapsed() >= Duration::from_secs(seconds) {
                break 'outer;
            }
            fed += 1;
            match decoder.decode_packet(au) {
                Ok(Some(frame)) => {
                    decoded += 1;
                    if decoded == 1 {
                        eprintln!("[decode-loop] first frame decoded");
                    }
                    if on_frame(&mut decoder, &frame) {
                        frames_seen += 1;
                    }
                    if decoded == 1 {
                        eprintln!("[decode-loop] first frame handled");
                    }
                    pace_60fps(&mut next);
                }
                Ok(None) => {}
                Err(err) => {
                    if note.is_empty() {
                        note = format!("Decode-Fehler: {err:?}");
                    }
                }
            }
        }
    }
    let backend_name = decoder
        .used_hw_backend()
        .map(|b| format!("{b:?}"))
        .unwrap_or_else(|| "Software".into());
    note.push_str(&format!(" [Backend: {backend_name}, {} AUs]", aus.len()));
    (fed, decoded, frames_seen, note, Some(decoder))
}

fn phase_d3d11va(seconds: u64, shared: &Arc<OverlayShared>) -> PhaseOutcome {
    *shared.phase.lock().unwrap() = "d3d11va (GPU-GPU Copy)".into();
    let Ok(sink) = GpuSink::new(OVERLAY_TITLE, (1920, 1080), chiaki_render::gpu_sink::GpuSinkConfig::default()) else {
        return PhaseOutcome {
            name: "d3d11va",
            wall_secs: 0.0,
            cpu_seconds: 0.0,
            presented: 0,
            fed: 0,
            notes: "SINK-FEHLER".into(),
        };
    };
    eprintln!("[phase2] sink ready");
    let handle = sink.handle();
    let opts = DecoderOpts {
        raw_hw_output: true,
        // +1-Referenzen: FFmpeg releast Device+Context beim Decoder-Drop.
        d3d11_device: handle.d3d11_device_addref(),
        d3d11_device_context: handle.d3d11_context_addref(),
    };
    let handle_for_cb = handle.clone();
    let cpu0 = win::process_cpu_time();
    let t0 = Instant::now();
    let (fed, decoded, submitted, mut note, decoder) = decode_loop(
        seconds,
        HwBackend::D3D11Va,
        opts,
        move |_decoder, frame| match frame.memory {
            FrameMemory::D3d11Texture { texture, subresource } => {
                // SAFETY: Textur stammt aus dem Decoder (gleiches Device wie
                // der Sink; DecodedFrame-Vertrag: gültig bis zum nächsten
                // Decode — die Kopie im Render-Thread läuft in Einreihenfolge
                // VOR jeder späteren Decode-Nutzung derselben Surface).
                unsafe { handle_for_cb.submit_d3d11(texture, subresource) };
                true
            }
            other => {
                tracing::warn!("d3d11va-Phase: unerwartetes Frame-Memory {other:?}");
                false
            }
        },
    );
    let wall = t0.elapsed().as_secs_f64();
    let stats = handle.stats_values();
    let lost = handle.is_lost();
    eprintln!("[phase2] done: presented={} fed={fed}", stats.frames_presented);
    drop(sink);
    eprintln!("[phase2] sink joined");
    drop(decoder); // Decoder NACH dem Sink/Render-Thread freigeben.
    if lost {
        note.push_str(" [DEVICE LOST]");
    }

    PhaseOutcome {
        name: "d3d11va",
        wall_secs: wall,
        cpu_seconds: win::process_cpu_time() - cpu0,
        presented: stats.frames_presented,
        fed,
        notes: format!(
            "decoded {decoded}, submitted {submitted}, d3d11-copies {}, dropped {}, {note}",
            stats.d3d11_copies, stats.frames_dropped
        ),
    }
}

fn phase_cuda_vsr(seconds: u64, shared: &Arc<OverlayShared>) -> PhaseOutcome {
    *shared.phase.lock().unwrap() = "cuda-vsr (Interop)".into();
    let sdk = std::env::var_os("CHIAKI_VSR_SDK_DIR").map(PathBuf::from);
    // Zustand der Phase (vom Frame-Callback aufgebaut):
    let mut vsr: Option<VsrUpscaler> = None;
    let mut sink: Option<GpuSink> = None;
    let mut handle: Option<GpuSinkHandle> = None;
    let mut interop: Option<CudaD3d11Interop> = None;
    let mut setup_error: Option<String> = None;
    let mut cuda_ctx: *mut c_void = std::ptr::null_mut();
    let mut cuda_stream: *mut c_void = std::ptr::null_mut();

    let cpu0 = win::process_cpu_time();
    let t0 = Instant::now();
    let (fed, decoded, written, mut note, decoder) = decode_loop(
        seconds,
        HwBackend::Cuda,
        DecoderOpts {
            raw_hw_output: true,
            ..Default::default()
        },
        |decoder, frame| {
            // Erstframe: VSR init (Output-Größe) → Sink mit Output-Größe →
            // Interop-Registrierung der BGRA-Textur. WICHTIG: VSR nur EINMAL
            // initialisieren (ein init+destroy-Zyklus invalidiert den
            // Decoder-CUDA-Kontext für erneutes init — rc=201 beobachtet);
            // schlägt Sink/Interop fehl, bleibt der Upscaler erhalten und der
            // Sink-Aufbau wird in späteren Frames wiederholt.
            if vsr.is_none() {
                let ctx = decoder.cuda_context().unwrap_or(std::ptr::null_mut());
                let stream = decoder.cuda_stream().unwrap_or(std::ptr::null_mut());
                eprintln!("[phase3] VSR init start (ctx non-null: {})", !ctx.is_null());
                let mut up = VsrUpscaler::new(sdk.clone());
                if !up.init(frame, ctx, stream, 200, None) {
                    setup_error = Some(format!(
                        "VSR-Init fehlgeschlagen: {:?}",
                        up.last_error().unwrap_or("?")
                    ));
                    return false;
                }
                eprintln!("[phase3] VSR init OK");
                vsr = Some(up);
                cuda_ctx = ctx;
                cuda_stream = stream;
                return false; // Erstframe nicht zählen.
            }
            if sink.is_none() {
                eprintln!("[phase3] sink creation start");
                let Some(up) = vsr.as_ref() else { return false };
                let (out_w, out_h) = up.output_size();
                match GpuSink::new(OVERLAY_TITLE, (out_w, out_h), chiaki_render::gpu_sink::GpuSinkConfig::default()) {
                    Ok(s) => {
                        let h = s.handle();
                        h.set_zoom(SinkZoom::Fit);
                        match CudaD3d11Interop::new(cuda_ctx, cuda_stream, h.rgba_texture()) {
                            Ok(inter) => {
                                interop = Some(inter);
                                handle = Some(h);
                                sink = Some(s);
                                tracing::info!(
                                    "CUDA-VSR-Phase: Sink {}x{} + Interop bereit",
                                    out_w,
                                    out_h
                                );
                            }
                            Err(err) => {
                                setup_error = Some(format!("Interop-Init fehlgeschlagen: {err}"));
                            }
                        }
                    }
                    Err(err) => setup_error = Some(format!("Sink-Fehler: {err}")),
                }
                return false;
            }
            let (Some(up), Some(h), Some(inter)) =
                (vsr.as_mut(), handle.as_ref(), interop.as_mut())
            else {
                return false;
            };
            let ok = match frame.memory {
                FrameMemory::CudaDevice => up.process_frame_gpu(
                    frame.planes[0].as_ptr() as *const c_void,
                    frame.planes[0].stride,
                    frame.planes[1].as_ptr() as *const c_void,
                    frame.planes[1].stride,
                    frame.width,
                    frame.height,
                    frame.pts,
                    frame.duration,
                    frame.frames_lost,
                    frame.recovered,
                ),
                _ => false,
            };
            if ok {
                if let Some(src_img) = up.gpu_rgba_image() {
                    // Interop-Schreibvorgang: Textur gegen Render-Thread sperren.
                    let guard = h.interop_lock();
                    // SAFETY: src_img ist das SDK-GPU-Image (Decoder-Kontext,
                    // gerade synchronisiert); D3D11 nutzt die Textur nicht
                    // (interop_lock gehalten).
                    let result = unsafe { inter.write_from(src_img) };
                    drop(guard);
                    if result.is_ok() {
                        h.submit_rgba_ready();
                        return true;
                    } else {
                        tracing::warn!("Interop-Schreibvorgang: {:?}", result.err());
                    }
                }
            }
            false
        },
    );
    let wall = t0.elapsed().as_secs_f64();
    let stats = handle
        .as_ref()
        .map(|h| h.stats_values())
        .unwrap_or_default();
    let lost = handle.as_ref().map(|h| h.is_lost()).unwrap_or(false);
    // WICHTIG: CUDA-abhängige Objekte VOR dem Decoder freigeben — der
    // Decoder-Drop zerstört den CUDA-Kontext (FFmpeg cuCtxDestroy), danach
    // würde VSR-/Interop-Teardown auf totem Kontext segfaulten.
    drop(interop);
    drop(vsr);
    drop(sink);
    drop(decoder);
    if let Some(err) = setup_error {
        note.push_str(&format!(" {err}"));
    }
    if lost {
        note.push_str(" [DEVICE LOST]");
    }

    PhaseOutcome {
        name: "cuda-vsr",
        wall_secs: wall,
        cpu_seconds: win::process_cpu_time() - cpu0,
        presented: stats.frames_presented,
        fed,
        notes: format!(
            "decoded {decoded}, interop-writes {written}, rgba-presents {}, dropped {}, {note}",
            stats.rgba_presents, stats.frames_dropped
        ),
    }
}

// ---------------------------------------------------------------------------

fn main() {
    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(12);
    ensure_dev_env();

    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    println!("Spike GPU: {seconds} s je Phase (cpu-upload, d3d11va, cuda-vsr)");

    let shared = Arc::new(OverlayShared {
        phase: Mutex::new("Start…".into()),
        fps: Mutex::new(0.0),
        ui_frames: AtomicU64::new(0),
    });

    // Phase-Treiber im Hintergrund; gpui-Overlay auf dem Main-Thread.
    let shared_driver = Arc::clone(&shared);
    thread::spawn(move || {
        let mut outcomes: Vec<PhaseOutcome> = Vec::new();
        eprintln!("[driver] phase1 start");
        outcomes.push(phase_cpu_upload(seconds, &shared_driver));
        eprintln!("[driver] phase2 start");
        outcomes.push(phase_d3d11va(seconds, &shared_driver));
        eprintln!("[driver] phase3 start");
        outcomes.push(phase_cuda_vsr(seconds, &shared_driver));
        eprintln!("[driver] all phases done");

        let mut report = String::new();
        let _ = writeln!(report, "# Spike GPU — GPU-residenter Videopfad (Variante b)\n");
        let _ = writeln!(
            report,
            "Automatisch erzeugt vom Spike-Binary (`cargo run -p chiaki-render --bin spike-gpu --release -- {seconds}`)."
        );
        let _ = writeln!(
            report,
            "Overlay: gpui-Fenster (DirectComposition, PREMULTIPLIED) ÜBER dem D3D11-Video-Sink-Fenster;\nungepixelte Bereiche transparent (HUD sichtbar, Video scheint durch).\n"
        );
        let _ = writeln!(
            report,
            "| Phase | Wandzeit s | presented | fed | FPS (presented/Wand) | Prozess-CPU Kerne | Notizen |"
        );
        let _ = writeln!(report, "|---|---|---|---|---|---|---|");
        for o in &outcomes {
            let fps = if o.wall_secs > 0.0 {
                o.presented as f64 / o.wall_secs
            } else {
                0.0
            };
            let cores = if o.wall_secs > 0.0 {
                o.cpu_seconds / o.wall_secs
            } else {
                0.0
            };
            let _ = writeln!(
                report,
                "| {} | {:.1} | {} | {} | {fps:.1} | {cores:.2} | {} |",
                o.name, o.wall_secs, o.presented, o.fed, o.notes
            );
        }
        let path = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("spike-gpu-results.md");
        let _ = std::fs::write(&path, &report);
        println!("Ergebnisse → {}", path.display());
        println!("{report}");
        *shared_driver.phase.lock().unwrap() = "FERTIG — Fenster schließen zum Beenden".into();
    });

    let shared_view = Arc::clone(&shared);
    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1600.0), px(900.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(OVERLAY_TITLE.into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_window, cx| {
                cx.new(|_cx| OverlayView {
                    shared: Arc::clone(&shared_view),
                })
            },
        )
        .expect("Overlay-Fenster konnte nicht geöffnet werden");
    });
}
