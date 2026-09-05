//! Spike S1 — "Video in GPUI" (hartes Phase-0-Gate des chiaki-rs Rebuilds).
//!
//! Treibt ein 60-fps-NV12-Testpattern (scrollendes Checkerboard + bewegter
//! Kreis + Frame-Counter-Graubalken) durch den RenderImage-Pfad von gpui 0.2.2:
//!
//!   Producer-Thread (60 fps)  →  NV12-Frame (RAM)
//!     → CPU-Konvertierung NV12→BGRA (BT.601 limited)
//!     → Arc<RenderImage> (image::Frame, Polychrom-Atlas, B8G8R8A8_UNORM)
//!     → Window::paint_image (Atlas-Upload + Sprite)
//!     → Window::drop_image (altes Atlas-Tile freigeben)
//!
//! Gemessen wird pro Auflösung (1080p, 4K — je 30 s): NV12→BGRA-Zeit,
//! RenderImage-Wrap-Zeit, paint_image-Zeit (enthält GPU-Upload), drop_image-Zeit,
//! End-to-End-FPS des GPUI-Render-Loops, verworfene Frames sowie die
//! Prozess-CPU-Last (GetProcessTimes). Am Ende schreibt der Spike eine
//! Zusammenfassung nach stdout und in `spike-s1-results.md`.
//!
//! Der GPUI-Sprite-Atlas evictet nichts selbst und `RenderImage` erlaubt kein
//! In-Place-Update eines Tiles — jede Frame-Aktualisierung ist ein neues Tile
//! (ImageId) + Upload; `drop_image` gibt das alte frei. Genau dieser
//! "RenderImage-Pfad" ist Variante (a) der Spike-S1-Entscheidung.

use chiaki_render::nv12::{NV12Frame, nv12_to_bgra_parallel};
use chiaki_render::presenter::{SampleSummary, VideoPresenter};
use gpui::{
    App, Application, Bounds, Context, Render, TitlebarOptions, Window, WindowBounds,
    WindowOptions, div, prelude::*, px, size,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};
use std::{fmt::Write as _, thread};

// ---------------------------------------------------------------------------
// Windows-FFI: Prozess-CPU-Zeit (GetProcessTimes) — nur für die Messung.
// ---------------------------------------------------------------------------

