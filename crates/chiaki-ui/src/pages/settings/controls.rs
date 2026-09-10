//! Settings-Kategorie „Controls“ (QML: `SettingsControls.qml`): Tastatur-
//! Mapping mit Key-Capture („Aufzeichnen“ → nächster Tastendruck wird
//! übernommen, gespeichert als Qt-KeySequence-Name via
//! `chiaki_input::keyboard::Key::to_qt_name()`), Dpad-Touchpad, Haptics.

use chiaki_input::keyboard::Key;

use crate::app::AppShell;
use crate::theme;

use super::{
    action_row, combo_select, custom_row, inactive, label_col, opts, select_row, slider_row,
    toggle_row, Section, SRow,
};

pub(crate) fn sections(
    shell: &mut AppShell,
    _needle: &str,
    _cx: &mut gpui::Context<AppShell>,
) -> Vec<Section> {
    let settings = shell.backend.settings().clone();
    let s = settings.lock().unwrap_or_else(|e| e.into_inner());

    let keyboard = s.keyboard_enabled();
    let mouse_touch = s.mouse_touch_enabled();
    let keymap = s.controller_mapping();
    let background_events = s.allow_joystick_background_events();
    let buttons_by_pos = s.buttons_by_position();
    let dpad_touch = s.dpad_touch_enabled();
    let dpad_increment = s.dpad_touch_increment();
    let dpad_combos = [
        s.dpad_touch_shortcut1(),
        s.dpad_touch_shortcut2(),
        s.dpad_touch_shortcut3(),
        s.dpad_touch_shortcut4(),
    ];
    let rumble = rumble_value(s.rumble_haptics_intensity());
    let haptic = s.haptic_override();
    let deadzone = s.stick_deadzone();
    drop(s);

    let mut keyboard_section = Section::new("Keyboard");
    keyboard_section.push(toggle_row(
        "controls-keyboard-enabled",
        "Keyboard as controller",
        None,
        "keyboard keys input",
        true,
        keyboard,
    ));
    keyboard_section.push(inactive(toggle_row(
        "controls-mouse-touch",
        "Enable mouse touchpad",
        Some("The mouse acts as the console touchpad"),
        "mouse touchpad pointer",
        true,
        mouse_touch,
    ), "Mouse-as-touchpad not ported in the Rust client"));
    keyboard_section.push(action_row(
        "controls-reset-keys",
        "Reset all keys",
        Some("Restore the default keyboard mapping"),
        "keyboard reset default",
        true,
        "Reset",
        true,
        |shell, cx| {
            let settings = shell.backend.settings().clone();
            let result = settings
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .update(|s| s.clear_key_mapping());
            let kind = if result.is_ok() {
                crate::components::ToastKind::Success
            } else {
                crate::components::ToastKind::Danger
            };
            shell.push_toast(crate::components::ToastData::new(kind, "Keymap reset"), cx);
        },
    ));

    // Key Mapping: eine Row pro Button (Label + aktuell gedrückte Taste).
    // Klick öffnet das Capture-Overlay (nächster Tastendruck wird übernommen).
    let mut keymap_section = Section::new("Key Mapping");
    for (button, key) in &keymap {
        keymap_section.push(key_capture_row(*button, key));
    }

    let mut controller = Section::new("Controller");
    controller.push(inactive(action_row(
        "controls-change-mapping",
        "Change controller mapping",
        Some("Press buttons on the controller to capture its layout"),
        "controller mapping sdl",
        true,
        "Start",
        false,
        |_shell, cx| {
            // TODO(konsolen-agent): `beginControllerMapping`-Flow gegen
            // `backend.controllers()` verdrahten (Capture-Dialog folgt mit
            // dem Controller-Agenten).
            super::push_toast(
                cx,
                crate::components::ToastKind::Info,
                "Controller mapping",
                "Will be wired up with the controller agent (backend flow still missing).",
            );
        },
    ), "Capture flow follows with the controller agent"));
    controller.push(inactive(action_row(
        "controls-reset-mapping",
        "Reset controller mapping",
        Some("Restore the default SDL mapping for the next selected controller"),
        "controller mapping reset default",
        true,
        "Reset",
        false,
        |_shell, cx| {
            // TODO(konsolen-agent): wie oben — SDL-Mapping-Reset im Backend.
            super::push_toast(
                cx,
                crate::components::ToastKind::Info,
                "Controller mapping",
                "Will be wired up with the controller agent (backend flow still missing).",
            );
        },
    ), "Capture flow follows with the controller agent"));
    controller.push(inactive(toggle_row(
        "controls-background-events",
        "Background controller events",
        Some("Process controller input while the window is in background"),
        "background controller input",
        true,
        background_events,
    ), "SDL hint is not set in the Rust input backend"));
    controller.push(inactive(toggle_row(
        "controls-buttons-by-pos",
        "Buttons by position",
        Some("Use buttons by physical position instead of by label (Nintendo-style)"),
        "nintendo layout abxy",
        true,
        buttons_by_pos,
    ), "Position-based button mapping not implemented in the input backend"));
    // Stick-Deadzone: wird live im Stream-Input-Loop auf den kombinierten
    // Controller-State angewendet (Rescale oberhalb der Zone; 0 = aus).
    controller.push(slider_row(
        "controls-stick-deadzone",
        "Stick deadzone",
        Some(
            "Analog stick deadzone in percent — inputs below are ignored, \
             above they are rescaled (0 = off)",
        ),
        "deadzone stick axis analog drift",
        true,
        deadzone as f64,
        0.0,
        50.0,
        1.0,
        format!("{deadzone} % (0 = off)"),
        |v, s| s.set_stick_deadzone(v.round() as i64),
    ));

    let mut dpad = Section::new("Dpad Touchpad Emulation");
    dpad.push(inactive(toggle_row(
        "controls-dpad-touch-enabled",
        "Dpad touchpad emulation",
        Some("The dpad moves the touchpad cursor while the combo is held"),
        "dpad touch cursor",
        true,
        dpad_touch,
    ), "Dpad touchpad emulation not ported"));
    dpad.push(inactive(slider_row(
        "controls-dpad-touch-increment",
        "Dpad touch increment",
        None,
        "dpad speed mm",
        dpad_touch,
        dpad_increment as f64,
        1.0,
        1079.0,
        1.0,
        format!("{:.2} mm (default 0.30 mm)", dpad_increment as f64 / 100.0),
        |v, s| s.set_dpad_touch_increment(v.round().clamp(1.0, u16::MAX as f64) as u16),
    ), "Dpad touchpad emulation not ported"));
    for (index, value) in dpad_combos.iter().enumerate() {
        let id: &'static str = match index {
            0 => "controls-dpad-touch-combo-1",
            1 => "controls-dpad-touch-combo-2",
            2 => "controls-dpad-touch-combo-3",
            _ => "controls-dpad-touch-combo-4",
        };
        dpad.push(inactive(combo_select(
            id,
            &format!("Dpad touch combo {}", index + 1),
            "dpad combo controller button",
            dpad_touch,
            *value,
            move |v, s| match index {
                0 => s.set_dpad_touch_shortcut1(v),
                1 => s.set_dpad_touch_shortcut2(v),
                2 => s.set_dpad_touch_shortcut3(v),
                _ => s.set_dpad_touch_shortcut4(v),
            },
        ), "Dpad touchpad emulation not ported"));
    }

    let mut haptics = Section::new("Haptics");
    haptics.push(select_row(
        "controls-rumble-intensity",
        "Rumble haptics intensity",
        None,
        "rumble vibration force feedback",
        true,
        opts(&[
            ("Off", "Off"),
            ("Very weak", "Very Weak"),
            ("Weak", "Weak"),
            ("Normal", "Normal"),
            ("Strong", "Strong"),
            ("Very Strong", "Very Strong"),
        ]),
        rumble,
        |v, s| {
            s.set_rumble_haptics_intensity(match v {
                "Off" => chiaki_settings::settings::RumbleHapticsIntensity::Off,
                "Very weak" => chiaki_settings::settings::RumbleHapticsIntensity::VeryWeak,
                "Weak" => chiaki_settings::settings::RumbleHapticsIntensity::Weak,
                "Strong" => chiaki_settings::settings::RumbleHapticsIntensity::Strong,
                "Very Strong" => chiaki_settings::settings::RumbleHapticsIntensity::VeryStrong,
                _ => chiaki_settings::settings::RumbleHapticsIntensity::Normal,
            });
        },
    ));
    haptics.push(slider_row(
        "controls-haptic-override",
        "True haptics intensity",
        Some("PS5 DualSense adaptive haptics scale"),
        "dualsense haptics adaptive",
        true,
        haptic,
        0.0,
        2.0,
        0.1,
        format!(
            "{}",
            if (haptic - 1.0).abs() < 0.011 {
                "console setting".to_string()
            } else {
                format!("{} % console setting", (haptic * 100.0).round() as i64)
            }
        ),
        |v, s| s.set_haptic_override((v * 10.0).round() / 10.0),
    ));

    vec![keyboard_section, keymap_section, controller, dpad, haptics]
}

