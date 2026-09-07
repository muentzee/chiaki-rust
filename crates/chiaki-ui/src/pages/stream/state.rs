//! StreamUiState — das gpui-Global der Stream-Ansicht (ui-v2-spec §2.4).
//!
//! Die Seite (`pages::stream::page`) erzeugt dieses Global beim ersten Frame
//! für `Route::Stream(host)` und entfernt es beim Wegnavigieren
//! ([`shutdown`], aus `AppShell::navigate`). Als Global ist der Zustand auch
//! aus `&mut App`-Closures erreichbar (Dialog-Buttons!), ohne
//! `AppShell`-Felder zu erweitern — dasselbe Muster wie `SettingsUiState`.
//!
//! Verantwortlich: Connecting-Flow (Aufwecken → Anmelden → Kalibrieren →
//! Streamen), Controller-/Tastatur-Loop (nur bei Änderung, ~120-Hz-Deckel —
//! wie `SendFeedbackState` im C++), HUD-Statistik (aus
//! [`StreamTelemetry`](crate::backend::sessions::StreamTelemetry) +
//! Presenter-Stats) und die Stream-Overlays (PIN, Konsole-Tastatur).
//!
//! Abweichung zum C++-Flow (dokumentiert): Der C++-Client läuft Senkusha VOR
//! der Session; im Rust-Port (chiaki-core/session.rs) läuft die Senkusha
//! **innerhalb** des session_thread (nach Ctrl/Login-PIN, vor der
//! StreamConnection). Eine eigene Senkusha-Vorphase würde sie doppelt
//! ausführen — die Station „Verbindung kalibrieren" wird deshalb aus den
//! vorhandenen Session-Kanten abgeleitet: Session-Start + 2,5 s (Ctrl-Phase,
//! sofern keine PIN ansteht) → Kalibrieren; `SessionEvent::Connected` →
//! Streamen.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{App, Context, FocusHandle, Window};

use crate::app::AppShell;
use crate::backend::sessions::{
    keyboard_mapper_from_settings, ConnectRequest, FeedbackCmd, LinkQuality, StreamTelemetry,
};
use crate::backend::{Backend, HostId};
use crate::components::ToastData;
use chiaki_core::controller::ControllerState;
use chiaki_core::discovery::DiscoveryHostState;
use chiaki_input::gamepad::apply_deadzone;
use chiaki_input::{combine_states, Key as InKey, KeyboardMapper};
use chiaki_render::gpu_sink::{GpuSink, GpuSinkConfig, GpuSinkHandle, SinkZoom};
use crate::backend::sessions::GPUI_WINDOW_TITLE;
use chiaki_render::presenter::VideoPresenter;
use chiaki_settings::hosts::HostMac;

use super::fake;

// ---------------------------------------------------------------------------
// Einstiegspunkte für page() / app.rs
// ---------------------------------------------------------------------------

/// Pro Frame (von `page()`): Global anlegen/aktualisieren und ticken.
/// Liefert `true`, wenn das Global in diesem Frame neu erstellt wurde.
pub(crate) fn ensure_and_tick(
    shell: &mut AppShell,
    host: HostId,
    window: &mut Window,
    cx: &mut Context<AppShell>,
) -> bool {
    let mut created = false;
    if !cx.has_global::<StreamUiState>() {
        let state = StreamUiState::new(cx.entity().downgrade(), shell.backend.clone(), host, cx);
        cx.set_global(state);
        created = true;
    } else if cx.global::<StreamUiState>().host_id != host {
        // Host-Wechsel: alte Streams sauber stoppen, neu aufsetzen.
        let mut old = cx.remove_global::<StreamUiState>();
        old.stop_threads();
        shell.backend.sessions().stop_current();
        let state = StreamUiState::new(cx.entity().downgrade(), shell.backend.clone(), host, cx);
        cx.set_global(state);
        created = true;
    }

    cx.global_mut::<StreamUiState>().tick(window);

    // WLAN-/Netzwerk-Drops-Warnung (einmalig pro Session; aus update_stats
    // gesetzt — dort gibt es kein cx) als Toast ausliefern.
    let wifi_warn = cx.global_mut::<StreamUiState>().take_wifi_warn();
    if wifi_warn {
        let shell = cx.global::<StreamUiState>().shell.clone();
        cx.spawn(async move |_shell_weak, cx| {
            let _ = shell.update(cx, |shell, cx| {
                shell.push_toast(
                    ToastData::new(crate::components::ToastKind::Warn, "WLAN/Netzwerk-Drops")
                        .message(
                            "Hoher Frame-Verlust im Netzwerk — der Stream kann ruckeln. \
                             (Schwellwert: Einstellungen → Audio & Latency)",
                        ),
                    cx,
                );
            });
        })
        .detach();
    }

    if created {
        let focus = cx.global::<StreamUiState>().focus.clone();
        window.defer(cx, move |window, _cx| focus.focus(window));
    }
    created
}

/// Session-Events aus `AppShell::apply_events` in die Stream-Ansicht leiten.
/// Liefert `false`, wenn kein Stream-Global existiert (Fallback in app.rs).
pub(crate) fn forward_session_event(
    session_id: u64,
    event: chiaki_core::session::SessionEvent,
    cx: &mut Context<AppShell>,
) -> bool {
    if !cx.has_global::<StreamUiState>() {
        return false;
    }
    // Global kurz herausnehmen (Ownership), Event anwenden, zurücklegen —
    // vermeidet die Doppel-Borrow von cx (global_mut + Methode mit cx).
    let mut state = cx.remove_global::<StreamUiState>();
    state.on_session_event(session_id, event, cx);
    cx.set_global(state);
    true
}

/// PSN-Connecting-Stufen aus `AppShell::apply_events` in die Stream-Ansicht
/// leiten (Port der C++ PsnConnectState-Anzeige). Liefert `false`, wenn kein
/// Stream-Global existiert.
pub(crate) fn forward_psn_connect_state(
    state: crate::backend::psn::PsnConnectState,
    cx: &mut Context<AppShell>,
) -> bool {
    if !cx.has_global::<StreamUiState>() {
        return false;
    }
    let mut global = cx.remove_global::<StreamUiState>();
    global.on_psn_connect_state(state);
    cx.set_global(global);
    true
}

/// Aufräumen beim Verlassen von `Route::Stream` (AppShell::navigate):
/// Fake-Thread stoppen, Session stoppen, Global entfernen.
pub fn shutdown(backend: &Backend, cx: &mut App) {
    backend.sessions().stop_current();
    if cx.has_global::<StreamUiState>() {
        let mut state = cx.remove_global::<StreamUiState>();
        state.stop_threads();
    }
}

/// Esc im Stream (ohne offenen Dialog): Trennen-Bestätigung statt hartem
/// Rausnavigieren (Spec §2.4: „Abbrechen immer möglich", C++-Confirm-Pfad).
pub(crate) fn maybe_open_disconnect_dialog(
    shell: &mut AppShell,
    cx: &mut Context<AppShell>,
) -> bool {
    if !matches!(shell.route, crate::app::Route::Stream(_))
        || shell.has_dialog()
        || !cx.has_global::<StreamUiState>()
    {
        return false;
    }
    if cx.global::<StreamUiState>().stage != Stage::Streaming {
        return false;
    }
    super::dialogs::push_disconnect_confirm(shell, cx);
    true
}

// ---------------------------------------------------------------------------
// Ablauf-/Anzeige-Status
// ---------------------------------------------------------------------------

/// Die 4 Status-Stationen (bindend, Spec §2.4).
pub const CONNECTING_STATIONS: [&str; 4] =
    ["Aufwecken", "Anmelden", "Verbindung kalibrieren", "Streamen"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    Wake,
    Login,
    Calibrate,
    Streaming,
}

