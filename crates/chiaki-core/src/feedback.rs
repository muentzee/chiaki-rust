// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/feedback.c + lib/include/chiaki/feedback.h (chiaki-ng).
//
// Baut das 0x10-Byte-Feedback-Packet (bzw. 0x1c für v12) und das
// Feedback-History-Ringbuffer-Format für den FeedbackSender.

use super::controller::{
    ANALOG_BUTTON_L2, ANALOG_BUTTON_R2, BUTTON_BOX, BUTTON_CROSS, BUTTON_DPAD_DOWN,
    BUTTON_DPAD_LEFT, BUTTON_DPAD_RIGHT, BUTTON_DPAD_UP, BUTTON_L1, BUTTON_L3, BUTTON_MOON,
    BUTTON_OPTIONS, BUTTON_PS, BUTTON_PYRAMID, BUTTON_R1, BUTTON_R3, BUTTON_SHARE, BUTTON_TOUCHPAD,
};
use super::error::{ChiakiError, ChiakiResult};

const GYRO_MIN: f32 = -30.0;
const GYRO_MAX: f32 = 30.0;
const ACCEL_MIN: f32 = -5.0;
const ACCEL_MAX: f32 = 5.0;

/// M_SQRT1_2 aus math.h (double) — identisch zu f64::consts::FRAC_1_SQRT_2.
const M_SQRT1_2: f64 = std::f64::consts::FRAC_1_SQRT_2;

/// `CHIAKI_FEEDBACK_STATE_BUF_SIZE_MAX`.
pub const FEEDBACK_STATE_BUF_SIZE_MAX: usize = 0x1c;

/// `CHIAKI_FEEDBACK_STATE_BUF_SIZE_V9`.
pub const FEEDBACK_STATE_BUF_SIZE_V9: usize = 0x19;

/// `CHIAKI_FEEDBACK_STATE_BUF_SIZE_V12`.
pub const FEEDBACK_STATE_BUF_SIZE_V12: usize = 0x1c;

/// `CHIAKI_HISTORY_EVENT_SIZE_MAX`.
pub const HISTORY_EVENT_SIZE_MAX: usize = 0x5;

/// Port von `ChiakiFeedbackState`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FeedbackState {
    pub gyro_x: f32,
    pub gyro_y: f32,
    pub gyro_z: f32,
    pub accel_x: f32,
    pub accel_y: f32,
    pub accel_z: f32,
    pub orient_x: f32,
    pub orient_y: f32,
    pub orient_z: f32,
    pub orient_w: f32,
    pub left_x: i16,
    pub left_y: i16,
    pub right_x: i16,
    pub right_y: i16,
}

impl Default for FeedbackState {
    fn default() -> Self {
        FeedbackState {
            gyro_x: 0.0,
            gyro_y: 0.0,
            gyro_z: 0.0,
            accel_x: 0.0,
            accel_y: 1.0,
            accel_z: 0.0,
            orient_x: 0.0,
            orient_y: 0.0,
            orient_z: 0.0,
            orient_w: 1.0,
            left_x: 0,
            left_y: 0,
            right_x: 0,
            right_y: 0,
        }
    }
}

/// "very similar idea as https://github.com/jpreiss/quatcompress" (feedback.c):
/// größte Komponente wird über 2 Bit kodiert (Index + Vorzeichen), die drei
/// restlichen als 9-Bit-Fixpunkt über [-sqrt(1/2), sqrt(1/2)].
fn compress_quat(q: &[f32; 4]) -> u32 {
    let mut largest_i = 0usize;
    for i in 1..4 {
        if q[i].abs() > q[largest_i].abs() {
            largest_i = i;
        }
    }
    let mut r: u32 = if q[largest_i] < 0.0 { 1 } else { 0 } | ((largest_i as u32) << 1);
    for i in 0..3usize {
        let qi = if i < largest_i { i } else { i + 1 };
        let mut v: f32 = q[qi];
        // C rechnet die Grenzvergleiche und die Skalierung in double nach und
        // schreibt zurück in float — exakt nachempfunden.
        if (v as f64) < -M_SQRT1_2 {
            v = -M_SQRT1_2 as f32;
        }
        if (v as f64) > M_SQRT1_2 {
            v = M_SQRT1_2 as f32;
        }
        v = (v as f64 + M_SQRT1_2) as f32;
        // C: v *= (float)0x1ff / (2.0f * M_SQRT1_2);
        v = (v as f64 * (511.0f64 / (2.0f64 * M_SQRT1_2))) as f32;
        r |= (v as u32) << (3 + i * 9);
    }
    r
}

