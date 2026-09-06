//! Modal-Layer: Dim-Backdrop + zentrierte Card + Dialog-Modell (Spec §3:
//! „Schatten nur für aktive Overlays").
//!
//! Die AppShell hält einen Dialog-Stack (`Vec<Dialog>`); [`ModalLayer`]
//! rendert eine **clonebare Ansicht** ([`DialogView`]) des obersten Dialogs
//! als Overlay und blockiert Maus-Events dahinter (`occlude`). Button-
//! Aktionen werden über Indizes zurückgerufen (`on_button`) — die AppShell
//! kann so Aktionen ausführen und den Dialog schließen, ohne nicht-Clone-
//! Closures durch den Render zu reichen. Esc schließt (AppShell::handle_escape).

use gpui::{
    div, prelude::FluentBuilder as _, px, App, ElementId, IntoElement, InteractiveElement as _,
    ParentElement, RenderOnce, SharedString, StatefulInteractiveElement as _, Styled, Window,
};

use crate::{components::Button, components::ButtonVariant, theme};

/// Beschreibbarer Dialog-Button (Modell im Stack).
pub struct DialogButton {
    pub label: SharedString,
    pub variant: ButtonVariant,
    /// `true`: Button schließt den Dialog automatisch (pop) VOR der Action.
    pub closes: bool,
    /// Action nach (`closes` → pop) — bekommt `(&mut Window, &mut App)`.
    pub action: Option<Box<dyn Fn(&mut Window, &mut App) + 'static>>,
}

impl DialogButton {
    pub fn new(label: impl Into<SharedString>, variant: ButtonVariant) -> Self {
        Self { label: label.into(), variant, closes: true, action: None }
    }

    pub fn action(mut self, f: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.action = Some(Box::new(f));
        self
    }

    pub fn keeps_open(mut self) -> Self {
        self.closes = false;
        self
    }

    fn view(&self) -> DialogViewButton {
        DialogViewButton { label: self.label.clone(), variant: self.variant }
    }
}

/// Clonebare Ansicht eines Dialog-Buttons.
#[derive(Debug, Clone)]
pub struct DialogViewButton {
    pub label: SharedString,
    pub variant: ButtonVariant,
}

/// Clonebare Ansicht eines Dialogs (für den Render-Pfad).
#[derive(Debug, Clone)]
pub struct DialogView {
    pub id: SharedString,
    pub title: SharedString,
    pub body: SharedString,
    pub buttons: Vec<DialogViewButton>,
}

/// Ein Dialog (Modell im AppShell-Stack).
pub struct Dialog {
    pub id: SharedString,
    pub title: SharedString,
    pub body: SharedString,
    pub buttons: Vec<DialogButton>,
}

impl Dialog {
    pub fn new(
        id: impl Into<SharedString>,
        title: impl Into<SharedString>,
        body: impl Into<SharedString>,
    ) -> Self {
        Self { id: id.into(), title: title.into(), body: body.into(), buttons: Vec::new() }
    }

    /// Standard: Abbrechen (Ghost, schließt) + Bestätigen (Primary, schließt).
    pub fn confirm(self) -> Self {
        self.button(DialogButton::new("Abbrechen", ButtonVariant::Ghost))
            .button(DialogButton::new("OK", ButtonVariant::Primary))
    }

    pub fn button(mut self, button: DialogButton) -> Self {
        self.buttons.push(button);
        self
    }

    /// Clonebare Ansicht für den Render-Pfad.
    pub fn view(&self) -> DialogView {
        DialogView {
            id: self.id.clone(),
            title: self.title.clone(),
            body: self.body.clone(),
            buttons: self.buttons.iter().map(|b| b.view()).collect(),
        }
    }
}

/// Overlay für den obersten Dialog des Stacks.
#[derive(gpui::IntoElement)]
pub struct ModalLayer {
    /// Clonebare Ansicht des obersten Dialogs.
    pub view: DialogView,
    /// Button-Index-Callback (0-basiert über `view.buttons`).
    pub on_button: Option<std::rc::Rc<dyn Fn(usize, &mut Window, &mut App) + 'static>>,
    /// Dialog schließen (pop) — Backdrop und `closes`-Buttons.
    pub on_close: Option<std::rc::Rc<dyn Fn(&mut Window, &mut App) + 'static>>,
}

impl ModalLayer {
    pub fn new(view: DialogView) -> Self {
        Self { view, on_button: None, on_close: None }
    }

    pub fn on_button(
        mut self,
        f: impl Fn(usize, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_button = Some(std::rc::Rc::new(f));
        self
    }

    pub fn on_close(mut self, f: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_close = Some(std::rc::Rc::new(f));
        self
    }
}

impl RenderOnce for ModalLayer {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let on_close = self.on_close;
        let on_button = self.on_button;

        div()
            .absolute()
            .inset_0()
            .bg(theme::BACKDROP)
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .id(ElementId::Name(format!("modal-{}", self.view.id).into()))
            .when_some(on_close.clone(), |el, close| {
                el.on_click(move |_, window, cx| {
                    // Nur Klicks auf den Backdrop selbst schließen (kein
                    // Click-Through von der Card — occlude verhindert das).
                    close(window, cx);
                })
            })
            .child(
                div()
                    .w(px(480.0))
                    .max_w(px(640.0))
                    .flex()
                    .flex_col()
                    .gap_4()
                    .rounded(px(theme::RADIUS_LG))
                    .bg(theme::SURFACE)
                    .border_1()
                    .border_color(theme::OUTLINE)
                    .p(px(theme::SP_6))
                    .shadow(vec![gpui::BoxShadow {
                        color: gpui::black().opacity(0.24),
                        offset: gpui::point(px(0.0), px(2.0)),
                        blur_radius: px(24.0),
                        spread_radius: px(0.0),
                    }])
                    .child(
                        div()
                            .text_size(px(theme::SIZE_TITLE))
                            .font_weight(gpui::FontWeight(theme::WEIGHT_TITLE))
                            .text_color(theme::TEXT_PRIMARY)
                            .child(self.view.title),
                    )
                    .child(
                        div()
                            .text_size(px(theme::SIZE_BODY))
                            .text_color(theme::TEXT_SECONDARY)
                            .child(self.view.body),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .mt_2()
                            .children(self.view.buttons.into_iter().enumerate().map(
                                |(i, btn)| {
                                    let close = on_close.clone();
                                    let on_button = on_button.clone();
                                    Button::new(
                                        ElementId::Name(
                                            format!("dialog-btn-{i}-{}", btn.label).into(),
                                        ),
                                        btn.label.clone(),
                                    )
                                    .variant(btn.variant)
                                    .when_some(
                                        // Handler nur, wenn es etwas zu tun gibt.
                                        if on_button.is_some() || on_close.is_some() {
                                            Some(())
                                        } else {
                                            None
                                        },
                                        |b, _| {
                                            b.on_click(move |_, window, cx| {
                                                if let Some(on_button) = on_button.as_ref() {
                                                    on_button(i, window, cx);
                                                } else if let Some(close) = close.as_ref() {
                                                    close(window, cx);
                                                }
                                            })
                                        },
                                    )
                                },
                            )),
                    ),
            )
    }
}
