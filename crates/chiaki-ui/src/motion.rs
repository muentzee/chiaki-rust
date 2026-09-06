//! Motion-Tokens + Seitenwechsel-Transition (ui-v2-spec §1.4, bindend).
//!
//! * Seitenwechsel: Fade + Slide-12px, 220 ms, OutCubic.
//! * Hover: 120 ms (gpui-Style-Refinements sind instant; Hover-Feedback
//!   kommt über die `.hover(...)`-Refinements der Komponenten — die
//!   Duration dient als Zielvorgabe für evtl. spätere Frame-getriebene
//!   Hover-Animationen).
//! * Press-Scale 0.98: gpui 0.2.2 `StyleRefinement` hat keine Transformation
//!   für Divs — als Ersatz wird `PRESS_OPACITY` (0.92) im `.active(...)`
//!   benutzt. `PRESS_SCALE` bleibt als Konstante dokumentiert.
//!
//! Die Transition ist frame-getrieben: [`Transition::progress`] liefert den
//! easeten Fortschritt aus `Instant::now()`; der Renderer ruft während einer
//! laufenden Transition `window.request_animation_frame()` + `cx.notify()`.

use std::time::{Duration, Instant};

/// Dauer Seitenwechsel (220 ms).
pub const PAGE_TRANSITION: Duration = Duration::from_millis(220);
/// Dauer Hover (120 ms).
pub const HOVER: Duration = Duration::from_millis(120);
/// Slide-Distanz beim Seitenwechsel (12 px).
pub const SLIDE_DISTANCE_PX: f32 = 12.0;
/// Press-Scale laut Spec (0.98) — in gpui 0.2.2 nicht als Div-Transform
/// verfügbar; die Komponenten nutzen stattdessen [`PRESS_OPACITY`].
pub const PRESS_SCALE: f32 = 0.98;
/// Ersatz-Feedback für Press (Opacity).
pub const PRESS_OPACITY: f32 = 0.92;

/// Port der gpui-Easing-Funktion `ease_out_cubic` (identisch zu
/// `gpui::easing::ease_out_cubic`, hier testbar nachgebaut).
pub fn ease_out_cubic(delta: f32) -> f32 {
    1.0 - (1.0 - delta).powi(3)
}

/// Eine laufende Seitenwechsel-Transition.
#[derive(Debug, Clone)]
pub struct Transition {
    started: Instant,
    duration: Duration,
}

impl Transition {
    /// Startet jetzt.
    pub fn start_now() -> Self {
        Self::start_at(Instant::now())
    }

    /// Mit festem Startzeitpunkt (testbar).
    pub fn start_at(started: Instant) -> Self {
        Self { started, duration: PAGE_TRANSITION }
    }

    /// Echtester (ungeeaster) Fortschritt 0..=1.
    pub fn raw(&self) -> f32 {
        self.raw_at(Instant::now())
    }

    /// Fortschritt zu einem Zeitpunkt (testbar).
    pub fn raw_at(&self, now: Instant) -> f32 {
        let elapsed = now.saturating_duration_since(self.started);
        (elapsed.as_secs_f32() / self.duration.as_secs_f32()).min(1.0)
    }

    /// Ease-Out-Cubic-geglätteter Fortschritt 0..=1 (für Opacity/Offset).
    pub fn progress(&self) -> f32 {
        ease_out_cubic(self.raw())
    }

    /// Fortschritt zu einem Zeitpunkt (geeaseed, testbar).
    pub fn progress_at(&self, now: Instant) -> f32 {
        ease_out_cubic(self.raw_at(now))
    }

    /// Ist die Transition abgeschlossen?
    pub fn finished(&self) -> bool {
        self.raw() >= 1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn easing_werte() {
        assert!((ease_out_cubic(0.0) - 0.0).abs() < 1e-6);
        assert!((ease_out_cubic(0.5) - 0.875).abs() < 1e-6);
        assert!((ease_out_cubic(1.0) - 1.0).abs() < 1e-6);
        // monoton
        let mut prev = 0.0;
        for i in 0..=20 {
            let v = ease_out_cubic(i as f32 / 20.0);
            assert!(v >= prev);
            prev = v;
        }
    }

    #[test]
    fn transition_laeuft_und_endet() {
        let start = Instant::now();
        let t = Transition::start_at(start);
        assert!((t.progress_at(start) - 0.0).abs() < 1e-4);
        let mid = start + Duration::from_millis(110);
        let mid_progress = t.progress_at(mid);
        assert!(mid_progress > 0.5 && mid_progress < 1.0);
        assert!(t.progress_at(start + PAGE_TRANSITION) >= 1.0);
    }

    #[test]
    fn konstanten_matchen_die_spec() {
        assert_eq!(PAGE_TRANSITION, Duration::from_millis(220));
        assert_eq!(HOVER, Duration::from_millis(120));
        assert_eq!(SLIDE_DISTANCE_PX, 12.0);
        assert_eq!(PRESS_SCALE, 0.98);
    }
}
