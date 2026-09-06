//! Kleine Bausteine: SectionLabel, StatusBadge, Card, GlassPanel, EmptyState
//! (ui-v2-spec §4).

use gpui::{
    div, px, AnyElement, App, ClickEvent, ElementId, FocusHandle,
    IntoElement, InteractiveElement as _, ParentElement, RenderOnce, SharedString, Styled,
    StatefulInteractiveElement as _, Window,
};

use crate::{icons, theme};

/// Caption-Label (12/500, uppercase) für Gruppen-/Sektionen-Überschriften.
#[derive(Debug)]
#[derive(gpui::IntoElement)]
pub struct SectionLabel {
    text: SharedString,
}

impl SectionLabel {
    pub fn new(text: impl Into<SharedString>) -> Self {
        Self { text: text.into() }
    }
}

impl RenderOnce for SectionLabel {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        div()
            .text_size(px(theme::SIZE_CAPTION))
            .font_weight(gpui::FontWeight(theme::WEIGHT_CAPTION))
            .text_color(theme::TEXT_SECONDARY)
            .child(self.text.to_uppercase())
    }
}

/// Konsolen-/Session-Status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    Ready,
    Standby,
    Offline,
    Unregistered,
}

impl StatusKind {
    /// Spec-Namen (auch als Statusfarben-Schlüssel in theme::status_color).
    pub fn as_str(self) -> &'static str {
        match self {
            StatusKind::Ready => "ready",
            StatusKind::Standby => "standby",
            StatusKind::Offline => "offline",
            StatusKind::Unregistered => "unregistered",
        }
    }

    pub fn color(self) -> gpui::Hsla {
        theme::status_color(self.as_str())
    }
}

/// Status-Badge: Farb-Punkt + Label.
#[derive(Debug)]
#[derive(gpui::IntoElement)]
pub struct StatusBadge {
    status: StatusKind,
    label: SharedString,
}

impl StatusBadge {
    pub fn new(status: StatusKind) -> Self {
        Self { status, label: SharedString::from(status.as_str()) }
    }

    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = label.into();
        self
    }
}

impl RenderOnce for StatusBadge {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let color = self.status.color();
        div()
            .flex()
            .items_center()
            .gap_2()
            .px(px(theme::SP_2))
            .py(px(theme::SP_1))
            .rounded(px(theme::RADIUS_SM))
            .bg(gpui::black().opacity(0.25))
            .border_1()
            .border_color(theme::HAIRLINE)
            .child(div().size(px(8.0)).rounded_full().bg(color))
            .child(
                div()
                    .text_size(px(theme::SIZE_CAPTION))
                    .font_weight(gpui::FontWeight(theme::WEIGHT_CAPTION))
                    .text_color(theme::TEXT_SECONDARY)
                    .child(self.label),
            )
    }
}

/// Flache Karte (surface + hairline Kontur, Radius 10/16).
#[derive(gpui::IntoElement)]
pub struct Card {
    id: Option<ElementId>,
    children: Vec<AnyElement>,
    hero: bool,
    on_click: Option<crate::components::ClickHandler>,
    focus: Option<FocusHandle>,
}

impl Card {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: Some(id.into()),
            children: Vec::new(),
            hero: false,
            on_click: None,
            focus: None,
        }
    }

    /// Hero-Karte: Radius 16 statt 10 (Spec §3).
    pub fn hero(mut self) -> Self {
        self.hero = true;
        self
    }

    pub fn focus_handle(mut self, handle: FocusHandle) -> Self {
        self.focus = Some(handle);
        self
    }

    pub fn on_click(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Box::new(f));
        self
    }
}

impl ParentElement for Card {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl RenderOnce for Card {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let radius = if self.hero { theme::RADIUS_LG } else { theme::RADIUS_MD };
        // id zuerst → Stateful<Div>: alle Interaktions-Methoden verfügbar.
        let card = div()
            .id(self.id.unwrap_or(ElementId::Name("card".into())))
            .flex()
            .flex_col()
            .flex_1()
            .gap_3()
            .rounded(px(radius))
            .bg(theme::SURFACE)
            .border_1()
            .border_color(theme::HAIRLINE)
            .p(px(theme::SP_5))
            .children(self.children);
        let card = match self.on_click {
            Some(handler) => card
                .cursor_pointer()
                .hover(|s| s.bg(theme::SURFACE2))
                .on_click(move |ev, window, cx| handler(ev, window, cx)),
            None => card,
        };
        match self.focus {
            Some(fh) => card.track_focus(&fh).focus(|s| s.border_2().border_color(theme::ACCENT)),
            None => card,
        }
    }
}

/// Glas-Layer (BG @ 78 % + 8 % Kontur) — HUD/Overlay-Panels.
#[derive(gpui::IntoElement)]
pub struct GlassPanel {
    children: Vec<AnyElement>,
}

impl GlassPanel {
    pub fn new() -> Self {
        Self { children: Vec::new() }
    }
}

impl Default for GlassPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl ParentElement for GlassPanel {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl RenderOnce for GlassPanel {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_3()
            .rounded(px(theme::RADIUS_LG))
            .bg(theme::GLASS)
            .border_1()
            .border_color(theme::OUTLINE)
            .p(px(theme::SP_4))
            .children(self.children)
    }
}

/// Leerzustand: Icon + Titel + Beschreibung + optionale Aktion.
#[derive(gpui::IntoElement)]
pub struct EmptyState {
    icon_path: &'static str,
    title: SharedString,
    message: SharedString,
    action: Option<AnyElement>,
}

impl EmptyState {
    pub fn new(
        icon_path: &'static str,
        title: impl Into<SharedString>,
        message: impl Into<SharedString>,
    ) -> Self {
        Self { icon_path, title: title.into(), message: message.into(), action: None }
    }

    /// Optionale Aktion (z. B. ein Button) unter dem Text.
    pub fn action(mut self, element: impl IntoElement) -> Self {
        self.action = Some(element.into_any_element());
        self
    }
}

impl RenderOnce for EmptyState {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .p(px(theme::SP_6))
            .size_full()
            .child(icons::icon(self.icon_path, 40.0, theme::TEXT_DISABLED))
            .child(
                div()
                    .text_size(px(theme::SIZE_HEADLINE))
                    .font_weight(gpui::FontWeight(theme::WEIGHT_HEADLINE))
                    .text_color(theme::TEXT_PRIMARY)
                    .child(self.title),
            )
            .child(
                div()
                    .text_size(px(theme::SIZE_BODY))
                    .text_color(theme::TEXT_SECONDARY)
                    .child(self.message),
            )
            .children(self.action)
    }
}
