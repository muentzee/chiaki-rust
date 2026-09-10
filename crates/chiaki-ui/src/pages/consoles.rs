//! Konsolen (ui-v2-spec §2.2): Grid aller Konsolen (Discovery + Manuell +
//! PSN-Remote), Filter-Chips (Alle / Bereit / Offline / PSN) und der
//! Registrierungs-Wizard als Overlay dieser Seite.
//!
//! Referenz: `qml2/pages/ConsolesPage.qml` — Chip-Logik auf den
//! Host-Feldern, Klick = Verbinden, Kontextmenü identisch zur Home-Seite
//! (geteilt über [`crate::pages::home`]).

use gpui::{
    div, px, Context, FontWeight, IntoElement, InteractiveElement as _, ParentElement as _,
    StatefulInteractiveElement as _, Styled, Window,
};

use chiaki_settings::hosts::{HostMac, ManualHost};

use crate::app::AppShell;
use crate::components::{
    Button, ButtonVariant, Card, EmptyState, SectionLabel, StatusKind, TextField, ToastData,
    ToastKind,
};
use crate::icons;
use crate::pages::home::{console_entries, console_tile, ConsoleEntry};
use crate::pages::regist_wizard;
use crate::pages::{page_header, page_scaffold};
use crate::theme;

/// Filter-Chips der Konsolen-Seite (Reihenfolge bindend, Spec §2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleFilter {
    All,
    Ready,
    Offline,
    Psn,
}

impl ConsoleFilter {
    /// Chip-Reihenfolge (bindend).
    pub const ALL: [ConsoleFilter; 4] = [
        ConsoleFilter::All,
        ConsoleFilter::Ready,
        ConsoleFilter::Offline,
        ConsoleFilter::Psn,
    ];

    /// Label (bindend, Spec §2.2).
    pub fn label(self) -> &'static str {
        match self {
            ConsoleFilter::All => "All",
            ConsoleFilter::Ready => "Ready",
            ConsoleFilter::Offline => "Offline",
            ConsoleFilter::Psn => "PSN",
        }
    }

    /// Port von `matchesFilter` (ConsolesPage.qml): „Bereit“ = Discovery
    /// ready, „Offline“ = alles andere, „PSN“ = reine PSN-Remote-Hosts.
    pub(crate) fn matches(self, entry: &ConsoleEntry) -> bool {
        match self {
            ConsoleFilter::All => true,
            ConsoleFilter::Ready => entry.status == StatusKind::Ready,
            ConsoleFilter::Offline => entry.status != StatusKind::Ready,
            ConsoleFilter::Psn => entry.psn,
        }
    }
}

pub fn page(
    shell: &mut AppShell,
    window: &mut Window,
    cx: &mut Context<AppShell>,
) -> impl IntoElement {
    // Wizard offen? Dann Wizard statt Liste (keine Route-Änderung nötig).
    if shell.show_regist_wizard {
        return regist_wizard::page(shell, window, cx).into_any_element();
    }

    let filter = shell.regist_wizard.console_filter;
    let entries = console_entries(shell);
    let filtered: Vec<ConsoleEntry> = entries
        .iter()
        .filter(|entry| filter.matches(entry))
        .cloned()
        .collect();
    let focus_handles = shell.regist_wizard.sync_tile_focus(entries.len(), cx);
    let ui_focus = shell.regist_wizard.sync_ui_focus(2, cx);

    let mut children: Vec<gpui::AnyElement> = Vec::new();
    children.push(page_header(
        shell,
        "Consoles",
        Some(&format!(
            "{} of {} consoles visible",
            filtered.len(),
            entries.len()
        )),
        window,
        cx,
    ));

    // Kopfzeile: Filter-Chips links, Registrieren rechts.
    let chip_focus = ui_focus[0].clone();
    children.push(
        div()
            .flex()
            .items_center()
            .justify_between()
            .flex_wrap()
            .gap(px(theme::SP_4))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_2()
                    .children(ConsoleFilter::ALL.iter().enumerate().map(|(i, chip)| {
                        filter_chip(i, *chip, *chip == filter, chip_focus.clone(), cx)
                    })),
            )
            .child(
                Button::new("consoles-regist", "Register console")
                    .variant(ButtonVariant::Primary)
                    .focus_handle(ui_focus[1].clone())
                    .on_click(cx.listener(|shell, _ev, _window, cx| {
                        regist_wizard::open(shell, cx);
                    })),
            )
            .into_any_element(),
    );

    // Grid (240×140-Kacheln, Zeilenumbruch wie das QML-Flow-Layout).
    if filtered.is_empty() {
        children.push(
            Card::new("consoles-empty")
                .child(EmptyState::new(
                    icons::paths::SEARCH,
                    "No consoles here",
                    match filter {
                        ConsoleFilter::All => {
                            "Register a console or add a manual host.\n\
                             Discovery keeps running in the background."
                        }
                        ConsoleFilter::Psn => {
                            "No PSN remote consoles found.\n\
                             Sign in under Settings › PSN — consoles from your \
                             PSN account will then appear here."
                        }
                        _ => "No console matches this filter.",
                    },
                ))
                .into_any_element(),
        );
    } else {
        children.push(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(theme::SP_4))
                .children(filtered.iter().enumerate().map(|(i, entry)| {
                    // Original-Index für den stabilen Kachel-/Fokus-Bezug
                    // (ConsolesPage.qml: „Original-Indizes in Chiaki.hosts“).
                    let orig = entries
                        .iter()
                        .position(|e| e.addr == entry.addr && e.name == entry.name)
                        .unwrap_or(i);
                    console_tile(orig, entry, focus_handles[orig].clone(), shell, cx)
                }))
                .into_any_element(),
        );
    }

    // Manueller Host (C++: ManualHostLayer — die Adresse reicht; die
    // Verknüpfung mit einem registrierten Host macht der Registrierungs-
    // erfolg wie im C++ `QmlRegist::success`-Pfad).
    children.push(SectionLabel::new("Manual host").into_any_element());
    children.push(
        Card::new("manual-host-card")
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .flex_wrap()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(theme::SIZE_BODY))
                                    .text_color(theme::TEXT_PRIMARY)
                                    .child("Add manual host"),
                            )
                            .child(
                                div()
                                    .text_size(px(theme::SIZE_CAPTION))
                                    .text_color(theme::TEXT_SECONDARY)
                                    .child(
                                        "IP or hostname of a console that is not found by discovery.",
                                    ),
                            ),
                    )
                    .child(manual_host_field(shell, window, cx)),
            )
            .into_any_element(),
    );

    page_scaffold(children)
}

