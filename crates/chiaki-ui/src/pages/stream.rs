//! Stream (ui-v2-spec §2.4): die Stream-Ansicht — Video volles Fenster
//! (VideoPresenter-Pfad aus Spike S1), Connecting-Sequence mit 4 Stationen,
//! Stats-HUD, Einblend-Panel (H), PIN-/Tastatur-Overlays und die volle
//! Session-Verkabelung über [`state::StreamUiState`].
//!
//! Modulstruktur (Task „Modulstruktur frei, dokumentieren"):
//! * [`state`]  — gpui-Global: Flow, Input-Loop, Stats, Aktionen.
//! * [`connecting`] — Connecting-Sequence-Overlay (4 Stationen + Abbrechen).
//! * [`hud`]    — Stats-Badges, VSR-Badge, Einblend-Panel.
//! * [`dialogs`] — PIN-/Tastatur-Overlay + Trennen-Confirm.
//! * [`fake`]   — FAKE-Mode (`CHIAKI_UI_FAKE_STREAM=1[|pin]`): NV12-
//!   Testpattern + Fake-Telemetrie ohne Konsole.
//!
//! Der Videopfad ist der verifizierte Spike-S1-Weg: `presenter.take_image
//! (window)` pro Frame (GPUI-Thread) + `window.request_animation_frame()` +
//! `cx.notify()` treiben den Loop; `VideoSurface` malt das Bild
//! aspektgetreu (Fit/Zoom/Stretch wie die C++ window_type-Zweige).

pub(crate) mod connecting;
pub(crate) mod dialogs;
pub(crate) mod fake;
pub(crate) mod hud;
pub mod state;

pub use state::shutdown;
pub(crate) use state::{forward_psn_connect_state, forward_session_event, maybe_open_disconnect_dialog};

use std::sync::Arc;

use gpui::{
    div, px, App, Bounds, Corners, Context, Element, ElementId, GlobalElementId,
    InspectorElementId, InteractiveElement as _, IntoElement, KeyDownEvent, KeyUpEvent, LayoutId,
    ParentElement as _, Pixels, Point, RenderImage, Size, StatefulInteractiveElement as _, Style,
    Styled, Window,
};

use crate::app::{AppShell, Route};
use crate::backend::HostId;
use crate::theme;

use state::{Stage, StreamUiState, ZoomMode};

