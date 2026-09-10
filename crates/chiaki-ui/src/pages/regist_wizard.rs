//! Registrierungs-Wizard (ui-v2-spec §2.2): 3 Schritte — Art der Konsole →
//! PIN / PSN → Ergebnis (Live-Log), Fortschrittsanzeige oben.
//!
//! Der Flow ist 1:1 an `qml2/components/RegistWizard.qml` +
//! `QmlBackend::registerHost` (qmlbackend.cpp) portiert:
//! * Target-Presets: PS4 < 7.0 (800) / 7.0–8.0 (900) / ≥ 8.0 (1000) / PS5
//!   (1000100) — PS4 < 7.0 nutzt die PSN-Online-ID, alle anderen die
//!   8-Byte-PSN-Account-ID (Base64).
//! * Account-ID wird aus `settings.psn_account_id` (Base64 der 8 Bytes)
//!   vorausgefüllt (C++: `accountIdField.value = Chiaki.settings.psnAccountId`).
//! * PIN: exakt 8 Ziffern; Konsolen-PIN: leer oder 4 Ziffern.
//! * „Registrieren“ startet `backend.sessions().regist_host()` (chiaki_core::
//!   regist) und springt zum Ergebnis-Schritt mit Live-Log; Erfolg/Fehler
//!   kommen über das RegistHandle + `UiEvent::Regist` (Notify in app.rs).
//!
//! [`WizardState`] ist zugleich der Seitenzustand-Container (CONTRACT-UI §5:
//! „Neue Shell-Felder für Seitenzustand sind erlaubt“) — siehe Report.

use gpui::{
    div, prelude::FluentBuilder as _, px, App, Context, FocusHandle, FontWeight, IntoElement,
    InteractiveElement as _, ParentElement as _, SharedString, StatefulInteractiveElement as _,
    Styled, Window,
};

use crate::app::AppShell;
use crate::backend::RegistRequest;
use crate::components::{Button, ButtonVariant, Card, SectionLabel, StatusBadge, StatusKind};
use crate::icons;
use crate::pages::{page_header, page_scaffold};
use crate::theme;

/// Die 3 Wizard-Schritte (Reihenfolge bindend).
pub const WIZARD_STEPS: [&str; 3] = ["Console type", "PIN / PSN", "Result"];

/// Target-Presets des Wizards (Werte = ChiakiTarget, wie im C++-QML).
const TARGET_PRESETS: [(chiaki_settings::hosts::Target, &str, &str); 4] = [
    (
        chiaki_settings::hosts::Target::Ps4Eight,
        "PS4 firmware < 7.0",
        "Registration with PSN online ID",
    ),
    (
        chiaki_settings::hosts::Target::Ps4Nine,
        "PS4 firmware 7.0 – 8.0",
        "Registration with PSN account ID (Base64)",
    ),
    (
        chiaki_settings::hosts::Target::Ps4Ten,
        "PS4 firmware ≥ 8.0",
        "Registration with PSN account ID (Base64)",
    ),
    (
        chiaki_settings::hosts::Target::Ps5One,
        "PS5",
        "Registration with PSN account ID (Base64)",
    ),
];

/// Seitenzustand: Wizard + Konsolen-Seite (Filter, Manueller-Host-Formular)
/// + wiederverwendbare Kachel-Fokus-Handles. Liegt als EIN neues pub-Feld
/// auf der AppShell (CONTRACT-UI §5), damit die Seiten keine globalen
/// Stati brauchen; Fokus-Handles werden nie pro Frame neu erzeugt.
pub struct WizardState {
    /// Aktueller Schritt (0..=2).
    pub step: usize,
    /// Gewählter Konsolen-Typ (Step 1).
    pub target: chiaki_settings::hosts::Target,
    /// Ziel-Host (IP oder 255.255.255.255 für Broadcast).
    pub host: String,
    /// PSN-Online-ID (PS4 < 7.0) bzw. Account-ID Base64 (PS4 ≥ 7.0 / PS5).
    pub account_id: String,
    /// Remote-Play-PIN (8 Ziffern).
    pub pin: String,
    /// Optionaler Konsolen-Login-PIN (4 Ziffern).
    pub console_pin: String,
    /// Laufender/beendeter Regist-Flow (Step 3).
    pub regist: Option<crate::backend::RegistHandle>,

