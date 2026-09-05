// SPDX-License-Identifier: AGPL-3.0-only
// Tastatur -> Controller-Mapping.
//
// Port von:
//   - gui/include/settings.h ControllerButtonExt (EXT-Bits ab 1<<18)
//   - gui/src/settings.cpp Settings::GetControllerMapping() (Defaults) +
//     SetControllerButtonMapping() (settings-Keys "keymap/<button_name>")
//   - gui/src/streamsession.cpp StreamSession::HandleKeyboardEvent()
//     (Decode-Logik: ANALOG_BUTTON_L2 -> l2_state=0xff, Sticks -> ±0x7fff)
//
// Das neutrale `Key`-Enum ersetzt Qt::Key (GPUI liefert später eigene Key-
// Typen — die UI mappt sie über Key::from_qt_name oder direkt auf dieses
// Enum; für Bytekompatibilität der settings.ini gibt es to_qt_name(), das
// exakt die QKeySequence-Namen des C++ erzeugt (z. B. "Return", "Backslash")).

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use chiaki_core::controller::{
    ANALOG_BUTTON_L2, ANALOG_BUTTON_R2, BUTTON_BOX, BUTTON_CROSS, BUTTON_DPAD_DOWN,
    BUTTON_DPAD_LEFT, BUTTON_DPAD_RIGHT, BUTTON_DPAD_UP, BUTTON_L1, BUTTON_L3, BUTTON_MOON,
    BUTTON_OPTIONS, BUTTON_PS, BUTTON_PYRAMID, BUTTON_R1, BUTTON_R3, BUTTON_SHARE,
    BUTTON_TOUCHPAD, ControllerState,
};

/// ControllerButtonExt aus settings.h — Werte dürfen sich nicht mit den
/// ChiakiControllerButtons überlappen.
pub mod ext {
    pub const ANALOG_STICK_LEFT_X_UP: u32 = 1 << 18;
    pub const ANALOG_STICK_LEFT_X_DOWN: u32 = 1 << 19;
    pub const ANALOG_STICK_LEFT_Y_UP: u32 = 1 << 20;
    pub const ANALOG_STICK_LEFT_Y_DOWN: u32 = 1 << 21;
    pub const ANALOG_STICK_RIGHT_X_UP: u32 = 1 << 22;
    pub const ANALOG_STICK_RIGHT_X_DOWN: u32 = 1 << 23;
    pub const ANALOG_STICK_RIGHT_Y_UP: u32 = 1 << 24;
    pub const ANALOG_STICK_RIGHT_Y_DOWN: u32 = 1 << 25;
    pub const ANALOG_STICK_LEFT_X: u32 = 1 << 26;
    pub const ANALOG_STICK_LEFT_Y: u32 = 1 << 27;
    pub const ANALOG_STICK_RIGHT_X: u32 = 1 << 28;
    pub const ANALOG_STICK_RIGHT_Y: u32 = 1 << 29;
    pub const MISC1: u32 = 1 << 30;
}

/// Neutrales Tasten-Enum (ersetzt den Qt::Key-Typ des C++).
///
/// Fürs UI-Mapping: die GPUI-Keystrokes werden pro Taste auf dieses Enum
/// abgebildet (z. B. über die physische Position/WGL-scancode). Für die
/// settings-Kompatibilität liefern to_qt_name()/from_qt_name() die
/// QKeySequence-Texte aus dem C++ ("keymap/*"-Einträge).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Key {
    // Letters
    A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R, S, T, U, V, W, X, Y, Z,
    // Digits
    Num0, Num1, Num2, Num3, Num4, Num5, Num6, Num7, Num8, Num9,
    // Navigation
    Up, Down, Left, Right,
    Insert, Delete, Home, End, PageUp, PageDown,
    // Steuerung
    Return, Escape, Backspace, Tab, Space,
    // Modifier
    Shift, Control, Alt, Meta,
    // F-Tasten
    F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12,
    // ASCII-Interpunktion (Namen wie QKeySequence)
    BracketLeft,  // [
    BracketRight, // ]
    Backslash,    // \
    Minus,        // -
    Equal,        // =
    Comma,        // ,
    Period,       // .
    Slash,        // /
    Semicolon,    // ;
    Quote,        // '
    GraveAccent,  // `
    // Fallback für alles andere (virtueller KeyCode)
    Other(#[serde(skip)] u32),
}

