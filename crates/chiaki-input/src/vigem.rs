// SPDX-License-Identifier: AGPL-3.0-only
// Virtueller Controller über ViGEmBus (vigem-client-Crate 0.1.4) — damit
// XInput-erwartende Programme (Steam, Spiele) die Fernsteuerung nutzen.
//
// Port-Grundlage: chiaki-ng "Moonlight"-ähnliche DS4-Ausgabe — chiaki-ng selbst
// emuliert kein ViGEm-Gerät, dieser Baustein ist eine Windows-spezifische
// Ergänzung (siehe RUST-REBUILD.md: ViGEm-DS4-Emu).
//
// API-Status vigem-client 0.1.4 (geprüft gegen die Crate-Quelle):
//   - Xbox360Wired + XGamepad: vollständig nutzbar
//   - DualShock4Wired + DS4Report: vorhanden, hinter feature "unstable_ds4"
//     (offiziell "under development"); wir nutzen es, markieren die API aber
//     als instabil
//   - KEIN Rumble-Feedback: die Crate exponiert keine
//     Notification-Callbacks (VIGEM_TARGET-XINPUT-Notification fehlt) —
//     Rumble vom Host zurück in die Session ist hier NICHT möglich (TODO,
//     sobald die Crate das kann oder wir eigene FFI erlauben)
//
// Kein unsafe in diesem Modul: vigem-client kapselt die ViGEm-FFI selbst.

use std::borrow::Borrow;

use vigem_client::{
    Client, DS4Report, DualShock4Wired, TargetId, XButtons, XGamepad, Xbox360Wired,
};

use chiaki_core::controller::{
    ANALOG_BUTTON_L2, ANALOG_BUTTON_R2, BUTTON_BOX, BUTTON_CROSS, BUTTON_DPAD_DOWN,
    BUTTON_DPAD_LEFT, BUTTON_DPAD_RIGHT, BUTTON_DPAD_UP, BUTTON_L1, BUTTON_L3, BUTTON_MOON,
    BUTTON_OPTIONS, BUTTON_PS, BUTTON_PYRAMID, BUTTON_R1, BUTTON_R3, BUTTON_SHARE,
    BUTTON_TOUCHPAD, ControllerState,
};

use crate::InputError;

fn vigem_err(e: vigem_client::Error) -> InputError {
    InputError::Vigem(e.to_string())
}

// ---------------------------------------------------------------------------
// Mapping-Funktionen (pure, getestet)
// ---------------------------------------------------------------------------

/// Chiaki-Buttons -> XInput-Flags (physische Positionen wie das SDL-Mapping:
/// A=unten=CROSS, B=rechts=MOON, X=links=BOX, Y=oben=PYRAMID).
pub fn buttons_to_xinput(buttons: u32) -> u16 {
    let mut out = 0u16;
    if buttons & BUTTON_CROSS != 0 {
        out |= XButtons::A;
    }
    if buttons & BUTTON_MOON != 0 {
        out |= XButtons::B;
    }
    if buttons & BUTTON_BOX != 0 {
        out |= XButtons::X;
    }
    if buttons & BUTTON_PYRAMID != 0 {
        out |= XButtons::Y;
    }
    if buttons & BUTTON_DPAD_UP != 0 {
        out |= XButtons::UP;
    }
    if buttons & BUTTON_DPAD_DOWN != 0 {
        out |= XButtons::DOWN;
    }
    if buttons & BUTTON_DPAD_LEFT != 0 {
        out |= XButtons::LEFT;
    }
    if buttons & BUTTON_DPAD_RIGHT != 0 {
        out |= XButtons::RIGHT;
    }
    if buttons & BUTTON_L1 != 0 {
        out |= XButtons::LB;
    }
    if buttons & BUTTON_R1 != 0 {
        out |= XButtons::RB;
    }
    if buttons & BUTTON_L3 != 0 {
        out |= XButtons::LTHUMB;
    }
    if buttons & BUTTON_R3 != 0 {
        out |= XButtons::RTHUMB;
    }
    if buttons & BUTTON_OPTIONS != 0 {
        out |= XButtons::START;
    }
    if buttons & BUTTON_SHARE != 0 {
        out |= XButtons::BACK;
    }
    if buttons & BUTTON_PS != 0 {
        out |= XButtons::GUIDE;
    }
    out
}

