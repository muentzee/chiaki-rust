//! VideoPresenter: stellt NV12-Frames als GPUI-Texturen dar (Spike-S1-Pfad).
//!
//! Architektur (Pfad "(a) RenderImage-Upload"):
//! 1. Decoder-/Producer-Thread: [`VideoPresenter::set_frame`] legt den aktuellen
//!    [`NV12Frame`] in einer 1-Frame-Queue ab (überschreibt, zählt verworfene).
//! 2. GPUI-Render-Thread: [`VideoPresenter::take_image`] holt den neuesten Frame,
//!    konvertiert NV12→BGRA (CPU), verpackt ihn als `Arc<gpui::RenderImage>`
//!    (`image::Frame` → Polychrom-Atlas, `DXGI_FORMAT_B8G8R8A8_UNORM`) und gibt
//!    das alte Bild über `Window::drop_image` wieder frei (der GPUI-Sprite-Atlas
//!    räumt nichts selbst auf — ohne `drop_image` wächst die VRAM-Nutzung mit
//!    jedem Frame).
//! 3. [`VideoFrameElement`] zeichnet das Bild via `Window::paint_image` und misst
//!    die Zeit dafür (Atlas-Upload via UpdateSubresource + Szene-Insert).
//!
//! Bekannte API-Grenze (gpui 0.2.2): `RenderImage` erlaubt kein In-Place-Update
//! eines bestehenden Atlas-Tiles (Daten privat, Tile-Key = `ImageId`); jede
//! Frame-Aktualisierung ist daher ein neues Tile + Upload + Freigabe. Genau
//! dieser Overhead wird im Spike gemessen.

use crate::nv12::NV12Frame;
use gpui::{
    App, Bounds, Corners, Element, ElementId, GlobalElementId, InspectorElementId, LayoutId,
    Pixels, RenderImage, Style, Window, prelude::*, relative,
};
use image::{Frame as ImageFrame, RgbaImage};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};
use tracing::warn;

// ---------------------------------------------------------------------------
// Statistik-Sammlung
// ---------------------------------------------------------------------------

/// Zeit-Messreihe (Mikrosekunden) mit Perzentil-Auswertung.
#[derive(Default)]
pub struct Samples {
    samples: Mutex<Vec<u32>>,
}

const MAX_SAMPLES: usize = 250_000;

impl Samples {
    fn record(&self, elapsed: Duration) {
        let us = u32::try_from(elapsed.as_nanos() / 1_000).unwrap_or(u32::MAX);
        let mut samples = self.samples.lock().unwrap();
        if samples.len() < MAX_SAMPLES {
            samples.push(us);
        }
    }

    fn clear(&self) {
        self.samples.lock().unwrap().clear();
    }

    /// Perzentil-Zusammenfassung (sortiert eine Kopie; für Statistik-Abfragen OK).
    pub fn summary(&self) -> SampleSummary {
        let samples = self.samples.lock().unwrap();
        SampleSummary::from_samples(&samples)
    }
}

/// Zusammenfassung einer Messreihe (Zeiten in Mikrosekunden).
#[derive(Debug, Clone, Copy, Default)]
pub struct SampleSummary {
    pub count: usize,
    pub mean_us: f64,
    pub p50_us: u32,
    pub p95_us: u32,
    pub p99_us: u32,
    pub max_us: u32,
}

impl SampleSummary {
    fn from_samples(samples: &[u32]) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        let mut sorted: Vec<u32> = samples.to_vec();
        sorted.sort_unstable();
        let count = sorted.len();
        let percentile = |p: f64| sorted[((count as f64 - 1.0) * p).round() as usize];
        Self {
            count,
            mean_us: sorted.iter().map(|&v| v as f64).sum::<f64>() / count as f64,
            p50_us: percentile(0.50),
            p95_us: percentile(0.95),
            p99_us: percentile(0.99),
            max_us: sorted[count - 1],
        }
    }
}

