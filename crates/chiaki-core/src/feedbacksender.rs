// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/feedbacksender.c + lib/include/chiaki/feedbacksender.h (chiaki-ng).
//
// Eigener Thread, der bei Controller-State-Änderungen (spätestens alle
// FEEDBACK_STATE_TIMEOUT_MAX_MS) das Feedback-State-Packet und History-Events
// (Touch-/Button-Kanten) an die Konsole sendet.

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{Builder, JoinHandle};
use std::time::{Duration, Instant};

use super::controller::{BUTTONS_COUNT, ControllerState, TOUCHES_MAX};
use super::error::{ChiakiError, ChiakiResult};
use super::feedback::{FeedbackHistoryBuffer, FeedbackHistoryEvent, FeedbackState};
use super::takion::Takion;
use super::time::now_ms;

/// Minimum time to wait between sending 2 packets
/// (im C derzeit ungenutzt — "TODO: FEEDBACK_STATE_TIMEOUT_MIN_MS")
#[allow(dead_code)]
const FEEDBACK_STATE_TIMEOUT_MIN_MS: u64 = 8;
/// Maximum time to wait between sending 2 packets
const FEEDBACK_STATE_TIMEOUT_MAX_MS: u64 = 200;

/// `FEEDBACK_HISTORY_BUFFER_SIZE`
const FEEDBACK_HISTORY_BUFFER_SIZE: usize = 0x10;
/// `FEEDBACK_HISTORY_RESEND_EVENT_COUNT` — so viele Events werden nach einem
/// Flush für die erneute Übertragung zurückbehalten.
const FEEDBACK_HISTORY_RESEND_EVENT_COUNT: usize = 0x4;

/// `CHIAKI_FEEDBACK_HISTORY_PACKET_BUF_SIZE`
pub const FEEDBACK_HISTORY_PACKET_BUF_SIZE: usize = 0x300;
/// `CHIAKI_FEEDBACK_HISTORY_PACKET_QUEUE_SIZE`
pub const FEEDBACK_HISTORY_PACKET_QUEUE_SIZE: usize = 0x40;

/// Mutex-geschützter Zustand, 1:1 die entsprechenden `ChiakiFeedbackSender`-
/// Felder aus feedbacksender.h.
struct FeedbackSenderInner {
    history_buf: FeedbackHistoryBuffer,
    history_packets: Vec<Box<[u8; FEEDBACK_HISTORY_PACKET_BUF_SIZE]>>,
    history_packet_sizes: Vec<usize>,
    history_packet_begin: usize,
    history_packet_len: usize,

    should_stop: bool,
    controller_state_prev: ControllerState,
    controller_state_history_prev: ControllerState,
    controller_state: ControllerState,
    controller_state_changed: bool,
    history_dirty: bool,
}

/// Zwischen Threads geteilter Teil des Senders (C: struct-Felder + Pointer).
struct Shared {
    takion: Arc<Takion>,
    state_mutex: Mutex<FeedbackSenderInner>,
    state_cond: Condvar,
    /// Beide SeqNums werden beim Senden außerhalb der Sperre gelesen/inkrementiert
    /// (im C gehören sie dem Sender-Thread) — daher Atomics.
    state_seq_num: AtomicU16,
    history_seq_num: AtomicU16,
}