impl Stage {
    pub fn index(self) -> usize {
        match self {
            Stage::Wake => 0,
            Stage::Login => 1,
            Stage::Calibrate => 2,
            Stage::Streaming => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageState {
    Pending,
    Active,
    Done,
    Failed,
}

/// Video-Skalierung im Fenster (C++ window_type-Zweige Zoom/Stretch).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoomMode {
    /// Schwarze Balken, komplettes Bild.
    Fit,
    /// Füllt das Fenster (Crop — „Zoom" im C++).
    Zoom,
    /// Verzerrt füllend („Stretch").
    Stretch,
}

impl ZoomMode {
    pub fn label(self) -> &'static str {
        match self {
            ZoomMode::Fit => "Original",
            ZoomMode::Zoom => "Zoom",
            ZoomMode::Stretch => "Strecken",
        }
    }

    pub fn next(self) -> Self {
        match self {
            ZoomMode::Fit => ZoomMode::Zoom,
            ZoomMode::Zoom => ZoomMode::Stretch,
            ZoomMode::Stretch => ZoomMode::Fit,
        }
    }
}

/// PIN-Overlay-Zustand (8 Boxen, Auto-Submit bei 8 Ziffern).
#[derive(Default)]
pub struct PinState {
    pub visible: bool,
    pub incorrect: bool,
    pub digits: String,
}

/// Konsolen-Tastatur-Overlay (KeyboardText-Events).
pub struct KeyboardOverlay {
    pub text: String,
    pub focus: FocusHandle,
}

/// HUD-Werte (1×/Frame aus Telemetrie + Presenter berechnet).
#[derive(Debug, Clone, Default)]
pub struct HudStats {
    pub bitrate_mbit: f32,
    pub rtt_ms: Option<f32>,
    pub loss_pct: Option<f32>,
    pub frame_time_ms: f32,
    pub fps: f32,
    pub audio_fill_ms: f32,
    pub decoder: String,
    pub haptics: String,
}

/// Snapshot der anzuzeigenden Zustände (für den Render-Pfad, damit kein
/// Global-Borrow die `cx.listener`-Aufrufe blockiert).
#[derive(Clone)]
pub struct StreamSnapshot {
    pub stage: Stage,
    pub stages: [StageState; 4],
    pub host_label: String,
    pub error: Option<String>,
    pub status_line: String,
    pub fake: bool,
    pub pin_visible: bool,
    pub pin_digits: String,
    pub pin_incorrect: bool,
    pub keyboard_open: bool,
    pub keyboard_text: String,
    pub panel_open: bool,
    pub hud_open: bool,
    pub zoom: ZoomMode,
    pub zoom_factor: f32,
    pub mic_unmuted: bool,
    /// settings/fullscreen_doubleclick: Doppelklick in die Video-Fläche
    /// toggelt Vollbild (Stream-Seite, click_count ≥ 2).
    pub doubleclick_fullscreen: bool,
    /// settings/hide_cursor: Mauszeiger über der Video-Fläche verstecken.
    pub hide_cursor: bool,
    pub stats: HudStats,
    pub vsr_active: bool,
    pub vsr_scale: u32,
    pub vsr_badge_wanted: bool,
    pub video_size: (u32, u32),
    /// Overlay-Badge-Sichtbarkeit + Debug-Zeilen-Flag (Live-Lesen der
    /// Settings im Snapshot; Filterlogik siehe `hud::badge_visible`).
    pub overlay: super::hud::OverlayConfig,
    /// Debug-Zeile unterm HUD (`settings/overlay_debug`, Zusammenbau in
    /// `update_stats`); `None` = aus.
    pub debug_line: Option<String>,
}

/// Das Stream-UI-Global (siehe Modul-Doku).
pub struct StreamUiState {
    /// Render-Rate-Cap: Zeitpunkt des letzten Seiten-Renderings.
    pub last_render: std::time::Instant,
    /// Läuft gerade ein Throttle-Timer (nur EINER, sonst Timer-Schwärme).
    pub throttle_timer_pending: bool,
    pub host_id: HostId,
    pub host_label: String,
    pub fake: bool,
    pub shell: gpui::WeakEntity<AppShell>,
    pub backend: Backend,
    pub focus: FocusHandle,
    /// Fenster-Modus aus settings/window_type (einmalig anwenden).
    pub want_fullscreen: bool,

    // Video/HUD-Quellen.
    pub presenter: Option<VideoPresenter>,
    pub telemetry: Option<Arc<StreamTelemetry>>,
    /// GPU-Videopfad-Handle (`settings/video_output`); `Some` + !is_lost →
    /// die Stream-Seite malt den Video-Bereich TRANSPARENT (das D3D11-Sink-
    /// Fenster liegt unter dem gpui-Fenster).
    pub gpu: Option<GpuSinkHandle>,
    /// Owner des Sink-Fensters (lebt bis zum Verlassen der Stream-Seite;
    /// im echten Pfad besitzt ihn der Session-Stop-Thread).
    pub gpu_sink: Option<GpuSink>,

    // Connecting-Flow.
    pub request: Option<ConnectRequest>,
    pub standby: bool,
    pub stage: Stage,
    pub stages: [StageState; 4],
    pub error: Option<String>,
    wake_sent: bool,
    connect_started: bool,
    login_active_since: Option<Instant>,
    fullscreen_done: bool,

    // Fake-Mode-Handles.
    fake_stop: Option<Arc<AtomicBool>>,
    fake_connected: Option<Arc<AtomicBool>>,
    fake_pin: Option<Arc<AtomicBool>>,

    // Overlays/Panels.
    pub pin: PinState,
    pub keyboard: Option<KeyboardOverlay>,
    pub panel_open: bool,
    pub hud_open: bool,
    pub zoom: ZoomMode,
    /// Benutzerdefinierter Zoom (settings/zoom_factor; 0 = aus). > 0: der
    /// Zoom-Modus skaliert mit **Fit-Skala × Faktor** statt füllend
    /// (VideoSurface + GPU-Sink, C++-Pendant settings/zoom_factor).
    pub zoom_factor: f32,
    pub mic_unmuted: bool,

    // Input-Loop.
    keys: HashSet<InKey>,
    last_controller: Option<ControllerState>,
    last_send: Instant,
    /// Nach Overlay-Schluss den Stream-Fokus wiederherstellen (page).
    pub(crate) refocus_pending: bool,

    // Disconnect-Action (settings/disconnect_action): auto-goto_bed nur
    // EINMAL pro Stream (das Ruhemodus-Quit erzeugt ein zweites Quit-Event).
    auto_bed_sent: bool,

    // WLAN-/Netzwerk-Drops (settings/wifi_dropped_notif_percent):
    // 5-Sekunden-Fenster über die Session-Telemetrie, Warnung EINMALIG
    // pro Session (pending_wifi_warn wird von ensure_and_tick ausgeliefert).
    wifi_warned: bool,
    wifi_window: Option<(Instant, u64, u64)>,
    pending_wifi_warn: bool,

    // Stats-Buchhaltung.
    stats: HudStats,
    last_stats_at: Instant,
    last_video_bytes: u64,
    last_presented: u64,
    bitrate_ema: f32,
    fps_ema: f32,
    /// Debug-Zeile (settings/overlay_debug; `None` = aus) — in update_stats
    /// gebaut, via snapshot in die Stream-Ansicht.
    debug_line: Option<String>,
    /// Sink-presented-Zähler des letzten Ticks + EMA (Debug-Zeile im GPU-
    /// Pfad — dort presentet der Sink, der Presenter bleibt bei 0).
    last_sink_presented: u64,
    sink_fps_ema: f32,

    started_at: Instant,
}

impl gpui::Global for StreamUiState {}

impl StreamUiState {
    fn new(
        shell: gpui::WeakEntity<AppShell>,
        backend: Backend,
        host: HostId,
        cx: &mut Context<AppShell>,
    ) -> Self {
        let fake = std::env::var("CHIAKI_UI_FAKE_STREAM")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false);

        let mut state = Self {
            host_label: host_label(&backend, &host),
            host_id: host,
            fake,
            shell,
            backend,
            focus: cx.focus_handle(),
            want_fullscreen: false,
            last_render: std::time::Instant::now(),
            throttle_timer_pending: false,
            presenter: None,
            telemetry: None,
            gpu: None,
            gpu_sink: None,
            request: None,
            standby: false,
            stage: Stage::Wake,
            stages: [
                StageState::Active,
                StageState::Pending,
                StageState::Pending,
                StageState::Pending,
            ],
            error: None,
            wake_sent: false,
            connect_started: false,
            login_active_since: None,
            fullscreen_done: false,
            fake_stop: None,
            fake_connected: None,
            fake_pin: None,
            pin: PinState::default(),
            keyboard: None,
            panel_open: false,
            hud_open: true,
            zoom: ZoomMode::Fit,
            zoom_factor: 0.0,
            mic_unmuted: false,
            keys: HashSet::new(),
            last_controller: None,
            last_send: Instant::now(),
            refocus_pending: false,
            auto_bed_sent: false,
            wifi_warned: false,
            wifi_window: None,
            pending_wifi_warn: false,
            stats: HudStats::default(),
            last_stats_at: Instant::now(),
            last_video_bytes: 0,
            last_presented: 0,
            bitrate_ema: 0.0,
            fps_ema: 0.0,
            debug_line: None,
            last_sink_presented: 0,
            sink_fps_ema: 0.0,
            started_at: Instant::now(),
        };

