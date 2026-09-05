//! Port von `gui/src/settings.cpp` + `gui/include/settings.h`.
//!
//! Alle INI-Keys sind 1:1 aus dem C++ übernommen (Sektion `settings`,
//! Host-Arrays `registered_hosts`/`hidden_hosts`/`manual_hosts`/
//! `controller_mappings`/`profiles`, Keymap-Sektion `keymap`,
//! libplacebo-Sektion `placebo_settings` in `placebo_render_params.ini`).
//!
//! Speicherverhalten: Im C++ puffert `QSettings` Änderungen im Speicher und
//! schreibt lazy (sync/Destruktor). Hier ändern die Set-Methoden nur den
//! In-Memory-Store; die App ruft nach Änderungen [`Settings::save`] oder
//! benutzt den Helper [`Settings::update`].
//!
//! Legacy-Migration aus der Windows-Registry (`ImportLegacySettings`) ist
//! bewusst NICHT portiert — die Rust-App ist portable-first; die C++-INI-
//! Migrationen (v1→v2, Video-Profil, Controller-Mappings, Frame-Mixer)
//! sind vollständig übernommen.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::app_paths;
use crate::hosts::{HostMac, HiddenHost, ManualHost, RegisteredHost, Target};
use crate::ini::{IniStore, Value};

/// `#define SETTINGS_VERSION 2`
pub const SETTINGS_VERSION: i64 = 2;
/// `PORT_GUESS_COUNT_DEFAULT`
pub const PORT_GUESS_COUNT_DEFAULT: i64 = 75;
/// `PORT_GUESS_SOCKS_DEFAULT`
pub const PORT_GUESS_SOCKS_DEFAULT: i64 = 250;
/// `SDL_MIX_MAXVOLUME` (Default für settings/audio_volume)
pub const SDL_MIX_MAXVOLUME: i64 = 128;
/// `CHIAKI_LOG_ALL` ((1 << 5) - 1)
pub const CHIAKI_LOG_ALL: u32 = 0x1F;
/// `CHIAKI_LOG_VERBOSE` (1 << 3)
pub const CHIAKI_LOG_VERBOSE: u32 = 1 << 3;
/// `GetAudioBufferSizeDefault()`
pub const AUDIO_BUFFER_SIZE_DEFAULT: u64 = 9600;

// ---------------------------------------------------------------------------
// Chiaki-Enums, die die Settings speichern (Werte wie in lib/include/chiaki,
// dort mit "values must not change" markiert)
// ---------------------------------------------------------------------------

/// `ChiakiVideoResolutionPreset` (session.h)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(i32)]
pub enum ResolutionPreset {
    P360 = 1,
    P540 = 2,
    #[default]
    P720 = 3,
    P1080 = 4,
}

/// `ChiakiVideoFPSPreset` (session.h)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(i32)]
pub enum FpsPreset {
    Fps30 = 30,
    #[default]
    Fps60 = 60,
}

/// `ChiakiCodec` (common.h)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(i32)]
pub enum Codec {
    #[default]
    H264 = 0,
    H265 = 1,
    H265Hdr = 2,
}

/// `ChiakiDisableAudioVideo` (takion.h)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(i32)]
pub enum DisableAudioVideo {
    #[default]
    None = 0,
    Audio = 1,
    Video = 2,
    AudioVideo = 3,
}

/// `ChiakiConnectVideoProfile` (session.h)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConnectVideoProfile {
    pub width: u32,
    pub height: u32,
    pub max_fps: u32,
    pub bitrate: u32,
    pub codec: Codec,
}

/// `chiaki_connect_video_profile_preset` (lib/src/session.c)
pub fn connect_video_profile_preset(resolution: ResolutionPreset, fps: FpsPreset) -> ConnectVideoProfile {
    let (width, height, bitrate) = match resolution {
        ResolutionPreset::P360 => (640, 360, 2000),
        ResolutionPreset::P540 => (960, 540, 6000),
        ResolutionPreset::P720 => (1280, 720, 10000),
        ResolutionPreset::P1080 => (1920, 1080, 15000),
    };
    ConnectVideoProfile {
        width,
        height,
        bitrate,
        max_fps: fps as u32,
        codec: Codec::H264,
    }
}

// ---------------------------------------------------------------------------
// GUI-Enums mit ihren QSettings-String-Repräsentationen
// ---------------------------------------------------------------------------

/// `RumbleHapticsIntensity`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RumbleHapticsIntensity {
    Off,
    VeryWeak,
    Weak,
    #[default]
    Normal,
    Strong,
    VeryStrong,
}

const RUMBLE_INTENSITIES: &[(RumbleHapticsIntensity, &str)] = &[
    (RumbleHapticsIntensity::Off, "Off"),
    (RumbleHapticsIntensity::VeryWeak, "Very weak"),
    (RumbleHapticsIntensity::Weak, "Weak"),
    (RumbleHapticsIntensity::Normal, "Normal"),
    (RumbleHapticsIntensity::Strong, "Strong"),
    (RumbleHapticsIntensity::VeryStrong, "Very Strong"),
];

/// `DisconnectAction`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DisconnectAction {
    AlwaysNothing,
    AlwaysSleep,
    #[default]
    Ask,
}

const DISCONNECT_ACTIONS: &[(DisconnectAction, &str)] = &[
    (DisconnectAction::Ask, "ask"),
    (DisconnectAction::AlwaysNothing, "nothing"),
    (DisconnectAction::AlwaysSleep, "sleep"),
];

/// `SuspendAction`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SuspendAction {
    #[default]
    Nothing,
    Sleep,
}

const SUSPEND_ACTIONS: &[(SuspendAction, &str)] = &[
    (SuspendAction::Sleep, "sleep"),
    (SuspendAction::Nothing, "nothing"),
];

/// `Decoder`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Decoder {
    #[default]
    Ffmpeg,
    Pi,
}

const DECODERS: &[(Decoder, &str)] = &[(Decoder::Ffmpeg, "ffmpeg"), (Decoder::Pi, "pi")];

/// `PlaceboPreset`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaceboPreset {
    Fast,
    #[default]
    HighQuality,
    Default,
    HighQualitySpatial,
    HighQualityAdvancedSpatial,
    Custom,
}

const PLACEBO_PRESETS: &[(PlaceboPreset, &str)] = &[
    (PlaceboPreset::Fast, "fast"),
    (PlaceboPreset::Default, "default"),
    (PlaceboPreset::HighQuality, "high_quality"),
    (PlaceboPreset::HighQualitySpatial, "high_quality_spatial"),
    (PlaceboPreset::HighQualityAdvancedSpatial, "high_quality_advanced_spatial"),
    (PlaceboPreset::Custom, "custom"),
];

/// `RenderBackend`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RenderBackend {
    #[default]
    Vulkan,
    OpenGL,
}

const RENDER_BACKENDS: &[(RenderBackend, &str)] = &[
    (RenderBackend::Vulkan, "vulkan"),
    (RenderBackend::OpenGL, "opengl"),
];

/// `WindowType`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WindowType {
    SelectedResolution,
    CustomResolution,
    #[default]
    AdjustableResolution,
    Fullscreen,
    Zoom,
    Stretch,
}

const WINDOW_TYPES: &[(WindowType, &str)] = &[
    (WindowType::SelectedResolution, "Selected Resolution"),
    (WindowType::CustomResolution, "Custom Resolution"),
    (WindowType::AdjustableResolution, "Adjust Manually"),
    (WindowType::Fullscreen, "Fullscreen"),
    (WindowType::Zoom, "Zoom"),
    (WindowType::Stretch, "Stretch"),
];

/// `PlaceboUpscaler`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaceboUpscaler {
    #[default]
    None,
    Nearest,
    Bilinear,
    Oversample,
    Bicubic,
    Gaussian,
    CatmullRom,
    Lanczos,
    EwaLanczos,
    EwaLanczosSharp,
    EwaLanczos4Sharpest,
    Fsr,
    Fsrcnnx8,
    Fsrcnnx16,
}

const PLACEBO_UPSCALERS: &[(PlaceboUpscaler, &str)] = &[
    (PlaceboUpscaler::None, "none"),
    (PlaceboUpscaler::Nearest, "nearest"),
    (PlaceboUpscaler::Bilinear, "bilinear"),
    (PlaceboUpscaler::Oversample, "oversample"),
    (PlaceboUpscaler::Bicubic, "bicubic"),
    (PlaceboUpscaler::Gaussian, "gaussian"),
    (PlaceboUpscaler::CatmullRom, "catmull_rom"),
    (PlaceboUpscaler::Lanczos, "lanczos"),
    (PlaceboUpscaler::EwaLanczos, "ewa_lanczos"),
    (PlaceboUpscaler::EwaLanczosSharp, "ewa_lanczossharp"),
    (PlaceboUpscaler::EwaLanczos4Sharpest, "ewa_lanczos4sharpest"),
    (PlaceboUpscaler::Fsr, "fsr"),
    (PlaceboUpscaler::Fsrcnnx8, "fsrcnnx_x2_8_0_4_1"),
    (PlaceboUpscaler::Fsrcnnx16, "fsrcnnx_x2_16_0_4_1"),
];

/// `PlaceboDownscaler`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaceboDownscaler {
    #[default]
    None,
    Box,
    Hermite,
    Bilinear,
    Bicubic,
    Gaussian,
    CatmullRom,
    Mitchell,
    Lanczos,
}

const PLACEBO_DOWNSCALERS: &[(PlaceboDownscaler, &str)] = &[
    (PlaceboDownscaler::None, "none"),
    (PlaceboDownscaler::Box, "box"),
    (PlaceboDownscaler::Hermite, "hermite"),
    (PlaceboDownscaler::Bilinear, "bilinear"),
    (PlaceboDownscaler::Bicubic, "bicubic"),
    (PlaceboDownscaler::Gaussian, "gaussian"),
    (PlaceboDownscaler::CatmullRom, "catmull_rom"),
    (PlaceboDownscaler::Mitchell, "mitchell"),
    (PlaceboDownscaler::Lanczos, "lanczos"),
];

/// `PlaceboFrameMixer`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaceboFrameMixer {
    #[default]
    None,
    Oversample,
    Hermite,
    Linear,
    Cubic,
}

const PLACEBO_FRAME_MIXERS: &[(PlaceboFrameMixer, &str)] = &[
    (PlaceboFrameMixer::None, "none"),
    (PlaceboFrameMixer::Oversample, "oversample"),
    (PlaceboFrameMixer::Hermite, "hermite"),
    (PlaceboFrameMixer::Linear, "linear"),
    (PlaceboFrameMixer::Cubic, "cubic"),
];

/// `PlaceboDeinterlacePreset`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaceboDeinterlacePreset {
    #[default]
    Default,
}

/// `PlaceboDeinterlaceAlgorithm`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaceboDeinterlaceAlgorithm {
    Weave,
    Bob,
    #[default]
    Yadif,
    Bwdif,
}

const PLACEBO_DEINTERLACE_PRESETS: &[(PlaceboDeinterlacePreset, &str)] =
    &[(PlaceboDeinterlacePreset::Default, "default")];

const PLACEBO_DEINTERLACE_ALGOS: &[(PlaceboDeinterlaceAlgorithm, &str)] = &[
    (PlaceboDeinterlaceAlgorithm::Weave, "weave"),
    (PlaceboDeinterlaceAlgorithm::Bob, "bob"),
    (PlaceboDeinterlaceAlgorithm::Yadif, "yadif"),
    (PlaceboDeinterlaceAlgorithm::Bwdif, "bwdif"),
];

/// `PlaceboDebandPreset` — `None` hat im C++ einen LEEREN String als Wert
/// (der Key wird dann beim Setzen entfernt).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaceboDebandPreset {
    #[default]
    None,
    Default,
}

/// `PlaceboSigmoidPreset`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaceboSigmoidPreset {
    #[default]
    None,
    Default,
}

/// `PlaceboColorAdjustmentPreset`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaceboColorAdjustmentPreset {
    #[default]
    None,
    Neutral,
}

/// `PlaceboPeakDetectionPreset`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaceboPeakDetectionPreset {
    #[default]
    None,
    Default,
    HighQuality,
}

/// `PlaceboColorMappingPreset`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaceboColorMappingPreset {
    #[default]
    None,
    Default,
    HighQuality,
}

/// `PlaceboGamutMappingFunction`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaceboGamutMappingFunction {
    Clip,
    #[default]
    Perceptual,
    SoftClip,
    Relative,
    Saturation,
    Absolute,
    Desaturate,
    Darken,
    Highlight,
    Linear,
}

