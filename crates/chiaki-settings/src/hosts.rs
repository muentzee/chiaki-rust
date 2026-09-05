//! Port von `gui/include/host.h` + `gui/src/host.cpp`:
//! `HostMAC`, `RegisteredHost`, `HiddenHost`, `ManualHost`, `PsnHost`.
//!
//! Die INI-Serialisierung ist bytekompatibel zur C++-Version
//! (`SaveToSettings`/`LoadFromSettings`): Binärfelder (`server_mac`,
//! `rp_regist_key`, `rp_key`, `registered_mac`) werden als
//! `@ByteArray(...)` gespeichert, genau wie QSettings es tut.

use crate::ini::{IniStore, Value};

/// `CHIAKI_SESSION_AUTH_SIZE` (lib/include/chiaki/session.h)
pub const CHIAKI_SESSION_AUTH_SIZE: usize = 0x10;
/// `CHIAKI_PSN_ACCOUNT_ID_SIZE` (lib/include/chiaki/regist.h)
pub const CHIAKI_PSN_ACCOUNT_ID_SIZE: usize = 8;

/// `ChiakiTarget` (lib/include/chiaki/common.h) — Werte dürfen sich nicht
/// ändern, sie sind protokollrelevant und stehen so in der settings.ini.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(i32)]
pub enum Target {
    #[default]
    Ps4Unknown = 0,
    Ps4Eight = 800,
    Ps4Nine = 900,
    Ps4Ten = 1000,
    Ps5Unknown = 1_000_000,
    Ps5One = 1_000_100,
}

impl Target {
    pub fn from_i32(v: i32) -> Target {
        match v {
            800 => Target::Ps4Eight,
            900 => Target::Ps4Nine,
            1000 => Target::Ps4Ten,
            1_000_000 => Target::Ps5Unknown,
            1_000_100 => Target::Ps5One,
            _ => Target::Ps4Unknown,
        }
    }

    pub fn as_i32(self) -> i32 {
        self as i32
    }

    /// `chiaki_target_is_unknown`
    pub fn is_unknown(self) -> bool {
        matches!(self, Target::Ps5Unknown | Target::Ps4Unknown)
    }

    /// `chiaki_target_is_ps5`
    pub fn is_ps5(self) -> bool {
        self.as_i32() >= Target::Ps5Unknown as i32
    }
}

/// Port von `HostMAC` (gui/include/host.h). Sortierung wie `operator<`:
/// aufsteigend nach dem 48-Bit-Wert (relevant für die QMap-Ordnung, die
/// die Reihenfolge der Array-Einträge in der INI bestimmt).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HostMac([u8; 6]);

impl Default for HostMac {
    fn default() -> Self {
        HostMac([0; 6])
    }
}

impl PartialOrd for HostMac {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HostMac {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.to_value().cmp(&other.0.to_value())
    }
}

trait MacValue {
    fn to_value(&self) -> u64;
}

impl MacValue for [u8; 6] {
    fn to_value(&self) -> u64 {
        ((self[0] as u64) << 0x28)
            | ((self[1] as u64) << 0x20)
            | ((self[2] as u64) << 0x18)
            | ((self[3] as u64) << 0x10)
            | ((self[4] as u64) << 0x8)
            | (self[5] as u64)
    }
}

impl HostMac {
    pub fn new(mac: [u8; 6]) -> Self {
        HostMac(mac)
    }

    /// Aus 6 Bytes; liefert `None` bei falscher Länge (wie der
    /// Längencheck im C++ `LoadFromSettings`).
    pub fn from_slice(mac: &[u8]) -> Option<Self> {
        if mac.len() == 6 {
            Some(HostMac([mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]]))
        } else {
            None
        }
    }

    pub fn mac(&self) -> &[u8; 6] {
        &self.0
    }

    /// `HostMAC::ToString()` — lowercase hex (QByteArray::toHex).
    pub fn to_hex_string(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn value(&self) -> u64 {
        self.0.to_value()
    }
}

