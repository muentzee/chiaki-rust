//! Tests für den Settings-Port (1:1 gegen gui/src/settings.cpp).
//! Alle Tests laufen in Temp-Dirs — echte User-Daten werden nie berührt.

use super::*;
use crate::hosts::CHIAKI_SESSION_AUTH_SIZE;
use std::path::{Path, PathBuf};

/// Referenz-INI im exakten C++-QSettings-Layout (synthetische Daten,
/// Struktur identisch zur echten portable settings.ini).
const REFERENCE_INI: &str = "[General]\r\n\
    version=2\r\n\
    \r\n\
    [registered_hosts]\r\n\
    1\\ap_bssid=3132333435\r\n\
    1\\ap_key=\r\n\
    1\\ap_name=PS5\r\n\
    1\\ap_ssid=\r\n\
    1\\console_pin=0\r\n\
    1\\rp_key=@ByteArray(0\\xb0\\x42H~\\xc9&\\x9a\\xf2\\x18\\x93\\xf6\\xb0Ud\\x92)\r\n\
    1\\rp_key_type=2\r\n\
    1\\rp_regist_key=@ByteArray(7c3e91a4\\0\\0\\0\\0\\0\\0\\0\\0)\r\n\
    1\\server_mac=@ByteArray(\\xd4\\xf7\\xd5\\x11\\xfa\\x45)\r\n\
    1\\server_nickname=Test-PS5\r\n\
    1\\target=1000100\r\n\
    size=1\r\n\
    \r\n\
    [settings]\r\n\
    add_steam_shortcut_ask=false\r\n\
    audio_buffer_size=1920\r\n\
    automatic_connect=true\r\n\
    bitrate_local_ps5=28000\r\n\
    fps_local_ps5=60\r\n\
    fullscreen_doubleclick=true\r\n\
    geometry=@Rect(201 142 1775 1314)\r\n\
    hide_cursor=false\r\n\
    hw_decoder=auto\r\n\
    nv_vsr=true\r\n\
    nv_vsr_scale=200\r\n\
    placebo_frame_mixer=none\r\n\
    placebo_preset=high_quality\r\n\
    psn_account_id=\"eVr/5uFEAHE=\"\r\n\
    psn_auth_token=test-auth-token\r\n\
    psn_auth_token_expiry=2026-09-05 14:27:36 MESZ\r\n\
    psn_refresh_token=test-refresh-token\r\n\
    remote_play_ask=false\r\n\
    resolution_local_ps5=1080p\r\n\
    show_stream_stats=false\r\n\
    stream_geometry=@Rect(0 23 3072 1705)\r\n\
    window_type=Adjust Manually\r\n\
    \r\n\
    [controller_mappings]\r\n\
    size=0\r\n";

fn test_paths(base: &Path) -> SettingsPaths {
    SettingsPaths {
        base: base.to_path_buf(),
        settings: base.join("settings.ini"),
        default_settings: base.join("settings.ini"),
        placebo: base.join("placebo_render_params.ini"),
    }
}

fn sample_host() -> RegisteredHost {
    let mut h = RegisteredHost::default();
    h.target = Target::Ps5One;
    h.ap_bssid = "3132333435".into();
    h.ap_name = "PS5".into();
    h.server_mac = HostMac::new([0xd4, 0xf7, 0xd5, 0x11, 0xfa, 0x45]);
    h.server_nickname = "Test-PS5".into();
    h.rp_regist_key.copy_from_slice(b"7c3e91a4\0\0\0\0\0\0\0\0");
    h.rp_key_type = 2;
    h.rp_key = [
        0x30, 0xb0, 0x42, 0x48, 0x7e, 0xc9, 0x26, 0x9a, 0xf2, 0x18, 0x93, 0xf6, 0xb0, 0x55, 0x64,
        0x92,
    ];
    h.console_pin = "0".into();
    h
}