        // Controller-Feedback-Sink (Session-Events → DualSense/Gamepads):
        // Rumble, adaptive Trigger, Haptics-Intensität (C++:
        // controller->SetRumble/SetTriggerEffects-Handler).
        {
            let controllers = state.backend.controllers().clone();
            state.backend.sessions().set_feedback_sink(Some(Arc::new(move |cmd| match cmd {
                FeedbackCmd::Rumble { left, right } => controllers.set_rumble(left, right),
                FeedbackCmd::TriggerEffects { type_left, data_left, type_right, data_right } => {
                    controllers.set_trigger_effects(
                        type_left,
                        &data_left,
                        type_right,
                        &data_right,
                    )
                }
                FeedbackCmd::HapticIntensity(v) => controllers.set_haptic_intensity(v),
                // LED-Farbe/Player-Index: Passthrough am ControllerHandle
                // folgt bei Bedarf (DualSenseManager hat set_led/
                // set_player_lights); bis dahin bewusst ohne Funktion.
                FeedbackCmd::LedColor(_) | FeedbackCmd::PlayerIndex(_) => {}
            })));
        }

        if state.fake {
            state.start_fake();
        } else {
            state.start_real();
        }

        // Fenster-/Skalierungsmodus aus den Settings (C++ window_type).
        let settings = state.backend.settings().lock().unwrap_or_else(|e| e.into_inner());
        state.zoom = match settings.window_type() {
            chiaki_settings::settings::WindowType::Zoom => ZoomMode::Zoom,
            chiaki_settings::settings::WindowType::Stretch => ZoomMode::Stretch,
            _ => ZoomMode::Fit,
        };
        state.want_fullscreen = matches!(
            settings.window_type(),
            chiaki_settings::settings::WindowType::Fullscreen
        );
        // Benutzerdefinierter Zoom (settings/zoom_factor, > 0 = aktiv):
        // startet im Zoom-Modus mit Fit-Skala × Faktor (VideoSurface +
        // GPU-Sink); -1/auto lässt die bisherige Logik laufen.
        let zoom_factor = settings.zoom_factor();
        if zoom_factor > 0.0 {
            state.zoom = ZoomMode::Zoom;
            state.zoom_factor = zoom_factor as f32;
        }
        drop(settings);

        state
    }

    /// FAKE-Mode (env `CHIAKI_UI_FAKE_STREAM`): StreamView ohne Konsole —
    /// synthetisches NV12-Testpattern + Fake-Telemetrie (siehe fake.rs).
    fn start_fake(&mut self) {
        tracing::info!("Stream FAKE-Mode aktiv (CHIAKI_UI_FAKE_STREAM) — keine echte Verbindung");
        let presenter = VideoPresenter::new(fake::WIDTH, fake::HEIGHT);
        let telemetry = StreamTelemetry::new();
        let stop = Arc::new(AtomicBool::new(false));
        let connected = Arc::new(AtomicBool::new(false));
        let pin_enabled = std::env::var("CHIAKI_UI_FAKE_STREAM")
            .map(|v| v.eq_ignore_ascii_case("pin"))
            .unwrap_or(false);
        let pin_flag = Arc::new(AtomicBool::new(false));
        // GPU-Sink für den Fake-Modus (settings/video_output != cpu) — der
        // komplette GPU-Anzeigepfad ist so ohne Konsole testbar (inkl.
        // vsync/frame_pacing-Schalter).
        let mut gpu_handle: Option<GpuSinkHandle> = None;
        {
            let (want, vsync, paced) = {
                let settings = self.backend.settings().lock().unwrap_or_else(|e| e.into_inner());
                (settings.video_output(), settings.vsync_enabled(), settings.frame_pacing())
            };
            if want != "cpu" {
                let config = GpuSinkConfig {
                    vsync,
                    paced,
                    frame_period_us: 1_000_000 / fake::FPS,
                };
                match GpuSink::new(GPUI_WINDOW_TITLE, (fake::WIDTH, fake::HEIGHT), config) {
                    Ok(sink) => {
                        let h = sink.handle();
                        tracing::info!("FAKE-Stream: GPU-Sink aktiv (Upload-Pfad, {}x{})", fake::WIDTH, fake::HEIGHT);
                        gpu_handle = Some(h);
                        self.gpu = gpu_handle.clone();
                        self.gpu_sink = Some(sink);
                    }
                    Err(err) => {
                        tracing::error!("FAKE-Stream: GPU-Sink-Start fehlgeschlagen ({err}) — Presenter-Pfad")
                    }
                }
            }
        }
        fake::start(
            presenter.clone(),
            Arc::clone(&telemetry),
            Arc::clone(&stop),
            Arc::clone(&connected),
            if pin_enabled { Some(Arc::clone(&pin_flag)) } else { None },
            gpu_handle,
        );
        self.presenter = Some(presenter);
        self.telemetry = Some(telemetry);
        self.fake_stop = Some(stop);
        self.fake_connected = Some(connected);
        self.fake_pin = if pin_enabled { Some(pin_flag) } else { None };
    }

    /// Echter Pfad: ConnectRequest auflösen (Settings + Discovery); der
    /// Session-Start passiert in `tick_real` (Station Anmelden).
    fn start_real(&mut self) {
        match resolve_request(&self.backend, &self.host_id) {
            Ok((request, standby)) => {
                self.request = Some(request);
                self.standby = standby;
                if !standby {
                    // Konsole läuft: Station 1 ist erledigt.
                    self.advance_to(Stage::Login);
                }
            }
            Err(err) => {
                self.error = Some(err);
                self.stages[self.stage.index()] = StageState::Failed;
            }
        }
    }

    /// Keyboard-Mapping der Session (aus den Settings aufgelöst).
    fn mapper(&self) -> KeyboardMapper {
        if let Some(session) = self.backend.sessions().active() {
            return session.keyboard_mapper();
        }
        let settings = self.backend.settings().lock().unwrap_or_else(|e| e.into_inner());
        keyboard_mapper_from_settings(&settings)
    }

    // -- Lebenszyklus ------------------------------------------------------

    /// Fake-Thread stoppen (vor `remove_global`).
    pub(crate) fn stop_threads(&mut self) {
        if let Some(stop) = &self.fake_stop {
            stop.store(true, Ordering::Relaxed);
        }
    }

    /// 1×/Frame: Flow, Stats, Controller-Loop, Fenster-Modus.
    pub(crate) fn tick(&mut self, window: &mut Window) {
        // Fenster-Modus (einmalig): settings/window_type == Fullscreen.
        if !self.fullscreen_done {
            self.fullscreen_done = true;
            if self.want_fullscreen && !window.is_fullscreen() {
                window.toggle_fullscreen();
            }
        }

        if self.fake {
            self.tick_fake();
        } else {
            self.tick_real();
        }

        // Konsole-Tastatur-Overlay: Fokus halten (modales Eingabefeld).
        if let Some(overlay) = &self.keyboard {
            if !overlay.focus.is_focused(window) {
                overlay.focus.focus(window);
            }
        }

        self.update_stats();
        self.send_controller_state();
    }

    fn tick_fake(&mut self) {
        let connected = self
            .fake_connected
            .as_ref()
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(false);
        let elapsed = self.started_at.elapsed();
        if !connected {
            // Stationen nach dem fake.rs-Zeitplan.
            if elapsed >= Duration::from_millis(800) && self.stage == Stage::Wake {
                self.advance_to(Stage::Login);
            }
            if elapsed >= Duration::from_millis(1600) && self.stage == Stage::Login {
                self.advance_to(Stage::Calibrate);
            }
            // Optionales Fake-PIN-Event (CHIAKI_UI_FAKE_STREAM=pin).
            if let Some(pin) = &self.fake_pin {
                if pin.load(Ordering::Relaxed) && !self.pin.visible {
                    self.pin.visible = true;
                    self.pin.incorrect = false;
                    self.pin.digits.clear();
                }
            }
        } else if self.stage != Stage::Streaming {
            self.advance_to(Stage::Streaming);
            self.stages[3] = StageState::Done;
            // Fake-PIN-Overlay bleibt bis zur Eingabe offen (Testbarkeit);
            // beim echten Pfad kommt Connected erst nach korrekter PIN.
        }
    }

