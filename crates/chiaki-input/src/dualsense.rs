// SPDX-License-Identifier: AGPL-3.0-only
// Native hidapi-Erweiterung für Sony-Pads — ersetzt den hidapi-Teil von SDL,
// den der C++-Client über SDL_GameController mitbekommt, den gilrs unter
// Windows (XInput-only) aber NICHT sieht.
//
// Geräte: DualSense (0x054c:0x0ce6), DualSense Edge (0x054c:0x0df2),
// DualShock4 v1 (0x054c:0x05c4) und v2 (0x054c:0x09cc) — Liste wie in
// controllermanager.cpp (chiaki_dualsense_controller_ids etc.).
//
// == Output-Report-Layout (DualSense) ==
// Quelle: SDL2 SDL_hidapi_ps5.c (zlib, Copyright Sam Lantinga) — identisch zum
// DS5EffectsState_t in controllermanager.cpp; Report-IDs und CRC wie in SDL:
//   USB:  Report-ID 0x02 + 47 Byte Effects-Payload (48 Byte total)
//   BT:   Report-ID 0x31 + 0x02 (Magic) + 47 Byte + CRC32 (0xA2-HDR,
//         poly 0xEDB88320) am Ende, auf 78 Byte gepaddet
// Payload (Offsets 0..46):
//   0  ucEnableBits1   0x01 rumble-emulation aktivieren, 0x02 audio haptics
//                      aus, 0x04 linker Trigger-Effekt, 0x08 rechter
//   1  ucEnableBits2   0x01 Mic-LED, 0x04 LED-Farbe, 0x10 Touchpad-Lichter,
//                      0x40 Haptik-/Intensitäts-Setzung (chiaki nutzt das
//                      zusammen mit Byte 0x24)
//   2  ucRumbleRight, 3 ucRumbleLeft
//   4-6 Lautstärken (Headphone/Speaker/Mic)
//   7  ucAudioEnableBits, 8 ucMicLightMode, 9 ucAudioMuteBits
//   10-20 rgucRightTriggerEffect[11] (byte0 = Typ, 1-10 Daten)
//   21-31 rgucLeftTriggerEffect[11]
//   32-37 rgucUnknown1 (byte 0x24 = DualSense-Intensität: high nibble Trigger,
//         low nibble Rumble)
//   38 ucEnableBits3   0x04 verbesserte Rumble-Emulation ab Firmware 0x0224
//   39-40 rgucUnknown2, 41 ucLedAnim, 42 ucLedBrightness
//   43 ucPadLights (Player-LED-Bitmaske, 0x1F alle, |0x20 sofort statt fade)
//   44-46 ucLedRed/Green/Blue
//
// == Input-Report (DualSense) ==
// Quelle: SDL2 SDL_hidapi_ps5.c PS5StatePacket_t:
//   USB:  Report-ID 0x01, 64 Byte, Payload ab Offset 1
//   BT:   Report-ID 0x31, 78 Byte (+CRC), Payload ab Offset 2
// Payload: 0-3 Sticks (0x80-Mitte), 4 L2, 5 R2, 6 Zähler,
//   7 Buttons0 (high nibble: 0x10 Square/0x20 Cross/0x40 Circle/0x80 Triangle,
//     low nibble DPad-Hat 0=N,1=NE,2=E,3=SE,4=S,5=SW,6=W,7=NW,8=keine),
//   8 Buttons1 (0x01 L1, 0x02 R1, 0x10 Create/Share, 0x20 Options, 0x40 L3,
//     0x80 R3), 9 Buttons2 (0x01 PS, 0x02 Touchpad-Klick),
//   11-14 Sequenz (u32 LE), 15-20 Gyro i16 LE (X/Y/Z), 21-26 Accel i16 LE,
//   32/36 Touch-Finger 1/2 (high bit des Zählers = Finger oben, dann 3 Byte
//   12-bit X/Y: x = d0 | (d1&0xF)<<8, y = d1>>4 | d2<<4; X max 1920, Y max 1070)
// Einheiten (SDL HIDAPI_DriverPS5_ApplyCalibrationData ohne Kalibrierung):
//   gyro = raw * 64 / 1024 * PI/180  -> rad/s
//   accel = raw / 8192               -> g
// (genau wie die Kette SDL-Sensor -> /SDL_STANDARD_GRAVITY in
// Controller::HandleSensorEvent in controllermanager.cpp)
//
// == DualShock4 ==
// Input (USB 0x01/64 Byte, Payload ab 1; BT 0x11/78 Byte, Payload ab 3):
//   0-3 Sticks, 4 Hat(nibble)+Square/Cross/Circle/Triangle (0x10..0x80),
//   5 (0x01 L1, 0x02 R1, 0x04 L2, 0x08 R2, 0x10 Share, 0x20 Options,
//      0x40 L3, 0x80 R3), 6 (0x01 PS, 0x02 Touchpad-Klick, high bits Zähler),
//   9 L2 analog, 10 R2 analog. Touch/IMU des DS4 werden (wie setsu sie nur
//   separat liefert) nicht ausgelesen — dokumentierte Einschränkung.
// Output (USB 0x05): 1 = Flags (0x07 Motoren+LED), 4 Rumble klein (rechts),
//   5 Rumble groß (links), 6-8 LED RGB (Layout wie Linux-Kernel hid-sony).
//
// == Haptics ==
// Der C++-Client spielt die Haptics-PCM-Frames (10 ms Stereo S16 @ 3000 Hz)
// NICHT über den HID-Output-Report, sondern über das 4-Kanal-48-kHz-Audio-
// device des DualSense (StreamSession::InitHaptics/ConnectHaptics/
// PushHapticsFrame; SDL_BuildAudioCVT(4ch,3000)->(4ch,48000), needle
// "Wireless Controller" unter Windows). Das wird hier 1:1 über waveOut
// (WASAPI-Legacy-Stack, gemeinsamer Modus) nachgebaut: die PCM-Bytes werden
// — wie im C++ — als 4-Kanal-Daten interpretiert, linear 16x hochgesampelt
// und auf das gefundene DualSense-Audiodevice geschrieben.
//
// Exklusivität: Windows-HID-Handles sind geteilt (kein hidraw-Grab wie unter
// Linux); gilrs/XInput und unsere hidapi-Instanz können dasselbe physische
// Gerät parallel öffnen. Da Sony-Pads unter Windows ohnehin kein XInput
// anbieten, konkurriert real nur Steam Input (eigener Virtual-Pad) mit uns.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use hidapi::{HidApi, DeviceInfo as HidDeviceInfo};

use chiaki_core::controller::{
    BUTTON_BOX, BUTTON_CROSS, BUTTON_DPAD_DOWN, BUTTON_DPAD_LEFT, BUTTON_DPAD_RIGHT,
    BUTTON_DPAD_UP, BUTTON_L1, BUTTON_L3, BUTTON_MOON, BUTTON_OPTIONS, BUTTON_PS, BUTTON_PYRAMID,
    BUTTON_R1, BUTTON_R3, BUTTON_SHARE, BUTTON_TOUCHPAD, ControllerState,
};
use chiaki_core::orientation::{self, AccelNewZero, OrientationTracker};

use crate::gamepad::{DeviceId, GamepadEvent, GamepadDeviceInfo, UPDATE_INTERVAL_MS};
use crate::InputError;

/// PS_TOUCHPAD_MAXX/Y aus controllermanager.h — Koordinaten, die die Konsole
/// erwartet (das DualSense liefert 1920x1070, skaliert wird nur Y auf 1079).
pub const PS_TOUCHPAD_MAXX: u16 = 1920;
pub const PS_TOUCHPAD_MAXY: u16 = 1079;
/// Touchpad-Auflösung des DualSense-Rohreports (SDL TOUCHPAD_SCALE 1/1920, 1/1070).
const DS_TOUCHPAD_MAXX: u32 = 1920;
const DS_TOUCHPAD_MAXY: u32 = 1070;

/// DUALSENSE_AUDIO_DEVICE_NEEDLE (nicht-Linux-Zweig aus streamsession.cpp).
pub const DUALSENSE_AUDIO_DEVICE_NEEDLE: &str = "Wireless Controller";

pub const VID_SONY: u16 = 0x054c;
pub const PID_DUALSENSE: u16 = 0x0ce6;
pub const PID_DUALSENSE_EDGE: u16 = 0x0df2;
pub const PID_DS4_V1: u16 = 0x05c4;
pub const PID_DS4_V2: u16 = 0x09cc;

/// Fehler-Typ für die native HID-Erweiterung.
#[derive(Debug, thiserror::Error)]
pub enum NativeError {
    #[error("hidapi-Fehler: {0}")]
    Hid(#[from] hidapi::HidError),
    #[error("Kein Sony-Controller gefunden")]
    DeviceNotFound,
    #[error("Aktion für diesen Gerätetyp nicht unterstützt: {0}")]
    Unsupported(&'static str),
    #[error("Haptics-Audio-Fehler: {0}")]
    Audio(String),
}

impl From<NativeError> for InputError {
    fn from(e: NativeError) -> Self {
        match e {
            NativeError::Hid(h) => InputError::Hid(h),
            NativeError::Audio(s) => InputError::Audio(s),
            other => InputError::Other(other.to_string()),
        }
    }
}

/// Erkannter Gerätetyp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SonyKind {
    DualSense,
    DualSenseEdge,
    DualShock4,
}

/// Identifikation eines nativen Geräts (wie Controller::GetVIDPIDString()).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SonyDeviceInfo {
    pub path: String,
    pub serial: Option<String>,
    pub vendor_id: u16,
    pub product_id: u16,
    pub kind: SonyKind,
    pub product_name: Option<String>,
}

impl SonyDeviceInfo {
    fn from_hid(info: &HidDeviceInfo) -> Option<SonyDeviceInfo> {
        if info.vendor_id() != VID_SONY {
            return None;
        }
        let kind = match info.product_id() {
            PID_DUALSENSE => SonyKind::DualSense,
            PID_DUALSENSE_EDGE => SonyKind::DualSenseEdge,
            PID_DS4_V1 | PID_DS4_V2 => SonyKind::DualShock4,
            _ => return None,
        };
        Some(SonyDeviceInfo {
            path: info.path().to_string_lossy().into_owned(),
            serial: info.serial_number().map(|s| s.to_owned()),
            vendor_id: info.vendor_id(),
            product_id: info.product_id(),
            kind,
            product_name: info.product_string().map(|s| s.to_owned()),
        })
    }