    // Fokus-Handles der Eingabefelder (nie pro Frame neu erzeugen).
    pub host_focus: FocusHandle,
    pub account_focus: FocusHandle,
    pub pin_focus: FocusHandle,
    pub console_pin_focus: FocusHandle,

    /// Konsolen-Filter der Konsolen-Seite (Alle/Bereit/Offline/PSN).
    pub console_filter: crate::pages::consoles::ConsoleFilter,
    /// Manueller-Host-Formular (Konsolen-Seite).
    pub manual_host: String,
    pub manual_host_focus: FocusHandle,
    /// Von der Home-Seite angefordert: Manueller-Host-Feld fokussieren.
    pub manual_focus_pending: bool,
    /// Kachel-Fokus-Handles (Home + Konsolen; stabil über Frames/Seiten).
    pub tile_focus: Vec<FocusHandle>,
    /// Weitere Fokus-Handles für Seiten-Buttons (Hero-„Verbinden“, Filter-
    /// Chips, Seiten-Aktionen) — ebenfalls nie pro Frame neu erzeugt.
    pub ui_focus: Vec<FocusHandle>,
}

impl WizardState {
    pub(crate) fn new(cx: &mut Context<AppShell>) -> Self {
        Self {
            step: 0,
            target: chiaki_settings::hosts::Target::Ps5One,
            host: "255.255.255.255".into(),
            account_id: String::new(),
            pin: String::new(),
            console_pin: String::new(),
            regist: None,
            host_focus: cx.focus_handle(),
            account_focus: cx.focus_handle(),
            pin_focus: cx.focus_handle(),
            console_pin_focus: cx.focus_handle(),
            console_filter: crate::pages::consoles::ConsoleFilter::All,
            manual_host: String::new(),
            manual_host_focus: cx.focus_handle(),
            manual_focus_pending: false,
            tile_focus: Vec::new(),
            ui_focus: Vec::new(),
        }
    }

    /// Wizard zurücksetzen (Beim Öffnen): PSN-Account-ID aus den Settings
    /// vorausfüllen (C++: `accountIdField.value = Chiaki.settings.psnAccountId`).
    pub(crate) fn reset(&mut self, account_id_b64: &str) {
        self.step = 0;
        self.target = chiaki_settings::hosts::Target::Ps5One;
        self.host = "255.255.255.255".into();
        self.account_id = account_id_b64.to_string();
        self.pin.clear();
        self.console_pin.clear();
        self.regist = None;
    }

    /// Kachel-Fokus-Handles auf n Konsolen synchronisieren (bestehende
    /// Handles bleiben erhalten — Fokus-Springen beim Re-Render vermeiden).
    pub(crate) fn sync_tile_focus(
        &mut self,
        n: usize,
        cx: &mut Context<AppShell>,
    ) -> Vec<FocusHandle> {
        self.tile_focus.resize_with(n, || cx.focus_handle());
        self.tile_focus.clone()
    }

    /// Fokus-Handle-Pool für Seiten-Buttons (Hero-„Verbinden“, Filter-Chips,
    /// Seiten-Aktionen) synchronisieren.
    pub(crate) fn sync_ui_focus(
        &mut self,
        n: usize,
        cx: &mut Context<AppShell>,
    ) -> Vec<FocusHandle> {
        self.ui_focus.resize_with(n, || cx.focus_handle());
        self.ui_focus.clone()
    }
}

/// Wizard öffnen (von Home/Konsolen/Info): Zustand zurücksetzen, Account-ID
/// vorausfüllen, Overlay aktivieren.
pub fn open(shell: &mut AppShell, cx: &mut Context<AppShell>) {
    let account = shell
        .backend
        .settings()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .psn_account_id();
    shell.regist_wizard.reset(&account);
    shell.show_regist_wizard = true;
    cx.notify();
}

