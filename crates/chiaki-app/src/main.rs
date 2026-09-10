// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only
//! `chiaki` — Einstiegspunkt des chiaki-ng-Rust-Remaster (Windows-only).
//!
//! Initialisierungsreihenfolge wie `gui/src/main.cpp`:
//! 1. Kommandozeile parsen (`--profile <name>`, `--help` — im C++ erledigt
//!    das QCommandLineParser; Fehler → stderr + Exit 1, wie `process()`).
//! 2. `SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS)` — senkt
//!    Scheduling-Jitter für die AV-kritischen Threads (main.cpp nach
//!    `chiaki_lib_init`). Rückgabewert ist auch im C++ ungeprüft; wir
//!    loggen ihn nur — Priorität ist kein Vorbedingung für Korrektheit.
//!    Kein `chiaki_lib_init`-Gegenstück nötig (Rust-Crates haben keine
//!    globale Init).
//! 3. `chiaki_ui::run(profile)` — blockiert bis zum Fensterschluss.
//!
//! Windows-GUI-Subsystem: Release-Builds werden ohne Konsolenfenster
//! gelinkt (`windows_subsystem = "windows"`) — Pendant zu `add_executable
//! (chiaki WIN32 …)` in gui/CMakeLists.txt. Debug-Builds behalten die
//! Konsole (stderr/--help bei der Entwicklung sichtbar). Abweichung im
//! Release dokumentiert: `--help`/Fehlertexte auf stderr landen (ohne
//! Konsole) im Nirvana — akzeptiert, denn Logs laufen weiterhin in die
//! Logdatei (`data/log/chiaki-ui.log`, chiaki-ui::init_tracing hängt den
//! File-Layer unabhängig vom Subsystem).
//!
//! DPI-Awareness entfällt bewusst: gpui 0.2.2 bettet über sein
//! Default-Feature `windows-manifest` ein Manifest mit
//! `PerMonitorV2`-DPI-Awareness in die exe ein
//! (resources/windows/gpui.manifest.xml); eine Laufzeit-Call wie
//! `SetProcessDpiAwareness` wäre doppelt.
//!
//! Fehlerbehandlung von `run()`: bewusst `std::eprintln` + Exit-Code 1
//! statt GUI-Dialog — schlägt der Start fehl (Settings/Backend/Fenster),
//! existiert die gpui-Infrastruktur ggf. noch gar nicht, und die Logs
//! (`data/log/chiaki-ui.log`) tragen die Details bereits mit tracing.

// Release-exe ohne Konsolenfenster (siehe Moduldoku); nur auf Windows —
// andere Targets kennen das Attribut nicht.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::process::ExitCode;

use chiaki_app::cli::{self, Args, Parsed};

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match cli::parse(&argv) {
        Err(usage) => {
            // Wie QCommandLineParser::process(): Fehler + Hilfe, Exit 1.
            eprintln!("chiaki: {usage}\n\n{}", cli::help_text());
            ExitCode::from(1)
        }
        Ok(Parsed::Help) => {
            print!("{}", cli::help_text());
            ExitCode::SUCCESS
        }
        Ok(Parsed::Run(args)) => real_main(args),
    }
}

fn real_main(args: Args) -> ExitCode {
    #[cfg(windows)]
    init_process_priority();

    // Headless-Virtualcam-Modus (HANDOFF §8/V2): Session + Media-Pipeline
    // ohne gpui-Fenster — Video in die virtuelle Kamera, Ton bleibt lokal.
    if let Some(vcam_host) = args.virtualcam.clone() {
        return match chiaki_app::virtualcam_headless::run(vcam_host, args.profile.clone()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("chiaki --virtualcam: {e}");
                tracing::error!("Headless-Virtualcam fehlgeschlagen: {e}");
                ExitCode::from(1)
            }
        };
    }

    match chiaki_ui::run(args.profile) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Kein GUI-Dialog (Fenster existiert im Fehlerfall noch nicht);
            // Details stehen im Log, falls tracing schon initialisiert war.
            eprintln!("chiaki: fatal: {e}");
            ExitCode::from(1)
        }
    }
}

/// `SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS)` — siehe
/// Moduldokumentation. Fehler werden nur geloggt (wie im C++).
#[cfg(windows)]
fn init_process_priority() {
    use windows::Win32::System::Threading::{
        GetCurrentProcess, SetPriorityClass, HIGH_PRIORITY_CLASS,
    };

    // SAFETY: GetCurrentProcess liefert das Pseudo-Handle des eigenen
    // Prozesses (-1, darf nicht geschlossen werden); SetPriorityClass
    // ändert nur die eigene Scheduling-Priorität. Keine Parameternvarianten.
    let result = unsafe {
        let process = GetCurrentProcess();
        SetPriorityClass(process, HIGH_PRIORITY_CLASS)
    };
    if let Err(e) = result {
        // tracing ist hier (noch) nicht initialisiert → stderr.
        eprintln!("chiaki: SetPriorityClass(HIGH_PRIORITY_CLASS) failed: {e}");
    }
}

#[cfg(not(windows))]
fn init_process_priority() {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rauchtest der Priority-Class-Einrichtung: setzt HIGH_PRIORITY_CLASS
    /// und liest sie zur Kontrolle zurück. Bewusst `#[ignore]` (verändert
    /// den eigenen Prozess, kein hartes Assert — Windows kann die
    /// Priorität je nach Umgebung verweigern) und ohne Assert: Das
    /// Ergebnis wird nur ausgegeben, ein Fehlschlag von SetPriorityClass
    /// ist wie im C++ kein Fehlerpfad.
    #[test]
    #[ignore = "verändert die eigene Prozess-Priorität; Ergebnis nur beobachten"]
    fn priority_class_setup_roundtrip() {
        init_process_priority();

        use windows::Win32::System::Threading::{GetPriorityClass, GetCurrentProcess, HIGH_PRIORITY_CLASS};
        // SAFETY: eigenes Pseudo-Handle, reine Abfrage.
        let current = unsafe { GetPriorityClass(GetCurrentProcess()) };
        println!(
            "GetPriorityClass = {current:#x} (HIGH_PRIORITY_CLASS = {:#x})",
            HIGH_PRIORITY_CLASS.0
        );
    }
}
