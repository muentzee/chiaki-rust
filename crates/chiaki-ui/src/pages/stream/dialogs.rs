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
///
/// Bekommt die Shell direkt (`&mut AppShell`): Alle Aufrufer stehen bereits
/// in einem laufenden `AppShell`-Update (cx.listener / handle_escape) — ein
/// `shell.update(cx, …)` hier wäre ein verbotener Re-Entry und bricht die
/// App mit „cannot update AppShell while it is already being updated“ ab.
pub(crate) fn push_disconnect_confirm(shell: &mut AppShell, cx: &mut Context<AppShell>) {
    let backend = {
        let state = cx.global::<StreamUiState>();
        state.backend.clone()
    };
    // disconnect_action: bei "sleep" (AlwaysSleep) bedeutet Trennen direkt
    // "Ruhemodus + Trennen" — ohne nochmaliges Nachfragen (C++-Pfad
    // DisconnectAction::AlwaysSleep → GoToBed + Stop).
    let always_sleep = {
        let settings = backend.settings().lock().unwrap_or_else(|e| e.into_inner());
        settings.disconnect_action() == chiaki_settings::settings::DisconnectAction::AlwaysSleep
    };
    let shell_quit = cx.entity().downgrade();

    let dialog = Dialog::new(
        "stream-disconnect",
        "End streaming?",
        "The connection to the console will be disconnected.",
    )
    .button(
        DialogButton::new("Disconnect", ButtonVariant::Danger).action(move |_window, cx: &mut App| {
            let _ = shell_quit.update(cx, |shell, cx| {
                if cx.has_global::<StreamUiState>() {
                    let state = cx.global_mut::<StreamUiState>();
                    if always_sleep {
                        state.goto_bed();
                    }
                    state.disconnect_now();
                }
                shell.navigate(crate::app::Route::Home, cx);
            });
        }),
    )
    .button(
        DialogButton::new("Rest mode", ButtonVariant::Primary).action(move |_window, cx: &mut App| {
            // Konsole in den Ruhemodus fahren (C++: chiaki_session_goto_bed);
            // das Quit-Event (RemoteShutdown) bringt die UI zurück nach Home.
            // auto_bed_sent markieren — der Quit-Handler (disconnect_action
            // "sleep") darf dann kein zweites goto_bed mehr schicken.
            if cx.has_global::<StreamUiState>() {
                let state = cx.global_mut::<StreamUiState>();
                state.goto_bed();
                state.mark_auto_bed_sent();
            }
        }),
    )
    .button(DialogButton::new("Cancel", ButtonVariant::Ghost));

    shell.push_dialog(dialog, cx);
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
                        .child("Enter login PIN"),
                )
                .child(
                    div()
                        .text_size(px(theme::SIZE_BODY))
                        .text_color(theme::TEXT_SECONDARY)
                        .child(
                            "Enter the 8-digit PIN shown on the console \
                             (type the digits, it is submitted automatically).",
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
                            "Incorrect PIN — please enter it again (Esc = cancel)".to_string()
                        } else {
                            "Esc = cancel".to_string()
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
                        .child("Console keyboard"),
                )
                .child(
                    div()
                        .text_size(px(theme::SIZE_BODY))
                        .text_color(theme::TEXT_SECONDARY)
                        .child("Enter text and send it to the console."),
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
                            Button::new("keyboard-cancel", "Cancel")
                                .variant(ButtonVariant::Ghost)
                                .on_click(cancel),
                        )
                        .child(
                            Button::new("keyboard-send", "Send")
                                .variant(ButtonVariant::Primary)
                                .on_click(send),
                        ),
                ),
            ),
        )
        .into_any_element()
}