    fn tick_real(&mut self) {
        if self.error.is_some() {
            return;
        }
        let active = self.backend.sessions().active();
        // Presenter + Telemetrie der aktiven Session übernehmen (einmalig):
        // der Media-Thread pusht Frames in DIESEN Presenter — ohne diese
        // Verdrahtung zeigt die Seite „Kein Video-Signal“, obwohl dekodiert
        // wird (der Fake-Pfad setzt seinen Presenter in start_fake()).
        if self.presenter.is_none() {
            if let Some(active) = &active {
                self.presenter = Some(active.presenter.clone());
                self.telemetry = Some(Arc::clone(&active.telemetry));
                if self.stage == Stage::Login {
                    self.login_active_since = None; // Session da → Heuristik stoppen
                }
            }
        }
        // GPU-Handle übernehmen (einmalig; Zoom-Modus an den Sink übergeben).
        if self.gpu.is_none() {
            if let Some(active) = &active {
                if let Some(gpu) = &active.gpu {
                    self.gpu = Some(gpu.clone());
                    gpu.set_zoom(match self.zoom {
                        ZoomMode::Fit => SinkZoom::Fit,
                        ZoomMode::Zoom => SinkZoom::Zoom,
                        ZoomMode::Stretch => SinkZoom::Stretch,
                    });
                    gpu.set_zoom_factor(self.zoom_factor);
                    tracing::info!("StreamView: GPU-Videopfad übernommen (transparenter Video-Bereich)");
                }
            }
        }
        match self.stage {
            Stage::Wake => {
                // Wakeup einmal senden, dann auf Ready warten (Timeout →
                // trotzdem verbinden, wie im C++ „we'll try anyway").
                if !self.wake_sent {
                    self.wake_sent = true;
                    if let (Some(request), true) = (&self.request, self.standby) {
                        send_wakeup(request);
                    }
                }
                let ready = discovery_state(&self.backend, &self.host_id)
                    == Some(DiscoveryHostState::Ready);
                if ready {
                    self.advance_to(Stage::Login);
                } else if self.wake_sent && self.started_at.elapsed() >= Duration::from_secs(15) {
                    tracing::info!("Wakeup bestätigt sich nicht — Verbindung wird trotzdem versucht");
                    self.advance_to(Stage::Login);
                }
            }
            Stage::Login => {
                if !self.connect_started {
                    self.connect_started = true;
                    if let Some(request) = self.request.clone() {
                        self.login_active_since = Some(Instant::now());
                        self.backend.sessions().start_session(request);
                    }
                }
                // Heuristik (siehe Modul-Doku): Senkusha läuft im
                // session_thread — ohne ausstehende PIN schalten wir 2,5 s
                // nach Session-Start auf „kalibrieren".
                if self.pin.visible {
                    self.login_active_since = None;
                } else if let Some(since) = self.login_active_since {
                    if active.is_some() && since.elapsed() >= Duration::from_millis(2500) {
                        self.advance_to(Stage::Calibrate);
                    }
                }
            }
            Stage::Calibrate | Stage::Streaming => {}
        }
    }

    fn advance_to(&mut self, stage: Stage) {
        let idx = stage.index();
        for (i, state) in self.stages.iter_mut().enumerate() {
            *state = if i < idx {
                StageState::Done
            } else if i == idx {
                if stage == Stage::Streaming {
                    StageState::Done
                } else {
                    StageState::Active
                }
            } else {
                StageState::Pending
            };
        }
        self.stage = stage;
        if stage == Stage::Streaming {
            self.hud_open = true;
        }
    }

    // -- Session-Events ----------------------------------------------------

    /// PSN-Connecting-Stufe anwenden (C++ PsnConnectState → Stationen):
    /// `LinkingConsole` = Control-Hole steht, Holepunch-Phase fertig →
    /// Anmeldestation erledigt, Kalibrierstation aktiv (Regist + Session-
    /// Request + Data-Hole laufen jetzt im session_thread).
    pub(crate) fn on_psn_connect_state(&mut self, state: crate::backend::psn::PsnConnectState) {
        if self.fake {
            return;
        }
        tracing::debug!("PSN connect state: {state:?}");
        match state {
            crate::backend::psn::PsnConnectState::LinkingConsole
            | crate::backend::psn::PsnConnectState::DataConnectionStart => {
                if self.stage < Stage::Calibrate {
                    self.advance_to(Stage::Calibrate);
                }
            }
            crate::backend::psn::PsnConnectState::InitiatingConnection
            | crate::backend::psn::PsnConnectState::DataConnectionFinished => {}
        }
    }

    fn on_session_event(
        &mut self,
        _session_id: u64,
        event: chiaki_core::session::SessionEvent,
        cx: &mut Context<AppShell>,
    ) {
        use chiaki_core::session::SessionEvent as E;
        match event {
            E::Connected => {
                if !self.fake {
                    self.advance_to(Stage::Streaming);
                    self.stages[3] = StageState::Done;
                }
            }
            E::LoginPinRequest(pin_incorrect) => {
                // Anmeldestation bleibt aktiv; PIN-Overlay öffnen.
                self.advance_to(Stage::Login);
                self.pin.visible = true;
                self.pin.incorrect = pin_incorrect;
                self.pin.digits.clear();
            }
            // PSN-Pfad (C++ DataHolepunchProgress → PsnConnectState::Data-
            // ConnectionStart): das Data-Hole wird gepuncht → Kalibrier-
            // station (Senkusha läuft im session_thread danach).
            E::Holepunch { finished: false } => {
                if !self.fake {
                    self.advance_to(Stage::Calibrate);
                }
            }
            // PSN-Auto-Regist (C++ AutoRegistSucceeded → finishAutoRegister):
            // registrierten Host in die Settings-Registry übernehmen.
            E::Regist(host) => {
                let nickname = host.server_nickname.clone();
                let mut registered = chiaki_settings::hosts::RegisteredHost::default();
                registered.target = if host.target.is_ps5() {
                    chiaki_settings::hosts::Target::Ps5One
                } else {
                    chiaki_settings::hosts::Target::Ps4Ten
                };
                registered.ap_ssid = host.ap_ssid.clone();
                registered.ap_bssid = host.ap_bssid.clone();
                registered.ap_key = host.ap_key.clone();
                registered.ap_name = host.ap_name.clone();
                registered.server_mac = HostMac::new(host.server_mac);
                registered.server_nickname = host.server_nickname.clone();
                registered.rp_regist_key = host.rp_regist_key;
                registered.rp_key_type = host.rp_key_type;
                registered.rp_key = host.rp_key;
                let result = self
                    .backend
                    .settings()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .update(|s| s.add_registered_host(registered));
                let display = if nickname.is_empty() { "Konsole".to_string() } else { nickname };
                match result {
                    Ok(()) => {
                        let shell = self.shell.clone();
                        cx.spawn(async move |_shell_weak, cx| {
                            let _ = shell.update(cx, |shell, cx| {
                                shell.push_toast(
                                    ToastData::new(
                                        crate::components::ToastKind::Success,
                                        "Konsole registriert",
                                    )
                                    .message(display),
                                    cx,
                                );
                            });
                        })
                        .detach();
                    }
                    Err(err) => {
                        tracing::error!("PSN-Regist konnte nicht gespeichert werden: {err}");
                    }
                }
            }
            E::KeyboardText(text) => {
                let focus = cx.focus_handle();
                self.keyboard = Some(KeyboardOverlay { text, focus });
            }
            E::KeyboardTextChange(text) => {
                if let Some(overlay) = &mut self.keyboard {
                    overlay.text = text;
                }
            }
            E::KeyboardRemoteClose => {
                self.keyboard = None;
            }
            E::Quit { reason, reason_str } => {
                // Quit-Handling: Session-Ressourcen freigeben (der Cleanup-
                // Thread wartet aufs Stop-Flag), Toast + zurück auf Home.
                self.stop_threads();
                // disconnect_action (settings/disconnect_action): bei einem
                // sauberen Quit (kein Fehler) fährt "sleep" die Konsole
                // automatisch in den Ruhemodus (goto_bed, einmalig pro
                // Stream — das Ruhemodus-Quit erzeugt ein zweites Quit-
                // Event), "nothing" tut nichts, "ask" verhält sich wie
                // bisher ohne automatische Aktion (C++ closeRequested).
                let action = self
                    .backend
                    .settings()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .disconnect_action();
                if !chiaki_core::session::quit_reason_is_error(reason)
                    && action == chiaki_settings::settings::DisconnectAction::AlwaysSleep
                    && !self.auto_bed_sent
                {
                    self.auto_bed_sent = true;
                    if let Some(session) = self.backend.sessions().active() {
                        session.goto_bed();
                    }
                }
                self.backend.sessions().stop_current();
                let is_error = chiaki_core::session::quit_reason_is_error(reason);
                let text = if reason_str.is_empty() {
                    chiaki_core::session::quit_reason_string(reason).to_string()
                } else {
                    reason_str
                };
                let kind = if is_error {
                    crate::components::ToastKind::Danger
                } else {
                    crate::components::ToastKind::Info
                };
                let shell = self.shell.clone();
                let toast_text = text.clone();
                cx.spawn(async move |_shell_weak, cx| {
                    let _ = shell.update(cx, |shell, cx| {
                        shell.push_toast(
                            ToastData::new(kind, "Stream beendet").message(toast_text),
                            cx,
                        );
                        shell.navigate(crate::app::Route::Home, cx);
                    });
                })
                .detach();
                if is_error {
                    self.error = Some(text);
                }
            }
            _ => {}
        }
    }