const PLACEBO_GAMUT_MAPPINGS: &[(PlaceboGamutMappingFunction, &str)] = &[
    (PlaceboGamutMappingFunction::Clip, "clip"),
    (PlaceboGamutMappingFunction::Perceptual, "perceptual"),
    (PlaceboGamutMappingFunction::SoftClip, "softclip"),
    (PlaceboGamutMappingFunction::Relative, "relative"),
    (PlaceboGamutMappingFunction::Saturation, "saturation"),
    (PlaceboGamutMappingFunction::Absolute, "absolute"),
    (PlaceboGamutMappingFunction::Desaturate, "desaturate"),
    (PlaceboGamutMappingFunction::Darken, "darken"),
    (PlaceboGamutMappingFunction::Highlight, "highlight"),
    (PlaceboGamutMappingFunction::Linear, "linear"),
];

/// `PlaceboToneMappingFunction`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaceboToneMappingFunction {
    Clip,
    #[default]
    Spline,
    St209440,
    St209410,
    Bt2390,
    Bt2446a,
    Reinhard,
    Mobius,
    Hable,
    Gamma,
    Linear,
    LinearLight,
}

const PLACEBO_TONE_MAPPINGS: &[(PlaceboToneMappingFunction, &str)] = &[
    (PlaceboToneMappingFunction::Clip, "clip"),
    (PlaceboToneMappingFunction::Spline, "spline"),
    (PlaceboToneMappingFunction::St209440, "st2094-40"),
    (PlaceboToneMappingFunction::St209410, "st2094-10"),
    (PlaceboToneMappingFunction::Bt2390, "bt2390"),
    (PlaceboToneMappingFunction::Bt2446a, "bt2446a"),
    (PlaceboToneMappingFunction::Reinhard, "reinhard"),
    (PlaceboToneMappingFunction::Mobius, "mobius"),
    (PlaceboToneMappingFunction::Hable, "hable"),
    (PlaceboToneMappingFunction::Gamma, "gamma"),
    (PlaceboToneMappingFunction::Linear, "linear"),
    (PlaceboToneMappingFunction::LinearLight, "linearlight"),
];

/// `PlaceboToneMappingMetadata`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaceboToneMappingMetadata {
    #[default]
    Any,
    None,
    Hdr10,
    Hdr10Plus,
    CieY,
}

const PLACEBO_TONE_METADATA: &[(PlaceboToneMappingMetadata, &str)] = &[
    (PlaceboToneMappingMetadata::Any, "any"),
    (PlaceboToneMappingMetadata::None, "none"),
    (PlaceboToneMappingMetadata::Hdr10, "hdr10"),
    (PlaceboToneMappingMetadata::Hdr10Plus, "hdr10plus"),
    (PlaceboToneMappingMetadata::CieY, "cie_y"),
];

