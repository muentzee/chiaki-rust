// SPDX-License-Identifier: AGPL-3.0-only
// chiaki-input: Gamepad/Tastatur-Eingabe für den chiaki-ng-Rust-Port (Windows-only).
//
// Port der Input-Teile von chiaki-ng:
//   - gui/src/controllermanager.cpp (SDL-Controller-Handling) -> gamepad.rs (gilrs) +
//     dualsense.rs (native hidapi für DualSense/DualShock4 — gilrs benutzt unter
//     Windows nur XInput und sieht Sony-Pads deshalb gar nicht)
//   - gui/src/settings.cpp (Tastatur-Mapping "keymap/...") + streamsession.cpp
//     (HandleKeyboardEvent) -> keyboard.rs
//   - ViGEm-DS4-Emulation -> vigem.rs (XInput/DS4-Virtual-Controller für
//     Steam/XInput-Spiele)
//
// Kein unsafe im gesamten Crate: gilrs/hidapi/vigem-client sind safe-Wrappers, die
// Haptics-Ausgabe benutzt die sicheren windows-crate-Bindings (FFI-Details in
// dualsense::haptics::ffi, dort mit Modul-Attribut freigegeben).

#![deny(unsafe_code)]

pub mod dualsense;
pub mod gamepad;
pub mod keyboard;
pub mod vigem;

pub use dualsense::haptics::HapticsPlayer;
pub use dualsense::{DualSenseDevice, DualSenseManager, NativeError};
pub use gamepad::{DeviceId, GamepadDeviceInfo, GamepadEvent, GamepadManager};
pub use keyboard::{ButtonOrAxis, Key, KeyboardMapper};
pub use vigem::{VirtualDs4, VirtualXbox360};

use chiaki_core::controller::ControllerState;

/// Fehler-Typ der Crate (Konvention: ein Error-Enum pro Crate, thiserror).
#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("gilrs-Fehler: {0}")]
    Gilrs(String),
    #[error("hidapi-Fehler: {0}")]
    Hid(#[from] hidapi::HidError),
    #[error("ViGEm-Fehler: {0}")]
    Vigem(String),
    #[error("Haptics-Audio-Fehler: {0}")]
    Audio(String),
    #[error("{0}")]
    Other(String),
}

/// Kombiniert mehrere Controller-States wie `chiaki_controller_state_or()`
/// (Comfort-Funktion für die Session, die Gamepad + DualSense + Tastatur
/// zusammenführt — wie StreamSession::SendFeedbackState im C++).
pub fn combine_states(states: &[ControllerState]) -> ControllerState {
    let mut out = ControllerState::default();
    for s in states {
        out = ControllerState::or(&out, s);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chiaki_core::controller::{BUTTON_CROSS, BUTTON_PS};

    #[test]
    fn combine_states_or_semantics() {
        let mut a = ControllerState::default();
        let mut b = ControllerState::default();
        a.buttons = BUTTON_CROSS;
        b.buttons = BUTTON_PS;
        b.r2_state = 0xff;

        let out = combine_states(&[a, b]);
        assert_eq!(out.buttons, BUTTON_CROSS | BUTTON_PS);
        assert_eq!(out.r2_state, 0xff);
    }
}
