//! Einstellungen (ui-v2-spec §2.3): zweispaltig — Kategorie-Liste links,
//! SettingsRows rechts, Suchfeld oben. GERÜST — der Settings-Agent füllt
//! die Kategorien/Rows aus den chiaki-settings-Gettern/Settern.

use gpui::{
    div, px, Context, IntoElement, InteractiveElement as _, ParentElement as _, Styled, Window,
};

use crate::app::AppShell;
use crate::components::{
    Button, Card, SectionLabel, Select, SelectOption, Slider, TextField, Toggle,
};
use crate::pages::{page_header, page_scaffold};
use crate::theme;

/// Kategorien (bindende Reihenfolge, Spec §2.3).
pub const CATEGORIES: [&str; 7] = [
    "Verbindung",
    "Video",
    "Audio",
    "Steuerung",
    "Netzwerk & Latenz",
    "PSN-Konto",
    "System",
];

pub fn page(
    shell: &mut AppShell,
    window: &mut Window,
    cx: &mut Context<AppShell>,
) -> impl IntoElement {
    let mut children: Vec<gpui::AnyElement> = Vec::new();
    children.push(page_header(shell, "Einstellungen", None, window, cx));

    // Suchfeld (live-Filter kommt mit dem Settings-Agenten).
    children.push(
        TextField::new("settings-search")
            .placeholder("Einstellungen durchsuchen…")
            .width(360.0)
            .into_any_element(),
    );

    // Zweispaltiger Rahmen.
    let categories: Vec<gpui::AnyElement> = CATEGORIES
        .iter()
        .map(|cat| {
            div()
                .px(px(theme::SP_3))
                .py(px(theme::SP_2))
                .rounded(px(theme::RADIUS_SM))
                .text_size(px(theme::SIZE_BODY))
                .text_color(theme::TEXT_PRIMARY)
                .hover(|s| s.bg(theme::SURFACE2))
                .cursor_pointer()
                .child(cat.to_string())
                .into_any_element()
        })
        .collect();

    // SettingsRows-Platzhalter mit allen Control-Varianten (Demo der
    // Komponenten; Bindung an Settings kommt mit dem Settings-Agenten).
    let rows: Vec<gpui::AnyElement> = vec![
        SectionLabel::new("Verbindung").into_any_element(),
        Card::new("row-connect")
            .child(
                div().flex().flex_col().gap_3()
                    .child(
                        div().flex().items_center().justify_between()
                            .child(div().text_size(px(theme::SIZE_BODY)).child("Automatic connect"))
                            .child(Toggle::new("toggle-demo", false).on_change(|_v, _w, _cx| {})),
                    )
                    .child(
                        div().flex().items_center().justify_between()
                            .child(div().text_size(px(theme::SIZE_BODY)).child("Auflösung (lokal, PS5)"))
                            .child(
                                Select::new("select-demo")
                                    .options(vec![
                                        SelectOption::new("720", "720p"),
                                        SelectOption::new("1080", "1080p"),
                                    ])
                                    .selected("1080"),
                            ),
                    )
                    .child(
                        div().flex().items_center().justify_between()
                            .child(div().text_size(px(theme::SIZE_BODY)).child("Bitrate-Limit (Mbps)"))
                            .child(Slider::new("slider-demo", 15.0, 5.0, 50.0)),
                    ),
            )
            .into_any_element(),
    ];

    children.push(
        div()
            .flex()
            .flex_row()
            .gap(px(theme::SP_5))
            .flex_1()
            .child(
                div()
                    .w(px(220.0))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(categories),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(theme::SP_4))
                    .children(rows),
            )
            .into_any_element(),
    );

    children.push(
        div()
            .child(
                Button::new("settings-reset", "Keymap zurücksetzen")
                    .on_click(cx.listener(|shell, _ev, _window, cx| {
                        let result = shell
                            .backend
                            .settings()
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .update(|s| s.clear_key_mapping());
                        let kind = if result.is_ok() {
                            crate::components::ToastKind::Success
                        } else {
                            crate::components::ToastKind::Danger
                        };
                        shell.push_toast(
                            crate::components::ToastData::new(kind, "Keymap zurückgesetzt"),
                            cx,
                        );
                    })),
            )
            .into_any_element(),
    );

    page_scaffold(children)
}
