//! Settings-Kategorie „General“ (QML: `SettingsGeneral.qml`): Verhalten beim
//! Trennen/Anhalten, Streamer-Mode, Auto-Connect/Discovery, Stream-Menü +
//! Controller-Kombos, Diagnostics (Log-Ordner).

use crate::app::AppShell;

use super::{action_row, combo_select, inactive, opts, select_row, toggle_row, Section};

pub(crate) fn sections(
    shell: &mut AppShell,
    _needle: &str,
    _cx: &mut gpui::Context<AppShell>,
) -> Vec<Section> {
    let s = shell.backend.settings().lock().unwrap_or_else(|e| e.into_inner());

    // Enum → INI-String (Namen wie in chiaki-settings).
    let disconnect = match s.disconnect_action() {
        chiaki_settings::settings::DisconnectAction::AlwaysNothing => "nothing",
        chiaki_settings::settings::DisconnectAction::AlwaysSleep => "sleep",
        chiaki_settings::settings::DisconnectAction::Ask => "ask",
    };
    let suspend = match s.suspend_action() {
        chiaki_settings::settings::SuspendAction::Nothing => "nothing",
        chiaki_settings::settings::SuspendAction::Sleep => "sleep",
    };
    let audio_video = s.audio_video_disabled_raw().to_string();
    let streamer_mode = s.streamer_mode();
    let automatic_connect = s.automatic_connect();
    let auto_discovery = s.discovery_enabled();
    let remote_play_ask = s.remote_play_ask();
    let add_steam_ask = s.add_steam_shortcut_ask();
    let stream_menu = s.stream_menu_enabled();
    let menu_combos = [
        s.stream_menu_shortcut1(),
        s.stream_menu_shortcut2(),
        s.stream_menu_shortcut3(),
        s.stream_menu_shortcut4(),
    ];
    let log_dir = chiaki_settings::app_paths::log_dir();
    drop(s);

    let mut behaviour = Section::new("Behaviour");
    behaviour.push(inactive(select_row(
        "general-disconnect-action",
        "Action on disconnect",
        Some("What to do with the console when the stream is disconnected"),
        "quit sleep ask close",
        true,
        opts(&[("nothing", "Do Nothing"), ("sleep", "Enter Sleep Mode"), ("ask", "Ask")]),
        disconnect,
        |v, s| {
            s.set_disconnect_action(match v {
                "sleep" => chiaki_settings::settings::DisconnectAction::AlwaysSleep,
                "nothing" => chiaki_settings::settings::DisconnectAction::AlwaysNothing,
                _ => chiaki_settings::settings::DisconnectAction::Ask,
            });
        },
    ), "Trenn-Aktion (Sleep/Ask) beim Stream-Ende nicht implementiert"));
    behaviour.push(inactive(select_row(
        "general-suspend-action",
        "Action on suspend",
        Some("Console behaviour when the PC suspends during a stream"),
        "sleep pc suspend",
        true,
        opts(&[("nothing", "Do Nothing"), ("sleep", "Enter Sleep Mode")]),
        suspend,
        |v, s| {
            s.set_suspend_action(match v {
                "sleep" => chiaki_settings::settings::SuspendAction::Sleep,
                _ => chiaki_settings::settings::SuspendAction::Nothing,
            });
        },
    ), "Suspend-Hook für die Stream-Session nicht implementiert"));
    behaviour.push(select_row(
        "general-audio-video",
        "Audio / Video",
        Some("Disable parts of the stream (background audio, etc.)"),
        "mute sound picture",
        true,
        opts(&[
            ("0", "Audio and Video Enabled"),
            ("1", "Audio Disabled"),
            ("2", "Video Disabled"),
            ("3", "Audio and Video Disabled"),
        ]),
        &audio_video,
        |v, s| {
            s.set_audio_video_disabled_raw(v.parse().unwrap_or(0));
        },
    ));
    behaviour.push(toggle_row(
        "general-streamer-mode",
        "Streamer mode",
        Some("Hides sensitive info (MAC addresses, console names, PINs)"),
        "privacy hide twitch",
        true,
        streamer_mode,
    ));
    behaviour.push(toggle_row(
        "general-automatic-connect",
        "Automatically connect on discovery",
        Some(
            "Master switch \u{2014} connects automatically to the console marked \
             \u{201C}Auto-Connect\u{201D} (see Consoles category)",
        ),
        "auto connect discovery",
        true,
        automatic_connect,
    ));
    behaviour.push(inactive(toggle_row(
        "general-auto-discovery",
        "Auto discovery",
        Some("Discover consoles on the local network while the app is running"),
        "discovery broadcast network",
        true,
        auto_discovery,
    ), "Discovery-Service läuft immer — der Filter greift nicht"));
    behaviour.push(inactive(toggle_row(
        "general-remote-play-ask",
        "Ask before starting Remote Play",
        Some("Ask for confirmation before a PSN remote play session is started"),
        "remote play confirm psn",
        true,
        remote_play_ask,
    ), "Remote-Play-Start fragt nicht nach"));
    behaviour.push(inactive(toggle_row(
        "general-add-steam-shortcut-ask",
        "Offer adding a Steam shortcut",
        Some("Offer to add the app to the Steam library after setup"),
        "steam shortcut library",
        true,
        add_steam_ask,
    ), "Steam-Shortcut-Flow ist noch nicht verdrahtet"));

    let mut menu = Section::new("Stream Menu");
    menu.push(inactive(toggle_row(
        "general-stream-menu-enabled",
        "Stream menu shortcut enabled",
        Some("Open the in-stream menu with a controller button combo"),
        "overlay menu combo",
        true,
        stream_menu,
    ), "In-Stream-Menü nicht portiert — das HUD läuft über Tastatur/HUD-Button"));
    for (combo, value) in menu_combos.iter().enumerate() {
        let id: &'static str = match combo {
            0 => "general-stream-menu-combo-1",
            1 => "general-stream-menu-combo-2",
            2 => "general-stream-menu-combo-3",
            _ => "general-stream-menu-combo-4",
        };
        menu.push(inactive(combo_select(
            id,
            &format!("Stream menu combo {}", combo + 1),
            "controller button",
            stream_menu,
            *value,
            move |index, s| match combo {
                0 => s.set_stream_menu_shortcut1(index),
                1 => s.set_stream_menu_shortcut2(index),
                2 => s.set_stream_menu_shortcut3(index),
                _ => s.set_stream_menu_shortcut4(index),
            },
        ), "In-Stream-Menü nicht portiert"));
    }

    let mut diagnostics = Section::new("Diagnostics");
    diagnostics.push(action_row(
        "general-open-log-dir",
        "Log directory",
        Some(&log_dir.display().to_string()),
        "logs folder files",
        true,
        "Open",
        false,
        move |_shell, _cx| {
            let _ = std::process::Command::new("explorer").arg(&log_dir).spawn();
        },
    ));

    vec![behaviour, menu, diagnostics]
}