impl FeedbackState {
    /// Port von `chiaki_feedback_state_format_v9()`.
    ///
    /// `buf` muss mindestens `FEEDBACK_STATE_BUF_SIZE_V9` Bytes haben.
    pub fn format_v9(&self, buf: &mut [u8]) -> ChiakiResult<()> {
        if buf.len() < FEEDBACK_STATE_BUF_SIZE_V9 {
            return Err(ChiakiError::BufTooSmall);
        }
        buf[0x0] = 0xa0;
        let put = |buf: &mut [u8], off: usize, v: u16| {
            buf[off] = v as u8;
            buf[off + 1] = (v >> 8) as u8;
        };

        let scale = |v: f32, min: f32, max: f32| -> u16 {
            // C: (uint16_t)(0xffff * ((float)v - min) / (max - min)) — f32-Arithmetik
            (0xffffu32 as f32 * (v - min) / (max - min)) as u16
        };

        put(buf, 0x1, scale(self.gyro_x, GYRO_MIN, GYRO_MAX));
        put(buf, 0x3, scale(self.gyro_y, GYRO_MIN, GYRO_MAX));
        put(buf, 0x5, scale(self.gyro_z, GYRO_MIN, GYRO_MAX));
        put(buf, 0x7, scale(self.accel_x, ACCEL_MIN, ACCEL_MAX));
        put(buf, 0x9, scale(self.accel_y, ACCEL_MIN, ACCEL_MAX));
        put(buf, 0xb, scale(self.accel_z, ACCEL_MIN, ACCEL_MAX));

        let q = [
            self.orient_x,
            self.orient_y,
            self.orient_z,
            self.orient_w,
        ];
        let qc = compress_quat(&q);
        buf[0xd] = qc as u8;
        buf[0xe] = (qc >> 0x8) as u8;
        buf[0xf] = (qc >> 0x10) as u8;
        buf[0x10] = (qc >> 0x18) as u8;

        put(buf, 0x11, self.left_x as u16); // htons: big-endian
        put(buf, 0x13, self.left_y as u16);
        put(buf, 0x15, self.right_x as u16);
        put(buf, 0x17, self.right_y as u16);
        Ok(())
    }

    /// Port von `chiaki_feedback_state_format_v12()`.
    ///
    /// `buf` muss mindestens `FEEDBACK_STATE_BUF_SIZE_V12` Bytes haben.
    pub fn format_v12(&self, buf: &mut [u8]) -> ChiakiResult<()> {
        self.format_v9(buf)?;
        if buf.len() < FEEDBACK_STATE_BUF_SIZE_V12 {
            return Err(ChiakiError::BufTooSmall);
        }
        buf[0x19] = 0x0;
        buf[0x1a] = 0x0;
        buf[0x1b] = 0x1;
        Ok(())
    }

    /// Convenience: formatiert in ein festes 0x19-Byte-Array (v9).
    pub fn to_array_v9(&self) -> [u8; FEEDBACK_STATE_BUF_SIZE_V9] {
        let mut buf = [0u8; FEEDBACK_STATE_BUF_SIZE_V9];
        self.format_v9(&mut buf).expect("0x19-Array passt immer");
        buf
    }

