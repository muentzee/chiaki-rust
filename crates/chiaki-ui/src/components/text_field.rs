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

        // Fokus-Klick und Tasteneingaben rufen bewusst `window.refresh()` auf:
        // gpui rendert nur auf explizites Notify, und das TextField ist
        // RenderOnce ohne eigenes Entity. Ohne Refresh blieben Fokus-Rahmen und
        // neuer Wert unsichtbar, bis irgendein anderes Element zufällig ein
        // Notify auslöst (Root Cause des toten Tippens im Registrierungs-
        // Wizard, 11/2026).
        let field: AnyElement = match self.focus {
            Some(fh) => {
                let id_for_click = self.id.clone();
                let id_for_key = self.id.clone();
                base
                .id(self.id.clone())
                .track_focus(&fh)
                .cursor_pointer()
                .on_click(move |_, window, _cx| {
                    tracing::debug!("TextField {id_for_click} click → Fokus gesetzt");
                    fh.focus(window);
                    window.refresh();
                })
                .when(focused, |el| el.focus(|s| s.border_color(theme::ACCENT)))
                .on_key_down(move |event: &KeyDownEvent, window, cx| {
                    let key: &str = &event.keystroke.key;
                    tracing::debug!("TextField {id_for_key} key '{key}' (value={value})");
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
                        // Wert-Änderung sofort sichtbar machen (siehe oben):
                        // die on_change-Bridges laufen über WeakEntity-Updates
                        // ohne garantiertes Notify — der Refresh rendert den
                        // neuen Zustand der Shell in jedem Fall neu.
                        window.refresh();
                    }
                })
                .into_any_element()
            }
            None => base.into_any_element(),
        };
        field
    }
}

// ---------------------------------------------------------------------------
// Tests (gpui-Testsimulation — Regression gegen den toten Feld-Bug 11/2026:
// ohne Fokus-Pfad-Dispatch und Refresh blieb jedes Feld wert- und fokuslos)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Context, Keystroke, Modifiers, Render, TestAppContext};
    use std::sync::{Arc, Mutex};

    /// Host-View: hält Wert + Fokus-Handle und rendert ein TextField; jede
    /// on_change-Ausgabe landet im Shared-Vec (Simulation der Shell-Bridge).
    struct FieldHost {
        fh: FocusHandle,
        values: Arc<Mutex<Vec<String>>>,
    }

    impl Render for FieldHost {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let _ = cx;
            let values = self.values.clone();
            div().child(
                TextField::new("regression-field")
                    .value("255.255.255.255")
                    .focus_handle(self.fh.clone())
                    .on_change(move |v, _w, _a| {
                        values
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(v.to_string());
                    }),
            )
        }
    }

    fn key(k: char) -> Keystroke {
        Keystroke {
            key: k.to_string(),
            modifiers: Modifiers::default(),
            key_char: Some(k.to_string()),
        }
    }

    #[gpui::test]
    fn focused_field_receives_keystrokes(cx: &mut TestAppContext) {
        let values: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let values_for_view = values.clone();
        let (view, vcx) =
            cx.add_window_view(|_window, cx| FieldHost {
                fh: cx.focus_handle(),
                values: values_for_view,
            });

        // Fokus setzen (was der Klick-Handler nach dem Refresh-Fix tut).
        vcx.update(|window, cx| {
            let fh = view.read(cx).fh.clone();
            fh.focus(window);
        });
        vcx.run_until_parked();

        // Drei Tasten dispatchen (echter gpui-Dispatch-Pfad über den Fokus).
        vcx.update(|window, cx| {
            for k in ['1', '9', '2'] {
                window.dispatch_keystroke(key(k), cx);
            }
        });

        let got = values.lock().unwrap_or_else(|e| e.into_inner()).clone();
        // on_change feuert pro Taste mit dem appendierten Wert (stale-Value
        // aus dem letzten Render ist erwartetes TextField-Verhalten — die
        // Shell-Bridge schreibt den Wert und das Refresh re-rendert).
        assert_eq!(
            got,
            vec![
                "255.255.255.2551",
                "255.255.255.2559",
                "255.255.255.2552",
            ],
            "focused field must receive every keystroke via on_key_down"
        );
    }

    #[gpui::test]
    fn unfocused_field_ignores_keystrokes(cx: &mut TestAppContext) {
        let values: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let values_for_view = values.clone();
        let (_view, vcx) =
            cx.add_window_view(|_window, cx| FieldHost {
                fh: cx.focus_handle(),
                values: values_for_view,
            });

        vcx.update(|window, cx| {
            window.dispatch_keystroke(key('5'), cx);
        });

        let got = values.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(
            got.is_empty(),
            "unfocused field must not receive keystrokes, got {got:?}"
        );
    }
}
