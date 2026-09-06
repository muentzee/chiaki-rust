//! Slider (SettingsRow-Control, ui-v2-spec §4).
//!
//! Kontrolliert: `value` gehört dem Parent, `on_change` liefert den neuen
//! Wert. Bedienung per Klick/Ziehen auf der Spur: die Track-Bounds werden
//! über ein `canvas()`-Element ermittelt (prepaint liefert Bounds), der
//! Wert aus der Maus-X-Position relativ dazu berechnet.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    canvas, div, prelude::FluentBuilder as _, px, App, Bounds, ElementId, FocusHandle, IntoElement,
    InteractiveElement as _, ParentElement, Pixels, RenderOnce, Styled, Window,
};

use crate::theme;

/// Auf Steps runden + clampen (auch von Tests geprüft).
fn snap_value(value: f64, min: f64, max: f64, step: f64) -> f64 {
    let steps = ((value - min) / step).round();
    (min + steps * step).clamp(min, max)
}

#[derive(gpui::IntoElement)]
pub struct Slider {
    id: ElementId,
    value: f64,
    min: f64,
    max: f64,
    step: f64,
    disabled: bool,
    focus: Option<FocusHandle>,
    on_change: Option<Box<dyn Fn(f64, &mut Window, &mut App) + 'static>>,
}

impl Slider {
    pub fn new(id: impl Into<ElementId>, value: f64, min: f64, max: f64) -> Self {
        Self {
            id: id.into(),
            value,
            min,
            max,
            step: 1.0,
            disabled: false,
            focus: None,
            on_change: None,
        }
    }

    pub fn step(mut self, step: f64) -> Self {
        self.step = step.max(0.0001);
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn focus_handle(mut self, handle: FocusHandle) -> Self {
        self.focus = Some(handle);
        self
    }

    pub fn on_change(mut self, f: impl Fn(f64, &mut Window, &mut App) + 'static) -> Self {
        self.on_change = Some(Box::new(f));
        self
    }

    fn fraction(&self) -> f32 {
        if self.max <= self.min {
            return 0.0;
        }
        let v = self.value.clamp(self.min, self.max);
        ((v - self.min) / (self.max - self.min)) as f32
    }

    fn snap(&self, value: f64) -> f64 {
        snap_value(value, self.min, self.max, self.step)
    }
}

impl RenderOnce for Slider {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let fraction = self.fraction();
        let track_w = 220.0_f32;
        let bounds: Rc<Cell<Option<Bounds<Pixels>>>> = Rc::new(Cell::new(None));
        let bounds_for_handler = Rc::clone(&bounds);
        let min = self.min;
        let max = self.max;
        let step = self.step;
        let on_change = self.on_change;
        let disabled = self.disabled;
        let knob_px = (fraction * track_w).min(track_w - 12.0);

        div()
            .id(self.id)
            .flex()
            .items_center()
            .w(px(track_w + 16.0))
            .h(px(24.0))
            .when_some(self.focus, |el, fh| {
                el.track_focus(&fh).focus(move |s| s.border_2().border_color(theme::ACCENT))
            })
            .child(
                div()
                    .relative()
                    .w(px(track_w))
                    .h(px(6.0))
                    .rounded_full()
                    .bg(theme::SURFACE2)
                    .border_1()
                    .border_color(theme::HAIRLINE)
                    .when(!disabled, |el| {
                        el.cursor_pointer()
                            .on_mouse_down(gpui::MouseButton::Left, move |event, window, cx| {
                                if let Some(bounds) = bounds_for_handler.get() {
                                    let dx = f32::from(event.position.x - bounds.origin.x);
                                    let width = f32::from(bounds.size.width);
                                    let rel = if width > 0.0 { dx / width } else { 0.0 };
                                    let raw = (min as f32 + rel * (max - min) as f32) as f64;
                                    let value = snap_value(raw, min, max, step);
                                    if let Some(cb) = on_change.as_ref() {
                                        cb(value, window, cx);
                                    }
                                }
                            })
                    })
                    // Track-Bounds über canvas bestimmen (prepaint).
                    .child(canvas(
                        {
                            let bounds = Rc::clone(&bounds);
                            move |b: Bounds<Pixels>, _window, _cx| {
                                bounds.set(Some(b));
                            }
                        },
                        |_bounds, _state: (), _window, _cx| {},
                    ))
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .top_0()
                            .h_full()
                            .w(px(fraction * track_w))
                            .rounded_full()
                            .bg(if disabled { theme::TEXT_DISABLED } else { theme::ACCENT }),
                    )
                    .child(
                        div()
                            .absolute()
                            .left(px(knob_px))
                            .top(px(-5.0))
                            .size(px(16.0))
                            .rounded_full()
                            .bg(if disabled { theme::TEXT_DISABLED } else { gpui::white() })
                            .border_2()
                            .border_color(if disabled { theme::SURFACE } else { theme::ACCENT }),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::snap_value;

    #[test]
    fn snap_rundet_auf_steps_und_clamped() {
        assert_eq!(snap_value(12.4, 0.0, 50.0, 1.0), 12.0);
        assert_eq!(snap_value(12.6, 0.0, 50.0, 1.0), 13.0);
        assert_eq!(snap_value(200.0, 0.0, 50.0, 5.0), 50.0);
        assert_eq!(snap_value(-10.0, 5.0, 50.0, 5.0), 5.0);
        assert_eq!(snap_value(7.0, 5.0, 50.0, 2.5), 7.5);
    }
}
