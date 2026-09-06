// SPDX-License-Identifier: AGPL-3.0-only
//! GPU-Video-Sink (Spike-Variante b): eigenes Top-Level-Fenster mit D3D11-
//! Swapchain (BGRA8, FLIP_DISCARD, Present SyncInterval 0), gefüttert aus dem
//! Media-Thread — der GPUI-Render-Thread wird pro Videoframe NICHT mehr
//! belastet (kein Atlas-Upload, kein drop_image, keine Szenen-Renderkosten).
//!
//! ## Architektur
//! * **Render-Thread** (besitzt Fenster+Device+Swapchain+Shaders): Message-Loop
//!   (GetMessageW); geweckt wird er per `WM_APP_FRAME` (neuer Frame) oder
//!   `WM_TIMER` (50 ms Follow-Tick: Position/Größe des Overlay-Fensters
//!   verfolgen, Z-Ordnung sicherstellen, Swapchain-Resize).
//! * **Mailbox** (`Mutex<Option<FrameInput>>` + PostMessage-Wakeup): der
//!   Media-Thread legt den neuesten Frame ab (ältere werden überschrieben —
//!   wie die 1-Frame-Queue des [`crate::presenter::VideoPresenter`]).
//! * **Drei Frame-Quellen**, alle enden in denselben zwei Shader-Input-
//!   Texturen:
//!   1. `Cpu(NV12Frame)` — `UpdateSubresource` (Fallback; VSR-FrameBuf-Pfad).
//!   2. `D3d11 { texture, array_index }` — FFmpeg-D3D11VA-Dekode auf DEMSELBEN
//!      Device → `CopySubresourceRegion` (GPU-GPU, Zero-Copy). Die Quelle
//!      bleibt bis zum nächsten Decode gültig (DecodedFrame-Vertrag); die
//!      Kopie läuft im Render-Thread BEVOR der Media-Thread realistisch den
//!      Surface-Pool recycled (11+ Surfaces), und Immediate-Context-Befehle
//!      laufen in Einreihenfolge — die Kopie geht jeder späteren Decode-Nutzung
//!      derselben Surface also strukturell voraus.
//!   3. `BgraReady` — VSR-Output wurde per CUDA-D3D11-Interop bereits in die
//!      BGRA-Textur geschrieben (chiaki-media); nur noch zeichnen. Der
//!      [`GpuSinkHandle::interop_lock`] hält den Render-Thread während des
//!      CUDA-Mappings vom Zeichnen fern.
//! * **Overlay-Zusammenspiel**: das gpui-Fenster (DirectComposition,
//!   PREMULTIPLIED-Alpha, Clear auf [0,0,0,0]) bleibt das Top-Fenster; dieses
//!   Video-Fenster sitzt unmittelbar darunter. Unbemalte gpui-Bereiche sind
//!   transparent → Video scheint durch; HUD/Dialoge bleiben opak. Fallback
//!   (dokumentiert, nicht aktiv): WS_EX_LAYERED+LWA_COLORKEY via
//!   [`sys::set_color_key`], falls DComp-Alpha auf einer Maschine nicht greift.
//! * **Device-Lost**: nach fehlgeschlagenem Present wird der Device-Zustand
//!   geprüft; `is_lost()` liefert dem Media-Thread das Signal, auf den CPU-
//!   Pfad ([`crate::presenter::VideoPresenter`]) zurückzufallen.

pub mod sys;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use windows::core::Interface as _;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::MSG;

use crate::nv12::NV12Frame;
use sys::{D3d11, DrawSource, Nv12Texture, RgbaTexture, Shaders, SysError, SysResult};

/// Gewählter Skalierungsmodus (C window_type; default Fit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SinkZoom {
    #[default]
    Fit,
    Zoom,
    Stretch,
}

impl From<SinkZoom> for sys::ZoomMode {
    fn from(z: SinkZoom) -> Self {
        match z {
            SinkZoom::Fit => sys::ZoomMode::Fit,
            SinkZoom::Zoom => sys::ZoomMode::Zoom,
            SinkZoom::Stretch => sys::ZoomMode::Stretch,
        }
    }
}

/// Eine Frame-Einheit in der Mailbox (siehe Modul-Doku, Quellen 1–3).
enum FrameInput {
    /// CPU-NV12 (VSR-FrameBuf-Fallback oder Decoder-Transfer-Pfad).
    Cpu(NV12Frame),
    /// FFmpeg-D3D11VA-Frame: rohe NV12-Array-Textur + Array-Index.
    D3d11 {
        texture: *mut std::os::raw::c_void,
        array_index: u32,
    },
    /// Interop-Schreibvorgang abgeschlossen — RGBA-Textur zeichnen.
    RgbaReady,
}

