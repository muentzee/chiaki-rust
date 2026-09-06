//! Toggle-Schalter (SettingsRow-Control, ui-v2-spec §4).

use gpui::{
    div, prelude::FluentBuilder as _, px, App, ElementId, FocusHandle, IntoElement,
    InteractiveElement as _, ParentElement, RenderOnce, Styled, StatefulInteractiveElement as _,
    Window,
};

use crate::{motion, theme};

/// Kontrollierter Toggle. `checked` ist State des Parents (Settings/Seite);
/// `on_change` liefert den neuen Wert.
#[derive(gpui::IntoElement)]
pub struct Toggle {
    id: ElementId,
    checked: bool,
    disabled: bool,
    on_change: Option<Box<dyn Fn(bool, &mut Window, &mut App) + 'static>>,
    focus: Option<FocusHandle>,
}

impl Toggle {
    pub fn new(id: impl Into<ElementId>, checked: bool) -> Self {
        Self { id: id.into(), checked, disabled: false, on_change: None, focus: None }
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn on_change(
        mut self,
        f: impl Fn(bool, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_change = Some(Box::new(f));
        self
    }

    pub fn focus_handle(mut self, handle: FocusHandle) -> Self {
        self.focus = Some(handle);
        self
    }
}

impl RenderOnce for Toggle {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let track = if self.disabled {
            theme::SURFACE2
        } else if self.checked {
            theme::ACCENT
        } else {
            theme::SURFACE2
        };
        let knob = if self.checked { gpui::white() } else { theme::TEXT_SECONDARY };
        let checked = self.checked;
        let disabled = self.disabled;

        div()
            .id(self.id)
            .w(px(40.0))
            .h(px(22.0))
            .rounded(px(11.0))
            .bg(track)
            .border_1()
            .border_color(theme::OUTLINE)
            .p_1()
            .flex()
            .items_center()
            .when(checked, |el| el.justify_end())
            .when(!checked, |el| el.justify_start())
            .cursor_pointer()
            .when(!disabled, |el| {
                el.hover(move |s| s.border_color(theme::ACCENT))
                    .active(move |s| s.opacity(motion::PRESS_OPACITY))
                    .when_some(self.on_change, |el, handler| {
                        el.on_click(move |_, window, cx| handler(!checked, window, cx))
                    })
            })
            .when_some(self.focus, |el, fh| {
                el.track_focus(&fh).focus(move |s| s.border_2().border_color(theme::ACCENT))
            })
            .child(div().size(px(14.0)).rounded_full().bg(knob))
    }
}