/// Port von `ChiakiFeedbackSender`.
pub struct FeedbackSender {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

/// Port von `controller_state_equals_for_feedback_state()` (feedbacksender.c):
/// vergleicht nur die Felder, die ins FeedbackState-Paket eingehen.
fn controller_state_equals_for_feedback_state(a: &ControllerState, b: &ControllerState) -> bool {
    let eps = 0.0000001f32;
    macro_rules! checkf {
        ($n:ident) => {
            if a.$n < b.$n - eps || a.$n > b.$n + eps {
                return false;
            }
        };
    }
    a.left_x == b.left_x
        && a.left_y == b.left_y
        && a.right_x == b.right_x
        && a.right_y == b.right_y
        && {
            checkf!(gyro_x);
            checkf!(gyro_y);
            checkf!(gyro_z);
            checkf!(accel_x);
            checkf!(accel_y);
            checkf!(accel_z);
            checkf!(orient_x);
            checkf!(orient_y);
            checkf!(orient_z);
            checkf!(orient_w);
            true
        }
}

/// Port von `feedback_sender_record_history()` — pure Logik, testbar ohne Takion.
///
/// Liefert zurück, ob mindestens ein Event gepusht wurde (entspricht dem
/// Setzen von `history_dirty` im C).
fn record_history(
    history_buf: &mut FeedbackHistoryBuffer,
    state_prev: &ControllerState,
    state_now: &ControllerState,
) -> bool {
    let mut dirty = false;

    for i in 0..TOUCHES_MAX {
        if state_prev.touches[i].id != state_now.touches[i].id && state_prev.touches[i].id >= 0 {
            let mut event = FeedbackHistoryEvent::default();
            event.set_touchpad(
                false,
                state_prev.touches[i].id as u8,
                state_prev.touches[i].x,
                state_prev.touches[i].y,
            );
            history_buf.push(event);
            dirty = true;
        } else if state_now.touches[i].id >= 0
            && (state_prev.touches[i].id != state_now.touches[i].id
                || state_prev.touches[i].x != state_now.touches[i].x
                || state_prev.touches[i].y != state_now.touches[i].y)
        {
            let mut event = FeedbackHistoryEvent::default();
            event.set_touchpad(
                true,
                state_now.touches[i].id as u8,
                state_now.touches[i].x,
                state_now.touches[i].y,
            );
            history_buf.push(event);
            dirty = true;
        }
    }

    let buttons_prev = state_prev.buttons as u64;
    let buttons_now = state_now.buttons as u64;
    for i in 0..BUTTONS_COUNT {
        let button_id = 1u64 << i;
        let prev = buttons_prev & button_id != 0;
        let now = buttons_now & button_id != 0;
        if prev != now {
            let mut event = FeedbackHistoryEvent::default();
            match event.set_button(button_id, if now { 0xff } else { 0 }) {
                Ok(()) => {
                    history_buf.push(event);
                    dirty = true;
                }
                Err(_) => {
                    tracing::error!(
                        "Feedback Sender failed to format button history event for button id {}",
                        button_id
                    );
                }
            }
        }
    }

    if state_prev.l2_state != state_now.l2_state {
        let mut event = FeedbackHistoryEvent::default();
        match event.set_button(super::controller::ANALOG_BUTTON_L2 as u64, state_now.l2_state) {
            Ok(()) => {
                history_buf.push(event);
                dirty = true;
            }
            Err(_) => tracing::error!("Feedback Sender failed to format button history event for L2"),
        }
    }

    if state_prev.r2_state != state_now.r2_state {
        let mut event = FeedbackHistoryEvent::default();
        match event.set_button(super::controller::ANALOG_BUTTON_R2 as u64, state_now.r2_state) {
            Ok(()) => {
                history_buf.push(event);
                dirty = true;
            }
            Err(_) => tracing::error!("Feedback Sender failed to format button history event for R2"),
        }
    }

    dirty
}

/// Port von `feedback_sender_flush_history_locked()` (Mutex muss gehalten
/// werden — daher `&mut FeedbackSenderInner`).
fn flush_history_locked(inner: &mut FeedbackSenderInner) {
    if !inner.history_dirty {
        return;
    }

    let packet_index = (inner.history_packet_begin + inner.history_packet_len)
        % FEEDBACK_HISTORY_PACKET_QUEUE_SIZE;
    let packet_size = {
        let buf = &mut inner.history_packets[packet_index][..];
        match inner.history_buf.format(buf) {
            Ok(size) => size,
            Err(_) => {
                tracing::error!("Feedback Sender failed to format history buffer");
                return;
            }
        }
    };

    if inner.history_packet_len < FEEDBACK_HISTORY_PACKET_QUEUE_SIZE {
        inner.history_packet_sizes[packet_index] = packet_size;
        inner.history_packet_len += 1;
    } else {
        // C: memcpy(history_packets[begin], history_packets[packet_index], size)
        // — dabei ist packet_index == (begin + QUEUE) % QUEUE == begin, das
        // memcpy also ein Self-Copy ohne Effekt. Wir übernehmen nur sizes/begin.
        inner.history_packet_sizes[inner.history_packet_begin] = packet_size;
        inner.history_packet_begin = (inner.history_packet_begin + 1) % FEEDBACK_HISTORY_PACKET_QUEUE_SIZE;
        tracing::warn!("Feedback Sender history packet queue overflow");
    }

    inner
        .history_buf
        .truncate_len(FEEDBACK_HISTORY_RESEND_EVENT_COUNT);
    inner.history_dirty = false;
}

/// Port von `feedback_sender_thread_func()`.
fn feedback_sender_thread_func(shared: &Shared) {
    let mut inner = lock(&shared.state_mutex);

    let mut last_feedback_state_ms = now_ms();
    loop {
        if inner.history_packet_len == 0 {
            let now = now_ms();
            let mut next_timeout = FEEDBACK_STATE_TIMEOUT_MAX_MS;
            if now.wrapping_sub(last_feedback_state_ms) < FEEDBACK_STATE_TIMEOUT_MAX_MS {
                next_timeout = FEEDBACK_STATE_TIMEOUT_MAX_MS - (now - last_feedback_state_ms);
            }

            // chiaki_cond_timedwait_pred(pred: should_stop || controller_state_changed):
            // kehrt bei erfüllter Prädikatsbedingung ("SUCCESS") oder Timeout zurück.
            let deadline = Instant::now() + Duration::from_millis(next_timeout);
            loop {
                if inner.should_stop || inner.controller_state_changed {
                    break; // "SUCCESS"
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break; // "TIMEOUT" — wird wie im C toleriert
                }
                let (guard, _wait_result) = shared
                    .state_cond
                    .wait_timeout(inner, remaining)
                    .unwrap_or_else(|e| e.into_inner());
                inner = guard;
            }
        }

        if inner.should_stop {
            break;
        }

        let now = now_ms();
        let mut send_feedback_state = now - last_feedback_state_ms >= FEEDBACK_STATE_TIMEOUT_MAX_MS;
        let state_now = inner.controller_state;
        let mut send_feedback_history = false;
        let mut history_buf = [0u8; FEEDBACK_HISTORY_PACKET_BUF_SIZE];
        let mut history_buf_size = 0usize;

        if inner.controller_state_changed {
            // TODO: FEEDBACK_STATE_TIMEOUT_MIN_MS (so auch im C)
            inner.controller_state_changed = false;
            send_feedback_state = true;

            // don't need to send feedback state if nothing relevant changed
            if controller_state_equals_for_feedback_state(&state_now, &inner.controller_state_prev) {
                send_feedback_state = false;
            }
        } // else: timeout

        if inner.history_packet_len > 0 {
            let packet_index = inner.history_packet_begin;
            history_buf_size = inner.history_packet_sizes[packet_index];
            history_buf[..history_buf_size]
                .copy_from_slice(&inner.history_packets[packet_index][..history_buf_size]);
            inner.history_packet_begin = (inner.history_packet_begin + 1)
                % FEEDBACK_HISTORY_PACKET_QUEUE_SIZE;
            inner.history_packet_len -= 1;
            send_feedback_history = true;
        }
        drop(inner);

        if send_feedback_state {
            feedback_sender_send_state(shared, &state_now);
        }

        if send_feedback_history {
            feedback_sender_send_history_packet(shared, &history_buf[..history_buf_size]);
        }

        inner = lock(&shared.state_mutex);
        if send_feedback_state {
            inner.controller_state_prev = state_now;
            last_feedback_state_ms = now_ms();
        }
    }

    drop(inner);
}

/// Port von `feedback_sender_send_state()`.
fn feedback_sender_send_state(shared: &Shared, state: &ControllerState) {
    let feedback_state = FeedbackState {
        left_x: state.left_x,
        left_y: state.left_y,
        right_x: state.right_x,
        right_y: state.right_y,
        gyro_x: state.gyro_x,
        gyro_y: state.gyro_y,
        gyro_z: state.gyro_z,
        accel_x: state.accel_x,
        accel_y: state.accel_y,
        accel_z: state.accel_z,
        orient_x: state.orient_x,
        orient_y: state.orient_y,
        orient_z: state.orient_z,
        orient_w: state.orient_w,
    };

    let seq_num = shared.state_seq_num.fetch_add(1, Ordering::Relaxed);
    if shared.takion.send_feedback_state(seq_num, &feedback_state).is_err() {
        tracing::error!("FeedbackSender failed to send Feedback State");
    }
}

/// Port von `feedback_sender_send_history_packet()`.
fn feedback_sender_send_history_packet(shared: &Shared, buf: &[u8]) {
    let seq_num = shared.history_seq_num.fetch_add(1, Ordering::Relaxed);
    if let Err(e) = shared.takion.send_feedback_history(seq_num, buf) {
        tracing::warn!("FeedbackSender failed to send Feedback History: {e}");
    }
}

fn lock(mutex: &Mutex<FeedbackSenderInner>) -> std::sync::MutexGuard<'_, FeedbackSenderInner> {
    // Poisoned Mutex: Zustand bleibt strukturell konsistent.
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl FeedbackSender {
    /// Port von `chiaki_feedback_sender_init()`.
    ///
    /// `takion` als `Arc<Takion>` (C: zeigergeteilter `ChiakiTakion*`).
    pub fn new(takion: Arc<Takion>) -> ChiakiResult<FeedbackSender> {
        let history_packets: Vec<Box<[u8; FEEDBACK_HISTORY_PACKET_BUF_SIZE]>> = (0
            ..FEEDBACK_HISTORY_PACKET_QUEUE_SIZE)
            .map(|_| Box::new([0u8; FEEDBACK_HISTORY_PACKET_BUF_SIZE]))
            .collect();

        let inner = FeedbackSenderInner {
            history_buf: FeedbackHistoryBuffer::new(FEEDBACK_HISTORY_BUFFER_SIZE),
            history_packets,
            history_packet_sizes: vec![0; FEEDBACK_HISTORY_PACKET_QUEUE_SIZE],
            history_packet_begin: 0,
            history_packet_len: 0,
            should_stop: false,
            controller_state_prev: ControllerState::default(),
            controller_state_history_prev: ControllerState::default(),
            controller_state: ControllerState::default(),
            controller_state_changed: false,
            history_dirty: false,
        };

        let shared = Arc::new(Shared {
            takion,
            state_mutex: Mutex::new(inner),
            state_cond: Condvar::new(),
            state_seq_num: AtomicU16::new(0),
            history_seq_num: AtomicU16::new(0),
        });

        let thread_shared = Arc::clone(&shared);
        let thread = Builder::new()
            .name("Chiaki Feedback Sender".to_string())
            .spawn(move || feedback_sender_thread_func(&thread_shared))
            .map_err(|e| {
                tracing::error!("FeedbackSender: thread create failed: {e}");
                ChiakiError::Thread
            })?;

        Ok(FeedbackSender {
            shared,
            thread: Some(thread),
        })
    }

    /// Port von `chiaki_feedback_sender_set_controller_state()`.
    ///
    /// Gleicher State -> No-op (kein History-Record, kein Wecken).
    pub fn set_controller_state(&self, state: &ControllerState) -> ChiakiResult<()> {
        let mut inner = lock(&self.shared.state_mutex);

        if inner.controller_state.equals(state) {
            return Ok(());
        }

        {
            // Reborrow als plain &mut: erlaubt disjointe Feld-Borrows im Aufruf
            // (durch den MutexGuard selbst wäre das kein Field-Splitting).
            let inner = &mut *inner;
            inner.controller_state = *state;
            if record_history(
                &mut inner.history_buf,
                &inner.controller_state_history_prev,
                &inner.controller_state,
            ) {
                inner.history_dirty = true;
            }
            flush_history_locked(inner);
            inner.controller_state_history_prev = inner.controller_state;
            inner.controller_state_changed = true;
        }

        drop(inner);
        self.shared.state_cond.notify_all();

        Ok(())
    }

    /// Fortlaufende Feedback-State-Sequenznummer (Statistik/Test).
    pub fn state_seq_num(&self) -> u16 {
        self.shared.state_seq_num.load(Ordering::Relaxed)
    }
}

impl Drop for FeedbackSender {
    /// Port von `chiaki_feedback_sender_fini()`: stoppen und Thread joinen.
    fn drop(&mut self) {
        {
            let mut inner = lock(&self.shared.state_mutex);
            inner.should_stop = true;
        }
        self.shared.state_cond.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::{
        ANALOG_BUTTON_L2, BUTTON_CROSS, BUTTON_DPAD_UP, BUTTON_PS, BUTTON_TOUCHPAD,
    };

    fn idle() -> ControllerState {
        ControllerState::default()
    }

    fn event_bytes(history: &FeedbackHistoryBuffer) -> Vec<u8> {
        let mut buf = [0u8; FEEDBACK_HISTORY_PACKET_BUF_SIZE];
        let n = history.format(&mut buf).unwrap();
        buf[..n].to_vec()
    }

    #[test]
    fn record_history_touch_down_up_and_move() {
        let prev = idle();
        let mut now = idle();
        let mut history = FeedbackHistoryBuffer::new(0x10);

        // Touch starten (id 0, Position 100/200)
        now.start_touch(100, 200);
        assert!(record_history(&mut history, &prev, &now));
        // neuestes Event zuerst: touch down
        assert_eq!(event_bytes(&history), vec![0xd0, 0x00, 0x06, 0x40, 0xc8]);

        // Bewegung auf (110, 220)
        now.set_touch_pos(0, 110, 220);
        assert!(record_history(&mut history, &prev, &now));
        // newest first: down(110,220), dann down(100,200)
        assert_eq!(
            event_bytes(&history),
            vec![
                0xd0, 0x00, 0x06, 0xe0, 0xdc, // (110, 220)
                0xd0, 0x00, 0x06, 0x40, 0xc8, // (100, 200)
            ]
        );

        // Touch lösen (prev = Bewegungs-State, now = idle)
        let moved = now;
        let up = idle();
        assert!(record_history(&mut history, &moved, &up));
        // prev hat id 0 >= 0 und ids unterscheiden sich -> touch up mit alten Koordinaten
        assert_eq!(event_bytes(&history)[..5], [0xc0, 0x00, 0x06, 0xe0, 0xdc]);
    }

    #[test]
    fn record_history_button_edges() {
        let mut history = FeedbackHistoryBuffer::new(0x10);
        let prev = idle();
        let mut now = idle();
        now.buttons = BUTTON_CROSS | BUTTON_PS;

        assert!(record_history(&mut history, &prev, &now));
        // newest first: PS-down (2 Byte), cross-down (3 Byte)
        assert_eq!(event_bytes(&history), vec![0x80, 0xae, 0x80, 0x88, 0xff]);

        // Keine Änderung -> kein Event
        assert!(!record_history(&mut history, &now, &now));
        assert_eq!(event_bytes(&history), vec![0x80, 0xae, 0x80, 0x88, 0xff]);

        // Cross wieder loslassen — prev ist (wie im Sender-Fluss: nach jedem
        // Flush wird controller_state_history_prev = controller_state) der
        // zuletzt gesendete State, nicht der initiale idle-State.
        let prev_state = now;
        now.buttons = BUTTON_PS;
        assert!(record_history(&mut history, &prev_state, &now));
        assert_eq!(event_bytes(&history)[..3], [0x80, 0x88, 0x00]);
    }

    #[test]
    fn record_history_dpad_3byte_touchpad_2byte() {
        let mut history = FeedbackHistoryBuffer::new(0x10);
        let prev = idle();
        let mut now = idle();
        now.buttons = BUTTON_DPAD_UP | BUTTON_TOUCHPAD;
        record_history(&mut history, &prev, &now);
        // newest first: touchpad-down (State im 2. Byte), dpad_up-down (State im 3. Byte)
        assert_eq!(event_bytes(&history), vec![0x80, 0xb1, 0x80, 0x80, 0xff]);
    }

    #[test]
    fn record_history_analog_triggers() {
        let mut history = FeedbackHistoryBuffer::new(0x10);
        let prev = idle();
        let mut now = idle();
        now.l2_state = 0x42;
        now.r2_state = 0xff;
        assert!(record_history(&mut history, &prev, &now));
        // newest first: R2, L2 (jeweils 3 Byte mit analogem Wert)
        assert_eq!(event_bytes(&history), vec![0x80, 0x87, 0xff, 0x80, 0x86, 0x42]);

        // L2-Wertänderung allein
        let mut now2 = now;
        now2.l2_state = 0x7f;
        assert!(record_history(&mut history, &now, &now2));
        assert_eq!(event_bytes(&history)[..3], [0x80, 0x86, 0x7f]);
    }

    #[test]
    fn flush_history_formats_packet_and_truncates() {
        let mut inner = FeedbackSenderInner {
            history_buf: FeedbackHistoryBuffer::new(FEEDBACK_HISTORY_BUFFER_SIZE),
            history_packets: (0..FEEDBACK_HISTORY_PACKET_QUEUE_SIZE)
                .map(|_| Box::new([0u8; FEEDBACK_HISTORY_PACKET_BUF_SIZE]))
                .collect(),
            history_packet_sizes: vec![0; FEEDBACK_HISTORY_PACKET_QUEUE_SIZE],
            history_packet_begin: 0,
            history_packet_len: 0,
            should_stop: false,
            controller_state_prev: idle(),
            controller_state_history_prev: idle(),
            controller_state: idle(),
            controller_state_changed: false,
            history_dirty: false,
        };

        // Mehr Events als FEEDBACK_HISTORY_RESEND_EVENT_COUNT erzeugen
        let mut prev = idle();
        for i in 0..6u8 {
            let mut now = idle();
            now.buttons = if i % 2 == 0 { BUTTON_CROSS } else { 0 };
            if record_history(&mut inner.history_buf, &prev, &now) {
                inner.history_dirty = true;
            }
            prev = now;
        }
        assert!(inner.history_dirty);
        assert_eq!(inner.history_buf.len(), 6);

        flush_history_locked(&mut inner);

        assert!(!inner.history_dirty);
        assert_eq!(inner.history_packet_len, 1);
        assert_eq!(inner.history_packet_begin, 0);
        let size = inner.history_packet_sizes[0];
        assert_eq!(size, 6 * 3, "6 Button-Events à 3 Bytes");
        // Puffer wurde auf RESEND-EVENT-COUNT eingekürzt
        assert_eq!(inner.history_buf.len(), FEEDBACK_HISTORY_RESEND_EVENT_COUNT);

        // Zweiter Flush ohne neue Events: kein zweites Paket
        flush_history_locked(&mut inner);
        assert_eq!(inner.history_packet_len, 1);
    }

    #[test]
    fn state_equals_for_feedback_ignores_buttons() {
        // Buttons sind irrelevant fürs Feedback-State-Paket
        let mut b = idle();
        b.buttons = 0xffffffff;
        assert!(controller_state_equals_for_feedback_state(&idle(), &b));

        // Sticks zählen
        let mut c = idle();
        c.left_x = 5;
        assert!(!controller_state_equals_for_feedback_state(&idle(), &c));

        // Float-Epsilon wie im C
        let mut d = idle();
        d.gyro_z = idle().gyro_z + 0.0000001 * 2.0;
        assert!(!controller_state_equals_for_feedback_state(&idle(), &d));
        let mut e = idle();
        e.orient_w = idle().orient_w + 0.0000001 / 2.0;
        assert!(controller_state_equals_for_feedback_state(&idle(), &e));
    }

    #[test]
    fn constants_match_c_header() {
        assert_eq!(FEEDBACK_STATE_TIMEOUT_MIN_MS, 8);
        assert_eq!(FEEDBACK_STATE_TIMEOUT_MAX_MS, 200);
        assert_eq!(FEEDBACK_HISTORY_BUFFER_SIZE, 0x10);
        assert_eq!(FEEDBACK_HISTORY_RESEND_EVENT_COUNT, 0x4);
        assert_eq!(FEEDBACK_HISTORY_PACKET_BUF_SIZE, 0x300);
        assert_eq!(FEEDBACK_HISTORY_PACKET_QUEUE_SIZE, 0x40);
    }

    #[test]
    fn analog_button_ids_in_record_history() {
        // ANALOG_BUTTON_L2/R2 werden als eigene Events kodiert
        let mut history = FeedbackHistoryBuffer::new(0x10);
        let prev = idle();
        let mut now = idle();
        now.l2_state = 1;
        record_history(&mut history, &prev, &now);
        let mut ev = FeedbackHistoryEvent::default();
        ev.set_button(ANALOG_BUTTON_L2 as u64, 1).unwrap();
        assert_eq!(event_bytes(&history), ev.buf[..ev.len].to_vec());
    }
}