/// Interne Zähler + Messreihen des Presenters (von Element und Presenter geteilt).
#[derive(Default)]
pub(crate) struct PresenterStats {
    frames_generated: AtomicU64,
    frames_dropped: AtomicU64,
    frames_presented: AtomicU64,
    conversion_errors: AtomicU64,
    alloc_us: Samples,
    conversion_us: Samples,
    wrap_us: Samples,
    paint_us: Samples,
    drop_image_us: Samples,
}

/// Snapshot der Presenter-Statistik für eine Messphase.
#[derive(Debug, Clone, Copy, Default)]
pub struct PresenterStatsSnapshot {
    pub frames_generated: u64,
    pub frames_dropped: u64,
    pub frames_presented: u64,
    pub conversion_errors: u64,
    pub alloc_us: SampleSummary,
    pub conversion_us: SampleSummary,
    pub wrap_us: SampleSummary,
    pub paint_us: SampleSummary,
    pub drop_image_us: SampleSummary,
}

impl PresenterStats {
    pub(crate) fn record_paint(&self, elapsed: Duration) {
        self.paint_us.record(elapsed);
    }
}

// ---------------------------------------------------------------------------
// VideoPresenter
// ---------------------------------------------------------------------------

/// Präsentiert NV12-Frames in einem GPUI-Fenster als Texturen.
///
/// `Clone` teilt dieselbe innere Struktur — Producer-Thread und GPUI-View
/// halten je einen Klon.
#[derive(Clone)]
pub struct VideoPresenter {
    inner: Arc<PresenterInner>,
}

struct PresenterInner {
    width: u32,
    height: u32,
    queue: Mutex<Option<NV12Frame>>,
    previous: Mutex<Option<Arc<RenderImage>>>,
    stats: Arc<PresenterStats>,
}

