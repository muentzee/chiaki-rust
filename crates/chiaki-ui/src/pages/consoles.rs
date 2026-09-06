//! Konsolen (ui-v2-spec §2.2): Grid aller Konsolen + Filter-Chips + Registrierungs-Wizard.
//! GERÜST — der Konsolen-Agent füllt Grid/Chips/Kontextmenüs aus.

use gpui::{div, px, Context, ElementId, IntoElement, ParentElement as _, Styled, Window};

use crate::app::AppShell;
use crate::components::{Button, Card, EmptyState, SectionLabel, StatusBadge, StatusKind};
use crate::icons;
use crate::pages::{page_header, page_scaffold};
use crate::theme;

pub fn page(
    shell: &mut AppShell,
    window: &mut Window,
    cx: &mut Context<AppShell>,
) -> impl IntoElement {
    // Wizard offen? Dann Wizard statt Liste (keine Route-Änderung nötig).
    if shell.show_regist_wizard {
        return crate::pages::regist_wizard::page(shell, window, cx).into_any_element();
    }

    let hosts = shell.backend.discovery().hosts();
    let registered = shell.backend.settings().lock().unwrap_or_else(|e| e.into_inner()).registered_hosts().len();

    let mut children: Vec<gpui::AnyElement> = Vec::new();
    children.push(page_header(
        shell,
        "Konsolen",
        Some(&format!("{} entdeckt · {} registriert", hosts.len(), registered)),
        window,
        cx,
    ));

    // Filter-Chips (Platzhalter — der Konsolen-Agent baut die Chips als Buttons).
    children.push(SectionLabel::new("Filter: Alle / Bereit / Offline / PSN").into_any_element());

    children.push(
        div()
            .grid()
            .grid_cols(3)
            .gap(px(theme::SP_4))
            .children(hosts.iter().enumerate().map(|(i, host)| {
                let addr = host.host_addr.clone();
                let status = match host.state {
                    chiaki_core::discovery::DiscoveryHostState::Ready => StatusKind::Ready,
                    chiaki_core::discovery::DiscoveryHostState::Standby => StatusKind::Standby,
                    _ => StatusKind::Offline,
                };
                Card::new(ElementId::Name(format!("tile-{i}-{addr}").into()))
                    .child(
                        div()
                            .text_size(px(theme::SIZE_HEADLINE))
                            .font_weight(gpui::FontWeight(theme::WEIGHT_HEADLINE))
                            .child(host.host_name.clone().unwrap_or_else(|| addr.clone())),
                    )
                    .child(StatusBadge::new(status).label(addr.clone()))
                    .child(
                        Button::new(
                            ElementId::Name(format!("connect-{i}-{addr}").into()),
                            "Verbinden",
                        )
                        .variant(crate::components::ButtonVariant::Primary)
                        .on_click(cx.listener(|shell, _ev, _window, _cx| {
                            let _ = shell; // TODO(konsolen-agent): verbinden
                        })),
                    )
                    .into_any_element()
            }))
            .into_any_element(),
    );

    children.push(
        div()
            .flex()
            .gap_2()
            .child(
                Button::new("open-wizard", "Konsole registrieren")
                    .variant(crate::components::ButtonVariant::Primary)
                    .on_click(cx.listener(|shell, _ev, _window, cx| {
                        shell.show_regist_wizard = true;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("add-manual", "Manuellen Host hinzufügen")
                    .on_click(cx.listener(|shell, _ev, _window, cx| {
                        shell.push_toast(
                            crate::components::ToastData::new(
                                crate::components::ToastKind::Info,
                                "Manueller Host",
                            )
                            .message("Formular kommt mit dem Konsolen-Agenten."),
                            cx,
                        );
                    })),
            )
            .into_any_element(),
    );

    if hosts.is_empty() && registered == 0 {
        children.push(
            Card::new("empty")
                .child(EmptyState::new(
                    icons::paths::SEARCH,
                    "Noch keine Konsolen",
                    "Discovery läuft — oder starte den Registrierungs-Wizard.",
                ))
                .into_any_element(),
        );
    }

    page_scaffold(children)
}
