//! Design-System der chiaki-ui — 1:1 aus `docs/ui-v2-spec.md` §3 (bindend).
//!
//! Alle Farben als [`gpui::Hsla`]-Konstanten (H/S/L/A je 0..1). Die
//! Hex-Werte der Spec stehen jeweils im Kommentar; die Tests rechnen die
//! Tokens zurück nach RGB-Bytes und prüfen sie gegen die Spec-Hex-Werte
//! (Toleranz ±1/255 durch die f32-Rundung).
//!
//! Namen: SCREAMING_CASE-Konstanten + gleichnamige lowercase-Helper
//! (`ACCENT` / `accent()`), damit Seiten und Komponenten beides nutzen
//! können. KEINE harten Farben in Komponenten/Seiten — immer Tokens!

use gpui::Hsla;

// ---------------------------------------------------------------------------
// Farb-Tokens (ui-v3 Redesign nach UI-Prototype, 09/2026)
// ---------------------------------------------------------------------------

/// Grundton `#070A12` — App-Hintergrund (dunkles Navy).
pub const BG: Hsla = Hsla { h: 0.6212, s: 0.44, l: 0.049, a: 1.0 };
/// Sidebar `#05070D` — noch dunkler als BG.
pub const SIDEBAR: Hsla = Hsla { h: 0.625, s: 0.4447, l: 0.0353, a: 1.0 };
/// Fläche `#0D1120` — Karten.
pub const SURFACE: Hsla = Hsla { h: 0.6316, s: 0.4224, l: 0.0882, a: 1.0 };
/// Fläche 2 `#151B2E` — Hover/Elevated/Input-Felder.
pub const SURFACE2: Hsla = Hsla { h: 0.6267, s: 0.3729, l: 0.1314, a: 1.0 };
/// Akzent `#3D6BFF` (Blau) — primäre Aktionen, Fokusring, aktive Nav-Pills.
pub const ACCENT: Hsla = Hsla { h: 0.6272, s: 1.0, l: 0.6196, a: 1.0 };
/// Akzent hell `#6E9BFF` — Gradient-Ende, weiche Fills, Icon-Tiles.
pub const ACCENT_SOFT: Hsla = Hsla { h: 0.6149, s: 1.0, l: 0.7157, a: 1.0 };
/// Text `#EAECF2`.
pub const TEXT_PRIMARY: Hsla = Hsla { h: 0.625, s: 0.2353, l: 0.9333, a: 1.0 };
/// Text sekundär `#98A1B3`.
pub const TEXT_SECONDARY: Hsla = Hsla { h: 0.6111, s: 0.1508, l: 0.649, a: 1.0 };
/// Text dezent/deaktiviert `#5A6272`.
pub const TEXT_DISABLED: Hsla = Hsla { h: 0.6111, s: 0.1176, l: 0.4, a: 1.0 };
/// Erfolg `#3DD68C`.
pub const SUCCESS: Hsla = Hsla { h: 0.4194, s: 0.6511, l: 0.5392, a: 1.0 };
/// Warnung `#F0B35A`.
pub const WARN: Hsla = Hsla { h: 0.0989, s: 0.8333, l: 0.6471, a: 1.0 };
/// Fehler/Gefahr `#FF6161`.
pub const DANGER: Hsla = Hsla { h: 0.0, s: 1.0, l: 0.6902, a: 1.0 };
/// Info `#7DA7FF`.
pub const INFO: Hsla = Hsla { h: 0.6128, s: 1.0, l: 0.7451, a: 1.0 };

/// Kontur (6 % Weiß) — flache Karten-Trennungen.
pub const HAIRLINE: Hsla = Hsla { h: 0.0, s: 0.0, l: 1.0, a: 0.06 };
/// Kontur (8 % Weiß) — Glas-Layer, Hover-Konturen.
pub const OUTLINE: Hsla = Hsla { h: 0.0, s: 0.0, l: 1.0, a: 0.08 };

/// Glas — BG @ 78 % — HUD-/Overlay-Layer.
pub const GLASS: Hsla = Hsla { h: 0.6212, s: 0.44, l: 0.049, a: 0.78 };

/// Dim-Backdrop für Modals (BG @ 60 %).
pub const BACKDROP: Hsla = Hsla { h: 0.6212, s: 0.44, l: 0.049, a: 0.6 };

// --- abgeleitete Interaktions-Farben (aus den Basis-Tokens) ----------------

/// Akzent beim Hover (+8 % Lightness).
pub fn accent_hover() -> Hsla {
    Hsla { l: (ACCENT.l + 0.08).min(1.0), ..ACCENT }
}
/// Akzent gedrückt (−8 % Lightness).
pub fn accent_pressed() -> Hsla {
    Hsla { l: (ACCENT.l - 0.08).max(0.0), ..ACCENT }
}