pub fn page(
    shell: &mut AppShell,
    window: &mut Window,
    cx: &mut Context<AppShell>,
) -> impl IntoElement {
    let state = &shell.regist_wizard;
    let step = state.step;
    let running = state.regist.as_ref().map(|r| r.snapshot().running).unwrap_or(false);

    let mut children: Vec<gpui::AnyElement> = Vec::new();
    children.push(page_header(shell, "Register console", None, window, cx));

    // StepBar (Fortschrittsanzeige, Spec §2.2): erledigt = Check,
    // aktiv = Akzent-Kontur, kommend = dezent.
    children.push(
        div()
            .flex()
            .flex_row()
            .gap_2()
            .children(WIZARD_STEPS.iter().enumerate().map(|(i, step_label)| {
                let done = i < step;
                let active = i == step;
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px(px(theme::SP_3))
                    .py(px(theme::SP_2))
                    .rounded(px(theme::RADIUS_SM))
                    .bg(if active { theme::SURFACE2 } else { theme::SURFACE })
                    .border_1()
                    .border_color(if active { theme::ACCENT } else { theme::HAIRLINE })
                    .child(if done {
                        icons::icon(icons::paths::CHECK, 14.0, theme::SUCCESS).into_any_element()
                    } else {
                        div()
                            .text_size(px(theme::SIZE_CAPTION))
                            .font_weight(FontWeight(theme::WEIGHT_CAPTION))
                            .text_color(if active { theme::ACCENT } else { theme::TEXT_DISABLED })
                            .child(format!("{}", i + 1))
                            .into_any_element()
                    })
                    .child(
                        div()
                            .text_size(px(theme::SIZE_BODY))
                            .font_weight(FontWeight(if active {
                                theme::WEIGHT_HEADLINE
                            } else {
                                theme::WEIGHT_BODY
                            }))
                            .text_color(if active || done {
                                theme::TEXT_PRIMARY
                            } else {
                                theme::TEXT_SECONDARY
                            })
                            .child(step_label.to_string()),
                    )
            }))
            .into_any_element(),
    );

    children.push(match step {
        0 => step_target(shell, cx).into_any_element(),
        1 => step_auth(shell, cx).into_any_element(),
        _ => step_result(shell, window, cx).into_any_element(),
    });

    // Fußleiste: Abbrechen/Zurück links, Weiter/Aktionen rechts.
    let back_label = if step == 0 { "Cancel" } else { "Back" };
    let can_continue = step == 0 || auth_valid(shell);
    let next = match step {
        0 => Button::new("wizard-next", "Next")
            .variant(ButtonVariant::Primary)
            .on_click(cx.listener(|shell, _ev, window, cx| {
                window.blur();
                shell.regist_wizard.step = 1;
                cx.notify();
            })),
        1 => Button::new("wizard-register", "Register")
            .variant(ButtonVariant::Primary)
            .disabled(!can_continue || running)
            .on_click(cx.listener(|shell, _ev, _window, cx| {
                start_regist(shell, cx);
            })),
        _ => {
            let label = if running { "Running …" } else { "Done" };
            Button::new("wizard-close", label)
                .variant(ButtonVariant::Primary)
                .on_click(cx.listener(|shell, _ev, _window, cx| {
                    shell.show_regist_wizard = false;
                    shell.regist_wizard.regist = None;
                    cx.notify();
                }))
        }
    };

    children.push(
        div()
            .flex()
            .justify_between()
            .child(
                Button::new("wizard-back", back_label).on_click(cx.listener(
                    move |shell, _ev, window, cx| {
                        window.blur();
                        if shell.regist_wizard.step == 0 {
                            // Abbrechen: laufenden Flow stoppen nicht (Thread
                            // endet von selbst), Overlay nur schließen.
                            shell.show_regist_wizard = false;
                            shell.regist_wizard.regist = None;
                        } else {
                            shell.regist_wizard.step -= 1;
                        }
                        cx.notify();
                    },
                )),
            )
            .child(next)
            .into_any_element(),
    );

    page_scaffold(children)
}
// ---------------------------------------------------------------------------
// Schritt 1: Art der Konsole
// ---------------------------------------------------------------------------