impl Key {
    /// Qt-KeySequence-Name (settings-Kompatibilität: so speichert das C++
    /// "keymap/<button>"-Einträge, z. B. "Return").
    pub fn to_qt_name(self) -> String {
        match self {
            Key::A => "A",
            Key::B => "B",
            Key::C => "C",
            Key::D => "D",
            Key::E => "E",
            Key::F => "F",
            Key::G => "G",
            Key::H => "H",
            Key::I => "I",
            Key::J => "J",
            Key::K => "K",
            Key::L => "L",
            Key::M => "M",
            Key::N => "N",
            Key::O => "O",
            Key::P => "P",
            Key::Q => "Q",
            Key::R => "R",
            Key::S => "S",
            Key::T => "T",
            Key::U => "U",
            Key::V => "V",
            Key::W => "W",
            Key::X => "X",
            Key::Y => "Y",
            Key::Z => "Z",
            Key::Num0 => "0",
            Key::Num1 => "1",
            Key::Num2 => "2",
            Key::Num3 => "3",
            Key::Num4 => "4",
            Key::Num5 => "5",
            Key::Num6 => "6",
            Key::Num7 => "7",
            Key::Num8 => "8",
            Key::Num9 => "9",
            Key::Up => "Up",
            Key::Down => "Down",
            Key::Left => "Left",
            Key::Right => "Right",
            Key::Insert => "Ins",
            Key::Delete => "Del",
            Key::Home => "Home",
            Key::End => "End",
            Key::PageUp => "PgUp",
            Key::PageDown => "PgDown",
            Key::Return => "Return",
            Key::Escape => "Esc",
            Key::Backspace => "Backspace",
            Key::Tab => "Tab",
            Key::Space => "Space",
            Key::Shift => "Shift",
            Key::Control => "Ctrl",
            Key::Alt => "Alt",
            Key::Meta => "Meta",
            Key::F1 => "F1",
            Key::F2 => "F2",
            Key::F3 => "F3",
            Key::F4 => "F4",
            Key::F5 => "F5",
            Key::F6 => "F6",
            Key::F7 => "F7",
            Key::F8 => "F8",
            Key::F9 => "F9",
            Key::F10 => "F10",
            Key::F11 => "F11",
            Key::F12 => "F12",
            Key::BracketLeft => "[",
            Key::BracketRight => "]",
            Key::Backslash => "\\",
            Key::Minus => "-",
            Key::Equal => "=",
            Key::Comma => ",",
            Key::Period => ".",
            Key::Slash => "/",
            Key::Semicolon => ";",
            Key::Quote => "'",
            Key::GraveAccent => "`",
            Key::Other(code) => return format!("#0x{code:04x}"),
        }
        .to_owned()
    }