mod sys {
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

// ---------------------------------------------------------------------------
// Testpattern-Generierung (NV12 im RAM)
// ---------------------------------------------------------------------------

const CHECKER_LIGHT: u8 = 110;
const CHECKER_DARK: u8 = 30;
const CIRCLE_LUMA: u8 = 220;
const CIRCLE_U: u8 = 90; // U niedrig + V hoch → rötlich (BT.601)
const CIRCLE_V: u8 = 220;

/// Erzeugt das NV12-Testpattern für Frame `frame_no`:
/// * 32-px-Checkerboard, scrollt 1 px/Frame nach rechts,
/// * Kreis auf Lissajous-Bahn (hell im Luma, rot getintet im Chroma),
/// * Color-Cycling-Balken oben (V = frame % 256),
/// * Frame-Counter unten als Grauwert-Balken (Y = frame % 256).
fn generate_test_frame(frame_no: u64, width: u32, height: u32) -> NV12Frame {
    let mut frame = NV12Frame::new(width, height).expect("pattern dimensions are valid");
    let w = width as usize;
    let h = height as usize;
    let y_stride = frame.y_stride;
    let uv_stride = frame.uv_stride;
    let (y_len, _) = (y_stride * h, uv_stride * (h / 2));
    let (y, uv) = frame.data.split_at_mut(y_len);

    // Checkerboard 32 px, horizontal scrollierend.
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

    // Color-Cycling-Balken oben (16 px hoch, nur Chroma).
    for uv_row in 0..(8usize.min(h / 2)) {
        let uv_start = uv_row * uv_stride;
        for col in 0..w / 2 {
            uv[uv_start + col * 2] = 128;
            uv[uv_start + col * 2 + 1] = (frame_no % 256) as u8;
        }
    }

    // Frame-Counter als Graubalken unten (32 px hoch).
    let counter = (frame_no % 256) as u8;
    for row in h.saturating_sub(32)..h {
        let row_start = row * y_stride;
        y[row_start..row_start + w].fill(counter);
    }

    // Bewegter Kreis (Lissajous), überschreibt Checker/Counter.
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
        // Chroma-Halbzeile des Kreises rot tinten.
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

/// Producer-Thread: erzeugt das Pattern mit 60 fps und legt es im Presenter ab.
fn run_producer(
    presenter: VideoPresenter,
    width: u32,
    height: u32,
    stop: Arc<AtomicBool>,
    gen_samples: Arc<Mutex<Vec<u32>>>,
) {
    const TARGET_FPS: u64 = 60;
    let period = Duration::from_nanos(1_000_000_000 / TARGET_FPS);
    let mut next_frame_time = Instant::now();
    let mut frame_no: u64 = 0;
    while !stop.load(Ordering::Relaxed) {
        let start = Instant::now();
        let frame = generate_test_frame(frame_no, width, height);
        presenter.set_frame(frame);
        let gen_us = u32::try_from(start.elapsed().as_nanos() / 1_000).unwrap_or(u32::MAX);
        gen_samples.lock().unwrap().push(gen_us);
        frame_no += 1;

        next_frame_time += period;
        let now = Instant::now();
        if next_frame_time > now {
            thread::sleep(next_frame_time - now);
        } else {
            // Hinten dran geraten: nachjustieren statt aufholen wollen.
            next_frame_time = now;
        }
    }
}

// ---------------------------------------------------------------------------
// GPUI-Fenster: rendert jeden Frame und zählt FPS
// ---------------------------------------------------------------------------

struct SpikeWindow {
    presenter: VideoPresenter,
    render_frames_total: Arc<AtomicU64>,
    fps_samples: Arc<Mutex<Vec<f64>>>,
    current_fps: f64,
    frames_this_second: u64,
    last_second: Instant,
}

impl Render for SpikeWindow {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // Ende-zu-Ende-Frame-Zählung (GPUI-Render-Loop).
        self.frames_this_second += 1;
        self.render_frames_total.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        let since_second = now.duration_since(self.last_second);
        if since_second >= Duration::from_secs(1) {
            self.current_fps = self.frames_this_second as f64 / since_second.as_secs_f64();
            self.fps_samples.lock().unwrap().push(self.current_fps);
            self.frames_this_second = 0;
            self.last_second = now;
        }
        // Kontinuierlicher Render-Loop (dokumentierter Videopfad in gpui 0.2.2).
        window.request_animation_frame();

        let image = self.presenter.take_image(window);
        let mut layout = div().size_full().bg(gpui::black());
        if let Some(image) = image {
            layout = layout.child(self.presenter.video_element(image));
        }

        let (w, h) = self.presenter.size();
        layout.child(
            div()
                .absolute()
                .bottom_2()
                .left_2()
                .p_2()
                .rounded_sm()
                .bg(gpui::black().opacity(0.65))
                .text_size(px(16.0))
                .text_color(gpui::white())
                .child(format!(
                    "Spike S1 — {w}x{h} — {:.1} fps render loop",
                    self.current_fps
                )),
        )
    }
}

// ---------------------------------------------------------------------------
// Phasen-Orchestrierung + Ergebnisbericht
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct PhaseSpec {
    width: u32,
    height: u32,
    seconds: u64,
}

#[derive(Clone)]
struct PhaseResult {
    spec: PhaseSpec,
    wall_secs: f64,
    cpu_seconds: f64,
    render_frames_total: u64,
    fps_samples: Vec<f64>,
    stats: chiaki_render::presenter::PresenterStatsSnapshot,
    gen_summary: SampleSummary,
}

fn summary_line(name: &str, s: &SampleSummary) -> String {
    format!(
        "| {name} | {:.1} | {} | {} | {} | {} |\n",
        s.mean_us, s.p50_us, s.p95_us, s.p99_us, s.max_us
    )
}

fn build_report(results: &[PhaseResult], benches: &[ConvBench]) -> String {
    let mut report = String::new();
    report.push_str("# Spike S1 — „Video in GPUI“ — Messergebnisse\n\n");
    report.push_str(
        "Automatisch erzeugt vom Spike-Binary (`cargo run -p chiaki-render --bin spike-s1 --release`).\n\
         Pfad: gpui 0.2.2 (crates.io, DirectX-11-Backend) — `RenderImage`-Pro-Frame-Upload + `drop_image`.\n\
         Overhead-Kriterium: **NV12→BGRA + Upload < 2 ms @ 1080p60**.\n\n",
    );

    for phase in results {
        let s = &phase.stats;
        let overhead_mean_ms = (s.alloc_us.mean_us
            + s.conversion_us.mean_us
            + s.wrap_us.mean_us
            + s.paint_us.mean_us)
            / 1000.0;
        let overhead_p95_ms = (s.alloc_us.p95_us
            + s.conversion_us.p95_us
            + s.wrap_us.p95_us
            + s.paint_us.p95_us) as f64
            / 1000.0;
        let overhead_p99_ms = (s.alloc_us.p99_us
            + s.conversion_us.p99_us
            + s.wrap_us.p99_us
            + s.paint_us.p99_us) as f64
            / 1000.0;
        let cpu_cores = if phase.wall_secs > 0.0 {
            phase.cpu_seconds / phase.wall_secs
        } else {
            0.0
        };
        let fps_line = phase
            .fps_samples
            .iter()
            .map(|f| format!("{f:.1}"))
            .collect::<Vec<_>>()
            .join(", ");

        let _ = writeln!(
            report,
            "## {}x{} @ 60 fps — Phase {:.1} s\n",
            phase.spec.width, phase.spec.height, phase.wall_secs
        );
        let _ = writeln!(
            report,
            "- GPUI-Render-Loop: **{:.1} fps** gesamt ({} Frames), je Sekunde: [{}]",
            phase.render_frames_total as f64 / phase.wall_secs.max(0.001),
            phase.render_frames_total,
            fps_line
        );
        let _ = writeln!(
            report,
            "- Frames generiert (Producer 60 fps): {} — davon verworfen (Queue überrollt): {} — konvertiert+dargestellt: {} (Konvertierungsfehler: {})",
            s.frames_generated, s.frames_dropped, s.frames_presented, s.conversion_errors
        );
        let _ = writeln!(
            report,
            "- Prozess-CPU-Last: **{:.2} Kerne** ({:.1} s CPU / {:.1} s Wandzeit)",
            cpu_cores, phase.cpu_seconds, phase.wall_secs
        );
        let _ = writeln!(
            report,
            "- Testpattern-Generierung (Producer-Thread): mean {:.0} µs, max {} µs\n",
            phase.gen_summary.mean_us, phase.gen_summary.max_us
        );
        report.push_str("| Messpunkt | mean µs | p50 µs | p95 µs | p99 µs | max µs |\n");
        report.push_str("|---|---|---|---|---|---|\n");
        report.push_str(&summary_line("Zielbuffer-Allokation (8,3 MB)", &s.alloc_us));
        report.push_str(&summary_line("NV12→BGRA (CPU)", &s.conversion_us));
        report.push_str(&summary_line("BGRA→RenderImage (Wrap)", &s.wrap_us));
        report.push_str(&summary_line("paint_image (Atlas-Upload+Draw-Insert)", &s.paint_us));
        report.push_str(&summary_line("drop_image (Atlas-Freigabe)", &s.drop_image_us));
        let _ = writeln!(
            report,
            "\n- **Overhead pro Frame (Conv+Alloc+Wrap+Paint): mean {:.2} ms, p95 {:.2} ms, p99 {:.2} ms** → Kriterium < 2 ms @ 1080p60: {}\n",
            overhead_mean_ms,
            overhead_p95_ms,
            overhead_p99_ms,
            if phase.spec.width <= 1920 {
                if overhead_p95_ms < 2.0 { "ERFÜLLT" } else { "NICHT ERFÜLLT" }
            } else {
                "(Kriterium gilt für 1080p60)"
            }
        );
    }

    if !benches.is_empty() {
        report.push_str("## Mikro-Benchmark NV12→BGRA-Konvertierung (reine Schleife, ohne Fenster)\n\n");
        report.push_str("| Auflösung | Variante | mean µs | p50 µs | p95 µs | p99 µs | max µs | Durchsatz Mpx/s |\n");
        report.push_str("|---|---|---|---|---|---|---|---|\n");
        for bench in benches {
            let mp = |s: &SampleSummary| (bench.width as f64 * bench.height as f64 / 1e6) / (s.mean_us / 1e6);
            let _ = writeln!(
                report,
                "| {}x{} | single-thread | {} | {} | {} | {} | {} | {:.0} |",
                bench.width, bench.height,
                bench.serial.mean_us as u64, bench.serial.p50_us, bench.serial.p95_us,
                bench.serial.p99_us, bench.serial.max_us, mp(&bench.serial)
            );
            let _ = writeln!(
                report,
                "| {}x{} | parallel ({} Threads) | {} | {} | {} | {} | {} | {:.0} |",
                bench.width, bench.height, bench.workers,
                bench.parallel.mean_us as u64, bench.parallel.p50_us, bench.parallel.p95_us,
                bench.parallel.p99_us, bench.parallel.max_us, mp(&bench.parallel)
            );
        }
        report.push_str("\n");
    }
    report
}

fn parse_args() -> (Vec<PhaseSpec>, Option<std::path::PathBuf>) {
    let args: Vec<String> = std::env::args().collect();
    let mut seconds: u64 = 30;
    let mut only: Option<u32> = None;
    let mut out: Option<std::path::PathBuf> = None;
    let mut iter = args.iter().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--seconds" => {
                seconds = iter.next().and_then(|v| v.parse().ok()).unwrap_or(30);
            }
            "--1080p-only" => only = Some(1920),
            "--4k-only" => only = Some(3840),
            "--out" => {
                out = iter.next().map(std::path::PathBuf::from);
            }
            other => {
                eprintln!("Unbekanntes Argument: {other} (verstanden: --seconds N, --1080p-only, --4k-only, --out PATH)");
            }
        }
    }
    let specs = match only {
        Some(1920) => vec![PhaseSpec { width: 1920, height: 1080, seconds }],
        Some(_) => vec![PhaseSpec { width: 3840, height: 2160, seconds }],
        None => vec![
            PhaseSpec { width: 1920, height: 1080, seconds },
            PhaseSpec { width: 3840, height: 2160, seconds },
        ],
    };
    (specs, out)
}