/// Seiten-Einstieg (bindende Signatur, CONTRACT-UI §5).
pub fn page(
    shell: &mut AppShell,
    host: HostId,
    window: &mut Window,
    cx: &mut Context<AppShell>,
) -> impl IntoElement {
    let _created = state::ensure_and_tick(shell, host.clone(), window, cx);
    cx.notify(); // Loop am Laufen halten (Repaint → RAF → Render …)
    // Video-/Input-Loop: gpui-dokumentierter Pfad für kontinuierliches
    // Repainting (spike-s1-results.md „API-Annahmen"). Render-Rate-Cap:
    // auf 240-Hz-Monitoren frisst jedes RAF eine komplette Szenen-Renderung
    // (~4 ms Budget!), die mit Decode/VSR um CPU/GPU konkurriert und
    // präsentierte FPS kostet. ~80 Hz reichen für 60-fps-Video.
    let (request_raf, wait) = {
        let state = cx.global::<StreamUiState>();
        let elapsed = state.last_render.elapsed();
        if elapsed >= std::time::Duration::from_millis(12) {
            (true, None)
        } else {
            (false, Some(std::time::Duration::from_millis(12) - elapsed))
        }
    };
    {
        let state = cx.global_mut::<StreamUiState>();
        state.last_render = std::time::Instant::now();
        if request_raf {
            window.request_animation_frame();
        } else if wait.is_some() && !state.throttle_timer_pending {
            state.throttle_timer_pending = true;
            let shell = cx.entity().downgrade();
            let wait = wait.unwrap();
            cx.spawn(async move |_, cx| {
                cx.background_executor().timer(wait).await;
                let _ = shell.update(cx, |_shell, cx| {
                    if cx.try_global::<state::StreamUiState>().is_some() {
                        cx.global_mut::<state::StreamUiState>().throttle_timer_pending = false;
                    }
                    cx.notify();
                });
            })
            .detach();
        }
    }

    // State-Daten holen (kurze Borrows; danach folgt der Elementbau mit
    // cx.listener, das &mut cx braucht).
    let (presenter, gpu, snap, keyboard_focus, focus) = {
        let state = cx.global::<StreamUiState>();
        (
            state.presenter.clone(),
            state.gpu.clone(),
            state.snapshot(),
            state.keyboard.as_ref().map(|k| k.focus.clone()),
            state.focus.clone(),
        )
    };
    // GPU-Videopfad: das D3D11-Sink-Fenster zeigt das Bild UNTER der gpui-
    // Surface — der Video-Bereich wird transparent gemalt (nicht mal gepixelt)
    // und der Presenter wird nicht angefasst (UI-Kosten ~0 pro Videoframe).
    let gpu_mode = snap.stage == Stage::Streaming && gpu.as_ref().is_some_and(|g| !g.is_lost());
    let image = if gpu_mode {
        None
    } else {
        presenter.as_ref().and_then(|p| p.take_image(window))
    };

    // -- Handler (jeweils kurz &mut cx) -------------------------------------

    let on_cancel = cx.listener(|shell, _ev, _window, cx| {
        if cx.has_global::<StreamUiState>() {
            cx.global_mut::<StreamUiState>().disconnect_now();
        }
        shell.navigate(Route::Home, cx);
    });
    let on_options = cx.listener(|_shell, _ev, _window, cx| {
        if cx.has_global::<StreamUiState>() {
            cx.global_mut::<StreamUiState>().toggle_panel();
        }
    });
    let on_disconnect = cx.listener(|shell, _ev, _window, cx| {
        dialogs::push_disconnect_confirm(shell, cx);
    });
    let on_goto_bed = cx.listener(|_shell, _ev, _window, cx| {
        if cx.has_global::<StreamUiState>() {
            cx.global_mut::<StreamUiState>().goto_bed();
        }
    });
    let on_mic = cx.listener(|_shell, _ev, _window, cx| {
        if cx.has_global::<StreamUiState>() {
            cx.global_mut::<StreamUiState>().toggle_mic();
        }
    });
    let on_zoom = cx.listener(|_shell, _ev, _window, cx| {
        if cx.has_global::<StreamUiState>() {
            cx.global_mut::<StreamUiState>().cycle_zoom();
        }
    });
    let on_video_click = cx.listener(|_shell, ev: &gpui::ClickEvent, window, _cx| {
        // Doppelklick = Vollbild-Toggle (wie F11, C++-Verhalten).
        if ev.click_count() >= 2 {
            window.toggle_fullscreen();
        }
    });
    let on_keyboard_send = cx.listener(|_shell, _ev, window, cx| {
        if cx.has_global::<StreamUiState>() {
            cx.global_mut::<StreamUiState>().keyboard_accept();
            refocus(cx, window);
        }
    });
    let on_keyboard_cancel = cx.listener(|_shell, _ev, window, cx| {
        if cx.has_global::<StreamUiState>() {
            cx.global_mut::<StreamUiState>().keyboard_cancel();
            refocus(cx, window);
        }
    });

    let on_key_down = cx.listener(|_shell, ev: &KeyDownEvent, window, cx| {
        if !cx.has_global::<StreamUiState>() {
            return;
        }
        let key: &str = &ev.keystroke.key;
        let consumed =
            cx.global_mut::<StreamUiState>().on_key_down(key, ev.keystroke.modifiers, window);
        let pending = cx.global::<StreamUiState>().refocus_pending;
        if pending {
            cx.global_mut::<StreamUiState>().refocus_pending = false;
            refocus(cx, window);
        }
        if consumed {
            cx.stop_propagation();
        }
    });
    let on_key_up = cx.listener(|_shell, ev: &KeyUpEvent, _window, cx| {
        if !cx.has_global::<StreamUiState>() {
            return;
        }
        let key: &str = &ev.keystroke.key;
        cx.global_mut::<StreamUiState>().on_key_up(key, ev.keystroke.modifiers);
    });

    // -- Elementbaum ---------------------------------------------------------

    // Video-Fläche (immer da; Connecting-Overlay liegt darüber). Im GPU-Modus
    // KEIN Hintergrund und KEIN Element — ungepixelte Bereiche der gpui-
    // Surface sind transparent (DComp, PREMULTIPLIED), das Video-Fenster
    // dahinter scheint durch; Klicks (Doppelklick = Fullscreen) landen in der
    // gpui-Fläche und behalten damit den Input im UI-Fenster.
    let mut video_area = div()
        .id("stream-video")
        .absolute()
        .inset_0()
        .on_click(on_video_click);
    if gpu_mode {
        video_area = video_area.child(div());
    } else {
        video_area = video_area
            .flex()
            .items_center()
            .justify_center()
            .overflow_hidden()
            .bg(gpui::black())
            .child(match image {
                Some(image) => VideoSurface {
                    image,
                    mode: snap.zoom,
                    factor: snap.zoom_factor,
                }
                .into_any_element(),
                None => div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(theme::SIZE_HEADLINE))
                            .text_color(theme::TEXT_SECONDARY)
                            .child("Kein Video-Signal"),
                    )
                    .child(
                        div()
                            .text_size(px(theme::SIZE_CAPTION))
                            .text_color(theme::TEXT_DISABLED)
                            .child("Warte auf Frames der Session…".to_string()),
                    )
                    .into_any_element(),
            });
    }

    // Connecting-Sequence (Vollbild-Overlay, solange nicht gestreamt wird).
    let connecting_overlay = if snap.stage != Stage::Streaming {
        vec![connecting::view(&snap, on_cancel).into_any_element()]
    } else {
        Vec::new()
    };

    // HUD/Panel (nur im Streaming).
    let mut streaming_overlays: Vec<gpui::AnyElement> = Vec::new();
    if snap.stage == Stage::Streaming {
        let mut top_left = hud::live_badge(&snap);
        if snap.hud_open {
            top_left = div()
                .absolute()
                .top_4()
                .left_4()
                .flex()
                .flex_col()
                .gap_2()
                .max_w(px(720.0))
                .child(top_left)
                .child(hud::stats_row(&snap))
                .into_any_element();
        }
        streaming_overlays.push(top_left);

        let mut top_right: Vec<gpui::AnyElement> = Vec::new();
        if let Some(badge) = hud::vsr_badge(&snap) {
            top_right.push(badge);
        }
        top_right.push(hud::options_button(snap.panel_open, on_options));
        streaming_overlays.push(
            div()
                .absolute()
                .top_4()
                .right_4()
                .flex()
                .items_center()
                .gap_2()
                .children(top_right)
                .into_any_element(),
        );

        if snap.panel_open {
            streaming_overlays.push(
                hud::panel(
                    &snap,
                    hud::PanelHandlers {
                        disconnect: Box::new(on_disconnect),
                        goto_bed: Box::new(on_goto_bed),
                        mic: Box::new(on_mic),
                        zoom: Box::new(on_zoom),
                    },
                )
                .into_any_element(),
            );
        }
    }

    // Overlays: PIN + Konsole-Tastatur.
    let mut overlays: Vec<gpui::AnyElement> = Vec::new();
    if snap.pin_visible {
        overlays.push(dialogs::pin_view(&snap).into_any_element());
    }
    if let (true, Some(kfocus)) = (snap.keyboard_open, keyboard_focus) {
        overlays.push(
            dialogs::keyboard_view(
                &snap.keyboard_text,
                kfocus,
                on_keyboard_send,
                on_keyboard_cancel,
            )
            .into_any_element(),
        );
    }

    // Root-Hintergrund im GPU-Modus TRANSPARENT (gpui::transparent statt
    // theme::BG) — sonst würde die opake Flächenfarbe das Video-Fenster
    // unter der gpui-Surface verdecken.
    let root_bg = if gpu_mode {
        gpui::transparent_black()
    } else {
        theme::BG
    };
    div()
        .id("stream-root")
        .size_full()
        .relative()
        .bg(root_bg)
        .text_color(theme::TEXT_PRIMARY)
        .key_context("Stream")
        .track_focus(&focus)
        .on_key_down(on_key_down)
        .on_key_up(on_key_up)
        .child(video_area)
        .children(connecting_overlay)
        .children(streaming_overlays)
        .children(overlays)
}