    /// Convenience: formatiert in ein festes 0x1c-Byte-Array (v12).
    pub fn to_array_v12(&self) -> [u8; FEEDBACK_STATE_BUF_SIZE_V12] {
        let mut buf = [0u8; FEEDBACK_STATE_BUF_SIZE_V12];
        self.format_v12(&mut buf).expect("0x1c-Array passt immer");
        buf
    }
}

/// Port von `chiaki_feedback_state_format_v9()` (kanonische API, Reihenfolge
/// der Argumente wie im C: `(buf, state)`).
///
/// `buf` muss mindestens `FEEDBACK_STATE_BUF_SIZE_V9` Bytes haben.
pub fn feedback_state_format_v9(buf: &mut [u8], state: &FeedbackState) -> ChiakiResult<()> {
    state.format_v9(buf)
}

/// Port von `chiaki_feedback_state_format_v12()` (kanonische API, Reihenfolge
/// der Argumente wie im C: `(buf, state)`).
///
/// `buf` muss mindestens `FEEDBACK_STATE_BUF_SIZE_V12` Bytes haben.
pub fn feedback_state_format_v12(buf: &mut [u8], state: &FeedbackState) -> ChiakiResult<()> {
    state.format_v12(buf)
}

/// Port von `ChiakiFeedbackHistoryEvent`.
#[derive(Debug, Clone, Copy)]
pub struct FeedbackHistoryEvent {
    pub buf: [u8; HISTORY_EVENT_SIZE_MAX],
    pub len: usize,
}

impl Default for FeedbackHistoryEvent {
    fn default() -> Self {
        FeedbackHistoryEvent {
            buf: [0u8; HISTORY_EVENT_SIZE_MAX],
            len: 0,
        }
    }
}

impl FeedbackHistoryEvent {
    /// Port von `chiaki_feedback_history_event_set_button()`.
    ///
    /// `button` ist ein `ChiakiControllerButton` oder
    /// `ChiakiControllerAnalogButton`-Wert, `state` 0x0 für "nicht gedrückt",
    /// 0xff für "gedrückt", Zwischenwerte für analoge Trigger.
    pub fn set_button(&mut self, button: u64, state: u8) -> ChiakiResult<()> {
        // some buttons use a third byte for the state, some don't
        self.buf[0] = 0x80;
        self.len = 2;
        let code = match button {
            b if b == BUTTON_CROSS as u64 => 0x88,
            b if b == BUTTON_MOON as u64 => 0x89,
            b if b == BUTTON_BOX as u64 => 0x8a,
            b if b == BUTTON_PYRAMID as u64 => 0x8b,
            b if b == BUTTON_DPAD_LEFT as u64 => 0x82,
            b if b == BUTTON_DPAD_RIGHT as u64 => 0x83,
            b if b == BUTTON_DPAD_UP as u64 => 0x80,
            b if b == BUTTON_DPAD_DOWN as u64 => 0x81,
            b if b == BUTTON_L1 as u64 => 0x84,
            b if b == BUTTON_R1 as u64 => 0x85,
            b if b == ANALOG_BUTTON_L2 as u64 => 0x86,
            b if b == ANALOG_BUTTON_R2 as u64 => 0x87,
            // Diese Buttons legen den State ins zweite Byte und sind immer
            // nur 2 Bytes lang:
            b if b == BUTTON_L3 as u64 => {
                self.buf[1] = if state != 0 { 0xaf } else { 0x8f };
                return Ok(());
            }
            b if b == BUTTON_R3 as u64 => {
                self.buf[1] = if state != 0 { 0xb0 } else { 0x90 };
                return Ok(());
            }
            b if b == BUTTON_OPTIONS as u64 => {
                self.buf[1] = if state != 0 { 0xac } else { 0x8c };
                return Ok(());
            }
            b if b == BUTTON_SHARE as u64 => {
                self.buf[1] = if state != 0 { 0xad } else { 0x8d };
                return Ok(());
            }
            b if b == BUTTON_TOUCHPAD as u64 => {
                self.buf[1] = if state != 0 { 0xb1 } else { 0x91 };
                return Ok(());
            }
            b if b == BUTTON_PS as u64 => {
                self.buf[1] = if state != 0 { 0xae } else { 0x8e };
                return Ok(());
            }
            _ => return Err(ChiakiError::InvalidData),
        };
        self.buf[1] = code;
        self.buf[2] = state;
        self.len = 3;
        Ok(())
    }