/// Port von `RegisteredHost` (gui/include/host.h).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredHost {
    pub target: Target,
    pub ap_ssid: String,
    pub ap_bssid: String,
    pub ap_key: String,
    pub ap_name: String,
    pub server_mac: HostMac,
    pub server_nickname: String,
    /// `rp_regist_key[CHIAKI_SESSION_AUTH_SIZE]` — muss komplett gefüllt
    /// sein (mit \0 gepolstert).
    pub rp_regist_key: [u8; CHIAKI_SESSION_AUTH_SIZE],
    pub rp_key_type: u32,
    pub rp_key: [u8; 0x10],
    pub console_pin: String,
}

impl Default for RegisteredHost {
    fn default() -> Self {
        RegisteredHost {
            target: Target::default(),
            ap_ssid: String::new(),
            ap_bssid: String::new(),
            ap_key: String::new(),
            ap_name: String::new(),
            server_mac: HostMac::default(),
            server_nickname: String::new(),
            rp_regist_key: [0; CHIAKI_SESSION_AUTH_SIZE],
            rp_key_type: 0,
            rp_key: [0; 0x10],
            console_pin: String::new(),
        }
    }
}

impl RegisteredHost {
    /// (Key, Value)-Paare eines Array-Eintrags, exakt wie
    /// `RegisteredHost::SaveToSettings` im C++.
    pub fn save_to_items(&self) -> Vec<(String, Value)> {
        vec![
            ("target".to_string(), Value::Int(self.target.as_i32() as i64)),
            ("ap_ssid".to_string(), Value::Str(self.ap_ssid.clone())),
            ("ap_bssid".to_string(), Value::Str(self.ap_bssid.clone())),
            ("ap_key".to_string(), Value::Str(self.ap_key.clone())),
            ("ap_name".to_string(), Value::Str(self.ap_name.clone())),
            ("server_nickname".to_string(), Value::Str(self.server_nickname.clone())),
            ("server_mac".to_string(), Value::ByteArray(self.server_mac.mac().to_vec())),
            ("rp_regist_key".to_string(), Value::ByteArray(self.rp_regist_key.to_vec())),
            ("rp_key_type".to_string(), Value::UInt(self.rp_key_type as u64)),
            ("rp_key".to_string(), Value::ByteArray(self.rp_key.to_vec())),
            ("console_pin".to_string(), Value::Str(self.console_pin.clone())),
        ]
    }

    /// Array-Eintragsschlüssel `prefix/<n>/...` — die Serialisierung auf
    /// den Store übernehmen die Settings (SaveRegisteredHosts).
    pub fn save_to(&self, store: &mut IniStore, prefix: &str) {
        let p = format!("{prefix}/");
        for (k, v) in self.save_to_items() {
            store.set_value(&format!("{p}{k}"), v);
        }
    }

    /// C++ `RegisteredHost::LoadFromSettings` — Felder mit falscher
    /// Bytelänge bleiben auf ihren Defaults.
    pub fn load_from(store: &IniStore, prefix: &str) -> RegisteredHost {
        let p = format!("{prefix}/");
        let mut r = RegisteredHost::default();
        r.target = Target::from_i32(store.int_or(&format!("{p}target"), 0) as i32);
        r.ap_ssid = store.string_or(&format!("{p}ap_ssid"), "");
        r.ap_bssid = store.string_or(&format!("{p}ap_bssid"), "");
        r.ap_key = store.string_or(&format!("{p}ap_key"), "");
        r.ap_name = store.string_or(&format!("{p}ap_name"), "");
        r.server_nickname = store.string_or(&format!("{p}server_nickname"), "");
        if let Some(mac) = store.byte_array(&format!("{p}server_mac")) {
            if mac.len() == 6 {
                r.server_mac = HostMac::from_slice(&mac).unwrap_or_default();
            }
        }
        if let Some(key) = store.byte_array(&format!("{p}rp_regist_key")) {
            if key.len() == CHIAKI_SESSION_AUTH_SIZE {
                r.rp_regist_key.copy_from_slice(&key);
            }
        }
        r.rp_key_type = store.uint_or(&format!("{p}rp_key_type"), 0) as u32;
        if let Some(key) = store.byte_array(&format!("{p}rp_key")) {
            if key.len() == 0x10 {
                r.rp_key.copy_from_slice(&key);
            }
        }
        r.console_pin = store.string_or(&format!("{p}console_pin"), "");
        r
    }
}