    pub fn device_id(&self) -> DeviceId {
        DeviceId::NativeDualSense(self.path.clone())
    }

    pub fn gamepad_info(&self) -> GamepadDeviceInfo {
        let mut info = GamepadDeviceInfo::from_vid_pid(Some(self.vendor_id), Some(self.product_id));
        info.name = self.product_name.clone().unwrap_or_else(|| match self.kind {
            SonyKind::DualSense => "DualSense".into(),
            SonyKind::DualSenseEdge => "DualSense Edge".into(),
            SonyKind::DualShock4 => "DualShock4".into(),
        });
        info.guid = self
            .serial
            .clone()
            .unwrap_or_else(|| format!("{:04x}:{:04x}", self.vendor_id, self.product_id));
        info.is_dualsense = matches!(self.kind, SonyKind::DualSense | SonyKind::DualSenseEdge);
        info.is_dualsense_edge = self.kind == SonyKind::DualSenseEdge;
        info
    }
}

// ---------------------------------------------------------------------------
// CRC32 (IEEE, reflected poly 0xEDB88320) für Bluetooth-Reports — gleiche
// Variante wie SDL_crc32 in SDL_hidapi_ps5.c.
// ---------------------------------------------------------------------------

pub fn crc32(data: &[u8]) -> u32 {
    // Byte-weise berechnet (Tabelle lohnt sich bei 78-Byte-Reports nicht).
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

// ---------------------------------------------------------------------------
// Output-Report-Builder (DS5EffectsState_t aus SDL/controllermanager.cpp)
// ---------------------------------------------------------------------------

/// Effekt-Payload des DualSense-Output-Reports (47 Byte, siehe Modul-Doku).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ds5EffectsState {
    pub data: [u8; 47],
}

impl Default for Ds5EffectsState {
    fn default() -> Self {
        Ds5EffectsState { data: [0; 47] }
    }
}

// Offsets wie im struct-Kommentar in controllermanager.cpp.
pub const OFF_ENABLE_BITS1: usize = 0;
pub const OFF_ENABLE_BITS2: usize = 1;
pub const OFF_RUMBLE_RIGHT: usize = 2;
pub const OFF_RUMBLE_LEFT: usize = 3;
pub const OFF_RIGHT_TRIGGER_EFFECT: usize = 10;
pub const OFF_LEFT_TRIGGER_EFFECT: usize = 21;
pub const OFF_UNKNOWN1: usize = 32;
pub const OFF_DUALSENSE_INTENSITY: usize = 36; // rgucUnknown1[4]
pub const OFF_ENABLE_BITS3: usize = 38;
pub const OFF_PAD_LIGHTS: usize = 43;
pub const OFF_LED_RED: usize = 44;
pub const OFF_LED_GREEN: usize = 45;
pub const OFF_LED_BLUE: usize = 46;

impl Ds5EffectsState {
    /// Port von `Controller::SetDualSenseRumble()` (controllermanager.cpp):
    /// Firmware < 0x0224: Rumble-Emulation mit >>1, sonst volle Stärke über
    /// EnableBits3; audio haptics werden ausgeschaltet (0x02).
    pub fn dualsense_rumble(firmware_version: u16, left: u8, right: u8, intensity: u8) -> Self {
        let mut s = Self::default();
        if firmware_version < 0x0224 {
            s.data[OFF_ENABLE_BITS1] |= 0x01;
            // Shift reduziert die effektive Stärke auf Xbox-Niveau
            s.data[OFF_RUMBLE_LEFT] = left >> 1;
            s.data[OFF_RUMBLE_RIGHT] = right >> 1;
        } else {
            s.data[OFF_ENABLE_BITS3] |= 0x04;
            s.data[OFF_RUMBLE_LEFT] = left;
            s.data[OFF_RUMBLE_RIGHT] = right;
        }
        s.data[OFF_DUALSENSE_INTENSITY] = intensity;
        s.data[OFF_ENABLE_BITS2] |= 0x40;
        s.data[OFF_ENABLE_BITS1] |= 0x02;
        s
    }

    /// Port von `Controller::SetTriggerEffects()`: Trigger-Effekt-Bytes
    /// (Typ + 10 Daten-Bytes) direkt aus dem ChiakiEvent, Intensität dazu.
    pub fn trigger_effects(
        type_left: u8,
        data_left: &[u8; 10],
        type_right: u8,
        data_right: &[u8; 10],
        intensity: u8,
    ) -> Self {
        let mut s = Self::default();
        s.data[OFF_DUALSENSE_INTENSITY] = intensity;
        s.data[OFF_ENABLE_BITS2] |= 0x40;
        s.data[OFF_ENABLE_BITS1] |= 0x04 /* left trigger */ | 0x08 /* right trigger */;
        s.data[OFF_LEFT_TRIGGER_EFFECT] = type_left;
        s.data[OFF_LEFT_TRIGGER_EFFECT + 1..OFF_LEFT_TRIGGER_EFFECT + 11]
            .copy_from_slice(data_left);
        s.data[OFF_RIGHT_TRIGGER_EFFECT] = type_right;
        s.data[OFF_RIGHT_TRIGGER_EFFECT + 1..OFF_RIGHT_TRIGGER_EFFECT + 11]
            .copy_from_slice(data_right);
        s
    }

    /// Port von `Controller::ChangeLEDColor()` (SDL k_EDS5EffectLED).
    pub fn led(led_color: [u8; 3]) -> Self {
        let mut s = Self::default();
        s.data[OFF_ENABLE_BITS2] |= 0x04;
        s.data[OFF_LED_RED] = led_color[0];
        s.data[OFF_LED_GREEN] = led_color[1];
        s.data[OFF_LED_BLUE] = led_color[2];
        s
    }

    /// Port der Player-Lichter (SDL SetLightsForPlayerIndex — Bitmask 0x1F
    /// alle Lichter, 0x20 = sofort statt faden; SDL_GAMECONTROLLER Player-LED).
    pub fn player_lights(player_index: i32) -> Self {
        let mut s = Self::default();
        const LIGHTS: [u8; 4] = [0x04, 0x0A, 0x15, 0x1B];
        s.data[OFF_ENABLE_BITS2] |= 0x10; /* Enable touchpad lights */
        if player_index >= 0 {
            s.data[OFF_PAD_LIGHTS] = LIGHTS[(player_index as usize) % LIGHTS.len()] | 0x20;
        } else {
            s.data[OFF_PAD_LIGHTS] = 0x00;
        }
        s
    }

    /// Port von `Controller::SetDualsenseMic()`.
    pub fn mic(on: bool, intensity: u8) -> Self {
        let mut s = Self::default();
        s.data[OFF_DUALSENSE_INTENSITY] = intensity;
        s.data[OFF_ENABLE_BITS2] |= 0x40;
        s.data[OFF_ENABLE_BITS2] |= 0x01 /* mic light */ | 0x02 /* mic */;
        s.data[8] = if on { 0x01 } else { 0x00 }; // ucMicLightMode
        s.data[9] = if on { 0x08 } else { 0x00 }; // ucAudioMuteBits
        s
    }

    /// DualSense-Intensität setzen (rgucUnknown1[4] + EnableBits 0x02|0x40),
    /// wie SetDualSenseRumble/SetTriggerEffects sie mitsenden; separat
    /// nutzbar für CHIAKI_EVENT_HAPTIC_INTENSITY/TRIGGER_INTENSITY.
    pub fn intensity(intensity: u8) -> Self {
        let mut s = Self::default();
        s.data[OFF_DUALSENSE_INTENSITY] = intensity;
        s.data[OFF_ENABLE_BITS2] |= 0x40;
        s.data[OFF_ENABLE_BITS1] |= 0x02;
        s
    }
}

/// CRC32-Referenzwert (IEEE): "123456789" -> 0xCBF43926.
#[test]
fn crc32_reference_vector() {
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
}

// ---------------------------------------------------------------------------
// Input-Report-Parsing
// ---------------------------------------------------------------------------

/// Verbindungstransport (bestimmt Report-IDs und Offsets).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Usb,
    Bluetooth,
}

/// Dekodierte Werte eines DualSense-State-Pakets (Rohwerte, siehe Modul-Doku).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DualSenseInput {
    pub left_x: u8,
    pub left_y: u8,
    pub right_x: u8,
    pub right_y: u8,
    pub l2: u8,
    pub r2: u8,
    pub buttons0: u8,
    pub buttons1: u8,
    pub buttons2: u8,
    pub sequence: u32,
    /// Gyro-Rohwerte (i16 LE) — Umrechnung siehe [`imu_values`].
    pub gyro_raw: [i16; 3],
    /// Accel-Rohwerte (i16 LE).
    pub accel_raw: [i16; 3],
    /// Beide Finger: (down, x, y, counter)
    pub touch: [(bool, u16, u16, u8); 2],
}

/// Dekodierter DS4-State (Rohwerte).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Ds4Input {
    pub left_x: u8,
    pub left_y: u8,
    pub right_x: u8,
    pub right_y: u8,
    pub buttons4: u8,
    pub buttons5: u8,
    pub buttons6: u8,
    pub l2: u8,
    pub r2: u8,
}

/// DPad-Hat-Nibble -> Chiaki-Bits (Hut-Tabelle aus SDL hidapi).
pub fn hat_to_buttons(hat: u8) -> u32 {
    let mut out = 0;
    if hat <= 7 {
        if hat == 0 || hat == 1 || hat == 7 {
            out |= BUTTON_DPAD_UP;
        }
        if (1..=3).contains(&hat) {
            out |= BUTTON_DPAD_RIGHT;
        }
        if (3..=5).contains(&hat) {
            out |= BUTTON_DPAD_DOWN;
        }
        if (5..=7).contains(&hat) {
            out |= BUTTON_DPAD_LEFT;
        }
    }
    out
}

