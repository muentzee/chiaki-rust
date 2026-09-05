// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/controller.c + lib/include/chiaki/controller.h (chiaki-ng).
//
// Reine Datenstruktur + Manipulatoren für den Controller-State, wie er über
// Takion-Feedback an die Konsole gesendet wird.

/// `TOUCH_ID_MASK` (controller.c).
pub const TOUCH_ID_MASK: u8 = 0x7f;

/// `CHIAKI_CONTROLLER_TOUCHES_MAX`.
pub const TOUCHES_MAX: usize = 2;

/// `CHIAKI_CONTROLLER_BUTTONS_COUNT`.
pub const BUTTONS_COUNT: usize = 16;

// --- ChiakiControllerButton (Bitmasken, Werte dürfen sich nicht ändern) ---
pub const BUTTON_CROSS: u32 = 1 << 0;
pub const BUTTON_MOON: u32 = 1 << 1;
pub const BUTTON_BOX: u32 = 1 << 2;
pub const BUTTON_PYRAMID: u32 = 1 << 3;
pub const BUTTON_DPAD_LEFT: u32 = 1 << 4;
pub const BUTTON_DPAD_RIGHT: u32 = 1 << 5;
pub const BUTTON_DPAD_UP: u32 = 1 << 6;
pub const BUTTON_DPAD_DOWN: u32 = 1 << 7;
pub const BUTTON_L1: u32 = 1 << 8;
pub const BUTTON_R1: u32 = 1 << 9;
pub const BUTTON_L3: u32 = 1 << 10;
pub const BUTTON_R3: u32 = 1 << 11;
pub const BUTTON_OPTIONS: u32 = 1 << 12;
pub const BUTTON_SHARE: u32 = 1 << 13;
pub const BUTTON_TOUCHPAD: u32 = 1 << 14;
pub const BUTTON_PS: u32 = 1 << 15;

// --- ChiakiControllerAnalogButton (darf sich nicht mit den Buttons überlappen) ---
pub const ANALOG_BUTTON_L2: u32 = 1 << 16;
pub const ANALOG_BUTTON_R2: u32 = 1 << 17;

/// Port von `ChiakiControllerTouch`. `id == -1` bedeutet "nicht gedrückt".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ControllerTouch {
    pub x: u16,
    pub y: u16,
    pub id: i8, // -1 = up
}

/// Port von `ChiakiControllerState`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ControllerState {
    /// Bitmask aus `ChiakiControllerButton` (Bits 16/17 sind die analogen
    /// Trigger als `ANALOG_BUTTON_*`, die `*_state`-Felder halten den Wert).
    pub buttons: u32,
    pub l2_state: u8,
    pub r2_state: u8,
    pub left_x: i16,
    pub left_y: i16,
    pub right_x: i16,
    pub right_y: i16,
    pub touch_id_next: u8,
    pub touches: [ControllerTouch; TOUCHES_MAX],
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
}

impl Default for ControllerState {
    fn default() -> Self {
        let mut state = ControllerState {
            buttons: 0,
            l2_state: 0,
            r2_state: 0,
            left_x: 0,
            left_y: 0,
            right_x: 0,
            right_y: 0,
            touch_id_next: 0,
            touches: [ControllerTouch::default(); TOUCHES_MAX],
            gyro_x: 0.0,
            gyro_y: 0.0,
            gyro_z: 0.0,
            accel_x: 0.0,
            accel_y: 0.0,
            accel_z: 0.0,
            orient_x: 0.0,
            orient_y: 0.0,
            orient_z: 0.0,
            orient_w: 0.0,
        };
        state.set_idle();
        state
    }
}

/// Epsilon aus dem `CHECKF`-Makro in controller.c (`0.0000001f`).
const FLOAT_EPS: f32 = 0.000_000_1;

impl ControllerState {
    /// Port von `chiaki_controller_state_set_idle()`.
    pub fn set_idle(&mut self) {
        self.buttons = 0;
        self.l2_state = 0;
        self.r2_state = 0;
        self.left_x = 0;
        self.left_y = 0;
        self.right_x = 0;
        self.right_y = 0;
        self.touch_id_next = 0;
        for touch in &mut self.touches {
            touch.id = -1;
            touch.x = 0;
            touch.y = 0;
        }
        self.gyro_x = 0.0;
        self.gyro_y = 0.0;
        self.gyro_z = 0.0;
        self.accel_x = 0.0;
        self.accel_y = 1.0; // Schwerkraft zeigt nach +Y
        self.accel_z = 0.0;
        self.orient_x = 0.0;
        self.orient_y = 0.0;
        self.orient_z = 0.0;
        self.orient_w = 1.0;
    }