/// Port von `HiddenHost`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HiddenHost {
    pub server_mac: HostMac,
    pub server_nickname: String,
}

impl HiddenHost {
    pub fn new(server_mac: HostMac, server_nickname: String) -> Self {
        HiddenHost { server_mac, server_nickname }
    }

    pub fn save_to(&self, store: &mut IniStore, prefix: &str) {
        let p = format!("{prefix}/");
        store.set_value(&format!("{p}server_nickname"), Value::Str(self.server_nickname.clone()));
        store.set_value(
            &format!("{p}server_mac"),
            Value::ByteArray(self.server_mac.mac().to_vec()),
        );
    }

    pub fn load_from(store: &IniStore, prefix: &str) -> HiddenHost {
        let p = format!("{prefix}/");
        let mut r = HiddenHost::default();
        r.server_nickname = store.string_or(&format!("{p}server_nickname"), "");
        if let Some(mac) = store.byte_array(&format!("{p}server_mac")) {
            if mac.len() == 6 {
                r.server_mac = HostMac::from_slice(&mac).unwrap_or_default();
            }
        }
        r
    }
}

/// Port von `ManualHost`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualHost {
    pub id: i32,
    pub host: String,
    pub registered: bool,
    pub registered_mac: HostMac,
}

impl Default for ManualHost {
    fn default() -> Self {
        ManualHost { id: -1, host: String::new(), registered: false, registered_mac: HostMac::default() }
    }
}

impl ManualHost {
    pub fn new(id: i32, host: String, registered: bool, registered_mac: HostMac) -> Self {
        ManualHost { id, host, registered, registered_mac }
    }

    pub fn save_to(&self, store: &mut IniStore, prefix: &str) {
        let p = format!("{prefix}/");
        store.set_value(&format!("{p}id"), Value::Int(self.id as i64));
        store.set_value(&format!("{p}host"), Value::Str(self.host.clone()));
        store.set_value(&format!("{p}registered"), Value::Bool(self.registered));
        store.set_value(
            &format!("{p}registered_mac"),
            Value::ByteArray(self.registered_mac.mac().to_vec()),
        );
    }

    pub fn load_from(store: &IniStore, prefix: &str) -> ManualHost {
        let p = format!("{prefix}/");
        let mut r = ManualHost::default();
        r.id = store.int_or(&format!("{p}id"), -1) as i32;
        r.host = store.string_or(&format!("{p}host"), "");
        r.registered = store.bool_or(&format!("{p}registered"), false);
        if let Some(mac) = store.byte_array(&format!("{p}registered_mac")) {
            if mac.len() == 6 {
                r.registered_mac = HostMac::from_slice(&mac).unwrap_or_default();
            }
        }
        r
    }
}

/// Port von `PsnHost` (nur im Speicher der GUI, kein Persistenzformat).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PsnHost {
    pub duid: String,
    pub name: String,
    pub ps5: bool,
}

impl PsnHost {
    pub fn new(duid: String, name: String, ps5: bool) -> Self {
        PsnHost { duid, name, ps5 }
    }