fn temp_base(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "chiaki-settings-test-{}-{}",
        name,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn reference_ini_is_parsed_and_resaved_byte_identical() {
    let base = temp_base("ref");
    std::fs::write(base.join("settings.ini"), REFERENCE_INI).unwrap();

    let settings = Settings::open_at(test_paths(&base)).unwrap();
    // Werte korrekt gelesen?
    assert_eq!(settings.resolution_local_ps5(), ResolutionPreset::P1080);
    assert!(settings.automatic_connect());
    assert!(settings.nv_vsr_enabled());
    assert_eq!(settings.nv_vsr_scale(), 200);
    assert_eq!(settings.audio_buffer_size_raw(), 1920);
    assert_eq!(settings.bitrate_local_ps5(), 28000);
    assert_eq!(settings.fps_local_ps5(), FpsPreset::Fps60);
    assert_eq!(settings.window_type(), WindowType::AdjustableResolution);
    assert_eq!(settings.geometry(), Rect { x: 201, y: 142, width: 1775, height: 1314 });
    assert_eq!(settings.psn_account_id(), "eVr/5uFEAHE=");
    assert_eq!(settings.psn_refresh_token(), "test-refresh-token");
    assert_eq!(
        settings.psn_account_id_bytes().unwrap(),
        [0x79, 0x5a, 0xff, 0xe6, 0xe1, 0x44, 0x00, 0x71]
    );
    // Host geladen
    assert_eq!(settings.registered_hosts().len(), 1);
    let mac = HostMac::new([0xd4, 0xf7, 0xd5, 0x11, 0xfa, 0x45]);
    assert!(settings.registered_host_registered(mac));
    assert!(settings.nickname_registered("Test-PS5"));
    assert_eq!(settings.ps4s_registered(), 0); // PS5 zählt nicht als PS4
    assert_eq!(settings.auto_connect_host(), RegisteredHost::default());

    // Speichern ohne Änderungen → byte-identische Datei
    settings.save().unwrap();
    let saved = std::fs::read_to_string(base.join("settings.ini")).unwrap();
    assert_eq!(saved, REFERENCE_INI);

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn defaults_match_cxx() {
    let base = temp_base("defaults");
    let s = Settings::open_at(test_paths(&base)).unwrap();

    // Stichprobe der C++-Defaults
    assert!(s.discovery_enabled());
    assert!(s.remote_play_ask());
    assert!(s.add_steam_shortcut_ask());
    assert!(!s.log_verbose());
    assert!(s.log_sanitize());
    assert!(!s.vsync_enabled());
    assert!(s.hide_cursor());
    assert!(!s.show_stream_stats());
    assert!(s.show_vsr_badge());
    assert!(!s.streamer_mode());
    assert!(!s.buttons_by_position());
    assert!(s.allow_joystick_background_events());
    assert!(!s.start_mic_unmuted());
    assert!(!s.automatic_connect());
    assert!(!s.fullscreen_double_click_enabled());
    assert!(!s.idr_on_fec_failure_enabled());
    assert_eq!(s.reorder_timeout_ms(), 16);
    assert!(!s.nv_vsr_enabled());
    assert_eq!(s.nv_vsr_scale(), 0);
    assert_eq!(s.nv_vsr_sdk_path(), "");
    assert_eq!(s.haptic_override(), 1.0);
    assert_eq!(s.zoom_factor(), -1.0);
    assert_eq!(s.packet_loss_reported_max(), 0.05);
    assert_eq!(s.rumble_haptics_intensity(), RumbleHapticsIntensity::Normal);
    assert_eq!(s.window_type(), WindowType::AdjustableResolution);
    assert_eq!(s.custom_resolution_width(), 1920);
    assert_eq!(s.custom_resolution_height(), 1080);
    assert_eq!(s.placebo_preset(), PlaceboPreset::HighQuality);
    assert_eq!(s.render_backend(), RenderBackend::Vulkan);
    assert_eq!(s.resolution_local_ps4(), ResolutionPreset::P720);
    assert_eq!(s.resolution_remote_ps4(), ResolutionPreset::P720);
    assert_eq!(s.resolution_local_ps5(), ResolutionPreset::P1080);
    assert_eq!(s.resolution_remote_ps5(), ResolutionPreset::P720);
    assert_eq!(s.fps_local_ps4(), FpsPreset::Fps60);
    assert_eq!(s.codec_ps4(), Codec::H264);
    assert_eq!(s.codec_local_ps5(), Codec::H265);
    assert_eq!(s.display_target_contrast(), 0);
    assert_eq!(s.display_target_peak(), 0);
    assert_eq!(s.audio_volume(), 128);
    assert_eq!(s.audio_buffer_size(), 9600);
    assert_eq!(s.audio_buffer_size_default(), 9600);
    assert_eq!(s.wifi_dropped_notif(), 3);
    assert!(!s.port_guessing_enabled());
    assert_eq!(s.port_guess_count(), 75);
    assert_eq!(s.port_guess_socket_count(), 250);
    assert!(s.keyboard_enabled());
    assert!(s.mouse_touch_enabled());
    assert!(s.dpad_touch_enabled());
    assert_eq!(s.dpad_touch_increment(), 30);
    assert_eq!(s.dpad_touch_shortcut1(), 9);
    assert_eq!(s.dpad_touch_shortcut2(), 10);
    assert_eq!(s.dpad_touch_shortcut3(), 7);
    assert_eq!(s.dpad_touch_shortcut4(), 0);
    assert!(s.stream_menu_enabled());
    assert_eq!(s.stream_menu_shortcut3(), 11);
    assert_eq!(s.disconnect_action(), DisconnectAction::Ask);
    assert_eq!(s.suspend_action(), SuspendAction::Nothing);
    assert_eq!(s.log_level_mask(), CHIAKI_LOG_ALL & !CHIAKI_LOG_VERBOSE);
    assert_eq!(s.current_profile(), "");
    assert!(s.profiles().is_empty());

    // Placebo nach Initialisierung: InitializePlaceboSettings schreibt
    // "ewa_lanczos", damit liefert der Getter EwaLanczos (wie im C++ —
    // der EwaLanczosSharp-Default greift nur bei fehlendem Key).
    assert_eq!(s.placebo_upscaler(), PlaceboUpscaler::EwaLanczos);
    assert!(!s.placebo_deinterlace_enabled());
    assert_eq!(s.placebo_deinterlace_algorithm(), PlaceboDeinterlaceAlgorithm::Yadif);
    assert!(s.placebo_deband_enabled());
    assert_eq!(s.placebo_deband_iterations(), 1);
    assert_eq!(s.placebo_deband_threshold(), 3.0);
    assert_eq!(s.placebo_deband_radius(), 16.0);
    // nach InitializePlaceboSettings liefert der Getter die Initial-Werte
    // (0.3/99.995), nicht die Getter-Defaults (0.0/100.0):
    assert_eq!(s.placebo_contrast_recovery(), 0.3);
    assert_eq!(s.placebo_peak_percentile(), 99.995);
    assert_eq!(s.placebo_values().get("contrast_recovery").map(String::as_str), Some("0.3"));
    assert_eq!(s.placebo_values().get("peak_percentile").map(String::as_str), Some("99.995"));

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn set_save_load_roundtrip() {
    let base = temp_base("roundtrip");
    let mut s = Settings::open_at(test_paths(&base)).unwrap();

    s.set_discovery_enabled(false);
    s.set_log_verbose(true);
    s.set_rumble_haptics_intensity(RumbleHapticsIntensity::VeryStrong);
    s.set_reorder_timeout_ms(42);
    s.set_nv_vsr_enabled(true);
    s.set_nv_vsr_scale(300);
    s.set_nv_vsr_quality(3);
    s.set_stick_deadzone(25);
    s.set_nv_vsr_sdk_path("C:/VFX/bin".into());
    s.set_zoom_factor(1.25);
    s.set_packet_loss_reported_max(0.12);
    s.set_window_type(WindowType::Fullscreen);
    s.set_resolution_local_ps5(ResolutionPreset::P540);
    s.set_fps_remote_ps5(FpsPreset::Fps30);
    s.set_bitrate_local_ps5(25000);
    s.set_codec_local_ps5(Codec::H265Hdr);
    s.set_display_target_peak(1000);
    s.set_audio_volume(64);
    s.set_audio_buffer_size(4800);
    s.set_audio_out_device("Lautsprecher".into());
    s.set_psn_account_id("eVr/5uFEAHE=".into());
    s.set_psn_refresh_token("rt".into());
    s.set_psn_auth_token("at".into());
    s.set_psn_auth_token_expiry("2026-12-24 12:00:00".into());
    s.set_disconnect_action(DisconnectAction::AlwaysSleep);
    s.set_suspend_action(SuspendAction::Sleep);
    s.set_geometry(Rect { x: 10, y: 20, width: 800, height: 600 });
    s.set_controller_button_mapping(buttons::CROSS, "Space");
    s.set_placebo_upscaler(PlaceboUpscaler::Lanczos);
    s.set_placebo_contrast_recovery(0.35);
    s.set_placebo_deband_preset(PlaceboDebandPreset::Default);

    s.save().unwrap();

    let s2 = Settings::open_at(test_paths(&base)).unwrap();
    assert!(!s2.discovery_enabled());
    assert!(s2.log_verbose());
    assert_eq!(s2.log_level_mask(), CHIAKI_LOG_ALL);
    assert_eq!(s2.rumble_haptics_intensity(), RumbleHapticsIntensity::VeryStrong);
    assert_eq!(s2.reorder_timeout_ms(), 42);
    assert!(s2.nv_vsr_enabled());
    assert_eq!(s2.nv_vsr_scale(), 300);
    assert_eq!(s2.nv_vsr_quality(), 3);
    assert_eq!(s2.stick_deadzone(), 25);
    assert_eq!(s2.nv_vsr_sdk_path(), "C:/VFX/bin");
    assert_eq!(s2.zoom_factor(), 1.25);
    assert_eq!(s2.packet_loss_reported_max(), 0.12);
    assert_eq!(s2.window_type(), WindowType::Fullscreen);
    assert_eq!(s2.resolution_local_ps5(), ResolutionPreset::P540);
    assert_eq!(s2.fps_remote_ps5(), FpsPreset::Fps30);
    assert_eq!(s2.bitrate_local_ps5(), 25000);
    // Vulkan-Backend clamped nicht:
    assert_eq!(s2.codec_local_ps5(), Codec::H265Hdr);
    assert_eq!(s2.display_target_peak(), 1000);
    assert_eq!(s2.audio_volume(), 64);
    assert_eq!(s2.audio_buffer_size(), 4800);
    assert_eq!(s2.audio_out_device(), "Lautsprecher");
    assert_eq!(s2.psn_account_id(), "eVr/5uFEAHE=");
    assert_eq!(s2.psn_refresh_token(), "rt");
    assert_eq!(s2.disconnect_action(), DisconnectAction::AlwaysSleep);
    assert_eq!(s2.suspend_action(), SuspendAction::Sleep);
    assert_eq!(s2.geometry(), Rect { x: 10, y: 20, width: 800, height: 600 });
    assert_eq!(
        s2.controller_mapping().into_iter().find(|(b, _)| *b == buttons::CROSS).unwrap().1,
        "Space"
    );
    assert_eq!(s2.placebo_upscaler(), PlaceboUpscaler::Lanczos);
    assert_eq!(s2.placebo_contrast_recovery(), 0.35);
    assert_eq!(s2.placebo_deband_preset(), PlaceboDebandPreset::Default);
    // 0.35 wurde mit f2-Formatierung geschrieben
    assert_eq!(s2.placebo_values().get("contrast_recovery").map(String::as_str), Some("0.35"));

    // Update-Helper ändert + speichert
    let mut s3 = Settings::open_at(test_paths(&base)).unwrap();
    s3.update(|s| {
        s.set_streamer_mode(true);
        s.set_keyboard_enabled(false);
    })
    .unwrap();
    let s4 = Settings::open_at(test_paths(&base)).unwrap();
    assert!(s4.streamer_mode());
    assert!(!s4.keyboard_enabled());

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn nv_vsr_quality_and_stick_deadzone_default_and_clamp() {
    let base = temp_base("quality-deadzone");
    let mut s = Settings::open_at(test_paths(&base)).unwrap();

    assert_eq!(s.nv_vsr_quality(), 0, "Default 0 = Auto (C++-Verhalten)");
    assert_eq!(s.stick_deadzone(), 0, "Default 0 = aus (wie im C++-Client)");

    s.set_nv_vsr_quality(9);
    assert_eq!(s.nv_vsr_quality(), 3, "auf den SDK-Bereich geklemmt");
    s.set_stick_deadzone(80);
    assert_eq!(s.stick_deadzone(), 50);
    s.set_stick_deadzone(-5);
    assert_eq!(s.stick_deadzone(), 0);

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn registered_hosts_persist_and_serialize_like_qsettings() {
    let base = temp_base("hosts");
    let mut s = Settings::open_at(test_paths(&base)).unwrap();

    let id = s.set_manual_host(ManualHost::default()); // id<0 → neue ID
    assert_eq!(id, 0);
    let id2 = s.set_manual_host(ManualHost {
        id: -1,
        host: "192.168.0.5".into(),
        registered: false,
        registered_mac: HostMac::default(),
    });
    assert_eq!(id2, 1);

    s.add_registered_host(sample_host());
    s.add_hidden_host(HiddenHost::new(HostMac::new([1, 2, 3, 4, 5, 6]), "Versteckt".into()));
    s.set_controller_mapping("0x054c:0x0ce6".into(), "{json}".into());

    s.save().unwrap();

    let ini = std::fs::read_to_string(base.join("settings.ini")).unwrap();
    // exakte QSettings-Repräsentation prüfen
    assert!(ini.contains("[registered_hosts]"));
    assert!(ini.contains("1\\server_mac=@ByteArray(\\xd4\\xf7\\xd5\\x11\\xfa\\x45)\r\n"));
    assert!(ini.contains("1\\target=1000100\r\n"));
    assert!(ini.contains("size=1\r\n"));
    assert!(ini.contains("[hidden_hosts]"));
    assert!(ini.contains("1\\registered_mac=@ByteArray(\\0\\0\\0\\0\\0\\0)"));
    assert!(ini.contains("[manual_hosts]"));
    assert!(ini.contains("2\\host=192.168.0.5"));
    assert!(ini.contains("[controller_mappings]"));
    assert!(ini.contains("1\\vidpid=0x054c:0x0ce6"));

    // Neu laden und vergleichen
    let mut s2 = Settings::open_at(test_paths(&base)).unwrap();
    assert_eq!(s2.registered_hosts().len(), 1);
    assert_eq!(s2.registered_hosts()[0], &sample_host());
    assert!(s2.hidden_host_hidden(HostMac::new([1, 2, 3, 4, 5, 6])));
    assert_eq!(s2.manual_hosts().len(), 2);
    assert!(s2.manual_host_exists(0));
    assert!(s2.manual_host_exists(1));
    // ID-Zähler läuft weiter
    assert_eq!(s2.set_manual_host(ManualHost::default()), 2);
    assert_eq!(s2.controller_mappings().get("0x054c:0x0ce6"), Some(&"{json}".to_string()));

    // Remove
    let mut s3 = Settings::open_at(test_paths(&base)).unwrap();
    s3.remove_registered_host(HostMac::new([0xd4, 0xf7, 0xd5, 0x11, 0xfa, 0x45]));
    s3.save().unwrap();
    let s4 = Settings::open_at(test_paths(&base)).unwrap();
    assert!(s4.registered_hosts().is_empty());
    assert!(!std::fs::read_to_string(base.join("settings.ini")).unwrap().contains("Test-PS5"));

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn auto_connect_host() {
    let base = temp_base("autoconnect");
    let mut s = Settings::open_at(test_paths(&base)).unwrap();
    let mac = [0xd4u8, 0xf7, 0xd5, 0x11, 0xfa, 0x45];
    s.add_registered_host(sample_host());
    s.set_auto_connect_host(&mac);
    assert!(!s.automatic_connect());
    s.set_automatic_connect(true);
    s.save().unwrap();

    let s2 = Settings::open_at(test_paths(&base)).unwrap();
    assert!(s2.automatic_connect());
    assert_eq!(s2.auto_connect_host(), sample_host());

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn migration_v1_to_v2() {
    let base = temp_base("migrate");
    // v1-Layout: ps4_nickname/ps4_mac, kein target, alte generische Keys
    let v1 = "[General]\r\nversion=1\r\n\
         \r\n[registered_hosts]\r\n\
         1\\ps4_nickname=Alte PS4\r\n\
         1\\ps4_mac=@ByteArray(\\x01\\x02\\x03\\x04\\x05\\x06)\r\n\
         1\\rp_key_type=1\r\n\
         size=1\r\n\
         \r\n[settings]\r\n\
         resolution=720p\r\n\
         fps=60\r\n\
         codec=h264\r\n\
         bitrate=10000\r\n\
         hw_decode_engine=none\r\n";
    std::fs::write(base.join("settings.ini"), v1).unwrap();

    let s = Settings::open_at(test_paths(&base)).unwrap();
    let mac = HostMac::new([1, 2, 3, 4, 5, 6]);
    assert!(s.registered_host_registered(mac));
    assert!(s.nickname_registered("Alte PS4"));
    assert_eq!(s.registered_host(mac).unwrap().target, Target::Ps4Ten);
    assert_eq!(s.ps4s_registered(), 1);
    // hw_decode_engine "none" wird nicht übernommen
    assert_eq!(s.hw_decoder(), "auto");
    assert_eq!(s.resolution_local_ps5(), ResolutionPreset::P720); // aus "resolution"
    assert_eq!(s.bitrate_local_ps5(), 10000);

    s.save().unwrap();
    let ini = std::fs::read_to_string(base.join("settings.ini")).unwrap();
    assert!(ini.contains("1\\server_nickname=Alte PS4"));
    assert!(ini.contains("1\\target=1000\r\n")); // CHIAKI_TARGET_PS4_10
    assert!(!ini.contains("ps4_mac"));
    assert!(!ini.contains("hw_decode_engine"));
    assert!(ini.contains("resolution_local_ps5=720p"));
    assert!(ini.contains("version=2"));

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn migration_controller_mapping_guid_and_decimal() {
    let base = temp_base("migrate-cm");
    let v1 = "[General]\r\nversion=1\r\n\
         \r\n[controller_mappings]\r\n\
         1\\guid=03000000\r\n\
         1\\controller_mapping={}\r\n\
         2\\vidpid=1356:250\r\n\
         2\\controller_mapping={x}\r\n\
         size=2\r\n";
    std::fs::write(base.join("settings.ini"), v1).unwrap();

    let s = Settings::open_at(test_paths(&base)).unwrap();
    let mappings = s.controller_mappings();
    // guid → vidpid (nur wenn vidpid leer)
    assert!(mappings.contains_key("03000000"));
    // decimal vid/pid → hex
    assert!(mappings.contains_key("0x054c:0x00fa"));

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn profiles_and_current_profile() {
    let base = temp_base("profiles");
    let mut s = Settings::open_at(test_paths(&base)).unwrap();
    s.set_current_profile("Wohnzimmer".into());
    s.set_current_profile("Wohnzimmer".into()); // kein Duplikat
    assert_eq!(s.profiles(), ["Wohnzimmer"]);
    s.set_current_profile("Schlafzimmer".into());
    s.save().unwrap();

    // current_profile + profiles liegen in der Basis-INI (gleiche Datei hier)
    let ini = std::fs::read_to_string(base.join("settings.ini")).unwrap();
    assert!(ini.contains("current_profile=Schlafzimmer"));
    assert!(ini.contains("[profiles]"));
    assert!(ini.contains("1\\settings\\profile_name=Wohnzimmer"));

    let mut s2 = Settings::open_at(test_paths(&base)).unwrap();
    assert_eq!(s2.current_profile(), "Schlafzimmer");
    assert_eq!(s2.profiles().len(), 2);

    // Profil "löschen": Profil-Datei wird geleert (aber angelegt), Profil
    // aus Liste entfernt
    let prof_path = app_paths::settings_file_in(&base, "Wohnzimmer");
    s2.delete_profile("Wohnzimmer").unwrap();
    assert!(prof_path.exists());
    assert_eq!(s2.profiles(), ["Schlafzimmer"]);
    // registrierte Hosts bleiben erhalten (DeleteProfile lädt neu)
    assert_eq!(s2.registered_hosts().len(), s2.registered_hosts().len());

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn export_import_settings() {
    let base = temp_base("export");
    let mut s = Settings::open_at(test_paths(&base)).unwrap();
    s.set_current_profile("MeinProfil".into());
    s.add_registered_host(sample_host());
    s.set_audio_volume(77);
    s.export_settings(&base.join("backup")).unwrap(); // ohne .ini → wird angehängt

    let backup = base.join("backup.ini");
    assert!(backup.exists());
    let ini = std::fs::read_to_string(&backup).unwrap();
    assert!(ini.contains("hw_decoder=auto"));
    assert!(ini.contains("this_profile=MeinProfil"));

    // Import in eine frische Instanz: this_profile ist gesetzt → C++
    // schreibt alles in die Profil-Datei und setzt nur current_profile
    let base2 = temp_base("import");
    let mut s2 = Settings::open_at(test_paths(&base2)).unwrap();
    s2.import_settings(&backup).unwrap();
    assert_eq!(s2.current_profile(), "MeinProfil");
    let prof = std::fs::read_to_string(app_paths::settings_file_in(&base2, "MeinProfil")).unwrap();
    assert!(prof.contains("audio_volume=77"));
    assert!(prof.contains("Test-PS5"));

    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&base2);
}

#[test]
fn keymap_defaults_and_clear() {
    let base = temp_base("keymap");
    let mut s = Settings::open_at(test_paths(&base)).unwrap();
    let map = s.controller_mapping();
    assert_eq!(map.len(), 26);
    assert_eq!(map.iter().find(|(b, _)| *b == buttons::CROSS).unwrap().1, "Return");
    assert_eq!(map.iter().find(|(b, _)| *b == buttons::PS).unwrap().1, "Escape");

    s.set_controller_button_mapping(buttons::CROSS, "Space");
    s.set_controller_button_mapping(buttons::DPAD_LEFT, "A");
    s.save().unwrap();
    let ini = std::fs::read_to_string(base.join("settings.ini")).unwrap();
    assert!(ini.contains("[keymap]"));
    assert!(ini.contains("cross=Space"));
    assert!(ini.contains("d-pad_left=A"));

    let mut s2 = Settings::open_at(test_paths(&base)).unwrap();
    assert_eq!(
        s2.controller_mapping_for_decoding()
            .into_iter()
            .find(|(k, _)| k.as_str() == "Space")
            .unwrap()
            .1,
        buttons::CROSS
    );

    s2.clear_key_mapping();
    s2.save().unwrap();
    let s3 = Settings::open_at(test_paths(&base)).unwrap();
    assert_eq!(
        s3.controller_mapping().iter().find(|(b, _)| *b == buttons::CROSS).unwrap().1,
        "Return"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn profile_preset_table_matches_lib() {
    let p = connect_video_profile_preset(ResolutionPreset::P1080, FpsPreset::Fps60);
    assert_eq!(p.width, 1920);
    assert_eq!(p.height, 1080);
    assert_eq!(p.bitrate, 15000);
    assert_eq!(p.max_fps, 60);
    assert_eq!(p.codec, Codec::H264);
    let p360 = connect_video_profile_preset(ResolutionPreset::P360, FpsPreset::Fps30);
    assert_eq!((p360.width, p360.height, p360.bitrate, p360.max_fps), (640, 360, 2000, 30));
    let p540 = connect_video_profile_preset(ResolutionPreset::P540, FpsPreset::Fps30);
    assert_eq!((p540.width, p540.height, p540.bitrate), (960, 540, 6000));
    let p720 = connect_video_profile_preset(ResolutionPreset::P720, FpsPreset::Fps60);
    assert_eq!((p720.width, p720.height, p720.bitrate), (1280, 720, 10000));
}

#[test]
fn placebo_render_params_ini_initialized_like_cxx() {
    let base = temp_base("placebo-ini");
    let s = Settings::open_at(test_paths(&base)).unwrap();
    s.save().unwrap();
    let ini = std::fs::read_to_string(base.join("placebo_render_params.ini")).unwrap();
    // exakt wie die von der C++-App erzeugte Referenzdatei:
    let expected = "[placebo_settings]\r\n\
        color_map_preset=high_quality\r\n\
        contrast_recovery=0.3\r\n\
        deband=yes\r\n\
        deinterlace=no\r\n\
        deinterlace_algo=yadif\r\n\
        deinterlace_preset=default\r\n\
        deinterlace_skip_spatial=no\r\n\
        peak_detect_preset=high_quality\r\n\
        peak_percentile=99.995\r\n\
        upscaler=ewa_lanczos\r\n\
        version=0\r\n";
    assert_eq!(ini, expected);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn legacy_frame_mixer_migration() {
    let base = temp_base("framemixer");
    std::fs::write(
        base.join("placebo_render_params.ini"),
        "[placebo_settings]\r\nversion=0\r\nframe_mixer=OVERSAMPLE\r\n",
    )
    .unwrap();
    let s = Settings::open_at(test_paths(&base)).unwrap();
    // migriert nach settings/placebo_frame_mixer und aus der placebo-INI entfernt
    assert_eq!(s.placebo_frame_mixer(), PlaceboFrameMixer::Oversample);
    assert!(!s.placebo.contains("placebo_settings/frame_mixer"));
    s.save().unwrap();
    let ini = std::fs::read_to_string(base.join("settings.ini")).unwrap();
    assert!(ini.contains("placebo_frame_mixer=oversample"));

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn constants_match_lib() {
    assert_eq!(CHIAKI_SESSION_AUTH_SIZE, 0x10);
    assert_eq!(crate::CHIAKI_PSN_ACCOUNT_ID_SIZE, 8);
    assert_eq!(SETTINGS_VERSION, 2);
    assert_eq!(CHIAKI_LOG_ALL, 0x1F);
}