    /// Port von `chiaki_controller_state_start_touch()`.
    ///
    /// Liefert die neu vergebene Touch-ID oder -1, wenn kein Slot frei ist.
    pub fn start_touch(&mut self, x: u16, y: u16) -> i8 {
        for touch in &mut self.touches {
            if touch.id < 0 {
                touch.id = self.touch_id_next as i8;
                self.touch_id_next = (self.touch_id_next + 1) & TOUCH_ID_MASK;
                touch.x = x;
                touch.y = y;
                return touch.id;
            }
        }
        -1
    }

    /// Port von `chiaki_controller_state_stop_touch()`.
    pub fn stop_touch(&mut self, id: u8) {
        for touch in &mut self.touches {
            if touch.id == id as i8 {
                touch.id = -1;
                break;
            }
        }
    }

    /// Port von `chiaki_controller_state_set_touch_pos()`.
    pub fn set_touch_pos(&mut self, id: u8, x: u16, y: u16) {
        let id = id & TOUCH_ID_MASK;
        for touch in &mut self.touches {
            if touch.id == id as i8 {
                touch.x = x;
                touch.y = y;
                break;
            }
        }
    }

    /// Port von `chiaki_controller_state_equals()` (inkl. `CHECKF`-Epsilon
    /// für alle float-Felder).
    pub fn equals(&self, other: &ControllerState) -> bool {
        let a = self;
        let b = other;
        if !(a.buttons == b.buttons
            && a.l2_state == b.l2_state
            && a.r2_state == b.r2_state
            && a.left_x == b.left_x
            && a.left_y == b.left_y
            && a.right_x == b.right_x
            && a.right_y == b.right_y)
        {
            return false;
        }

        for i in 0..TOUCHES_MAX {
            if a.touches[i].id != b.touches[i].id {
                return false;
            }
            // Bei aktiven Touches zählen auch die Koordinaten.
            if a.touches[i].id >= 0
                && (a.touches[i].x != b.touches[i].x || a.touches[i].y != b.touches[i].y)
            {
                return false;
            }
        }

        macro_rules! checkf {
            ($n:ident) => {
                // C: a->n < b->n - eps || a->n > b->n + eps
                if a.$n < b.$n - FLOAT_EPS || a.$n > b.$n + FLOAT_EPS {
                    return false;
                }
            };
        }
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

    /// Port von `chiaki_controller_state_or()`: Vereinigung zweier States.
    ///
    /// Ignoriert gyro/accel/orient als Kombination — stattdessen gewinnt der
    /// erste Controller, der überhaupt Motion-Daten liefert (z. B. DualSense
    /// vor Steam Deck); Mischen würde die Orientierungswerte kaputtmachen.
    pub fn or(a: &ControllerState, b: &ControllerState) -> ControllerState {
        let mut out = ControllerState {
            buttons: a.buttons | b.buttons,
            l2_state: a.l2_state.max(b.l2_state),
            r2_state: a.r2_state.max(b.r2_state),
            left_x: max_abs(a.left_x, b.left_x),
            left_y: max_abs(a.left_y, b.left_y),
            right_x: max_abs(a.right_x, b.right_x),
            right_y: max_abs(a.right_y, b.right_y),
            touch_id_next: 0,
            touches: [ControllerTouch::default(); TOUCHES_MAX],
            gyro_x: 0.0,
            gyro_y: 0.0,
            gyro_z: 0.0,
            accel_x: 0.0,
            accel_y: 0.0,
            accel_z: 0.0,
            orient_x: 0.0,
            orient_y: 0.0,
            orient_z: 0.0,
            orient_w: 0.0,
        };

        for i in 0..TOUCHES_MAX {
            out.touches[i] = if a.touches[i].id >= 0 {
                a.touches[i]
            } else if b.touches[i].id >= 0 {
                b.touches[i]
            } else {
                ControllerTouch { x: 0, y: 0, id: -1 }
            };
        }

        // If first value has gyro set take it, otherwise give 2nd value
        // prioritizes DualSense over Steam Deck for instance if it's attached
        // since setsu and controller_state are before steamdeck
        // don't mix gyro / accel values from different controllers with simple or
        // doing so will cause problems w/ orientation values among other things
        // since orient is calculated from accel and gyro, just check those.
        let mut chooser = false;
        macro_rules! setchooser {
            ($n:ident, $default:expr) => {
                if a.$n < $default - FLOAT_EPS || a.$n > $default + FLOAT_EPS {
                    chooser = true;
                }
            };
        }
        setchooser!(gyro_x, 0.0);
        setchooser!(gyro_y, 0.0);
        setchooser!(gyro_z, 0.0);
        setchooser!(accel_x, 0.0);
        setchooser!(accel_y, 1.0);
        setchooser!(accel_z, 0.0);

        macro_rules! choosef {
            ($n:ident) => {
                out.$n = if chooser { a.$n } else { b.$n };
            };
        }
        choosef!(gyro_x);
        choosef!(gyro_y);
        choosef!(gyro_z);
        choosef!(accel_x);
        choosef!(accel_y);
        choosef!(accel_z);
        choosef!(orient_x);
        choosef!(orient_y);
        choosef!(orient_z);
        choosef!(orient_w);

        out
    }
}

/// C: `MAX_ABS(a, b)` — `ABS(a) > ABS(b) ? a : b` (Vorzeichen des größeren Betrags gewinnt).
fn max_abs(a: i16, b: i16) -> i16 {
    if a.abs() > b.abs() {
        a
    } else {
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_values_match_c_enum() {
        assert_eq!(BUTTON_CROSS, 1 << 0);
        assert_eq!(BUTTON_MOON, 1 << 1);
        assert_eq!(BUTTON_BOX, 1 << 2);
        assert_eq!(BUTTON_PYRAMID, 1 << 3);
        assert_eq!(BUTTON_DPAD_LEFT, 1 << 4);
        assert_eq!(BUTTON_DPAD_RIGHT, 1 << 5);
        assert_eq!(BUTTON_DPAD_UP, 1 << 6);
        assert_eq!(BUTTON_DPAD_DOWN, 1 << 7);
        assert_eq!(BUTTON_L1, 1 << 8);
        assert_eq!(BUTTON_R1, 1 << 9);
        assert_eq!(BUTTON_L3, 1 << 10);
        assert_eq!(BUTTON_R3, 1 << 11);
        assert_eq!(BUTTON_OPTIONS, 1 << 12);
        assert_eq!(BUTTON_SHARE, 1 << 13);
        assert_eq!(BUTTON_TOUCHPAD, 1 << 14);
        assert_eq!(BUTTON_PS, 1 << 15);
        assert_eq!(ANALOG_BUTTON_L2, 1 << 16);
        assert_eq!(ANALOG_BUTTON_R2, 1 << 17);
        assert_eq!(BUTTONS_COUNT, 16);
        assert_eq!(TOUCHES_MAX, 2);
        assert_eq!(TOUCH_ID_MASK, 0x7f);
    }

    #[test]
    fn idle_state_matches_c() {
        let s = ControllerState::default();
        assert_eq!(s.buttons, 0);
        assert_eq!(s.l2_state, 0);
        assert_eq!(s.r2_state, 0);
        assert_eq!(s.left_x, 0);
        assert_eq!(s.right_y, 0);
        assert_eq!(s.touch_id_next, 0);
        for t in &s.touches {
            assert_eq!(t.id, -1);
        }
        assert_eq!(s.gyro_x, 0.0);
        assert_eq!(s.accel_y, 1.0);
        assert_eq!(s.orient_w, 1.0);
        assert_eq!(s.orient_x, 0.0);
    }

    #[test]
    fn touch_lifecycle_ids_wrap() {
        let mut s = ControllerState::default();
        let id0 = s.start_touch(100, 200);
        assert_eq!(id0, 0);
        let id1 = s.start_touch(300, 400);
        assert_eq!(id1, 1);
        // Kein dritter Slot (CHIAKI_CONTROLLER_TOUCHES_MAX == 2)
        assert_eq!(s.start_touch(500, 600), -1);

        s.set_touch_pos(1, 310, 410);
        assert_eq!(s.touches[1], ControllerTouch { x: 310, y: 410, id: 1 });
        // set_touch_pos maskt die ID auf 7 Bit (wie im C)
        s.set_touch_pos(1 | !TOUCH_ID_MASK, 320, 420);
        assert_eq!(s.touches[1], ControllerTouch { x: 320, y: 420, id: 1 });

        s.stop_touch(0);
        assert_eq!(s.touches[0].id, -1);
        assert_eq!(s.touches[1].id, 1);

        let id2 = s.start_touch(1, 2);
        assert_eq!(id2, 2, "nächste ID aus touch_id_next");
        assert_eq!(s.touches[0], ControllerTouch { x: 1, y: 2, id: 2 });
    }

    #[test]
    fn touch_id_wraps_at_0x7f() {
        let mut s = ControllerState::default();
        s.touch_id_next = 0x7f;
        let id = s.start_touch(0, 0);
        assert_eq!(id, 0x7f);
        // Wrapping auf 0 für den nächsten Touch
        s.stop_touch(0x7f);
        let id2 = s.start_touch(0, 0);
        assert_eq!(id2, 0);
    }

    #[test]
    fn equals_with_float_epsilon() {
        let mut a = ControllerState::default();
        let mut b = ControllerState::default();
        assert!(a.equals(&b));

        b.gyro_x = a.gyro_x + FLOAT_EPS / 2.0;
        assert!(a.equals(&b), "innerhalb des Epsilons gleich");
        b.gyro_x = a.gyro_x + FLOAT_EPS * 2.0;
        assert!(!a.equals(&b), "außerhalb des Epsilons ungleich");

        b = ControllerState::default();
        b.buttons = BUTTON_CROSS;
        assert!(!a.equals(&b));
        b = ControllerState::default();
        b.touches[0] = ControllerTouch { x: 1, y: 2, id: 5 };
        assert!(!a.equals(&b));
        // Koordinaten bei id == -1 zählen nicht
        a.touches[0] = ControllerTouch { x: 9, y: 9, id: -1 };
        b.touches[0] = ControllerTouch { x: 1, y: 2, id: -1 };
        assert!(a.equals(&b), "bei id == -1 werden x/y ignoriert");
    }

    #[test]
    fn or_combines_states() {
        let mut a = ControllerState::default();
        let mut b = ControllerState::default();

        a.buttons = BUTTON_CROSS | BUTTON_L1;
        a.left_x = -200;
        b.buttons = BUTTON_CROSS | BUTTON_BOX;
        b.left_x = 300;
        b.r2_state = 0xff;

        let out = ControllerState::or(&a, &b);
        assert_eq!(out.buttons, BUTTON_CROSS | BUTTON_L1 | BUTTON_BOX);
        assert_eq!(out.l2_state, 0);
        assert_eq!(out.r2_state, 0xff);
        // MAX_ABS: +300 (Betrag größer als -200)
        assert_eq!(out.left_x, 300);

        // Touches: erster aktiver gewinnt
        a.touches[0] = ControllerTouch { x: 10, y: 20, id: 3 };
        b.touches[1] = ControllerTouch { x: 30, y: 40, id: 4 };
        let out = ControllerState::or(&a, &b);
        assert_eq!(out.touches[0], ControllerTouch { x: 10, y: 20, id: 3 });
        assert_eq!(out.touches[1], ControllerTouch { x: 30, y: 40, id: 4 });
    }

    #[test]
    fn or_chooses_first_motion_source() {
        // a liefert Motion-Daten -> alle float-Felder komplett aus a
        let mut a = ControllerState::default();
        let b = ControllerState::default();
        a.gyro_y = 1.5;
        a.orient_w = 0.25;

        let out = ControllerState::or(&a, &b);
        assert_eq!(out.gyro_y, 1.5);
        assert_eq!(out.orient_w, 0.25);

        // a ohne Motion -> chooser bleibt false -> Werte aus b
        let a = ControllerState::default();
        let mut b = ControllerState::default();
        b.orient_x = 0.125;
        let out = ControllerState::or(&a, &b);
        assert_eq!(out.orient_x, 0.125);
        assert_eq!(out.accel_y, 1.0);
    }

    #[test]
    fn max_abs_semantics() {
        assert_eq!(max_abs(-200, 300), 300);
        assert_eq!(max_abs(300, -200), 300);
        assert_eq!(max_abs(-300, 200), -300);
        assert_eq!(max_abs(-300, -200), -300);
        assert_eq!(max_abs(0, 0), 0);
    }
}