    /// Port von `chiaki_feedback_history_event_set_touchpad()`.
    ///
    /// `pointer_id` 0..127, `x` 0..1920, `y` 0..942.
    pub fn set_touchpad(&mut self, down: bool, pointer_id: u8, x: u16, y: u16) {
        self.len = 5;
        self.buf[0] = if down { 0xd0 } else { 0xc0 };
        self.buf[1] = pointer_id & 0x7f;
        self.buf[2] = (x >> 4) as u8;
        self.buf[3] = ((x & 0xf) << 4) as u8 | (y >> 8) as u8;
        self.buf[4] = y as u8;
    }
}

/// Port von `ChiakiFeedbackHistoryBuffer` — Ringpuffer von
/// `FeedbackHistoryEvent`s, `push()` fügt vorne ein.
pub struct FeedbackHistoryBuffer {
    events: Vec<FeedbackHistoryEvent>,
    size: usize,
    begin: usize,
    len: usize,
}

impl FeedbackHistoryBuffer {
    /// Port von `chiaki_feedback_history_buffer_init()`.
    ///
    /// Kann — anders als das C-Original (calloc kann fehlschlagen) — nicht
    /// fehlschlagen und liefert daher kein `Result`.
    pub fn new(size: usize) -> Self {
        FeedbackHistoryBuffer {
            events: vec![FeedbackHistoryEvent::default(); size],
            size,
            begin: 0,
            len: 0,
        }
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Port von `chiaki_feedback_history_buffer_format()`.
    ///
    /// Schreibt alle Events hintereinander in `buf` und liefert die
    /// geschriebene Größe; `ChiakiError::BufTooSmall`, wenn es nicht passt.
    pub fn format(&self, buf: &mut [u8]) -> ChiakiResult<usize> {
        let size_max = buf.len();
        let mut written = 0usize;

        for i in 0..self.len {
            let event = &self.events[(self.begin + i) % self.size];
            if written + event.len > size_max {
                return Err(ChiakiError::BufTooSmall);
            }
            buf[written..written + event.len].copy_from_slice(&event.buf[..event.len]);
            written += event.len;
        }

        Ok(written)
    }

    /// Port von `chiaki_feedback_history_buffer_push()`: fügt ein Event an den
    /// Anfang des Puffers.
    pub fn push(&mut self, event: FeedbackHistoryEvent) {
        self.begin = (self.begin + self.size - 1) % self.size;
        self.len += 1;
        if self.len >= self.size {
            self.len = self.size;
        }
        self.events[self.begin] = event;
    }

    /// Kürzt die logische Länge (feedbacksender.c: History wird nach dem
    /// Flush auf `FEEDBACK_HISTORY_RESEND_EVENT_COUNT` eingekürzt).
    ///
    /// Semantik wie C: `len` wird direkt gesetzt (die Events bleiben liegen,
    /// `begin` bleibt unverändert — abgeschnittene älteste Events wandern
    /// logisch ans Ende und werden beim Weiterlaufen des Rings überschrieben).
    pub fn truncate_len(&mut self, max_len: usize) {
        if self.len > max_len {
            self.len = max_len;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::{BUTTON_OPTIONS, BUTTON_PS};

    /// Golden-Vektor A (neutral): mit numpy-f32-Emulation der C-Arithmetik
    /// berechnet (gyro/accel auf Mittelwert des Bereichs, Identity-Quaternion).
    #[test]
    fn format_v9_neutral_matches_c_bytes() {
        let state = FeedbackState::default();
        let buf = state.to_array_v9();
        // gyro 0 -> Mittelwert 0x7fff; accel_y 1 -> 0xffff*(1+5)/10 = 0x9999;
        // Identity-Quaternion -> qc = 0x1feff7fe
        assert_eq!(
            buf,
            [
                0xa0, //
                0xff, 0x7f, // gyro_x
                0xff, 0x7f, // gyro_y
                0xff, 0x7f, // gyro_z
                0xff, 0x7f, // accel_x
                0x99, 0x99, // accel_y
                0xff, 0x7f, // accel_z
                0xfe, 0xf7, 0xef, 0x1f, // quat 0x1feff7fe (LE)
                0x00, 0x00, // left_x
                0x00, 0x00, // left_y
                0x00, 0x00, // right_x
                0x00, 0x00, // right_y
            ]
        );
        assert_eq!(FEEDBACK_STATE_BUF_SIZE_V9, 0x19);
    }

    /// Golden-Vektor B (gemischte Werte), ebenfalls per C-f32-Emulation berechnet.
    #[test]
    fn format_v9_mixed_matches_c_bytes() {
        let state = FeedbackState {
            gyro_x: 1.0,
            gyro_y: -30.0,
            gyro_z: 30.0,
            accel_x: 0.0,
            accel_y: 1.0,
            accel_z: -5.0,
            orient_x: 0.5,
            orient_y: -0.25,
            orient_z: 0.125,
            orient_w: -0.75,
            left_x: 0x1234,
            left_y: -0x2345,
            right_x: 0x7fff,
            right_y: -0x8000,
        };
        let buf = state.to_array_v9();
        assert_eq!(
            buf,
            [
                0xa0, //
                0x43, 0x84, // gyro_x = 0x8443
                0x00, 0x00, // gyro_y = 0x0000 (Min)
                0xff, 0xff, // gyro_z = 0xffff (Max)
                0xff, 0x7f, // accel_x
                0x99, 0x99, // accel_y
                0x00, 0x00, // accel_z (Min)
                0xa7, 0x5d, 0x8a, 0x25, // quat 0x258a5da7 (LE)
                0x34, 0x12, // left_x (big-endian)
                0xbb, 0xdc, // left_y (big-endian, -0x2345)
                0xff, 0x7f, // right_x
                0x00, 0x80, // right_y (big-endian, -0x8000)
            ]
        );
    }

    #[test]
    fn format_v12_appends_trailer() {
        let state = FeedbackState::default();
        let buf = state.to_array_v12();
        assert_eq!(buf.len(), FEEDBACK_STATE_BUF_SIZE_V12);
        assert_eq!(&buf[..0x19], &state.to_array_v9()[..]);
        assert_eq!(buf[0x19], 0x0);
        assert_eq!(buf[0x1a], 0x0);
        assert_eq!(buf[0x1b], 0x1);
    }

    #[test]
    fn format_rejects_small_buffers() {
        let state = FeedbackState::default();
        let mut small = [0u8; 0x18];
        assert_eq!(state.format_v9(&mut small), Err(ChiakiError::BufTooSmall));
        let mut v11 = [0u8; 0x1b];
        assert_eq!(state.format_v12(&mut v11), Err(ChiakiError::BufTooSmall));
        // format_v9 in größerem Buffer beschreibt nur die ersten 0x19 Bytes
        let mut big = [0xaau8; 0x1c];
        state.format_v9(&mut big).unwrap();
        assert_eq!(&big[..0x19], &state.to_array_v9()[..]);
        assert_eq!(&big[0x19..], &[0xaa, 0xaa, 0xaa]);
    }

    #[test]
    fn compress_quat_identity_and_sign_index() {
        // Identity: w größte Komponente -> largest=3, Vorzeichenbit 0 -> 6 | Felder
        let q = [0.0f32, 0.0, 0.0, 1.0];
        assert_eq!(compress_quat(&q), 0x1feff7fe);

        // negierte Quaternion repräsentiert dieselbe Rotation:
        // largest=3, Vorzeichenbit 1, Felder: |w| -> 255 für die restlichen
        let q = [0.0f32, 0.0, 0.0, -1.0];
        assert_eq!(compress_quat(&q), 0x1feff7ff);

        // x = 1: largest=0 -> (0 | 0<<1), Felder sind y/z/w = 0 -> 255er-Muster
        let q = [1.0f32, 0.0, 0.0, 0.0];
        assert_eq!(compress_quat(&q), 0x1feff7fe & !0x7u32);
    }

    #[test]
    fn history_event_button_two_byte_and_three_byte_variants() {
        let mut event = FeedbackHistoryEvent::default();

        // Buttons mit drittem State-Byte
        event.set_button(BUTTON_CROSS as u64, 0xff).unwrap();
        assert_eq!(event.len, 3);
        assert_eq!(&event.buf[..3], &[0x80, 0x88, 0xff]);
        event.set_button(BUTTON_CROSS as u64, 0).unwrap();
        assert_eq!(&event.buf[..3], &[0x80, 0x88, 0x00]);

        event.set_button(BUTTON_DPAD_UP as u64, 0x7f).unwrap();
        assert_eq!(&event.buf[..3], &[0x80, 0x80, 0x7f]);

        event.set_button(ANALOG_BUTTON_L2 as u64, 0x42).unwrap();
        assert_eq!(&event.buf[..3], &[0x80, 0x86, 0x42]);
        event.set_button(ANALOG_BUTTON_R2 as u64, 0x11).unwrap();
        assert_eq!(&event.buf[..3], &[0x80, 0x87, 0x11]);

        // L3/R3/Options/Share/Touchpad/PS kodieren den State ins zweite Byte
        event.set_button(BUTTON_L3 as u64, 0xff).unwrap();
        assert_eq!(event.len, 2);
        assert_eq!(&event.buf[..2], &[0x80, 0xaf]);
        event.set_button(BUTTON_L3 as u64, 0).unwrap();
        assert_eq!(&event.buf[..2], &[0x80, 0x8f]);
        event.set_button(BUTTON_R3 as u64, 0xff).unwrap();
        assert_eq!(&event.buf[..2], &[0x80, 0xb0]);
        event.set_button(BUTTON_R3 as u64, 0).unwrap();
        assert_eq!(&event.buf[..2], &[0x80, 0x90]);
        event.set_button(BUTTON_OPTIONS as u64, 0xff).unwrap();
        assert_eq!(&event.buf[..2], &[0x80, 0xac]);
        event.set_button(BUTTON_OPTIONS as u64, 0).unwrap();
        assert_eq!(&event.buf[..2], &[0x80, 0x8c]);
        event.set_button(BUTTON_SHARE as u64, 0xff).unwrap();
        assert_eq!(&event.buf[..2], &[0x80, 0xad]);
        event.set_button(BUTTON_SHARE as u64, 0).unwrap();
        assert_eq!(&event.buf[..2], &[0x80, 0x8d]);
        event.set_button(BUTTON_TOUCHPAD as u64, 0xff).unwrap();
        assert_eq!(&event.buf[..2], &[0x80, 0xb1]);
        event.set_button(BUTTON_TOUCHPAD as u64, 0).unwrap();
        assert_eq!(&event.buf[..2], &[0x80, 0x91]);
        event.set_button(BUTTON_PS as u64, 0xff).unwrap();
        assert_eq!(&event.buf[..2], &[0x80, 0xae]);
        event.set_button(BUTTON_PS as u64, 0).unwrap();
        assert_eq!(&event.buf[..2], &[0x80, 0x8e]);

        // Unbekannter Button -> InvalidData
        assert_eq!(event.set_button(1 << 31, 0xff), Err(ChiakiError::InvalidData));
    }

    #[test]
    fn history_event_touchpad_packing() {
        let mut event = FeedbackHistoryEvent::default();
        // C: buf[2] = (uint8_t)(x >> 4) — 0x1234 >> 4 = 0x123, truncated zu 0x23
        event.set_touchpad(true, 0x55, 0x1234, 0x03ab);
        assert_eq!(event.len, 5);
        assert_eq!(event.buf[..5], [0xd0, 0x55, 0x23, 0x43, 0xab]);
        // pointer_id wird auf 7 Bit maskiert
        event.set_touchpad(false, 0xff, 0, 0);
        assert_eq!(event.buf[..5], [0xc0, 0x7f, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn history_buffer_push_front_and_format() {
        let mut buffer = FeedbackHistoryBuffer::new(4);

        let mut ev = FeedbackHistoryEvent::default();
        ev.set_button(BUTTON_CROSS as u64, 0xff).unwrap();
        buffer.push(ev); // [cross_down]

        let mut ev2 = FeedbackHistoryEvent::default();
        ev2.set_button(BUTTON_CROSS as u64, 0).unwrap();
        buffer.push(ev2); // [cross_up, cross_down]

        let mut ev3 = FeedbackHistoryEvent::default();
        ev3.set_touchpad(true, 1, 0x123, 0x45);
        buffer.push(ev3); // [touch_down, cross_up, cross_down]

        let mut out = [0u8; 64];
        let written = buffer.format(&mut out).unwrap();
        assert_eq!(written, 3 + 3 + 5);
        // touch_down(true, 1, 0x123, 0x45) -> [d0 01 12 30 45] (C-Packing)
        assert_eq!(
            &out[..written],
            &[0xd0, 0x01, 0x12, 0x30, 0x45, 0x80, 0x88, 0x00, 0x80, 0x88, 0xff]
        );

        // Ringpuffer: ein vierter Push füllt den Puffer (size=4),
        // ein fünfter verdrängt das älteste Event.
        let mut ev4 = FeedbackHistoryEvent::default();
        ev4.set_button(BUTTON_PS as u64, 0xff).unwrap();
        buffer.push(ev4);
        assert_eq!(buffer.len(), 4);
        let mut ev5 = FeedbackHistoryEvent::default();
        ev5.set_button(BUTTON_PS as u64, 0).unwrap();
        buffer.push(ev5); // Puffer voll, len bleibt 4, touch_down ist raus
        assert_eq!(buffer.len(), 4);

        let written = buffer.format(&mut out).unwrap();
        // neuestes zuerst: ps_up, ps_down (PS = 2-Byte-Events ohne State-Byte),
        // touch_down, cross_up
        assert_eq!(
            &out[..written],
            &[0x80, 0x8e, 0x80, 0xae, 0xd0, 0x01, 0x12, 0x30, 0x45, 0x80, 0x88, 0x00]
        );
    }

    #[test]
    fn history_buffer_format_buf_too_small() {
        let mut buffer = FeedbackHistoryBuffer::new(2);
        let mut ev = FeedbackHistoryEvent::default();
        ev.set_touchpad(true, 0, 0, 0);
        buffer.push(ev);
        buffer.push(ev);
        let mut out = [0u8; 9]; // 2 * 5 = 10 passt nicht
        assert_eq!(buffer.format(&mut out), Err(ChiakiError::BufTooSmall));
        let mut out = [0u8; 10];
        assert_eq!(buffer.format(&mut out), Ok(10));
    }

    #[test]
    fn history_buffer_truncate_len() {
        let mut buffer = FeedbackHistoryBuffer::new(8);
        for i in 0..6u8 {
            let mut ev = FeedbackHistoryEvent::default();
            ev.set_button(BUTTON_CROSS as u64, i).unwrap();
            buffer.push(ev);
        }
        assert_eq!(buffer.len(), 6);
        buffer.truncate_len(4);
        assert_eq!(buffer.len(), 4);
        buffer.truncate_len(8); // vergrößert nicht
        assert_eq!(buffer.len(), 4);
    }
}
