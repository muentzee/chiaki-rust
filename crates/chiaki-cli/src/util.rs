//! Hilfsfunktionen der CLI: Key-/PIN-Parsing (als clap-`value_parser`) und
//! die gemeinsame Ctrl+C-Behandlung.

use std::sync::atomic::{AtomicBool, Ordering};

use chiaki_core::base64;

/// Globaler Ctrl+C-Status (wird von `ctrlc::set_handler` gesetzt und von den
/// Kommandos in ihren Warteschleifen geprüft, um sauber herunterzufahren).
static CTRL_C: AtomicBool = AtomicBool::new(false);

/// Installiert den Ctrl+C-Handler (ctrlc-Crate, Windows:
/// `SetConsoleCtrlHandler`). Setzt nur ein Flag — das Herunterfahren (Session
/// stop/join/fini) macht das jeweilige Kommando selbst.
pub fn install_ctrlc_handler() -> Result<(), String> {
    ctrlc::set_handler(|| {
        // Nur der erste Druck meldet sich; weitere setzen still das Flag.
        if !CTRL_C.swap(true, Ordering::SeqCst) {
            tracing::warn!("Ctrl+C received, stopping ...");
        }
    })
    .map_err(|e| format!("failed to install Ctrl+C handler: {e}"))
}

/// Ctrl+C wurde (mindestens einmal) gedrückt.
pub fn ctrl_c_received() -> bool {
    CTRL_C.load(Ordering::SeqCst)
}

/// Validiert einen Regist-PIN (`--pin` der Console): nur Ziffern, 4..=8
/// Stellen (PS5 zeigt 8-stellige PINs, alte PS4 4-stellige).
pub fn parse_pin(s: &str) -> Result<u32, String> {
    validate_pin_str(s)?;
    s.parse::<u32>()
        .map_err(|_| "PIN must be 4 to 8 digits".to_owned())
}

/// Wie [`parse_pin`], behält aber den String (die Session will die
/// Login-PIN als ASCII-Bytes, `set_login_pin`).
pub fn validate_pin_str(s: &str) -> Result<String, String> {
    if !(4..=8).contains(&s.len()) || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err("PIN must be 4 to 8 digits".to_owned());
    }
    Ok(s.to_owned())
}

/// Validiert `--morning` (Session-Morning-Key): 32 Hex-Zeichen = 16 Bytes.
pub fn parse_morning_hex(s: &str) -> Result<[u8; 16], String> {
    let bytes = hex::decode(s).map_err(|e| format!("--morning must be 32 hex chars: {e}"))?;
    bytes.try_into().map_err(|v: Vec<u8>| {
        format!(
            "--morning must be 32 hex chars (16 bytes), got {} bytes",
            v.len()
        )
    })
}

/// Validiert `--regist-key`: UTF-8-String bis 16 Zeichen, der — wie im
/// C-Client (`QByteArray(len, 0)`-Padding in gui/src/main.cpp) — mit `\0`
/// auf CHIAKI_SESSION_AUTH_SIZE aufgefüllt wird.
pub fn parse_regist_key(s: &str) -> Result<[u8; 16], String> {
    let bytes = s.as_bytes();
    if bytes.len() > 16 {
        return Err(format!(
            "--regist-key must be at most 16 chars, got {}",
            bytes.len()
        ));
    }
    let mut out = [0u8; 16];
    out[..bytes.len()].copy_from_slice(bytes);
    Ok(out)
}

/// Validiert `--psn-account-id`: Base64 der 8 Account-Bytes
/// (CHIAKI_PSN_ACCOUNT_ID_SIZE, wie sie die PSN-Webseite liefert).
pub fn parse_psn_account_id(s: &str) -> Result<[u8; 8], String> {
    let bytes = base64::decode(s.as_bytes())
        .map_err(|e| format!("--psn-account-id must be base64: {e}"))?;
    bytes.try_into().map_err(|v: Vec<u8>| {
        format!(
            "--psn-account-id must decode to {} bytes, got {}",
            chiaki_core::regist::PSN_ACCOUNT_ID_SIZE,
            v.len()
        )
    })
}

/// `rp_regist_key`-Bytes als "String" ausgeben (auf `\0` gekürzt) — so
/// legt der C++-Client den Key in der Registry ab und so nimmt die CLI ihn
/// per `--regist-key` wieder an.
pub fn regist_key_string(key: &[u8; 16]) -> String {
    let len = key.iter().position(|&b| b == 0).unwrap_or(key.len());
    String::from_utf8_lossy(&key[..len]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_validation() {
        assert_eq!(parse_pin("12345678"), Ok(12345678));
        assert_eq!(parse_pin("0000"), Ok(0));
        assert!(parse_pin("123").is_err()); // zu kurz
        assert!(parse_pin("123456789").is_err()); // zu lang
        assert!(parse_pin("1234abcd").is_err()); // keine Ziffern
        assert!(parse_pin("").is_err());
        assert!(validate_pin_str("1234").is_ok());
    }

    #[test]
    fn morning_hex_parsing() {
        let m = parse_morning_hex("000102030405060708090a0b0c0d0e0f").unwrap();
        assert_eq!(m, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
        // Groß-/Kleinschreibung egal
        assert!(parse_morning_hex("000102030405060708090A0B0C0D0E0F").is_ok());
        // zu kurz/lang, keine Hex-Zeichen
        assert!(parse_morning_hex("00").is_err());
        assert!(parse_morning_hex("000102030405060708090a0b0c0d0e0f00").is_err());
        assert!(parse_morning_hex("zz000102030405060708090a0b0c0d0e0f").is_err());
    }

    #[test]
    fn regist_key_padding() {
        // Wie der C++-Client: String-Bytes, Rest mit \0 aufgefüllt.
        let k = parse_regist_key("abc").unwrap();
        assert_eq!(&k[..3], b"abc");
        assert_eq!(k[3..], [0u8; 13]);
        assert_eq!(parse_regist_key("0123456789abcdef").unwrap().len(), 16);
        assert!(parse_regist_key("0123456789abcdef0").is_err());
    }

    #[test]
    fn psn_account_id_parsing() {
        // 8 Bytes -> 12 Base64-Zeichen inkl. '='-Padding (chiaki-base64).
        let bytes = [0u8, 159, 146, 79, 0, 1, 2, 3];
        let b64 = chiaki_core::base64::encode(&bytes);
        assert_eq!(parse_psn_account_id(&b64).unwrap(), bytes);
        assert!(parse_psn_account_id("AAAA").is_err()); // zu kurz
        assert!(parse_psn_account_id("!!!!").is_err()); // ungültig
    }

    #[test]
    fn regist_key_string_output() {
        let mut key = *b"0123456789abcdef";
        assert_eq!(regist_key_string(&key), "0123456789abcdef");
        key = *b"abc\0def\0\0\0\0\0\0\0\0\0";
        assert_eq!(regist_key_string(&key), "abc");
        assert_eq!(regist_key_string(&[0u8; 16]), "");
    }
}
