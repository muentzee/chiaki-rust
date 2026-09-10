//! Connecting-Sequence (ui-v2-spec §2.4): Vollbild-Overlay mit den 4
//! Status-Stationen und Live-Fortschritt aus dem Stream-UI-State. Abbrechen
//! ist immer möglich (stoppt Session/Fake-Thread sauber und navigiert zurück).

use gpui::{div, px, App, ClickEvent, IntoElement, ParentElement as _, Styled, Window};

use crate::components::{Button, ButtonVariant};
use crate::theme;

use super::state::{CONNECTING_STATIONS, StageState, StreamSnapshot};

/// Vollbild-Connecting-Overlay (wird über der Video-Fläche gelegt).
pub fn view(
    snap: &StreamSnapshot,
    cancel: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::AnyElement {
    let station_rows: Vec<gpui::AnyElement> = CONNECTING_STATIONS
        .iter()
        .enumerate()
        .map(|(i, station)| {
            let number = i + 1;
            (number, *station, snap.stages[i])
        })
        .map(|(number, station, state)| station_row(number, station, state))
        .collect();

    let status = if let Some(err) = &snap.error {
        err.clone()
    } else {
        snap.status_line.clone()
    };

    div()
        .absolute()
        .inset_0()
        .bg(theme::BG)
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(theme::SP_5))
        .child(
            div()
                .text_size(px(theme::SIZE_DISPLAY))
                .font_weight(gpui::FontWeight(theme::WEIGHT_DISPLAY))
                .text_color(theme::TEXT_PRIMARY)
                .child(format!("Connecting to {}", snap.host_label)),
        )
        .child(
            div()
                .text_size(px(theme::SIZE_BODY))
                .text_color(theme::TEXT_SECONDARY)
                .child(status),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .w(px(420.0))
                .children(station_rows),
        )
        .children(if snap.fake {
            vec![div()
                .text_size(px(theme::SIZE_CAPTION))
                .text_color(theme::TEXT_DISABLED)
                .child("FAKE mode (CHIAKI_UI_FAKE_STREAM) — no real console".to_string())
                .into_any_element()]
        } else {
            vec![]
        })
        .child(
            Button::new("stream-cancel", "Cancel")
                .variant(ButtonVariant::Danger)
                .on_click(cancel),
        )
        .into_any_element()
}

fn station_row(index: usize, label: &str, state: StageState) -> gpui::AnyElement {
    let (dot_color, text_color, suffix) = match state {
        StageState::Pending => (theme::TEXT_DISABLED, theme::TEXT_DISABLED, ""),
        StageState::Active => (theme::ACCENT, theme::TEXT_PRIMARY, " …"),
        StageState::Done => (theme::SUCCESS, theme::TEXT_PRIMARY, ""),
        StageState::Failed => (theme::DANGER, theme::DANGER, " — failed"),
    };
    div()
        .flex()
        .items_center()
        .gap_3()
        .px(px(theme::SP_3))
        .py(px(theme::SP_2))
        .rounded(px(theme::RADIUS_MD))
        .bg(theme::SURFACE)
        .border_1()
        .border_color(if state == StageState::Active {
            theme::OUTLINE
        } else {
            theme::HAIRLINE
        })
        .child(div().size(px(10.0)).rounded_full().bg(dot_color))
        .child(
            div()
                .text_size(px(theme::SIZE_HEADLINE))
                .font_weight(gpui::FontWeight(theme::WEIGHT_HEADLINE))
                .text_color(text_color)
                .child(format!("{index}. {label}{suffix}")),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::super::state::{StageState, CONNECTING_STATIONS};
    use super::{all_done, done_count};

    #[test]
    fn stationen_sind_bindend() {
        assert_eq!(
            CONNECTING_STATIONS,
            ["Wake", "Log in", "Calibrate connection", "Streaming"]
        );
    }

    #[test]
    fn station_zaehler() {
        let stages = [
            StageState::Done,
            StageState::Done,
            StageState::Active,
            StageState::Pending,
        ];
        assert_eq!(done_count(&stages), 2);
        assert!(!all_done(&stages));
        assert!(all_done(&[StageState::Done; 4]));
    }
}

/// Zählt erledigte Stationen (Progress-Anzeige/Tests).
#[allow(dead_code)]
pub(crate) fn done_count(stages: &[StageState; 4]) -> usize {
    stages.iter().filter(|s| **s == StageState::Done).count()
}

/// Alle Stationen Done? (Übergang in den Streaming-Zustand)
#[allow(dead_code)]
pub(crate) fn all_done(stages: &[StageState; 4]) -> bool {
    stages.iter().all(|s| *s == StageState::Done)
}