    /// Umkehrung von to_qt_name (liest "keymap/*"-Werte des C++-Clients).
    pub fn from_qt_name(name: &str) -> Option<Key> {
        let key = match name {
            "A" => Key::A,
            "B" => Key::B,
            "C" => Key::C,
            "D" => Key::D,
            "E" => Key::E,
            "F" => Key::F,
            "G" => Key::G,
            "H" => Key::H,
            "I" => Key::I,
            "J" => Key::J,
            "K" => Key::K,
            "L" => Key::L,
            "M" => Key::M,
            "N" => Key::N,
            "O" => Key::O,
            "P" => Key::P,
            "Q" => Key::Q,
            "R" => Key::R,
            "S" => Key::S,
            "T" => Key::T,
            "U" => Key::U,
            "V" => Key::V,
            "W" => Key::W,
            "X" => Key::X,
            "Y" => Key::Y,
            "Z" => Key::Z,
            "0" => Key::Num0,
            "1" => Key::Num1,
            "2" => Key::Num2,
            "3" => Key::Num3,
            "4" => Key::Num4,
            "5" => Key::Num5,
            "6" => Key::Num6,
            "7" => Key::Num7,
            "8" => Key::Num8,
            "9" => Key::Num9,
            "Up" | "ArrowUp" => Key::Up,
            "Down" | "ArrowDown" => Key::Down,
            "Left" | "ArrowLeft" => Key::Left,
            "Right" | "ArrowRight" => Key::Right,
            "Ins" | "Insert" => Key::Insert,
            "Del" | "Delete" => Key::Delete,
            "Home" => Key::Home,
            "End" => Key::End,
            "PgUp" | "PageUp" => Key::PageUp,
            "PgDown" | "PageDown" => Key::PageDown,
            "Return" | "Enter" => Key::Return,
            "Esc" | "Escape" => Key::Escape,
            "Backspace" => Key::Backspace,
            "Tab" => Key::Tab,
            "Space" | " " => Key::Space,
            "Shift" => Key::Shift,
            "Ctrl" | "Control" => Key::Control,
            "Alt" => Key::Alt,
            "Meta" => Key::Meta,
            "F1" => Key::F1,
            "F2" => Key::F2,
            "F3" => Key::F3,
            "F4" => Key::F4,
            "F5" => Key::F5,
            "F6" => Key::F6,
            "F7" => Key::F7,
            "F8" => Key::F8,
            "F9" => Key::F9,
            "F10" => Key::F10,
            "F11" => Key::F11,
            "F12" => Key::F12,
            "[" | "BracketLeft" => Key::BracketLeft,
            "]" | "BracketRight" => Key::BracketRight,
            "\\" | "Backslash" => Key::Backslash,
            "-" | "Minus" => Key::Minus,
            "=" | "Equal" => Key::Equal,
            "," | "Comma" => Key::Comma,
            "." | "Period" => Key::Period,
            "/" | "Slash" => Key::Slash,
            ";" | "Semicolon" => Key::Semicolon,
            "'" | "Quote" => Key::Quote,
            "`" | "GraveAccent" => Key::GraveAccent,
            _ => return None,
        };
        Some(key)
    }
}

/// Mapping-Ziel: entweder ein Chiaki-Button-Bit oder eine Stick-Richtung
/// (EXT-Werte aus ControllerButtonExt).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ButtonOrAxis {
    /// Direktes Chiaki-Button-Bit (BUTTON_*) oder ANALOG_BUTTON_L2/R2.
    Button(u32),
    LeftXUp,
    LeftXDown,
    LeftYUp,
    LeftYDown,
    RightXUp,
    RightXDown,
    RightYUp,
    RightYDown,
}

