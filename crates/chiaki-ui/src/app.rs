//! AppShell: gpui-Fenster, NavRail, Seitenwechsel, Fokus-Verwaltung,
//! Toast-/Dialog-Stacks und die Backend-Event-Loop.
//!
//! Das globale App-Model ist die gpui-`Entity<AppShell>` — Seiten erhalten
//! in ihren `*_page()`-Funktionen `&mut AppShell` + `&mut Context<AppShell>`
//! und lesen/schreiben direkt die Felder (`shell.route`, `shell.toasts`, …).
//! Backend-Zugriff immer über `shell.backend`.

use std::time::Duration;

use gpui::{
    div, px, App, Context, FocusHandle, Focusable, InteractiveElement as _, IntoElement,
    KeyDownEvent, ParentElement, Render, Styled, Window,
};

use crate::backend::{Backend, HostId, UiEvent};
use crate::components::{Dialog, IconButton, ModalLayer, ToastData, ToastId, ToastView};
use crate::icons;
use crate::motion::{self, Transition};
use crate::pages;
use crate::theme;

// ---------------------------------------------------------------------------
// Route
// ---------------------------------------------------------------------------

/// Die 5 Bereiche der Informationsarchitektur (ui-v2-spec §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Home,
    Consoles,
    Settings,
    Info,
    /// Stream-Ansicht zu einem Host (Connecting-Sequence/HUD).
    Stream(HostId),
}

impl Route {
    /// (Icon, Label, ist NavRail-Eintrag?) — Stream ist kein Rail-Eintrag.
    pub fn nav_items() -> [(Route, &'static str, &'static str); 4] {
        [
            (Route::Home, icons::paths::HOME, "Home"),
            (Route::Consoles, icons::paths::CONSOLE, "Konsolen"),
            (Route::Settings, icons::paths::SETTINGS, "Einstellungen"),
            (Route::Info, icons::paths::INFO, "Info"),
        ]
    }
}

// ---------------------------------------------------------------------------
// AppShell
// ---------------------------------------------------------------------------

/// Globales App-Model.
pub struct AppShell {
    pub backend: Backend,
    /// Aktuelle Route (Navigation).
    pub route: Route,
    /// Laufende Seitenwechsel-Transition (Fade+Slide).
    pub transition: Option<Transition>,
    /// Toast-Stack (unten rechts).
    pub toasts: Vec<ToastData>,
    /// Dialog-Stack (Modals; oberster = letzter Eintrag).
    pub dialogs: Vec<Dialog>,
    /// Registrierungs-Wizard offen (von der Konsolen-Seite gesetzt).
    pub show_regist_wizard: bool,
    /// Seitenzustand (Wizard-Formular, Konsolen-Filter, Kachel-Fokus-Handles,
    /// Manueller-Host-Formular) — CONTRACT-UI §5 erlaubt neue Shell-Felder.
    pub regist_wizard: pages::regist_wizard::WizardState,
    next_toast_id: u64,
    /// Fokus des Shells (Key-Dispatch-Wurzel).
    pub shell_focus: FocusHandle,
    /// Fokus-Handles der NavRail-Einträge (parallel zu Route::nav_items()).
    pub rail_focus: Vec<FocusHandle>,
}

impl AppShell {
    pub fn new(backend: Backend, cx: &mut Context<Self>) -> Self {
        let rail_focus = (0..Route::nav_items().len()).map(|_| cx.focus_handle()).collect();
        // FAKE-Stream-Smoke (StreamView-Agent): mit `CHIAKI_UI_FAKE_STREAM=1`
        // startet die App direkt in der Stream-Ansicht (Testpattern ohne
        // Konsole) —`=pin` fordert zusätzlich eine Fake-Login-PIN an. Ohne
        // diese Env startet die App IMMER auf Home — Verbinden ist bewusst
        // nur manuell per Klick (Auto-Connect beim Start wurde auf
        // Benutzerwunsch entfernt; siehe SETTINGS-AUDIT.md).
        let initial_route = if std::env::var("CHIAKI_UI_FAKE_STREAM")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false)
        {
            Route::Stream(HostId::Address { host: "FAKE-STREAM".into() })
        } else {
            Route::Home
        };

