//! Stream (ui-v2-spec §2.4): Connecting-Sequence (4 Status-Stationen) +
//! Video-Fläche (VideoPresenter-Pfad) + HUD-Gerüst. GERÜST — der Stream-
//! Agent füllt Decode-Pipeline (chiaki_media::Decoder → NV12 → optional VSR)
//! und HUD-Stats aus.

use gpui::{div, px, Context, IntoElement, ParentElement as _, Styled, Window};

use crate::backend::HostId;
use crate::app::AppShell;
use crate::components::{Button, ButtonVariant, Card, GlassPanel, StatusBadge, StatusKind};
use crate::pages::page_scaffold;
use crate::theme;

/// Die 4 Status-Stationen der Connecting-Sequence (bindend, Spec §2.4).
pub const CONNECTING_STATIONS: [&str; 4] =
    ["Aufwecken", "Anmelden", "Verbindung kalibrieren", "Streamen"];

pub fn page(
    shell: &mut AppShell,
    host: HostId,
    _window: &mut Window,
    cx: &mut Context<AppShell>,
) -> impl IntoElement {
    let session = shell.backend.sessions().active();

    let mut children: Vec<gpui::AnyElement> = Vec::new();

    // Stationsanzeige.
    children.push(
        div()
            .flex()
            .flex_row()
            .gap_2()
            .children(CONNECTING_STATIONS.iter().enumerate().map(|(i, station)| {
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px(px(theme::SP_3))
                    .py(px(theme::SP_2))
                    .rounded(px(theme::RADIUS_SM))
                    .bg(theme::SURFACE)
                    .border_1()
                    .border_color(theme::HAIRLINE)
                    .child(
                        div()
                            .size(px(8.0))
                            .rounded_full()
                            .bg(if i == 0 { theme::ACCENT } else { theme::TEXT_DISABLED }),
                    )
                    .child(
                        div()
                            .text_size(px(theme::SIZE_BODY))
                            .text_color(theme::TEXT_PRIMARY)
                            .child(station.to_string()),
                    )
            }))
            .into_any_element(),
    );

    // Video-Fläche (Platzhalter): Der Stream-Agent hängt hier
    // `session.presenter.video_element(take_image(window))` an.
    children.push(
        div()
            .flex_1()
            .rounded(px(theme::RADIUS_LG))
            .bg(theme::SURFACE)
            .border_1()
            .border_color(theme::HAIRLINE)
            .items_center()
            .justify_center()
            .child(
                div()
                    .text_color(theme::TEXT_SECONDARY)
                    .text_size(px(theme::SIZE_HEADLINE))
                    .child(format!(
                        "Video-Fläche — Host {} (Session {} aktiv: {})",
                        host.describe(),
                        session.as_ref().map(|s| s.id).unwrap_or(0),
                        session.as_ref().map(|s| s.is_running()).unwrap_or(false),
                    )),
            )
            .into_any_element(),
    );

    // HUD-Gerüst (Glas-Layer mit Stat-Badges — später Taste H).
    children.push(
        GlassPanel::new()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(StatusBadge::new(StatusKind::Ready).label("stream"))
                    .child(
                        div()
                            .text_size(px(theme::SIZE_BODY))
                            .text_color(theme::TEXT_SECONDARY)
                            .child("Bitrate · RTT · Loss · Frame-Time (HUD kommt)"),
                    ),
            )
            .into_any_element(),
    );

    children.push(
        Card::new("stream-controls")
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new("stream-cancel", "Verbindung trennen")
                            .variant(ButtonVariant::Danger)
                            .on_click(cx.listener(|shell, _ev, _window, cx| {
                                shell.backend.sessions().stop_current();
                                shell.navigate(crate::app::Route::Home, cx);
                            })),
                    )
                    .child(
                        Button::new("stream-home", "Zurück")
                            .on_click(cx.listener(|shell, _ev, _window, cx| {
                                shell.navigate(crate::app::Route::Home, cx);
                            })),
                    ),
            )
            .into_any_element(),
    );

    page_scaffold(children)
}