/// DPad-Chiaki-Bits -> Hat-Nibble (DS4-Report, DS4_BUTTON_DPAD_* aus ViGEm).
pub fn buttons_to_hat(buttons: u32) -> u8 {
    let up = buttons & BUTTON_DPAD_UP != 0;
    let down = buttons & BUTTON_DPAD_DOWN != 0;
    let left = buttons & BUTTON_DPAD_LEFT != 0;
    let right = buttons & BUTTON_DPAD_RIGHT != 0;
    match (up, right, down, left) {
        (true, false, false, false) => 0,
        (true, true, false, false) => 1,
        (false, true, false, false) => 2,
        (false, true, true, false) => 3,
        (false, false, true, false) => 4,
        (false, false, true, true) => 5,
        (false, false, false, true) => 6,
        (true, false, false, true) => 7,
        _ => 8, // DS4_BUTTON_DPAD_NONE
    }
}

/// Gyro/Accel-Rohwerte in Chiaki-Einheiten (rad/s bzw. g) — Kette wie im
/// C++: SDL-Sensorformat -> /SDL_STANDARD_GRAVITY (controllermanager.cpp).
pub fn imu_values(raw: &[i16; 3], accel: bool) -> [f32; 3] {
    if accel {
        [
            raw[0] as f32 / 8192.0,
            raw[1] as f32 / 8192.0,
            raw[2] as f32 / 8192.0,
        ]
    } else {
        [
            raw[0] as f32 * 64.0 / 1024.0 * std::f32::consts::PI / 180.0,
            raw[1] as f32 * 64.0 / 1024.0 * std::f32::consts::PI / 180.0,
            raw[2] as f32 * 64.0 / 1024.0 * std::f32::consts::PI / 180.0,
        ]
    }
}

/// Parst einen DualSense-Input-Report (USB: 0x01/64 Byte ab Offset 1,
/// BT: 0x31/78 Byte ab Offset 2 — wie HIDAPI_DriverPS5_HandleStatePacket).
pub fn parse_dualsense_input(data: &[u8]) -> Option<Transport> {
    match data.first() {
        Some(0x01) if data.len() >= 54 => Some(Transport::Usb),
        Some(0x31) if data.len() >= 54 => Some(Transport::Bluetooth),
        _ => None,
    }
}

fn le_i16(data: &[u8], off: usize) -> i16 {
    i16::from_le_bytes([data[off], data[off + 1]])
}

fn le_u32(data: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}

/// Dekodiert den Payload (data inkl. Report-ID) in Rohwerte.
pub fn decode_dualsense(data: &[u8]) -> Option<DualSenseInput> {
    let base = match data.first() {
        Some(0x01) => 1, // USB
        Some(0x31) => 2, // BT (byte 1 = Magic 0x02)
        _ => return None,
    };
    if data.len() < base + 54 {
        return None;
    }
    let p = base;

    let decode_touch = |i: usize| -> (bool, u16, u16, u8) {
        let counter = data[p + i];
        let d = &data[p + i + 1..p + i + 4];
        let x = d[0] as u16 | (((d[1] & 0x0F) as u16) << 8);
        let y = ((d[1] >> 4) as u16) | ((d[2] as u16) << 4);
        // high bit clear = Finger ist unten (SDL: ucTouchpadCounterN & 0x80 == 0)
        let down = counter & 0x80 == 0;
        (down, x, y, counter & 0x7F)
    };

    Some(DualSenseInput {
        left_x: data[p],
        left_y: data[p + 1],
        right_x: data[p + 2],
        right_y: data[p + 3],
        l2: data[p + 4],
        r2: data[p + 5],
        buttons0: data[p + 7],
        buttons1: data[p + 8],
        buttons2: data[p + 9],
        sequence: le_u32(data, p + 11),
        gyro_raw: [le_i16(data, p + 15), le_i16(data, p + 17), le_i16(data, p + 19)],
        accel_raw: [le_i16(data, p + 21), le_i16(data, p + 23), le_i16(data, p + 25)],
        touch: [decode_touch(32), decode_touch(36)],
    })
}

/// Parst einen DS4-Input-Report (USB 0x01 ab Offset 1, BT 0x11 ab Offset 3).
pub fn decode_ds4(data: &[u8]) -> Option<Ds4Input> {
    let base = match data.first() {
        Some(0x01) => 1,  // USB
        Some(0x11) => 3,  // BT
        _ => return None,
    };
    if data.len() < base + 11 {
        return None;
    }
    let p = base;
    Some(Ds4Input {
        left_x: data[p],
        left_y: data[p + 1],
        right_x: data[p + 2],
        right_y: data[p + 3],
        buttons4: data[p + 4],
        buttons5: data[p + 5],
        buttons6: data[p + 6],
        l2: data[p + 9],
        r2: data[p + 10],
    })
}

/// Port der Button-Dekodierung aus SDL HandleStatePacket (DualSense).
pub fn dualsense_buttons(input: &DualSenseInput) -> u32 {
    let mut buttons = 0;
    let nibble = input.buttons0 >> 4;
    if nibble & 0x01 != 0 {
        buttons |= BUTTON_BOX; // Square
    }
    if nibble & 0x02 != 0 {
        buttons |= BUTTON_CROSS; // Cross
    }
    if nibble & 0x04 != 0 {
        buttons |= BUTTON_MOON; // Circle
    }
    if nibble & 0x08 != 0 {
        buttons |= BUTTON_PYRAMID; // Triangle
    }
    buttons |= hat_to_buttons(input.buttons0 & 0x0F);

    let b1 = input.buttons1;
    if b1 & 0x01 != 0 {
        buttons |= BUTTON_L1;
    }
    if b1 & 0x02 != 0 {
        buttons |= BUTTON_R1;
    }
    if b1 & 0x10 != 0 {
        buttons |= BUTTON_SHARE; // Create
    }
    if b1 & 0x20 != 0 {
        buttons |= BUTTON_OPTIONS;
    }
    if b1 & 0x40 != 0 {
        buttons |= BUTTON_L3;
    }
    if b1 & 0x80 != 0 {
        buttons |= BUTTON_R3;
    }
    if input.buttons2 & 0x01 != 0 {
        buttons |= BUTTON_PS;
    }
    if input.buttons2 & 0x02 != 0 {
        buttons |= BUTTON_TOUCHPAD;
    }
    buttons
}

/// Port der DS4-Button-Dekodierung (Report-Bytes 4-6).
pub fn ds4_buttons(input: &Ds4Input) -> u32 {
    let mut buttons = hat_to_buttons(input.buttons4 & 0x0F);
    let b4 = input.buttons4 >> 4;
    if b4 & 0x01 != 0 {
        buttons |= BUTTON_BOX; // Square
    }
    if b4 & 0x02 != 0 {
        buttons |= BUTTON_CROSS; // Cross
    }
    if b4 & 0x04 != 0 {
        buttons |= BUTTON_MOON; // Circle
    }
    if b4 & 0x08 != 0 {
        buttons |= BUTTON_PYRAMID; // Triangle
    }
    let b5 = input.buttons5;
    if b5 & 0x01 != 0 {
        buttons |= BUTTON_L1;
    }
    if b5 & 0x02 != 0 {
        buttons |= BUTTON_R1;
    }
    if b5 & 0x10 != 0 {
        buttons |= BUTTON_SHARE;
    }
    if b5 & 0x20 != 0 {
        buttons |= BUTTON_OPTIONS;
    }
    if b5 & 0x40 != 0 {
        buttons |= BUTTON_L3;
    }
    if b5 & 0x80 != 0 {
        buttons |= BUTTON_R3;
    }
    if input.buttons6 & 0x01 != 0 {
        buttons |= BUTTON_PS;
    }
    if input.buttons6 & 0x02 != 0 {
        buttons |= BUTTON_TOUCHPAD;
    }
    buttons
}