        Self {
            backend,
            route: initial_route,
            transition: None,
            toasts: Vec::new(),
            dialogs: Vec::new(),
            show_regist_wizard: false,
            regist_wizard: pages::regist_wizard::WizardState::new(cx),
            next_toast_id: 1,
            shell_focus: cx.focus_handle(),
            rail_focus,
        }
    }

    // -- Navigation ---------------------------------------------------------

    /// Seitenwechsel mit Fade+Slide-12px-220ms-OutCubic (Spec §1.4).
    ///
    /// Stream-Ansicht-Lifecycle (StreamView-Agent): beim Verlassen von
    /// `Route::Stream` werden Fake-Thread + Session sauber gestoppt und das
    /// Stream-Global entfernt (`pages::stream::shutdown`).
    pub fn navigate(&mut self, route: Route, cx: &mut Context<Self>) {
        if self.route == route {
            return;
        }
        if matches!(self.route, Route::Stream(_)) && !matches!(route, Route::Stream(_)) {
            pages::stream::shutdown(&self.backend, cx);
        }
        tracing::debug!("Navigation: {:?} → {:?}", self.route, route);
        self.route = route;
        self.transition = Some(Transition::start_now());
        cx.notify();
    }

    // -- Toasts -------------------------------------------------------------

    /// Toast anzeigen (mit Auto-Timeout, außer timeout == 0).
    pub fn push_toast(&mut self, mut toast: ToastData, cx: &mut Context<Self>) {
        toast.id = self.next_toast_id;
        self.next_toast_id += 1;
        let id = toast.id;
        let timeout = toast.timeout;
        self.toasts.push(toast);
        if self.toasts.len() > 5 {
            self.toasts.remove(0); // Stack-Deckel (Spec: nichts hüpft)
        }
        cx.notify();
        if timeout > Duration::ZERO {
            spawn_toast_dismiss(id, timeout, cx);
        }
    }

    pub fn dismiss_toast(&mut self, id: ToastId, cx: &mut Context<Self>) {
        self.toasts.retain(|t| t.id != id);
        cx.notify();
    }

    // -- Dialoge ------------------------------------------------------------

    /// Dialog auf den Stack legen.
    pub fn push_dialog(&mut self, dialog: Dialog, cx: &mut Context<Self>) {
        self.dialogs.push(dialog);
        cx.notify();
    }

    /// Obersten Dialog schließen.
    pub fn pop_dialog(&mut self, cx: &mut Context<Self>) {
        self.dialogs.pop();
        cx.notify();
    }

    /// Button-Index im obersten Dialog gedrückt: `closes` → pop, dann Action.
    ///
    /// Bewusst als assoziierte Funktion mit `WeakEntity` + `&mut App`: Die
    /// Action läuft NACH dem `shell.update` (außerhalb des Entity-Borrows) —
    /// Dialog-Actions rufen selbst `shell.update` auf (Trennen → disconnect +
    /// navigate, Kontextmenü → connect). Ein Aufruf innerhalb dieses Updates
    /// wäre ein verbotener Re-Entry und bricht die App mit „cannot update
    /// AppShell while it is already being updated“ ab.
    pub fn invoke_dialog_button(
        shell: &gpui::WeakEntity<Self>,
        index: usize,
        window: &mut Window,
        cx: &mut App,
    ) {
        let action = shell
            .update(cx, |shell, cx| {
                // Action aus dem Dialog nehmen (Option::take braucht &mut);
                // pop erst nach dem Borrow-Scope.
                let (closes, action) = {
                    let Some(dialog) = shell.dialogs.last_mut() else { return None };
                    let Some(button) = dialog.buttons.get_mut(index) else { return None };
                    let action = button.action.take();
                    let closes = button.closes || action.is_some();
                    (closes, action)
                };
                if closes {
                    shell.dialogs.pop();
                    cx.notify();
                }
                Some(action)
            })
            // Result → Option (update) → Option (Closure) → Option (action).
            .ok()
            .flatten()
            .flatten();
        if let Some(action) = action {
            action(window, cx);
        }
    }

    /// Ist ein Dialog offen?
    pub fn has_dialog(&self) -> bool {
        !self.dialogs.is_empty()
    }

    // -- Esc / Fokus --------------------------------------------------------

    /// Esc = Zurück/Kontext abbrechen (Spec §1.3): Dialog zu → Stream
    /// (laufender Stream): Trennen-Bestätigung statt hartem Rausnavigieren
    /// (StreamView-Agent) → Route nach Home.
    pub fn handle_escape(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.pop_dialog_if_open(cx) {
            return;
        }
        if pages::stream::maybe_open_disconnect_dialog(self, cx) {
            return;
        }
        if !matches!(self.route, Route::Home) {
            self.navigate(Route::Home, cx);
        }
    }

    fn pop_dialog_if_open(&mut self, cx: &mut Context<Self>) -> bool {
        if self.dialogs.is_empty() {
            false
        } else {
            self.dialogs.pop();
            cx.notify();
            true
        }
    }

    /// Tab/Shift-Tab: gpui-Fokuszyklus.
    fn handle_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_ref() {
            "tab" => {
                if event.keystroke.modifiers.shift {
                    window.focus_prev();
                } else {
                    window.focus_next();
                }
            }
            "escape" => self.handle_escape(window, cx),
            _ => {}
        }
    }

    // -- Backend-Events -----------------------------------------------------

    /// 1×/Frame: Backend-Event-Queue leeren und anwenden.
    pub fn apply_events(&mut self, cx: &mut Context<Self>) {
        let events = self.backend.poll_events();
        if events.is_empty() {
            return;
        }
        for event in events {
            match event {
                UiEvent::HostFound(_)
                | UiEvent::HostRemoved { .. }
                | UiEvent::HostsChanged
                | UiEvent::Regist(_) => {
                    // Seiten lesen discovery.hosts() direkt beim Render —
                    // hier reicht ein Notify.
                    cx.notify();
                }
                UiEvent::Session { session_id, event } => {
                    // StreamView-Agent: Ist die Stream-Ansicht aktiv, verbraucht
                    // sie das Event (Connected/PIN/Keyboard/Quit → Flow- und
                    // Overlay-Logik inkl. eigener Toasts/Navigation). Ohne
                    // Stream-Ansicht greift der Fallback darunter.
                    if !pages::stream::forward_session_event(session_id, event.clone(), cx) {
                        self.on_session_event(event, cx);
                    }
                }
                UiEvent::Controller(_) => {
                    // Controller-Badges rendern bei Bedarf neu.
                    cx.notify();
                }
                UiEvent::Psn(psn) => {
                    // PSN-Remote-Flow (backend::psn): Geräteliste in den
                    // PsnHandle übernehmen, Connecting-Stufen an die
                    // Stream-Ansicht weiterleiten (falls aktiv).
                    match psn {
                        crate::backend::PsnUiEvent::Devices(devices) => {
                            self.backend.psn().apply_devices(devices);
                        }
                        crate::backend::PsnUiEvent::DevicesFailed(_) => {
                            // Fehler kommt zusätzlich als Toast-Event.
                        }
                        crate::backend::PsnUiEvent::Connecting(state) => {
                            self.backend.psn().apply_connect_state(state);
                            pages::stream::forward_psn_connect_state(state, cx);
                        }
                    }
                    cx.notify();
                }
                UiEvent::Toast(toast) => {
                    self.push_toast(toast, cx);
                }
            }
        }
    }

    fn on_session_event(
        &mut self,
        event: chiaki_core::session::SessionEvent,
        cx: &mut Context<Self>,
    ) {
        use chiaki_core::session::SessionEvent as E;
        cx.notify();
        match event {
            E::Quit { reason, reason_str } => {
                if chiaki_core::session::quit_reason_is_error(reason) {
                    self.push_toast(
                        ToastData::new(crate::components::ToastKind::Danger, "Verbindung beendet")
                            .message(reason_str),
                        cx,
                    );
                }
            }
            E::LoginPinRequest { .. } => {
                self.push_toast(
                    ToastData::new(crate::components::ToastKind::Info, "PIN-Eingabe erwartet"),
                    cx,
                );
            }
            _ => {}
        }
    }
}

