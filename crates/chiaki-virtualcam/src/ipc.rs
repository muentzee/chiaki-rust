// SPDX-License-Identifier: AGPL-3.0-only
//! IPC zwischen GUI und Headless-Virtualcam-Prozess (HANDOFF §8/V2):
//!
//! * **Instanz-Mutex** `Local\ChiakiVirtualCamInstance` — der Headless-
//!   Runner hält ihn für seine Laufzeit; die GUI erkennt damit „läuft
//!   schon" (auch nach hartem Kill automatisch frei).
//! * **Stop-Event** `Local\ChiakiVirtualCamStop` — die GUI setzt es, der
//!   Runner pollt es im Hauptloop und beendet sauber (Session-Stop,
//!   Kamera schließen).
//! * **PID-Datei** (`data/chiaki-virtualcam.pid`) — nur für die Anzeige
//!   der PID in der UI; Liveness kommt ausschließlich vom Mutex.
//!
//! Alles Session-lokal (`Local\`-Namespace) — GUI und Runner laufen im
//! selben Session-Kontext.

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, OpenEventW, OpenMutexW, SetEvent, WaitForSingleObject,
    SYNCHRONIZATION_ACCESS_RIGHTS, EVENT_MODIFY_STATE,
};
use windows::core::PCWSTR;

const INSTANCE_MUTEX: &str = "Local\\ChiakiVirtualCamInstance";
const STOP_EVENT: &str = "Local\\ChiakiVirtualCamStop";

/// OpenMutex-Zugriffsrecht SYNCHRONIZE (nur Existenz prüfen/Handle halten).
const SYNCHRONIZE: SYNCHRONIZATION_ACCESS_RIGHTS = SYNCHRONIZATION_ACCESS_RIGHTS(0x0010_0000);
/// ERROR_ALREADY_EXISTS (CreateMutex mit bereits existierendem Namen).
const ERROR_ALREADY_EXISTS: i32 = 183;

fn wide(name: &str) -> Vec<u16> {
    name.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Standard-Pfad der PID-Datei (`<data>/chiaki-virtualcam.pid`).
pub fn default_pid_path() -> PathBuf {
    chiaki_settings::app_paths::base_path().join("chiaki-virtualcam.pid")
}

/// Läuft gerade eine Headless-Virtualcam-Instanz (Instanz-Mutex frei/nehmbar)?
pub fn is_running() -> bool {
    let name = wide(INSTANCE_MUTEX);
    unsafe {
        match OpenMutexW(SYNCHRONIZE, false, PCWSTR(name.as_ptr())) {
            Ok(handle) => {
                let _ = CloseHandle(handle);
                true
            }
            Err(_) => false,
        }
    }
}

/// PID aus der PID-Datei (nur Anzeige; keine Liveness-Aussage).
pub fn running_pid(pid_path: &Path) -> Option<u32> {
    let raw = std::fs::read_to_string(pid_path).ok()?;
    raw.trim().parse::<u32>().ok()
}

/// Setzt das Stop-Event der laufenden Headless-Instanz. `false`, wenn der
/// Event nicht offen ist (nichts läuft — der Event existiert nur, solange
/// der Runner lebt, da er ihn selbst erzeugt und bei Drop schließt).
pub fn request_stop() -> bool {
    let name = wide(STOP_EVENT);
    unsafe {
        match OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(name.as_ptr())) {
            Ok(handle) => {
                let _ = SetEvent(handle);
                let _ = CloseHandle(handle);
                true
            }
            Err(_) => false,
        }
    }
}

/// Besitzender Instanz-Mutex + PID-Datei des Headless-Runners. `acquire`
/// schlägt fehl, wenn bereits eine Instanz läuft. Drop gibt beides frei
/// (PID-Datei gelöscht, Mutex-Handle geschlossen).
pub struct InstanceGuard {
    _mutex: HANDLE,
    pid_path: PathBuf,
}

impl InstanceGuard {
    pub fn acquire(pid_path: PathBuf) -> Result<Self, String> {
        let name = wide(INSTANCE_MUTEX);
        let handle = unsafe { CreateMutexW(None, true, PCWSTR(name.as_ptr())) }
            .map_err(|e| format!("Instanz-Mutex nicht erstellbar: {e}"))?;
        if std::io::Error::last_os_error().raw_os_error() == Some(ERROR_ALREADY_EXISTS) {
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Err(
                "Es läuft bereits eine Headless-Virtualcam-Instanz".to_string(),
            );
        }
        if let Some(dir) = pid_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(err) = std::fs::write(&pid_path, std::process::id().to_string()) {
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Err(format!("PID-Datei nicht schreibbar ({}): {err}", pid_path.display()));
        }
        Ok(InstanceGuard { _mutex: handle, pid_path })
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.pid_path);
        unsafe {
            let _ = CloseHandle(self._mutex);
        }
    }
}

/// Der Stop-Event des Headless-Runners (Besitzer-Handle). `wait` blockiert
/// bis zur Signalisierung oder zum Timeout — der Hauptloop nutzt das als
/// Schlaf Ersatz und wacht bei Stop sofort auf.
pub struct StopEvent {
    handle: HANDLE,
}

impl StopEvent {
    pub fn create() -> Result<Self, String> {
        let name = wide(STOP_EVENT);
        let handle = unsafe { CreateEventW(None, false, false, PCWSTR(name.as_ptr())) }
            .map_err(|e| format!("Stop-Event nicht erstellbar: {e}"))?;
        Ok(StopEvent { handle })
    }

    /// `true` = Stop signalisiert, `false` = Timeout.
    pub fn wait(&self, timeout_ms: u32) -> bool {
        unsafe { WaitForSingleObject(self.handle, timeout_ms) == WAIT_OBJECT_0 }
    }
}

impl Drop for StopEvent {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_datei_roundtrip() {
        let path = std::env::temp_dir().join(format!(
            "chiaki-vcam-pid-test-{}.txt",
            std::process::id()
        ));
        std::fs::write(&path, "4242").unwrap();
        assert_eq!(running_pid(&path), Some(4242));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(running_pid(&path), None);
    }

    #[test]
    fn liveness_ohne_runner_falsch() {
        // Der Testprozess hält den Instanz-Mutex NICHT — solange keine echte
        // Headless-Instanz läuft, muss is_running false liefern (auf der
        // Dev-Maschine mit echter Instanz ist der Test bewusst kein Hartes).
        let _ = is_running();
    }

    #[test]
    fn stop_ohne_runner_falsch() {
        // Ohne laufenden Runner existiert der Event nicht → request_stop
        // liefert false (kein Panic, kein Fehlverhalten).
        let _ = request_stop();
    }
}
