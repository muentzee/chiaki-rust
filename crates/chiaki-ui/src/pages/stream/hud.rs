//! HUD + Einblend-Panel (ui-v2-spec §2.4 „Stream (HUD)"): ausblendbare
//! Stats-Badges (Bitrate, RTT, Loss, Frame-Time, FPS, Audio-Puffer,
//! Decoder-Backend, Haptics), der NVIDIA-VSR-Badge oben rechts, die
//! optionale Debug-Zeile (`settings/overlay_debug`) und das Einblend-Panel
//! (Taste H bzw. Options-Klick) mit Trennen/Ruhemodus/Mikrofon/Zoom.
//!
//! Badge-Filterung (modular konfigurierbar): der Master
//! `settings/show_stream_stats` blendet die komplette Badge-Reihe aus, je
//! Badge gibt es einen Einzel-Toggle (`settings/overlay_*`, Default an =
//! heutiges Verhalten). Die reine Filterlogik liegt in [`badge_visible`]
//! (getestet); die Flags werden 1×/Frame als [`OverlayConfig`] in den
//! Snapshot übernommen (state::snapshot), damit der Render-Pfad kein
//! Settings-Lock sieht. Die Badge-Reihe bricht bei schmalen Fenstern um
//! (`flex_wrap`), statt zu clippen; sind alle Badges aus, entfällt sie ganz
//! (kein leerer Streifen).

use gpui::{div, px, App, ClickEvent, IntoElement, ParentElement as _, Styled, Window};

use chiaki_settings::settings::Settings;

use crate::components::{Button, ButtonVariant, GlassPanel, IconButton, StatusBadge, StatusKind};
use crate::theme;

use super::state::{HudStats, StreamSnapshot};

// ---------------------------------------------------------------------------
// Badge-Filterung (reine Logik, settings-getrieben)
// ---------------------------------------------------------------------------

/// Die 8 Stats-Badges der Badge-Reihe (VSR-Badge zählt NICHT dazu — er
/// hängt an `settings/show_vsr_badge`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeKind {
    Bitrate,
    Rtt,
    Loss,
    FrameTime,
    Fps,
    Audio,
    Decoder,
    Haptics,
}

/// Alle Badge-Arten in der Anzeigereihenfolge.
pub const BADGE_KINDS: [BadgeKind; 8] = [
    BadgeKind::Bitrate,
    BadgeKind::Rtt,
    BadgeKind::Loss,
    BadgeKind::FrameTime,
    BadgeKind::Fps,
    BadgeKind::Audio,
    BadgeKind::Decoder,
    BadgeKind::Haptics,
];

/// Reine Filterlogik: Ist diese Badge sichtbar?
///
/// Master `settings/show_stream_stats` (aus = gar keine Badges) UND der
/// Einzel-Toggle `settings/overlay_<badge>` (Default an = heutiges
/// Verhalten). Der VSR-Badge läuft separat über `show_vsr_badge`.
pub fn badge_visible(kind: BadgeKind, settings: &Settings) -> bool {
    if !settings.show_stream_stats() {
        return false;
    }
    match kind {
        BadgeKind::Bitrate => settings.overlay_bitrate(),
        BadgeKind::Rtt => settings.overlay_rtt(),
        BadgeKind::Loss => settings.overlay_loss(),
        BadgeKind::FrameTime => settings.overlay_frametime(),
        BadgeKind::Fps => settings.overlay_fps(),
        BadgeKind::Audio => settings.overlay_audio(),
        BadgeKind::Decoder => settings.overlay_decoder(),
        BadgeKind::Haptics => settings.overlay_haptics(),
    }
}

/// Pro-Badge-Sichtbarkeit + Debug-Zeilen-Flag — einmal pro Frame aus den
/// Settings gelesen (Snapshot-Pfad), damit `stats_row` ohne Lock baut.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OverlayConfig {
    pub master: bool,
    pub bitrate: bool,
    pub rtt: bool,
    pub loss: bool,
    pub frame_time: bool,
    pub fps: bool,
    pub audio: bool,
    pub decoder: bool,
    pub haptics: bool,
    pub debug: bool,
}

impl OverlayConfig {
    pub fn from_settings(s: &Settings) -> Self {
        Self {
            master: s.show_stream_stats(),
            bitrate: s.overlay_bitrate(),
            rtt: s.overlay_rtt(),
            loss: s.overlay_loss(),
            frame_time: s.overlay_frametime(),
            fps: s.overlay_fps(),
            audio: s.overlay_audio(),
            decoder: s.overlay_decoder(),
            haptics: s.overlay_haptics(),
            debug: s.overlay_debug(),
        }
    }