impl Focusable for AppShell {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.shell_focus.clone()
    }
}

impl Render for AppShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Backend-Events anwenden (Poll 1×/Frame — Spec-Architektur).
        self.apply_events(cx);

        // Transition weiterfahren (Frame-Treiben während 220 ms).
        let transition_active = self
            .transition
            .as_ref()
            .map(|t| !t.finished())
            .unwrap_or(false);
        if transition_active {
            window.request_animation_frame();
            cx.notify();
        }
        let progress = self.transition.as_ref().map(|t| t.progress()).unwrap_or(1.0);
        let slide_px = (1.0 - progress) * motion::SLIDE_DISTANCE_PX;

        // Beim ersten Frame den Shell-Fokus setzen (Key-Dispatch).
        let focus = self.shell_focus.clone();
        window.defer(cx, move |window, _cx| focus.focus(window));

        // NavRail (Spec §1.2: schmale Icon-Rail links) — im Stream entfällt
        // sie (StreamView-Agent): Video/HUD laufen fensterfüllend (Spec §2.4
        // „Connecting-Sequence im Vollbild“/C++-Fullscreen-Stream).
        let nav: gpui::AnyElement = if matches!(self.route, Route::Stream(_)) {
            div().into_any_element()
        } else {
            self.render_nav_rail(window, cx)
        };

        // Aktive Seite.
        let route = self.route.clone();
        let content: gpui::AnyElement = match route {
            Route::Home => pages::home::page(self, window, cx).into_any_element(),
            Route::Consoles => pages::consoles::page(self, window, cx).into_any_element(),
            Route::Settings => pages::settings::page(self, window, cx).into_any_element(),
            Route::Info => pages::info::page(self, window, cx).into_any_element(),
            Route::Stream(host) => pages::stream::page(self, host, window, cx).into_any_element(),
        };

        // Toasts (unten rechts, absolute Overlay-Ebene).
        let toasts: Vec<gpui::AnyElement> = self
            .toasts
            .clone()
            .into_iter()
            .map(|toast| {
                let id = toast.id;
                ToastView::new(toast)
                    .on_dismiss(cx.listener(move |shell, _ev, _window, cx| {
                        shell.dismiss_toast(id, cx);
                    }))
                    .into_any_element()
            })
            .collect();

        // Dialog-Overlay (oberster Dialog des Stacks).
        let shell_weak = cx.entity().downgrade();
        let shell_weak_close = cx.entity().downgrade();
        let modal = self.dialogs.last().map(|dialog| {
            let view = dialog.view();
            let interactive = dialog.buttons.iter().any(|b| b.closes || b.action.is_some());
            let mut layer = ModalLayer::new(view);
            if interactive {
                let shell_weak = shell_weak.clone();
                layer = layer.on_button(move |index, window, cx: &mut App| {
                    AppShell::invoke_dialog_button(&shell_weak, index, window, cx);
                });
            }
            layer
                .on_close(move |_window, cx: &mut App| {
                    let _ = shell_weak_close.update(cx, |shell, cx| shell.pop_dialog(cx));
                })
                .into_any_element()
        });

        // GPU-Stream-Modus: Shell-Hintergrund transparent, damit das Video-
        // Fenster unter der gpui-Surface sichtbar bleibt (Stream-Seite malt
        // ihren Bereich selbst; außerhalb des Streams bleibt theme::BG).
        let shell_bg = if cx
            .has_global::<crate::pages::stream::state::StreamUiState>()
            && cx.global::<crate::pages::stream::state::StreamUiState>().gpu_active()
        {
            gpui::transparent_black()
        } else {
            theme::BG
        };
        div()
            .id("shell")
            .size_full()
            .flex()
            .flex_row()
            .bg(shell_bg)
            .text_color(theme::TEXT_PRIMARY)
            .font_family(theme::FONT_FAMILY)
            .text_size(px(theme::SIZE_BODY))
            .key_context("Shell")
            .track_focus(&self.shell_focus)
            .on_key_down(cx.listener(Self::handle_key))
            .child(nav)
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .child(
                        div()
                            .size_full()
                            .opacity(progress)
                            .ml(px(slide_px))
                            .child(content),
                    ),
            )
            .child(
                div()
                    .absolute()
                    .bottom_4()
                    .right_4()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(toasts),
            )
            .children(modal)
    }
}