fn step_target(shell: &mut AppShell, cx: &mut Context<AppShell>) -> Card {
    let selected = shell.regist_wizard.target;
    let host = shell.regist_wizard.host.clone();
    let host_focus = shell.regist_wizard.host_focus.clone();
    let weak = cx.entity().downgrade();

    let mut card = Card::new("wizard-target");
    card = card.child(SectionLabel::new("Console type"));
    for (target, label, hint) in TARGET_PRESETS {
        let is_selected = selected == target;
        let (bg, border) = if is_selected {
            (
                gpui::Hsla { a: 0.14, ..theme::ACCENT },
                gpui::Hsla { a: 0.6, ..theme::ACCENT },
            )
        } else {
            (theme::SURFACE2, theme::HAIRLINE)
        };
        card = card.child(
            div()
                .id(("wizard-target", target.as_i32() as usize))
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .px(px(theme::SP_3))
                .py(px(theme::SP_2))
                .rounded(px(theme::RADIUS_SM))
                .bg(bg)
                .border_1()
                .border_color(border)
                .cursor_pointer()
                .hover(|s| s.border_color(theme::OUTLINE))
                .on_click(cx.listener(move |shell, _ev, _window, cx| {
                    shell.regist_wizard.target = target;
                    cx.notify();
                }))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_size(px(theme::SIZE_HEADLINE))
                                .font_weight(FontWeight(theme::WEIGHT_HEADLINE))
                                .text_color(theme::TEXT_PRIMARY)
                                .child(label.to_string()),
                        )
                        .child(
                            div()
                                .text_size(px(theme::SIZE_CAPTION))
                                .text_color(theme::TEXT_SECONDARY)
                                .child(hint.to_string()),
                        ),
                )
                .when(is_selected, |row| {
                    row.child(icons::icon(icons::paths::CHECK, 16.0, theme::ACCENT))
                }),
        );
    }

    card.child(
        div()
            .flex()
            .flex_col()
            .gap_2()
            .mt_1()
            .child(SectionLabel::new("Host address"))
            .child(
                div()
                    .text_size(px(theme::SIZE_CAPTION))
                    .text_color(theme::TEXT_SECONDARY)
                    .child(
                        "Console IP — or 255.255.255.255 to broadcast on the local network.",
                    ),
            )
            .child(
                crate::components::TextField::new("wizard-host")
                    .value(host)
                    .placeholder("255.255.255.255")
                    .width(340.0)
                    .focus_handle(host_focus)
                    .on_change(wizard_text_change(
                        &weak,
                        |shell, value| shell.regist_wizard.host = value,
                    )),
            ),
    )
}

// ---------------------------------------------------------------------------
// Schritt 2: PIN / PSN
// ---------------------------------------------------------------------------

fn step_auth(shell: &mut AppShell, cx: &mut Context<AppShell>) -> Card {
    let state = &shell.regist_wizard;
    let ps4_pre7 = state.target == chiaki_settings::hosts::Target::Ps4Eight;
    let account = state.account_id.clone();
    let pin = state.pin.clone();
    let console_pin = state.console_pin.clone();
    let account_focus = state.account_focus.clone();
    let pin_focus = state.pin_focus.clone();
    let console_pin_focus = state.console_pin_focus.clone();
    let weak = cx.entity().downgrade();

    let account_label = if ps4_pre7 {
        "PSN online ID"
    } else {
        "PSN account ID (Base64)"
    };
    let account_hint = if ps4_pre7 {
        "Username (case-sensitive) — only for PS4 < 7.0."
    } else {
        "Base64 account ID — pre-filled from the PSN sign-in."
    };
    let account_valid = if ps4_pre7 {
        !account.trim().is_empty()
    } else {
        chiaki_settings::psn::account_id_to_bytes(account.trim()).is_some()
    };

    let mut card = Card::new("wizard-auth");
    card = card.child(SectionLabel::new("Sign-in"));
    card = card.child(auth_row(
        "wizard-account-row",
        account_label,
        account_hint,
        crate::components::TextField::new("wizard-account")
            .value(account.clone())
            .placeholder(if ps4_pre7 { "PSN username" } else { "e.g. eVr/5uFEAHE=" })
            .width(340.0)
            .focus_handle(account_focus)
            .on_change(wizard_text_change(&weak, |shell, value| {
                shell.regist_wizard.account_id = value
            })),
        (!account.trim().is_empty() && !account_valid)
            .then(|| "Account ID must decode to exactly 8 Base64-encoded bytes.")
            .map(text_hint),
    ));

    card = card.child(auth_row(
        "wizard-pin-row",
        "Remote play PIN",
        "Shown on the console: Settings → System → Remote Play → Link device.",
        crate::components::TextField::new("wizard-pin")
            .value(pin.clone())
            .placeholder("12345678")
            .width(180.0)
            .focus_handle(pin_focus)
            .on_change(wizard_text_change(&weak, |shell, value| {
                // Nur Ziffern, max. 8 (PIN-Format der Konsole).
                let filtered: String =
                    value.chars().filter(|c| c.is_ascii_digit()).take(8).collect();
                shell.regist_wizard.pin = filtered;
            })),
        (!pin.is_empty() && pin.len() != 8).then(|| "The PIN is always 8 digits.").map(text_hint),
    ));

    card.child(auth_row(
        "wizard-cpin-row",
        "Console login PIN (optional)",
        "4-digit login PIN of the user account — stored for future streams.",
        crate::components::TextField::new("wizard-console-pin")
            .value(console_pin.clone())
            .placeholder("0000")
            .width(140.0)
            .focus_handle(console_pin_focus)
            .on_change(wizard_text_change(&weak, |shell, value| {
                let filtered: String =
                    value.chars().filter(|c| c.is_ascii_digit()).take(4).collect();
                shell.regist_wizard.console_pin = filtered;
            })),
        (!console_pin.is_empty() && console_pin.len() != 4)
            .then(|| "The login PIN is 4 digits (or empty).")
            .map(text_hint),
    ))
}

