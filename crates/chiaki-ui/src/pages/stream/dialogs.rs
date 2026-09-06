//! Stream-Dialoge: PIN-Overlay (LoginPinRequest, 8 Boxen mit Auto-Submit,
//! PS-Look), Konsole-Tastatur-Overlay (KeyboardText → TextField mit
//! Senden/Abbrechen → keyboard_set_text/accept/reject) und der
//! Trennen-Confirm (Trennen / Ruhemodus / Abbrechen — wie im C++
//! Disconnect-Dialog).

use gpui::{div, px, App, Context, IntoElement, ParentElement as _, Styled, Window};

use crate::app::AppShell;
use crate::components::{
    Button, ButtonVariant, Dialog, DialogButton, GlassPanel, TextField,
};
use crate::theme;

use super::state::StreamUiState;

// ---------------------------------------------------------------------------
// Trennen-Confirm (über den Shell-Dialog-Stack)
// ---------------------------------------------------------------------------

/// Bestätigungsdialog „Stream beenden?“ mit Trennen / Ruhemodus / Abbrechen
/// (Port des C++-Quit-Dialogs: quit → Trennen, goto_bed → Ruhemodus).
pub(crate) fn push_disconnect_confirm(cx: &mut Context<AppShell>) {
    let (shell, backend) = {
        let state = cx.global::<StreamUiState>();
        (state.shell.clone(), state.backend.clone())
    };
    let shell_quit = shell.clone();
    let shell_bed = shell.clone();
    let backend_bed = backend.clone();

    let dialog = Dialog::new(
        "stream-disconnect",
        "Stream beenden?",
        "Die Verbindung zur Konsole wird getrennt.",
    )
    .button(
        DialogButton::new("Trennen", ButtonVariant::Danger).action(move |_window, cx: &mut App| {
            let _ = shell_quit.update(cx, |shell, cx| {
                if cx.has_global::<StreamUiState>() {
                    cx.global_mut::<StreamUiState>().disconnect_now();
                }
                shell.navigate(crate::app::Route::Home, cx);
            });
        }),
    )
    .button(
        DialogButton::new("Ruhemodus", ButtonVariant::Primary).action(move |_window, cx: &mut App| {
            // Konsole in den Ruhemodus fahren (C++: chiaki_session_goto_bed);
            // das Quit-Event (RemoteShutdown) bringt die UI zurück nach Home.
            if let Some(session) = backend_bed.sessions().active() {
                session.goto_bed();
            }
            let _ = shell_bed.update(cx, |_shell, _cx| {});
        }),
    )
    .button(DialogButton::new("Abbrechen", ButtonVariant::Ghost));

    let _ = shell.update(cx, |shell, cx| shell.push_dialog(dialog, cx));
}

// ---------------------------------------------------------------------------
// PIN-Overlay (8 Boxen, Auto-Submit)
// ---------------------------------------------------------------------------

/// PIN-Overlay: 8 Boxen im PS-Login-Look; Ziffern kommen über den
/// Stream-Root-Key-Handler (Auto-Submit bei 8 Ziffern), Esc bricht ab.
pub fn pin_view(snap: &super::state::StreamSnapshot) -> gpui::AnyElement {
    let digits: Vec<char> = snap.pin_digits.chars().collect();
    let boxes: Vec<gpui::AnyElement> = (0..8)
        .map(|i| {
            let filled = digits.get(i).copied();
            div()
                .flex()
                .items_center()
                .justify_center()
                .w(px(40.0))
                .h(px(52.0))
                .rounded(px(theme::RADIUS_MD))
                .bg(theme::SURFACE2)
                .border_1()
                .border_color(if filled.is_some() {
                    theme::ACCENT
                } else {
                    theme::OUTLINE
                })
                .text_size(px(theme::SIZE_TITLE))
                .font_weight(gpui::FontWeight(theme::WEIGHT_TITLE))
                .text_color(theme::TEXT_PRIMARY)
                .child(filled.map(|c| c.to_string()).unwrap_or_default())
                .into_any_element()
        })
        .collect();

    div()
        .absolute()
        .inset_0()
        .bg(theme::BACKDROP)
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(theme::SP_4))
        .child(
            GlassPanel::new()
                .child(
                    div()
                        .text_size(px(theme::SIZE_TITLE))
                        .font_weight(gpui::FontWeight(theme::WEIGHT_TITLE))
                        .text_color(theme::TEXT_PRIMARY)
                        .child("Login-PIN eingeben"),
                )
                .child(
                    div()
                        .text_size(px(theme::SIZE_BODY))
                        .text_color(theme::TEXT_SECONDARY)
                        .child(
                            "Gib die 8-stellige PIN ein, die auf der Konsole angezeigt wird \
                             (Ziffern tippen, automatisch übernehmen).",
                        ),
                )
                .child(div().flex().gap_2().children(boxes))
                .child(
                    div()
                        .text_size(px(theme::SIZE_CAPTION))
                        .text_color(if snap.pin_incorrect {
                            theme::DANGER
                        } else {
                            theme::TEXT_DISABLED
                        })
                        .child(if snap.pin_incorrect {
                            "PIN war falsch — bitte erneut eingeben (Esc = Abbrechen)".to_string()
                        } else {
                            "Esc = Abbrechen".to_string()
                        }),
                ),
        )
        .into_any_element()
}

// ---------------------------------------------------------------------------
// Konsole-Tastatur-Overlay
// ---------------------------------------------------------------------------

/// Konsolen-Tastatur: TextField (fokussiert gehalten vom State-Tick) +
/// Senden (keyboard_set_text + accept) / Abbrechen (keyboard_reject).
pub fn keyboard_view(
    text: &str,
    focus: gpui::FocusHandle,
    send: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    cancel: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::AnyElement {
    div()
        .absolute()
        .inset_0()
        .bg(theme::BACKDROP)
        .flex()
        .items_center()
        .justify_center()
        .child(
            div().w(px(560.0)).child(
                GlassPanel::new()
                .child(
                    div()
                        .text_size(px(theme::SIZE_TITLE))
                        .font_weight(gpui::FontWeight(theme::WEIGHT_TITLE))
                        .text_color(theme::TEXT_PRIMARY)
                        .child("Tastatur der Konsole"),
                )
                .child(
                    div()
                        .text_size(px(theme::SIZE_BODY))
                        .text_color(theme::TEXT_SECONDARY)
                        .child("Text eingeben und an die Konsole senden."),
                )
                .child(
                    TextField::new("console-keyboard")
                        .value(text.to_string())
                        .placeholder("Text…")
                        .width(460.0)
                        .focus_handle(focus)
                        .on_change(|value: &gpui::SharedString, _window, cx: &mut App| {
                            let value = value.to_string();
                            if cx.has_global::<StreamUiState>() {
                                cx.global_mut::<StreamUiState>().keyboard_set_text(value);
                            }
                        }),
                )
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .justify_end()
                        .child(
                            Button::new("keyboard-cancel", "Abbrechen")
                                .variant(ButtonVariant::Ghost)
                                .on_click(cancel),
                        )
                        .child(
                            Button::new("keyboard-send", "Senden")
                                .variant(ButtonVariant::Primary)
                                .on_click(send),
                        ),
                ),
            ),
        )
        .into_any_element()
}