/// Fokus zurück auf den Stream-Root (nach Overlay-Schluss).
fn refocus(cx: &mut Context<AppShell>, window: &mut Window) {
    if !cx.has_global::<StreamUiState>() {
        return;
    }
    let focus = cx.global::<StreamUiState>().focus.clone();
    window.defer(cx, move |window, _cx| focus.focus(window));
}

// ---------------------------------------------------------------------------
// VideoSurface — aspektgetreues Malen des Presenter-Bilds
// ---------------------------------------------------------------------------

/// Malt ein `RenderImage` je Skalierungsmodus: Fit (Balken), Zoom (Crop,
/// geclippt durch `overflow_hidden` des Containers), Stretch (verzerrt
/// füllend). Baut auf dem gemessenen `paint_image`-Pfad des Spike S1 auf.
///
/// `factor` = benutzerdefinierter Zoom (settings/zoom_factor; 0 = aus):
/// im Zoom-Modus wird statt der füllenden Skala die **Fit-Skala × Faktor**
/// verwendet — dieselbe Mathe wie im GPU-Sink (`sys::draw_and_present`).
struct VideoSurface {
    image: Arc<RenderImage>,
    mode: ZoomMode,
    factor: f32,
}

impl IntoElement for VideoSurface {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for VideoSurface {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(ElementId::Name("video-surface".into()))
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = gpui::relative(1.0).into();
        style.size.height = gpui::relative(1.0).into();
        let layout_id = window.request_layout(style, [], cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        _cx: &mut App,
    ) {
        let size = self.image.size(0);
        let (iw, ih) = ((size.width.0 as f32).max(1.0), (size.height.0 as f32).max(1.0));
        let (bw, bh) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        let target = match self.mode {
            ZoomMode::Stretch => bounds,
            ZoomMode::Fit | ZoomMode::Zoom => {
                let scale = match self.mode {
                    ZoomMode::Fit => (bw / iw).min(bh / ih),
                    _ => {
                        if self.factor > 1.0 {
                            // Benutzerdefinierter Zoom: Fit-Skala × Faktor
                            // (Crop ab Faktor > füllend).
                            (bw / iw).min(bh / ih) * self.factor
                        } else {
                            (bw / iw).max(bh / ih)
                        }
                    }
                };
                let (w, h) = (iw * scale, ih * scale);
                Bounds {
                    origin: Point {
                        x: bounds.origin.x + px((bw - w) / 2.0),
                        y: bounds.origin.y + px((bh - h) / 2.0),
                    },
                    size: Size { width: px(w), height: px(h) },
                }
            }
        };
        let _ = window.paint_image(target, Corners::default(), self.image.clone(), 0, false);
    }
}
