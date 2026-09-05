//! chiaki-settings — Settings-Single-Source-of-Truth der Rust-App.
//!
//! 1:1-Port der chiaki-ng GUI-Settings (`gui/src/settings.cpp`,
//! `gui/include/settings.h`, `gui/src/host.cpp`, `gui/src/apppaths.cpp`).
//! Alle INI-Keys, Defaults und Serialisierungsformate sind bytekompatibel
//! zur C++-QSettings-Ausgabe (siehe [`ini`]).
//!
//! Module:
//! - [`app_paths`]: portabler `data/`-Modus mit `%APPDATA%/Chiaki-Rs`-Fallback
//! - [`ini`]: Qt-QSettings-INI-kompatibler Store (parse + serialize)
//! - [`hosts`]: HostMAC, RegisteredHost, HiddenHost, ManualHost, PsnHost
//! - [`psn`]: PSN-Token/Account-ID (INI-Keys `settings/psn_*`)
//! - [`settings`]: die `Settings`-Klasse mit allen Accessoren, Migrationen,
//!   Host-Verwaltung, Profilen, Keymap, Export/Import
//!
//! Kein eigenes `audio_devices`-Modul: Audio-In/Out-Geräte sind einfache
//! String-Keys (`settings/audio_out_device`, `settings/audio_in_device`).
//! Die libplacebo-Renderparameter (`placebo_render_params.ini`) gehören
//! zur `Settings`-Klasse (C++-Konstruktor lädt sie mit).

#![deny(unsafe_code)]

pub mod app_paths;
pub mod hosts;
pub mod ini;
pub mod psn;
pub mod settings;

pub use hosts::{
    CHIAKI_PSN_ACCOUNT_ID_SIZE, CHIAKI_SESSION_AUTH_SIZE, HiddenHost, HostMac, ManualHost, PsnHost,
    RegisteredHost, Target,
};
pub use ini::{IniStore, Value};
pub use psn::PsnAccountData;
pub use settings::{
    connect_video_profile_preset, controller_button_name, buttons as controller_buttons, Codec,
    ConnectVideoProfile, Decoder, DisableAudioVideo, DisconnectAction, Error, FpsPreset, Rect,
    RenderBackend, ResolutionPreset, Result, RumbleHapticsIntensity, Settings, SettingsPaths,
    SuspendAction, WindowType,
};