    /// Sichtbarkeit eines Badges (gleiche Logik wie [`badge_visible`], nur
    /// auf der pro-Frame-Kopie gearbeitet).
    pub fn badge(&self, kind: BadgeKind) -> bool {
        if !self.master {
            return false;
        }
        match kind {
            BadgeKind::Bitrate => self.bitrate,
            BadgeKind::Rtt => self.rtt,
            BadgeKind::Loss => self.loss,
            BadgeKind::FrameTime => self.frame_time,
            BadgeKind::Fps => self.fps,
            BadgeKind::Audio => self.audio,
            BadgeKind::Decoder => self.decoder,
            BadgeKind::Haptics => self.haptics,
        }
    }

    /// Mindestens eine Badge sichtbar? (aus → Reihe komplett weglassen,
    /// kein leerer Streifen).
    pub fn any_badge(&self) -> bool {
        BADGE_KINDS.iter().any(|&kind| self.badge(kind))
    }
}

/// Eine Stat-Badge (Caption-Label + Wert, dunkler Pill-Hintergrund).
fn stat_badge(label: &str, value: String) -> gpui::AnyElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .px(px(theme::SP_2))
        .py(px(theme::SP_1))
        .rounded(px(theme::RADIUS_SM))
        .bg(gpui::black().opacity(0.35))
        .border_1()
        .border_color(theme::HAIRLINE)
        .child(
            div()
                .text_size(px(theme::SIZE_CAPTION))
                .font_weight(gpui::FontWeight(theme::WEIGHT_CAPTION))
                .text_color(theme::TEXT_SECONDARY)
                .child(label.to_string()),
        )
        .child(
            div()
                // min-width: Werte wie „—“ ↔ „50.8 ms“ sollen die Badge-
                // Reihe nicht jede Sekunde verschieben (Spec: „nichts hüpft“).
                .min_w(px(40.0))
                .text_size(px(theme::SIZE_CAPTION))
                .font_weight(gpui::FontWeight(theme::WEIGHT_CAPTION))
                .text_color(theme::TEXT_PRIMARY)
                .whitespace_nowrap()
                .child(value),
        )
        .into_any_element()
}

/// Format-Helfer: F32 mit einer Nachkommastelle.
fn f1(v: f32) -> String {
    format!("{v:.1}")
}

/// Die Stats-Badge-Zeile (ausblendbar via Taste H / HUD-Toggle), gefiltert
/// nach den Overlay-Einzel-Toggles: Master aus → `None`, alle Einzel-
/// Toggles aus → `None` (kein leerer Streifen). Die Reihe bricht bei
/// schmalen Fenstern um (`flex_wrap`), statt zu clippen.
pub fn stats_row(snap: &StreamSnapshot) -> Option<gpui::AnyElement> {
    let cfg = &snap.overlay;
    if !cfg.any_badge() {
        return None;
    }
    let HudStats { bitrate_mbit, rtt_ms, loss_pct, frame_time_ms, fps, audio_fill_ms, decoder, haptics } =
        &snap.stats;
    let mut badges: Vec<gpui::AnyElement> = Vec::new();
    if cfg.badge(BadgeKind::Bitrate) {
        badges.push(stat_badge(
            "BITRATE",
            if *bitrate_mbit > 0.0 { format!("{f} Mbit/s", f = f1(*bitrate_mbit)) } else { "—".into() },
        ));
    }
    if cfg.badge(BadgeKind::Rtt) {
        badges.push(stat_badge(
            "RTT",
            rtt_ms.map(|v| format!("{f} ms", f = f1(v))).unwrap_or_else(|| "—".into()),
        ));
    }
    if cfg.badge(BadgeKind::Loss) {
        badges.push(stat_badge(
            "LOSS",
            loss_pct.map(|v| format!("{f} %", f = f1(v))).unwrap_or_else(|| "—".into()),
        ));
    }
    if cfg.badge(BadgeKind::FrameTime) {
        badges.push(stat_badge(
            "FRAME-TIME",
            if *frame_time_ms > 0.0 { format!("{f} ms", f = f1(*frame_time_ms)) } else { "—".into() },
        ));
    }
    if cfg.badge(BadgeKind::Fps) {
        badges.push(stat_badge("FPS", if *fps > 0.5 { f1(*fps) } else { "—".into() }));
    }
    if cfg.badge(BadgeKind::Audio) {
        badges.push(stat_badge("AUDIO", format!("{f} ms", f = f1(*audio_fill_ms))));
    }
    if cfg.badge(BadgeKind::Decoder) {
        badges.push(stat_badge("DECODER", decoder.clone()));
    }
    if cfg.badge(BadgeKind::Haptics) {
        badges.push(stat_badge("HAPTICS", haptics.clone()));
    }
    let mut row = div().flex().flex_wrap().gap_2();
    for badge in badges {
        row = row.child(badge);
    }
    Some(row.into_any_element())
}

