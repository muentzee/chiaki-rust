//! Port von `gui/src/apppaths.cpp` + `gui/include/apppaths.h`.
//!
//! Die Anwendung ist portabel by default: alles (Settings, Profile,
//! PSN-Token, Logs, Caches) liegt in einem `data`-Ordner neben der exe.
//! Ist dieser Ort nicht beschreibbar (z.B. Installation unter
//! `Program Files`), wird transparent auf `%APPDATA%/Chiaki-Rs`
//! ausgewichen. Es wird nie in die Registry oder versteckte
//! Konfigurationsorte geschrieben.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Basis-Override (für Tests und optionales App-Bootstrap-Flag);
/// wird in `base_path()` vor der automatischen Erkennung geprüft.
static BASE_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);

static BASE_CACHE: OnceLock<PathBuf> = OnceLock::new();

/// C++ `dir_writable()`: erzeugt den Pfad (`mkpath`) und testet per
/// `.write_probe`-Datei, ob er wirklich beschreibbar ist.
fn dir_writable(path: &Path) -> bool {
    if std::fs::create_dir_all(path).is_err() {
        return false;
    }
    let probe = path.join(".write_probe");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            // Datei schließt beim Drop; Danach entfernen.
            drop(std::fs::remove_file(&probe));
            true
        }
        Err(_) => false,
    }
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(|p| p.to_path_buf())
}

/// Fallback: `%APPDATA%/Chiaki-Rs` (Windows). Entspricht dem
/// `QStandardPaths::AppDataLocation` der C++-App; der Name bewusst
/// abweichend zur C++-Installation, damit sich die beiden nicht in die
/// Quere kommen. Fehlt `APPDATA`, wird das Arbeitsverzeichnis benutzt
/// (wie im C++ `QDir::currentPath()`).
fn fallback_app_data() -> PathBuf {
    match std::env::var("APPDATA") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join("Chiaki-Rs"),
        _ => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    }
}

/// Portabler Daten-Basispfad; `data/` neben der exe wenn beschreibbar,
/// sonst der AppData-Fallback. Das Ergebnis wird gecacht (wie das
/// `static const` im C++) — für Tests gibt es [`set_base_override`].
pub fn base_path() -> &'static Path {
    {
        let guard = BASE_OVERRIDE.lock().unwrap();
        if let Some(p) = guard.as_ref() {
            // Override nur beim ersten Zugriff in den Cache übernehmen
            return BASE_CACHE.get_or_init(|| p.clone());
        }
    }
    BASE_CACHE.get_or_init(|| {
        if let Some(exe_dir) = exe_dir() {
            let data_dir = exe_dir.join("data");
            if dir_writable(&data_dir) {
                return data_dir;
            }
        }
        let app_data = fallback_app_data();
        let _ = std::fs::create_dir_all(&app_data);
        app_data
    })
}

/// True, wenn `base_path()` zum `data`-Ordner neben der exe aufgelöst hat.
pub fn is_portable() -> bool {
    match exe_dir() {
        Some(exe_dir) => exe_dir.join("data") == base_path(),
        None => false,
    }
}

/// Setzt den Basispfad für Tests/Bootstrap (überschreibt die Erkennung).
/// Muss vor dem ersten `base_path()`-Aufruf gesetzt werden.
pub fn set_base_override(path: Option<PathBuf>) {
    let mut guard = BASE_OVERRIDE.lock().unwrap();
    *guard = path;
}

/// Erzeugt den Daten-Basispfad, falls er nicht existiert.
pub fn ensure_data_dir() -> std::io::Result<&'static Path> {
    std::fs::create_dir_all(base_path())?;
    Ok(base_path())
}

/// `baseDir()/filename`.
pub fn path_for(filename: &str) -> PathBuf {
    base_path().join(filename)
}

/// Haupt-Settings-Datei, `settings.ini` im Basispfad. Nicht-leere
/// Profilnamen mappen auf `profiles/<sanitisierter Name>.ini`.
pub fn settings_file(profile: &str) -> PathBuf {
    settings_file_in(base_path(), profile)
}

/// Wie [`settings_file`], aber mit explizitem Basispfad (Tests, Tools).
pub fn settings_file_in(base: &Path, profile: &str) -> PathBuf {
    if profile.is_empty() {
        return base.join("settings.ini");
    }
    // Profilnamen für die Verwendung als Dateinamen absichern
    let mut safe = String::with_capacity(profile.len());
    for c in profile.chars() {
        if c.is_ascii_alphanumeric() || c == ' ' || c == '_' || c == '-' {
            safe.push(c);
        } else {
            safe.push('_');
        }
    }
    base.join("profiles").join(format!("{safe}.ini"))
}

/// libplacebo-Renderparameter-Speicher (QSettings-INI).
pub fn placebo_file() -> PathBuf {
    path_for("placebo_render_params.ini")
}

/// Verzeichnis für Session-Logs (`log` im Basispfad).
pub fn log_dir() -> PathBuf {
    let dir = path_for("log");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Verzeichnis für Caches wie den libplacebo-Shader-Cache.
pub fn cache_dir() -> PathBuf {
    let dir = path_for("cache");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writable_probe_matches_cxx_rules() {
        // schreibbarer Ordner → true
        let tmp = std::env::temp_dir().join(format!("chiaki-settings-ap-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(dir_writable(&tmp));
        assert!(tmp.join("data").is_dir() == false);

        // Pfad unterhalb einer Datei → create_dir_all schlägt fehl → false
        let blocker = tmp.join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        assert!(!dir_writable(&blocker.join("data")));

        // Probe-Datei muss entfernt worden sein
        assert!(!tmp.join(".write_probe").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn settings_file_profiles_sanitized() {
        // reine Pfadberechnung — nutzt die echte Basiserkennung
        assert_eq!(settings_file(""), base_path().join("settings.ini"));
        assert_eq!(
            settings_file("Profil 1-A"),
            base_path().join("profiles").join("Profil 1-A.ini")
        );
        assert_eq!(
            settings_file("a/b\\c:d?*e"),
            base_path().join("profiles").join("a_b_c_d__e.ini")
        );
        assert_eq!(placebo_file(), base_path().join("placebo_render_params.ini"));
        assert_eq!(path_for("foo/bar.ini"), base_path().join("foo/bar.ini"));
    }

    #[test]
    fn base_path_is_absolute() {
        assert!(base_path().is_absolute(), "base_path = {:?}", base_path());
    }
}
