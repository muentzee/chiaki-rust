//! Dünne gpui-Komponenten der chiaki-ui (ui-v2-spec §4, neu gebaut).
//!
//! **Entscheidung gpui-component:** gpui-component 0.5.1 wäre semver-
//! kompatibel (deps: `gpui ^0.2.2`), wird aber NICHT verwendet:
//! 1. Die ui-v2-spec §4 schreibt „Komponenten (neu bauen, keine
//!    Wiederverwendung)" vor.
//! 2. gpui-component bringt ein eigenes Theme-Singleton mit, das mit den
//!    bindenden Tokens aus ui-v2-spec §3 kollidieren würde.
//! 3. Kleiner Dependency-Baum + stabile, eigene Contract-API.
//!
//! Alle Komponenten sind `RenderOnce`-Structs mit Builder-API, nutzen
//! ausschließlich [`crate::theme`]-Tokens und feuern gpui-nativ
//! `Fn(&ClickEvent, &mut Window, &mut App)`-Callbacks (in Seiten via
//! `cx.listener(...)` erzeugt).

mod button;
mod misc;
mod modal;
mod select;
mod slider;
mod text_field;
mod toast;
mod toggle;

pub use button::{Button, ButtonVariant, ClickHandler, IconButton};
pub use misc::{Card, EmptyState, GlassPanel, SectionLabel, StatusBadge, StatusKind};
pub use modal::{Dialog, DialogButton, DialogView, ModalLayer};
pub use select::{Select, SelectOption};
pub use slider::Slider;
pub use text_field::TextField;
pub use toast::{ToastData, ToastId, ToastKind, ToastView};
pub use toggle::Toggle;

#[cfg(test)]
pub(crate) mod test_support {
    /// Farb-Logik der Button-Varianten (aus button.rs, für Tokentests).
    pub fn variant_colors(
        variant: ButtonVariant,
        disabled: bool,
    ) -> (gpui::Hsla, gpui::Hsla, gpui::Hsla) {
        super::button::variant_colors(variant, disabled)
    }

    use super::ButtonVariant;
}