/// Label links, Control rechts, Erklärzeile darunter (SettingsRow-Anordnung).
fn auth_row(
    id: &'static str,
    label: &str,
    hint: &str,
    field: crate::components::TextField,
    error_hint: Option<gpui::AnyElement>,
) -> gpui::AnyElement {
    div()
        .id(id)
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .text_size(px(theme::SIZE_BODY))
                        .text_color(theme::TEXT_PRIMARY)
                        .child(label.to_string()),
                )
                .child(field),
        )
        .child(
            div()
                .text_size(px(theme::SIZE_CAPTION))
                .text_color(theme::TEXT_SECONDARY)
                .child(hint.to_string()),
        )
        .children(error_hint)
        .into_any_element()
}

/// Fehler-/Warnhinweis-Text (dezent rot).
fn text_hint(message: &str) -> gpui::AnyElement {
    div()
        .text_size(px(theme::SIZE_CAPTION))
        .text_color(theme::DANGER)
        .child(message.to_string())
        .into_any_element()
}

/// Sind die Step-2-Eingaben gültig (Registrieren freischalten)?
fn auth_valid(shell: &AppShell) -> bool {
    let state = &shell.regist_wizard;
    let pin_ok = state.pin.len() == 8 && state.pin.chars().all(|c| c.is_ascii_digit());
    let cpin_ok = state.console_pin.is_empty()
        || (state.console_pin.len() == 4 && state.console_pin.chars().all(|c| c.is_ascii_digit()));
    let account_ok = if state.target == chiaki_settings::hosts::Target::Ps4Eight {
        !state.account_id.trim().is_empty()
    } else {
        chiaki_settings::psn::account_id_to_bytes(state.account_id.trim()).is_some()
    };
    pin_ok && cpin_ok && account_ok
}

/// „Registrieren“: RegistRequest bauen (1:1 wie QmlBackend::registerHost —
/// Online-ID nur bei PS4 < 7.0, sonst Account-ID; Broadcast bei
/// 255.255.255.255) und den Flow im Backend starten.
fn start_regist(shell: &mut AppShell, cx: &mut Context<AppShell>) {
    let state = &shell.regist_wizard;
    let host = state.host.trim().to_string();
    if host.is_empty() {
        return;
    }
    let account = state.account_id.trim().to_string();
    let pin: u32 = state.pin.trim().parse().unwrap_or(0);
    let console_pin: u32 = state.console_pin.trim().parse().unwrap_or(0);
    let ps4_pre7 = state.target == chiaki_settings::hosts::Target::Ps4Eight;
    let (psn_online_id, psn_account_id) = if ps4_pre7 {
        (Some(account), None)
    } else {
        (
            None,
            chiaki_settings::psn::account_id_to_bytes(&state.account_id.trim().to_string()),
        )
    };
    let request = RegistRequest {
        host: host.clone(),
        target: state.target,
        broadcast: host == "255.255.255.255",
        psn_online_id,
        psn_account_id,
        pin,
        console_pin,
    };
    tracing::info!("Registrierungs-Wizard: starte Regist gegen {host}");
    shell.regist_wizard.regist = Some(shell.backend.sessions().regist_host(request));
    shell.regist_wizard.step = 2;
    cx.notify();
}