    pub fn target(&self) -> Target {
        if self.ps5 { Target::Ps5One } else { Target::Ps4Ten }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            0x30, 0xb0, 0x42, 0x48, 0x7e, 0xc9, 0x26, 0x9a, 0xf2, 0x18, 0x93, 0xf6, 0xb0, 0x55,
            0x64, 0x92,
        ];
        h.console_pin = "0".into();
        h
    }

    #[test]
    fn registered_host_serialization_matches_qsettings_format() {
        let mut store = IniStore::new();
        let host = sample_host();
        host.save_to(&mut store, "registered_hosts/1");

        // Die Serialisierung muss exakt der QSettings-Darstellung entsprechen
        assert_eq!(store.get_raw("registered_hosts/1/server_mac"), Some("@ByteArray(\\xd4\\xf7\\xd5\\x11\\xfa\\x45)"));
        assert_eq!(
            store.get_raw("registered_hosts/1/rp_regist_key"),
            Some("@ByteArray(7c3e91a4\\0\\0\\0\\0\\0\\0\\0\\0)")
        );
        assert_eq!(
            store.get_raw("registered_hosts/1/rp_key"),
            Some("@ByteArray(0\\xb0\\x42H~\\xc9&\\x9a\\xf2\\x18\\x93\\xf6\\xb0Ud\\x92)")
        );
        assert_eq!(store.get_raw("registered_hosts/1/target"), Some("1000100"));
        assert_eq!(store.get_raw("registered_hosts/1/rp_key_type"), Some("2"));

        // Roundtrip
        let loaded = RegisteredHost::load_from(&store, "registered_hosts/1");
        assert_eq!(loaded, host);
    }

    #[test]
    fn registered_host_load_tolerates_wrong_lengths() {
        let mut store = IniStore::new();
        store.set_value("registered_hosts/1/server_mac", Value::ByteArray(vec![1, 2, 3]));
        store.set_value("registered_hosts/1/rp_key", Value::ByteArray(vec![1; 5]));
        let h = RegisteredHost::load_from(&store, "registered_hosts/1");
        assert_eq!(h.server_mac, HostMac::default());
        assert_eq!(h.rp_key, [0u8; 0x10]);
        assert_eq!(h.target, Target::Ps4Unknown);
    }

    #[test]
    fn hostmac_hex_string() {
        let mac = HostMac::new([0xd4, 0xf7, 0xd5, 0x11, 0xfa, 0x45]);
        assert_eq!(mac.to_hex_string(), "aabbccddeeff");
        assert_eq!(HostMac::from_slice(&[1, 2, 3]), None);
    }

    #[test]
    fn target_values_match_lib() {
        assert_eq!(Target::Ps4Unknown.as_i32(), 0);
        assert_eq!(Target::Ps4Eight.as_i32(), 800);
        assert_eq!(Target::Ps4Ten.as_i32(), 1000);
        assert_eq!(Target::Ps5Unknown.as_i32(), 1_000_000);
        assert_eq!(Target::Ps5One.as_i32(), 1_000_100);
        assert!(Target::Ps5One.is_ps5());
        assert!(!Target::Ps4Ten.is_ps5());
        assert_eq!(Target::from_i32(1_000_100), Target::Ps5One);
        assert_eq!(Target::from_i32(4711), Target::Ps4Unknown);
    }

    #[test]
    fn hidden_and_manual_hosts() {
        let hidden = HiddenHost::new(HostMac::new([1, 2, 3, 4, 5, 6]), "Hidden".into());
        let mut store = IniStore::new();
        hidden.save_to(&mut store, "hidden_hosts/1");
        assert_eq!(HiddenHost::load_from(&store, "hidden_hosts/1"), hidden);

        let manual = ManualHost::new(3, "192.168.1.5".into(), true, HostMac::new([9, 8, 7, 6, 5, 4]));
        manual.save_to(&mut store, "manual_hosts/1");
        assert_eq!(ManualHost::load_from(&store, "manual_hosts/1"), manual);
        assert_eq!(ManualHost::load_from(&IniStore::new(), "manual_hosts/1").id, -1);
    }
}
