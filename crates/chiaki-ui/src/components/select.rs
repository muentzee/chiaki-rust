//! Select (Dropdown) — kontrolliertes Popup (ui-v2-spec §4 SettingsRow).
//!
//! Das Popup ist **kontrolliert**: `open` + `on_open_change` gehören dem
//! Parent (Seite/SettingsSection). Die Option-Liste wird als absolutes
//! Overlay unter dem Feld gerendert (kein zweites gpui-Fenster). Für das
//! Fundament ausreichend; ein `anchored`-Popup kann später gegengetauscht
//! werden, ohne die API zu ändern.

use gpui::{
    div, prelude::FluentBuilder as _, px, App, ClickEvent, ElementId, FocusHandle, IntoElement,
    InteractiveElement as _, ParentElement, RenderOnce, SharedString, Styled,
    StatefulInteractiveElement as _, Window,
};

use crate::{icons, motion, theme};

/// Eine Auswahloption (Value + Label).
#[derive(Debug, Clone)]
pub struct SelectOption {
    pub value: SharedString,
    pub label: SharedString,
}

impl SelectOption {
    pub fn new(value: impl Into<SharedString>, label: impl Into<SharedString>) -> Self {
        Self { value: value.into(), label: label.into() }
    }
}

/// Dropdown-Auswahl.
#[derive(gpui::IntoElement)]
pub struct Select {
    id: ElementId,
    options: Vec<SelectOption>,
    selected: Option<SharedString>,
    placeholder: SharedString,
    open: bool,
    width_px: f32,
    disabled: bool,
    focus: Option<FocusHandle>,
    on_select: Option<std::rc::Rc<dyn Fn(&SharedString, &mut Window, &mut App) + 'static>>,
    on_open_change: Option<std::rc::Rc<dyn Fn(bool, &mut Window, &mut App) + 'static>>,
}

impl Select {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            options: Vec::new(),
            selected: None,
            placeholder: "Auswählen".into(),
            open: false,
            width_px: 220.0,
            disabled: false,
            focus: None,
            on_select: None,
            on_open_change: None,
        }
    }

    pub fn options(mut self, options: Vec<SelectOption>) -> Self {
        self.options = options;
        self
    }

    pub fn selected(mut self, value: impl Into<SharedString>) -> Self {
        self.selected = Some(value.into());
        self
    }

    pub fn placeholder(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    pub fn open(mut self, open: bool) -> Self {
        self.open = open;
        self
    }

    pub fn width(mut self, px_width: f32) -> Self {
        self.width_px = px_width;
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

    pub fn on_select(
        mut self,
        f: impl Fn(&SharedString, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_select = Some(std::rc::Rc::new(f));
        self
    }

    pub fn on_open_change(mut self, f: impl Fn(bool, &mut Window, &mut App) + 'static) -> Self {
        self.on_open_change = Some(std::rc::Rc::new(f));
        self
    }

    fn label_of(&self) -> SharedString {
        let selected: Option<&str> = self.selected.as_ref().map(|s| &***s);
        self.options
            .iter()
            .find(|o| Some(&**o.value) == selected)
            .map(|o| o.label.clone())
            .unwrap_or_else(|| self.placeholder.clone())
    }
}

impl RenderOnce for Select {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let label = self.label_of();
        let width = self.width_px;
        let open = self.open && !self.disabled;
        let on_open_change = self.on_open_change;
        let on_open_change_trigger = on_open_change.clone();
        let on_select = self.on_select;
        let options = self.options;

        div()
            .id(self.id)
            .relative()
            .when_some(self.focus, |el, fh| {
                el.track_focus(&fh).focus(move |s| s.border_2().border_color(theme::ACCENT))
            })
            .child(
                // Trigger
                div()
                    .id("trigger")
                    .flex()
                    .items_center()
                    .justify_between()
                    .w(px(width))
                    .px(px(theme::SP_3))
                    .py(px(theme::SP_2))
                    .rounded(px(theme::RADIUS_SM))
                    .bg(theme::SURFACE2)
                    .border_1()
                    .border_color(if open { theme::ACCENT } else { theme::HAIRLINE })
                    .text_color(if self.disabled {
                        theme::TEXT_DISABLED
                    } else {
                        theme::TEXT_PRIMARY
                    })
                    .text_size(px(theme::SIZE_BODY))
                    .cursor_pointer()
                    .when(!self.disabled, |el| {
                        el.hover(move |s| s.border_color(theme::ACCENT))
                            .active(move |s| s.opacity(motion::PRESS_OPACITY))
                            .on_click(move |_, window, cx| {
                                if let Some(cb) = on_open_change_trigger.as_ref() {
                                    cb(!open, window, cx);
                                }
                            })
                    })
                    .child(label)
                    .child(icons::icon(
                        icons::paths::CHEVRON_DOWN,
                        14.0,
                        theme::TEXT_SECONDARY,
                    )),
            )
            .when(open, |el| {
                el.child(
                    div()
                        .id("select-popup")
                        .absolute()
                        .top_full()
                        .mt_1()
                        .w(px(width))
                        .max_h(px(320.0))
                        .overflow_y_scroll()
                        .rounded(px(theme::RADIUS_MD))
                        .bg(theme::SURFACE)
                        .border_1()
                        .border_color(theme::OUTLINE)
                        .shadow(vec![gpui::BoxShadow {
                            color: gpui::black().opacity(0.24),
                            offset: gpui::point(px(0.0), px(2.0)),
                            blur_radius: px(16.0),
                            spread_radius: px(0.0),
                        }])
                        .py_1()
                        .children(options.into_iter().enumerate().map(|(i, opt)| {
                            let selected_str: Option<&str> =
                                self.selected.as_ref().map(|s| &***s);
                            let is_selected = selected_str == Some(&*opt.value);
                            let on_select = on_select.clone();
                            let on_open_change = on_open_change.clone();
                            let option_id = ElementId::Name(
                                format!("opt-{i}-{}", opt.value).into(),
                            );
                            div()
                                .id(option_id)
                                .flex()
                                .items_center()
                                .justify_between()
                                .px(px(theme::SP_3))
                                .py(px(theme::SP_2))
                                .text_size(px(theme::SIZE_BODY))
                                .text_color(if is_selected {
                                    theme::TEXT_PRIMARY
                                } else {
                                    theme::TEXT_SECONDARY
                                })
                                .bg(if is_selected {
                                    gpui::black().opacity(0.25)
                                } else {
                                    gpui::transparent_black()
                                })
                                .cursor_pointer()
                                .hover(move |s| s.bg(theme::SURFACE2))
                                .on_click(move |_: &ClickEvent, window, cx| {
                                    if let Some(cb) = on_select.as_ref() {
                                        cb(&opt.value, window, cx);
                                    }
                                    if let Some(cb) = on_open_change.as_ref() {
                                        cb(false, window, cx);
                                    }
                                })
                                .child(opt.label.clone())
                                .when(is_selected, |row| {
                                    row.child(icons::icon(
                                        icons::paths::CHECK,
                                        14.0,
                                        theme::ACCENT,
                                    ))
                                })
                        })),
                )
            })
    }
}
