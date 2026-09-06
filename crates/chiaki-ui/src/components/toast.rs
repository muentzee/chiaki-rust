//! Toast (unten rechts, automatisch ausblendbar — ui-v2-spec §4).
//!
//! Modell ([`ToastData`]) und View ([`ToastView`]) getrennt: die AppShell
//! hält `Vec<ToastData>`, rendert [`ToastView`]s und verwaltet Auto-Timeout
//! + Stack (siehe app.rs). Der UiEvent-Pfad erzeugt Toasts über
//! `UiEvent::Toast(ToastData)`.

use std::time::Duration;

use gpui::{
    div, prelude::FluentBuilder as _, px, App, IntoElement, InteractiveElement as _,
    ParentElement, RenderOnce, SharedString, StatefulInteractiveElement as _, Styled, Window,
};

use crate::{icons, theme};

pub type ToastId = u64;

/// Art des Toasts (steuert die Akzentfarbe).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Warn,
    Danger,
}

impl ToastKind {
    pub fn color(self) -> gpui::Hsla {
        match self {
            ToastKind::Info => theme::INFO,
            ToastKind::Success => theme::SUCCESS,
            ToastKind::Warn => theme::WARN,
            ToastKind::Danger => theme::DANGER,
        }
    }
}

/// Ein Toast-Eintrag (Modell).
#[derive(Debug, Clone)]
pub struct ToastData {
    pub id: ToastId,
    pub kind: ToastKind,
    pub title: SharedString,
    pub message: Option<SharedString>,
    /// 0 = kein Auto-Timeout.
    pub timeout: Duration,
}

impl ToastData {
    pub fn new(kind: ToastKind, title: impl Into<SharedString>) -> Self {
        Self { id: 0, kind, title: title.into(), message: None, timeout: Duration::from_secs(4) }
    }

    pub fn message(mut self, message: impl Into<SharedString>) -> Self {
        self.message = Some(message.into());
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_id(mut self, id: ToastId) -> Self {
        self.id = id;
        self
    }
}

/// View eines Toasts.
#[derive(gpui::IntoElement)]
pub struct ToastView {
    pub toast: ToastData,
    on_dismiss: Option<crate::components::ClickHandler>,
}

impl ToastView {
    pub fn new(toast: ToastData) -> Self {
        Self { toast, on_dismiss: None }
    }

    pub fn on_dismiss(
        mut self,
        f: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_dismiss = Some(Box::new(f));
        self
    }
}

impl RenderOnce for ToastView {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let accent = self.toast.kind.color();
        let on_dismiss = self.on_dismiss;
        div()
            .flex()
            .flex_col()
            .w(px(360.0))
            .rounded(px(theme::RADIUS_MD))
            .bg(theme::GLASS)
            .border_1()
            .border_color(theme::OUTLINE)
            .p(px(theme::SP_3))
            .gap_1()
            .shadow(vec![gpui::BoxShadow {
                color: gpui::black().opacity(0.24),
                offset: gpui::point(px(0.0), px(2.0)),
                blur_radius: px(16.0),
                spread_radius: px(0.0),
            }])
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().size(px(8.0)).rounded_full().bg(accent))
                            .child(
                                div()
                                    .text_size(px(theme::SIZE_HEADLINE))
                                    .font_weight(gpui::FontWeight(theme::WEIGHT_HEADLINE))
                                    .text_color(theme::TEXT_PRIMARY)
                                    .child(self.toast.title.clone()),
                            ),
                    )
                    .when_some(on_dismiss, |el, handler| {
                        el.child(
                            div()
                                .id(("toast-close", self.toast.id))
                                .cursor_pointer()
                                .p_1()
                                .rounded(px(theme::RADIUS_SM))
                                .hover(|s| s.bg(theme::SURFACE2))
                                .on_click(move |ev, window, cx| handler(ev, window, cx))
                                .child(icons::icon(
                                    icons::paths::CLOSE,
                                    12.0,
                                    theme::TEXT_SECONDARY,
                                )),
                        )
                    }),
            )
            .children(self.toast.message.clone().map(|m| {
                div().text_size(px(theme::SIZE_BODY)).text_color(theme::TEXT_SECONDARY).child(m)
            }))
    }
}
