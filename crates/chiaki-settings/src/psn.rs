//! PSN-Token-Handling, portiert aus `gui/src/settings.cpp`
//! (`Get/SetPsnAuthToken`, `Get/SetPsnRefreshToken`,
//! `Get/SetPsnAuthTokenExpiry`, `Get/SetPsnAccountId`).
//!
//! Speicherformat exakt wie im C++: alle vier Werte liegen als
//! Strings in der settings.ini unter `settings/psn_*`. Die
//! PSN-Account-ID ist dabei die Base64-Darstellung der 8
//! Account-Bytes (`CHIAKI_PSN_ACCOUNT_ID_SIZE`), z.B.
//! `psn_account_id="eVr/5uFEAHE="` — kein separates JSON-File.

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;

use crate::hosts::CHIAKI_PSN_ACCOUNT_ID_SIZE;
use crate::ini::IniStore;

/// Schlüssel in der settings.ini (identisch zum C++).
pub const KEY_PSN_AUTH_TOKEN: &str = "settings/psn_auth_token";
pub const KEY_PSN_REFRESH_TOKEN: &str = "settings/psn_refresh_token";
pub const KEY_PSN_AUTH_TOKEN_EXPIRY: &str = "settings/psn_auth_token_expiry";
pub const KEY_PSN_ACCOUNT_ID: &str = "settings/psn_account_id";

/// Die vier PSN-bezogenen Settings als Bündel (nur Komfort; gespeichert
/// wird weiterhin 1:1 über die Settings-Keys).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PsnAccountData {
    /// Base64-String der 8 Account-Bytes (exakt so in der INI).
    pub account_id: String,
    pub refresh_token: String,
    pub auth_token: String,
    /// Ablaufdatum als von der PSN-API gelieferter String
    /// (z.B. "2026-09-05 14:27:36 MESZ").
    pub auth_token_expiry: String,
}

impl PsnAccountData {
    pub fn load_from(store: &IniStore) -> Self {
        PsnAccountData {
            account_id: store.string_or(KEY_PSN_ACCOUNT_ID, ""),
            refresh_token: store.string_or(KEY_PSN_REFRESH_TOKEN, ""),
            auth_token: store.string_or(KEY_PSN_AUTH_TOKEN, ""),
            auth_token_expiry: store.string_or(KEY_PSN_AUTH_TOKEN_EXPIRY, ""),
        }
    }

    pub fn save_to(&self, store: &mut IniStore) {
        store.set_value(KEY_PSN_ACCOUNT_ID, crate::ini::Value::Str(self.account_id.clone()));
        store.set_value(KEY_PSN_REFRESH_TOKEN, crate::ini::Value::Str(self.refresh_token.clone()));
        store.set_value(KEY_PSN_AUTH_TOKEN, crate::ini::Value::Str(self.auth_token.clone()));
        store.set_value(KEY_PSN_AUTH_TOKEN_EXPIRY, crate::ini::Value::Str(self.auth_token_expiry.clone()));
    }

    /// Base64-String → die 8 Account-Bytes (für `ChiakiConnectInfo`).
    pub fn account_id_bytes(&self) -> Option<[u8; CHIAKI_PSN_ACCOUNT_ID_SIZE]> {
        account_id_to_bytes(&self.account_id)
    }
}

/// Dekodiert den INI-Wert von `settings/psn_account_id` in die 8
/// Account-Bytes; liefert `None` bei fehlerhafter Länge/Base64.
pub fn account_id_to_bytes(account_id_b64: &str) -> Option<[u8; CHIAKI_PSN_ACCOUNT_ID_SIZE]> {
    let bytes = BASE64_STANDARD.decode(account_id_b64.trim()).ok()?;
    if bytes.len() != CHIAKI_PSN_ACCOUNT_ID_SIZE {
        return None;
    }
    let mut out = [0u8; CHIAKI_PSN_ACCOUNT_ID_SIZE];
    out.copy_from_slice(&bytes);
    Some(out)
}

/// Kodiert die 8 Account-Bytes als Base64 (INIR-Format des C++).
pub fn account_id_to_b64(bytes: &[u8; CHIAKI_PSN_ACCOUNT_ID_SIZE]) -> String {
    BASE64_STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_id_base64_roundtrip() {
        // Dekodierung des echten INI-Werts aus der C++-Referenz-Datei
        let bytes = account_id_to_bytes("eVr/5uFEAHE=").unwrap();
        assert_eq!(bytes, [0x79, 0x5a, 0xff, 0xe6, 0xe1, 0x44, 0x00, 0x71]);
        assert_eq!(account_id_to_b64(&bytes), "eVr/5uFEAHE=");
    }

    #[test]
    fn account_id_rejects_wrong_length() {
        assert_eq!(account_id_to_bytes("AAAA"), None);
        let too_long = BASE64_STANDARD.encode([0u8; 9]);
        assert_eq!(account_id_to_bytes(&too_long), None);
        assert_eq!(account_id_to_bytes("!!!kein base64!!!"), None);
    }

    #[test]
    fn psn_data_ini_roundtrip_matches_cxx_keys() {
        let mut store = IniStore::new();
        let data = PsnAccountData {
            account_id: "eVr/5uFEAHE=".into(),
            refresh_token: "0eba52e5-3900-4742-906e-22ca97b770d4".into(),
            auth_token: "bc0cd1cf-6a21-43e1-af10-a852c8f5480b".into(),
            auth_token_expiry: "2026-09-05 14:27:36 MESZ".into(),
        };
        data.save_to(&mut store);
        // exakt die C++-Keys
        assert!(store.contains("settings/psn_account_id"));
        assert!(store.contains("settings/psn_refresh_token"));
        assert!(store.contains("settings/psn_auth_token"));
        assert!(store.contains("settings/psn_auth_token_expiry"));
        assert_eq!(PsnAccountData::load_from(&store), data);
    }
}