/// Eine Keymap-Row: Button-Name links, Taste rechts als „Aufzeichnen“-Button.
fn key_capture_row(button: u32, key: &str) -> SRow {
    use gpui::{div, px, IntoElement as _, ParentElement as _, Styled as _};
    use crate::components::Button;

    let button_name = chiaki_settings::settings::controller_button_name(button);
    let key_display = Key::from_qt_name(key)
        .map(|k| k.to_qt_name())
        .unwrap_or_else(|| key.to_string());

    let control = Button::new(
        gpui::ElementId::Name(format!("key-capture-{button:08x}").into()),
        format!("{key_display} \u{2014} record"),
    )
    .on_click(move |_: &gpui::ClickEvent, window, cx| {
        super::start_key_capture(cx, window, button);
    })
    .into_any_element();

    let label = label_col(button_name, Some(&format!("Key: {key_display}")));
    custom_row(
        &format!("{button_name} key {key_display}"),
        None,
        "key mapping capture",
        true,
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(theme::SP_4))
            .child(label)
            .child(control)
            .into_any_element(),
    )
}

/// gpui-Keystroke-Key → neutrales [`Key`]-Enum (für die Keymap).
pub(crate) fn key_from_gpui(key: &str) -> Option<Key> {
    use Key::*;
    let k = match key {
        "enter" => Return,
        "escape" => Escape,
        "backspace" => Backspace,
        "tab" => Tab,
        "space" => Space,
        "up" => Up,
        "down" => Down,
        "left" => Left,
        "right" => Right,
        "insert" => Insert,
        "delete" => Delete,
        "home" => Home,
        "end" => End,
        "pageup" => PageUp,
        "pagedown" => PageDown,
        "shift" => Shift,
        "control" => Control,
        "alt" => Alt,
        "platform" => Meta,
        "f1" => F1,
        "f2" => F2,
        "f3" => F3,
        "f4" => F4,
        "f5" => F5,
        "f6" => F6,
        "f7" => F7,
        "f8" => F8,
        "f9" => F9,
        "f10" => F10,
        "f11" => F11,
        "f12" => F12,
        "[" => BracketLeft,
        "]" => BracketRight,
        "\\" => Backslash,
        "-" => Minus,
        "=" => Equal,
        "," => Comma,
        "." => Period,
        "/" => Slash,
        ";" => Semicolon,
        "'" => Quote,
        "`" => GraveAccent,
        single if single.chars().count() == 1 => {
            let c = single.chars().next().unwrap_or('\0');
            if ('a'..='z').contains(&c) {
                letter(c)
            } else if ('0'..='9').contains(&c) {
                digit(c)
            } else {
                return None;
            }
        }
        _ => return None,
    };
    Some(k)
}