// ---------------------------------------------------------------------------
// Schritt 3: Ergebnis (Live-Log)
// ---------------------------------------------------------------------------

fn step_result(shell: &mut AppShell, _window: &mut Window, _cx: &mut Context<AppShell>) -> Card {
    let snapshot = shell
        .regist_wizard
        .regist
        .as_ref()
        .map(|r| r.snapshot())
        .unwrap_or_default();

    let mut card = Card::new("wizard-result");
    card = card.child(SectionLabel::new("Operation log"));

    // Live-Log (scrollbar; neue Zeilen kommen über UiEvent::Regist-Notifies).
    let mut log = div()
        .id("wizard-log")
        .flex()
        .flex_col()
        .gap_1()
        .overflow_y_scroll()
        .max_h(px(280.0))
        .p(px(theme::SP_2))
        .rounded(px(theme::RADIUS_SM))
        .bg(theme::SURFACE2)
        .border_1()
        .border_color(theme::HAIRLINE);
    if snapshot.lines.is_empty() {
        log = log.child(
            div()
                .text_size(px(theme::SIZE_CAPTION))
                .text_color(theme::TEXT_DISABLED)
                .child("Waiting for the console …"),
        );
    }
    for (i, line) in snapshot.lines.iter().enumerate() {
        log = log.child(
            div()
                .id(("wizard-log-line", i))
                .text_size(px(theme::SIZE_CAPTION))
                .text_color(theme::TEXT_SECONDARY)
                .child(line.clone()),
        );
    }
    card = card.child(log);

    // Statuszeile: läuft / erfolgreich (mit Host-Namen) / fehlgeschlagen.
    card.child(match (&snapshot.result, snapshot.running) {
        (None, true) => status_row(StatusKind::Ready, "Registration running …", theme::TEXT_PRIMARY),
        (Some(Ok(nickname)), _) => {
            // Hinweis: Der Erfolg kommt zusätzlich als Backend-Toast
            // (UiEvent::Toast) — hier die permanente Anzeige im Wizard.
            status_row(
                StatusKind::Ready,
                &format!("Console \"{nickname}\" registered"),
                theme::SUCCESS,
            )
        }
        (Some(Err(reason)), _) => status_row(StatusKind::Unregistered, reason, theme::DANGER),
        (None, false) => status_row(
            StatusKind::Offline,
            "No registration in progress — please go back and start again.",
            theme::TEXT_SECONDARY,
        ),
    })
}

fn status_row(status: StatusKind, text: &str, color: gpui::Hsla) -> gpui::AnyElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(StatusBadge::new(status).label(""))
        .child(
            div()
                .text_size(px(theme::SIZE_HEADLINE))
                .font_weight(FontWeight(theme::WEIGHT_HEADLINE))
                .text_color(color)
                .child(text.to_string()),
        )
        .into_any_element()
}

// ---------------------------------------------------------------------------
// Hilfs-Funktionen
// ---------------------------------------------------------------------------

/// on_change-Brücke für Wizard-Textfelder: Das TextField-Callback bekommt
/// kein AppShell — über die WeakEntity zurück zur Shell (Doku CONTRACT-UI §2).
fn wizard_text_change(
    weak: &gpui::WeakEntity<AppShell>,
    f: impl Fn(&mut AppShell, String) + 'static,
) -> Box<dyn Fn(&SharedString, &mut Window, &mut App) + 'static> {
    let weak = weak.clone();
    Box::new(move |value, _window, cx| {
        let value = value.to_string();
        let _ = weak.update(cx, |shell, _cx| f(shell, value));
    })
}
