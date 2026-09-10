//! Info / Erste Schritte (ui-v2-spec §2.5): 3 Onboarding-Karten —
//! Willkommen & Portable-Modus (mit Daten-Pfad), PSN verbinden, Konsole
//! registrieren — plus About-Zeile (Version, Log-Verzeichnis).
//!
//! Referenz: `qml2/pages/InfoPage.qml` + `qml2/pages`-Onboarding-Entscheid
//! (Spec §2.5: „3 Karten … Überspringbar“).

use gpui::{div, px, Context, FontWeight, IntoElement, ParentElement as _, Styled, Window};

use crate::app::{AppShell, Route};
use crate::components::{Button, ButtonVariant, Card, SectionLabel};
use crate::icons;
use crate::pages::regist_wizard;
use crate::pages::{page_header, page_scaffold};
use crate::theme;

pub fn page(
    shell: &mut AppShell,
    window: &mut Window,
    cx: &mut Context<AppShell>,
) -> impl IntoElement {
    let mut children: Vec<gpui::AnyElement> = Vec::new();
    children.push(page_header(
        shell,
        "Info & getting started",
        Some("Everything you need for your first stream"),
        window,
        cx,
    ));

    let portable = chiaki_settings::app_paths::is_portable();
    let base = chiaki_settings::app_paths::base_path().display().to_string();

    // Die 3 Karten des First-Run-Onboardings (Spec §2.5).
    children.push(
        div()
            .grid()
            .grid_cols(3)
            .gap(px(theme::SP_4))
            .child(
                Card::new("card-welcome")
                    .child(icons::icon(icons::paths::INFO, 24.0, theme::ACCENT))
                    .child(
                        div()
                            .text_size(px(theme::SIZE_HEADLINE))
                            .font_weight(FontWeight(theme::WEIGHT_HEADLINE))
                            .text_color(theme::TEXT_PRIMARY)
                            .child("Welcome & portable mode"),
                    )
                    .child(
                        div()
                            .text_size(px(theme::SIZE_BODY))
                            .text_color(theme::TEXT_SECONDARY)
                            .child(
                                "Chiaki Remaster stores everything (settings, registry, logs) \
                                 in a single folder — next to the application in portable mode, \
                                 otherwise in the user directory.",
                            ),
                    )
                    .child(info_line("Mode", if portable { "Portable" } else { "User directory" }))
                    .child(info_line("Data folder", &base)),
            )
            .child(
                Card::new("card-psn")
                    .child(icons::icon(icons::paths::GLOBE, 24.0, theme::ACCENT))
                    .child(
                        div()
                            .text_size(px(theme::SIZE_HEADLINE))
                            .font_weight(FontWeight(theme::WEIGHT_HEADLINE))
                            .text_color(theme::TEXT_PRIMARY)
                            .child("Connect PSN"),
                    )
                    .child(
                        div()
                            .text_size(px(theme::SIZE_BODY))
                            .text_color(theme::TEXT_SECONDARY)
                            .child(
                                "Sign in with your PSN account to use remote play over the \
                                 internet (including the PSN account ID for registration).",
                            ),
                    )
                    .child(
                        // wry/WebView2-PSN-Login (psn_login.rs): Tokens +
                        // Account-ID in den Settings speichern; Erfolg/Misserfolg
                        // kommt als Toast-UiEvent zurück.
                        Button::new("info-psn", "Start PSN sign-in")
                            .on_click(cx.listener(|shell, _ev, _window, _cx| {
                                crate::psn_login::start_psn_login_for_settings(
                                    shell.backend.settings().clone(),
                                    shell.backend.event_sender(),
                                );
                            })),
                    ),
            )
            .child(
                Card::new("card-regist")
                    .child(icons::icon(icons::paths::CONSOLE, 24.0, theme::ACCENT))
                    .child(
                        div()
                            .text_size(px(theme::SIZE_HEADLINE))
                            .font_weight(FontWeight(theme::WEIGHT_HEADLINE))
                            .text_color(theme::TEXT_PRIMARY)
                            .child("Register console"),
                    )
                    .child(
                        div()
                            .text_size(px(theme::SIZE_BODY))
                            .text_color(theme::TEXT_SECONDARY)
                            .child(
                                "Enable remote play on the console, show the 8-digit PIN \
                                 and run through the wizard — done.",
                            ),
                    )
                    .child(
                        Button::new("info-regist", "Get started")
                            .variant(ButtonVariant::Primary)
                            .on_click(cx.listener(|shell, _ev, _window, cx| {
                                regist_wizard::open(shell, cx);
                                shell.navigate(Route::Consoles, cx);
                            })),
                    ),
            )
            .into_any_element(),
    );

    // About-Zeile (Version + Log-Verzeichnis).
    children.push(
        Card::new("card-about")
            .child(SectionLabel::new("About"))
            .child(info_line(
                "Version",
                &format!("chiaki-ui {}", env!("CARGO_PKG_VERSION")),
            ))
            .child(info_line("Log directory", &chiaki_settings::app_paths::log_dir().display().to_string()))
            .child(
                div()
                    .text_size(px(theme::SIZE_CAPTION))
                    .text_color(theme::TEXT_SECONDARY)
                    .child(
                        "Windows-only · GPUI frontend of the chiaki-ng port · \
                         AGPL-3.0 with OpenSSL exception",
                    ),
            )
            .into_any_element(),
    );

    page_scaffold(children)
}

/// Kleine Label/Wert-Zeile (wie InfoRow im QML).
fn info_line(label: &str, value: &str) -> gpui::AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(theme::SP_1))
        .child(
            div()
                .text_size(px(theme::SIZE_CAPTION))
                .font_weight(FontWeight(theme::WEIGHT_CAPTION))
                .text_color(theme::TEXT_SECONDARY)
                .child(label.to_uppercase()),
        )
        .child(
            div()
                .text_size(px(theme::SIZE_CAPTION))
                .text_color(theme::TEXT_PRIMARY)
                .child(value.to_string()),
        )
        .into_any_element()
}
