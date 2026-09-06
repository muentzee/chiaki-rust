//! Settings-Kategorie „PSN & Network“ (QML: `SettingsPsn.qml`): PSN-Konto-
//! Status, Login über den wry/WebView2-Flow (`crate::psn_login::
//! start_psn_login_for_settings` — Tokens + Account-ID landen in den
//! Settings, das Ergebnis kommt als Toast-UiEvent zurück), Token-Verwaltung
//! (Sign-out löscht alle vier PSN-Werte), Hole-Punching-Einstellungen.

use crate::app::AppShell;
use crate::theme;

use super::{action_row, info_row, slider_row, toggle_row, Section};

pub(crate) fn sections(
    shell: &mut AppShell,
    _needle: &str,
    _cx: &mut gpui::Context<AppShell>,
) -> Vec<Section> {
    let settings = shell.backend.settings().clone();
    let s = settings.lock().unwrap_or_else(|e| e.into_inner());

    let refresh = s.psn_refresh_token();
    let auth = s.psn_auth_token();
    let expiry = s.psn_auth_token_expiry();
    let account = s.psn_account_id();
    let port_guessing = s.port_guessing_enabled();
    let guess_count = s.port_guess_count();
    let socket_count = s.port_guess_socket_count();
    drop(s);

    // Wie QML: verbunden = alle vier Werte gesetzt.
    let connected = !refresh.is_empty()
        && !auth.is_empty()
        && !expiry.is_empty()
        && !account.is_empty();

    let mut account_section = Section::new("PSN Account");
    account_section.push(info_row_status(connected, expiry, account));
    if connected {
        account_section.push(info_row(
            "PSN remote play streams over the internet and wakes the console from rest mode via \
             PSN. Hole punching settings below affect this connection type.",
            "psn remote internet wake",
        ));
        account_section.push(action_row(
            "psn-refresh-devices",
            "Refresh PSN consoles",
            Some("Loads the consoles of your PSN account into the Consoles page"),
            "psn devices refresh remote",
            true,
            "Refresh",
            false,
            |shell, cx| {
                // C++ updatePsnHosts: Geräteliste in einem Thread laden;
                // Ergebnis landet als UiEvent::Psn(Devices) im Backend und
                // aktualisiert die PSN-Kacheln der Konsolen-Seite.
                let settings = shell.backend.settings().clone();
                let events = shell.backend.event_sender();
                shell.backend.psn().list_devices(settings, events, false);
                shell.push_toast(
                    crate::components::ToastData::new(
                        crate::components::ToastKind::Info,
                        "PSN-Konsolen werden geladen …",
                    ),
                    cx,
                );
            },
        ));
        account_section.push(action_row(
            "psn-clear-token",
            "Clear PSN token",
            Some("Signs out; PSN remote play will no longer work until you log in again"),
            "psn logout token clear",
            true,
            "Sign out",
            true,
            |shell, cx| {
                // Sign-out: alle vier PSN-Werte löschen (wie das C++ im
                // Logout-Pfad) und die Status-Row per Toast-Notify auffrischen.
                let settings = shell.backend.settings().clone();
                let result = settings.lock().unwrap_or_else(|e| e.into_inner()).update(|s| {
                    s.set_psn_refresh_token(String::new());
                    s.set_psn_auth_token(String::new());
                    s.set_psn_auth_token_expiry(String::new());
                    s.set_psn_account_id(String::new());
                });
                if let Err(err) = result {
                    tracing::warn!("PSN-Sign-out konnte nicht gespeichert werden: {err}");
                }
                shell.push_toast(
                    crate::components::ToastData::new(
                        crate::components::ToastKind::Info,
                        "PSN abgemeldet",
                    )
                    .message("PSN-Token gelöscht."),
                    cx,
                );
            },
        ));
    } else {
        account_section.push(action_row(
            "psn-login",
            "Log in to PSN",
            Some("Opens the PlayStation login in a browser window"),
            "psn login account sign",
            true,
            "Log in\u{2026}",
            false,
            |shell, _cx| {
                // wry/WebView2-Login-Flow (psn_login.rs); Tokens + Account-ID
                // landen in den Settings, das Ergebnis kommt als Toast-UiEvent
                // zurück und frischt die Status-Row auf.
                crate::psn_login::start_psn_login_for_settings(
                    shell.backend.settings().clone(),
                    shell.backend.event_sender(),
                );
            },
        ));
    }

    let mut holepunch = Section::new("Hole Punching (Remote Play)");
    holepunch.push(toggle_row(
        "psn-port-guessing",
        "Hole punching port guessing",
        Some("Force STUN port guessing \u{2014} helps with strict routers"),
        "stun nat port guessing",
        true,
        port_guessing,
    ));
    holepunch.push(slider_row(
        "psn-port-guess-count",
        "Port guess count",
        None,
        "port guesses nat",
        port_guessing,
        guess_count as f64,
        0.0,
        75.0,
        1.0,
        format!("{guess_count} guesses (default 75)"),
        |v, s| s.set_port_guess_count(v.round() as i64),
    ));
    holepunch.push(slider_row(
        "psn-port-guess-sockets",
        "Port guess socket count",
        None,
        "sockets nat firewall",
        port_guessing,
        socket_count as f64,
        0.0,
        500.0,
        1.0,
        format!("{socket_count} sockets (default 250)"),
        |v, s| s.set_port_guess_socket_count(v.round() as i64),
    ));

    vec![account_section, holepunch]
}

/// Status-Zeile: Punkt (success/warn) + Text + Ablaufdatum/Account-ID.
fn info_row_status(connected: bool, expiry: String, account: String) -> super::SRow {
    use gpui::{div, px, IntoElement as _, ParentElement as _, Styled as _};

    let dot = div()
        .size(px(8.0))
        .rounded_full()
        .bg(if connected { theme::SUCCESS } else { theme::WARN });
    let text = div()
        .text_size(px(theme::SIZE_BODY))
        .text_color(theme::TEXT_PRIMARY)
        .child(if connected {
            "PSN account connected".to_string()
        } else {
            "No PSN account connected".to_string()
        });
    let detail = if connected {
        let expiry_display = if expiry.is_empty() { "\u{2013}".to_string() } else { expiry };
        let account_display = if account.is_empty() { "\u{2013}".to_string() } else { account };
        format!("Token expires: {expiry_display} \u{00B7} Account ID: {account_display}")
    } else {
        "Log in to enable PSN remote play and remote wake-up.".to_string()
    };
    let detail = div()
        .text_size(px(theme::SIZE_CAPTION))
        .text_color(theme::TEXT_SECONDARY)
        .child(detail);

    super::custom_row(
        "psn account status connected expiry",
        None,
        "psn account status token expiry",
        true,
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(theme::SP_4))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(dot)
                    .child(text)
                    .child(detail),
            )
            .into_any_element(),
    )
}