impl ButtonOrAxis {
    /// Aus dem int-Wert des C++ (GetControllerMapping()-Keys inkl.
    /// ControllerButtonExt).
    pub fn from_ext(code: u32) -> Option<ButtonOrAxis> {
        match code {
            ANALOG_BUTTON_L2 => Some(ButtonOrAxis::Button(ANALOG_BUTTON_L2)),
            ANALOG_BUTTON_R2 => Some(ButtonOrAxis::Button(ANALOG_BUTTON_R2)),
            ext::ANALOG_STICK_LEFT_X_UP => Some(ButtonOrAxis::LeftXUp),
            ext::ANALOG_STICK_LEFT_X_DOWN => Some(ButtonOrAxis::LeftXDown),
            ext::ANALOG_STICK_LEFT_Y_UP => Some(ButtonOrAxis::LeftYUp),
            ext::ANALOG_STICK_LEFT_Y_DOWN => Some(ButtonOrAxis::LeftYDown),
            ext::ANALOG_STICK_RIGHT_X_UP => Some(ButtonOrAxis::RightXUp),
            ext::ANALOG_STICK_RIGHT_X_DOWN => Some(ButtonOrAxis::RightXDown),
            ext::ANALOG_STICK_RIGHT_Y_UP => Some(ButtonOrAxis::RightYUp),
            ext::ANALOG_STICK_RIGHT_Y_DOWN => Some(ButtonOrAxis::RightYDown),
            BUTTON_CROSS
            | BUTTON_MOON
            | BUTTON_BOX
            | BUTTON_PYRAMID
            | BUTTON_DPAD_LEFT
            | BUTTON_DPAD_RIGHT
            | BUTTON_DPAD_UP
            | BUTTON_DPAD_DOWN
            | BUTTON_L1
            | BUTTON_R1
            | BUTTON_L3
            | BUTTON_R3
            | BUTTON_OPTIONS
            | BUTTON_SHARE
            | BUTTON_TOUCHPAD
            | BUTTON_PS => Some(ButtonOrAxis::Button(code)),
            _ => None,
        }
    }

    /// int-Wert wie im C++ (für settings-Kompatibilität).
    pub fn to_ext(self) -> u32 {
        match self {
            ButtonOrAxis::Button(code) => code,
            ButtonOrAxis::LeftXUp => ext::ANALOG_STICK_LEFT_X_UP,
            ButtonOrAxis::LeftXDown => ext::ANALOG_STICK_LEFT_X_DOWN,
            ButtonOrAxis::LeftYUp => ext::ANALOG_STICK_LEFT_Y_UP,
            ButtonOrAxis::LeftYDown => ext::ANALOG_STICK_LEFT_Y_DOWN,
            ButtonOrAxis::RightXUp => ext::ANALOG_STICK_RIGHT_X_UP,
            ButtonOrAxis::RightXDown => ext::ANALOG_STICK_RIGHT_X_DOWN,
            ButtonOrAxis::RightYUp => ext::ANALOG_STICK_RIGHT_Y_UP,
            ButtonOrAxis::RightYDown => ext::ANALOG_STICK_RIGHT_Y_DOWN,
        }
    }
}

/// settings-Name eines Buttons ("keymap/<name>") — 1:1
/// GetChiakiControllerButtonName().replace(' ', '_').toLower().
pub fn button_settings_name(code: u32) -> Option<&'static str> {
    match code {
        BUTTON_CROSS => Some("cross"),
        BUTTON_MOON => Some("moon"),
        BUTTON_BOX => Some("box"),
        BUTTON_PYRAMID => Some("pyramid"),
        BUTTON_DPAD_LEFT => Some("dpad_left"),
        BUTTON_DPAD_RIGHT => Some("dpad_right"),
        BUTTON_DPAD_UP => Some("dpad_up"),
        BUTTON_DPAD_DOWN => Some("dpad_down"),
        BUTTON_L1 => Some("l1"),
        BUTTON_R1 => Some("r1"),
        BUTTON_L3 => Some("l3"),
        BUTTON_R3 => Some("r3"),
        BUTTON_OPTIONS => Some("options"),
        BUTTON_SHARE => Some("share"),
        BUTTON_TOUCHPAD => Some("touchpad"),
        BUTTON_PS => Some("ps"),
        ANALOG_BUTTON_L2 => Some("l2"),
        ANALOG_BUTTON_R2 => Some("r2"),
        ext::ANALOG_STICK_LEFT_X_UP => Some("left_x_up"),
        ext::ANALOG_STICK_LEFT_X_DOWN => Some("left_x_down"),
        ext::ANALOG_STICK_LEFT_Y_UP => Some("left_y_up"),
        ext::ANALOG_STICK_LEFT_Y_DOWN => Some("left_y_down"),
        ext::ANALOG_STICK_RIGHT_X_UP => Some("right_x_up"),
        ext::ANALOG_STICK_RIGHT_X_DOWN => Some("right_x_down"),
        ext::ANALOG_STICK_RIGHT_Y_UP => Some("right_y_up"),
        ext::ANALOG_STICK_RIGHT_Y_DOWN => Some("right_y_down"),
        _ => None,
    }
}