/// Die optionale Debug-Zeile unterm HUD (settings/overlay_debug):
/// monospaced Caption-Stil, live Werte aus Presenter-/Sink-/Slot-Statistik
/// (Zusammenbau siehe `state::StreamUiState::update_stats`).
pub fn debug_line(text: &str) -> gpui::AnyElement {
    div()
        .min_w_0()
        .max_w_full()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_size(px(theme::SIZE_CAPTION))
        .font_family("Consolas")
        .text_color(theme::TEXT_SECONDARY)
        .child(text.to_string())
        .into_any_element()
}

/// VSR-Badge oben rechts: „● NVIDIA VSR · 2x" (grün) solange VSR aktiv.
pub fn vsr_badge(snap: &StreamSnapshot) -> Option<gpui::AnyElement> {
    if !snap.vsr_active || !snap.vsr_badge_wanted {
        return None;
    }
    let label = if snap.vsr_scale > 100 {
        format!("● NVIDIA VSR · {}x", snap.vsr_scale / 100)
    } else {
        "● NVIDIA VSR".to_string()
    };
    Some(
        div()
            .flex()
            .items_center()
            .px(px(theme::SP_2))
            .py(px(theme::SP_1))
            .rounded(px(theme::RADIUS_SM))
            .bg(gpui::black().opacity(0.35))
            .border_1()
            .border_color(theme::SUCCESS)
            .child(
                div()
                    .text_size(px(theme::SIZE_CAPTION))
                    .font_weight(gpui::FontWeight(theme::WEIGHT_CAPTION))
                    .text_color(theme::SUCCESS)
                    .child(label),
            )
            .into_any_element(),
    )
}

/// Live-Badge + Host-Name oben links.
pub fn live_badge(snap: &StreamSnapshot) -> gpui::AnyElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .min_w_0()
        .child(StatusBadge::new(StatusKind::Ready).label(if snap.fake { "Stream (FAKE)" } else { "Stream" }))
        .child(
            div()
                .flex_1()
                .min_w_0()
                // Lange Host-Namen: eine Zeile, rechts geclippt.
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(px(theme::SIZE_BODY))
                .font_weight(gpui::FontWeight(theme::WEIGHT_HEADLINE))
                .text_color(theme::TEXT_PRIMARY)
                .child(snap.host_label.clone()),
        )
        .into_any_element()
}

/// Das Einblend-Panel (Glas-Layer): Trennen (Confirm), Ruhemodus, Mikrofon,
/// Zoom, Vollbild — plus die Stats als Kompaktblock.
pub fn panel(
    snap: &StreamSnapshot,
    handlers: PanelHandlers,
) -> gpui::AnyElement {
    let mic_label = if snap.mic_unmuted { "Mute microphone" } else { "Enable microphone" };

    div()
        .absolute()
        .top_4()
        .right_4()
        .w(px(320.0))
        .child(
            GlassPanel::new()
                .child(
                    div()
                        .text_size(px(theme::SIZE_HEADLINE))
                        .font_weight(gpui::FontWeight(theme::WEIGHT_HEADLINE))
                        .text_color(theme::TEXT_PRIMARY)
                        .child("Stream panel"),
                )
                .children(stats_row(snap))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            Button::new("panel-disconnect", "Disconnect…")
                                .variant(ButtonVariant::Danger)
                                .full_width()
                                .on_click(handlers.disconnect),
                        )
                        .child(
                            Button::new("panel-gotobed", "Rest mode (console)")
                                .full_width()
                                .on_click(handlers.goto_bed),
                        )
                        .child(
                            Button::new("panel-mic", mic_label)
                                .full_width()
                                .on_click(handlers.mic),
                        )
                        .child(
                            Button::new(
                                "panel-zoom",
                                format!("Scaling: {} (H panel)", snap.zoom.label()),
                            )
                            .full_width()
                            .on_click(handlers.zoom),
                        ),
                )
                .child(
                    div()
                        .text_size(px(theme::SIZE_CAPTION))
                        .text_color(theme::TEXT_DISABLED)
                        // Kurz genug für eine Zeile in der 320-px-Panel-
                        // Breite (sonst hängt ein Solo-„?“ in Zeile 2).
                        .child("H = panel · F11 = fullscreen · Esc = disconnect?"),
                ),
        )
        .into_any_element()
}