/// Filter-Chip (Optik wie FilterChip.qml: Akzent-Tönung wenn gewählt).
fn filter_chip(
    i: usize,
    filter: ConsoleFilter,
    selected: bool,
    focus: gpui::FocusHandle,
    cx: &mut Context<AppShell>,
) -> gpui::AnyElement {
    let (bg, border, fg) = if selected {
        (
            gpui::Hsla { a: 0.22, ..theme::ACCENT },
            gpui::Hsla { a: 0.6, ..theme::ACCENT },
            theme::TEXT_PRIMARY,
        )
    } else {
        (gpui::transparent_black(), theme::OUTLINE, theme::TEXT_SECONDARY)
    };
    div()
        .id(("console-filter", i))
        .flex()
        .items_center()
        .justify_center()
        .px(px(theme::SP_3))
        .py(px(theme::SP_1))
        .rounded_full()
        .bg(bg)
        .border_1()
        .border_color(border)
        .text_size(px(theme::SIZE_CAPTION))
        .font_weight(FontWeight(if selected {
            theme::WEIGHT_HEADLINE
        } else {
            theme::WEIGHT_CAPTION
        }))
        .text_color(fg)
        .cursor_pointer()
        .hover(|s| s.bg(theme::SURFACE2))
        .track_focus(&focus)
        .focus(|s| s.border_2().border_color(theme::ACCENT))
        .on_click(cx.listener(move |shell, _ev, _window, cx| {
            shell.regist_wizard.console_filter = filter;
            cx.notify();
        }))
        .child(filter.label().to_string())
        .into_any_element()
}

/// Manueller-Host-Eingabe + Hinzufügen-Button (kontrolliertes Feld über den
/// Seitenzustand, Fokus-Handle persistent in [`regist_wizard::WizardState`]).
fn manual_host_field(
    shell: &mut AppShell,
    window: &mut Window,
    cx: &mut Context<AppShell>,
) -> gpui::AnyElement {
    // Von Home angefordert (Schnellaktion): Feld nach dem Frame fokussieren.
    if shell.regist_wizard.manual_focus_pending {
        shell.regist_wizard.manual_focus_pending = false;
        let handle = shell.regist_wizard.manual_host_focus.clone();
        window.defer(cx, move |window, _cx| handle.focus(window));
    }

    let value = shell.regist_wizard.manual_host.clone();
    let focus = shell.regist_wizard.manual_host_focus.clone();
    let weak = cx.entity().downgrade();

    div()
        .flex()
        .items_center()
        .gap_2()
        .child(
            TextField::new("manual-host-field")
                .value(value)
                .placeholder("e.g. 192.168.1.42")
                .width(260.0)
                .focus_handle(focus)
                .on_change(move |v, _window, cx| {
                    let v = v.to_string();
                    let _ = weak.update(cx, |shell, _cx| shell.regist_wizard.manual_host = v);
                }),
        )
        .child(
            Button::new("manual-host-add", "Add").on_click(cx.listener(
                |shell, _ev, _window, cx| add_manual_host(shell, cx),
            )),
        )
        .into_any_element()
}

/// Port von `QmlBackend::addManualHost` (unregistriert): ManualHost anlegen.
fn add_manual_host(shell: &mut AppShell, cx: &mut Context<AppShell>) {
    let addr = shell.regist_wizard.manual_host.trim().to_string();
    if addr.is_empty() {
        shell.push_toast(
            ToastData::new(ToastKind::Warn, "No address")
                .message("Enter an IP or hostname."),
            cx,
        );
        return;
    }
    let saved = shell
        .backend
        .settings()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .update(|s| {
            s.set_manual_host(ManualHost::new(-1, addr.clone(), false, HostMac::default()));
        });
    match saved {
        Ok(_) => {
            shell.regist_wizard.manual_host.clear();
            shell.push_toast(
                ToastData::new(ToastKind::Success, "Manual host added").message(addr),
                cx,
            );
        }
        Err(err) => {
            shell.push_toast(
                ToastData::new(ToastKind::Danger, "Save failed")
                    .message(err.to_string()),
                cx,
            );
        }
    }
}