// --- lowercase-Helper (Contract-Namen) -------------------------------------

/// [`BG`]
pub fn bg() -> Hsla { BG }
/// [`SIDEBAR`]
pub fn sidebar() -> Hsla { SIDEBAR }
/// [`SURFACE`]
pub fn surface() -> Hsla { SURFACE }
/// [`SURFACE2`]
pub fn surface2() -> Hsla { SURFACE2 }
/// [`ACCENT`]
pub fn accent() -> Hsla { ACCENT }
/// [`ACCENT_SOFT`]
pub fn accent_soft() -> Hsla { ACCENT_SOFT }
/// [`TEXT_PRIMARY`]
pub fn text_primary() -> Hsla { TEXT_PRIMARY }
/// [`TEXT_SECONDARY`]
pub fn text_secondary() -> Hsla { TEXT_SECONDARY }
/// [`TEXT_DISABLED`]
pub fn text_disabled() -> Hsla { TEXT_DISABLED }
/// [`SUCCESS`]
pub fn success() -> Hsla { SUCCESS }
/// [`WARN`]
pub fn warn() -> Hsla { WARN }
/// [`DANGER`]
pub fn danger() -> Hsla { DANGER }
/// [`INFO`]
pub fn info() -> Hsla { INFO }
/// [`HAIRLINE`]
pub fn hairline() -> Hsla { HAIRLINE }
/// [`OUTLINE`]
pub fn outline() -> Hsla { OUTLINE }
/// [`GLASS`]
pub fn glass() -> Hsla { GLASS }
/// [`BACKDROP`]
pub fn backdrop() -> Hsla { BACKDROP }

// ---------------------------------------------------------------------------
// Radius / Spacing / Typo / Motion (ui-v2-spec §3)
// ---------------------------------------------------------------------------

/// Radius klein (Chips, Badges, Inputs).
pub const RADIUS_SM: f32 = 6.0;
/// Radius Standard (Buttons, Karten).
pub const RADIUS_MD: f32 = 10.0;
/// Radius groß (Hero-/Karten-Flächen).
pub const RADIUS_LG: f32 = 16.0;

/// Spacing-Skala (px): 4 / 8 / 12 / 16 / 24 / 32.
pub const SP_1: f32 = 4.0;
pub const SP_2: f32 = 8.0;
pub const SP_3: f32 = 12.0;
pub const SP_4: f32 = 16.0;
pub const SP_5: f32 = 24.0;
pub const SP_6: f32 = 32.0;

/// Typo-Skala: Größen in px.
pub const SIZE_HERO: f32 = 38.0; // 700 — Seiten-Hero ("Home")
pub const SIZE_DISPLAY: f32 = 30.0; // 700
pub const SIZE_TITLE: f32 = 20.0; // 600
pub const SIZE_HEADLINE: f32 = 16.0; // 600
pub const SIZE_BODY: f32 = 14.0; // 400
pub const SIZE_CAPTION: f32 = 12.0; // 500, uppercase

/// Font-Gewichte passend zur Typo-Skala (gpui `FontWeight`-Werte).
pub const WEIGHT_DISPLAY: f32 = 700.0;
pub const WEIGHT_TITLE: f32 = 600.0;
pub const WEIGHT_HEADLINE: f32 = 600.0;
pub const WEIGHT_BODY: f32 = 400.0;
pub const WEIGHT_CAPTION: f32 = 500.0;

/// Fokusring: 2 px Akzent + 4 px Offset (ui-v2-spec §1.3).
pub const FOCUS_RING_WIDTH: f32 = 2.0;
pub const FOCUS_RING_OFFSET: f32 = 4.0;

/// Schriftfamilie: Segoe UI Variable → Inter → System (Windows-Default).
pub const FONT_FAMILY: &str = "Segoe UI Variable";

// ---------------------------------------------------------------------------
// Status-Badge-Farben (Konsolen-Status; von StatusBadge/Home genutzt)
// ---------------------------------------------------------------------------

use gpui::Hsla as H;

/// Farbe eines Konsolen-Statuswerts (ready→success, standby→warn, sonst faint).
pub fn status_color(status: &str) -> H {
    match status {
        "ready" => SUCCESS,
        "standby" => WARN,
        "offline" => TEXT_DISABLED,
        "unregistered" => DANGER,
        _ => TEXT_SECONDARY,
    }
}

