//! Settings-Kategorie „System“ (QML: `SettingsSystem.qml`): Profile,
//! Settings-Backup (Import/Export, libplacebo-Tuning), Logging, Steam,
//! About + Datenordner.

use chiaki_settings::app_paths;

use crate::app::AppShell;

use super::{action_row, inactive, info_row, select_row, toggle_row, Section};

pub(crate) fn sections(
    shell: &mut AppShell,
    _needle: &str,
    _cx: &mut gpui::Context<AppShell>,
) -> Vec<Section> {
    let settings = shell.backend.settings().clone();
    let s = settings.lock().unwrap_or_else(|e| e.into_inner());
    let profiles: Vec<String> = s.profiles().to_vec();
    let current = s.current_profile();
    let sanitize = s.log_sanitize();
    let verbose = s.log_verbose();
    let base = s.paths().base.clone();
    drop(s);

    let mut profiles_section = Section::new("Profiles");
    {
        let mut options = vec![crate::components::SelectOption::new("", "default")];
        for profile in &profiles {
            options.push(crate::components::SelectOption::new(profile.clone(), profile.clone()));
        }
        profiles_section.push(select_row(
            "system-current-profile",
            "Current profile",
            Some("Each profile keeps a complete set of settings in its own file"),
            "profile switch config set",
            true,
            options,
            current.clone(),
            |v, s| {
                // Der Wechsel schreibt current_profile; das Laden der
                // Profil-Datei passiert beim Neustart (Settings::open).
                s.set_current_profile(v.to_string());
            },
        ));
    }
    if !current.is_empty() {
        profiles_section.push(action_row(
            "system-delete-profile",
            "Delete current profile",
            Some(&format!(
                "Removes \u{201C}{current}\u{201D} and returns to the default profile"
            )),
            "profile remove delete",
            true,
            "Delete",
            true,
            |shell, cx| {
                let settings = shell.backend.settings().clone();
                let result = (|| {
                    let mut guard = settings.lock().unwrap_or_else(|e| e.into_inner());
                    let profile = guard.current_profile();
                    if profile.is_empty() {
                        return Ok(());
                    }
                    guard.delete_profile(&profile)
                })();
                let kind = if result.is_ok() {
                    crate::components::ToastKind::Success
                } else {
                    crate::components::ToastKind::Danger
                };
                shell.push_toast(
                    crate::components::ToastData::new(kind, "Profile delete").message(
                        result
                            .err()
                            .map(|e| e.to_string())
                            .unwrap_or_else(|| "Profile deleted.".into()),
                    ),
                    cx,
                );
            },
        ));
    }

    // Backup: ohne Datei-Dialog im Fundament wird in feste Dateien im
    // data-Ordner exportiert/importiert (dokumentierte Abweichung zum
    // QML-Dateidialog).
    let export_path = base.join("settings-export.ini");
    let mut backup = Section::new("Backup");
    backup.push(action_row(
        "system-export",
        "Export settings",
        Some(&format!(
            "Saves all settings to {} (portable data folder)",
            export_path.display()
        )),
        "export backup save file",
        true,
        "Export",
        false,
        move |shell, cx| {
            let settings = shell.backend.settings().clone();
            let path = export_path_display();
            let result = settings
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .export_settings(&path);
            push_result(shell, cx, "Export settings", result);
        },
    ));
    backup.push(action_row(
        "system-import",
        "Import settings",
        Some(&format!(
            "Restores settings from {} if present",
            export_path_display().display()
        )),
        "import restore load file",
        true,
        "Import",
        false,
        move |shell, cx| {
            let settings = shell.backend.settings().clone();
            let path = export_path_display();
            let result = (|| {
                let mut guard = settings.lock().unwrap_or_else(|e| e.into_inner());
                guard.import_settings(&path)?;
                guard.save()
            })();
            push_result(shell, cx, "Import settings", result);
        },
    ));
    backup.push(action_row(
        "system-export-placebo",
        "Export display tuning",
        Some("Exports the libplacebo tuning (placebo options)"),
        "placebo export tuning",
        true,
        "Export",
        false,
        move |shell, cx| {
            let settings = shell.backend.settings().clone();
            let path = placebo_export_path_display();
            let result = settings
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .export_placebo_settings(&path);
            push_result(shell, cx, "Export display tuning", result);
        },
    ));
    backup.push(action_row(
        "system-import-placebo",
        "Import display tuning",
        Some("Imports libplacebo tuning options"),
        "placebo import tuning",
        true,
        "Import",
        false,
        move |shell, cx| {
            let settings = shell.backend.settings().clone();
            let result = (|| {
                let mut guard = settings.lock().unwrap_or_else(|e| e.into_inner());
                guard.import_placebo_settings(&placebo_export_path_display())?;
                guard.save()
            })();
            push_result(shell, cx, "Import display tuning", result);
        },
    ));

    let mut logging = Section::new("Logging");
    logging.push(inactive(toggle_row(
        "system-log-sanitize",
        "Sanitize logs",
        Some("Removes account info and addresses from log files"),
        "sanitize privacy logs",
        true,
        sanitize,
    ), "Log-Sanitizer ist nicht implementiert"));
    logging.push(inactive(toggle_row(
        "system-log-verbose",
        "Verbose logging",
        Some("Debug-level logs \u{2014} only for diagnostics, grows quickly"),
        "verbose debug logs",
        true,
        verbose,
    ), "Log-Level kommt aus der Umgebung (RUST_LOG), nicht aus diesem Key"));

    let mut steam = Section::new("Steam");
    steam.push(inactive(action_row(
        "system-steam-shortcut",
        "Add to Steam library",
        Some("Creates a Steam shortcut that launches chiaki with this profile"),
        "steam shortcut library big picture",
        true,
        "Create\u{2026}",
        false,
        |_shell, cx| {
            // chiaki_app::steam::SteamShortcuts::add_to_library kann hier
            // nicht benutzt werden: chiaki-app hängt von chiaki-ui ab
            // (Binary) — eine Rück-Dependenz wäre ein Cargo-Zyklus. Der
            // Flow gehört daher ins chiaki-app-Binary (TODO(app-agent)).
            super::push_toast(
                cx,
                crate::components::ToastKind::Info,
                "Add to Steam library",
                "Der Steam-Shortcut-Flow (chiaki_app::steam) wird vom \
                 chiaki-app-Binary verdrahtet (Dep-Zyklus chiaki-app → chiaki-ui).",
            );
        },
    ), "Flow wird vom chiaki-app-Binary verdrahtet (Dep-Zyklus)"));

    let mut data = Section::new("Data");
    data.push(action_row(
        "system-open-data",
        "Open data folder",
        Some(&base.display().to_string()),
        "data folder portable files",
        true,
        "Open",
        false,
        move |_shell, _cx| {
            let _ = std::process::Command::new("explorer").arg(&base).spawn();
        },
    ));

    let mut about = Section::new("About");
    about.push(info_row(
        "Chiaki Remaster \u{00B7} version 1.10.0 (Rust). A remaster of chiaki-ng (by Street \
         Pea), itself a fork of Chiaki (by Florian Markl). Licensed under the GNU AGPL v3, \
         without any warranty. UI: GPUI (Rust).",
        "about version license agpl credits gpui",
    ));

    vec![profiles_section, backup, logging, steam, data, about]
}

fn export_path_display() -> std::path::PathBuf {
    app_paths::base_path().join("settings-export.ini")
}

fn placebo_export_path_display() -> std::path::PathBuf {
    app_paths::base_path().join("placebo-export.ini")
}

fn push_result(
    shell: &mut AppShell,
    cx: &mut gpui::Context<AppShell>,
    title: &'static str,
    result: chiaki_settings::settings::Result<()>,
) {
    let kind = if result.is_ok() {
        crate::components::ToastKind::Success
    } else {
        crate::components::ToastKind::Danger
    };
    shell.push_toast(
        crate::components::ToastData::new(kind, title).message(
            result
                .err()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "Done.".into()),
        ),
        cx,
    );
}
