//! Eingebettete SVG-Icons + gpui-`AssetSource`.
//!
//! gpui 0.2.2 rendert `svg()`-Elemente über den `SvgRenderer`, der Pfade
//! über die `AssetSource` der Anwendung auflöst. [`IconAssets`] liefert die
//! hier eingebetteten Feather/Lucide-artigen 24×24-Stroke-Icons als Bytes;
//! die Tintenfarbe kommt aus `.text_color(...)` am svg-Element (gpui rendert
//! die Alpha-Maske in der Textfarbe).
//!
//! Neue Icons: hier eine `include_str!`-freie Konstante ergänzen und in
//! [`ICONS`] registrieren. Pfade immer über [`paths`] referenzieren.

use gpui::{svg, Svg, Styled as _};
use std::borrow::Cow;

/// Icon-Pfade (Schlüssel der Asset-Quelle).
pub mod paths {
    pub const HOME: &str = "icons/home.svg";
    pub const CONSOLE: &str = "icons/console.svg";
    pub const SETTINGS: &str = "icons/settings.svg";
    pub const INFO: &str = "icons/info.svg";
    pub const POWER: &str = "icons/power.svg";
    pub const PLAY: &str = "icons/play.svg";
    pub const CLOSE: &str = "icons/close.svg";
    pub const CHEVRON_DOWN: &str = "icons/chevron-down.svg";
    pub const CHECK: &str = "icons/check.svg";
    pub const SEARCH: &str = "icons/search.svg";
    pub const GLOBE: &str = "icons/globe.svg";
    pub const EDIT: &str = "icons/edit.svg";
    pub const TRASH: &str = "icons/trash.svg";
    pub const PULSE: &str = "icons/pulse.svg";
    pub const LINK: &str = "icons/link.svg";
    pub const USER_PLUS: &str = "icons/user-plus.svg";
    pub const NETWORK: &str = "icons/network.svg";
    pub const MAP_PIN: &str = "icons/map-pin.svg";
    pub const CLOCK: &str = "icons/clock.svg";
    pub const LIGHTBULB: &str = "icons/lightbulb.svg";
    pub const CHEVRON_RIGHT: &str = "icons/chevron-right.svg";
    pub const MORE: &str = "icons/more-horizontal.svg";
}

const ICON_HOME: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 9l9-7 9 7v11a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/><polyline points="9 22 9 12 15 12 15 22"/></svg>"##;
const ICON_CONSOLE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="6" x2="10" y1="11" y2="11"/><line x1="8" x2="8" y1="9" y2="13"/><line x1="15" x2="15.01" y1="12" y2="12"/><line x1="18" x2="18.01" y1="10" y2="10"/><path d="M17.32 5H6.68a4 4 0 0 0-3.978 3.59c-.006.052-.01.101-.017.152C2.604 9.416 2 14.456 2 16a3 3 0 0 0 3 3c1 0 1.5-.5 2-1l1.414-1.414A2 2 0 0 1 9.828 16h4.344a2 2 0 0 1 1.414.586L17 18c.5.5 1 1 2 1a3 3 0 0 0 3-3c0-1.545-.604-6.584-.685-7.258-.007-.05-.011-.1-.017-.151A4 4 0 0 0 17.32 5z"/></svg>"##;
const ICON_SETTINGS: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z"/><circle cx="12" cy="12" r="3"/></svg>"##;
const ICON_INFO: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><path d="M12 16v-4"/><path d="M12 8h.01"/></svg>"##;
const ICON_POWER: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M18.36 6.64a9 9 0 1 1-12.73 0"/><line x1="12" x2="12" y1="2" y2="12"/></svg>"##;
const ICON_PLAY: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polygon points="5 3 19 12 5 21 5 3"/></svg>"##;
const ICON_CLOSE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M18 6 6 18"/><path d="m6 6 12 12"/></svg>"##;
const ICON_CHEVRON_DOWN: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m6 9 6 6 6-6"/></svg>"##;
const ICON_CHECK: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5"/></svg>"##;
const ICON_SEARCH: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3"/></svg>"##;
const ICON_GLOBE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><path d="M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z"/><path d="M2 12h20"/></svg>"##;
const ICON_EDIT: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M17 3a2.85 2.83 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5Z"/></svg>"##;
const ICON_TRASH: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 6h18"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6"/><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/></svg>"##;
const ICON_PULSE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="22 12 18 12 15 21 9 3 6 12 2 12"/></svg>"##;
const ICON_LINK: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71"/><path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71"/></svg>"##;
const ICON_USER_PLUS: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><line x1="19" x2="19" y1="8" y2="14"/><line x1="22" x2="16" y1="11" y2="11"/></svg>"##;
const ICON_NETWORK: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="16" y="16" width="6" height="6" rx="1"/><rect x="2" y="16" width="6" height="6" rx="1"/><rect x="9" y="2" width="6" height="6" rx="1"/><path d="M5 16v-3a1 1 0 0 1 1-1h12a1 1 0 0 1 1 1v3"/><line x1="12" x2="12" y1="8" y2="12"/></svg>"##;
const ICON_MAP_PIN: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M20 10c0 6-8 12-8 12s-8-6-8-12a8 8 0 0 1 16 0Z"/><circle cx="12" cy="10" r="3"/></svg>"##;
const ICON_CLOCK: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><polyline points="12 6 12 12 16 14"/></svg>"##;
const ICON_LIGHTBULB: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M15 14c.2-1 .7-1.7 1.5-2.5 1-.9 1.5-2.2 1.5-3.5A6 6 0 0 0 6 8c0 1 .2 2.2 1.5 3.5.7.7 1.3 1.5 1.5 2.5"/><path d="M9 18h6"/><path d="M10 22h4"/></svg>"##;
const ICON_CHEVRON_RIGHT: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m9 18 6-6-6-6"/></svg>"##;
const ICON_MORE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="1"/><circle cx="19" cy="12" r="1"/><circle cx="5" cy="12" r="1"/></svg>"##;

