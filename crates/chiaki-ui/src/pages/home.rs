//! Home (ui-v2-spec §2.1): Hero-Karte der letzten Konsole, „Deine Konsolen"-
//! Reihe, Schnellaktionen. GERÜST — der Home-Agent füllt Hero/Tiles aus.

use gpui::{div, px, Context, IntoElement, ParentElement as _, Styled, Window};

use crate::app::{AppShell, Route};
use crate::components::{Button, ButtonVariant, Card, EmptyState, SectionLabel};
use crate::icons;
use crate::pages::{page_header, page_scaffold};
use crate::theme;

pub fn page(
    shell: &mut AppShell,
    window: &mut Window,
    cx: &mut Context<AppShell>,
) -> impl IntoElement {
    let hosts = shell.backend.discovery().hosts();

    let mut children: Vec<gpui::AnyElement> = Vec::new();
    children.push(page_header(shell, "Home", Some("Willkommen zurück"), window, cx));

    // Hero (Platzhalter): erste Konsole oder Leerzustand.
    children.push(
        match hosts.first() {
            Some(host) => Card::new("hero")
                .hero()
                .child(
                    div()
                        .text_size(px(theme::SIZE_TITLE))
                        .font_weight(gpui::FontWeight(theme::WEIGHT_TITLE))
                        .child(host.host_name.clone().unwrap_or_else(|| host.host_addr.clone())),
                )
                .child(
                    div()
                        .text_size(px(theme::SIZE_BODY))
                        .text_color(theme::TEXT_SECONDARY)
                        .child(format!(
                            "{} ({})",
                            host.host_addr,
                            chiaki_core::discovery::discovery_host_state_string(host.state)
                        )),
                )
                .child(
                    Button::new("hero-connect", "Verbinden")
                        .variant(ButtonVariant::Primary)
                        .on_click(cx.listener(|_shell, _ev, _window, _cx| {
                            // TODO(home-agent): Session über backend.sessions() starten.
                        })),
                )
                .into_any_element(),
            None => Card::new("hero-empty")
                .hero()
                .child(
                    EmptyState::new(
                        icons::paths::CONSOLE,
                        "Keine Konsole gefunden",
                        "Stelle sicher, dass die Konsole im Netzwerk ist — oder registriere sie.",
                    )
                    .action(
                        Button::new("hero-regist", "Konsole registrieren")
                            .variant(ButtonVariant::Primary)
                            .on_click(cx.listener(|shell, _ev, _window, cx| {
                                shell.navigate(Route::Consoles, cx);
                            })),
                    ),
                )
                .into_any_element(),
        },
    );

    // Schnellaktionen (Spec §2.1).
    children.push(SectionLabel::new("Schnellaktionen").into_any_element());
    children.push(
        div()
            .flex()
            .flex_row()
            .gap_2()
            .child(
                Button::new("qa-regist", "Konsole registrieren")
                    .on_click(cx.listener(|shell, _ev, _window, cx| {
                        shell.navigate(Route::Consoles, cx);
                    })),
            )
            .child(
                Button::new("qa-psn", "PSN Remote aktivieren")
                    .on_click(cx.listener(|shell, _ev, _window, cx| {
                        shell.navigate(Route::Settings, cx);
                    })),
            )
            .child(
                Button::new("qa-manual", "Manuellen Host hinzufügen")
                    .on_click(cx.listener(|shell, _ev, _window, cx| {
                        shell.navigate(Route::Consoles, cx);
                    })),
            )
            .into_any_element(),
    );

    // Komponenten-Demo (wird vom Home-Agenten ersetzt): Toast-Test.
    children.push(
        div()
            .flex()
            .gap_2()
            .child(
                Button::new("demo-toast", "Toast testen")
                    .on_click(cx.listener(|shell, _ev, _window, cx| {
                        shell.push_toast(
                            crate::components::ToastData::new(
                                crate::components::ToastKind::Success,
                                "Backend steht",
                            )
                            .message("Discovery, Settings, Controller und Session-Layer sind verdrahtet."),
                            cx,
                        );
                    })),
            )
            .child(
                Button::new("demo-dialog", "Dialog testen")
                    .variant(ButtonVariant::Danger)
                    .on_click(cx.listener(|shell, _ev, _window, cx| {
                        shell.push_dialog(
                            crate::components::Dialog::new(
                                "demo",
                                "Dialog-Layer",
                                "Dim-Backdrop + zentrierte Card — Esc oder Abbrechen schließt.",
                            )
                            .confirm(),
                            cx,
                        );
                    })),
            )
            .into_any_element(),
    );

    page_scaffold(children)
}
