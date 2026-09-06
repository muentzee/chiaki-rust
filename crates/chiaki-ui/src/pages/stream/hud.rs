//! HUD + Einblend-Panel (ui-v2-spec §2.4 „Stream (HUD)"): ausblendbare
//! Stats-Badges (Bitrate, RTT, Loss, Frame-Time, FPS, Audio-Puffer,
//! Decoder-Backend), der NVIDIA-VSR-Badge oben rechts und das Einblend-Panel
//! (Taste H bzw. Options-Klick) mit Trennen/Ruhemodus/Mikrofon/Zoom.

use gpui::{div, px, App, ClickEvent, IntoElement, ParentElement as _, Styled, Window};

use crate::components::{Button, ButtonVariant, GlassPanel, IconButton, StatusBadge, StatusKind};
use crate::theme;

use super::state::{HudStats, StreamSnapshot};

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

/// Die Stats-Badge-Zeile (ausblendbar via Taste H / HUD-Toggle).
pub fn stats_row(snap: &StreamSnapshot) -> gpui::AnyElement {
    let HudStats { bitrate_mbit, rtt_ms, loss_pct, frame_time_ms, fps, audio_fill_ms, decoder, haptics } =
        &snap.stats;
    div()
        .flex()
        .flex_wrap()
        .gap_2()
        .child(stat_badge(
            "BITRATE",
            if *bitrate_mbit > 0.0 { format!("{f} Mbit/s", f = f1(*bitrate_mbit)) } else { "—".into() },
        ))
        .child(stat_badge(
            "RTT",
            rtt_ms.map(|v| format!("{f} ms", f = f1(v))).unwrap_or_else(|| "—".into()),
        ))
        .child(stat_badge(
            "LOSS",
            loss_pct.map(|v| format!("{f} %", f = f1(v))).unwrap_or_else(|| "—".into()),
        ))
        .child(stat_badge(
            "FRAME-TIME",
            if *frame_time_ms > 0.0 { format!("{f} ms", f = f1(*frame_time_ms)) } else { "—".into() },
        ))
        .child(stat_badge(
            "FPS",
            if *fps > 0.5 { f1(*fps) } else { "—".into() },
        ))
        .child(stat_badge("AUDIO", format!("{f} ms", f = f1(*audio_fill_ms))))
        .child(stat_badge("DECODER", decoder.clone()))
        .child(stat_badge("HAPTICS", haptics.clone()))
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
    let mic_label = if snap.mic_unmuted { "Mikrofon stummschalten" } else { "Mikrofon aktivieren" };

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
                        .child("Stream-Panel"),
                )
                .child(stats_row(snap))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            Button::new("panel-disconnect", "Trennen…")
                                .variant(ButtonVariant::Danger)
                                .full_width()
                                .on_click(handlers.disconnect),
                        )
                        .child(
                            Button::new("panel-gotobed", "Ruhemodus (Konsole)")
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
                                format!("Skalierung: {} (H-Panel)", snap.zoom.label()),
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
                        .child("H = Panel · F11 = Vollbild · Esc = Trennen?"),
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
    IconButton::new("stream-options", crate::icons::paths::SETTINGS, "Optionen (H)")
        .active(active)
        .size(40.0)
        .on_click(on_click)
        .into_any_element()
}