/// Chiaki-Buttons -> DS4-Buttons u16 (wButtons, high bits; DPad separat im
/// low nibble — Konstanten aus ViGEmClient include/ViGEm/Common.h).
pub mod ds4_buttons {
    /// DS4_BUTTONS (ViGEm Common.h).
    pub const THUMB_RIGHT: u16 = 1 << 15;
    pub const THUMB_LEFT: u16 = 1 << 14;
    pub const OPTIONS: u16 = 1 << 13;
    pub const SHARE: u16 = 1 << 12;
    pub const TRIGGER_RIGHT: u16 = 1 << 11;
    pub const TRIGGER_LEFT: u16 = 1 << 10;
    pub const SHOULDER_RIGHT: u16 = 1 << 9;
    pub const SHOULDER_LEFT: u16 = 1 << 8;
    pub const TRIANGLE: u16 = 1 << 7;
    pub const CIRCLE: u16 = 1 << 6;
    pub const CROSS: u16 = 1 << 5;
    pub const SQUARE: u16 = 1 << 4;
    /// DS4_SPECIAL_BUTTONS.
    pub const SPECIAL_PS: u8 = 1 << 0;
    pub const SPECIAL_TOUCHPAD: u8 = 1 << 1;
}

/// Chiaki-Buttons -> DS4 wButtons (ohne DPad-Nibble, siehe buttons_to_ds4).
pub fn ds4_face_buttons(buttons: u32) -> u16 {
    let mut out = 0u16;
    if buttons & BUTTON_CROSS != 0 {
        out |= ds4_buttons::CROSS;
    }
    if buttons & BUTTON_MOON != 0 {
        out |= ds4_buttons::CIRCLE;
    }
    if buttons & BUTTON_BOX != 0 {
        out |= ds4_buttons::SQUARE;
    }
    if buttons & BUTTON_PYRAMID != 0 {
        out |= ds4_buttons::TRIANGLE;
    }
    if buttons & BUTTON_L1 != 0 {
        out |= ds4_buttons::SHOULDER_LEFT;
    }
    if buttons & BUTTON_R1 != 0 {
        out |= ds4_buttons::SHOULDER_RIGHT;
    }
    if buttons & BUTTON_L3 != 0 {
        out |= ds4_buttons::THUMB_LEFT;
    }
    if buttons & BUTTON_R3 != 0 {
        out |= ds4_buttons::THUMB_RIGHT;
    }
    if buttons & BUTTON_OPTIONS != 0 {
        out |= ds4_buttons::OPTIONS;
    }
    if buttons & BUTTON_SHARE != 0 {
        out |= ds4_buttons::SHARE;
    }
    if buttons & ANALOG_BUTTON_L2 != 0 {
        out |= ds4_buttons::TRIGGER_LEFT;
    }
    if buttons & ANALOG_BUTTON_R2 != 0 {
        out |= ds4_buttons::TRIGGER_RIGHT;
    }
    out
}

/// Mappt einen kompletten ControllerState auf den XInput-Report.
/// Touches/Gyro haben im XInput-Report keinen Platz (dokumentiert).
pub fn state_to_xgamepad(state: &ControllerState) -> XGamepad {
    XGamepad {
        buttons: XButtons {
            raw: buttons_to_xinput(state.buttons),
        },
        left_trigger: state.l2_state,
        right_trigger: state.r2_state,
        thumb_lx: state.left_x,
        thumb_ly: state.left_y,
        thumb_rx: state.right_x,
        thumb_ry: state.right_y,
    }
}

/// Mappt einen kompletten ControllerState auf den DS4-Report (PS-BUTTONS
/// semantisch 1:1: CROSS bleibt CROSS; Trigger analog + digital).
pub fn state_to_ds4_report(state: &ControllerState) -> DS4Report {
    // sticks: i16 (-32768..32767) -> u8 (0..255, 0x80 Mitte) — Umkehrung der
    // SDL-Formel (value*257-32768), siehe dualsense::i16_to_stick.
    let stick = |v: i16| crate::dualsense::i16_to_stick(v);
    DS4Report {
        thumb_lx: stick(state.left_x),
        thumb_ly: stick(state.left_y),
        thumb_rx: stick(state.right_x),
        thumb_ry: stick(state.right_y),
        // buttons: DPad-Nibble (low) + Face/Shoulder-Bits (high)
        buttons: crate::dualsense::buttons_to_hat(state.buttons) as u16
            | ds4_face_buttons(state.buttons),
        special: {
            let mut s = 0u8;
            if state.buttons & BUTTON_PS != 0 {
                s |= ds4_buttons::SPECIAL_PS;
            }
            if state.buttons & BUTTON_TOUCHPAD != 0 {
                s |= ds4_buttons::SPECIAL_TOUCHPAD;
            }
            s
        },
        trigger_l: state.l2_state,
        trigger_r: state.r2_state,
    }
}

// ---------------------------------------------------------------------------
// Virtuelle Controller
// ---------------------------------------------------------------------------

/// Virtueller DualShock 4 über ViGEmBus. Nutzt die "unstable_ds4"-API der
/// vigem-client-Crate — funktional, aber offiziell unter-development.
pub struct VirtualDs4 {
    target: DualShock4Wired<Client>,
}

