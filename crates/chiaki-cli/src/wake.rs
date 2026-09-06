//! `chiaki-cli wake` — Konsole per Discovery-Wakeup-Paket aufwecken
//! (Port von `DiscoveryManager::SendWakeup` + `chiaki_discovery_wakeup`).

use clap::Parser;

#[derive(Debug, Clone, Parser)]
pub struct WakeArgs {
    /// Console-IP bzw. Hostname
    #[arg(long)]
    pub host: String,
    /// RP-Regist-Key des registrierten Hosts (bis 16 Zeichen; nur die
    /// Zeichen vor dem ersten \0 zählen, max. 8 ASCII-Hex-Ziffern).
    /// Enspricht `rp_regist_key` aus der Host-Registry, z. B. "7c3e91a4".
    #[arg(long)]
    pub regist_key: String,
    /// Konsole ist eine PS5 (Default: PS4)
    #[arg(long)]
    pub ps5: bool,
}

/// Port von `wakeup_credential` (GUI) bzw. `SendWakeup` (C++): Key am
/// ersten \0 kappen, max. 8 Zeichen, als Hex-Zahl parsen.
fn wakeup_credential(regist_key: &str) -> Result<u64, String> {
    let key = regist_key.split('\0').next().unwrap_or("");
    if key.is_empty() || key.len() > 8 {
        return Err(format!(
            "regist key must be 1..=8 ASCII hex chars (got {} chars)",
            key.len()
        ));
    }
    u64::from_str_radix(key, 16).map_err(|_| format!("regist key \"{key}\" is not valid hex"))
}

pub fn run(args: WakeArgs) -> Result<(), String> {
    let credential = wakeup_credential(&args.regist_key)?;
    println!(
        "Waking {} (credential {credential:#x}, {}) ...",
        args.host,
        if args.ps5 { "PS5" } else { "PS4" }
    );
    chiaki_core::discovery::wakeup(None, &args.host, credential, args.ps5)
        .map_err(|e| format!("wakeup failed: {e}"))?;
    println!("Wakeup packet sent.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_parse() {
        assert_eq!(wakeup_credential("7c3e91a4").unwrap(), 0x7c3e91a4);
        // \0-Padding der INI-@ByteArray-Form wird ignoriert.
        assert_eq!(wakeup_credential("7c3e91a4\0\0\0\0\0\0\0\0").unwrap(), 0x7c3e91a4);
        assert_eq!(wakeup_credential("1234ABCD").unwrap(), 0x1234abcd);
        assert!(wakeup_credential("1234ABCDE").is_err());
        assert!(wakeup_credential("").is_err());
        assert!(wakeup_credential("zz").is_err());
    }
}