/// Tastatur-Mapper: Key -> Button/Achse.
///
/// Richtung wie Settings::GetControllerMappingForDecoding() (Key -> Button),
/// Defaults 1:1 aus Settings::GetControllerMapping().
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyboardMapper {
    pub key_map: HashMap<Key, ButtonOrAxis>,
}

impl Default for KeyboardMapper {
    fn default() -> Self {
        KeyboardMapper::cpp_defaults()
    }
}

impl KeyboardMapper {
    /// Defaults exakt aus Settings::GetControllerMapping() (settings.cpp):
    /// CROSS->Return, MOON->Backspace, BOX->Backslash, PYRAMID->C, DPad->Pfeile,
    /// L1->2, R1->3, L3->5, R3->6, OPTIONS->O, SHARE->F, TOUCHPAD->T, PS->Esc,
    /// L2->1, R2->4, Sticks -> Brackets/Insert/Delete/=/-/PgUp/PgDown.
    pub fn cpp_defaults() -> Self {
        let entries: &[(u32, Key)] = &[
            (BUTTON_CROSS, Key::Return),
            (BUTTON_MOON, Key::Backspace),
            (BUTTON_BOX, Key::Backslash),
            (BUTTON_PYRAMID, Key::C),
            (BUTTON_DPAD_LEFT, Key::Left),
            (BUTTON_DPAD_RIGHT, Key::Right),
            (BUTTON_DPAD_UP, Key::Up),
            (BUTTON_DPAD_DOWN, Key::Down),
            (BUTTON_L1, Key::Num2),
            (BUTTON_R1, Key::Num3),
            (BUTTON_L3, Key::Num5),
            (BUTTON_R3, Key::Num6),
            (BUTTON_OPTIONS, Key::O),
            (BUTTON_SHARE, Key::F),
            (BUTTON_TOUCHPAD, Key::T),
            (BUTTON_PS, Key::Escape),
            (ANALOG_BUTTON_L2, Key::Num1),
            (ANALOG_BUTTON_R2, Key::Num4),
            (ext::ANALOG_STICK_LEFT_X_UP, Key::BracketRight),
            (ext::ANALOG_STICK_LEFT_X_DOWN, Key::BracketLeft),
            (ext::ANALOG_STICK_LEFT_Y_UP, Key::Insert),
            (ext::ANALOG_STICK_LEFT_Y_DOWN, Key::Delete),
            (ext::ANALOG_STICK_RIGHT_X_UP, Key::Equal),
            (ext::ANALOG_STICK_RIGHT_X_DOWN, Key::Minus),
            (ext::ANALOG_STICK_RIGHT_Y_UP, Key::PageUp),
            (ext::ANALOG_STICK_RIGHT_Y_DOWN, Key::PageDown),
        ];
        KeyboardMapper {
            key_map: entries
                .iter()
                .filter_map(|(code, key)| {
                    ButtonOrAxis::from_ext(*code).map(|target| (*key, target))
                })
                .collect(),
        }
    }

    /// Port von SetControllerButtonMapping(): ein Mapping ändern.
    pub fn set_mapping(&mut self, key: Key, target: ButtonOrAxis) {
        // Ein Button kann nur auf einer Taste liegen (QMap-Semantik im C++:
        // GetControllerMappingForDecoding invertiert, letzter Eintrag gewinnt).
        self.key_map.retain(|_, t| *t != target);
        self.key_map.insert(key, target);
    }

    /// Port von ClearKeyMapping().
    pub fn clear(&mut self) {
        self.key_map.clear();
    }