    // -- Input-Loop (Controller + Tastatur → Session) ------------------------

    /// Key-Down aus dem Stream-Root (page). Liefert `true`, wenn der Key
    /// verbraucht wurde (Overlay-Kommandos) und nicht bubblen soll.
    pub(crate) fn on_key_down(
        &mut self,
        key: &str,
        mods: gpui::Modifiers,
        window: &mut Window,
    ) -> bool {
        self.sync_modifiers(mods);
        // Overlays zuerst (Esc/Digits) — Stop-Propagation entscheidet.
        if self.pin.visible {
            match key {
                "escape" => {
                    self.close_pin();
                    return true;
                }
                "backspace" => {
                    self.pin_pop_digit();
                    return true;
                }
                "enter" => {
                    self.submit_pin();
                    return true;
                }
                other => {
                    if let Some(digit) = digit_char(other) {
                        self.pin_push_digit(digit);
                        return true;
                    }
                }
            }
        }
        if self.keyboard.is_some() {
            // Text tippt das fokussierte TextField; Esc = Abbrechen.
            if key == "escape" {
                self.keyboard_cancel();
                self.refocus_pending = true;
                let _ = window;
                return true;
            }
            return false; // TextField soll tippen — nicht als Gamepad werten
        }
        // Stream-Shortcuts.
        match key {
            "f11" => {
                window.toggle_fullscreen();
                return true;
            }
            "h" if self.stage == Stage::Streaming && !mods.control && !mods.alt => {
                self.toggle_panel();
                return true;
            }
            _ => {}
        }
        if let Some(k) = gpui_key_to_chiaki(key) {
            self.keys.insert(k);
        }
        false
    }

    pub(crate) fn on_key_up(&mut self, key: &str, mods: gpui::Modifiers) {
        self.sync_modifiers(mods);
        if let Some(k) = gpui_key_to_chiaki(key) {
            self.keys.remove(&k);
        }
    }

    fn sync_modifiers(&mut self, mods: gpui::Modifiers) {
        for (on, key) in [
            (mods.control, InKey::Control),
            (mods.shift, InKey::Shift),
            (mods.alt, InKey::Alt),
            (mods.platform, InKey::Meta),
        ] {
            if on {
                self.keys.insert(key);
            } else {
                self.keys.remove(&key);
            }
        }
    }

    /// Kombinierter Controller-State (Gamepads + DualSense + Tastatur-Mapping)
    /// — nur bei Änderung und mit ~120-Hz-Deckel senden (C++:
    /// `SendFeedbackState` nur bei State-Änderung). Die Stick-Deadzone
    /// (settings/stick_deadzone, 0 = aus wie im C++-Client) wird live aus
    /// den Settings gelesen und auf den kombinierten State angewendet.
    fn send_controller_state(&mut self) {
        let Some(session) = self.backend.sessions().active() else { return };
        if !session.is_running() {
            return;
        }
        // Overlays: getippte Zeichen nicht als Gamepad-Input missbrauchen.
        let typing = self.keyboard.is_some() || self.pin.visible;
        let mut states: Vec<ControllerState> = Vec::new();
        states.push(self.backend.controllers().active_controller_state());
        if !typing && !self.keys.is_empty() {
            states.push(self.mapper().apply_keyboard_state(&self.keys));
        }
        let mut state = combine_states(&states);
        let deadzone = {
            let settings = self
                .backend
                .settings()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            settings.stick_deadzone() as f32 / 100.0
        };
        if deadzone > 0.0 {
            let rescale = |v: i16| -> i16 {
                (apply_deadzone(v as f32 / 32767.0, deadzone).clamp(-1.0, 1.0) * 32767.0) as i16
            };
            state.left_x = rescale(state.left_x);
            state.left_y = rescale(state.left_y);
            state.right_x = rescale(state.right_x);
            state.right_y = rescale(state.right_y);
        }
        if self.last_controller.as_ref() != Some(&state)
            && self.last_send.elapsed() >= Duration::from_millis(8)
        {
            self.last_send = Instant::now();
            self.last_controller = Some(state);
            session.send_controller_state(&state);
        }
    }

    // -- Aktionen (Panels/Overlays) ------------------------------------------

    pub fn toggle_panel(&mut self) {
        self.panel_open = !self.panel_open;
    }

    pub fn toggle_hud(&mut self) {
        self.hud_open = !self.hud_open;
    }

    pub fn cycle_zoom(&mut self) {
        self.zoom = self.zoom.next();
        // GPU-Pfad: Viewport-Mathe läuft im Sink-Render-Thread (Fit/Zoom/
        // Stretch wie in VideoSurface.paint, inkl. benutzerdefinierter
        // Zoom-Faktor).
        if let Some(gpu) = &self.gpu {
            gpu.set_zoom(match self.zoom {
                ZoomMode::Fit => SinkZoom::Fit,
                ZoomMode::Zoom => SinkZoom::Zoom,
                ZoomMode::Stretch => SinkZoom::Stretch,
            });
            gpu.set_zoom_factor(self.zoom_factor);
        }
    }

    /// GPU-Videopfad aktiv UND gesund → Video-Bereich transparent malen
    /// (das D3D11-Sink-Fenster zeigt das Bild unter dem gpui-Fenster).
    pub fn gpu_active(&self) -> bool {
        self.stage == Stage::Streaming
            && self.gpu.as_ref().is_some_and(|g| !g.is_lost())
    }

    pub fn toggle_mic(&mut self) {
        if let Some(session) = self.backend.sessions().active() {
            self.mic_unmuted = !self.mic_unmuted;
            session.set_mic_unmuted(self.mic_unmuted);
        }
    }

    pub fn goto_bed(&mut self) {
        if let Some(session) = self.backend.sessions().active() {
            session.goto_bed();
        }
    }

    /// Markiert die Auto-Ruhemodus-Aktion als erledigt (Ruhemodus-Knopf des
    /// Disconnect-Dialogs — der Quit-Handler darf dann bei
    /// disconnect_action "sleep" kein zweites goto_bed senden).
    pub(crate) fn mark_auto_bed_sent(&mut self) {
        self.auto_bed_sent = true;
    }

    pub fn disconnect_now(&mut self) {
        self.stop_threads();
        self.backend.sessions().stop_current();
    }

    // PIN -------------------------------------------------------------------

    pub fn pin_push_digit(&mut self, digit: char) {
        if !self.pin.visible || self.pin.digits.chars().count() >= 8 {
            return;
        }
        self.pin.digits.push(digit);
        if self.pin.digits.chars().count() == 8 {
            self.submit_pin();
        }
    }