impl AppShell {
    fn render_nav_rail(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let items = Route::nav_items();
        let buttons: Vec<gpui::AnyElement> = items
            .into_iter()
            .enumerate()
            .map(|(i, (route, icon, label))| {
                let active = self.route == route;
                let focus = self.rail_focus[i].clone();
                IconButton::new(("rail", i), icon, label)
                    .active(active)
                    .focus_handle(focus)
                    .on_click(cx.listener(move |shell, _ev, _window, cx| {
                        shell.navigate(route.clone(), cx);
                    }))
                    .into_any_element()
            })
            .collect();

        div()
            .w(px(88.0))
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .gap_2()
            .pt(px(theme::SP_4))
            .bg(theme::SURFACE)
            .border_r_1()
            .border_color(theme::HAIRLINE)
            .child(
                div()
                    .text_size(px(18.0))
                    .font_weight(gpui::FontWeight(theme::WEIGHT_DISPLAY))
                    .text_color(theme::ACCENT)
                    .mb_3()
                    .child("CHIAKI"),
            )
            .children(buttons)
            .into_any_element()
    }
}

/// Toast-Auto-Timeout (gpui-Task; nach Ablauf Toast dismissen).
/// `Context::spawn` liefert die WeakEntity der Shell als erstes Argument.
fn spawn_toast_dismiss(id: ToastId, timeout: Duration, cx: &mut Context<AppShell>) {
    cx.spawn(async move |shell, cx| {
        cx.background_executor().timer(timeout).await;
        let _ = shell.update(cx, |shell, cx| shell.dismiss_toast(id, cx));
    })
    .detach();
}
