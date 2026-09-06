//! Info / Erste Schritte (ui-v2-spec §2.5): First-Run-Onboarding-Karten.
//! GERÜST — der Info-Agent füllt Willkommen/Portable-Modus/PSN-Karten aus.

use gpui::{div, px, Context, IntoElement, ParentElement as _, Styled, Window};

use crate::app::{AppShell, Route};
use crate::components::{Button, ButtonVariant, Card, SectionLabel};
use crate::pages::{page_header, page_scaffold};
use crate::theme;

pub fn page(
    shell: &mut AppShell,
    window: &mut Window,
    cx: &mut Context<AppShell>,
) -> impl IntoElement {
    let mut children: Vec<gpui::AnyElement> = Vec::new();
    children.push(page_header(shell, "Info & Erste Schritte", None, window, cx));

    let portable = chiaki_settings::app_paths::is_portable();
    let base = chiaki_settings::app_paths::base_path().display().to_string();

    children.push(
        div()
            .grid()
            .grid_cols(3)
            .gap(px(theme::SP_4))
            .child(
                Card::new("card-welcome")
                    .child(SectionLabel::new("Karte 1"))
                    .child(
                        div()
                            .text_size(px(theme::SIZE_HEADLINE))
                            .font_weight(gpui::FontWeight(theme::WEIGHT_HEADLINE))
                            .child("Willkommen"),
                    )
                    .child(
                        div()
                            .text_size(px(theme::SIZE_BODY))
                            .text_color(theme::TEXT_SECONDARY)
                            .child(format!(
                                "Portable-Modus: {portable} — Daten-Ordner: {base}"
                            )),
                    ),
            )
            .child(
                Card::new("card-psn")
                    .child(SectionLabel::new("Karte 2"))
                    .child(
                        div()
                            .text_size(px(theme::SIZE_HEADLINE))
                            .font_weight(gpui::FontWeight(theme::WEIGHT_HEADLINE))
                            .child("PSN verbinden"),
                    )
                    .child(
                        div()
                            .text_size(px(theme::SIZE_BODY))
                            .text_color(theme::TEXT_SECONDARY)
                            .child("Remote-Play über PSN einrichten (später verfügbar)."),
                    ),
            )
            .child(
                Card::new("card-regist")
                    .child(SectionLabel::new("Karte 3"))
                    .child(
                        div()
                            .text_size(px(theme::SIZE_HEADLINE))
                            .font_weight(gpui::FontWeight(theme::WEIGHT_HEADLINE))
                            .child("Konsole registrieren"),
                    )
                    .child(
                        Button::new("info-regist", "Direkt einsteigen")
                            .variant(ButtonVariant::Primary)
                            .on_click(cx.listener(|shell, _ev, _window, cx| {
                                shell.show_regist_wizard = true;
                                shell.navigate(Route::Consoles, cx);
                            })),
                    ),
            )
            .into_any_element(),
    );

    children.push(
        div()
            .text_size(px(theme::SIZE_BODY))
            .text_color(theme::TEXT_SECONDARY)
            .child(format!(
                "chiaki-rs v{} — GPUI-App-Shell (Windows-only). Logs: {}",
                env!("CARGO_PKG_VERSION"),
                chiaki_settings::app_paths::log_dir().display()
            ))
            .into_any_element(),
    );

    page_scaffold(children)
}