    pub fn pin_pop_digit(&mut self) {
        self.pin.digits.pop();
    }

    pub fn submit_pin(&mut self) {
        if self.pin.digits.chars().count() != 8 {
            return;
        }
        let pin = self.pin.digits.clone();
        if let Some(session) = self.backend.sessions().active() {
            session.set_login_pin(&pin);
        }
        // Overlay schließen; bei falscher PIN kommt ein neues
        // LoginPinRequest(true)-Event und öffnet es erneut.
        self.pin.visible = false;
        self.pin.digits.clear();
    }

    pub fn close_pin(&mut self) {
        self.pin.visible = false;
        self.pin.digits.clear();
    }

    // Konsole-Tastatur -------------------------------------------------------

    pub fn keyboard_set_text(&mut self, text: String) {
        if let Some(overlay) = &mut self.keyboard {
            overlay.text = text;
        }
    }

    pub fn keyboard_accept(&mut self) {
        let text = self.keyboard.as_ref().map(|k| k.text.clone()).unwrap_or_default();
        if let Some(session) = self.backend.sessions().active() {
            session.keyboard_submit(&text);
        }
        self.keyboard = None;
    }

    pub fn keyboard_cancel(&mut self) {
        if let Some(session) = self.backend.sessions().active() {
            session.keyboard_reject();
        }
        self.keyboard = None;
    }

    // -- Stats (1×/Frame) ---------------------------------------------------

    fn update_stats(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_stats_at).as_secs_f32();
        if dt < 0.05 {
            return;
        }
        self.last_stats_at = now;

        let (bytes, frames, lost) = match &self.telemetry {
            Some(t) => (
                t.video_bytes.load(Ordering::Relaxed),
                t.video_frames.load(Ordering::Relaxed),
                t.video_frames_lost.load(Ordering::Relaxed),
            ),
            None => (0, 0, 0),
        };
        let inst_mbit = (bytes.saturating_sub(self.last_video_bytes) as f32 * 8.0 / dt) / 1e6;
        self.last_video_bytes = bytes;
        self.bitrate_ema = if self.bitrate_ema == 0.0 {
            inst_mbit
        } else {
            self.bitrate_ema * 0.9 + inst_mbit * 0.1
        };

        if let Some(presenter) = &self.presenter {
            let stats = presenter.stats();
            let presented = stats.frames_presented;
            let inst_fps = (presented.saturating_sub(self.last_presented) as f32) / dt;
            self.last_presented = presented;
            self.fps_ema = if self.fps_ema == 0.0 {
                inst_fps
            } else {
                self.fps_ema * 0.9 + inst_fps * 0.1
            };
            // Frame-Zeit (CPU-Pfad) = Presenter-Overhead (Alloc + NV12→BGRA +
            // Wrap). Im GPU-Pfad bleibt der Presenter ungenutzt (alle Werte
            // 0) — dort zählt die Media-Thread-EMA (siehe unten).
            self.stats.frame_time_ms = (stats.alloc_us.mean_us
                + stats.conversion_us.mean_us
                + stats.wrap_us.mean_us) as f32
                / 1000.0;
        }
        // Sink-FPS (GPU-Pfad: presentet der D3D11-Sink, der Presenter bleibt
        // bei 0 — dieselbe EMA-Mathematik wie die Presenter-FPS oben). Wird
        // IMMER gepflegt (Badge + Debug-Zeile), nicht nur bei aktivem Debug.
        let sink_presented_total = self.gpu.as_ref().map(|g| g.stats_values()).map(|s| s.frames_presented).unwrap_or(0);
        if let Some(gpu) = &self.gpu {
            let presented = gpu.stats_values().frames_presented;
            let inst = (presented.saturating_sub(self.last_sink_presented) as f32) / dt;
            self.last_sink_presented = presented;
            self.sink_fps_ema = if self.sink_fps_ema == 0.0 {
                inst
            } else {
                self.sink_fps_ema * 0.9 + inst * 0.1
            };
        }
        // FPS/Frame-Time je Pfad: GPU-Pfad (Sink präsentiert) → Sink-FPS +
        // Media-Thread-EMA; CPU-Pfad → Presenter-FPS + Presenter-Overhead.
        if sink_presented_total > 0 {
            self.stats.fps = self.sink_fps_ema;
            self.stats.frame_time_ms = self
                .telemetry
                .as_ref()
                .map(|t| t.media_frame_us.load(Ordering::Relaxed))
                .unwrap_or(0) as f32
                / 1000.0;
        } else {
            self.stats.fps = self.fps_ema;
        }
        self.stats.bitrate_mbit = self.bitrate_ema;
        self.stats.loss_pct = if frames > 0 {
            Some(lost as f32 * 100.0 / frames as f32)
        } else {
            None
        };
        if let Some(t) = &self.telemetry {
            self.stats.audio_fill_ms = t.audio_fill_ms_x10.load(Ordering::Relaxed) as f32 / 10.0;
            self.stats.decoder = t.decoder_backend_name();
            self.stats.haptics = t.haptics_mode_name();
        }

        // WLAN-/Netzwerk-Drops (settings/wifi_dropped_notif_percent, C++
        // wifi-dropped-Hinweis): Frame-Verlust im 5-Sekunden-Fenster
        // (video_frames_lost/video_frames) > X % → EINMALIG pro Session ein
        // Warn-Toast; der Schwellwert wird live gelesen (commit_setting).
        if !self.wifi_warned && self.stage == Stage::Streaming {
            let threshold = {
                let settings = self
                    .backend
                    .settings()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                settings.wifi_dropped_notif() as f32
            };
            let now = Instant::now();
            match self.wifi_window {
                None => self.wifi_window = Some((now, frames, lost)),
                Some((start, f0, l0)) if now.duration_since(start) >= Duration::from_secs(5) => {
                    self.wifi_window = Some((now, frames, lost));
                    let window_frames = frames.saturating_sub(f0);
                    let window_lost = lost.saturating_sub(l0);
                    if window_frames > 0 {
                        let pct = window_lost as f32 * 100.0 / window_frames as f32;
                        if pct > threshold {
                            self.wifi_warned = true;
                            self.pending_wifi_warn = true;
                            tracing::warn!(
                                "WLAN/Netzwerk-Drops: {pct:.1} % Frame-Verlust im 5-s-Fenster (> {threshold:.0} %)"
                            );
                        }
                    }
                }
                Some(_) => {}
            }
        }

        // RTT: bewusst **direkter Poll über das Backend** statt neuem
        // UiEvent — der Wert steht nach der Senkusha-Phase fest und ändert
        // sich nicht mehr; die Stats laufen ohnehin 1×/Frame (siehe
        // Modul-Doku „HUD-Statistik"). Echter Wert = `Session::rtt_us()`
        // (C: session->rtt aus der Senkusha); im FAKE-Mode gibt es keine
        // Session, dort liefert die Fake-Telemetrie den Wert.
        self.stats.rtt_ms = match self.backend.sessions().active() {
            Some(session) => session.rtt_us().map(|us| us as f32 / 1000.0),
            None => {
                let x10 = self
                    .telemetry
                    .as_ref()
                    .map(|t| t.rtt_ms_x10.load(Ordering::Relaxed))
                    .unwrap_or(0);
                (x10 > 0).then_some(x10 as f32 / 10.0)
            }
        };

