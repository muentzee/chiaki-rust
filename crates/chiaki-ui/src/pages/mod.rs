//! Seiten-Gerüste (ui-v2-spec §2 Informationsarchitektur).
//!
//! **Verbindliche Seiten-Signatur** (CONTRACT-UI.md):
//! ```ignore
//! pub fn page(shell: &mut AppShell, window: &mut Window, cx: &mut Context<AppShell>) -> impl IntoElement
//! // Stream:
//! pub fn page(shell: &mut AppShell, host: HostId, window: &mut Window, cx: &mut Context<AppShell>) -> impl IntoElement
//! ```
//! Seiten erhalten das AppShell-Model direkt und bauen Elemente über die
//! Komponenten (`crate::components`) mit `cx.listener(...)`-Callbacks.

pub mod consoles;
pub mod home;
pub mod info;
pub mod regist_wizard;
pub mod settings;
pub mod stream;

use gpui::{div, px, Context, IntoElement as _, ParentElement as _, Styled};

use crate::app::AppShell;
use crate::theme;

/// Gemeinsamer Seiten-Kopf: Title + optionale Beschreibung.
pub(crate) fn page_header(
    shell: &mut AppShell,
    title: &str,
    subtitle: Option<&str>,
    _window: &mut gpui::Window,
    _cx: &mut Context<AppShell>,
) -> gpui::AnyElement {
    let _ = shell;
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(px(theme::SIZE_DISPLAY))
                .font_weight(gpui::FontWeight(theme::WEIGHT_DISPLAY))
                .text_color(theme::TEXT_PRIMARY)
                .child(title.to_string()),
        )
        .children(subtitle.map(|s| {
            div().text_size(px(theme::SIZE_BODY)).text_color(theme::TEXT_SECONDARY).child(s.to_string())
        }))
        .into_any_element()
}

/// Standard-Seitenrahmen: Padding + vertikale Anordnung.
pub(crate) fn page_scaffold(children: Vec<gpui::AnyElement>) -> gpui::AnyElement {
    div()
        .size_full()
        .flex()
        .flex_col()
        .gap(px(theme::SP_5))
        .p(px(theme::SP_6))
        .children(children)
        .into_any_element()
}