impl VideoPresenter {
    /// Neuer Presenter für eine feste Videoauflösung.
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            inner: Arc::new(PresenterInner {
                width,
                height,
                queue: Mutex::new(None),
                previous: Mutex::new(None),
                stats: Arc::new(PresenterStats::default()),
            }),
        }
    }

    /// Videoauflösung (Breite, Höhe).
    pub fn size(&self) -> (u32, u32) {
        (self.inner.width, self.inner.height)
    }

    /// Legt den neuesten Frame ab (Producer-Thread). Überschreibt einen noch
    /// nicht konvertierten Frame (dieser zählt als `frames_dropped`).
    pub fn set_frame(&self, frame: NV12Frame) {
        self.inner.stats.frames_generated.fetch_add(1, Ordering::Relaxed);
        let mut queue = self.inner.queue.lock().unwrap();
        if queue.replace(frame).is_some() {
            self.inner.stats.frames_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Holt den neuesten Frame, konvertiert ihn nach BGRA und verpackt ihn als
    /// `RenderImage`. Gibt das vorherige Bild im GPUI-Atlas frei. Wenn gerade
    /// kein neuer Frame ansteht, wird das zuletzt präsentierte Bild zurückgegeben
    /// (Re-Paint ohne Upload).
    ///
    /// Muss auf dem GPUI-Render-Thread laufen (`Window`-Zugriff).
    pub fn take_image(&self, window: &mut Window) -> Option<Arc<RenderImage>> {
        let frame = self.inner.queue.lock().unwrap().take();
        let Some(frame) = frame else {
            return self.inner.previous.lock().unwrap().clone();
        };

        // 1) Zielbuffer (8,3 MB @ 1080p) — frisch pro Frame, weil der Besitz
        //    an RenderImage/image::Frame übergeht. Wird separat gemessen
        //    (Page-Commit-Kosten sind ein realer Teil des RenderImage-Pfads).
        let alloc_start = Instant::now();
        let mut bgra = vec![0u8; frame.bgra_len()];
        let alloc_us = alloc_start.elapsed();

        // 2) NV12 → BGRA (CPU, messbar)
        let conversion_start = Instant::now();
        if let Err(err) = frame.to_bgra(&mut bgra) {
            warn!("NV12→BGRA conversion failed: {err}");
            self.inner.stats.conversion_errors.fetch_add(1, Ordering::Relaxed);
            return self.inner.previous.lock().unwrap().clone();
        }
        let conversion_us = conversion_start.elapsed();

        // 2) BGRA → RenderImage (Allokation + nullkopierte Frame-Übernahme)
        let wrap_start = Instant::now();
        let rgba = RgbaImage::from_raw(frame.width, frame.height, bgra)?;
        let image = Arc::new(RenderImage::new([ImageFrame::new(rgba)]));
        let wrap_us = wrap_start.elapsed();

        // 3) Vorheriges Atlas-Tile freigeben (sonst VRAM-Wachstum)
        let previous = {
            let mut prev = self.inner.previous.lock().unwrap();
            prev.replace(image.clone())
        };
        if let Some(previous) = previous {
            let drop_start = Instant::now();
            if let Err(err) = window.drop_image(previous) {
                warn!("drop_image failed: {err}");
            }
            self.inner.stats.drop_image_us.record(drop_start.elapsed());
        }

        self.inner.stats.alloc_us.record(alloc_us);
        self.inner.stats.conversion_us.record(conversion_us);
        self.inner.stats.wrap_us.record(wrap_us);
        self.inner.stats.frames_presented.fetch_add(1, Ordering::Relaxed);
        Some(image)
    }

    /// Element, das `image` in den GPUI-Frame malt (inkl. Upload-Messung).
    pub fn video_element(&self, image: Arc<RenderImage>) -> VideoFrameElement {
        VideoFrameElement {
            image,
            stats: Arc::clone(&self.inner.stats),
        }
    }

    /// Statistik-Snapshot der aktuellen Phase.
    pub fn stats(&self) -> PresenterStatsSnapshot {
        let stats = &self.inner.stats;
        PresenterStatsSnapshot {
            frames_generated: stats.frames_generated.load(Ordering::Relaxed),
            frames_dropped: stats.frames_dropped.load(Ordering::Relaxed),
            frames_presented: stats.frames_presented.load(Ordering::Relaxed),
            conversion_errors: stats.conversion_errors.load(Ordering::Relaxed),
            alloc_us: stats.alloc_us.summary(),
            conversion_us: stats.conversion_us.summary(),
            wrap_us: stats.wrap_us.summary(),
            paint_us: stats.paint_us.summary(),
            drop_image_us: stats.drop_image_us.summary(),
        }
    }

    /// Setzt alle Zähler und Messreihen zurück (Phasenwechsel).
    pub fn reset_stats(&self) {
        let stats = &self.inner.stats;
        stats.frames_generated.store(0, Ordering::Relaxed);
        stats.frames_dropped.store(0, Ordering::Relaxed);
        stats.frames_presented.store(0, Ordering::Relaxed);
        stats.conversion_errors.store(0, Ordering::Relaxed);
        stats.alloc_us.clear();
        stats.conversion_us.clear();
        stats.wrap_us.clear();
        stats.paint_us.clear();
        stats.drop_image_us.clear();
    }
}

// ---------------------------------------------------------------------------
// Video-Element
// ---------------------------------------------------------------------------

/// GPUI-Element, das ein `RenderImage` flächendeckend malt und die
/// `paint_image`-Zeit (Atlas-Upload + Szene-Insert) misst.
pub struct VideoFrameElement {
    image: Arc<RenderImage>,
    stats: Arc<PresenterStats>,
}

impl IntoElement for VideoFrameElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for VideoFrameElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        // Füll das Eltern-Element (das StreamView-div) komplett aus.
        style.size.width = relative(1.0).into();
        style.size.height = relative(1.0).into();
        let layout_id = window.request_layout(style, [], cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        _cx: &mut App,
    ) {
        let start = Instant::now();
        let result = window.paint_image(bounds, Corners::default(), self.image.clone(), 0, false);
        self.stats.record_paint(start.elapsed());
        if let Err(err) = result {
            warn!("paint_image failed: {err}");
        }
    }
}
