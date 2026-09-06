//! Button + IconButton (ui-v2-spec §4).
//!
//! Varianten: Primary (Akzent-Fläche), Ghost (transparent + Kontur),
//! Danger (Rot-Fläche). Hover-Feedback über Style-Refinement, Press als
//! Opacity (gpui 0.2.2 kennt keine Div-Transformation — siehe motion.rs).
//! Fokusring: 2 px Akzent + versetzte 4 px Umrandung (Offset über
//! doppelten Rand + Puffer, solange gpui kein `outline-offset` hat).

use gpui::{
    div, prelude::FluentBuilder as _, px, App, ClickEvent, ElementId, FocusHandle, FontWeight,
    Hsla, IntoElement, InteractiveElement as _, ParentElement, RenderOnce, SharedString, Styled,
    StatefulInteractiveElement as _, Window,
};

use crate::{icons, motion, theme};

/// Click-Callback einer Komponente (gpui-nativ).
pub type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// Optik-Variante des Buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonVariant {
    /// Primäre Aktion („Verbinden") — Akzent-Fläche.
    Primary,
    /// Sekundäre Aktion — transparent + Kontur.
    Ghost,
    /// Destruktive Aktion („Vergessen", „Trennen") — Rot-Fläche.
    Danger,
}

/// Interne Farb-Auswahl (pub(crate) für Tests).
pub(crate) fn variant_colors(variant: ButtonVariant, disabled: bool) -> (Hsla, Hsla, Hsla) {
    if disabled {
        return (theme::SURFACE2, theme::TEXT_DISABLED, theme::HAIRLINE);
    }
    match variant {
        ButtonVariant::Primary => (theme::ACCENT, gpui::black(), gpui::black().opacity(0.2)),
        ButtonVariant::Ghost => (gpui::transparent_black(), theme::TEXT_PRIMARY, theme::HAIRLINE),
        ButtonVariant::Danger => (theme::DANGER, gpui::black(), gpui::black().opacity(0.2)),
    }
}

/// Standard-Button (Label).
#[derive(gpui::IntoElement)]
pub struct Button {
    id: ElementId,
    label: SharedString,
    variant: ButtonVariant,
    on_click: Option<ClickHandler>,
    disabled: bool,
    full_width: bool,
    focus: Option<FocusHandle>,
}

impl Button {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            variant: ButtonVariant::Ghost,
            on_click: None,
            disabled: false,
            full_width: false,
            focus: None,
        }
    }

    pub fn variant(mut self, variant: ButtonVariant) -> Self {
        self.variant = variant;
        self
    }

    pub fn on_click(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Box::new(f));
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn full_width(mut self) -> Self {
        self.full_width = true;
        self
    }

    /// FocusHandle für Gamepad/Tastatur-Fokus (Fokusring).
    pub fn focus_handle(mut self, handle: FocusHandle) -> Self {
        self.focus = Some(handle);
        self
    }
}

impl RenderOnce for Button {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let (bg, fg, line) = variant_colors(self.variant, self.disabled);
        let hover_bg = match self.variant {
            ButtonVariant::Primary => theme::accent_hover(),
            ButtonVariant::Ghost => theme::SURFACE2,
            ButtonVariant::Danger => Hsla { l: (theme::DANGER.l + 0.08).min(1.0), ..theme::DANGER },
        };

        div()
            .id(self.id)
            .flex()
            .items_center()
            .justify_center()
            .gap_2()
            // Labels bleiben einzeilig (gpui bricht Text sonst an jeder
            // Grenze um — z. B. „Ruhemodus (Konsole)“ im schmalen HUD-Panel).
            .whitespace_nowrap()
            .rounded(px(theme::RADIUS_MD))
            .px(px(theme::SP_4))
            .py(px(theme::SP_2))
            .min_w(px(96.0))
            .text_size(px(theme::SIZE_BODY))
            .font_weight(FontWeight(theme::WEIGHT_BODY))
            .cursor_pointer()
            .when(self.full_width, |el| el.w_full())
            .when(self.disabled, |el| {
                el.bg(bg)
                    .text_color(fg)
                    .border_1()
                    .border_color(line)
                    .cursor_default()
            })
            .when(!self.disabled, |el| {
                el.bg(bg)
                    .text_color(fg)
                    .border_1()
                    .border_color(line)
                    .hover(move |s| s.bg(hover_bg))
                    .active(move |s| s.opacity(motion::PRESS_OPACITY))
                    .when_some(self.on_click, |el, handler| el.on_click(move |ev, window, cx| {
                        handler(ev, window, cx)
                    }))
            })
            .when_some(self.focus, |el, fh| {
                el.track_focus(&fh).focus(move |s| s.border_2().border_color(theme::ACCENT))
            })
            .child(self.label)
    }
}

/// Quadratischer Icon-Button (NavRail, Toast-Schließen, Kontext-Aktionen).
#[derive(gpui::IntoElement)]
pub struct IconButton {
    id: ElementId,
    icon_path: &'static str,
    label: SharedString,
    active: bool,
    on_click: Option<ClickHandler>,
    focus: Option<FocusHandle>,
    size_px: f32,
}

impl IconButton {
    pub fn new(
        id: impl Into<ElementId>,
        icon_path: &'static str,
        label: impl Into<SharedString>,
    ) -> Self {
        Self {
            id: id.into(),
            icon_path,
            label: label.into(),
            active: false,
            on_click: None,
            focus: None,
            size_px: 56.0,
        }
    }

    /// Aktiver Zustand (Rail-Route angewählt).
    pub fn active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }

    pub fn on_click(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Box::new(f));
        self
    }

    pub fn focus_handle(mut self, handle: FocusHandle) -> Self {
        self.focus = Some(handle);
        self
    }

    /// Kachelgröße überschreiben (Default 56 px).
    pub fn size(mut self, px_size: f32) -> Self {
        self.size_px = px_size;
        self
    }
}

impl RenderOnce for IconButton {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let (bg, tint) = if self.active {
            (Hsla { a: 0.14, ..theme::ACCENT }, theme::TEXT_PRIMARY)
        } else {
            (gpui::transparent_black(), theme::TEXT_SECONDARY)
        };
        let hover_bg = theme::SURFACE2;
        let icon = icons::icon(self.icon_path, 22.0, tint);

        div()
            .id(self.id)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_1()
            .rounded(px(theme::RADIUS_MD))
            .size(px(self.size_px))
            .bg(bg)
            .cursor_pointer()
            .hover(move |s| s.bg(hover_bg))
            .active(move |s| s.opacity(motion::PRESS_OPACITY))
            .when_some(self.on_click, |el, handler| {
                el.on_click(move |ev, window, cx| handler(ev, window, cx))
            })
            .when_some(self.focus, |el, fh| {
                el.track_focus(&fh).focus(move |s| s.border_2().border_color(theme::ACCENT))
            })
            // Tooltip-artiges Label unter dem Icon (Spec: „Keine Icons ohne Label").
            // nowrap: Die feste Kachel (56/40 px) darf das Label nicht
            // mitten im Wort umbrechen („Einstellunge/n", „Optione/n (H)“) —
            // es darf mittig überstehen (overflow ist nicht geclippt).
            .child(icon)
            .child(
                div()
                    .text_size(px(10.0))
                    .font_weight(FontWeight(theme::WEIGHT_CAPTION))
                    .text_color(tint)
                    .whitespace_nowrap()
                    .child(self.label),
            )
    }
}