/// Callback-Bündel für das Panel (aus `cx.listener(...)`-Closures der Seite).
pub struct PanelHandlers {
    pub disconnect: Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>,
    pub goto_bed: Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>,
    pub mic: Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>,
    pub zoom: Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>,
}

/// Options-IconButton (Panel-Toggle) für die Topbar.
pub fn options_button(
    active: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::AnyElement {
    IconButton::new("stream-options", crate::icons::paths::SETTINGS, "Options (H)")
        .active(active)
        .size(40.0)
        .on_click(on_click)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chiaki_settings::settings::SettingsPaths;

    fn test_settings() -> Settings {
        let base = std::env::temp_dir().join(format!(
            "chiaki-ui-hud-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        Settings::open_at(SettingsPaths {
            settings: base.join("settings.ini"),
            default_settings: base.join("settings.ini"),
            placebo: base.join("placebo_render_params.ini"),
            base,
        })
        .unwrap()
    }

    /// Defaults: Master aus (C++-Default) → gar keine Badges, egal was die
    /// Einzel-Toggles liefern; Master an + Defaults → alle 8 Badges an.
    #[test]
    fn badge_visible_defaults() {
        let s = test_settings();
        // Master-Default (false, wie im C++) schaltet die ganze Reihe aus.
        for kind in BADGE_KINDS {
            assert!(!badge_visible(kind, &s), "{kind:?} muss hinter dem Master hängen");
        }
        let mut s = test_settings();
        s.set_show_stream_stats(true);
        for kind in BADGE_KINDS {
            assert!(badge_visible(kind, &s), "{kind:?} Default muss AN sein");
        }
        assert!(!s.overlay_debug(), "Debug-Zeile Default muss AUS sein");
    }

    /// Einzel-Toggles: pro Badge einer, die anderen bleiben an.
    #[test]
    fn badge_visible_single_toggles() {
        let mut s = test_settings();
        s.set_show_stream_stats(true);
        s.set_overlay_bitrate(false);
        s.set_overlay_fps(false);
        s.set_overlay_debug(true);
        assert!(!badge_visible(BadgeKind::Bitrate, &s));
        assert!(!badge_visible(BadgeKind::Fps, &s));
        for kind in BADGE_KINDS {
            if matches!(kind, BadgeKind::Bitrate | BadgeKind::Fps) {
                continue;
            }
            assert!(badge_visible(kind, &s), "{kind:?} darf nicht mit aus");
        }
        // Master aus → auch eingeschaltete Einzel-Toggles zeigen nichts.
        s.set_show_stream_stats(false);
        assert!(!badge_visible(BadgeKind::Rtt, &s));
    }

    /// OverlayConfig (Snapshot-Kopie) spiegelt badge_visible und liefert
    /// any_badge() = false, wenn alles aus ist (Reihe ganz weglassen).
    #[test]
    fn overlay_config_mirrors_badge_visible() {
        let mut s = test_settings();
        s.set_show_stream_stats(true);
        let cfg = OverlayConfig::from_settings(&s);
        assert!(cfg.any_badge());
        for kind in BADGE_KINDS {
            assert!(cfg.badge(kind));
        }
        assert!(!cfg.debug);

        s.set_overlay_decoder(false);
        s.set_overlay_haptics(false);
        let cfg = OverlayConfig::from_settings(&s);
        assert!(!cfg.badge(BadgeKind::Decoder));
        assert!(!cfg.badge(BadgeKind::Haptics));
        assert!(cfg.any_badge(), "6 Badges noch an → Reihe bleibt");

        // Master aus → any_badge false (auch bei allen Einzel-Toggles an).
        let mut s = test_settings();
        let cfg = OverlayConfig::from_settings(&s);
        assert!(!cfg.any_badge());
        // Debug-Flag unabhängig vom Master.
        s.set_overlay_debug(true);
        let cfg = OverlayConfig::from_settings(&s);
        assert!(cfg.debug);
        assert!(!cfg.any_badge());
    }
}