impl VirtualDs4 {
    /// Verbindet zum ViGEmBus, plugt einen DS4 ein und wartet auf Bereit-
    /// schaft. Fehler wenn der ViGEmBus-Treiber fehlt.
    pub fn new() -> Result<Self, InputError> {
        let client = Client::connect().map_err(vigem_err)?;
        let mut target = DualShock4Wired::new(
            client,
            TargetId::DUALSHOCK4_WIRED, // 0x054C:0x05C4, wie ViGEm-Default
        );
        target.plugin().map_err(vigem_err)?;
        target.wait_ready().map_err(vigem_err)?;
        Ok(VirtualDs4 { target })
    }

    /// Sendet den Chiaki-State als DS4-Report.
    pub fn send_controller_state(&mut self, state: &ControllerState) -> Result<(), InputError> {
        let report = state_to_ds4_report(state);
        self.target.update(&report).map_err(vigem_err)
    }

    /// XInput-User-Index des virtuellen Pads (für UI-Anzeige).
    pub fn user_index(&mut self) -> Result<u32, InputError> {
        // DualShock4Wired hat kein get_user_index — nur XBox360.
        Err(InputError::Other("DS4-Target: kein XInput-User-Index".into()))
    }
}

impl Drop for VirtualDs4 {
    fn drop(&mut self) {
        // Drop des Targets unplug't automatisch (vigem-client).
    }
}

/// Virtueller Xbox-360-Controller (XInput) über ViGEmBus — der zuverlässige
/// Pfad für Steam/XInput-Spiele.
pub struct VirtualXbox360 {
    target: Xbox360Wired<Client>,
}

impl VirtualXbox360 {
    pub fn new() -> Result<Self, InputError> {
        let client = Client::connect().map_err(vigem_err)?;
        let mut target = Xbox360Wired::new(client, TargetId::XBOX360_WIRED);
        target.plugin().map_err(vigem_err)?;
        target.wait_ready().map_err(vigem_err)?;
        Ok(VirtualXbox360 { target })
    }

    pub fn send_controller_state(&mut self, state: &ControllerState) -> Result<(), InputError> {
        let gamepad = state_to_xgamepad(state);
        self.target.update(&gamepad).map_err(vigem_err)
    }

    /// XInput-User-Index (0..3) des virtuellen Pads.
    pub fn user_index(&mut self) -> Result<u32, InputError> {
        self.target.get_user_index().map_err(vigem_err)
    }
}

impl Drop for VirtualXbox360 {
    fn drop(&mut self) {}
}