    /// Port von StreamSession::HandleKeyboardEvent(): erzeugt den State aus
    /// der Menge der aktuell gedrückten Tasten.
    ///
    /// Werte wie im C++: L2/R2 -> 0xff, Stick-Richtungen -> ±0x7fff
    /// ("up/left" = negative Achse).
    pub fn apply_keyboard_state(&self, pressed_keys: &HashSet<Key>) -> ControllerState {
        let mut state = ControllerState::default();
        for key in pressed_keys {
            let Some(target) = self.key_map.get(key) else {
                continue;
            };
            match target {
                ButtonOrAxis::Button(code) => match *code {
                    ANALOG_BUTTON_L2 => state.l2_state = 0xff,
                    ANALOG_BUTTON_R2 => state.r2_state = 0xff,
                    _ => state.buttons |= code,
                },
                ButtonOrAxis::LeftXUp => state.left_x = 0x7fff,
                ButtonOrAxis::LeftXDown => state.left_x = -0x7fff,
                ButtonOrAxis::LeftYUp => state.left_y = -0x7fff,
                ButtonOrAxis::LeftYDown => state.left_y = 0x7fff,
                ButtonOrAxis::RightXUp => state.right_x = 0x7fff,
                ButtonOrAxis::RightXDown => state.right_x = -0x7fff,
                ButtonOrAxis::RightYUp => state.right_y = -0x7fff,
                ButtonOrAxis::RightYDown => state.right_y = 0x7fff,
            }
        }
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chiaki_core::controller::ANALOG_BUTTON_L2 as L2;

    fn keys(keys: &[Key]) -> HashSet<Key> {
        keys.iter().copied().collect()
    }

    #[test]
    fn defaults_match_settings_cpp() {
        let m = KeyboardMapper::default();
        let expected: &[(Key, u32)] = &[
            (Key::Return, BUTTON_CROSS),
            (Key::Backspace, BUTTON_MOON),
            (Key::Backslash, BUTTON_BOX),
            (Key::C, BUTTON_PYRAMID),
            (Key::Left, BUTTON_DPAD_LEFT),
            (Key::Right, BUTTON_DPAD_RIGHT),
            (Key::Up, BUTTON_DPAD_UP),
            (Key::Down, BUTTON_DPAD_DOWN),
            (Key::Num2, BUTTON_L1),
            (Key::Num3, BUTTON_R1),
            (Key::Num5, BUTTON_L3),
            (Key::Num6, BUTTON_R3),
            (Key::O, BUTTON_OPTIONS),
            (Key::F, BUTTON_SHARE),
            (Key::T, BUTTON_TOUCHPAD),
            (Key::Escape, BUTTON_PS),
            (Key::Num1, ANALOG_BUTTON_L2),
            (Key::Num4, ANALOG_BUTTON_R2),
            (Key::BracketRight, ext::ANALOG_STICK_LEFT_X_UP),
            (Key::BracketLeft, ext::ANALOG_STICK_LEFT_X_DOWN),
            (Key::Insert, ext::ANALOG_STICK_LEFT_Y_UP),
            (Key::Delete, ext::ANALOG_STICK_LEFT_Y_DOWN),
            (Key::Equal, ext::ANALOG_STICK_RIGHT_X_UP),
            (Key::Minus, ext::ANALOG_STICK_RIGHT_X_DOWN),
            (Key::PageUp, ext::ANALOG_STICK_RIGHT_Y_UP),
            (Key::PageDown, ext::ANALOG_STICK_RIGHT_Y_DOWN),
        ];
        assert_eq!(m.key_map.len(), expected.len());
        for (key, code) in expected {
            let target = m.key_map.get(key).unwrap_or_else(|| panic!("key {key:?} fehlt"));
            assert_eq!(target.to_ext(), *code, "Mapping für {key:?} falsch");
        }
    }

    #[test]
    fn handle_keyboard_event_port() {
        let m = KeyboardMapper::default();

        // Buttons
        let state = m.apply_keyboard_state(&keys(&[Key::Return, Key::Escape]));
        assert!(state.buttons & BUTTON_CROSS != 0);
        assert!(state.buttons & BUTTON_PS != 0);

        // Analog-Trigger voll drücken = 0xff (wie im C++)
        let state = m.apply_keyboard_state(&keys(&[Key::Num1, Key::Num4]));
        assert_eq!(state.l2_state, 0xff);
        assert_eq!(state.r2_state, 0xff);
        assert_eq!(state.buttons & L2, 0, "L2 darf kein Button-Bit setzen");

        // Sticks: up = -0x7fff (wie HandleKeyboardEvent)
        let state = m.apply_keyboard_state(&keys(&[
            Key::Insert,
            Key::BracketRight,
            Key::Minus,
            Key::PageDown,
        ]));
        assert_eq!(state.left_y, -0x7fff, "LEFT_Y_UP -> -0x7fff");
        assert_eq!(state.left_x, 0x7fff, "LEFT_X_UP -> 0x7fff");
        assert_eq!(state.right_x, -0x7fff, "RIGHT_X_DOWN -> -0x7fff");
        assert_eq!(state.right_y, 0x7fff, "RIGHT_Y_DOWN -> 0x7fff");

        // Loslassen -> idle
        let state = m.apply_keyboard_state(&keys(&[]));
        assert_eq!(state.buttons, 0);
        assert_eq!(state.left_y, 0);
        assert_eq!(state.l2_state, 0);
    }

    #[test]
    fn qt_names_roundtrip_and_cpp_compatibility() {
        // Namen wie QKeySequence sie erzeugt (settings.cpp speichert sie)
        assert_eq!(Key::Return.to_qt_name(), "Return");
        assert_eq!(Key::Backslash.to_qt_name(), "\\");
        assert_eq!(Key::Insert.to_qt_name(), "Ins");
        assert_eq!(Key::PageDown.to_qt_name(), "PgDown");
        assert_eq!(Key::Escape.to_qt_name(), "Esc");

        for key in [
            Key::Return,
            Key::Backspace,
            Key::Backslash,
            Key::C,
            Key::Left,
            Key::Num1,
            Key::O,
            Key::F,
            Key::T,
            Key::Escape,
            Key::BracketRight,
            Key::BracketLeft,
            Key::Insert,
            Key::Delete,
            Key::Equal,
            Key::Minus,
            Key::PageUp,
            Key::PageDown,
        ] {
            assert_eq!(Key::from_qt_name(&key.to_qt_name()), Some(key));
        }
    }

    #[test]
    fn settings_names_for_keymap_keys() {
        // "keymap/" + button_settings_name — wie SetControllerButtonMapping
        assert_eq!(button_settings_name(BUTTON_CROSS), Some("cross"));
        assert_eq!(button_settings_name(BUTTON_PS), Some("ps"));
        assert_eq!(button_settings_name(ANALOG_BUTTON_L2), Some("l2"));
        assert_eq!(button_settings_name(ext::ANALOG_STICK_LEFT_X_UP), Some("left_x_up"));
    }

    #[test]
    fn set_mapping_replaces_single_target() {
        let mut m = KeyboardMapper::default();
        m.set_mapping(Key::W, ButtonOrAxis::LeftYUp);
        assert_eq!(m.key_map.get(&Key::W), Some(&ButtonOrAxis::LeftYUp));
        // Insert hatte LEFT_Y_UP — muss umgezogen sein (QMap-Semantik)
        assert!(!m.key_map.contains_key(&Key::Insert));
    }

    #[test]
    fn serde_config_roundtrip() {
        // Mapping-Config via serde_json (für chiaki-settings/UI)
        let m = KeyboardMapper::default();
        let json = serde_json::to_string(&m).expect("serialize");
        let back: KeyboardMapper = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(m, back);
    }
}
