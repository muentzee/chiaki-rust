//! TextField (einzeilig) — kontrolliertes Eingabefeld (ui-v2-spec §4).
//!
//! gpui 0.2.2 hat kein TextBox-Element — das Fundament rendert den Text als
//! Label; Tastatureingaben kommen über `on_key_down` auf dem (fokussierten)
//! Feld (backspace löscht, Einzelzeichen werden angehängt). Für PIN-Eingabe
//! und Host-Formulare ausreichend; eine komplette Cursor-Verwaltung kann
//! später ergänzt werden, ohne die API zu ändern.

use gpui::{
    div, prelude::FluentBuilder as _, px, AnyElement, App, ElementId, FocusHandle, IntoElement,
    InteractiveElement as _, KeyDownEvent, ParentElement, RenderOnce, SharedString, Styled,
    StatefulInteractiveElement as _, Window,
};

use crate::theme;

#[derive(gpui::IntoElement)]
pub struct TextField {
    id: ElementId,
    value: SharedString,
    placeholder: SharedString,
    /// zeigt, ob das Feld fokussiert wirken soll (wenn kein Handle gesetzt)
    focused: bool,
    password: bool,
    width_px: f32,
    focus: Option<FocusHandle>,
    on_change: Option<Box<dyn Fn(&SharedString, &mut Window, &mut App) + 'static>>,
}

impl TextField {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            value: "".into(),
            placeholder: "".into(),
            focused: false,
            password: false,
            width_px: 220.0,
            focus: None,
            on_change: None,
        }
    }

    pub fn value(mut self, value: impl Into<SharedString>) -> Self {
        self.value = value.into();
        self
    }

    pub fn placeholder(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    /// Password-Modus: Punkte statt Text.
    pub fn password(mut self) -> Self {
        self.password = true;
        self
    }

    pub fn width(mut self, px_width: f32) -> Self {
        self.width_px = px_width;
        self
    }

    /// gpui-Fokus-Handle: wird `.track_focus()`-ed und per Klick fokussiert.
    /// Der Rahmen folgt `fh.is_focused(window)`.
    pub fn focus_handle(mut self, handle: FocusHandle) -> Self {
        self.focus = Some(handle);
        self
    }

    /// Fokus-Optik ohne Handle (für statische Felder).
    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    pub fn on_change(
        mut self,
        f: impl Fn(&SharedString, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_change = Some(Box::new(f));
        self
    }
}

impl RenderOnce for TextField {
    fn render(self, window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let focused = self
            .focus
            .as_ref()
            .map(|fh| fh.is_focused(window))
            .unwrap_or(self.focused);
        let display: SharedString = if self.password && !self.value.is_empty() {
            "•".repeat(self.value.chars().count()).into()
        } else {
            self.value.clone()
        };
        let value = self.value;
        let on_change = self.on_change;

        // Basis ohne Interaktivität (Div); mit FocusHandle dann zu
        // Stateful<Div> verfeinert (Rückgabe: AnyElement).
        let base = div()
            .flex()
            .items_center()
            // Einzeilig: Überlaufender Wert wird geclippt, statt über die
            // feste Feldbreite hinaus zu malen.
            .overflow_hidden()
            .whitespace_nowrap()
            .w(px(self.width_px))
            .px(px(theme::SP_3))
            .py(px(theme::SP_2))
            .rounded(px(theme::RADIUS_SM))
            .bg(theme::SURFACE2)
            .border_1()
            .border_color(if focused { theme::ACCENT } else { theme::HAIRLINE })
            .text_size(px(theme::SIZE_BODY))
            .text_color(if value.is_empty() {
                theme::TEXT_DISABLED
            } else {
                theme::TEXT_PRIMARY
            })
            .when(value.is_empty(), |el| el.child(self.placeholder.clone()))
            .when(!value.is_empty(), |el| el.child(display));

        let field: AnyElement = match self.focus {
            Some(fh) => base
                .id(self.id.clone())
                .track_focus(&fh)
                .cursor_pointer()
                .on_click(move |_, window, _cx| fh.focus(window))
                .when(focused, |el| el.focus(|s| s.border_color(theme::ACCENT)))
                .on_key_down(move |event: &KeyDownEvent, window, cx| {
                    let key: &str = &event.keystroke.key;
                    let new_value: Option<String> = match key {
                        "backspace" => Some(
                            value
                                .chars()
                                .take(value.chars().count().saturating_sub(1))
                                .collect(),
                        ),
                        "space" => Some(format!("{value} ")),
                        k if k.chars().count() == 1
                            && !event.keystroke.modifiers.control
                            && !event.keystroke.modifiers.alt =>
                        {
                            Some(format!("{value}{k}"))
                        }
                        _ => None,
                    };
                    if let (Some(new_value), Some(cb)) = (new_value, on_change.as_ref()) {
                        let v: SharedString = new_value.into();
                        cb(&v, window, cx);
                    }
                })
                .into_any_element(),
            None => base.into_any_element(),
        };
        field
    }
}