        // Debug-Zeile (settings/overlay_debug): Live-Lesen wie der WLAN-
        // Schwellwert; nur rechnen, wenn sie auch angezeigt wird.
        let want_debug = {
            let settings = self
                .backend
                .settings()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            settings.overlay_debug()
        };
        if want_debug {
            // Sink-FPS-EMA wird oben IMMER gepflegt (Badge braucht sie im
            // GPU-Pfad) — hier nur die Zeile bauen.
            self.debug_line = Some(self.build_debug_line());
        } else {
            self.debug_line = None;
        }
    }

    /// Debug-Zeile unterm HUD (`settings/overlay_debug`), live Werte aus
    /// Presenter-/Sink-/Slot-Statistik:
    /// `presented X fps (drops Y) | media Z.Z ms (dec A / vsr B) |
    ///  slot-drops N | conv P.Q ms p95 | sink gen/uploads/drops`
    ///
    /// Datenquellen:
    /// * Presenter ([`VideoPresenter::stats`]): fps (EMA aus
    ///   frames_presented), drops = frames_dropped, media ms = Alloc+NV12→
    ///   BGRA+Wrap-Mittel (dieselbe Zahl wie die FRAME-TIME-Badge),
    ///   conv p95 = conversion_us.p95_us, gen = frames_generated.
    /// * Sink (`GpuSinkHandle::stats_values`, nur GPU-Pfad): uploads =
    ///   cpu_uploads + d3d11_copies, drops = frames_dropped; im GPU-Pfad
    ///   stammen presented/drops aus dem Sink (der Presenter wird dort nicht
    ///   angefasst).
    /// * Slot (`ActiveSession` → VideoSlot::dropped): verworfene Frames der
    ///   1-Slot-Queue (im FAKE-Mode ohne Session „—").
    /// * Telemetrie: Decoder-Backend + VSR-Status.
    fn build_debug_line(&self) -> String {
        let presenter = self.presenter.as_ref().map(|p| p.stats());
        let sink = self.gpu.as_ref().map(|g| g.stats_values());
        let (vsr_active, vsr_scale) = match &self.telemetry {
            Some(t) => (
                t.vsr_active.load(Ordering::Relaxed),
                t.vsr_scale.load(Ordering::Relaxed),
            ),
            None => (false, 0),
        };

        // presented/drops: Sink-Zähler, sobald ein GPU-Pfad lief; sonst der
        // Presenter (CPU-Pfad). FPS entsprechend: Sink-EMA im GPU-Pfad,
        // Presenter-EMA (Badge-Wert) im CPU-Pfad.
        let sink_presented = sink.as_ref().map(|s| s.frames_presented).unwrap_or(0);
        let fps = if sink_presented > 0 { self.sink_fps_ema } else { self.stats.fps };
        let drops = match &sink {
            Some(s) if sink_presented > 0 => s.frames_dropped,
            _ => presenter.as_ref().map(|p| p.frames_dropped).unwrap_or(0),
        };
        let media_ms = self.stats.frame_time_ms;
        let dec = self
            .telemetry
            .as_ref()
            .map(|t| t.decoder_backend_name())
            .unwrap_or_else(|| "—".into());
        let vsr = if vsr_active {
            if vsr_scale > 100 {
                format!("{}x", vsr_scale / 100)
            } else {
                "an".to_string()
            }
        } else {
            "aus".to_string()
        };
        let slot_drops = match self.backend.sessions().active() {
            Some(active) => active.shared_video_slot().dropped().to_string(),
            None => "—".to_string(),
        };
        let conv = match presenter.as_ref().map(|p| p.conversion_us) {
            Some(summary) if summary.count > 0 => {
                format!("{:.2} ms p95", summary.p95_us as f32 / 1000.0)
            }
            _ => "—".to_string(),
        };
        let gen = presenter.as_ref().map(|p| p.frames_generated).unwrap_or(0);
        let (uploads, sink_drops, too_fast, dt_ema, dt_min, dt_max, jit) = match &sink {
            Some(s) => (
                (s.cpu_uploads + s.d3d11_copies).to_string(),
                s.frames_dropped.to_string(),
                s.present_too_fast.to_string(),
                format!("{} µs", s.present_dt_ema_us),
                format!("{}", s.present_dt_min_us),
                format!("{}", s.present_dt_max_us),
                format!("{} µs", s.present_jitter_ema_us),
            ),
            None => (
                "—".to_string(),
                "—".to_string(),
                "—".to_string(),
                "—".to_string(),
                "—".to_string(),
                "—".to_string(),
                "—".to_string(),
            ),
        };

        format!(
            "presented {fps:.1} fps (drops {drops}) | media {media_ms:.1} ms (dec {dec} / vsr {vsr}) \
             | slot-drops {slot_drops} | conv {conv} | sink {gen}/{uploads}/{sink_drops} \
             | too-fast {too_fast}, dt {dt_ema} ({dt_min}..{dt_max}), jit {jit}",
        )
    }

    /// Ausstehenden WLAN-Warn-Toast abholen (einmalig true; ausgeliefert von
    /// `ensure_and_tick`, weil `update_stats` kein `cx` sieht).
    pub(crate) fn take_wifi_warn(&mut self) -> bool {
        std::mem::take(&mut self.pending_wifi_warn)
    }

    pub fn snapshot(&self) -> StreamSnapshot {
        let (vsr_active, vsr_scale) = match &self.telemetry {
            Some(t) => (
                t.vsr_active.load(Ordering::Relaxed),
                t.vsr_scale.load(Ordering::Relaxed),
            ),
            None => (false, 0),
        };
        let (vsr_badge_wanted, overlay, doubleclick_fullscreen, hide_cursor) = {
            let settings = self
                .backend
                .settings()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            (
                settings.show_vsr_badge(),
                super::hud::OverlayConfig::from_settings(&settings),
                settings.fullscreen_double_click_enabled(),
                settings.hide_cursor(),
            )
        };
        StreamSnapshot {
            stage: self.stage,
            stages: self.stages,
            host_label: self.host_label.clone(),
            error: self.error.clone(),
            status_line: self.status_line(),
            fake: self.fake,
            pin_visible: self.pin.visible,
            pin_digits: self.pin.digits.clone(),
            pin_incorrect: self.pin.incorrect,
            keyboard_open: self.keyboard.is_some(),
            keyboard_text: self.keyboard.as_ref().map(|k| k.text.clone()).unwrap_or_default(),
            panel_open: self.panel_open,
            hud_open: self.hud_open,
            zoom: self.zoom,
            zoom_factor: self.zoom_factor,
            mic_unmuted: self.mic_unmuted,
            doubleclick_fullscreen,
            hide_cursor,
            stats: self.stats.clone(),
            vsr_active,
            vsr_scale,
            vsr_badge_wanted,
            video_size: self.presenter.as_ref().map(|p| p.size()).unwrap_or((0, 0)),
            overlay,
            debug_line: self.debug_line.clone(),
        }
    }

    fn status_line(&self) -> String {
        match self.stage {
            Stage::Wake => {
                if self.fake {
                    "Sende Wakeup-Paket (fake)…".into()
                } else if self.standby {
                    "Konsole ist im Ruhemodus — Wakeup gesendet, warte auf Reaktion…".into()
                } else {
                    "Konsole ist erreichbar".into()
                }
            }
            Stage::Login => {
                if self.pin.visible {
                    "Konsole verlangt die Login-PIN".into()
                } else {
                    "Session-Anfrage läuft (Session-Request + Ctrl)…".into()
                }
            }
            Stage::Calibrate => "Senkusha: RTT/MTU-Kalibrierung mit der Konsole…".into(),
            Stage::Streaming => "Stream läuft".into(),
        }
    }
}

/// gpui liefert Ziffern als "0".."9" (auch auf dem Numpad).
fn digit_char(key: &str) -> Option<char> {
    let mut chars = key.chars();
    let (c @ '0'..='9', None) = (chars.next()?, chars.next()) else {
        return None;
    };
    Some(c)
}

// ---------------------------------------------------------------------------
// Host-/Request-Auflösung (Settings + Discovery)
// ---------------------------------------------------------------------------

fn host_label(backend: &Backend, host: &HostId) -> String {
    match host {
        HostId::Registered { mac } => backend
            .settings()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .registered_host(HostMac::new(*mac))
            .map(|h| {
                if h.server_nickname.is_empty() {
                    mac_string(mac)
                } else {
                    h.server_nickname.clone()
                }
            })
            .unwrap_or_else(|| mac_string(mac)),
        HostId::Manual { id } => backend
            .settings()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .manual_hosts()
            .into_iter()
            .find(|m| m.id == *id)
            .map(|m| m.host.clone())
            .unwrap_or_else(|| format!("Host {id}")),
        // PSN-Remote-Host: Nickname aus der Geräteliste, sonst DUID.
        HostId::Psn { duid } => backend
            .psn()
            .device(duid)
            .map(|d| d.nickname)
            .unwrap_or_else(|| format!("PSN {duid}")),
        HostId::Address { host } => host.clone(),
    }
}