/// Alle Icons: Pfad → SVG-Quelltext.
pub static ICONS: &[(&str, &str)] = &[
    (paths::HOME, ICON_HOME),
    (paths::CONSOLE, ICON_CONSOLE),
    (paths::SETTINGS, ICON_SETTINGS),
    (paths::INFO, ICON_INFO),
    (paths::POWER, ICON_POWER),
    (paths::PLAY, ICON_PLAY),
    (paths::CLOSE, ICON_CLOSE),
    (paths::CHEVRON_DOWN, ICON_CHEVRON_DOWN),
    (paths::CHECK, ICON_CHECK),
    (paths::SEARCH, ICON_SEARCH),
    (paths::GLOBE, ICON_GLOBE),
    (paths::EDIT, ICON_EDIT),
    (paths::TRASH, ICON_TRASH),
    (paths::PULSE, ICON_PULSE),
    (paths::LINK, ICON_LINK),
    (paths::USER_PLUS, ICON_USER_PLUS),
    (paths::NETWORK, ICON_NETWORK),
    (paths::MAP_PIN, ICON_MAP_PIN),
    (paths::CLOCK, ICON_CLOCK),
    (paths::LIGHTBULB, ICON_LIGHTBULB),
    (paths::CHEVRON_RIGHT, ICON_CHEVRON_RIGHT),
    (paths::MORE, ICON_MORE),
];

/// Eingebettete Raster-Bilder (Prototyp-Assets): Pfad → PNG-Bytes.
pub mod image_paths {
    pub const BG_SWOOSH: &str = "images/bg-swoosh.png";
    pub const PS5: &str = "images/ps5.png";
    pub const LOGO: &str = "images/logo.png";
}

static IMAGE_BG_SWOOSH: &[u8] = include_bytes!("../assets/bg-swoosh.png");
static IMAGE_PS5: &[u8] = include_bytes!("../assets/ps5.png");
static IMAGE_LOGO: &[u8] = include_bytes!("../assets/logo.png");

/// Alle Raster-Bilder: Pfad → PNG-Bytes.
pub static IMAGES: &[(&str, &[u8])] = &[
    (image_paths::BG_SWOOSH, IMAGE_BG_SWOOSH),
    (image_paths::PS5, IMAGE_PS5),
    (image_paths::LOGO, IMAGE_LOGO),
];

/// gpui-`AssetSource`, die nur die eingebetteten Icons kennt.
pub struct IconAssets;

impl gpui::AssetSource for IconAssets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, src)) = ICONS.iter().find(|(p, _)| *p == path) {
            return Ok(Some(Cow::Borrowed(src.as_bytes())));
        }
        if let Some((_, bytes)) = IMAGES.iter().find(|(p, _)| *p == path) {
            return Ok(Some(Cow::Borrowed(*bytes)));
        }
        Ok(None)
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<gpui::SharedString>> {
        Ok(ICONS
            .iter()
            .filter(|(p, _)| p.starts_with(path))
            .map(|(p, _)| gpui::SharedString::from(*p))
            .collect())
    }
}

/// Ein SVG-Icon mit Größe und Farbe (Textfarbe wird zur Tinte).
pub fn icon(path: &'static str, size_px: f32, color: gpui::Hsla) -> Svg {
    svg()
        .path(path)
        .w(gpui::px(size_px))
        .h(gpui::px(size_px))
        .flex_shrink_0()
        .text_color(color)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AssetSource as _;

    #[test]
    fn alle_icons_sind_gueltige_eintraege() {
        assert!(!ICONS.is_empty());
        for (path, src) in ICONS {
            assert!(path.starts_with("icons/") && path.ends_with(".svg"), "{path}");
            assert!(src.contains("<svg"), "{path}: kein SVG-Markup");
            assert!(src.contains("viewBox"), "{path}: kein viewBox");
        }
        // Duplikate?
        let mut seen = std::collections::HashSet::new();
        for (path, _) in ICONS {
            assert!(seen.insert(*path), "Duplikat: {path}");
        }
    }

    #[test]
    fn asset_source_laedt_icons_und_unbekannte_leer() {
        let assets = IconAssets;
        let home = assets.load(paths::HOME).unwrap().expect("home.svg muss existieren");
        assert!(home.starts_with(b"<svg"));
        assert!(assets.load("icons/gibt-es-nicht.svg").unwrap().is_none());
        let listed = assets.list("icons/").unwrap();
        assert_eq!(listed.len(), ICONS.len());
    }
}
