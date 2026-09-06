//! chiaki-app — Bibliotheksanteil des Binaries (Glue/Lifecycle).
//!
//! Enthält aktuell die Steam-Library-Shortcut-Verwaltung ([`steam`]),
//! einen 1:1-Port von `third-party/cpp-steam-tools` (VDF-Shortcuts-Editor)
//! plus der Parameter-Logik aus `gui/src/qmlbackend.cpp`
//! (`QmlBackend::createSteamShortcut`, "Add to Steam library").

#![deny(unsafe_code)]

pub mod cli;
pub mod steam;

#[cfg(test)]
mod portable_zip_tests {
    //! Trockentests der Portable-Zip-Infrastruktur (scripts/build-portable-zip.ps1).

    const SCRIPT: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../scripts/build-portable-zip.ps1"
    );

    /// Das Script existiert und nennt die vertraglich nötigen Eingaben:
    /// chiaki.exe, die FFmpeg-DLLs (avcodec-61 zieht zusätzlich
    /// swresample-5 nach — dumpbin /DEPENDENTS), libopus-0.dll, den
    /// data/README.txt-Marker und den data-Erhalt bei Wiederholung.
    /// KEIN vfx_sdk (Lizenz, wie im C++).
    #[test]
    fn zip_script_lists_required_inputs() {
        let script = std::fs::read_to_string(SCRIPT).expect("Zip-Script muss existieren");
        for expected in [
            "chiaki.exe",
            "avutil-59.dll",
            "avcodec-61.dll",
            "swscale-8.dll",
            "swresample-5.dll",
            "libopus-0.dll",
            "README.txt",
        ] {
            assert!(script.contains(expected), "Script muss '{expected}' erwähnen");
        }
        assert!(
            script.contains("data"),
            "Script muss den data/-Ordner erhalten (portabler Marker)"
        );
        // Keine VFX-SDK-DLL im Zip (Lizenz, wie im C++) — das Wort darf nur in
        // Doku-Kommentaren vorkommen, in keiner Copy-Zeile.
        let copy_lines: Vec<String> = script
            .lines()
            .filter(|l| l.to_ascii_lowercase().contains("copy-item"))
            .map(|l| l.to_ascii_lowercase())
            .collect();
        assert!(
            !copy_lines.is_empty(),
            "Script muss DLLs per Copy-Item zusammenstellen"
        );
        assert!(
            copy_lines.iter().all(|l| !l.contains("vfx")),
            "VFX-SDK-DLLs gehören wegen der Lizenz NICHT ins Zip (wie im C++)"
        );
    }

    /// Externe Eingaben (Release-exe, FFmpeg-Bin, libopus) — maschinen-
    /// spezifische Pfade, daher nur per `cargo test -p chiaki-app -- --ignored`
    /// vor einem echten Zip-Bau zu prüfen.
    #[test]
    #[ignore = "prüft maschinenspezifische Eingabepfade des Portable-Baus"]
    fn zip_script_external_inputs_present() {
        let script = std::fs::read_to_string(SCRIPT).expect("Zip-Script muss existieren");
        // Param-Defaults aus dem Script ziehen (Zeilen: `$Name = "Pfad",`).
        for line in script.lines().filter(|l| l.contains("= \"F:")) {
            let path = line.trim().trim_start_matches('$').split_once('=').expect("param").1;
            let path = path
                .trim()
                .trim_matches('"')
                .trim_end_matches(',')
                .trim_matches('"');
            assert!(
                std::path::Path::new(path).exists(),
                "Script-Eingabe fehlt: {path}"
            );
        }
    }
}