/// Thread-sichere Zähler des Sinks (für Spike/HUD).
#[derive(Default)]
pub struct SinkStats {
    pub frames_presented: AtomicU64,
    pub frames_dropped: AtomicU64,
    pub cpu_uploads: AtomicU64,
    pub d3d11_copies: AtomicU64,
    pub rgba_presents: AtomicU64,
    pub resizes: AtomicU64,
    pub last_draw_us: AtomicU64,
    /// Durch Burst-Collapse entfernte Frame-Nachrichten (zwei Frames kamen
    /// zusammen an; der neuere hätte den älteren sofort verdrängt).
    pub burst_collapsed: AtomicU64,
    /// Presents, die < 4 ms nach dem vorherigen erfolgten (Kadenz-Verletzung
    /// gegenüber 60 fps — Ursache für sichtbares Mikro-Ruckeln).
    pub present_too_fast: AtomicU64,
    /// EMA des Present-Abstands in µs (Render-Thread schreibt).
    pub present_dt_ema_us: AtomicU32,
}

/// Werte-Snapshot der Sink-Statistik.
#[derive(Debug, Clone, Copy, Default)]
pub struct SinkStatsValues {
    pub frames_presented: u64,
    pub frames_dropped: u64,
    pub cpu_uploads: u64,
    pub d3d11_copies: u64,
    pub rgba_presents: u64,
    pub resizes: u64,
    pub last_draw_us: u64,
    pub burst_collapsed: u64,
    pub present_too_fast: u64,
    pub present_dt_ema_us: u32,
}

/// Geteilter Zustand zwischen Owner/Media-Thread und Render-Thread.
struct SinkShared {
    /// HWND des Sink-Fensters — wird vom Render-Thread gesetzt (das Fenster
    /// MUSS auf dem Render-Thread erzeugt werden: posted messages landen in
    /// der Queue des erzeugenden Threads).
    hwnd: std::sync::Mutex<isize>,
    overlay_title: String,
    zoom: Mutex<SinkZoom>,
    /// „Benutzerdefinierter Zoom“ (settings/zoom_factor; 0 = aus) — wirkt
    /// nur im Zoom-Modus (Fit-Skala × Faktor, siehe `sys::draw_and_present`).
    zoom_factor: Mutex<f32>,
    slot: Mutex<Option<FrameInput>>,
    /// Serialisiert CUDA-Interop-Mapping (Media-Thread) gegen das Zeichnen
    /// (Render-Thread) auf der BGRA-Textur.
    interop_lock: Mutex<()>,
    /// settings/vsync: Present mit SyncInterval 1 (Display-Takt) statt 0.
    vsync: AtomicBool,
    stats: SinkStats,
    lost: AtomicBool,
    stopping: AtomicBool,
    // Roh-Pointer: vom Render-Thread NACH dem Device-Setup gesetzt, danach
    // nur lesend (die COM-Objekte leben bis zum Thread-Ende).
    device_ptr: Mutex<usize>,
    context_ptr: Mutex<usize>,
    bgra_tex_ptr: Mutex<usize>,
    // COM-Referenzen (+1) auf dieselben Objekte — halten Device/Context/
    // RGBA-Textur am Leben, solange ein Handle sie roh referenziert, AUCH
    // wenn der Render-Thread schon beendet ist. Ohne diese Referenzen könnte
    // der Media-Thread (D3D11VA-Decode/CUDA-Interop) beim Teardown-Race auf
    // freigegebene Objekte zugreifen (stiller Absturz beim Trennen).
    device_ref: Mutex<Option<windows::Win32::Graphics::Direct3D11::ID3D11Device>>,
    context_ref: Mutex<Option<windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext>>,
    bgra_ref: Mutex<Option<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D>>,
}

// SAFETY: HWND/Pointer sind Werte; die Objekte dahinter leben, solange der
// Render-Thread läuft (Join via GpuSink::drop).
unsafe impl Send for SinkShared {}
unsafe impl Sync for SinkShared {}

/// Klonbares Handle für den Media-Thread (Frames einspeisen, Status lesen).
#[derive(Clone)]
pub struct GpuSinkHandle {
    shared: Arc<SinkShared>,
}

