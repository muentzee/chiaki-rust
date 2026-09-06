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
        "Info & Erste Schritte",
        Some("Alles Wichtige für den ersten Stream"),
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
                            .child("Willkommen & Portable-Modus"),
                    )
                    .child(
                        div()
                            .text_size(px(theme::SIZE_BODY))
                            .text_color(theme::TEXT_SECONDARY)
                            .child(
                                "Chiaki Remaster speichert alles (Settings, Registry, Logs) \
                                 in einem Ordner — neben der Anwendung im Portable-Modus, \
                                 sonst im Nutzer-Verzeichnis.",
                            ),
                    )
                    .child(info_line("Modus", if portable { "Portable" } else { "Nutzer-Verzeichnis" }))
                    .child(info_line("Daten-Ordner", &base)),
            )
            .child(
                Card::new("card-psn")
                    .child(icons::icon(icons::paths::GLOBE, 24.0, theme::ACCENT))
                    .child(
                        div()
                            .text_size(px(theme::SIZE_HEADLINE))
                            .font_weight(FontWeight(theme::WEIGHT_HEADLINE))
                            .text_color(theme::TEXT_PRIMARY)
                            .child("PSN verbinden"),
                    )
                    .child(
                        div()
                            .text_size(px(theme::SIZE_BODY))
                            .text_color(theme::TEXT_SECONDARY)
                            .child(
                                "Mit deinem PSN-Konto anmelden, um Remote-Play über das \
                                 Internet zu nutzen (inkl. PSN-Account-ID für die \
                                 Registrierung).",
                            ),
                    )
                    .child(
                        // wry/WebView2-PSN-Login (psn_login.rs): Tokens +
                        // Account-ID in den Settings speichern; Erfolg/Misserfolg
                        // kommt als Toast-UiEvent zurück.
                        Button::new("info-psn", "PSN-Anmeldung starten")
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
                            .child("Konsole registrieren"),
                    )
                    .child(
                        div()
                            .text_size(px(theme::SIZE_BODY))
                            .text_color(theme::TEXT_SECONDARY)
                            .child(
                                "Remote Play auf der Konsole aktivieren, die 8-stellige PIN \
                                 anzeigen und den Wizard durchlaufen — fertig.",
                            ),
                    )
                    .child(
                        Button::new("info-regist", "Direkt einsteigen")
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
            .child(SectionLabel::new("Über"))
            .child(info_line(
                "Version",
                &format!("chiaki-ui {}", env!("CARGO_PKG_VERSION")),
            ))
            .child(info_line("Log-Verzeichnis", &chiaki_settings::app_paths::log_dir().display().to_string()))
            .child(
                div()
                    .text_size(px(theme::SIZE_CAPTION))
                    .text_color(theme::TEXT_SECONDARY)
                    .child(
                        "Windows-only · GPUI-Oberfläche des chiaki-ng-Ports · \
                         AGPL-3.0 mit OpenSSL-Ausnahme",
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
