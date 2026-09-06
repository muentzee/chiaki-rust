//! Registrierungs-Wizard (ui-v2-spec §2.2): 3 Schritte — Art der Konsole →
//! PIN/PSN → Ergebnis; Fortschrittsanzeige oben. GERÜST — der Regist-Agent
//! füllt PIN-Eingabe + chiaki_core::regist-Anbindung aus.

use gpui::{div, px, Context, IntoElement, ParentElement as _, Styled, Window};

use crate::app::AppShell;
use crate::components::{Button, ButtonVariant, Card, SectionLabel};
use crate::pages::{page_header, page_scaffold};
use crate::theme;

/// Die 3 Wizard-Schritte (Reihenfolge bindend).
pub const WIZARD_STEPS: [&str; 3] = ["Art der Konsole", "PIN / PSN", "Ergebnis"];

pub fn page(
    shell: &mut AppShell,
    window: &mut Window,
    cx: &mut Context<AppShell>,
) -> impl IntoElement {
    let mut children: Vec<gpui::AnyElement> = Vec::new();
    children.push(page_header(shell, "Konsole registrieren", None, window, cx));

    // StepBar (Fortschrittsanzeige, Spec §2.2).
    children.push(
        div()
            .flex()
            .flex_row()
            .gap_2()
            .children(WIZARD_STEPS.iter().enumerate().map(|(i, step)| {
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px(px(theme::SP_3))
                    .py(px(theme::SP_2))
                    .rounded(px(theme::RADIUS_SM))
                    .bg(theme::SURFACE)
                    .border_1()
                    .border_color(theme::HAIRLINE)
                    .child(
                        div()
                            .text_size(px(theme::SIZE_CAPTION))
                            .font_weight(gpui::FontWeight(theme::WEIGHT_CAPTION))
                            .text_color(theme::ACCENT)
                            .child(format!("{}. SCHRITT", i + 1)),
                    )
                    .child(
                        div()
                            .text_size(px(theme::SIZE_BODY))
                            .text_color(theme::TEXT_PRIMARY)
                            .child(step.to_string()),
                    )
            }))
            .into_any_element(),
    );

    children.push(SectionLabel::new("Schritt 1 von 3 — Art der Konsole").into_any_element());

    children.push(
        Card::new("wizard-step")
            .child(
                div()
                    .text_size(px(theme::SIZE_BODY))
                    .text_color(theme::TEXT_SECONDARY)
                    .child(
                        "Platzhalter: PS4 / PS5 / PSN-Remote auswählen. Der Regist-Agent \
                         verdrahtet hier chiaki_core::regist::Regist (PIN) bzw. PSN-Pfad.",
                    ),
            )
            .into_any_element(),
    );

    children.push(
        div()
            .flex()
            .justify_between()
            .child(
                Button::new("wizard-cancel", "Abbrechen")
                    .on_click(cx.listener(|shell, _ev, _window, cx| {
                        shell.show_regist_wizard = false;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("wizard-next", "Weiter")
                    .variant(ButtonVariant::Primary)
                    .on_click(cx.listener(|shell, _ev, _window, cx| {
                        shell.push_toast(
                            crate::components::ToastData::new(
                                crate::components::ToastKind::Info,
                                "Wizard-Gerüst",
                            )
                            .message("Schritte kommen mit dem Regist-Agenten."),
                            cx,
                        );
                    })),
            )
            .into_any_element(),
    );

    page_scaffold(children)
}