fn enum_name<T: PartialEq>(pairs: &[(T, &'static str)], v: T) -> &'static str {
    pairs.iter().find(|(val, _)| *val == v).map(|(_, s)| *s).unwrap_or("")
}

fn enum_value<T: Copy + PartialEq>(pairs: &[(T, &'static str)], s: &str, default: T) -> T {
    pairs.iter().find(|(_, key)| *key == s).map(|(v, _)| *v).unwrap_or(default)
}

// `PlaceboDebandPreset`/`Sigmoid`/`ColorAdjustment`/`PeakDetection`/
// `ColorMapping`-Presets: None → leerer String → Key wird entfernt.
const EMPTY_PRESETS: &str = "";

fn preset_name_with_empty<T: PartialEq>(pairs: &[(T, &'static str)], v: T, empty: T) -> &'static str {
    if v == empty {
        EMPTY_PRESETS
    } else {
        enum_name(pairs, v)
    }
}

// ---------------------------------------------------------------------------
// Controller-Keymap (GetChiakiControllerButtonName + Defaults)
// ---------------------------------------------------------------------------

/// `ChiakiControllerButton` + `ChiakiControllerAnalogButton` (controller.h)
pub mod buttons {
    pub const CROSS: u32 = 1 << 0;
    pub const MOON: u32 = 1 << 1;
    pub const BOX: u32 = 1 << 2;
    pub const PYRAMID: u32 = 1 << 3;
    pub const DPAD_LEFT: u32 = 1 << 4;
    pub const DPAD_RIGHT: u32 = 1 << 5;
    pub const DPAD_UP: u32 = 1 << 6;
    pub const DPAD_DOWN: u32 = 1 << 7;
    pub const L1: u32 = 1 << 8;
    pub const R1: u32 = 1 << 9;
    pub const L3: u32 = 1 << 10;
    pub const R3: u32 = 1 << 11;
    pub const OPTIONS: u32 = 1 << 12;
    pub const SHARE: u32 = 1 << 13;
    pub const TOUCHPAD: u32 = 1 << 14;
    pub const PS: u32 = 1 << 15;
    pub const ANALOG_L2: u32 = 1 << 16;
    pub const ANALOG_R2: u32 = 1 << 17;

    /// `ControllerButtonExt` (settings.h) — darf nicht mit den obigen
    /// Bits kollidieren.
    pub const EXT_ANALOG_STICK_LEFT_X_UP: u32 = 1 << 18;
    pub const EXT_ANALOG_STICK_LEFT_X_DOWN: u32 = 1 << 19;
    pub const EXT_ANALOG_STICK_LEFT_Y_UP: u32 = 1 << 20;
    pub const EXT_ANALOG_STICK_LEFT_Y_DOWN: u32 = 1 << 21;
    pub const EXT_ANALOG_STICK_RIGHT_X_UP: u32 = 1 << 22;
    pub const EXT_ANALOG_STICK_RIGHT_X_DOWN: u32 = 1 << 23;
    pub const EXT_ANALOG_STICK_RIGHT_Y_UP: u32 = 1 << 24;
    pub const EXT_ANALOG_STICK_RIGHT_Y_DOWN: u32 = 1 << 25;
    pub const EXT_ANALOG_STICK_LEFT_X: u32 = 1 << 26;
    pub const EXT_ANALOG_STICK_LEFT_Y: u32 = 1 << 27;
    pub const EXT_ANALOG_STICK_RIGHT_X: u32 = 1 << 28;
    pub const EXT_ANALOG_STICK_RIGHT_Y: u32 = 1 << 29;
    pub const EXT_MISC1: u32 = 1 << 30;
}

/// `Settings::GetChiakiControllerButtonName` — Namen exakt wie im C++
/// (inkl. der dortigen Zuordnung der Stick-Himmelsrichtungen).
pub fn controller_button_name(button: u32) -> &'static str {
    use buttons::*;
    match button {
        CROSS => "Cross",
        MOON => "Moon",
        BOX => "Box",
        PYRAMID => "Pyramid",
        DPAD_LEFT => "D-Pad Left",
        DPAD_RIGHT => "D-Pad Right",
        DPAD_UP => "D-Pad Up",
        DPAD_DOWN => "D-Pad Down",
        L1 => "L1",
        R1 => "R1",
        L3 => "L3",
        R3 => "R3",
        OPTIONS => "Options",
        SHARE => "Share",
        TOUCHPAD => "Touchpad",
        PS => "PS",
        ANALOG_L2 => "L2",
        ANALOG_R2 => "R2",
        EXT_ANALOG_STICK_LEFT_X_UP => "Left Stick Right",
        EXT_ANALOG_STICK_LEFT_Y_UP => "Left Stick Up",
        EXT_ANALOG_STICK_RIGHT_X_UP => "Right Stick Right",
        EXT_ANALOG_STICK_RIGHT_Y_UP => "Right Stick Up",
        EXT_ANALOG_STICK_LEFT_X_DOWN => "Left Stick Left",
        EXT_ANALOG_STICK_LEFT_Y_DOWN => "Left Stick Down",
        EXT_ANALOG_STICK_RIGHT_X_DOWN => "Right Stick Left",
        EXT_ANALOG_STICK_RIGHT_Y_DOWN => "Right Stick Down",
        EXT_ANALOG_STICK_LEFT_X => "Left Stick X",
        EXT_ANALOG_STICK_LEFT_Y => "Left Stick Y",
        EXT_ANALOG_STICK_RIGHT_X => "Right Stick X",
        EXT_ANALOG_STICK_RIGHT_Y => "Right Stick Y",
        EXT_MISC1 => "MIC",
        _ => "Unknown",
    }
}

/// Key-Name wie `GetChiakiControllerButtonName(b).replace(' ', '_').toLower()`.
fn button_key_name(button: u32) -> String {
    controller_button_name(button).replace(' ', "_").to_lowercase()
}

/// Default-Tastaturbelegung (`GetControllerMapping`-Initialisierung);
/// Werte sind Qt-KeySequence-Namen (QKeySequence(key).toString()).
fn default_keymap() -> Vec<(u32, &'static str)> {
    use buttons::*;
    vec![
        (CROSS, "Return"),
        (MOON, "Backspace"),
        (BOX, "Backslash"),
        (PYRAMID, "C"),
        (DPAD_LEFT, "Left"),
        (DPAD_RIGHT, "Right"),
        (DPAD_UP, "Up"),
        (DPAD_DOWN, "Down"),
        (L1, "2"),
        (R1, "3"),
        (L3, "5"),
        (R3, "6"),
        (OPTIONS, "O"),
        (SHARE, "F"),
        (TOUCHPAD, "T"),
        (PS, "Escape"),
        (ANALOG_L2, "1"),
        (ANALOG_R2, "4"),
        (EXT_ANALOG_STICK_LEFT_X_UP, "]"),
        (EXT_ANALOG_STICK_LEFT_X_DOWN, "["),
        (EXT_ANALOG_STICK_LEFT_Y_UP, "Insert"),
        (EXT_ANALOG_STICK_LEFT_Y_DOWN, "Delete"),
        (EXT_ANALOG_STICK_RIGHT_X_UP, "="),
        (EXT_ANALOG_STICK_RIGHT_X_DOWN, "-"),
        (EXT_ANALOG_STICK_RIGHT_Y_UP, "PgUp"),
        (EXT_ANALOG_STICK_RIGHT_Y_DOWN, "PgDown"),
    ]
}

// ---------------------------------------------------------------------------
// Accessor-Makros
// ---------------------------------------------------------------------------

macro_rules! acc_bool {
    ($get:ident, $set:ident, $key:expr, $default:expr) => {
        pub fn $get(&self) -> bool {
            self.store.bool_or($key, $default)
        }
        pub fn $set(&mut self, enabled: bool) {
            self.store.set_value($key, Value::Bool(enabled));
        }
    };
}

macro_rules! acc_int {
    ($get:ident, $set:ident, $key:expr, $default:expr) => {
        pub fn $get(&self) -> i64 {
            self.store.int_or($key, $default)
        }
        pub fn $set(&mut self, v: i64) {
            self.store.set_value($key, Value::Int(v));
        }
    };
}

macro_rules! acc_uint {
    ($get:ident, $set:ident, $key:expr, $default:expr) => {
        pub fn $get(&self) -> u64 {
            self.store.uint_or($key, $default)
        }
        pub fn $set(&mut self, v: u64) {
            self.store.set_value($key, Value::UInt(v));
        }
    };
}

macro_rules! acc_float {
    ($get:ident, $set:ident, $key:expr, $default:expr) => {
        pub fn $get(&self) -> f64 {
            self.store.float_or($key, $default)
        }
        pub fn $set(&mut self, v: f64) {
            self.store.set_value($key, Value::Float(v));
        }
    };
}

macro_rules! acc_string {
    ($get:ident, $set:ident, $key:expr) => {
        pub fn $get(&self) -> String {
            self.store.string_or($key, "")
        }
        pub fn $set(&mut self, v: String) {
            self.store.set_value($key, Value::Str(v));
        }
    };
}

/// Float mit fixer Nachkommastellenzahl beim Schreiben
/// (C++ `QString("%1").arg(v, 0, 'f', N)`).
macro_rules! placebo_float_fixed {
    ($get:ident, $set:ident, $key:expr, $default:expr, $prec:expr) => {
        pub fn $get(&self) -> f64 {
            self.placebo.float_or($key, $default)
        }
        pub fn $set(&mut self, v: f64) {
            let s = format!("{:.*}", $prec, v);
            self.placebo.set_value($key, Value::Str(s));
        }
    };
}

macro_rules! placebo_int {
    ($get:ident, $set:ident, $key:expr, $default:expr) => {
        pub fn $get(&self) -> i64 {
            self.placebo.int_or($key, $default)
        }
        pub fn $set(&mut self, v: i64) {
            self.placebo.set_value($key, Value::Int(v));
        }
    };
}

/// Preset mit ""-Semantik: None (leerer String) entfernt den Key.
macro_rules! placebo_preset_with_empty {
    ($get:ident, $set:ident, $key:expr, $pairs:expr, $ty:ty, $default:expr, $none:expr) => {
        pub fn $get(&self) -> $ty {
            let v = self.placebo.string_or($key, enum_name($pairs, $default));
            enum_value($pairs, &v, $default)
        }
        pub fn $set(&mut self, preset: $ty) {
            let name = preset_name_with_empty($pairs, preset, $none);
            if name.is_empty() {
                self.placebo.remove($key);
            } else {
                self.placebo.set_value($key, Value::Str(name.to_string()));
            }
        }
    };
}

macro_rules! placebo_enum {
    ($get:ident, $set:ident, $key:expr, $pairs:expr, $ty:ty, $default:expr) => {
        pub fn $get(&self) -> $ty {
            let v = self.placebo.string_or($key, enum_name($pairs, $default));
            enum_value($pairs, &v, $default)
        }
        pub fn $set(&mut self, v: $ty) {
            self.placebo.set_value($key, Value::Str(enum_name($pairs, v).to_string()));
        }
    };
}

/// Fehler-Typ der Crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("IO-Fehler bei Settings-Zugriff: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// QRect-Ersatz für die Fenster-Geometrien.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Pfade der drei INI-Dateien + Daten-Basispfad (für
/// [`Settings::open_at`], z.B. Tests).
#[derive(Debug, Clone)]
pub struct SettingsPaths {
    pub base: PathBuf,
    pub settings: PathBuf,
    pub default_settings: PathBuf,
    pub placebo: PathBuf,
}

/// Die Settings-Single-Source-of-Truth der Rust-App.
///
/// Drei Store-Instanzen wie im C++-Konstruktor:
/// - `store`: `settings.ini` des aktuellen Profils (oder die Basis-Datei)
/// - `default_store`: immer die Basis-`settings.ini` (`current_profile`, `profiles`)
/// - `placebo`: `placebo_render_params.ini`
pub struct Settings {
    store: IniStore,
    default_store: IniStore,
    placebo: IniStore,
    paths: SettingsPaths,
    same_settings_file: bool,

    hidden_hosts: BTreeMap<u64, HiddenHost>,
    registered_hosts: BTreeMap<u64, RegisteredHost>,
    nickname_registered_hosts: BTreeMap<String, RegisteredHost>,
    ps4s_registered: usize,
    manual_hosts: BTreeMap<i32, ManualHost>,
    manual_hosts_id_next: i32,
    controller_mappings: BTreeMap<String, String>,
    profiles: Vec<String>,
    time_format: String,
}

impl Settings {
    /// Öffnet die Settings für ein Profil (wie `Settings::Settings(conf)`).
    /// `None`/`""` = Basis-`settings.ini` (dann zeigen `store` und
    /// `default_store` auf dieselbe Datei und werden beim `save()`
    /// zusammengeführt — wie zwei QSettings-Instanzen auf dieselbe Datei).
    pub fn open(profile: Option<&str>) -> Result<Settings> {
        let base = app_paths::base_path().to_path_buf();
        let paths = SettingsPaths {
            settings: app_paths::settings_file_in(&base, profile.unwrap_or("")),
            default_settings: app_paths::settings_file_in(&base, ""),
            placebo: base.join("placebo_render_params.ini"),
            base,
        };
        Settings::open_at(paths)
    }

    /// Öffnet mit expliziten Pfaden (Tests, portable Tools).
    pub fn open_at(paths: SettingsPaths) -> Result<Settings> {
        let mut settings = Settings {
            store: read_ini_or_empty(&paths.settings)?,
            default_store: read_ini_or_empty(&paths.default_settings)?,
            placebo: read_ini_or_empty(&paths.placebo)?,
            same_settings_file: paths.settings == paths.default_settings,
            paths,
            hidden_hosts: BTreeMap::new(),
            registered_hosts: BTreeMap::new(),
            nickname_registered_hosts: BTreeMap::new(),
            ps4s_registered: 0,
            manual_hosts: BTreeMap::new(),
            manual_hosts_id_next: 0,
            controller_mappings: BTreeMap::new(),
            profiles: Vec::new(),
            time_format: "yyyy-MM-dd HH:mm:ss t".to_string(),
        };

        // Bei settings.ini == Basis-INI (kein Profil) betreiben wir nur EINEN
        // logischen Store (das C++ nutzt zwei QSettings-Instanzen auf derselben
        // Datei, wo sich Änderungen gegenseitig überschreiben können).
        if settings.same_settings_file {
            settings.default_store = IniStore::new();
        }

        // ImportLegacySettings (Registry → INI): bewusst nicht portiert,
        // portable Modus ist Standard; die Legacy-Daten bleiben unangetastet.

        migrate_settings(&mut settings.store);
        migrate_video_profile(&mut settings.store);
        migrate_controller_mappings(&mut settings.store);
        settings.manual_hosts_id_next = 0;
        settings.store.set_value("version", Value::Int(SETTINGS_VERSION));
        settings.load_registered_hosts();
        settings.load_hidden_hosts();
        settings.load_manual_hosts();
        settings.load_controller_mappings();

        migrate_settings(settings.base_store_mut());
        migrate_video_profile(settings.base_store_mut());
        settings.base_store_mut().set_value("version", Value::Int(SETTINGS_VERSION));
        settings.load_profiles();

        initialize_placebo_settings(&mut settings.placebo);
        let profiles = settings.profiles.clone();
        settings.migrate_legacy_frame_mixer(Some(&profiles), false);

        Ok(settings)
    }

    /// Persistiert alle geänderten Stores (settings.ini, Basis-INI,
    /// placebo_render_params.ini) im QSettings-INI-Format.
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.paths.settings.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if self.same_settings_file {
            // Nur ein logischer Store (siehe open_at) — Basis-Keys liegen
            // bereits im settings-Store.
            std::fs::write(&self.paths.settings, self.store.to_ini_string())?;
        } else {
            std::fs::write(&self.paths.settings, self.store.to_ini_string())?;
            if let Some(parent) = self.paths.default_settings.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&self.paths.default_settings, self.default_store.to_ini_string())?;
        }
        if let Some(parent) = self.paths.placebo.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.paths.placebo, self.placebo.to_ini_string())?;
        Ok(())
    }

    /// Ändert die Settings und speichert danach (Speicher-Trigger wie
    /// QSettings-sync).
    pub fn update(&mut self, f: impl FnOnce(&mut Self)) -> Result<()> {
        f(self);
        self.save()
    }

    /// Pfade (für Logs/App-Anzeige).
    pub fn paths(&self) -> &SettingsPaths {
        &self.paths
    }

    /// Profil-INI im eigenen Basispfad (statt app_paths, damit Tests mit
    /// Temp-Dirs nie echte User-Daten berühren).
    fn profile_file(&self, profile: &str) -> PathBuf {
        app_paths::settings_file_in(&self.paths.base, profile)
    }

    /// Basis-Store (current_profile, profiles) — bei gleicher Datei der
    /// settings-Store selbst.
    fn base_store(&self) -> &IniStore {
        if self.same_settings_file { &self.store } else { &self.default_store }
    }

    fn base_store_mut(&mut self) -> &mut IniStore {
        if self.same_settings_file { &mut self.store } else { &mut self.default_store }
    }

    /// `GetTimeFormat()` — Qt-Formatstring der GUI.
    pub fn time_format(&self) -> &str {
        &self.time_format
    }

    // ------------------------------------------------------------------
    // System / Allgemeines
    // ------------------------------------------------------------------

    acc_int!(audio_video_disabled_raw, set_audio_video_disabled_raw, "settings/audio_video_disabled", 0);

    /// `GetAudioVideoDisabled()` / `SetAudioVideoDisabled()`
    pub fn audio_video_disabled(&self) -> DisableAudioVideo {
        match self.audio_video_disabled_raw() {
            1 => DisableAudioVideo::Audio,
            2 => DisableAudioVideo::Video,
            3 => DisableAudioVideo::AudioVideo,
            _ => DisableAudioVideo::None,
        }
    }

    pub fn set_audio_video_disabled(&mut self, disabled: DisableAudioVideo) {
        self.set_audio_video_disabled_raw(disabled as i64);
    }

    acc_bool!(discovery_enabled, set_discovery_enabled, "settings/auto_discovery", true);
    acc_bool!(remote_play_ask, set_remote_play_ask, "settings/remote_play_ask", true);
    acc_bool!(add_steam_shortcut_ask, set_add_steam_shortcut_ask, "settings/add_steam_shortcut_ask", true);
    acc_bool!(log_verbose, set_log_verbose, "settings/log_verbose", false);
    acc_bool!(log_sanitize, set_log_sanitize, "settings/log_sanitize", true);
    acc_bool!(vsync_enabled, set_vsync_enabled, "settings/vsync", false);
    acc_bool!(hide_cursor, set_hide_cursor, "settings/hide_cursor", true);
    acc_bool!(show_stream_stats, set_show_stream_stats, "settings/show_stream_stats", false);
    acc_bool!(show_vsr_badge, set_show_vsr_badge, "settings/show_vsr_badge", true);
    acc_bool!(streamer_mode, set_streamer_mode, "settings/streamer_mode", false);
    acc_bool!(buttons_by_position, set_buttons_by_position, "settings/buttons_by_pos", false);
    acc_bool!(allow_joystick_background_events, set_allow_joystick_background_events, "settings/allow_joystick_background_events", true);
    acc_bool!(start_mic_unmuted, set_start_mic_unmuted, "settings/start_mic_unmuted", false);
    acc_bool!(automatic_connect, set_automatic_connect, "settings/automatic_connect", false);
    acc_bool!(fullscreen_double_click_enabled, set_fullscreen_double_click_enabled, "settings/fullscreen_doubleclick", false);
    acc_bool!(idr_on_fec_failure_enabled, set_idr_on_fec_failure_enabled, "settings/idr_on_fec_failure", false);

    acc_int!(reorder_timeout_ms, set_reorder_timeout_ms, "settings/av_reorder_timeout_ms", 16);
    acc_bool!(nv_vsr_enabled, set_nv_vsr_enabled, "settings/nv_vsr", false);
    acc_int!(nv_vsr_scale, set_nv_vsr_scale, "settings/nv_vsr_scale", 0);
    acc_string!(nv_vsr_sdk_path, set_nv_vsr_sdk_path, "settings/nv_vsr_sdk_path");

    /// `GetLogLevelMask()`: alles außer VERBOSE, wenn nicht log_verbose.
    pub fn log_level_mask(&self) -> u32 {
        let mut mask = CHIAKI_LOG_ALL;
        if !self.log_verbose() {
            mask &= !CHIAKI_LOG_VERBOSE;
        }
        mask
    }

    acc_float!(haptic_override, set_haptic_override, "settings/haptic_override", 1.0);

    /// `GetGeometry()`
    pub fn geometry(&self) -> Rect {
        self.store
            .rect_or("settings/geometry")
            .map(|(x, y, w, h)| Rect { x, y, width: w, height: h })
            .unwrap_or_default()
    }

    pub fn set_geometry(&mut self, geometry: Rect) {
        self.store
            .set_value("settings/geometry", Value::Rect(geometry.x, geometry.y, geometry.width, geometry.height));
    }

    /// `GetStreamGeometry()`
    pub fn stream_geometry(&self) -> Rect {
        self.store
            .rect_or("settings/stream_geometry")
            .map(|(x, y, w, h)| Rect { x, y, width: w, height: h })
            .unwrap_or_default()
    }

    pub fn set_stream_geometry(&mut self, geometry: Rect) {
        self.store.set_value(
            "settings/stream_geometry",
            Value::Rect(geometry.x, geometry.y, geometry.width, geometry.height),
        );
    }

    /// `GetRumbleHapticsIntensity()`
    pub fn rumble_haptics_intensity(&self) -> RumbleHapticsIntensity {
        let s = self
            .store
            .string_or("settings/rumble_haptics_intensity", enum_name(RUMBLE_INTENSITIES, RumbleHapticsIntensity::Normal));
        enum_value(RUMBLE_INTENSITIES, &s, RumbleHapticsIntensity::Normal)
    }

    pub fn set_rumble_haptics_intensity(&mut self, intensity: RumbleHapticsIntensity) {
        self.store.set_value(
            "settings/rumble_haptics_intensity",
            Value::Str(enum_name(RUMBLE_INTENSITIES, intensity).to_string()),
        );
    }

    /// `GetZoomFactor()` (Default -1)
    pub fn zoom_factor(&self) -> f64 {
        self.store.float_or("settings/zoom_factor", -1.0)
    }

    /// `SetZoomFactor` — schreibt mit 2 Nachkommastellen (wie C++).
    pub fn set_zoom_factor(&mut self, factor: f64) {
        self.store
            .set_value("settings/zoom_factor", Value::Str(format!("{:.2}", factor)));
    }

    /// `GetPacketLossReportedMax()` — mit Legacy-Key-Fallback
    /// (`settings/packet_loss_max`), Default 0.05.
    pub fn packet_loss_reported_max(&self) -> f64 {
        if self.store.contains("settings/packet_loss_reported_max") {
            return self.store.float_or("settings/packet_loss_reported_max", 0.0);
        }
        if self.store.contains("settings/packet_loss_max") {
            return self.store.float_or("settings/packet_loss_max", 0.0);
        }
        0.05
    }

    pub fn set_packet_loss_reported_max(&mut self, reported_max: f64) {
        self.store.set_value(
            "settings/packet_loss_reported_max",
            Value::Str(format!("{:.2}", reported_max)),
        );
    }

    /// `GetWindowType()`
    pub fn window_type(&self) -> WindowType {
        let v = self
            .store
            .string_or("settings/window_type", enum_name(WINDOW_TYPES, WindowType::AdjustableResolution));
        enum_value(WINDOW_TYPES, &v, WindowType::AdjustableResolution)
    }

    pub fn set_window_type(&mut self, window_type: WindowType) {
        self.store
            .set_value("settings/window_type", Value::Str(enum_name(WINDOW_TYPES, window_type).to_string()));
    }

    acc_uint!(custom_resolution_width, set_custom_resolution_width, "settings/custom_resolution_width", 1920);
    // Key heißt im C++ `custom_resolution_length` (nicht `..._height`)
    acc_uint!(custom_resolution_height, set_custom_resolution_height, "settings/custom_resolution_length", 1080);

    /// `GetPlaceboPreset()`
    pub fn placebo_preset(&self) -> PlaceboPreset {
        let v = self
            .store
            .string_or("settings/placebo_preset", enum_name(PLACEBO_PRESETS, PlaceboPreset::HighQuality));
        enum_value(PLACEBO_PRESETS, &v, PlaceboPreset::HighQuality)
    }

    pub fn set_placebo_preset(&mut self, preset: PlaceboPreset) {
        self.store
            .set_value("settings/placebo_preset", Value::Str(enum_name(PLACEBO_PRESETS, preset).to_string()));
    }

    /// `GetRenderBackend()` (Windows: Default Vulkan)
    pub fn render_backend(&self) -> RenderBackend {
        let v = self
            .store
            .string_or("settings/render_backend", enum_name(RENDER_BACKENDS, RenderBackend::Vulkan));
        enum_value(RENDER_BACKENDS, &v, RenderBackend::Vulkan)
    }

    /// `SetRenderBackend` — schreibt nur bei Änderung (wie C++).
    pub fn set_render_backend(&mut self, backend: RenderBackend) {
        if self.render_backend() == backend {
            return;
        }
        self.store
            .set_value("settings/render_backend", Value::Str(enum_name(RENDER_BACKENDS, backend).to_string()));
    }

    // ------------------------------------------------------------------
    // Video
    // ------------------------------------------------------------------

    fn resolution_get(&self, key: &str, default: ResolutionPreset) -> ResolutionPreset {
        const RESOLUTIONS: &[(ResolutionPreset, &str)] = &[
            (ResolutionPreset::P360, "360p"),
            (ResolutionPreset::P540, "540p"),
            (ResolutionPreset::P720, "720p"),
            (ResolutionPreset::P1080, "1080p"),
        ];
        enum_value(RESOLUTIONS, &self.store.string_or(key, enum_name(RESOLUTIONS, default)), default)
    }

    fn resolution_set(&mut self, key: &str, value: ResolutionPreset) {
        const RESOLUTIONS: &[(ResolutionPreset, &str)] = &[
            (ResolutionPreset::P360, "360p"),
            (ResolutionPreset::P540, "540p"),
            (ResolutionPreset::P720, "720p"),
            (ResolutionPreset::P1080, "1080p"),
        ];
        self.store.set_value(key, Value::Str(enum_name(RESOLUTIONS, value).to_string()));
    }

    pub fn resolution_local_ps4(&self) -> ResolutionPreset {
        self.resolution_get("settings/resolution_local_ps4", ResolutionPreset::P720)
    }
    pub fn resolution_remote_ps4(&self) -> ResolutionPreset {
        self.resolution_get("settings/resolution_remote_ps4", ResolutionPreset::P720)
    }
    pub fn resolution_local_ps5(&self) -> ResolutionPreset {
        self.resolution_get("settings/resolution_local_ps5", ResolutionPreset::P1080)
    }
    pub fn resolution_remote_ps5(&self) -> ResolutionPreset {
        self.resolution_get("settings/resolution_remote_ps5", ResolutionPreset::P720)
    }
    pub fn set_resolution_local_ps4(&mut self, r: ResolutionPreset) {
        self.resolution_set("settings/resolution_local_ps4", r);
    }
    pub fn set_resolution_remote_ps4(&mut self, r: ResolutionPreset) {
        self.resolution_set("settings/resolution_remote_ps4", r);
    }
    pub fn set_resolution_local_ps5(&mut self, r: ResolutionPreset) {
        self.resolution_set("settings/resolution_local_ps5", r);
    }
    pub fn set_resolution_remote_ps5(&mut self, r: ResolutionPreset) {
        self.resolution_set("settings/resolution_remote_ps5", r);
    }

    fn fps_get(&self, key: &str) -> FpsPreset {
        const FPS_VALUES: &[(FpsPreset, &str)] = &[(FpsPreset::Fps30, "30"), (FpsPreset::Fps60, "60")];
        let v = self.store.int_or(key, 60);
        enum_value(FPS_VALUES, &v.to_string(), FpsPreset::Fps60)
    }

    fn fps_set(&mut self, key: &str, value: FpsPreset) {
        self.store.set_value(key, Value::Int(value as i64));
    }

    pub fn fps_local_ps4(&self) -> FpsPreset {
        self.fps_get("settings/fps_local_ps4")
    }
    pub fn fps_remote_ps4(&self) -> FpsPreset {
        self.fps_get("settings/fps_remote_ps4")
    }
    pub fn fps_local_ps5(&self) -> FpsPreset {
        self.fps_get("settings/fps_local_ps5")
    }
    pub fn fps_remote_ps5(&self) -> FpsPreset {
        self.fps_get("settings/fps_remote_ps5")
    }
    pub fn set_fps_local_ps4(&mut self, fps: FpsPreset) {
        self.fps_set("settings/fps_local_ps4", fps);
    }
    pub fn set_fps_remote_ps4(&mut self, fps: FpsPreset) {
        self.fps_set("settings/fps_remote_ps4", fps);
    }
    pub fn set_fps_local_ps5(&mut self, fps: FpsPreset) {
        self.fps_set("settings/fps_local_ps5", fps);
    }
    pub fn set_fps_remote_ps5(&mut self, fps: FpsPreset) {
        self.fps_set("settings/fps_remote_ps5", fps);
    }

    acc_uint!(bitrate_local_ps4, set_bitrate_local_ps4, "settings/bitrate_local_ps4", 0);
    acc_uint!(bitrate_remote_ps4, set_bitrate_remote_ps4, "settings/bitrate_remote_ps4", 0);
    acc_uint!(bitrate_local_ps5, set_bitrate_local_ps5, "settings/bitrate_local_ps5", 0);
    acc_uint!(bitrate_remote_ps5, set_bitrate_remote_ps5, "settings/bitrate_remote_ps5", 0);

    fn codec_get(&self, key: &str, default: Codec) -> Codec {
        const CODECS: &[(Codec, &str)] = &[
            (Codec::H264, "h264"),
            (Codec::H265, "h265"),
            (Codec::H265Hdr, "h265_hdr"),
        ];
        enum_value(CODECS, &self.store.string_or(key, enum_name(CODECS, default)), default)
    }

    /// `clampCodecForBackend`: OpenGL kann kein HDR-Codec.
    fn clamp_codec_for_backend(&self, codec: Codec) -> Codec {
        if self.render_backend() == RenderBackend::OpenGL && codec == Codec::H265Hdr {
            Codec::H265
        } else {
            codec
        }
    }

    pub fn codec_ps4(&self) -> Codec {
        self.codec_get("settings/codec_ps4", Codec::H264)
    }
    pub fn codec_local_ps5(&self) -> Codec {
        self.clamp_codec_for_backend(self.codec_get("settings/codec_local_ps5", Codec::H265))
    }
    pub fn codec_remote_ps5(&self) -> Codec {
        self.clamp_codec_for_backend(self.codec_get("settings/codec_remote_ps5", Codec::H265))
    }

    pub fn set_codec_ps4(&mut self, codec: Codec) {
        const CODECS: &[(Codec, &str)] = &[
            (Codec::H264, "h264"),
            (Codec::H265, "h265"),
            (Codec::H265Hdr, "h265_hdr"),
        ];
        self.store.set_value("settings/codec_ps4", Value::Str(enum_name(CODECS, codec).to_string()));
    }

    fn codec_set_ps5(&mut self, key: &str, codec: Codec) {
        const CODECS: &[(Codec, &str)] = &[
            (Codec::H264, "h264"),
            (Codec::H265, "h265"),
            (Codec::H265Hdr, "h265_hdr"),
        ];
        self.store.set_value(key, Value::Str(enum_name(CODECS, codec).to_string()));
    }

    pub fn set_codec_local_ps5(&mut self, codec: Codec) {
        self.codec_set_ps5("settings/codec_local_ps5", codec);
    }
    pub fn set_codec_remote_ps5(&mut self, codec: Codec) {
        self.codec_set_ps5("settings/codec_remote_ps5", codec);
    }

    acc_int!(display_target_contrast, set_display_target_contrast, "settings/display_target_contrast", 0);
    acc_int!(display_target_peak, set_display_target_peak, "settings/display_target_peak", 0);
    acc_int!(display_target_trc, set_display_target_trc, "settings/display_target_trc", 0);
    acc_int!(display_target_prim, set_display_target_prim, "settings/display_target_prim", 0);

    /// `GetDecoder()` — der Rust-Client hat keinen Pi-Decoder, daher
    /// entspricht der Default dem C++-Build ohne `CHIAKI_LIB_ENABLE_PI_DECODER`
    /// (Ffmpeg). Gespeichert werden beide Werte wie im C++.
    pub fn decoder(&self) -> Decoder {
        enum_value(DECODERS, &self.store.string_or("settings/decoder", enum_name(DECODERS, Decoder::Ffmpeg)), Decoder::Ffmpeg)
    }

    pub fn set_decoder(&mut self, decoder: Decoder) {
        self.store.set_value("settings/decoder", Value::Str(enum_name(DECODERS, decoder).to_string()));
    }

    acc_string!(hardware_decoder, set_hardware_decoder, "settings/hw_decoder");

    /// `GetHardwareDecoder()` mit Default "auto".
    pub fn hw_decoder(&self) -> String {
        self.store.string_or("settings/hw_decoder", "auto")
    }

    acc_bool!(use_zero_copy, set_use_zero_copy, "settings/use_zero_copy", true);
    acc_bool!(vulkan_deferred_swap, set_vulkan_deferred_swap, "settings/vulkan_deferred_swap", false);

    /// `GetVideoProfileLocalPS4()` u.a.
    pub fn video_profile_local_ps4(&self) -> ConnectVideoProfile {
        self.video_profile(
            self.resolution_local_ps4(),
            self.fps_local_ps4(),
            self.bitrate_local_ps4(),
            self.codec_ps4(),
        )
    }
    pub fn video_profile_remote_ps4(&self) -> ConnectVideoProfile {
        self.video_profile(
            self.resolution_remote_ps4(),
            self.fps_remote_ps4(),
            self.bitrate_remote_ps4(),
            self.codec_ps4(),
        )
    }
    pub fn video_profile_local_ps5(&self) -> ConnectVideoProfile {
        self.video_profile(
            self.resolution_local_ps5(),
            self.fps_local_ps5(),
            self.bitrate_local_ps5(),
            self.codec_local_ps5(),
        )
    }
    pub fn video_profile_remote_ps5(&self) -> ConnectVideoProfile {
        self.video_profile(
            self.resolution_remote_ps5(),
            self.fps_remote_ps5(),
            self.bitrate_remote_ps5(),
            self.codec_remote_ps5(),
        )
    }

    fn video_profile(
        &self,
        resolution: ResolutionPreset,
        fps: FpsPreset,
        bitrate: u64,
        codec: Codec,
    ) -> ConnectVideoProfile {
        let mut profile = connect_video_profile_preset(resolution, fps);
        if bitrate != 0 {
            profile.bitrate = bitrate as u32;
        }
        profile.codec = codec;
        profile
    }

    // ------------------------------------------------------------------
    // Audio
    // ------------------------------------------------------------------

    acc_int!(audio_volume, set_audio_volume, "settings/audio_volume", SDL_MIX_MAXVOLUME);

    /// `GetAudioBufferSizeDefault()` — 9600.
    pub fn audio_buffer_size_default(&self) -> u64 {
        AUDIO_BUFFER_SIZE_DEFAULT
    }

    /// `GetAudioBufferSizeRaw()` — 0 heißt "automatisch".
    pub fn audio_buffer_size_raw(&self) -> u64 {
        self.store.uint_or("settings/audio_buffer_size", 0)
    }

    /// `GetAudioBufferSize()` — roher Wert oder Default.
    pub fn audio_buffer_size(&self) -> u64 {
        let v = self.audio_buffer_size_raw();
        if v != 0 { v } else { AUDIO_BUFFER_SIZE_DEFAULT }
    }

    pub fn set_audio_buffer_size(&mut self, size: u64) {
        self.store.set_value("settings/audio_buffer_size", Value::UInt(size));
    }

    acc_string!(audio_out_device, set_audio_out_device, "settings/audio_out_device");
    acc_string!(audio_in_device, set_audio_in_device, "settings/audio_in_device");

    acc_uint!(wifi_dropped_notif, set_wifi_dropped_notif, "settings/wifi_dropped_notif_percent", 3);

    acc_bool!(port_guessing_enabled, set_port_guessing_enabled, "settings/port_guessing_enabled", false);

    pub fn port_guess_count(&self) -> i64 {
        self.store.int_or("settings/port_guessing_count", PORT_GUESS_COUNT_DEFAULT)
    }

    pub fn set_port_guess_count(&mut self, count: i64) {
        let count = count.max(0);
        self.store.set_value("settings/port_guessing_count", Value::Int(count));
    }

    pub fn port_guess_socket_count(&self) -> i64 {
        self.store.int_or("settings/port_guessing_socket_count", PORT_GUESS_SOCKS_DEFAULT)
    }

    pub fn set_port_guess_socket_count(&mut self, count: i64) {
        let count = count.max(0);
        self.store.set_value("settings/port_guessing_socket_count", Value::Int(count));
    }

    // Speex-Keys (im C++ hinter CHIAKI_GUI_ENABLE_SPEEX; Keys werden
    // hier immer unterstützt, die Nutzung entscheidet die App):
    acc_bool!(speech_processing_enabled, set_speech_processing_enabled, "settings/enable_speech_processing", false);
    acc_int!(noise_suppress_level, set_noise_suppress_level, "settings/noise_suppress_level", 6);
    acc_int!(echo_suppress_level, set_echo_suppress_level, "settings/echo_suppress_level", 30);

    // ------------------------------------------------------------------
    // Controls
    // ------------------------------------------------------------------

    acc_bool!(keyboard_enabled, set_keyboard_enabled, "settings/keyboard_enabled", true);
    acc_bool!(mouse_touch_enabled, set_mouse_touch_enabled, "settings/mouse_touch_enabled", true);
    acc_bool!(dpad_touch_enabled, set_dpad_touch_enabled, "settings/dpad_touch_enabled", true);

    /// `GetDpadTouchIncrement()` (u16)
    pub fn dpad_touch_increment(&self) -> u16 {
        self.store.uint_or("settings/dpad_touch_increment", 30) as u16
    }

    pub fn set_dpad_touch_increment(&mut self, increment: u16) {
        self.store.set_value("settings/dpad_touch_increment", Value::UInt(increment as u64));
    }

    acc_uint!(dpad_touch_shortcut1, set_dpad_touch_shortcut1, "settings/dpad_touch_shortcut1", 9);
    acc_uint!(dpad_touch_shortcut2, set_dpad_touch_shortcut2, "settings/dpad_touch_shortcut2", 10);
    acc_uint!(dpad_touch_shortcut3, set_dpad_touch_shortcut3, "settings/dpad_touch_shortcut3", 7);
    acc_uint!(dpad_touch_shortcut4, set_dpad_touch_shortcut4, "settings/dpad_touch_shortcut4", 0);

    acc_bool!(stream_menu_enabled, set_stream_menu_enabled, "settings/stream_menu_enabled", true);
    acc_uint!(stream_menu_shortcut1, set_stream_menu_shortcut1, "settings/stream_menu_shortcut1", 9);
    acc_uint!(stream_menu_shortcut2, set_stream_menu_shortcut2, "settings/stream_menu_shortcut2", 10);
    acc_uint!(stream_menu_shortcut3, set_stream_menu_shortcut3, "settings/stream_menu_shortcut3", 11);
    acc_uint!(stream_menu_shortcut4, set_stream_menu_shortcut4, "settings/stream_menu_shortcut4", 12);

    /// `SetControllerButtonMapping` — `key` ist ein Qt-KeySequence-Name
    /// (z.B. "Return", "PgUp", "]").
    pub fn set_controller_button_mapping(&mut self, button: u32, key: &str) {
        let name = button_key_name(button);
        self.store.set_value(&format!("keymap/{name}"), Value::Str(key.to_string()));
    }

    /// `GetControllerMapping()` — Defaults, dann Overlay aus `keymap/*`.
    pub fn controller_mapping(&self) -> Vec<(u32, String)> {
        default_keymap()
            .into_iter()
            .map(|(button, default_key)| {
                let name = button_key_name(button);
                let key = self.store.string_or(&format!("keymap/{name}"), default_key);
                (button, key)
            })
            .collect()
    }

    /// `GetControllerMappingForDecoding()` — invertiert (Key → Button).
    pub fn controller_mapping_for_decoding(&self) -> Vec<(String, u32)> {
        self.controller_mapping().into_iter().map(|(b, k)| (k, b)).collect()
    }

    /// `ClearKeyMapping()` — entfernt die keymap-Keys aller Default-Buttons.
    pub fn clear_key_mapping(&mut self) {
        for (button, _) in default_keymap() {
            let name = button_key_name(button);
            self.store.remove(&format!("keymap/{name}"));
        }
    }

    // ------------------------------------------------------------------
    // Disconnect / Suspend
    // ------------------------------------------------------------------

    pub fn disconnect_action(&self) -> DisconnectAction {
        let v = self
            .store
            .string_or("settings/disconnect_action", enum_name(DISCONNECT_ACTIONS, DisconnectAction::Ask));
        enum_value(DISCONNECT_ACTIONS, &v, DisconnectAction::Ask)
    }

    pub fn set_disconnect_action(&mut self, action: DisconnectAction) {
        self.store
            .set_value("settings/disconnect_action", Value::Str(enum_name(DISCONNECT_ACTIONS, action).to_string()));
    }

    pub fn suspend_action(&self) -> SuspendAction {
        let v = self
            .store
            .string_or("settings/suspend_action", enum_name(SUSPEND_ACTIONS, SuspendAction::Nothing));
        enum_value(SUSPEND_ACTIONS, &v, SuspendAction::Nothing)
    }

    pub fn set_suspend_action(&mut self, action: SuspendAction) {
        self.store
            .set_value("settings/suspend_action", Value::Str(enum_name(SUSPEND_ACTIONS, action).to_string()));
    }

    // ------------------------------------------------------------------
    // PSN (Speicherformat siehe psn.rs)
    // ------------------------------------------------------------------

    acc_string!(psn_auth_token, set_psn_auth_token, "settings/psn_auth_token");
    acc_string!(psn_refresh_token, set_psn_refresh_token, "settings/psn_refresh_token");
    acc_string!(psn_auth_token_expiry, set_psn_auth_token_expiry, "settings/psn_auth_token_expiry");
    acc_string!(psn_account_id, set_psn_account_id, "settings/psn_account_id");

    /// `settings/psn_account_id` (Base64-String) als 8 Account-Bytes
    /// (CHIAKI_PSN_ACCOUNT_ID_SIZE).
    pub fn psn_account_id_bytes(&self) -> Option<[u8; 8]> {
        crate::psn::account_id_to_bytes(&self.psn_account_id())
    }

    // ------------------------------------------------------------------
    // Auto-Connect-Host
    // ------------------------------------------------------------------

    /// `GetAutoConnectHost()` — leerer RegisteredHost, wenn die MAC nicht
    /// registriert ist.
    pub fn auto_connect_host(&self) -> RegisteredHost {
        let mac = self.store.byte_array("settings/auto_connect_mac");
        match mac.as_deref().and_then(HostMac::from_slice) {
            Some(mac) => self.registered_host(mac).cloned().unwrap_or_default(),
            None => RegisteredHost::default(),
        }
    }

    /// `SetAutoConnectHost` (6-Byte-MAC).
    pub fn set_auto_connect_host(&mut self, mac: &[u8; 6]) {
        self.store
            .set_value("settings/auto_connect_mac", Value::ByteArray(mac.to_vec()));
    }

    // ------------------------------------------------------------------
    // Hosts-Verwaltung (RegisteredListen wie QMap im C++)
    // ------------------------------------------------------------------

    /// `GetRegisteredHosts()` — aufsteigend sortiert nach MAC-Wert
    /// (QMap<HostMAC, …>-Ordnung).
    pub fn registered_hosts(&self) -> Vec<&RegisteredHost> {
        self.registered_hosts.values().collect()
    }

    pub fn registered_host(&self, mac: HostMac) -> Option<&RegisteredHost> {
        self.registered_hosts.get(&mac.value())
    }

    pub fn registered_host_registered(&self, mac: HostMac) -> bool {
        self.registered_hosts.contains_key(&mac.value())
    }

    pub fn add_registered_host(&mut self, host: RegisteredHost) {
        self.registered_hosts.insert(host.server_mac.value(), host.clone());
        self.nickname_registered_hosts
            .insert(host.server_nickname.clone(), host);
        self.save_registered_hosts();
    }

    pub fn remove_registered_host(&mut self, mac: HostMac) {
        if self.registered_hosts.remove(&mac.value()).is_none() {
            return;
        }
        self.rebuild_nicknames();
        self.save_registered_hosts();
    }

    /// `GetNicknameRegisteredHostRegistered()`
    pub fn nickname_registered(&self, nickname: &str) -> bool {
        self.nickname_registered_hosts.contains_key(nickname)
    }

    /// `GetNicknameRegisteredHost()`
    pub fn nickname_registered_host(&self, nickname: &str) -> Option<&RegisteredHost> {
        self.nickname_registered_hosts.get(nickname)
    }

    /// `GetPS4RegisteredHostsRegistered()`
    pub fn ps4s_registered(&self) -> usize {
        self.ps4s_registered
    }

    pub fn hidden_hosts(&self) -> Vec<&HiddenHost> {
        self.hidden_hosts.values().collect()
    }

    pub fn add_hidden_host(&mut self, host: HiddenHost) {
        self.hidden_hosts.insert(host.server_mac.value(), host);
        self.save_hidden_hosts();
    }

    pub fn remove_hidden_host(&mut self, mac: HostMac) {
        if self.hidden_hosts.remove(&mac.value()).is_none() {
            return;
        }
        self.save_hidden_hosts();
    }

    pub fn hidden_host_hidden(&self, mac: HostMac) -> bool {
        self.hidden_hosts.contains_key(&mac.value())
    }

    pub fn hidden_host(&self, mac: HostMac) -> Option<&HiddenHost> {
        self.hidden_hosts.get(&mac.value())
    }

    // ManualHosts

    pub fn manual_hosts(&self) -> Vec<&ManualHost> {
        self.manual_hosts.values().collect()
    }

    /// `SetManualHost` — liefert die ID (neue ID bei `id < 0`).
    pub fn set_manual_host(&mut self, host: ManualHost) -> i32 {
        let id = if host.id < 0 {
            let id = self.manual_hosts_id_next;
            self.manual_hosts_id_next += 1;
            id
        } else {
            host.id
        };
        let mut saved = host;
        saved.id = id;
        self.manual_hosts.insert(id, saved);
        self.save_manual_hosts();
        id
    }

    pub fn remove_manual_host(&mut self, id: i32) {
        self.manual_hosts.remove(&id);
        self.save_manual_hosts();
    }

    pub fn manual_host_exists(&self, id: i32) -> bool {
        self.manual_hosts.contains_key(&id)
    }

    pub fn manual_host(&self, id: i32) -> Option<&ManualHost> {
        self.manual_hosts.get(&id)
    }

    // Controller-Mappings

    pub fn controller_mappings(&self) -> &BTreeMap<String, String> {
        &self.controller_mappings
    }

    pub fn set_controller_mapping(&mut self, vidpid: String, mapping: String) {
        self.controller_mappings.insert(vidpid, mapping);
        self.save_controller_mappings();
    }

    pub fn remove_controller_mapping(&mut self, vidpid: &str) {
        self.controller_mappings.remove(vidpid);
        self.save_controller_mappings();
    }

    // ------------------------------------------------------------------
    // Profiles
    // ------------------------------------------------------------------

    pub fn profiles(&self) -> &[String] {
        &self.profiles
    }

    /// `GetCurrentProfile()` (aus der Basis-INI).
    pub fn current_profile(&self) -> String {
        self.base_store().string_or("settings/current_profile", "")
    }

    /// `SetCurrentProfile` — legt das Profil in der Profil-Liste an, wenn
    /// unbekannt.
    pub fn set_current_profile(&mut self, profile: String) {
        if !profile.is_empty() && !self.profiles.contains(&profile) {
            self.profiles.push(profile.clone());
            self.save_profiles();
        }
        self.base_store_mut()
            .set_value("settings/current_profile", Value::Str(profile));
    }

    /// `DeleteProfile` — überschreibt die Profil-INI mit leeren Host-Listen
    /// (wie C++; die Datei bleibt bestehen), entfernt das Profil aus der
    /// Liste und lädt die eigenen Listen neu.
    pub fn delete_profile(&mut self, profile: &str) -> Result<()> {
        let path = self.profile_file(profile);
        let mut pstore = read_ini_or_empty(&path)?;

        self.registered_hosts.clear();
        self.manual_hosts.clear();
        self.controller_mappings.clear();
        self.save_registered_hosts_to(&mut pstore);
        self.save_hidden_hosts_to(&mut pstore);
        self.save_manual_hosts_to(&mut pstore);
        self.save_controller_mappings_to(&mut pstore);
        pstore.remove("settings");
        std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
        std::fs::write(&path, pstore.to_ini_string())?;

        self.profiles.retain(|p| p != profile);
        self.save_profiles();
        // C++ lädt danach die eigenen Listen neu (die member-Maps waren
        // oben nur temporär geleert worden).
        self.load_registered_hosts();
        self.load_hidden_hosts();
        self.load_manual_hosts();
        self.load_controller_mappings();
        Ok(())
    }

    // ------------------------------------------------------------------
    // Placebo (placebo_render_params.ini, Sektion placebo_settings)
    // ------------------------------------------------------------------

    /// Key im settings-Store (nicht placebo!): `settings/placebo_frame_mixer`.
    pub fn placebo_frame_mixer(&self) -> PlaceboFrameMixer {
        let v = self
            .store
            .string_or("settings/placebo_frame_mixer", enum_name(PLACEBO_FRAME_MIXERS, PlaceboFrameMixer::None))
            .to_lowercase();
        enum_value(PLACEBO_FRAME_MIXERS, &v, PlaceboFrameMixer::None)
    }

    pub fn set_placebo_frame_mixer(&mut self, frame_mixer: PlaceboFrameMixer) {
        self.store.set_value(
            "settings/placebo_frame_mixer",
            Value::Str(enum_name(PLACEBO_FRAME_MIXERS, frame_mixer).to_string()),
        );
    }

    placebo_enum!(placebo_upscaler, set_placebo_upscaler, "placebo_settings/upscaler", PLACEBO_UPSCALERS, PlaceboUpscaler, PlaceboUpscaler::EwaLanczosSharp);
    placebo_enum!(placebo_plane_upscaler, set_placebo_plane_upscaler, "placebo_settings/plane_upscaler", PLACEBO_UPSCALERS, PlaceboUpscaler, PlaceboUpscaler::None);
    placebo_enum!(placebo_downscaler, set_placebo_downscaler, "placebo_settings/downscaler", PLACEBO_DOWNSCALERS, PlaceboDownscaler, PlaceboDownscaler::Hermite);
    placebo_enum!(placebo_plane_downscaler, set_placebo_plane_downscaler, "placebo_settings/plane_downscaler", PLACEBO_DOWNSCALERS, PlaceboDownscaler, PlaceboDownscaler::None);

    placebo_enum!(placebo_deinterlace_preset, set_placebo_deinterlace_preset, "placebo_settings/deinterlace_preset", PLACEBO_DEINTERLACE_PRESETS, PlaceboDeinterlacePreset, PlaceboDeinterlacePreset::Default);
    placebo_enum!(placebo_deinterlace_algorithm, set_placebo_deinterlace_algorithm, "placebo_settings/deinterlace_algo", PLACEBO_DEINTERLACE_ALGOS, PlaceboDeinterlaceAlgorithm, PlaceboDeinterlaceAlgorithm::Yadif);

    pub fn placebo_deinterlace_enabled(&self) -> bool {
        self.placebo.string_or("placebo_settings/deinterlace", "no").to_lowercase() == "yes"
    }
    pub fn set_placebo_deinterlace_enabled(&mut self, enabled: bool) {
        self.placebo
            .set_value("placebo_settings/deinterlace", Value::Str(if enabled { "yes" } else { "no" }.to_string()));
    }

    pub fn placebo_deinterlace_skip_spatial(&self) -> bool {
        self.placebo.string_or("placebo_settings/deinterlace_skip_spatial", "no").to_lowercase() == "yes"
    }
    pub fn set_placebo_deinterlace_skip_spatial(&mut self, skip: bool) {
        self.placebo.set_value(
            "placebo_settings/deinterlace_skip_spatial",
            Value::Str(if skip { "yes" } else { "no" }.to_string()),
        );
    }

    /// `GetPlaceboDeinterlacePresetValue()`
    pub fn placebo_deinterlace_preset_value(&self) -> String {
        self.placebo
            .string_or("placebo_settings/deinterlace_preset", enum_name(PLACEBO_DEINTERLACE_PRESETS, PlaceboDeinterlacePreset::Default))
    }

    /// `GetPlaceboDeinterlaceAlgorithmValue()`
    pub fn placebo_deinterlace_algorithm_value(&self) -> String {
        self.placebo
            .string_or("placebo_settings/deinterlace_algo", enum_name(PLACEBO_DEINTERLACE_ALGOS, PlaceboDeinterlaceAlgorithm::Yadif))
    }

    placebo_float_fixed!(placebo_antiringing_strength, set_placebo_antiringing_strength, "placebo_settings/antiringing_strength", 0.0, 2);

    pub fn placebo_deband_enabled(&self) -> bool {
        self.placebo.string_or("placebo_settings/deband", "yes") == "yes"
    }
    pub fn set_placebo_deband_enabled(&mut self, enabled: bool) {
        self.placebo
            .set_value("placebo_settings/deband", Value::Str(if enabled { "yes" } else { "no" }.to_string()));
    }

    placebo_preset_with_empty!(placebo_deband_preset, set_placebo_deband_preset, "placebo_settings/deband_preset", PLACEBO_DEBAND_PRESETS, PlaceboDebandPreset, PlaceboDebandPreset::None, PlaceboDebandPreset::None);
    placebo_int!(placebo_deband_iterations, set_placebo_deband_iterations, "placebo_settings/deband_iterations", 1);
    placebo_float_fixed!(placebo_deband_threshold, set_placebo_deband_threshold, "placebo_settings/deband_threshold", 3.0, 1);
    placebo_float_fixed!(placebo_deband_radius, set_placebo_deband_radius, "placebo_settings/deband_radius", 16.0, 1);
    placebo_float_fixed!(placebo_deband_grain, set_placebo_deband_grain, "placebo_settings/deband_grain", 4.0, 1);

    pub fn placebo_sigmoid_enabled(&self) -> bool {
        self.placebo.string_or("placebo_settings/sigmoid", "yes") == "yes"
    }
    pub fn set_placebo_sigmoid_enabled(&mut self, enabled: bool) {
        self.placebo
            .set_value("placebo_settings/sigmoid", Value::Str(if enabled { "yes" } else { "no" }.to_string()));
    }

    placebo_preset_with_empty!(placebo_sigmoid_preset, set_placebo_sigmoid_preset, "placebo_settings/sigmoid_preset", PLACEBO_SIGMOID_PRESETS, PlaceboSigmoidPreset, PlaceboSigmoidPreset::None, PlaceboSigmoidPreset::None);
    placebo_float_fixed!(placebo_sigmoid_center, set_placebo_sigmoid_center, "placebo_settings/sigmoid_center", 0.75, 2);
    placebo_float_fixed!(placebo_sigmoid_slope, set_placebo_sigmoid_slope, "placebo_settings/sigmoid_slope", 6.5, 1);

    pub fn placebo_color_adjustment_enabled(&self) -> bool {
        self.placebo.string_or("placebo_settings/color_adjustment", "yes") == "yes"
    }
    pub fn set_placebo_color_adjustment_enabled(&mut self, enabled: bool) {
        self.placebo.set_value(
            "placebo_settings/color_adjustment",
            Value::Str(if enabled { "yes" } else { "no" }.to_string()),
        );
    }

    placebo_preset_with_empty!(placebo_color_adjustment_preset, set_placebo_color_adjustment_preset, "placebo_settings/color_adjustment_preset", PLACEBO_COLOR_ADJUSTMENT_PRESETS, PlaceboColorAdjustmentPreset, PlaceboColorAdjustmentPreset::None, PlaceboColorAdjustmentPreset::None);
    placebo_float_fixed!(placebo_color_adjustment_brightness, set_placebo_color_adjustment_brightness, "placebo_settings/brightness", 0.0, 2);
    placebo_float_fixed!(placebo_color_adjustment_contrast, set_placebo_color_adjustment_contrast, "placebo_settings/contrast", 1.0, 1);
    placebo_float_fixed!(placebo_color_adjustment_saturation, set_placebo_color_adjustment_saturation, "placebo_settings/saturation", 1.0, 2);
    placebo_float_fixed!(placebo_color_adjustment_hue, set_placebo_color_adjustment_hue, "placebo_settings/hue", 0.0, 2);
    placebo_float_fixed!(placebo_color_adjustment_gamma, set_placebo_color_adjustment_gamma, "placebo_settings/gamma", 1.0, 1);
    placebo_float_fixed!(placebo_color_adjustment_temperature, set_placebo_color_adjustment_temperature, "placebo_settings/temperature", 0.0, 3);

    pub fn placebo_peak_detection_enabled(&self) -> bool {
        self.placebo.string_or("placebo_settings/peak_detect", "yes") == "yes"
    }
    pub fn set_placebo_peak_detection_enabled(&mut self, enabled: bool) {
        self.placebo
            .set_value("placebo_settings/peak_detect", Value::Str(if enabled { "yes" } else { "no" }.to_string()));
    }

    placebo_preset_with_empty!(placebo_peak_detection_preset, set_placebo_peak_detection_preset, "placebo_settings/peak_detect_preset", PLACEBO_PEAK_DETECTION_PRESETS, PlaceboPeakDetectionPreset, PlaceboPeakDetectionPreset::None, PlaceboPeakDetectionPreset::None);
    placebo_float_fixed!(placebo_peak_smoothing_period, set_placebo_peak_smoothing_period, "placebo_settings/peak_smoothing_period", 20.0, 1);
    placebo_float_fixed!(placebo_scene_threshold_low, set_placebo_scene_threshold_low, "placebo_settings/scene_threshold_low", 1.0, 1);
    placebo_float_fixed!(placebo_scene_threshold_high, set_placebo_scene_threshold_high, "placebo_settings/scene_threshold_high", 3.0, 1);
    placebo_float_fixed!(placebo_peak_percentile, set_placebo_peak_percentile, "placebo_settings/peak_percentile", 100.0, 3);
    placebo_float_fixed!(placebo_black_cutoff, set_placebo_black_cutoff, "placebo_settings/black_cutoff", 1.0, 1);

    pub fn placebo_allow_delayed_peak(&self) -> bool {
        self.placebo.string_or("placebo_settings/allow_delayed_peak", "no") == "yes"
    }
    pub fn set_placebo_allow_delayed_peak(&mut self, allowed: bool) {
        self.placebo.set_value(
            "placebo_settings/allow_delayed_peak",
            Value::Str(if allowed { "yes" } else { "no" }.to_string()),
        );
    }

    pub fn placebo_color_mapping_enabled(&self) -> bool {
        self.placebo.string_or("placebo_settings/color_map", "yes") == "yes"
    }
    pub fn set_placebo_color_mapping_enabled(&mut self, enabled: bool) {
        self.placebo
            .set_value("placebo_settings/color_map", Value::Str(if enabled { "yes" } else { "no" }.to_string()));
    }

    placebo_preset_with_empty!(placebo_color_mapping_preset, set_placebo_color_mapping_preset, "placebo_settings/color_map_preset", PLACEBO_COLOR_MAPPING_PRESETS, PlaceboColorMappingPreset, PlaceboColorMappingPreset::None, PlaceboColorMappingPreset::None);

    placebo_enum!(placebo_gamut_mapping_function, set_placebo_gamut_mapping_function, "placebo_settings/gamut_mapping", PLACEBO_GAMUT_MAPPINGS, PlaceboGamutMappingFunction, PlaceboGamutMappingFunction::Perceptual);
    placebo_float_fixed!(placebo_perceptual_deadzone, set_placebo_perceptual_deadzone, "placebo_settings/perceptual_deadzone", 0.30, 2);
    placebo_float_fixed!(placebo_perceptual_strength, set_placebo_perceptual_strength, "placebo_settings/perceptual_strength", 0.80, 2);
    placebo_float_fixed!(placebo_colorimetric_gamma, set_placebo_colorimetric_gamma, "placebo_settings/colorimetric_gamma", 1.80, 2);
    placebo_float_fixed!(placebo_softclip_knee, set_placebo_softclip_knee, "placebo_settings/softclip_knee", 0.70, 2);
    placebo_float_fixed!(placebo_softclip_desat, set_placebo_softclip_desat, "placebo_settings/softclip_desat", 0.35, 2);
    placebo_int!(placebo_lut3d_size_i, set_placebo_lut3d_size_i, "placebo_settings/lut3d_size_I", 48);
    placebo_int!(placebo_lut3d_size_c, set_placebo_lut3d_size_c, "placebo_settings/lut3d_size_C", 32);
    placebo_int!(placebo_lut3d_size_h, set_placebo_lut3d_size_h, "placebo_settings/lut3d_size_h", 256);

    pub fn placebo_lut3d_tricubic_enabled(&self) -> bool {
        self.placebo.string_or("placebo_settings/lut3d_tricubic", "no") == "yes"
    }
    pub fn set_placebo_lut3d_tricubic_enabled(&mut self, enabled: bool) {
        self.placebo.set_value(
            "placebo_settings/lut3d_tricubic",
            Value::Str(if enabled { "yes" } else { "no" }.to_string()),
        );
    }

    pub fn placebo_gamut_expansion_enabled(&self) -> bool {
        self.placebo.string_or("placebo_settings/gamut_expansion", "no") == "yes"
    }
    pub fn set_placebo_gamut_expansion_enabled(&mut self, enabled: bool) {
        self.placebo.set_value(
            "placebo_settings/gamut_expansion",
            Value::Str(if enabled { "yes" } else { "no" }.to_string()),
        );
    }

    placebo_enum!(placebo_tone_mapping_function, set_placebo_tone_mapping_function, "placebo_settings/tone_mapping", PLACEBO_TONE_MAPPINGS, PlaceboToneMappingFunction, PlaceboToneMappingFunction::Spline);
    placebo_float_fixed!(placebo_knee_adaptation, set_placebo_knee_adaptation, "placebo_settings/knee_adaptation", 0.4, 2);
    placebo_float_fixed!(placebo_knee_minimum, set_placebo_knee_minimum, "placebo_settings/knee_minimum", 0.1, 2);
    placebo_float_fixed!(placebo_knee_maximum, set_placebo_knee_maximum, "placebo_settings/knee_maximum", 0.8, 2);
    placebo_float_fixed!(placebo_knee_default, set_placebo_knee_default, "placebo_settings/knee_default", 0.4, 2);
    placebo_float_fixed!(placebo_knee_offset, set_placebo_knee_offset, "placebo_settings/knee_offset", 1.0, 2);
    placebo_float_fixed!(placebo_slope_tuning, set_placebo_slope_tuning, "placebo_settings/slope_tuning", 1.5, 1);
    placebo_float_fixed!(placebo_slope_offset, set_placebo_slope_offset, "placebo_settings/slope_offset", 0.2, 2);
    placebo_float_fixed!(placebo_spline_contrast, set_placebo_spline_contrast, "placebo_settings/spline_contrast", 0.5, 2);
    placebo_float_fixed!(placebo_reinhard_contrast, set_placebo_reinhard_contrast, "placebo_settings/reinhard_contrast", 0.5, 2);
    placebo_float_fixed!(placebo_linear_knee, set_placebo_linear_knee, "placebo_settings/linear_knee", 0.3, 2);
    placebo_float_fixed!(placebo_exposure, set_placebo_exposure, "placebo_settings/exposure", 1.0, 1);

    pub fn placebo_inverse_tone_mapping_enabled(&self) -> bool {
        self.placebo.string_or("placebo_settings/inverse_tone_mapping", "no") == "yes"
    }
    pub fn set_placebo_inverse_tone_mapping_enabled(&mut self, enabled: bool) {
        self.placebo.set_value(
            "placebo_settings/inverse_tone_mapping",
            Value::Str(if enabled { "yes" } else { "no" }.to_string()),
        );
    }

    placebo_enum!(placebo_tone_mapping_metadata, set_placebo_tone_mapping_metadata, "placebo_settings/tone_map_metadata", PLACEBO_TONE_METADATA, PlaceboToneMappingMetadata, PlaceboToneMappingMetadata::Any);
    placebo_int!(placebo_tone_lut_size, set_placebo_tone_lut_size, "placebo_settings/tone_lut_size", 256);
    placebo_float_fixed!(placebo_contrast_recovery, set_placebo_contrast_recovery, "placebo_settings/contrast_recovery", 0.0, 2);
    placebo_float_fixed!(placebo_contrast_smoothness, set_placebo_contrast_smoothness, "placebo_settings/contrast_smoothness", 3.5, 1);

    /// `GetPlaceboValues()` — alle `placebo_settings/*`-Werte außer
    /// `version` (für den libplacebo-Parameterbaum).
    pub fn placebo_values(&self) -> BTreeMap<String, String> {
        let mut map = BTreeMap::new();
        for (k, raw) in self.placebo.entries() {
            if let Some(sub) = k.strip_prefix("placebo_settings/") {
                if sub == "version" {
                    continue;
                }
                map.insert(sub.to_string(), crate::ini::ini_unescape_string(raw));
            }
        }
        map
    }

    // ------------------------------------------------------------------
    // Export / Import
    // ------------------------------------------------------------------

    /// `ExportSettings` — schreibt Hosts, Mappings und alle Settings-Keys
    /// in eine portable INI; `hw_decoder` wird auf "auto" gesetzt und
    /// `settings/this_profile` auf das aktuelle Profil.
    pub fn export_settings(&self, filepath: &Path) -> Result<()> {
        let filepath = with_ini_suffix(filepath);
        let mut out = IniStore::new();
        self.save_registered_hosts_to(&mut out);
        self.save_hidden_hosts_to(&mut out);
        self.save_manual_hosts_to(&mut out);
        self.save_controller_mappings_to(&mut out);
        for (k, raw) in self.store.entries() {
            out.set_raw_value(k, raw.clone());
        }
        out.set_value("settings/hw_decoder", Value::Str("auto".to_string()));
        out.set_value("settings/this_profile", Value::Str(self.current_profile()));
        std::fs::create_dir_all(filepath.parent().unwrap_or(Path::new(".")))?;
        std::fs::write(filepath, out.to_ini_string())?;
        Ok(())
    }

    /// `ImportSettings` — siehe C++: ohne `this_profile` wird der eigene
    /// Store ersetzt, sonst landet alles in der Ziel-Profil-Datei.
    pub fn import_settings(&mut self, filepath: &Path) -> Result<()> {
        let src = read_ini_or_empty(filepath)?;
        self.registered_hosts.clear();
        self.nickname_registered_hosts.clear();
        self.ps4s_registered = 0;
        self.load_registered_hosts_from(&src);
        self.load_hidden_hosts_from(&src);
        self.load_manual_hosts_from(&src);
        self.load_controller_mappings_from(&src);

        let profile = src.string_or("settings/this_profile", "");
        if profile.is_empty() {
            self.store = IniStore::new();
            let hosts: Vec<_> = self.registered_hosts.values().cloned().collect();
            Self::save_registered_hosts_of(&hosts, &mut self.store);
            let hosts: Vec<_> = self.hidden_hosts.values().cloned().collect();
            Self::save_hidden_hosts_of(&hosts, &mut self.store);
            let hosts: Vec<_> = self.manual_hosts.values().cloned().collect();
            Self::save_manual_hosts_of(&hosts, &mut self.store);
            let mappings = self.controller_mappings.clone();
            Self::save_controller_mappings_of(&mappings, &mut self.store);
            for (k, raw) in src.entries() {
                self.store.set_raw_value(k, raw.clone());
            }
            self.migrate_legacy_frame_mixer(None, true);
            self.set_current_profile(profile);
        } else {
            let path = self.profile_file(&profile);
            let mut pstore = IniStore::new();
            self.save_registered_hosts_to(&mut pstore);
            self.save_hidden_hosts_to(&mut pstore);
            self.save_manual_hosts_to(&mut pstore);
            self.save_controller_mappings_to(&mut pstore);
            for (k, raw) in src.entries() {
                pstore.set_raw_value(k, raw.clone());
            }
            std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
            std::fs::write(&path, pstore.to_ini_string())?;
            self.set_current_profile(profile);
        }
        Ok(())
    }

    /// `ExportPlaceboSettings`
    pub fn export_placebo_settings(&self, filepath: &Path) -> Result<()> {
        let filepath = with_ini_suffix(filepath);
        let mut out = IniStore::new();
        for (k, raw) in self.placebo.entries() {
            out.set_raw_value(k, raw.clone());
        }
        std::fs::create_dir_all(filepath.parent().unwrap_or(Path::new(".")))?;
        std::fs::write(filepath, out.to_ini_string())?;
        Ok(())
    }

    /// `ImportPlaceboSettings`
    pub fn import_placebo_settings(&mut self, filepath: &Path) -> Result<()> {
        let src = read_ini_or_empty(filepath)?;
        self.placebo = IniStore::new();
        for (k, raw) in src.entries() {
            self.placebo.set_raw_value(k, raw.clone());
        }
        self.migrate_legacy_frame_mixer(None, true);
        Ok(())
    }

    // ------------------------------------------------------------------
    // Intern: Laden/Speichern der Listen
    // ------------------------------------------------------------------

    fn load_registered_hosts(&mut self) {
        self.registered_hosts.clear();
        self.nickname_registered_hosts.clear();
        self.ps4s_registered = 0;
        let store = self.store.clone();
        self.load_registered_hosts_from(&store);
    }

    fn load_registered_hosts_from(&mut self, src: &IniStore) {
        let size = src.int_or("registered_hosts/size", 0).max(0) as u32;
        for n in 1..=size {
            let prefix = format!("registered_hosts/{n}");
            let host = RegisteredHost::load_from(src, &prefix);
            if !host.target.is_ps5() {
                self.ps4s_registered += 1;
            }
            self.nickname_registered_hosts
                .insert(host.server_nickname.clone(), host.clone());
            self.registered_hosts.insert(host.server_mac.value(), host);
        }
    }

    fn save_registered_hosts(&mut self) {
        // Kopie nötig: write_array braucht &mut self.store, Iteration über
        // self.registered_hosts ist &-Borrow.
        let hosts: Vec<_> = self.registered_hosts.values().cloned().collect();
        Self::save_registered_hosts_of(&hosts, &mut self.store);
    }

    fn save_registered_hosts_to(&self, target: &mut IniStore) {
        let hosts: Vec<_> = self.registered_hosts.values().cloned().collect();
        Self::save_registered_hosts_of(&hosts, target);
    }

    fn save_registered_hosts_of(hosts: &[RegisteredHost], target: &mut IniStore) {
        target.write_array("registered_hosts", hosts.iter().map(|h| h.save_to_items()));
    }

    fn load_hidden_hosts(&mut self) {
        self.hidden_hosts.clear();
        let store = self.store.clone();
        self.load_hidden_hosts_from(&store);
    }

    fn load_hidden_hosts_from(&mut self, src: &IniStore) {
        let size = src.int_or("hidden_hosts/size", 0).max(0) as u32;
        for n in 1..=size {
            let prefix = format!("hidden_hosts/{n}");
            let host = HiddenHost::load_from(src, &prefix);
            self.hidden_hosts.insert(host.server_mac.value(), host);
        }
    }

    fn save_hidden_hosts(&mut self) {
        let hosts: Vec<_> = self.hidden_hosts.values().cloned().collect();
        Self::save_hidden_hosts_of(&hosts, &mut self.store);
    }

    fn save_hidden_hosts_to(&self, target: &mut IniStore) {
        let hosts: Vec<_> = self.hidden_hosts.values().cloned().collect();
        Self::save_hidden_hosts_of(&hosts, target);
    }

    fn save_hidden_hosts_of(hosts: &[HiddenHost], target: &mut IniStore) {
        target.write_array(
            "hidden_hosts",
            hosts.iter().map(|h| {
                vec![
                    ("server_nickname".to_string(), Value::Str(h.server_nickname.clone())),
                    ("server_mac".to_string(), Value::ByteArray(h.server_mac.mac().to_vec())),
                ]
            }),
        );
    }

    fn load_manual_hosts(&mut self) {
        self.manual_hosts.clear();
        let store = self.store.clone();
        self.load_manual_hosts_from(&store);
    }

    fn load_manual_hosts_from(&mut self, src: &IniStore) {
        let size = src.int_or("manual_hosts/size", 0).max(0) as u32;
        for n in 1..=size {
            let prefix = format!("manual_hosts/{n}");
            let host = ManualHost::load_from(src, &prefix);
            if host.id < 0 {
                continue;
            }
            if self.manual_hosts_id_next <= host.id {
                self.manual_hosts_id_next = host.id + 1;
            }
            self.manual_hosts.insert(host.id, host);
        }
    }

    fn save_manual_hosts(&mut self) {
        let hosts: Vec<_> = self.manual_hosts.values().cloned().collect();
        Self::save_manual_hosts_of(&hosts, &mut self.store);
    }

    fn save_manual_hosts_to(&self, target: &mut IniStore) {
        let hosts: Vec<_> = self.manual_hosts.values().cloned().collect();
        Self::save_manual_hosts_of(&hosts, target);
    }

    fn save_manual_hosts_of(hosts: &[ManualHost], target: &mut IniStore) {
        target.write_array(
            "manual_hosts",
            hosts.iter().map(|h| {
                vec![
                    ("id".to_string(), Value::Int(h.id as i64)),
                    ("host".to_string(), Value::Str(h.host.clone())),
                    ("registered".to_string(), Value::Bool(h.registered)),
                    ("registered_mac".to_string(), Value::ByteArray(h.registered_mac.mac().to_vec())),
                ]
            }),
        );
    }

    fn load_controller_mappings(&mut self) {
        self.controller_mappings.clear();
        let store = self.store.clone();
        self.load_controller_mappings_from(&store);
    }

    fn load_controller_mappings_from(&mut self, src: &IniStore) {
        let size = src.int_or("controller_mappings/size", 0).max(0) as u32;
        for n in 1..=size {
            let p = format!("controller_mappings/{n}");
            let vidpid = src.string_or(&format!("{p}/vidpid"), "");
            let mapping = src.string_or(&format!("{p}/controller_mapping"), "");
            self.controller_mappings.insert(vidpid, mapping);
        }
    }

    fn save_controller_mappings(&mut self) {
        let mappings = self.controller_mappings.clone();
        Self::save_controller_mappings_of(&mappings, &mut self.store);
    }

    fn save_controller_mappings_to(&self, target: &mut IniStore) {
        let mappings = self.controller_mappings.clone();
        Self::save_controller_mappings_of(&mappings, target);
    }

    fn save_controller_mappings_of(mappings: &BTreeMap<String, String>, target: &mut IniStore) {
        target.write_array(
            "controller_mappings",
            mappings.iter().map(|(vidpid, mapping)| {
                vec![
                    ("vidpid".to_string(), Value::Str(vidpid.clone())),
                    ("controller_mapping".to_string(), Value::Str(mapping.clone())),
                ]
            }),
        );
    }

    fn load_profiles(&mut self) {
        self.profiles.clear();
        let base = self.base_store().clone();
        let size = base.int_or("profiles/size", 0).max(0) as u32;
        for n in 1..=size {
            let name = base.string_or(&format!("profiles/{n}/settings/profile_name"), "");
            self.profiles.push(name);
        }
    }

    fn save_profiles(&mut self) {
        let profiles = self.profiles.clone();
        let store = self.base_store_mut();
        store.write_array(
            "profiles",
            profiles
                .iter()
                .map(|p| vec![("settings/profile_name".to_string(), Value::Str(p.clone()))]),
        );
    }

    fn rebuild_nicknames(&mut self) {
        self.nickname_registered_hosts.clear();
        for host in self.registered_hosts.values() {
            self.nickname_registered_hosts
                .insert(host.server_nickname.clone(), host.clone());
        }
    }

    /// `MigrateLegacyFrameMixerSetting` — portiert den alten
    /// `placebo_settings/frame_mixer` nach `settings/placebo_frame_mixer`
    /// (auch in alle Profil-Dateien, wie im C++-Konstruktor).
    fn migrate_legacy_frame_mixer(&mut self, profiles: Option<&[String]>, allow_overwrite: bool) {
        if self.store.contains("settings/placebo_frame_mixer") && !allow_overwrite {
            return;
        }
        if !self.placebo.contains("placebo_settings/frame_mixer") {
            return;
        }
        let legacy = self.placebo.string_or("placebo_settings/frame_mixer", "").trim().to_lowercase();
        if legacy.is_empty() {
            return;
        }
        let Some(mapped) = PLACEBO_FRAME_MIXERS.iter().find(|(_, k)| *k == legacy).map(|(_, k)| *k) else {
            return;
        };

        if allow_overwrite || !self.store.contains("settings/placebo_frame_mixer") {
            self.store
                .set_value("settings/placebo_frame_mixer", Value::Str(mapped.to_string()));
        }
        if let Some(profiles) = profiles {
            for profile in profiles {
                let path = self.profile_file(profile);
                let Ok(mut pstore) = std::fs::read_to_string(&path).map(|t| IniStore::parse(&t)) else {
                    continue;
                };
                if allow_overwrite || !pstore.contains("settings/placebo_frame_mixer") {
                    pstore.set_value("settings/placebo_frame_mixer", Value::Str(mapped.to_string()));
                    let _ = std::fs::write(&path, pstore.to_ini_string());
                }
            }
        }
        self.placebo.remove("placebo_settings/frame_mixer");
    }
}

fn with_ini_suffix(filepath: &Path) -> PathBuf {
    // "append .ini if not already added" (C++ QFileInfo::suffix)
    match filepath.extension() {
        Some(_) => filepath.to_path_buf(),
        None => {
            let mut p = filepath.as_os_str().to_os_string();
            p.push(".ini");
            PathBuf::from(p)
        }
    }
}

fn read_ini_or_empty(path: &Path) -> Result<IniStore> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(IniStore::parse(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(IniStore::new()),
        Err(e) => Err(Error::Io(e)),
    }
}

// Statische Preset-Tabellen mit ""-Semantik (C++: QMap mit ""-Wert).
static PLACEBO_DEBAND_PRESETS: &[(PlaceboDebandPreset, &str)] =
    &[(PlaceboDebandPreset::None, ""), (PlaceboDebandPreset::Default, "default")];
static PLACEBO_SIGMOID_PRESETS: &[(PlaceboSigmoidPreset, &str)] =
    &[(PlaceboSigmoidPreset::None, ""), (PlaceboSigmoidPreset::Default, "default")];
static PLACEBO_COLOR_ADJUSTMENT_PRESETS: &[(PlaceboColorAdjustmentPreset, &str)] =
    &[(PlaceboColorAdjustmentPreset::None, ""), (PlaceboColorAdjustmentPreset::Neutral, "neutral")];
static PLACEBO_PEAK_DETECTION_PRESETS: &[(PlaceboPeakDetectionPreset, &str)] = &[
    (PlaceboPeakDetectionPreset::None, ""),
    (PlaceboPeakDetectionPreset::Default, "default"),
    (PlaceboPeakDetectionPreset::HighQuality, "high_quality"),
];
static PLACEBO_COLOR_MAPPING_PRESETS: &[(PlaceboColorMappingPreset, &str)] = &[
    (PlaceboColorMappingPreset::None, ""),
    (PlaceboColorMappingPreset::Default, "default"),
    (PlaceboColorMappingPreset::HighQuality, "high_quality"),
];

// ---------------------------------------------------------------------------
// Migrationen (exakt aus settings.cpp)
// ---------------------------------------------------------------------------

/// `MigrateSettings` + Versionsprüfung.
fn migrate_settings(store: &mut IniStore) {
    let version_prev = store.int_or("version", 0);
    if version_prev < 1 {
        return;
    }
    if version_prev > SETTINGS_VERSION {
        tracing::error!(
            "Settings version {} is higher than application one ({})",
            version_prev,
            SETTINGS_VERSION
        );
        return;
    }
    let mut version_prev = version_prev;
    while version_prev < SETTINGS_VERSION {
        version_prev += 1;
        if version_prev == 2 {
            tracing::info!("Migrating settings to 2");
            migrate_settings_to_2(store);
        }
    }
}

/// `MigrateSettingsTo2` — benennt `ps4_nickname`/`ps4_mac` um und setzt
/// `target` auf `CHIAKI_TARGET_PS4_10` (1000).
fn migrate_settings_to_2(store: &mut IniStore) {
    let hosts = store.read_array("registered_hosts");
    store.remove("registered_hosts");
    let rewritten: Vec<Vec<(String, Value)>> = hosts
        .into_iter()
        .map(|host| {
            let mut out = vec![("target".to_string(), Value::Int(Target::Ps4Ten.as_i32() as i64))];
            for (k, v) in host {
                let k = match k.as_str() {
                    "ps4_nickname" => "server_nickname".to_string(),
                    "ps4_mac" => "server_mac".to_string(),
                    _ => k,
                };
                out.push((k, v));
            }
            out
        })
        .collect();
    store.write_array("registered_hosts", rewritten);

    let hw_decoder = store.string_or("settings/hw_decode_engine", "");
    store.remove("settings/hw_decode_engine");
    if hw_decoder != "none" {
        store.set_value("settings/hw_decoder", Value::Str(hw_decoder));
    }
}

/// `MigrateVideoProfile` — alte generische Keys werden zu
/// `*_local_ps5`-Keys.
fn migrate_video_profile(store: &mut IniStore) {
    for (old, new) in [
        ("settings/resolution", "settings/resolution_local_ps5"),
        ("settings/fps", "settings/fps_local_ps5"),
        ("settings/codec", "settings/codec_local_ps5"),
        ("settings/bitrate", "settings/bitrate_local_ps5"),
    ] {
        if store.contains(old) {
            let v = store.get_raw(old).map(|s| s.to_string());
            if let Some(raw) = v {
                store.set_raw_value(new, raw);
            }
            store.remove(old);
        }
    }
}

/// `MigrateControllerMappings` — `guid` → `vidpid` und dezimale
/// vid/pid-Angaben zu Hex ("0x%04x:0x%04x", lowercase wie QString::arg).
fn migrate_controller_mappings(store: &mut IniStore) {
    let mappings = store.read_array("controller_mappings");
    store.remove("controller_mappings");
    let rewritten: Vec<Vec<(String, Value)>> = mappings
        .into_iter()
        .map(|mapping| {
            let mut vidpid = String::new();
            let mut controller_mapping = String::new();
            for (k, v) in mapping {
                let v_str = match &v {
                    Value::Str(s) => s.clone(),
                    other => other.to_variant_string(),
                };
                match k.as_str() {
                    "guid" => {
                        // alter guid-Key → neuer vidpid-Key (nur wenn leer)
                        if vidpid.is_empty() {
                            vidpid = v_str;
                        }
                    }
                    "vidpid" => {
                        vidpid = v_str;
                    }
                    "controller_mapping" => {
                        controller_mapping = v_str;
                    }
                    _ => {}
                }
            }
            // dezimale vid/pid → hexadezimal
            if vidpid.contains(':') && !vidpid.contains('x') {
                let ids: Vec<&str> = vidpid.split(':').collect();
                if ids.len() == 2 {
                    if let (Ok(vid), Ok(pid)) = (ids[0].trim().parse::<u32>(), ids[1].trim().parse::<u32>()) {
                        vidpid = format!("0x{vid:04x}:0x{pid:04x}");
                    }
                }
            }
            vec![
                ("vidpid".to_string(), Value::Str(vidpid)),
                ("controller_mapping".to_string(), Value::Str(controller_mapping)),
            ]
        })
        .collect();
    store.write_array("controller_mappings", rewritten);
}

/// `InitializePlaceboSettings` — exakt die C++-Defaults (Werte sind
/// Strings außer contrast_recovery/peak_percentile, die als
/// Gleitkommazahlen geschrieben werden).
fn initialize_placebo_settings(placebo: &mut IniStore) {
    if placebo.contains("placebo_settings/version") {
        return;
    }
    // Reihenfolge exakt wie der reale QSettings-Output einer frischen
    // placebo_render_params.ini (alphabetisch, da Qt neue Keys sortiert):
    placebo.set_value("placebo_settings/color_map_preset", Value::Str("high_quality".to_string()));
    placebo.set_value("placebo_settings/contrast_recovery", Value::Float(0.3));
    placebo.set_value("placebo_settings/deband", Value::Str("yes".to_string()));
    placebo.set_value("placebo_settings/deinterlace", Value::Str("no".to_string()));
    placebo.set_value("placebo_settings/deinterlace_algo", Value::Str("yadif".to_string()));
    placebo.set_value("placebo_settings/deinterlace_preset", Value::Str("default".to_string()));
    placebo.set_value("placebo_settings/deinterlace_skip_spatial", Value::Str("no".to_string()));
    placebo.set_value("placebo_settings/peak_detect_preset", Value::Str("high_quality".to_string()));
    placebo.set_value("placebo_settings/peak_percentile", Value::Float(99.995));
    placebo.set_value("placebo_settings/upscaler", Value::Str("ewa_lanczos".to_string()));
    placebo.set_value("placebo_settings/version", Value::Str("0".to_string()));
}

#[cfg(test)]
#[path = "settings_tests.rs"]
mod tests;