impl GpuSinkHandle {
    /// Neuesten CPU-NV12-Frame ablegen (überschreibt einen älteren).
    pub fn submit_cpu(&self, frame: NV12Frame) {
        if self.shared.stopping.load(Ordering::Relaxed) {
            return;
        }
        {
            let mut slot = self.shared.slot.lock().unwrap();
            if slot.replace(FrameInput::Cpu(frame)).is_some() {
                self.shared.stats.frames_dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.wake();
    }

    /// FFmpeg-D3D11VA-Frame ankündigen (Kopie passiert im Render-Thread).
    ///
    /// # Safety
    /// `texture` muss eine NV12-ARRAY-Textur des Sink-Devices sein (Decoder
    /// teilt den Device — Modul-Doku) und bis zur nächsten Decoder-Nutzung
    /// gültig bleiben (DecodedFrame-Vertrag).
    pub unsafe fn submit_d3d11(&self, texture: *mut std::os::raw::c_void, array_index: u32) {
        if self.shared.stopping.load(Ordering::Relaxed) {
            return;
        }
        {
            let mut slot = self.shared.slot.lock().unwrap();
            if slot.replace(FrameInput::D3d11 { texture, array_index }).is_some() {
                self.shared.stats.frames_dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.wake();
    }

    /// Nach einem Interop-Schreibvorgang in die RGBA-Textur aufrufen.
    pub fn submit_rgba_ready(&self) {
        if self.shared.stopping.load(Ordering::Relaxed) {
            return;
        }
        {
            let mut slot = self.shared.slot.lock().unwrap();
            *slot = Some(FrameInput::RgbaReady);
        }
        self.wake();
    }

    /// Lock für den CUDA-Interop-Zugriff auf die BGRA-Textur.
    pub fn interop_lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.shared.interop_lock.lock().unwrap()
    }

    /// ID3D11Texture2D* der RGBA-Interop-Textur (für SDK-Registrierung).
    pub fn rgba_texture(&self) -> *mut std::os::raw::c_void {
        *self.shared.bgra_tex_ptr.lock().unwrap() as *mut std::os::raw::c_void
    }

    /// ID3D11Device* des Sinks (Decoder teilt es für D3D11VA).
    pub fn d3d11_device(&self) -> *mut std::os::raw::c_void {
        *self.shared.device_ptr.lock().unwrap() as *mut std::os::raw::c_void
    }

    /// ID3D11DeviceContext* des Sinks (FFmpeg-D3D11VA braucht ihn).
    pub fn d3d11_context(&self) -> *mut std::os::raw::c_void {
        *self.shared.context_ptr.lock().unwrap() as *mut std::os::raw::c_void
    }

    /// ID3D11Device* mit **+1 Referenz** — für Konsumenten, die den Pointer
    /// selbst Releasen (FFmpeg `d3d11va_device_uninit` ruft Release auf Device
    /// UND Context, ohne sich vorher AddRef'ing — ohne die eigene Referenz
    /// stirbt das Device unter dem Render-Thread, beobachtet als Segfault im
    /// Sink-Teardown).
    pub fn d3d11_device_addref(&self) -> *mut std::os::raw::c_void {
        let raw = *self.shared.device_ptr.lock().unwrap();
        if raw == 0 {
            return std::ptr::null_mut();
        }
        unsafe {
            let adopted = windows::Win32::Graphics::Direct3D11::ID3D11Device::from_raw(
                raw as *mut std::os::raw::c_void,
            );
            let extra = adopted.clone(); // AddRef
            std::mem::forget(adopted); // Sink-Basisreferenz nicht releasen
            extra.into_raw() // detach (kein Release; Konsument releaset)
        }
    }

    /// ID3D11DeviceContext* mit **+1 Referenz** (siehe `d3d11_device_addref`).
    pub fn d3d11_context_addref(&self) -> *mut std::os::raw::c_void {
        let raw = *self.shared.context_ptr.lock().unwrap();
        if raw == 0 {
            return std::ptr::null_mut();
        }
        unsafe {
            let adopted = windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext::from_raw(
                raw as *mut std::os::raw::c_void,
            );
            let extra = adopted.clone();
            std::mem::forget(adopted);
            extra.into_raw()
        }
    }

    pub fn is_lost(&self) -> bool {
        self.shared.lost.load(Ordering::Relaxed)
    }

    /// settings/vsync zur Laufzeit umschalten (wirkt ab dem nächsten Present).
    pub fn set_vsync(&self, vsync: bool) {
        self.shared.vsync.store(vsync, Ordering::Relaxed);
    }

    /// Statistik-Snapshot über das Handle (der Owner-Sink darf schon gedroppt
    /// sein — die Zähler leben im Arc weiter).
    pub fn stats_values(&self) -> SinkStatsValues {
        let s = &self.shared.stats;
        SinkStatsValues {
            frames_presented: s.frames_presented.load(Ordering::Relaxed),
            frames_dropped: s.frames_dropped.load(Ordering::Relaxed),
            cpu_uploads: s.cpu_uploads.load(Ordering::Relaxed),
            d3d11_copies: s.d3d11_copies.load(Ordering::Relaxed),
            rgba_presents: s.rgba_presents.load(Ordering::Relaxed),
            resizes: s.resizes.load(Ordering::Relaxed),
            last_draw_us: s.last_draw_us.load(Ordering::Relaxed),
            burst_collapsed: s.burst_collapsed.load(Ordering::Relaxed),
            present_too_fast: s.present_too_fast.load(Ordering::Relaxed),
            present_dt_ema_us: s.present_dt_ema_us.load(Ordering::Relaxed),
        }
    }

    pub fn set_zoom(&self, zoom: SinkZoom) {
        *self.shared.zoom.lock().unwrap() = zoom;
    }

    /// „Benutzerdefinierter Zoom“ (settings/zoom_factor; 0 = aus). Wirkt nur
    /// im Zoom-Modus: Fit-Skala × Faktor statt füllender Skala.
    pub fn set_zoom_factor(&self, factor: f32) {
        *self.shared.zoom_factor.lock().unwrap() = factor.max(0.0);
    }

    fn wake(&self) {
        let hwnd = *self.shared.hwnd.lock().unwrap();
        if hwnd == 0 {
            return; // Fenster noch nicht erzeugt.
        }
        unsafe {
            let ok = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                HWND(hwnd as isize as *mut _),
                sys::WM_APP_FRAME,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            );
            if let Err(err) = ok {
                tracing::warn!("GPU-Sink: PostMessageW fehlgeschlagen: {err}");
            }
        }
    }
}

/// Besitzender GPU-Video-Sink (Fenster + Render-Thread). `Drop` stoppt und
/// joint den Thread.
pub struct GpuSink {
    shared: Arc<SinkShared>,
    thread: Option<JoinHandle<()>>,
}

impl GpuSink {
    /// Startet Fenster + Render-Thread.
    ///
    /// `overlay_title` = exakter Titel des gpui-Fensters, dem gefolgt wird
    /// (Position/Größe/Z-Ordnung). `video_size` = Auflösung der Video-Quelle
    /// (NV12/BGRA-Input-Texturen; wird beim Auflösungswechsel neu gebaut).
    /// `vsync` = settings/vsync: Present am Display-Takt (SyncInterval 1)
    /// statt ohne Sync.
    pub fn new(overlay_title: &str, video_size: (u32, u32), vsync: bool) -> SysResult<GpuSink> {
        let (width, height) = (video_size.0.max(2), video_size.1.max(2));
        let shared = Arc::new(SinkShared {
            hwnd: Mutex::new(0),
            overlay_title: overlay_title.to_string(),
            zoom: Mutex::new(SinkZoom::Fit),
            zoom_factor: Mutex::new(0.0),
            slot: Mutex::new(None),
            interop_lock: Mutex::new(()),
            vsync: AtomicBool::new(vsync),
            stats: SinkStats::default(),
            lost: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            device_ptr: Mutex::new(0),
            context_ptr: Mutex::new(0),
            bgra_tex_ptr: Mutex::new(0),
            device_ref: Mutex::new(None),
            context_ref: Mutex::new(None),
            bgra_ref: Mutex::new(None),
        });

        let shared_for_thread = Arc::clone(&shared);
        let title_for_thread = overlay_title.to_string();
        let thread = std::thread::Builder::new()
            .name("gpu-sink-render".into())
            .spawn(move || render_thread_entry(&title_for_thread, width, height, shared_for_thread))
            .map_err(|e| SysError::Message(format!("render thread spawn: {e}")))?;

        // Auf das Device-Setup warten (Render-Thread setzt die Pointer).
        for _ in 0..500 {
            if *shared.device_ptr.lock().unwrap() != 0 {
                tracing::info!(
                    "GPU-Sink: bereit (video {}x{}, overlay {:?})",
                    width,
                    height,
                    overlay_title
                );
                return Ok(GpuSink { shared, thread: Some(thread) });
            }
            if shared.lost.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Err(SysError::Message("GPU-Sink: Device-Setup-Timeout/-Fehler".into()))
    }

    /// Klonbares Handle für den Media-Thread.
    pub fn handle(&self) -> GpuSinkHandle {
        GpuSinkHandle { shared: Arc::clone(&self.shared) }
    }

    /// Statistik-Snapshot (Werte für Log/Spike).
    pub fn stats_values(&self) -> SinkStatsValues {
        let s = &self.shared.stats;
        SinkStatsValues {
            frames_presented: s.frames_presented.load(Ordering::Relaxed),
            frames_dropped: s.frames_dropped.load(Ordering::Relaxed),
            cpu_uploads: s.cpu_uploads.load(Ordering::Relaxed),
            d3d11_copies: s.d3d11_copies.load(Ordering::Relaxed),
            rgba_presents: s.rgba_presents.load(Ordering::Relaxed),
            resizes: s.resizes.load(Ordering::Relaxed),
            last_draw_us: s.last_draw_us.load(Ordering::Relaxed),
            burst_collapsed: s.burst_collapsed.load(Ordering::Relaxed),
            present_too_fast: s.present_too_fast.load(Ordering::Relaxed),
            present_dt_ema_us: s.present_dt_ema_us.load(Ordering::Relaxed),
        }
    }

    pub fn is_lost(&self) -> bool {
        self.shared.lost.load(Ordering::Relaxed)
    }
}

impl Drop for GpuSink {
    fn drop(&mut self) {
        self.shared.stopping.store(true, Ordering::Relaxed);
        let hwnd = *self.shared.hwnd.lock().unwrap();
        // Stop-Nachricht nur an ein lebendes Fenster posten (IsWindow-Guard,
        // siehe sys::valid) — nach Device-Lost kann der Render-Thread schon
        // weg sein und das HWND mit ihm.
        if hwnd != 0 && sys::valid(HWND(hwnd as isize as *mut _)) {
            unsafe { sys::post_stop(HWND(hwnd as isize as *mut _)) };
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Render-Thread
// ---------------------------------------------------------------------------

/// Besitzende Objekte des Render-Threads.
struct RenderState {
    d3d: D3d11,
    shaders: Shaders,
    nv12: Nv12Texture,
    rgba: RgbaTexture,
    /// Welche Shader-Input-Textur zuletzt aktualisiert wurde.
    active_source: ActiveSource,
}

#[derive(PartialEq, Clone, Copy)]
enum ActiveSource {
    Nv12,
    Rgba,
}

/// Render-Thread-Einstieg: erzeugt das FENSTER AUF DIESEM THREAD (posted
/// messages landen in der Queue des erzeugenden Threads!), dann Device etc.
fn render_thread_entry(overlay_title: &str, width: u32, height: u32, shared: Arc<SinkShared>) {
    match sys::create_window(width, height) {
        Ok(hwnd) => {
            *shared.hwnd.lock().unwrap() = hwnd.0 as isize;
            render_thread(hwnd, overlay_title, width, height, shared);
        }
        Err(err) => {
            tracing::error!("GPU-Sink: Fenster-Erzeugung fehlgeschlagen: {err}");
            shared.lost.store(true, Ordering::Relaxed);
        }
    }
}

fn render_thread(
    hwnd: HWND,
    _overlay_title: &str, // via shared.overlay_title (Follow-Tick)
    video_w: u32,
    video_h: u32,
    shared: Arc<SinkShared>,
) {
    // D3D11-Setup; die Input-Texturen bekommen die angeforderte Video-Größe
    // (VSR: Output-Dims, sonst Stream-Dims). Vor dem Fix stand hier ein
    // 1280x720-Hardcode — die BGRA-Interop-Textur war dann kleiner als der
    // VSR-Output, der SDK-Transfer beschnitt ihn oben-links (2x/3x-Zoom-Bug).
    let mut state = match build_state(hwnd, video_w, video_h) {
        Ok(state) => {
            *shared.device_ptr.lock().unwrap() = sys::device_raw(&state.d3d) as usize;
            *shared.context_ptr.lock().unwrap() = sys::context_raw(&state.d3d) as usize;
            *shared.bgra_tex_ptr.lock().unwrap() = state.rgba.texture.as_raw() as usize;
            // COM-Referenzen (+1) im Shared-State: Handles (Media-Thread)
            // halten Device/Context/RGBA-Textur so über das Render-Thread-
            // Ende hinaus am Leben (Teardown-Race, siehe SinkShared).
            *shared.device_ref.lock().unwrap() = Some(state.d3d.device.clone());
            *shared.context_ref.lock().unwrap() = Some(state.d3d.context.clone());
            *shared.bgra_ref.lock().unwrap() = Some(state.rgba.texture.clone());
            Some(state)
        }
        Err(err) => {
            tracing::error!("GPU-Sink: Setup fehlgeschlagen: {err} — Fallback auf CPU-Pfad");
            shared.lost.store(true, Ordering::Relaxed);
            return;
        }
    };
    let state = state.as_mut().expect("state just built");

    unsafe { sys::set_timer(hwnd, 1, 50) };

    // Erst-Present: definiertes schwarzes Bild (NV12-Textur ist nullgesetzt →
    // Shader-clampet auf Schwarz), bevor der erste echte Frame eintrifft —
    // sonst könnte ein uninitialisierter Backbuffer sichtbar werden.
    {
        let zoom = *shared.zoom.lock().unwrap();
        let zoom_factor = *shared.zoom_factor.lock().unwrap();
        let _ = sys::draw_and_present(
            &state.d3d,
            &state.shaders,
            DrawSource::Nv12(&state.nv12.srv_y, &state.nv12.srv_uv),
            texture_size_of(&state.nv12.texture).unwrap_or((1280, 720)),
            zoom.into(),
            zoom_factor,
            0,
        );
    }

    let mut msg = MSG::default();
    let mut follow_visible = false;
    // Present-Kadenz-Buchhaltung (Render-Thread-lokal).
    let mut last_present: Option<Instant> = None;
    let mut present_dt_ema_us: u64 = 0;
    'loop_: loop {
        // Blockierend auf Nachrichten; WM_APP_FRAME/WM_TIMER/WM_APP_STOP.
        if !sys::get_message(&mut msg) {
            break; // WM_QUIT
        }
        sys::translate_and_dispatch(&msg);
        // Stop-Flag VOR dem Follow-Tick prüfen: der Tick (Overlay-Geometrie/
        // Z-Ordnung, sys::set_pos_below/hide_window/…) endet damit BEVOR der
        // Teardown das Overlay-Fenster (gpui) oder das Sink-Fenster anrührt;
        // die sys-Helfer prüfen zusätzlich selbst per IsWindow (sys::valid).
        if shared.stopping.load(Ordering::Relaxed) {
            break 'loop_;
        }
        let is_frame = msg.message == sys::WM_APP_FRAME;
        let is_timer = msg.message == windows::Win32::UI::WindowsAndMessaging::WM_TIMER;
        if !is_frame && !is_timer {
            continue;
        }

        // Kein Burst-Drain: Der Media-Thread meldet JEDEN angezeigten Frame
        // (VSR-Interop schreibt pro Frame in die RGBA-Textur, siehe
        // sessions.rs) — jede Nachricht ist ein eigenes Bild. Kollidiert das
        // Zeichnen mit dem Schreibvorgang des nächsten Frames, serialisiert
        // der interop_lock (unterhalb, RGBA-Quelle).

        // --- Follow-Tick: Overlay-Fenster verfolgen (Geometrie + Z-Ordnung).
        let mut follow_resized = false;
        {
            let d3d = &mut state.d3d;
            match follow_tick(hwnd, &shared, d3d, &mut follow_visible) {
                Ok(resized) => {
                    follow_resized = resized;
                    if resized {
                        shared.stats.resizes.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(err) => {
                    tracing::error!("GPU-Sink: Resize/Geometrie-Fehler: {err}");
                    if let Some(reason) = sys::device_removed_reason(&state.d3d.device) {
                        tracing::error!(
                            "GPU-Sink: Device removed ({reason}) — Fallback auf CPU-Pfad"
                        );
                        shared.lost.store(true, Ordering::Relaxed);
                        break 'loop_;
                    }
                }
            }
        }

        // --- Frame verarbeiten (falls einer da ist).
        let mut dirty = false;
        let input = shared.slot.lock().unwrap().take();
        match input {
            Some(FrameInput::Cpu(frame)) => {
                let result = handle_cpu_frame(&shared, state, frame);
                dirty |= result;
            }
            Some(FrameInput::D3d11 { texture, array_index }) => {
                dirty |= handle_d3d11_frame(&shared, state, texture, array_index);
            }
            Some(FrameInput::RgbaReady) => {
                state.active_source = ActiveSource::Rgba;
                shared.stats.rgba_presents.fetch_add(1, Ordering::Relaxed);
                dirty = true;
            }
            None => {}
        }

        if follow_resized {
            // Nach Resize: Backbuffer ist leer → einmal sofort zeichnen, damit
            // kein schwarzes Flackern zwischen Resize und nächstem Frame bleibt.
            let zoom = *shared.zoom.lock().unwrap();
            let zoom_factor = *shared.zoom_factor.lock().unwrap();
            let source = match state.active_source {
                ActiveSource::Nv12 => DrawSource::Nv12(&state.nv12.srv_y, &state.nv12.srv_uv),
                ActiveSource::Rgba => DrawSource::Rgba(&state.rgba.srv),
            };
            let desc = active_texture_size(state).unwrap_or((1280, 720));
            let _ = sys::draw_and_present(
                &state.d3d,
                &state.shaders,
                source,
                desc,
                zoom.into(),
                zoom_factor,
                shared.vsync.load(Ordering::Relaxed) as u32,
            );
        }

        if dirty {
            let started = Instant::now();
            let zoom = *shared.zoom.lock().unwrap();
            let zoom_factor = *shared.zoom_factor.lock().unwrap();
            let source = match state.active_source {
                ActiveSource::Nv12 => DrawSource::Nv12(&state.nv12.srv_y, &state.nv12.srv_uv),
                ActiveSource::Rgba => DrawSource::Rgba(&state.rgba.srv),
            };
            // Video-Auflösung für den Letterbox-Viewport aus der Input-Textur
            // der AKTUELLEN Quelle (RGBA-Pfad: die BGRA-Interop-Textur — nicht
            // die NV12-Textur, die im Interop-Modus nie Frames sieht).
            let desc = active_texture_size(state).unwrap_or((1280, 720));
            let sync_interval = shared.vsync.load(Ordering::Relaxed) as u32;
            // RGBA-Quelle: Zeichnen+Present unter interop_lock — der Media-
            // Thread schreibt JEDEN Frame per CUDA-Interop in DIESE Textur;
            // ohne Lock würde der nächste Schreibvorgang mitten im Draw
            // laufen (Texture-Tear).
            let _interop_guard = match state.active_source {
                ActiveSource::Rgba => Some(shared.interop_lock.lock().unwrap()),
                ActiveSource::Nv12 => None,
            };
            match sys::draw_and_present(&state.d3d, &state.shaders, source, desc, zoom.into(), zoom_factor, sync_interval)
            {
                Ok(()) => {
                    shared.stats.frames_presented.fetch_add(1, Ordering::Relaxed);
                    let us = started.elapsed().as_micros() as u64;
                    shared.stats.last_draw_us.store(us, Ordering::Relaxed);
                    // Kadenz: Present-Abstand messen (Pace-Jitter ist die
                    // sichtbare Größe bei Mikro-Ruckeln — Logs im Debug-Overlay).
                    let now = Instant::now();
                    if let Some(prev) = last_present {
                        let dt_us = now.duration_since(prev).as_micros() as u64;
                        present_dt_ema_us = if present_dt_ema_us == 0 {
                            dt_us
                        } else {
                            present_dt_ema_us * 9 / 10 + dt_us / 10
                        };
                        shared
                            .stats
                            .present_dt_ema_us
                            .store(present_dt_ema_us.min(u32::MAX as u64) as u32, Ordering::Relaxed);
                        if dt_us < 4000 {
                            shared.stats.present_too_fast.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    last_present = Some(now);
                }
                Err(err) => {
                    tracing::error!("GPU-Sink: Draw/Present fehlgeschlagen: {err}");
                    if let Some(reason) = sys::device_removed_reason(&state.d3d.device) {
                        tracing::error!(
                            "GPU-Sink: Device removed ({reason}) — Fallback auf CPU-Pfad"
                        );
                        shared.lost.store(true, Ordering::Relaxed);
                        break 'loop_;
                    }
                }
            }
        }
    }

    tracing::info!("GPU-Sink: Render-Thread beendet");
}

/// CPU-NV12: Textur-Größe prüfen (Session-Auflösungswechsel → neu bauen),
/// dann UpdateSubresource beider Planes.
fn handle_cpu_frame(shared: &Arc<SinkShared>, state: &mut RenderState, frame: NV12Frame) -> bool {
    let (w, h) = (frame.width, frame.height);
    let (y, uv) = (frame.y_plane(), frame.uv_plane());
    let (ys, uvs) = (frame.y_stride, frame.uv_stride);
    let started = Instant::now();
    let rebuild = texture_size_of(&state.nv12.texture) != Some((w, h));
    if rebuild {
        match recreate_nv12(state, w, h) {
            Ok(()) => {}
            Err(err) => {
                tracing::error!("GPU-Sink: NV12-Textur-Rebuild fehlgeschlagen: {err}");
                return false;
            }
        }
    }
    let result = unsafe {
        sys::update_nv12(&state.d3d.context, &state.nv12.texture, y, ys, uv, uvs, w, h)
    };
    match result {
        Ok(()) => {
            state.active_source = ActiveSource::Nv12;
            shared.stats.cpu_uploads.fetch_add(1, Ordering::Relaxed);
            let us = started.elapsed().as_micros() as u64;
            shared.stats.last_draw_us.store(us, Ordering::Relaxed);
            true
        }
        Err(err) => {
            tracing::error!("GPU-Sink: NV12-Upload fehlgeschlagen: {err}");
            false
        }
    }
}

/// D3D11VA-Frame: GPU-GPU-Kopie der beiden Planes in die eigene NV12-Textur.
/// Safety-Vertrag siehe `FrameInput::D3d11` / Modul-Doku (Quelle 2).
fn handle_d3d11_frame(
    shared: &Arc<SinkShared>,
    state: &mut RenderState,
    texture: *mut std::os::raw::c_void,
    array_index: u32,
) -> bool {
    use windows::core::Interface;
    // Textur-Größe an die Quelltextur angleichen (Fall: Session-Wechsel).
    let src: windows::Win32::Graphics::Direct3D11::ID3D11Texture2D =
        unsafe { Interface::from_raw(texture.cast()) };
    let mut desc = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
    unsafe { src.GetDesc(&mut desc) };
    if texture_size_of(&state.nv12.texture) != Some((desc.Width, desc.Height)) {
        if let Err(err) = recreate_nv12(state, desc.Width, desc.Height) {
            tracing::error!("GPU-Sink: NV12-Textur-Rebuild fehlgeschlagen: {err}");
            std::mem::forget(src); // Leihe — kein Release.
            return false;
        }
    }
    unsafe { sys::copy_d3d11_nv12(&state.d3d.context, &src, array_index, &state.nv12.texture) };
    // Wir LEIHEN die Textur nur (Besitz bleibt beim Decoder) → Wrapper ohne
    // Release fallen lassen.
    std::mem::forget(src);
    state.active_source = ActiveSource::Nv12;
    shared.stats.d3d11_copies.fetch_add(1, Ordering::Relaxed);
    true
}

fn texture_size_of(tex: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D) -> Option<(u32, u32)> {
    let mut desc = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
    unsafe { tex.GetDesc(&mut desc) };
    Some((desc.Width, desc.Height))
}

/// Größe der Shader-Input-Textur der AKTUELLEN Quelle — Grundlage für die
/// Letterbox-Mathe (`draw_and_present`). Im RGBA-Interop-Modus zählt die
/// BGRA-Textur, nicht die NV12-Textur.
fn active_texture_size(state: &RenderState) -> Option<(u32, u32)> {
    match state.active_source {
        ActiveSource::Nv12 => texture_size_of(&state.nv12.texture),
        ActiveSource::Rgba => texture_size_of(&state.rgba.texture),
    }
}

fn recreate_nv12(state: &mut RenderState, w: u32, h: u32) -> SysResult<()> {
    let nv12 = sys::create_nv12_texture(&state.d3d.device, w, h)?;
    state.nv12 = nv12;
    Ok(())
}

/// Folgt dem Overlay-Fenster: Geometrie + Z-Ordnung. Liefert Ok(true), wenn
/// die Swapchain wegen Größenänderung neu dimensioniert wurde.
fn follow_tick(
    hwnd: HWND,
    shared: &SinkShared,
    d3d: &mut D3d11,
    follow_visible: &mut bool,
) -> SysResult<bool> {
    let Some(overlay) = sys::find_window_by_title(&shared.overlay_title) else {
        if *follow_visible {
            unsafe { sys::hide_window(hwnd) };
            *follow_visible = false;
        }
        return Ok(false);
    };
    let Some((x, y, w, h)) = sys::client_rect_screen(overlay) else {
        return Ok(false);
    };
    if w == 0 || h == 0 {
        return Ok(false);
    }
    unsafe { sys::show_window_no_activate(hwnd) };
    *follow_visible = true;
    unsafe { sys::set_pos_below(hwnd, overlay, x, y, w, h) };

    let mut resized = false;
    if (w, h) != (d3d.width, d3d.height) {
        sys::resize_swap_chain(d3d, w, h)?;
        resized = true;
    }
    Ok(resized)
}

fn build_state(hwnd: HWND, video_w: u32, video_h: u32) -> SysResult<RenderState> {
    let d3d = sys::create_device_and_swapchain(hwnd, video_w, video_h)
        .map_err(|e| SysError::Message(format!("create_device_and_swapchain: {e}")))?;
    let shaders = sys::create_shaders(&d3d.device)
        .map_err(|e| SysError::Message(format!("create_shaders: {e}")))?;
    let nv12 = sys::create_nv12_texture(&d3d.device, video_w, video_h)
        .map_err(|e| SysError::Message(format!("create_nv12_texture: {e}")))?;
    let rgba = sys::create_rgba_texture(&d3d.device, video_w, video_h)
        .map_err(|e| SysError::Message(format!("create_rgba_texture: {e}")))?;
    Ok(RenderState {
        d3d,
        shaders,
        nv12,
        rgba,
        active_source: ActiveSource::Nv12,
    })
}
