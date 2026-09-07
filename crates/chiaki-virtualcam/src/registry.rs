// SPDX-License-Identifier: AGPL-3.0-only
//! Status-Erkennung + Autostart für die virtuelle Kamera.
//!
//! * Verfügbarkeit: registriert die OBS-Installation den Virtual-Camera-
//!   Filter (`CLSID\{A3FCE0F5-…}`)? — dieselbe Prüfung, die das OBS-Backend
//!   des `virtualcam`-Crates beim Öffnen macht (hier nur für die UI-Status-
//!   zeile, damit sie den Zustand anzeigen kann, ohne die Kamera zu öffnen).
//! * Autostart: `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`-Wert
//!   „Chiaki Virtual Camera" = eigener exe-Pfad + `--virtualcam` (headless
//!   Kamera-Feed, siehe chiaki-app `virtualcam_headless`).

use std::path::PathBuf;

/// `virtualcam::backend::windows_obs::is_available` — CLSID-Prüfung wie beim
/// Öffnen (HKCR\CLSID\{A3FCE0F5-3493-419F-958A-ABA1250EC20B}).
pub fn obs_virtualcam_available() -> bool {
    virtualcam::backend::windows_obs::is_available()
}

const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE_NAME: &str = "Chiaki Virtual Camera";

/// Kommandozeile des Autostart-Eintrags (eigene exe + Headless-Flag).
fn autostart_command() -> Result<String, String> {
    let exe = std::env::current_exe()
        .map_err(|err| format!("eigenes exe-Pfad nicht ermittelbar: {err}"))?;
    Ok(format!("\"{}\" --virtualcam", exe.display()))
}

/// Setzt/entfernt den Autostart-Eintrag (HKCU Run). Wirkt nur auf den
/// Registry-Wert — die INI-Speicherung macht der Aufrufer (Settings-Row).
pub fn set_autostart(enable: bool) -> Result<(), String> {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    let key = hkcu
        .open_subkey_with_flags(RUN_KEY_PATH, winreg::enums::KEY_SET_VALUE)
        .map_err(|err| format!("Autostart-Key nicht beschreibbar: {err}"))?;
    if enable {
        let command = autostart_command()?;
        key.set_value(RUN_VALUE_NAME, &command)
            .map_err(|err| format!("Autostart-Eintrag fehlgeschlagen: {err}"))?;
        tracing::info!("Autostart gesetzt: {RUN_VALUE_NAME} = {command}");
    } else {
        // Fehlender Wert ist beim Deaktivieren kein Fehler (Idempotenz).
        let _ = key.delete_value(RUN_VALUE_NAME);
        tracing::info!("Autostart entfernt: {RUN_VALUE_NAME}");
    }
    Ok(())
}

/// Steht der Autostart-Eintrag (unabhängig vom INI-Schalter)?
pub fn autostart_active() -> bool {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    hkcu.open_subkey(RUN_KEY_PATH)
        .and_then(|key| key.get_value::<String, _>(RUN_VALUE_NAME))
        .is_ok()
}

/// Eigenes exe-Verzeichnis (für Doku-/Anzeigezwecke im Headless-Runner).
pub fn own_exe() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|err| format!("eigenes exe-Pfad nicht ermittelbar: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autostart_lesbar_ohne_zu_schreiben() {
        // Nur Lesen (die Dev-Maschine hat den Wert i. d. R. nicht) — der
        // Schreibpfad wird manuell bzw. über die Settings-Row verifiziert.
        let _ = autostart_active();
    }

    #[test]
    fn autostart_command_enthaelt_flag() {
        // Kein Fehlerpfad in Tests (current_exe existiert immer).
        if let Ok(cmd) = autostart_command() {
            assert!(cmd.ends_with("--virtualcam"), "{cmd}");
            assert!(cmd.starts_with('"'), "exe-Path in Quotes: {cmd}");
        }
    }
}