// Client ist Borrow-typisch benutzt; Import nutzen (kein unused-Import).
#[allow(dead_code)]
fn _borrow_witness<C: Borrow<Client>>(_c: &C) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xinput_mapping_matches_sdl_position_layout() {
        // SDL: A->CROSS, B->MOON, X->BOX, Y->PYRAMID
        assert_eq!(buttons_to_xinput(BUTTON_CROSS), XButtons::A);
        assert_eq!(buttons_to_xinput(BUTTON_MOON), XButtons::B);
        assert_eq!(buttons_to_xinput(BUTTON_BOX), XButtons::X);
        assert_eq!(buttons_to_xinput(BUTTON_PYRAMID), XButtons::Y);
        assert_eq!(
            buttons_to_xinput(BUTTON_DPAD_UP | BUTTON_L1 | BUTTON_OPTIONS),
            XButtons::UP | XButtons::LB | XButtons::START
        );
        assert_eq!(
            buttons_to_xinput(BUTTON_PS),
            XButtons::GUIDE,
            "PS-Button -> Guide"
        );
        assert_eq!(buttons_to_xinput(BUTTON_SHARE), XButtons::BACK);
        assert_eq!(buttons_to_xinput(BUTTON_L3 | BUTTON_R3), XButtons::LTHUMB | XButtons::RTHUMB);
    }

    #[test]
    fn xgamepad_report_layout() {
        let mut state = ControllerState::default();
        state.buttons = BUTTON_CROSS | BUTTON_R1;
        state.l2_state = 0x40;
        state.r2_state = 0xff;
        state.left_x = -32767;
        state.right_y = 12345;

        let gp = state_to_xgamepad(&state);
        assert_eq!(gp.buttons.raw, XButtons::A | XButtons::RB);
        assert_eq!(gp.left_trigger, 0x40);
        assert_eq!(gp.right_trigger, 0xff);
        assert_eq!(gp.thumb_lx, -32767, "XInput-Achsen sind i16 1:1");
        assert_eq!(gp.thumb_ry, 12345);
    }

    #[test]
    fn ds4_button_constants_match_vigem_common_h() {
        // Konstanten aus ViGEmClient include/ViGEm/Common.h (hardcoded)
        assert_eq!(ds4_buttons::THUMB_RIGHT, 1 << 15);
        assert_eq!(ds4_buttons::THUMB_LEFT, 1 << 14);
        assert_eq!(ds4_buttons::OPTIONS, 1 << 13);
        assert_eq!(ds4_buttons::SHARE, 1 << 12);
        assert_eq!(ds4_buttons::TRIGGER_RIGHT, 1 << 11);
        assert_eq!(ds4_buttons::TRIGGER_LEFT, 1 << 10);
        assert_eq!(ds4_buttons::SHOULDER_RIGHT, 1 << 9);
        assert_eq!(ds4_buttons::SHOULDER_LEFT, 1 << 8);
        assert_eq!(ds4_buttons::TRIANGLE, 1 << 7);
        assert_eq!(ds4_buttons::CIRCLE, 1 << 6);
        assert_eq!(ds4_buttons::CROSS, 1 << 5);
        assert_eq!(ds4_buttons::SQUARE, 1 << 4);
        assert_eq!(ds4_buttons::SPECIAL_PS, 0x01);
        assert_eq!(ds4_buttons::SPECIAL_TOUCHPAD, 0x02);
    }

    #[test]
    fn ds4_report_layout_matches_vigem_ds4_report() {
        // PS-semantische Buttons: CROSS->CROSS etc.
        let mut state = ControllerState::default();
        state.buttons = BUTTON_CROSS
            | BUTTON_BOX
            | BUTTON_MOON
            | BUTTON_PYRAMID
            | BUTTON_PS
            | BUTTON_TOUCHPAD
            | BUTTON_DPAD_LEFT;
        state.l2_state = 0x12;
        state.r2_state = 0x34;
        state.left_x = -32767; // -> 0 ((−32767+32768)/257)
        state.left_y = 0; // -> 0x80
        state.right_x = 0x7fff; // -> (32767+32768)/257 = 255
        state.right_y = -32511; // -> 1 (exakt u8 1)

        let report = state_to_ds4_report(&state);
        // Face-Buttons (PS-semantisch, anders als XInput!)
        assert_eq!(report.buttons & ds4_buttons::CROSS, ds4_buttons::CROSS);
        assert_eq!(report.buttons & ds4_buttons::CIRCLE, ds4_buttons::CIRCLE);
        assert_eq!(report.buttons & ds4_buttons::SQUARE, ds4_buttons::SQUARE);
        assert_eq!(report.buttons & ds4_buttons::TRIANGLE, ds4_buttons::TRIANGLE);
        // DPad im low nibble
        assert_eq!(report.buttons & 0xF, 6, "DPAD_LEFT -> Hat 6");
        // special
        assert_eq!(
            report.special,
            ds4_buttons::SPECIAL_PS | ds4_buttons::SPECIAL_TOUCHPAD
        );
        // Trigger analog
        assert_eq!(report.trigger_l, 0x12);
        assert_eq!(report.trigger_r, 0x34);
        // Sticks u8 mit 0x80-Mitte (Umkehrung der SDL-Formel)
        assert_eq!(report.thumb_lx, 0);
        assert_eq!(report.thumb_ly, 0x80);
        assert_eq!(report.thumb_rx, 255);
        assert_eq!(report.thumb_ry, 1);
        // Default-Report des crates hat buttons=0x8 (DPad NONE) — unsere
        // Konstruktion mit neutralen Bits muss dasselbe ergeben.
        let neutral = state_to_ds4_report(&ControllerState::default());
        assert_eq!(neutral.buttons, 0x8);
        assert_eq!(neutral.thumb_lx, 0x80);
    }

    // Hardware-Test — braucht den ViGEmBus-Treiber (ViGEm/ViGEmBus installieren):
    // `cargo test -p chiaki-input -- --ignored` ausführen; prüfen, ob in
    // Windows "Gamecontroller" ein DualShock 4 erscheint.
    #[test]
    #[ignore = "braucht den ViGEmBus-Treiber"]
    fn hw_virtual_ds4_plugin_and_move() {
        let mut ds4 = VirtualDs4::new().expect("ViGEmBus nicht erreichbar?");
        let mut state = ControllerState::default();
        state.buttons = BUTTON_CROSS;
        state.left_x = 0x7fff;
        ds4.send_controller_state(&state).expect("update");
        std::thread::sleep(std::time::Duration::from_millis(500));
        state.left_x = 0;
        ds4.send_controller_state(&state).expect("update 2");
    }

    #[test]
    fn stick_roundtrip_matches_sdl_formula() {
        // stick_to_i16 (DS in) und i16_to_stick (ViGEm out) sind Umkehrungen
        // im 8-bit-Raster.
        for v in [0u8, 1, 64, 128, 200, 255] {
            assert_eq!(crate::dualsense::i16_to_stick(crate::dualsense::stick_to_i16(v)), v);
        }
    }
}