fn letter(c: char) -> Key {
    use Key::*;
    match c {
        'a' => A,
        'b' => B,
        'c' => C,
        'd' => D,
        'e' => E,
        'f' => F,
        'g' => G,
        'h' => H,
        'i' => I,
        'j' => J,
        'k' => K,
        'l' => L,
        'm' => M,
        'n' => N,
        'o' => O,
        'p' => P,
        'q' => Q,
        'r' => R,
        's' => S,
        't' => T,
        'u' => U,
        'v' => V,
        'w' => W,
        'x' => X,
        'y' => Y,
        _ => Z,
    }
}

fn digit(c: char) -> Key {
    use Key::*;
    match c {
        '0' => Num0,
        '1' => Num1,
        '2' => Num2,
        '3' => Num3,
        '4' => Num4,
        '5' => Num5,
        '6' => Num6,
        '7' => Num7,
        '8' => Num8,
        _ => Num9,
    }
}

fn rumble_value(i: chiaki_settings::settings::RumbleHapticsIntensity) -> &'static str {
    use chiaki_settings::settings::RumbleHapticsIntensity as I;
    match i {
        I::Off => "Off",
        I::VeryWeak => "Very weak",
        I::Weak => "Weak",
        I::Normal => "Normal",
        I::Strong => "Strong",
        I::VeryStrong => "Very Strong",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// gpui-Key-Namen müssen auf das neutrale Key-Enum mappen; die
    /// Speicherung läuft über die Qt-Namen der C++-Settings.
    #[test]
    fn gpui_keys_map_to_qt_names() {
        assert_eq!(key_from_gpui("enter").unwrap().to_qt_name(), "Return");
        assert_eq!(key_from_gpui("a").unwrap().to_qt_name(), "A");
        assert_eq!(key_from_gpui("5").unwrap().to_qt_name(), "5");
        assert_eq!(key_from_gpui("[").unwrap().to_qt_name(), "[");
        assert_eq!(key_from_gpui("pageup").unwrap().to_qt_name(), "PgUp");
        assert_eq!(key_from_gpui("escape").unwrap().to_qt_name(), "Esc");
        // C++-Default-Belegungen rücklesen (from_qt_name → to_qt_name).
        for stored in ["Return", "Backspace", "Backslash", "C", "Left", "Right", "Up", "Down", "2", "3", "5", "6", "O", "F", "T", "Escape", "1", "4", "]", "[", "Insert", "Delete", "=", "-", "PgUp", "PgDown"] {
            assert!(
                Key::from_qt_name(stored).is_some(),
                "C++-Default-Key {stored} muss parsebar sein"
            );
        }
        assert_eq!(key_from_gpui("f13"), None);
        assert_eq!(key_from_gpui("menu"), None);
    }
}