fn mac_string(mac: &[u8; 6]) -> String {
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

/// Discovery-Host passend zur HostId (MAC aus `host-id`, gleiche Parse-Regel
/// wie `pages::home::mac_from_host_id`).
fn matching_discovery_host(
    backend: &Backend,
    host: &HostId,
) -> Option<chiaki_core::discovery::DiscoveryHost> {
    let hosts = backend.discovery().hosts();
    match host {
        HostId::Registered { mac } => hosts.into_iter().find(|h| {
            crate::pages::home::mac_from_host_id(h.host_id.as_deref().unwrap_or("")).as_ref()
                == Some(mac)
        }),
        HostId::Address { host } => hosts.into_iter().find(|h| &h.host_addr == host),
        _ => None,
    }
}

fn discovery_state(backend: &Backend, host: &HostId) -> Option<DiscoveryHostState> {
    matching_discovery_host(backend, host).map(|h| h.state)
}

/// Löst die HostId zu `ConnectRequest` + Standby-Flag auf
/// (Settings-Registry × Discovery-Adressen; wie HomeView::connect im C++).
pub fn resolve_request(backend: &Backend, host: &HostId) -> Result<(ConnectRequest, bool), String> {
    let settings = backend.settings().lock().unwrap_or_else(|e| e.into_inner());
    match host {
        HostId::Registered { mac } => {
            let registered = settings
                .registered_host(HostMac::new(*mac))
                .cloned()
                .ok_or_else(|| format!("Host {} ist nicht registriert", mac_string(mac)))?;
            // Adresse: Discovery → verknüpfter ManualHost.
            let addr = matching_discovery_host(backend, host)
                .map(|h| h.host_addr)
                .or_else(|| {
                    settings
                        .manual_hosts()
                        .into_iter()
                        .find(|m| m.registered_mac.mac() == mac)
                        .map(|m| m.host.clone())
                })
                .ok_or_else(|| {
                    "Konsole nicht sichtbar — bitte Discovery abwarten oder Adresse prüfen".to_string()
                })?;
            let standby = matching_discovery_host(backend, host)
                .map(|h| h.state == DiscoveryHostState::Standby)
                .unwrap_or(false);
            Ok((
                ConnectRequest::from_registered(&registered, addr, LinkQuality::Local),
                standby,
            ))
        }
        HostId::Manual { id } => {
            let manual = settings
                .manual_hosts()
                .into_iter()
                .find(|m| m.id == *id)
                .ok_or_else(|| format!("Manueller Host {id} nicht gefunden"))?;
            let registered = settings
                .registered_host(manual.registered_mac)
                .cloned()
                .ok_or_else(|| {
                    "Zum manuellen Host ist keine Registrierung hinterlegt".to_string()
                })?;
            Ok((
                ConnectRequest::from_manual(&manual, &registered, LinkQuality::Local),
                false,
            ))
        }
        HostId::Address { host } => Err(format!(
            "Direktverbindung zu {host} braucht eine Registrierung (Auto-Regist folgt)"
        )),
        // PSN-Remote-Verbindung (C++ connectToHost-PSN-Zweig): ConnectRequest
        // mit Holepunch-Session aus den Settings-Token bauen (Token/Account-
        // ID-Prüfung inklusive — ohne PSN-Login gibt es einen sauberen
        // Fehler, der als Toast/Station-Fehler landet).
        HostId::Psn { duid } => {
            // PS4-Platzhalter-DUID ("Main PS4 Console") sonstige Auflösung
            // über die Geräteliste; ohne Eintrag: PS5 (die Device-Liste des
            // C++ listet nur PS5-Geräte).
            let ps5 = backend
                .psn()
                .device(duid)
                .map(|d| d.ps5)
                .unwrap_or(*duid != crate::backend::psn::PS4_PLACEHOLDER_DUID);
            let request = crate::backend::psn::build_psn_connect_request(&settings, duid, ps5)?;
            Ok((request, false))
        }
    }
}

/// Wakeup-Paket senden (chiaki_core::discovery::wakeup). PS5-Credential =
/// erste 8 Bytes des rp_regist_key als Big-Endian-u64 (der WAKEUP-Request
/// transportiert es hex-dekodiert als rp-registkey — wie im C++).
/// Läuft in einem Wegwerf-Thread (getaddrinfo blockt).
fn send_wakeup(request: &ConnectRequest) {
    let host = request.host.clone();
    let ps5 = request.ps5;
    let mut credential = 0u64;
    if ps5 {
        for b in request.regist_key.iter().take(8) {
            credential = (credential << 8) | u64::from(*b);
        }
    }
    let spawned = std::thread::Builder::new()
        .name("chiaki-ui-wakeup".into())
        .spawn(move || match chiaki_core::discovery::wakeup(None, &host, credential, ps5) {
            Ok(()) => tracing::info!("Wakeup an {host} gesendet"),
            Err(err) => tracing::warn!("Wakeup an {host} fehlgeschlagen: {err:?}"),
        });
    if let Err(err) = spawned {
        tracing::error!("Wakeup-Thread konnte nicht gestartet werden: {err}");
    }
}

// ---------------------------------------------------------------------------
// gpui-Key → chiaki_input::Key (Tastatur-Mapping)
// ---------------------------------------------------------------------------

/// Port von `StreamSession::HandleKeyboardEvent`: gpui-Keystroke-Name →
/// neutrales [`chiaki_input::Key`].
pub fn gpui_key_to_chiaki(key: &str) -> Option<InKey> {
    Some(match key {
        "space" => InKey::Space,
        "enter" => InKey::Return,
        "escape" => InKey::Escape,
        "backspace" => InKey::Backspace,
        "tab" => InKey::Tab,
        "up" => InKey::Up,
        "down" => InKey::Down,
        "left" => InKey::Left,
        "right" => InKey::Right,
        "insert" => InKey::Insert,
        "delete" => InKey::Delete,
        "home" => InKey::Home,
        "end" => InKey::End,
        "pageup" => InKey::PageUp,
        "pagedown" => InKey::PageDown,
        "shift" => InKey::Shift,
        "control" => InKey::Control,
        "alt" => InKey::Alt,
        "meta" | "super" => InKey::Meta,
        "f1" => InKey::F1,
        "f2" => InKey::F2,
        "f3" => InKey::F3,
        "f4" => InKey::F4,
        "f5" => InKey::F5,
        "f6" => InKey::F6,
        "f7" => InKey::F7,
        "f8" => InKey::F8,
        "f9" => InKey::F9,
        "f10" => InKey::F10,
        "f11" => InKey::F11,
        "f12" => InKey::F12,
        "[" => InKey::BracketLeft,
        "]" => InKey::BracketRight,
        "\\" => InKey::Backslash,
        "-" => InKey::Minus,
        "=" => InKey::Equal,
        "," => InKey::Comma,
        "." => InKey::Period,
        "/" => InKey::Slash,
        ";" => InKey::Semicolon,
        "'" => InKey::Quote,
        "`" => InKey::GraveAccent,
        other => {
            let mut chars = other.chars();
            let (c, None) = (chars.next()?, chars.next()) else {
                return None;
            };
            letter_key(c)?
        }
    })
}

fn letter_key(c: char) -> Option<InKey> {
    use InKey as K;
    Some(match c {
        'a' | 'A' => K::A,
        'b' | 'B' => K::B,
        'c' | 'C' => K::C,
        'd' | 'D' => K::D,
        'e' | 'E' => K::E,
        'f' | 'F' => K::F,
        'g' | 'G' => K::G,
        'h' | 'H' => K::H,
        'i' | 'I' => K::I,
        'j' | 'J' => K::J,
        'k' | 'K' => K::K,
        'l' | 'L' => K::L,
        'm' | 'M' => K::M,
        'n' | 'N' => K::N,
        'o' | 'O' => K::O,
        'p' | 'P' => K::P,
        'q' | 'Q' => K::Q,
        'r' | 'R' => K::R,
        's' | 'S' => K::S,
        't' | 'T' => K::T,
        'u' | 'U' => K::U,
        'v' | 'V' => K::V,
        'w' | 'W' => K::W,
        'x' | 'X' => K::X,
        'y' | 'Y' => K::Y,
        'z' | 'Z' => K::Z,
        '0' => K::Num0,
        '1' => K::Num1,
        '2' => K::Num2,
        '3' => K::Num3,
        '4' => K::Num4,
        '5' => K::Num5,
        '6' => K::Num6,
        '7' => K::Num7,
        '8' => K::Num8,
        '9' => K::Num9,
        _ => return None,
    })
}