// ---------------------------------------------------------------------------
// Tests: Tokentests gegen die Spec-Hex-Werte
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// HSL (0..1) → 8-Bit-RGB (wie in den CSS-Referenzrechnungen).
    fn hsla_to_rgb8(c: Hsla) -> (u8, u8, u8) {
        let h = c.h;
        let s = c.s;
        let l = c.l;
        let hue_to_rgb = |p: f32, q: f32, mut t: f32| -> f32 {
            if t < 0.0 { t += 1.0 }
            if t > 1.0 { t -= 1.0 }
            if t < 1.0 / 6.0 {
                p + (q - p) * 6.0 * t
            } else if t < 0.5 {
                q
            } else if t < 2.0 / 3.0 {
                p + (q - p) * (2.0 / 3.0 - t) * 6.0
            } else {
                p
            }
        };
        let (r, g, b) = if s == 0.0 {
            (l, l, l)
        } else {
            let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
            let p = 2.0 * l - q;
            (hue_to_rgb(p, q, h + 1.0 / 3.0), hue_to_rgb(p, q, h), hue_to_rgb(p, q, h - 1.0 / 3.0))
        };
        (
            (r * 255.0).round() as u8,
            (g * 255.0).round() as u8,
            (b * 255.0).round() as u8,
        )
    }

    fn assert_hex(name: &str, c: Hsla, hex: u32) {
        let expect = (
            (hex >> 16 & 0xFF) as u8,
            (hex >> 8 & 0xFF) as u8,
            (hex & 0xFF) as u8,
        );
        let got = hsla_to_rgb8(c);
        let close = |a: u8, b: u8| (a as i32 - b as i32).abs() <= 1;
        assert!(
            close(got.0, expect.0) && close(got.1, expect.1) && close(got.2, expect.2),
            "{name}: erwartet #{hex:06X} {expect:?}, bekommen {got:?}"
        );
        assert!((c.a - 1.0).abs() < f32::EPSILON, "{name}: Alpha muss 1.0 sein");
    }

    #[test]
    fn farb_tokens_matchen_die_spec() {
        assert_hex("BG", BG, 0x070A12);
        assert_hex("SIDEBAR", SIDEBAR, 0x05070D);
        assert_hex("SURFACE", SURFACE, 0x0D1120);
        assert_hex("SURFACE2", SURFACE2, 0x151B2E);
        assert_hex("ACCENT", ACCENT, 0x3D6BFF);
        assert_hex("ACCENT_SOFT", ACCENT_SOFT, 0x6E9BFF);
        assert_hex("TEXT_PRIMARY", TEXT_PRIMARY, 0xEAECF2);
        assert_hex("TEXT_SECONDARY", TEXT_SECONDARY, 0x98A1B3);
        assert_hex("TEXT_DISABLED", TEXT_DISABLED, 0x5A6272);
        assert_hex("SUCCESS", SUCCESS, 0x3DD68C);
        assert_hex("WARN", WARN, 0xF0B35A);
        assert_hex("DANGER", DANGER, 0xFF6161);
        assert_hex("INFO", INFO, 0x7DA7FF);
    }

    #[test]
    fn glas_und_konturen() {
        assert!((GLASS.a - 0.78).abs() < 1e-6, "Glas = BG @ 78 % Alpha");
        // Glas-Farbe entspricht BG.
        assert!((GLASS.h - BG.h).abs() < 1e-6 && (GLASS.l - BG.l).abs() < 1e-6);
        assert!((HAIRLINE.a - 0.06).abs() < 1e-6, "Kontur 6 % Weiß");
        assert!((OUTLINE.a - 0.08).abs() < 1e-6, "Kontur 8 % Weiß");
    }

    #[test]
    fn helper_liefern_die_konstanten() {
        assert_eq!(accent(), ACCENT);
        assert_eq!(text_primary(), TEXT_PRIMARY);
        assert_eq!(bg(), BG);
        assert_eq!(surface2(), SURFACE2);
        assert_eq!(danger(), DANGER);
    }

    #[test]
    fn abgeleitete_akkent_bleiben_im_bereich() {
        assert!(accent_hover().l > ACCENT.l);
        assert!(accent_pressed().l < ACCENT.l);
    }

    #[test]
    fn status_farben() {
        assert_eq!(status_color("ready"), SUCCESS);
        assert_eq!(status_color("standby"), WARN);
        assert_eq!(status_color("offline"), TEXT_DISABLED);
        assert_eq!(status_color("unregistered"), DANGER);
    }

    #[test]
    fn typo_und_spacing_skala() {
        assert_eq!(SIZE_DISPLAY, 30.0);
        assert_eq!(SIZE_TITLE, 20.0);
        assert_eq!(SIZE_HEADLINE, 16.0);
        assert_eq!(SIZE_BODY, 14.0);
        assert_eq!(SIZE_CAPTION, 12.0);
        assert_eq!((RADIUS_SM, RADIUS_MD, RADIUS_LG), (6.0, 10.0, 16.0));
        assert_eq!((SP_1, SP_2, SP_3, SP_4, SP_5, SP_6), (4.0, 8.0, 12.0, 16.0, 24.0, 32.0));
    }
}