fn percentile_summary(samples: &[u32]) -> SampleSummary {
    if samples.is_empty() {
        return SampleSummary::default();
    }
    let mut sorted: Vec<u32> = samples.to_vec();
    sorted.sort_unstable();
    let count = sorted.len();
    let percentile = |p: f64| sorted[((count as f64 - 1.0) * p).round() as usize];
    SampleSummary {
        count,
        mean_us: sorted.iter().map(|&v| v as f64).sum::<f64>() / count as f64,
        p50_us: percentile(0.50),
        p95_us: percentile(0.95),
        p99_us: percentile(0.99),
        max_us: sorted[count - 1],
    }
}

// ---------------------------------------------------------------------------
// Mikro-Benchmark: NV12→BGRA serial vs. parallel (ohne Fenster/GPUI)
// ---------------------------------------------------------------------------

struct ConvBench {
    width: u32,
    height: u32,
    workers: usize,
    serial: SampleSummary,
    parallel: SampleSummary,
}

fn us(d: Duration) -> u32 {
    u32::try_from(d.as_nanos() / 1_000).unwrap_or(u32::MAX)
}

/// Misst die reine Konvertierungs-Schleife (vorallokierter Zielbuffer, kein
/// Allokations- und Upload-Rauschen), 120 Iterationen je Variante.
fn bench_conversion() -> Vec<ConvBench> {
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(8);
    let mut out = Vec::new();
    for (width, height) in [(1920u32, 1080u32), (3840u32, 2160u32)] {
        let frame = generate_test_frame(0, width, height);
        let mut dst = vec![0u8; frame.bgra_len()];
        for _ in 0..10 {
            let _ = frame.to_bgra(&mut dst);
        }

        let mut samples = Vec::with_capacity(120);
        for _ in 0..120 {
            let start = Instant::now();
            let _ = frame.to_bgra(&mut dst);
            samples.push(us(start.elapsed()));
        }
        let serial = percentile_summary(&samples);

        samples.clear();
        for _ in 0..120 {
            let start = Instant::now();
            let _ = nv12_to_bgra_parallel(
                frame.y_plane(),
                frame.y_stride,
                frame.uv_plane(),
                frame.uv_stride,
                width,
                height,
                &mut dst,
                workers,
            );
            samples.push(us(start.elapsed()));
        }
        let parallel = percentile_summary(&samples);
        out.push(ConvBench { width, height, workers, serial, parallel });
    }
    out
}