/// u8-Stick (0x80-Mitte) -> i16 wie SDL ((value * 257) - 32768).
pub fn stick_to_i16(value: u8) -> i16 {
    ((value as i32) * 257 - 32768).clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

/// i16-Stick -> u8 (0x80-Mitte), Umkehrung von stick_to_i16 mit Rundung
/// (+128, damit i16 0 -> 0x80 neutral landet).
pub fn i16_to_stick(value: i16) -> u8 {
    (((value as i32) + 32768 + 128) / 257).clamp(0, 255) as u8
}

// ---------------------------------------------------------------------------
// DualSenseDevice: ein offenes HID-Handle
// ---------------------------------------------------------------------------

/// Report-IDs / -Größen (SDL).
const USB_PACKET_LENGTH: usize = 64;
const BT_PACKET_LENGTH: usize = 78;

/// Ein nativ geöffnetes Sony-Gerät.
pub struct DualSenseDevice {
    dev: hidapi::HidDevice,
    pub info: SonyDeviceInfo,
    transport: Mutex<Transport>,
    /// GetDualSenseIntensity() aus controllermanager.cpp (0x00 = voll).
    intensity: Mutex<u8>,
    firmware_version: u16,
}

impl DualSenseDevice {
    /// Öffnet das Gerät über den hidapi-Pfad. Bevorzugt die Gamepad-Collection
    /// (usage page 0x01, usage 0x05); Fehler sind nicht fatal, dann wird der
    /// erste Treffer genommen.
    pub fn open(api: &HidApi, info: &SonyDeviceInfo) -> Result<DualSenseDevice, NativeError> {
        let mut candidates: Vec<&HidDeviceInfo> = Vec::new();
        let mut fallback: Vec<&HidDeviceInfo> = Vec::new();
        for dev in api.device_list() {
            if dev.path().to_string_lossy() == info.path {
                match (dev.usage_page(), dev.usage()) {
                    (0x01, 0x05) => candidates.push(dev),
                    _ => fallback.push(dev),
                }
            }
        }
        let hid_info = candidates
            .first()
            .or_else(|| fallback.first())
            .ok_or(NativeError::DeviceNotFound)?;
        let dev = hid_info.open_device(api)?;
        tracing::info!(
            "Sony-Controller geöffnet: {:?} ({:04x}:{:04x}, {})",
            info.kind,
            info.vendor_id,
            info.product_id,
            info.path
        );
        let mut device = DualSenseDevice {
            dev,
            info: info.clone(),
            transport: Mutex::new(Transport::Usb),
            intensity: Mutex::new(0x00),
            firmware_version: 0,
        };
        device.read_firmware_version();
        Ok(device)
    }

    /// Firmware-Version (SDL liest Feature-Report 0x20, Bytes 44/45 LE).
    fn read_firmware_version(&mut self) {
        let mut buf = [0u8; USB_PACKET_LENGTH];
        buf[0] = 0x20;
        if let Ok(n) = self.dev.get_feature_report(&mut buf) {
            if n >= 46 {
                self.firmware_version = (buf[45] as u16) << 8 | buf[44] as u16;
                tracing::debug!("Firmware-Version: {:04x}", self.firmware_version);
            }
        }
    }

    pub fn firmware_version(&self) -> u16 {
        self.firmware_version
    }

    pub fn set_intensity(&self, intensity: u8) {
        *self.intensity.lock().expect("intensity poisoned") = intensity;
    }

    fn current_intensity(&self) -> u8 {
        *self.intensity.lock().expect("intensity poisoned")
    }

    /// Aktualisiert den Transport anhand eines gelesenen Input-Reports.
    fn note_input(&self, data: &[u8]) {
        if let Some(t) = parse_dualsense_input(data) {
            *self.transport.lock().expect("transport poisoned") = t;
        }
    }

    fn transport(&self) -> Transport {
        *self.transport.lock().expect("transport poisoned")
    }

    /// Sendet den 47-Byte-Effekt-Payload (USB 0x02/48, BT 0x31/78+CRC —
    /// HIDAPI_DriverPS5_SendJoystickEffect).
    pub fn send_effects(&self, effects: &Ds5EffectsState) -> Result<(), NativeError> {
        if self.info.kind == SonyKind::DualShock4 {
            return Err(NativeError::Unsupported("DS4 kennt keine DS5-Effects"));
        }
        let mut report;
        match self.transport() {
            Transport::Usb => {
                report = vec![0u8; 1 + 47];
                report[0] = 0x02;
                report[1..].copy_from_slice(&effects.data);
            }
            Transport::Bluetooth => {
                report = vec![0u8; BT_PACKET_LENGTH];
                report[0] = 0x31;
                report[1] = 0x02; // Magic value
                report[2..2 + 47].copy_from_slice(&effects.data);
                // BT-Reports brauchen eine CRC am Ende (mind. unter Linux);
                // hidp header 0xA2 ist Teil der Berechnung.
                let mut crc_input = vec![0xA2u8];
                crc_input.extend_from_slice(&report[..2 + 47]);
                let crc = crc32(&crc_input);
                let off = 2 + 47;
                report[off..off + 4].copy_from_slice(&crc.to_le_bytes());
            }
        }
        let n = self.dev.write(&report)?;
        if n != report.len() {
            tracing::warn!("Output-Report unvollständig geschrieben: {n}/{}", report.len());
        }
        Ok(())
    }

    /// Port von `Controller::SetRumble()` für DualSense/Edge.
    pub fn set_dualsense_rumble(&self, left: u8, right: u8) -> Result<(), NativeError> {
        self.send_effects(&Ds5EffectsState::dualsense_rumble(
            self.firmware_version,
            left,
            right,
            self.current_intensity(),
        ))
    }

    /// Port von `Controller::SetTriggerEffects()`.
    pub fn set_trigger_effects(
        &self,
        type_left: u8,
        data_left: &[u8; 10],
        type_right: u8,
        data_right: &[u8; 10],
    ) -> Result<(), NativeError> {
        self.send_effects(&Ds5EffectsState::trigger_effects(
            type_left,
            data_left,
            type_right,
            data_right,
            self.current_intensity(),
        ))
    }

    /// Trigger-Effekte und Rumble zurücksetzen — wie `~Controller()` und der
    /// Session-Abbau im C++ (SetTriggerEffects(0x05, {0}, 0x05, {0}), Rumble 0).
    pub fn clear_effects(&self) -> Result<(), NativeError> {
        let clear = [0u8; 10];
        self.set_trigger_effects(0x05, &clear, 0x05, &clear)?;
        self.set_dualsense_rumble(0, 0)
    }

    /// Port von `Controller::ChangeLEDColor()`.
    pub fn set_led(&self, color: [u8; 3]) -> Result<(), NativeError> {
        self.send_effects(&Ds5EffectsState::led(color))
    }

    /// Port von `Controller::ChangePlayerIndex()` (Player-LEDs).
    pub fn set_player_lights(&self, player_index: i32) -> Result<(), NativeError> {
        self.send_effects(&Ds5EffectsState::player_lights(player_index))
    }

    /// Port von `Controller::SetDualsenseMic()`.
    pub fn set_mic(&self, on: bool) -> Result<(), NativeError> {
        self.send_effects(&Ds5EffectsState::mic(on, self.current_intensity()))
    }

    /// DualSense-Intensität setzen (Byte 0x24, high nibble Trigger, low
    /// nibble Rumble — Werte wie CHIAKI_EVENT_*_INTENSITY im C++).
    pub fn set_haptic_intensity(&self, intensity: u8) -> Result<(), NativeError> {
        self.set_intensity(intensity);
        self.send_effects(&Ds5EffectsState::intensity(intensity))
    }

    /// Port von `Controller::SetHapticRumble()` für Nicht-DualSense (hier:
    /// DS4-Output-Report 0x05, Layout wie hid-sony.c).
    pub fn set_ds4_rumble(&self, left: u8, right: u8) -> Result<(), NativeError> {
        if self.info.kind != SonyKind::DualShock4 {
            return Err(NativeError::Unsupported("set_ds4_rumble nur am DS4"));
        }
        let mut report;
        match self.transport() {
            Transport::Usb => {
                report = vec![0u8; 11];
                report[0] = 0x05;
                report[1] = 0x07; // Motoren + LED setzen
            }
            Transport::Bluetooth => {
                // BT: Report 0x11, +2 Byte Versatz, CRC am Ende (wie DS5-BT).
                report = vec![0u8; BT_PACKET_LENGTH];
                report[0] = 0x11;
                report[1] = 0xC0; // CRC-fähiger Sequenz-Header (hid-sony)
            }
        }
        let base = match self.transport() {
            Transport::Usb => 0,
            Transport::Bluetooth => 2,
        };
        report[base + 4] = right; // kleiner Motor
        report[base + 5] = left; // großer Motor
        if self.transport() == Transport::Bluetooth {
            let mut crc_input = vec![0xA2u8];
            crc_input.extend_from_slice(&report[..2 + 11]);
            let crc = crc32(&crc_input);
            let off = 2 + 11;
            report[off..off + 4].copy_from_slice(&crc.to_le_bytes());
        }
        let _ = self.dev.write(&report)?;
        Ok(())
    }

    /// DS4-LED (Output-Report 0x05, Bytes 6-8).
    pub fn set_ds4_led(&self, color: [u8; 3]) -> Result<(), NativeError> {
        if self.info.kind != SonyKind::DualShock4 {
            return Err(NativeError::Unsupported("set_ds4_led nur am DS4"));
        }
        let mut report = vec![0u8; 11];
        report[0] = 0x05;
        report[1] = 0x07;
        report[6..9].copy_from_slice(&color);
        let _ = self.dev.write(&report)?;
        Ok(())
    }

    /// Blockierender Read mit Timeout; liefert Roh-Report-Bytes.
    pub fn read_input(&self, timeout_ms: i32) -> hidapi::HidResult<Option<Vec<u8>>> {
        let mut buf = [0u8; BT_PACKET_LENGTH];
        match self.dev.read_timeout(&mut buf, timeout_ms) {
            Ok(0) => Ok(None),
            Ok(n) => {
                self.note_input(&buf[..n]);
                Ok(Some(buf[..n].to_vec()))
            }
            Err(e) => Err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// DualSenseManager: Enumerate + Lesen + State
// ---------------------------------------------------------------------------

struct ManagedDevice {
    dev: DualSenseDevice,
    device_id: DeviceId,
    core: DeviceCore,
}

/// State-Anteil eines nativen Geräts — getrennt vom HID-Handle, damit die
/// Report-Verarbeitung ohne Hardware testbar ist.
struct DeviceCore {
    kind: SonyKind,
    state: ControllerState,
    /// Finger -> Chiaki-Touch-ID (wie touch_ids-Map im C++).
    touch_ids: [Option<u8>; 2],
    tracker: OrientationTracker,
    accel_zero: AccelNewZero,
    real_accel: AccelNewZero,
    last_motion_ts: u32,
}

impl DeviceCore {
    fn new(kind: SonyKind) -> Self {
        DeviceCore {
            kind,
            state: ControllerState::default(),
            touch_ids: [None, None],
            tracker: OrientationTracker::new(),
            accel_zero: AccelNewZero::default(),
            real_accel: {
                let mut real = AccelNewZero::default();
                // chiaki_accel_new_zero_set_inactive(&real, true)
                orientation::set_inactive(&mut real, true);
                real
            },
            last_motion_ts: 0,
        }
    }

    /// Wendet einen Input-Report auf den State an; liefert `true`, wenn sich
    /// der State geändert hat (chiaki_controller_state_equals).
    fn apply_report(&mut self, report: &[u8]) -> bool {
        let mut state = self.state;
        match self.kind {
            SonyKind::DualSense | SonyKind::DualSenseEdge => {
                let Some(input) = decode_dualsense(report) else {
                    return false;
                };
                state.buttons = dualsense_buttons(&input);
                state.left_x = stick_to_i16(input.left_x);
                state.left_y = stick_to_i16(input.left_y);
                state.right_x = stick_to_i16(input.right_x);
                state.right_y = stick_to_i16(input.right_y);
                state.l2_state = input.l2;
                state.r2_state = input.r2;

                // Touchpad (2 Finger): start/set/stop wie HandleTouchpadEvent.
                for (finger, &(down, x, y, _counter)) in input.touch.iter().enumerate() {
                    // SDL normalisiert auf 0..1 (SCALE 1/1920, 1/1070), chiaki
                    // skaliert zurück auf PS_TOUCHPAD_MAXX/MAXY.
                    let px = (x as u32 * PS_TOUCHPAD_MAXX as u32 / DS_TOUCHPAD_MAXX) as u16;
                    let py = (y as u32 * PS_TOUCHPAD_MAXY as u32 / DS_TOUCHPAD_MAXY) as u16;
                    let mapped = self.touch_ids[finger];
                    if down {
                        match mapped {
                            Some(id) => state.set_touch_pos(id, px, py),
                            None => {
                                let id = state.start_touch(px, py);
                                if id >= 0 {
                                    self.touch_ids[finger] = Some(id as u8);
                                }
                            }
                        }
                    } else if let Some(id) = mapped {
                        state.stop_touch(id);
                        self.touch_ids[finger] = None;
                    }
                }

                // IMU: accel zuerst (mit altem gyro), dann gyro (mit neuem
                // accel) — Reihenfolge wie HandleSensorEvent im C++.
                let accel = imu_values(&input.accel_raw, true);
                let gyro = imu_values(&input.gyro_raw, false);
                orientation::set_active(
                    &mut self.real_accel,
                    accel[0],
                    accel[1],
                    accel[2],
                    true,
                );
                let ts = now_us32();
                self.tracker.update(
                    state.gyro_x,
                    state.gyro_y,
                    state.gyro_z,
                    accel[0],
                    accel[1],
                    accel[2],
                    &self.accel_zero,
                    false,
                    ts,
                );
                self.tracker.update(
                    gyro[0],
                    gyro[1],
                    gyro[2],
                    state.accel_x,
                    state.accel_y,
                    state.accel_z,
                    &self.accel_zero,
                    true,
                    ts,
                );
                self.last_motion_ts = ts;
                self.tracker.apply_to_controller_state(&mut state);
            }
            SonyKind::DualShock4 => {
                let Some(input) = decode_ds4(report) else {
                    return false;
                };
                state.buttons = ds4_buttons(&input);
                state.left_x = stick_to_i16(input.left_x);
                state.left_y = stick_to_i16(input.left_y);
                state.right_x = stick_to_i16(input.right_x);
                state.right_y = stick_to_i16(input.right_y);
                state.l2_state = input.l2;
                state.r2_state = input.r2;
            }
        }

        // Nur bei echter Änderung dem Manager melden (state_equals).
        let changed = !self.state.equals(&state);
        if changed {
            self.state = state;
        }
        changed
    }

    fn reset_motion(&mut self) {
        // Port von Controller::resetMotionControls()
        orientation::set_active(
            &mut self.accel_zero,
            self.real_accel.accel_x,
            self.real_accel.accel_y,
            self.real_accel.accel_z,
            false,
        );
        self.tracker = OrientationTracker::new();
    }
}

/// Manager für alle nativen Sony-Pads (Ersatz für den SDL-hidapi-Teil von
/// `ControllerManager` unter Windows).
pub struct DualSenseManager {
    api: Arc<HidApi>,
    devices: Arc<Mutex<Vec<ManagedDevice>>>,
    running: Arc<AtomicBool>,
}

impl DualSenseManager {
    pub fn new() -> Result<Self, NativeError> {
        Ok(DualSenseManager {
            api: Arc::new(HidApi::new()?),
            devices: Arc::new(Mutex::new(Vec::new())),
            running: Arc::new(AtomicBool::new(false)),
        })
    }

    fn open_all(&self, events: &mut Vec<GamepadEvent>) {
        let mut devices = self.devices.lock().expect("ds devices poisoned");
        let mut open_paths: Vec<String> = devices
            .iter()
            .map(|d| d.dev.info.path.clone())
            .collect();
        for info in self.api.device_list().filter_map(SonyDeviceInfo::from_hid) {
            if open_paths.contains(&info.path) {
                continue;
            }
            match DualSenseDevice::open(&self.api, &info) {
                Ok(dev) => {
                    let device_id = info.device_id();
                    let gamepad_info = info.gamepad_info();
                    let state = ControllerState::default();
                    events.push(GamepadEvent::Connected {
                        device: device_id.clone(),
                        info: gamepad_info,
                        state,
                    });
                    devices.push(ManagedDevice {
                        dev,
                        device_id,
                        core: DeviceCore::new(info.kind),
                    });
                    open_paths.push(info.path.clone());
                }
                Err(e) => tracing::warn!("Konnte {:?} nicht öffnen: {}", info.kind, e),
            }
        }
    }

    /// Liest/aktualisiert alle Geräte; ersetzt den SDL-Poll-Timer
    /// (UPDATE_INTERVAL_MS). Liefert GamepadEvents.
    pub fn poll(&self) -> Vec<GamepadEvent> {
        let mut events = Vec::new();
        self.open_all(&mut events);

        let mut devices = self.devices.lock().expect("ds devices poisoned");
        let mut dead: Vec<usize> = Vec::new();
        for (idx, managed) in devices.iter_mut().enumerate() {
            let report = match managed.dev.read_input(UPDATE_INTERVAL_MS as i32) {
                Ok(Some(r)) => r,
                Ok(None) => continue,
                Err(e) => {
                    tracing::warn!(
                        "Sony-Controller {:?} Fehler beim Lesen: {} — entferne Gerät",
                        managed.dev.info.kind,
                        e
                    );
                    dead.push(idx);
                    continue;
                }
            };
            if managed.core.apply_report(&report) {
                events.push(GamepadEvent::StateUpdated {
                    device: managed.device_id.clone(),
                    state: managed.core.state,
                });
            }
        }
        for idx in dead.into_iter().rev() {
            let removed = devices.remove(idx);
            events.push(GamepadEvent::Disconnected(removed.device_id));
        }
        events
    }

    /// Port des QTimer-Loops: Lese-Thread für alle nativen Geräte.
    pub fn spawn_read_thread<F>(&self, interval_ms: u64, callback: F) -> JoinHandle<()>
    where
        F: Fn(GamepadEvent) + Send + Sync + 'static,
    {
        self.running.store(true, Ordering::Relaxed);
        let manager = DualSenseManager {
            api: Arc::clone(&self.api),
            devices: Arc::clone(&self.devices),
            running: Arc::clone(&self.running),
        };
        let callback = Arc::new(callback);
        std::thread::Builder::new()
            .name("chiaki-input-dualsense".into())
            .spawn(move || {
                let interval = std::time::Duration::from_millis(interval_ms.max(1));
                let callback = callback;
                while manager.running.load(Ordering::Relaxed) {
                    for event in manager.poll() {
                        callback(event);
                    }
                    std::thread::sleep(interval);
                }
                tracing::debug!("DualSense-Lese-Thread beendet");
            })
            .expect("DualSense-Thread konnte nicht gestartet werden")
    }

    pub fn stop_read_thread(&self) {
        self.running.store(false, Ordering::Relaxed);
    }

    fn with_device(
        &self,
        device: Option<&DeviceId>,
        f: impl Fn(&DualSenseDevice) -> Result<(), NativeError>,
        fallback: impl Fn(&DualSenseDevice) -> bool,
    ) -> Result<(), NativeError> {
        let devices = self.devices.lock().expect("ds devices poisoned");
        let mut last_err = None;
        for m in devices.iter() {
            let selected = match device {
                Some(id) => &m.device_id == id,
                None => fallback(&m.dev),
            };
            if selected {
                if let Err(e) = f(&m.dev) {
                    tracing::warn!("Natives Gerät {:?}: {}", m.device_id, e);
                    last_err = Some(e);
                }
            }
        }
        last_err.map_or(Ok(()), Err)
    }

    /// Rumble für alle (oder ein bestimmtes) DualSense/Edge-Gerät — Port des
    /// CHIAKI_EVENT_RUMBLE-Handlers in streamsession.cpp (DualSense volle
    /// Stärke, alles andere skaliert).
    pub fn set_rumble(
        &self,
        device: Option<&DeviceId>,
        left: u8,
        right: u8,
    ) -> Result<(), NativeError> {
        self.with_device(
            device,
            |dev| match dev.info.kind {
                SonyKind::DualSense | SonyKind::DualSenseEdge => dev.set_dualsense_rumble(left, right),
                SonyKind::DualShock4 => dev.set_ds4_rumble(left, right),
            },
            |dev| !matches!(dev.info.kind, SonyKind::DualShock4),
        )
    }

    /// CHIAKI_EVENT_TRIGGER_EFFECTS-Port.
    pub fn set_trigger_effects(
        &self,
        device: Option<&DeviceId>,
        type_left: u8,
        data_left: &[u8; 10],
        type_right: u8,
        data_right: &[u8; 10],
    ) -> Result<(), NativeError> {
        self.with_device(
            device,
            |dev| dev.set_trigger_effects(type_left, data_left, type_right, data_right),
            |dev| dev.info.kind != SonyKind::DualShock4,
        )
    }

    /// CHIAKI_EVENT_LED_COLOR-Port.
    pub fn set_led(&self, device: Option<&DeviceId>, color: [u8; 3]) -> Result<(), NativeError> {
        self.with_device(
            device,
            |dev| match dev.info.kind {
                SonyKind::DualShock4 => dev.set_ds4_led(color),
                _ => dev.set_led(color),
            },
            |_| true,
        )
    }

    /// CHIAKI_EVENT_PLAYER_INDEX-Port.
    pub fn set_player_lights(
        &self,
        device: Option<&DeviceId>,
        player_index: i32,
    ) -> Result<(), NativeError> {
        self.with_device(
            device,
            |dev| match dev.info.kind {
                SonyKind::DualShock4 => Err(NativeError::Unsupported("DS4: keine Player-LEDs")),
                _ => dev.set_player_lights(player_index),
            },
            |dev| dev.info.kind != SonyKind::DualShock4,
        )
    }

    /// DualSense-Intensität (CHIAKI_EVENT_HAPTIC_INTENSITY /
    /// CHIAKI_EVENT_TRIGGER_INTENSITY-Kombination im C++).
    pub fn set_haptic_intensity(
        &self,
        device: Option<&DeviceId>,
        intensity: u8,
    ) -> Result<(), NativeError> {
        self.with_device(
            device,
            |dev| match dev.info.kind {
                SonyKind::DualShock4 => Ok(()),
                _ => dev.set_haptic_intensity(intensity),
            },
            |dev| dev.info.kind != SonyKind::DualShock4,
        )
    }

    /// Effekte zurücksetzen (Session-Abbau wie im C++).
    pub fn clear_effects(&self, device: Option<&DeviceId>) -> Result<(), NativeError> {
        self.with_device(
            device,
            |dev| match dev.info.kind {
                SonyKind::DualShock4 => dev.set_ds4_rumble(0, 0),
                _ => dev.clear_effects(),
            },
            |_| true,
        )
    }

    /// Port von `Controller::resetMotionControls()` (CHIAKI_EVENT_MOTION_RESET).
    pub fn reset_motion(&self, device: Option<&DeviceId>) {
        let mut devices = self.devices.lock().expect("ds devices poisoned");
        for m in devices.iter_mut() {
            if device.is_none_or(|id| &m.device_id == id) {
                m.core.reset_motion();
            }
        }
    }

    /// Aktuellen State eines Geräts.
    pub fn state(&self, device: &DeviceId) -> Option<ControllerState> {
        self.devices
            .lock()
            .ok()?
            .iter()
            .find(|m| &m.device_id == device)
            .map(|m| m.core.state)
    }

    /// Alle nativ verbundenen Geräte.
    pub fn devices(&self) -> Vec<(DeviceId, GamepadDeviceInfo)> {
        self.devices
            .lock()
            .map(|devices| {
                devices
                    .iter()
                    .map(|m| (m.device_id.clone(), m.dev.info.gamepad_info()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl Drop for DualSenseManager {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

/// Monotone Mikrosekunden als u32 (wie die u32-µs-Uhr im C, wrappt bewusst).
fn now_us32() -> u32 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let start = START.get_or_init(std::time::Instant::now);
    start.elapsed().as_micros() as u32
}


// ---------------------------------------------------------------------------
// Haptics-Ausgabe (waveOut) — Port von InitHaptics/ConnectHaptics/
// PushHapticsFrame aus streamsession.cpp
// ---------------------------------------------------------------------------

pub mod haptics {
//! Haptics-PCM-Ausgabe über das 4-Kanal-Audiodevice des DualSense.
//! Siehe Modul-Doku oben (Quelle: streamsession.cpp).


    #[allow(unsafe_code)]
    pub(crate) mod ffi {
        //! Schmale waveOut-Kapselung (windows-crate). Der einzige unsafe-
        //! Bereich der Crate (Konvention: FFI nur in gekapselten sys-Modulen);
        //! alle Funktionen sind nach außen safe.

        use windows::Win32::Media::Audio::{
            waveOutClose, waveOutGetDevCapsW, waveOutGetNumDevs, waveOutOpen,
            waveOutPrepareHeader, waveOutReset, waveOutUnprepareHeader, waveOutWrite, HWAVEOUT,
            WAVEFORMATEX, WAVEHDR, WAVEOUTCAPSW, WAVE_MAPPER, WHDR_DONE,
            MIDI_WAVE_OPEN_TYPE,
        };

        /// Anzahl der waveOut-Geräte.
        pub fn num_devices() -> u32 {
            unsafe { waveOutGetNumDevs() }
        }

        /// Produktname des Geräts (für den "Wireless Controller"-Needle-Vergleich).
        pub fn device_name(index: u32) -> Option<String> {
            let mut caps = WAVEOUTCAPSW::default();
            let res = unsafe {
                waveOutGetDevCapsW(
                    index as usize,
                    &mut caps,
                    std::mem::size_of::<WAVEOUTCAPSW>() as u32,
                )
            };
            if res != 0 {
                return None;
            }
            // WAVEOUTCAPSW ist packed — Feld über addr_of! ohne Referenz kopieren
            let mut name = [0u16; 32];
            #[allow(unsafe_code)]
            unsafe {
                std::ptr::copy_nonoverlapping(
                    std::ptr::addr_of!(caps.szPname),
                    std::ptr::addr_of_mut!(name),
                    1,
                );
            }
            let len = name.iter().position(|&c| c == 0).unwrap_or(0);
            Some(String::from_utf16_lossy(&name[..len]))
        }

        /// zeroed MIDI_WAVE_OPEN_TYPE == CALLBACK_NULL.
        const CALLBACK_NULL: MIDI_WAVE_OPEN_TYPE = MIDI_WAVE_OPEN_TYPE(0);

        /// Handle-Wrapper, weil HWAVEOUT einen Rohpointer enthält und damit
        /// nicht Send ist — wir besitzen das Handle exklusiv im Worker-Thread.
        pub struct SendHandle(HWAVEOUT);
        #[allow(unsafe_code)]
        unsafe impl Send for SendHandle {}

        impl SendHandle {
            pub fn hwo(&self) -> HWAVEOUT {
                self.0
            }
        }

        /// Öffnet ein waveOut-Gerät (CALLBACK_NULL — wir pollen WHDR_DONE).
        /// Fällt das 4-Kanal-Format durch, wird WAVE_MAPPER versucht.
        pub fn open(device_id: u32, format: &WAVEFORMATEX) -> Result<SendHandle, String> {
            let mut hwo = HWAVEOUT::default();
            let mut res =
                unsafe { waveOutOpen(Some(&mut hwo), device_id, format, 0, 0, CALLBACK_NULL) };
            if res != 0 {
                res = unsafe {
                    waveOutOpen(Some(&mut hwo), WAVE_MAPPER, format, 0, 0, CALLBACK_NULL)
                };
                if res != 0 {
                    return Err(format!("waveOutOpen fehlgeschlagen (MMRESULT={res})"));
                }
            }
            Ok(SendHandle(hwo))
        }

        pub fn prepare(hwo: HWAVEOUT, header: &mut WAVEHDR) -> Result<(), String> {
            let res = unsafe {
                waveOutPrepareHeader(hwo, header, std::mem::size_of::<WAVEHDR>() as u32)
            };
            if res != 0 {
                return Err(format!("waveOutPrepareHeader fehlgeschlagen ({res})"));
            }
            Ok(())
        }

        pub fn write(hwo: HWAVEOUT, header: &mut WAVEHDR) -> Result<(), String> {
            let res = unsafe { waveOutWrite(hwo, header, std::mem::size_of::<WAVEHDR>() as u32) };
            if res != 0 {
                return Err(format!("waveOutWrite fehlgeschlagen ({res})"));
            }
            Ok(())
        }

        pub fn unprepare(hwo: HWAVEOUT, header: &mut WAVEHDR) {
            unsafe {
                let _ = waveOutUnprepareHeader(hwo, header, std::mem::size_of::<WAVEHDR>() as u32);
            }
        }

        pub fn reset(hwo: HWAVEOUT) {
            unsafe {
                let _ = waveOutReset(hwo);
            }
        }

        pub fn close(hwo: HWAVEOUT) -> Result<(), String> {
            let res = unsafe { waveOutClose(hwo) };
            if res != 0 {
                return Err(format!("waveOutClose fehlgeschlagen ({res})"));
            }
            Ok(())
        }

        pub fn is_done(header: &WAVEHDR) -> bool {
            header.dwFlags & WHDR_DONE == WHDR_DONE
        }
    }

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc};

    use windows::core::PSTR;
    use windows::Win32::Media::Audio::{WAVEFORMATEX, WAVEHDR, WAVE_FORMAT_PCM};

    use super::{DUALSENSE_AUDIO_DEVICE_NEEDLE, NativeError};

    /// 10 ms @ 3000 Hz Stereo S16 = 120 Byte (haptics-CVT-Kommentar im C++).
    pub const HAPTICS_FRAME_BYTES: usize = 120;
    /// Resample-Faktor 48000/3000.
    const RATE_RATIO: usize = 16;
    /// Ausgabe-Chunk: 120 Byte -> 16x hochgesampelt = 1920 Byte (10 ms @
    /// 48 kHz S16; die Kanalzahl steckt bereits in den 120 Byte drin).
    const OUTPUT_CHUNK_BYTES: usize = HAPTICS_FRAME_BYTES * RATE_RATIO;
    /// Ringpuffer-Header (Warteschlange für ~40 ms).
    const WAVE_BUFFERS: usize = 4;

    /// Haptics-Ausgabe — Port von StreamSession::InitHaptics/ConnectHaptics.
    pub struct HapticsPlayer {
        tx: mpsc::Sender<Vec<u8>>,
        device_name: String,
        frames_pushed: Arc<AtomicUsize>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl HapticsPlayer {
        /// Sucht das DualSense-Audiodevice (Needle wie im C++) und startet
        /// den Ausgabe-Thread. Format: 4ch/48000 Hz/S16 — wie
        /// SDL_OpenAudioDevice im C++.
        pub fn open() -> Result<Self, NativeError> {
            let mut found = None;
            for i in 0..ffi::num_devices() {
                if let Some(name) = ffi::device_name(i) {
                    if name.contains(DUALSENSE_AUDIO_DEVICE_NEEDLE) {
                        found = Some((i, name));
                        break;
                    }
                }
            }
            let (device_id, device_name) =
                found.ok_or(NativeError::Audio(format!(
                    "Kein Audiodevice mit '{DUALSENSE_AUDIO_DEVICE_NEEDLE}' gefunden"
                )))?;

            let format = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_PCM as u16,
                nChannels: 4,
                nSamplesPerSec: 48000,
                wBitsPerSample: 16,
                nBlockAlign: 4 * 2,
                nAvgBytesPerSec: 48000 * 4 * 2,
                cbSize: 0,
            };
            let hwo = ffi::open(device_id, &format)
                .map_err(NativeError::Audio)?;

            let (tx, rx) = mpsc::channel::<Vec<u8>>();
            let frames_pushed = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&frames_pushed);
            let thread = std::thread::Builder::new()
                .name("chiaki-input-haptics".into())
                .spawn(move || {
                    worker(hwo, rx, counter);
                })
                .map_err(|e| NativeError::Audio(e.to_string()))?;

            tracing::info!("Haptics-Ausgabe geöffnet: {device_name}");
            Ok(HapticsPlayer {
                tx,
                device_name,
                frames_pushed,
                thread: Some(thread),
            })
        }

        pub fn device_name(&self) -> &str {
            &self.device_name
        }

        /// Port von `StreamSession::PushHapticsFrame()`: 10-ms-Frame
        /// (Stereo S16 @ 3000 Hz) resampeln und in die Audiowaeschlange.
        pub fn push_haptics_frame(&self, buf: &[u8]) -> Result<(), NativeError> {
            if buf.is_empty() {
                tracing::warn!("Received empty haptics frame");
                return Ok(());
            }
            if !buf.len().is_multiple_of(2 * std::mem::size_of::<i16>()) {
                return Err(NativeError::Audio(format!(
                    "Haptics audio has invalid size: {}",
                    buf.len()
                )));
            }
            // S16LE-Samples lesen (Stereo-PCM, wird wie im C++ als
            // 4-Kanal-Daten behandelt).
            let samples: Vec<i16> = buf
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| i16::from_le_bytes(*c))
                .collect();
            let out = resample_16x(&samples);
            let mut out_bytes = Vec::with_capacity(out.len() * 2);
            for s in out {
                out_bytes.extend_from_slice(&s.to_le_bytes());
            }
            self.tx
                .send(out_bytes)
                .map_err(|e| NativeError::Audio(e.to_string()))
        }

        pub fn frames_pushed(&self) -> usize {
            self.frames_pushed.load(Ordering::Relaxed)
        }

        /// Schließt die Ausgabe (wie DisconnectHaptics).
        pub fn close(mut self) {
            self.tx.send(Vec::new()).ok(); // Stop-Signal (leerer Chunk)
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    impl std::fmt::Debug for HapticsPlayer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("HapticsPlayer")
                .field("device_name", &self.device_name)
                .finish()
        }
    }

    /// Linearer Resampler 16x ( SDL_AudioCVT 3000->48000 nachempfunden) —
    /// jeder "Kanal" wird unabhängig resampelt (das C++ behandelt das
    /// Stereo-PCM ebenfalls als 4-Kanal-Stream).
    fn resample_16x(input: &[i16]) -> Vec<i16> {
        if input.is_empty() {
            return Vec::new();
        }
        let in_frames = input.len();
        let out_frames = in_frames * RATE_RATIO;
        let mut out = Vec::with_capacity(out_frames);
        for i in 0..out_frames {
            let pos = i as f64 * (in_frames - 1) as f64 / out_frames as f64;
            let idx = pos as usize;
            let frac = (pos - idx as f64) as f32;
            let a = input[idx];
            let b = input[(idx + 1).min(in_frames - 1)];
            out.push((a as f32 + (b as f32 - a as f32) * frac).clamp(i16::MIN as f32, i16::MAX as f32) as i16);
        }
        out
    }

    /// Ausgabe-Worker: Ring aus WAVE_BUFFERS vorbereiteten Headern.
    fn worker(hwo: ffi::SendHandle, rx: mpsc::Receiver<Vec<u8>>, counter: Arc<AtomicUsize>) {
        let mut ring: Vec<(Box<WAVEHDR>, Box<[u8]>)> = Vec::new();
        for _ in 0..WAVE_BUFFERS {
            let mut storage = vec![0u8; OUTPUT_CHUNK_BYTES].into_boxed_slice();
            let mut header = Box::new(WAVEHDR {
                lpData: PSTR(storage.as_mut_ptr()),
                dwBufferLength: storage.len() as u32,
                ..Default::default()
            });
            if let Err(e) = ffi::prepare(hwo.hwo(), &mut header) {
                tracing::warn!("Haptics: {e}");
                return;
            }
            ring.push((header, storage));
        }

        let mut next = 0usize;
        while let Ok(chunk) = rx.recv() {
            if chunk.is_empty() {
                break; // Stop-Signal
            }
            let (header, storage) = &mut ring[next % WAVE_BUFFERS];
            // Warten bis der Header frei ist (max ~100 ms, sonst Frame droppen).
            for _ in 0..100 {
                if ffi::is_done(header) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            if !ffi::is_done(header) {
                tracing::warn!("Haptics: Ausgabe hinkt nach, Frame verworfen");
                continue;
            }
            let len = chunk.len().min(storage.len());
            storage[..len].copy_from_slice(&chunk[..len]);
            header.dwBufferLength = len as u32;
            if let Err(e) = ffi::write(hwo.hwo(), header) {
                tracing::warn!("Haptics: {e}");
                break;
            }
            next = next.wrapping_add(1);
            counter.fetch_add(1, Ordering::Relaxed);
        }

        ffi::reset(hwo.hwo());
        for (header, _) in ring.iter_mut() {
            ffi::unprepare(hwo.hwo(), header);
        }
        if let Err(e) = ffi::close(hwo.hwo()) {
            tracing::warn!("Haptics: {e}");
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn resample_16x_length_and_dc() {
            let input = vec![1000i16; 60]; // 30 Stereo-Frames
            let out = resample_16x(&input);
            assert_eq!(out.len(), 60 * 16);
            for v in out.iter().take(16) {
                assert_eq!(*v, 1000, "DC-Signal bleibt konstant");
            }
        }

        #[test]
        fn resample_interpolates() {
            let input = vec![0i16, 1600]; // 2 Samples -> 32 Ausgangssamples
            let out = resample_16x(&input);
            assert_eq!(out.len(), 32);
            assert_eq!(out[0], 0);
            assert_eq!(out[16], 800, "Mitte linear interpoliert");
            assert_eq!(out[31], 1550);
        }
    }
}


// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chiaki_core::controller::BUTTON_L1;

    // --- Output-Report-Bytes (hartcodiert, Quelle: DS5EffectsState_t) ---

    #[test]
    fn rumble_report_old_firmware_halves_strength() {
        let s = Ds5EffectsState::dualsense_rumble(0x0223, 0xFF, 0x40, 0x32);
        assert_eq!(s.data[OFF_ENABLE_BITS1], 0x01 | 0x02, "rumble-emu + audio-haptics aus");
        assert_eq!(s.data[OFF_RUMBLE_LEFT], 0xFF >> 1, "alter FW: >>1");
        assert_eq!(s.data[OFF_RUMBLE_RIGHT], 0x40 >> 1);
        assert_eq!(s.data[OFF_ENABLE_BITS3], 0);
        assert_eq!(s.data[OFF_DUALSENSE_INTENSITY], 0x32);
        assert_eq!(s.data[OFF_ENABLE_BITS2], 0x40);
    }

    #[test]
    fn rumble_report_new_firmware_full_strength() {
        let s = Ds5EffectsState::dualsense_rumble(0x0224, 0xFF, 0x80, 0x00);
        assert_eq!(s.data[OFF_ENABLE_BITS1], 0x02, "nur audio-haptics aus");
        assert_eq!(s.data[OFF_RUMBLE_LEFT], 0xFF, "neue FW: volle Stärke");
        assert_eq!(s.data[OFF_RUMBLE_RIGHT], 0x80);
        assert_eq!(s.data[OFF_ENABLE_BITS3], 0x04);
    }

    #[test]
    fn trigger_effects_report_bytes() {
        let left = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        let right = [0xAA; 10];
        let s = Ds5EffectsState::trigger_effects(0x05, &left, 0x02, &right, 0x0F);
        assert_eq!(s.data[OFF_ENABLE_BITS1], 0x04 | 0x08, "beide Trigger aktiviert");
        assert_eq!(s.data[OFF_ENABLE_BITS2], 0x40);
        assert_eq!(s.data[OFF_DUALSENSE_INTENSITY], 0x0F);
        // [21] = Typ links, [22..32] = Daten links; [10] = Typ rechts
        assert_eq!(s.data[OFF_LEFT_TRIGGER_EFFECT], 0x05);
        assert_eq!(&s.data[OFF_LEFT_TRIGGER_EFFECT + 1..OFF_LEFT_TRIGGER_EFFECT + 11], &left);
        assert_eq!(s.data[OFF_RIGHT_TRIGGER_EFFECT], 0x02);
        assert_eq!(&s.data[OFF_RIGHT_TRIGGER_EFFECT + 1..OFF_RIGHT_TRIGGER_EFFECT + 11], &right);
    }

    #[test]
    fn led_and_player_lights_report_bytes() {
        let led = Ds5EffectsState::led([0x11, 0x22, 0x33]);
        assert_eq!(led.data[OFF_ENABLE_BITS2], 0x04);
        assert_eq!(led.data[OFF_LED_RED], 0x11);
        assert_eq!(led.data[OFF_LED_GREEN], 0x22);
        assert_eq!(led.data[OFF_LED_BLUE], 0x33);

        // SDL SetLightsForPlayerIndex-Tabelle
        let p0 = Ds5EffectsState::player_lights(0);
        assert_eq!(p0.data[OFF_PAD_LIGHTS], 0x04 | 0x20);
        let p3 = Ds5EffectsState::player_lights(3);
        assert_eq!(p3.data[OFF_PAD_LIGHTS], 0x1B | 0x20);
        let off = Ds5EffectsState::player_lights(-1);
        assert_eq!(off.data[OFF_PAD_LIGHTS], 0x00);
        // Wrap wie im C++ (player_index %= 4)
        let p4 = Ds5EffectsState::player_lights(4);
        assert_eq!(p4.data[OFF_PAD_LIGHTS], 0x04 | 0x20);
    }

    #[test]
    fn mic_and_intensity_report_bytes() {
        let on = Ds5EffectsState::mic(true, 0x22);
        assert_eq!(on.data[OFF_ENABLE_BITS2], 0x40 | 0x01 | 0x02);
        assert_eq!(on.data[8], 0x01, "ucMicLightMode");
        assert_eq!(on.data[9], 0x08, "ucAudioMuteBits");
        let off = Ds5EffectsState::mic(false, 0);
        assert_eq!(off.data[8], 0x00);
        assert_eq!(off.data[9], 0x00);

        let intensity = Ds5EffectsState::intensity(0x93);
        assert_eq!(intensity.data[OFF_DUALSENSE_INTENSITY], 0x93);
        assert_eq!(intensity.data[OFF_ENABLE_BITS2], 0x40);
        assert_eq!(intensity.data[OFF_ENABLE_BITS1], 0x02);
    }

    #[test]
    fn crc32_matches_sdl_crc32() {
        // IEEE-Referenzvektor + leerer Input
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(&[]), 0x0000_0000);
        assert_eq!(crc32(b"The quick brown fox jumps over the lazy dog"), 0x414F_A339);
    }

    // --- Input-Report-Parsing (synthetische Reports) ---

    #[test]
    fn dualsense_decode_all_buttons() {
        let mut data = vec![0u8; USB_PACKET_LENGTH];
        data[0] = 0x01;
        {
            let p = &mut data[1..];
            p[0] = 0x40; // LX Mitte-64
            p[3] = 0xC0; // RY Mitte+64
            p[4] = 0x7F; // L2
            p[5] = 0xFF; // R2 voll
            // Buttons0: high nibble: square|cross|circle|triangle = 0xF0;
            // low nibble: hat NE = 1
            p[7] = 0xF0 | 0x01;
            // Buttons1: L1|R1|share|options|L3|R3
            p[8] = 0x01 | 0x02 | 0x10 | 0x20 | 0x40 | 0x80;
            // Buttons2: PS|touchpad
            p[9] = 0x01 | 0x02;
            // Sequenz
            p[11] = 0x78; p[12] = 0x56; p[13] = 0x34; p[14] = 0x12;
        }

        assert_eq!(parse_dualsense_input(&data), Some(Transport::Usb));
        let input = decode_dualsense(&data).expect("decode");
        assert_eq!(input.left_x, 0x40);
        assert_eq!(input.right_y, 0xC0);
        assert_eq!(input.l2, 0x7F);
        assert_eq!(input.r2, 0xFF);
        assert_eq!(input.sequence, 0x12345678);

        let buttons = dualsense_buttons(&input);
        assert_eq!(
            buttons,
            BUTTON_BOX
                | BUTTON_CROSS
                | BUTTON_MOON
                | BUTTON_PYRAMID
                | BUTTON_DPAD_UP
                | BUTTON_DPAD_RIGHT
                | BUTTON_L1
                | BUTTON_R1
                | BUTTON_SHARE
                | BUTTON_OPTIONS
                | BUTTON_L3
                | BUTTON_R3
                | BUTTON_PS
                | BUTTON_TOUCHPAD
        );

        // Sticks wie SDL: (value*257) - 32768 — 0x80 wird dabei zu 128
        // (SDL-Quirk, 1:1 übernommen; der C++-Client sendet denselben Wert)
        assert_eq!(stick_to_i16(0x40) as i32, 0x40i32 * 257 - 32768);
        assert_eq!(stick_to_i16(0x80), 128);
        assert_eq!(stick_to_i16(0xC0) as i32, 0xC0i32 * 257 - 32768);
        assert_eq!(stick_to_i16(0xFF), 32767);
    }

    #[test]
    fn dualsense_bt_report_uses_offset_2() {
        // BT: 0x31, Magic 0x02, dann derselbe Payload
        let mut data = vec![0u8; BT_PACKET_LENGTH];
        data[0] = 0x31;
        data[1] = 0x02;
        data[2] = 0x00; // LX
        data[2 + 4] = 0x11; // L2
        data[2 + 7] = 0x20 | 0x08; // Cross, Hat-Neuter 8
        let input = decode_dualsense(&data).expect("BT decode");
        assert_eq!(parse_dualsense_input(&data), Some(Transport::Bluetooth));
        assert_eq!(input.left_x, 0);
        assert_eq!(input.l2, 0x11);
        assert_eq!(dualsense_buttons(&input), BUTTON_CROSS);
    }

    #[test]
    fn dualsense_imu_units_match_sdl_chain() {
        // accel: raw/8192 -> g; gyro: raw*64/1024 -> deg -> *PI/180 -> rad/s
        let raw = [8192i16, -8192, 16384];
        let accel = imu_values(&raw, true);
        assert_eq!(accel, [1.0, -1.0, 2.0], "8192 LSB = 1g");

        let gyro = imu_values(&[1024, -1024, 2048], false);
        let deg2rad = std::f32::consts::PI / 180.0;
        // SDL-Kette: value*64/1024 deg/s
        assert!((gyro[0] - (1024.0 * 64.0 / 1024.0) * deg2rad).abs() < 1e-6);
        assert!((gyro[1] - (-64.0) * deg2rad).abs() < 1e-6);
        assert!((gyro[2] - (128.0) * deg2rad).abs() < 1e-6);
    }

    #[test]
    fn dualsense_touch_decode_12bit() {
        let mut data = vec![0u8; USB_PACKET_LENGTH];
        data[0] = 0x01;
        {
            let p = &mut data[1..];
            // Finger 0: down, counter 0x05; x/y als 12-bit-Päckchen
            p[32] = 0x05; // high bit clear = down
            p[33] = 0xBC; // x low
            p[34] = 0xCA; // low nibble x high (0xA) + high nibble y low (0xC -> y low nibble = 0xC? siehe unten)
            p[35] = 0x01; // y high
            // x = 0xBC | ((0xCA & 0x0F) << 8) = 0xABC
            // y = (0xCA >> 4) | (0x01 << 4) = 0x1C
            // Finger 2: up
            p[36] = 0x80 | 0x06;
        }
        let input = decode_dualsense(&data).expect("decode");
        assert_eq!(input.touch[0], (true, 0xABC, 0x1C, 0x05));
        assert_eq!(input.touch[1].0, false, "high bit = Finger oben");
    }

    #[test]
    fn dualsense_state_touch_scaling() {
        // y wird von 1070er auf 1079er Skala skaliert (SDL -> chiaki-Kette),
        // x bleibt (1920 -> 1920).
        let mut core = DeviceCore::new(SonyKind::DualSense);
        let mut data = vec![0u8; USB_PACKET_LENGTH];
        data[0] = 0x01;
        {
            let p = &mut data[1..];
            p[32] = 0x00; // Finger 0 down
            p[33] = 0xFF; // x low
            p[34] = 0x77; // x = 0xFF | (0x7 << 8) = 0x7FF, y low nibble = 0x7
            p[35] = 0x40; // y = 0x07 | (0x40 << 4) = 0x407
        }
        assert!(core.apply_report(&data), "erster Report ändert den State");
        assert_eq!(core.state.touches[0].id, 0, "Finger 0 startet Touch");
        // x = 0x7FF (2047) * 1920/1920 = 2047 (Formel-Check),
        // y = 0x407 (1031) * 1079/1070 ≈ 1039
        assert_eq!(core.state.touches[0].x, 0x7FF);
        assert_eq!(
            core.state.touches[0].y,
            (0x407 * 1079 / 1070) as u16
        );
        assert_eq!(core.touch_ids[0], Some(0));

        // Finger hoch -> Touch gestoppt
        let mut data = vec![0u8; USB_PACKET_LENGTH];
        data[0] = 0x01;
        data[1 + 32] = 0x80; // high bit = up
        assert!(core.apply_report(&data));
        assert_eq!(core.state.touches[0].id, -1);
        assert_eq!(core.touch_ids[0], None);
    }

    #[test]
    fn ds4_decode_report() {
        let mut data = vec![0u8; 64];
        data[0] = 0x01;
        {
            let p = &mut data[1..];
            p[0] = 0x80; // LX Mitte
            p[4] = 0x08 | 0x10 | 0x20; // hat none (8) + square + cross
            p[5] = 0x01 | 0x40; // L1 + L3 (SDL-PS4-Layout)
            p[6] = 0x01; // PS
            p[9] = 0x33; // L2 analog
            p[10] = 0x44; // R2 analog
        }
        let input = decode_ds4(&data).expect("ds4 decode");
        let buttons = ds4_buttons(&input);
        assert_eq!(
            buttons,
            BUTTON_BOX | BUTTON_CROSS | BUTTON_L1 | BUTTON_L3 | BUTTON_PS
        );
        assert_eq!(input.l2, 0x33);
        assert_eq!(input.r2, 0x44);
    }

    #[test]
    fn hat_table_roundtrip() {
        // Alle 9 Hut-Positionen verlustfrei über die Chiaki-Bits
        for hat in 0u8..=8 {
            let buttons = hat_to_buttons(hat);
            assert_eq!(buttons_to_hat(buttons), hat, "hat {hat} roundtrip");
        }
        // 8 = keine Richtung
        assert_eq!(hat_to_buttons(8), 0);
    }

    // Hardware-Tests — brauchen echte Geräte, mit `cargo test -- --ignored`
    // ausführen:
    //   DualSense/DualShock4 per USB/Bluetooth verbinden.

    /// Öffnet den ersten gefundenen Sony-Controller und liest 1 s lang
    /// Reports; drücke ein paar Buttons, um StateUpdated zu sehen.
    #[test]
    #[ignore = "braucht echten DualSense/DualShock4 an der Hardware"]
    fn hw_dualsense_read_and_rumble() {
        let manager = DualSenseManager::new().expect("HidApi");
        let events = manager.poll();
        assert!(
            !events.is_empty(),
            "kein Sony-Controller verbunden (Connected-Events erwartet)"
        );
        // Kurzer Rumble zur Verifikation des Output-Paths
        manager.set_rumble(None, 0xFF, 0xFF).expect("rumble");
        std::thread::sleep(std::time::Duration::from_millis(300));
        manager.set_rumble(None, 0, 0).expect("rumble off");

        // 1 s lesen und State-Änderungen zählen
        let mut updates = 0;
        for _ in 0..250 {
            for event in manager.poll() {
                if matches!(event, GamepadEvent::StateUpdated { .. }) {
                    updates += 1;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
        tracing::info!("State-Updates in 1 s: {updates}");
    }

    /// Trigger-Effekte setzen (spürbar: Widerstand an R2/L2).
    #[test]
    #[ignore = "braucht echten DualSense an der Hardware"]
    fn hw_dualsense_trigger_effects() {
        let manager = DualSenseManager::new().expect("HidApi");
        manager.poll();
        // Typ 0x02 = "Rigid" mit Widerstandsstärke 0xFF in den ersten Bytes
        let data_right = [0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        manager
            .set_trigger_effects(None, 0x05, &[0; 10], 0x02, &data_right)
            .expect("trigger effects");
        std::thread::sleep(std::time::Duration::from_secs(2));
        manager.clear_effects(None).expect("clear");
    }

    /// Haptics-Ausgabe über das DualSense-Audiodevice (Lautsprecher­klicks).
    #[test]
    #[ignore = "braucht DualSense-Audiodevice ('Wireless Controller')"]
    fn hw_haptics_audio_output() {
        let player = crate::dualsense::haptics::HapticsPlayer::open().expect("haptics device");
        // 100 ms Haptic-Puls (Sinus 60 Hz voll ausgesteuert)
        for _ in 0..10 {
            let frame: Vec<u8> = (0..60)
                .flat_map(|i| {
                    let v = ((i as f32 / 60.0 * 2.0 * std::f32::consts::PI).sin() * 32767.0) as i16;
                    [v, v] // Stereo (wird wie im C++ als 4ch interpretiert)
                })
                .flat_map(|s| s.to_le_bytes())
                .collect();
            player.push_haptics_frame(&frame).expect("push");
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(player.frames_pushed() >= 10);
        player.close();
    }

    // Hilfs-Stub für State-Tests ohne echtes Gerät ist nicht nötig — die
    // Report-Verarbeitung lebt in DeviceCore (siehe oben).
}
