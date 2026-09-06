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
];

/// gpui-`AssetSource`, die nur die eingebetteten Icons kennt.
pub struct IconAssets;

impl gpui::AssetSource for IconAssets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        Ok(
            ICONS.iter().find(|(p, _)| *p == path).map(|(_, src)| {
                Cow::Borrowed(src.as_bytes())
            }),
        )
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