fn main() {
    let (specs, out_path) = parse_args();
    let out_path = out_path.unwrap_or_else(|| {
        std::path::PathBuf::from("F:\\projekte\\chiaki-rs\\spike-s1-results.md")
    });
    println!(
        "Spike S1: {:?} (je Phase: {} s) → {}",
        specs.iter().map(|s| format!("{}x{}", s.width, s.height)).collect::<Vec<_>>(),
        specs.first().map(|s| s.seconds).unwrap_or(30),
        out_path.display()
    );

    let results: Arc<Mutex<Vec<PhaseResult>>> = Arc::new(Mutex::new(Vec::new()));
    let results_main = Arc::clone(&results);

    Application::new().run(move |cx: &mut App| {
        let results = Arc::clone(&results);
        cx.spawn(async move |cx| {
            // Wichtig: gpui auf Windows beendet die App (PostQuitMessage), sobald
            // das LETZTE Fenster geschlossen wird. Deshalb wird pro Phasenwechsel
            // erst das neue Fenster geöffnet und erst danach das alte geschlossen.
            let mut previous_window: Option<gpui::AnyWindowHandle> = None;
            let mut phase_error: Option<String> = None;

            for spec in specs {
                let presenter = VideoPresenter::new(spec.width, spec.height);
                let stop = Arc::new(AtomicBool::new(false));
                let render_frames_total = Arc::new(AtomicU64::new(0));
                let fps_samples: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));
                let gen_samples: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));

                // Fenster öffnen (VSync-Thread von GPUI treibt RedrawWindow).
                let handle = match cx
                    .update(|cx: &mut App| {
                        let bounds = Bounds::centered(
                            None,
                            size(px(spec.width as f32), px(spec.height as f32)),
                            cx,
                        );
                        cx.open_window(
                            WindowOptions {
                                window_bounds: Some(WindowBounds::Windowed(bounds)),
                                titlebar: Some(TitlebarOptions {
                                    title: Some(
                                        format!("Spike S1 — {}x{}", spec.width, spec.height)
                                            .into(),
                                    ),
                                    ..Default::default()
                                }),
                                ..Default::default()
                            },
                            |_, cx| {
                                cx.new(|_| SpikeWindow {
                                    presenter: presenter.clone(),
                                    render_frames_total: Arc::clone(&render_frames_total),
                                    fps_samples: Arc::clone(&fps_samples),
                                    current_fps: 0.0,
                                    frames_this_second: 0,
                                    last_second: Instant::now(),
                                })
                            },
                        )
                    })
                    .map_err(|err| format!("open_window failed: {err}"))
                    .and_then(|handle| handle.map_err(|err| format!("open_window failed: {err}")))
                {
                    Ok(handle) => handle,
                    Err(err) => {
                        phase_error = Some(err);
                        break;
                    }
                };

                // Vorheriges Fenster erst jetzt schließen (nie 0 Fenster!).
                if let Some(previous) = previous_window.take() {
                    if let Err(err) = cx.update_window(previous, |_, window, _| {
                        window.remove_window()
                    }) {
                        phase_error = Some(format!("remove_window failed: {err}"));
                        break;
                    }
                    cx.background_executor().timer(Duration::from_millis(500)).await;
                }

                // Producer-Thread starten und Phase laufen lassen.
                let producer = {
                    let presenter = presenter.clone();
                    let stop = Arc::clone(&stop);
                    let gen_samples = Arc::clone(&gen_samples);
                    thread::spawn(move || {
                        run_producer(presenter, spec.width, spec.height, stop, gen_samples)
                    })
                };
                presenter.reset_stats();
                let cpu_before = sys::process_cpu_time();
                let phase_start = Instant::now();

                cx.background_executor()
                    .timer(Duration::from_secs(spec.seconds))
                    .await;

                let wall = phase_start.elapsed().as_secs_f64();
                let cpu = sys::process_cpu_time() - cpu_before;
                stop.store(true, Ordering::Relaxed);
                let _ = producer.join();

                let stats = presenter.stats();
                let gen = percentile_summary(&gen_samples.lock().unwrap());
                let total = render_frames_total.load(Ordering::Relaxed);
                let fps = fps_samples.lock().unwrap().clone();
                results.lock().unwrap().push(PhaseResult {
                    spec,
                    wall_secs: wall,
                    cpu_seconds: cpu,
                    render_frames_total: total,
                    fps_samples: fps,
                    stats,
                    gen_summary: gen,
                });

                previous_window = Some(handle.into());
            }

            // Letztes Fenster schließen → gpui beendet die App von selbst;
            // explizites quit() als Backup (fehler tolerant).
            if let Some(previous) = previous_window.take() {
                let _ = cx.update_window(previous, |_, window, _| window.remove_window());
            }
            let _ = cx.update(|cx| cx.quit());
            if let Some(err) = phase_error {
                Err(err)
            } else {
                Ok(())
            }
        })
        .detach();
    });

    // Nach dem Run: Konvertierungs-Mikro-Benchmark + Bericht schreiben.
    let benches = bench_conversion();
    let results = results_main.lock().unwrap().clone();
    if results.is_empty() {
        eprintln!("Spike S1: keine Ergebnisse gesammelt!");
        return;
    }
    let report = build_report(&results, &benches);
    println!("\n{report}");
    if let Err(err) = std::fs::write(&out_path, &report) {
        eprintln!("Konnte {} nicht schreiben: {err}", out_path.display());
        std::process::exit(1);
    }
    println!("Bericht geschrieben nach {}", out_path.display());
}
