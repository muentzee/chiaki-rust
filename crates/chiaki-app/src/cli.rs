// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only
//! Kommandozeilen-Parsing des `chiaki`-Binaries (1:1 der relevanten Optionen
//! aus `gui/src/main.cpp`: `--profile` + `--help`).
//!
//! Bewusst schlank gehalten — die C++-GUI kennt zusätzlich die
//! `stream`/`list`-Positionalkommandos und diverse Stream-Flags
//! (`--fullscreen`, `--passcode`, ...); die hängen am noch nicht
//! vollständigen Stream-Pfad und kommen mit den Nachfolge-Agents.
//!
//! Fehlerverhalten wie `QCommandLineParser::process()`: `--help`/`-h` →
//! Hilfetext + Exit 0, unbekannte Optionen/fehlende Werte → Fehlermeldung +
//! Hilfetext auf stderr + Exit 1.

/// Geparste Kommandozeilenargumente.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Args {
    /// `--profile <name>` — Verbindungsprofil (`profiles/<name>.ini` statt
    /// `settings.ini`), für Steam-Shortcut-Launch-Options.
    pub profile: Option<String>,
}

/// Ergebnis des Parsings: App starten oder nur Hilfe ausgeben.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    Run(Args),
    Help,
}

/// Parst die Argumentzeile (ohne `argv[0]`). Fehler = Usage-Fehlermeldung.
pub fn parse(argv: &[String]) -> Result<Parsed, String> {
    let mut args = Args::default();
    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].as_str();
        match arg {
            "--help" | "-h" => return Ok(Parsed::Help),
            "--profile" | "-profile" => {
                // QCommandLineParser kennt --opt value und --opt=value (mit
                // ein oder zwei Strichen); beide Formen werden akzeptiert.
                i += 1;
                let value = argv.get(i).ok_or_else(|| {
                    format!("Option '{arg}' erwartet einen Profilnamen als Wert")
                })?;
                args.profile = Some(value.clone());
            }
            _ if arg.starts_with("--profile=") || arg.starts_with("-profile=") => {
                args.profile = Some(arg.split_once('=').expect("Präfix geprüft").1.to_string());
            }
            _ => {
                return Err(format!("Unbekanntes Argument '{arg}'"));
            }
        }
        i += 1;
    }
    Ok(Parsed::Run(args))
}

/// Hilfetext (`--help`), analog zur Optionstabelle des C++-Parsers.
pub fn help_text() -> String {
    // Keine `\`-Zeilenfortsetzung verwenden — die streicht führenden Whitespace.
    "\
chiaki — Chiaki Remaster (chiaki-ng Rust-Port, Windows)

Usage: chiaki [Optionen]

Optionen:
  --profile <name>  Verbindungsprofil laden (profiles/<name>.ini statt
                    settings.ini); für Steam-Shortcut-Launch-Options.
  -h, --help        Diesen Hilfetext anzeigen und beenden.
"
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_args_runs_with_default_profile() {
        let parsed = parse(&[]).expect("leere Argumentzeile muss parsen");
        assert_eq!(
            parsed,
            Parsed::Run(Args {
                profile: None
            })
        );
    }

    #[test]
    fn profile_with_space_value() {
        let parsed = parse(&argv(&["--profile", "PS5 Wohnzimmer"]))
            .expect("--profile mit Wert muss parsen");
        assert_eq!(
            parsed,
            Parsed::Run(Args {
                profile: Some("PS5 Wohnzimmer".to_string())
            })
        );
    }

    #[test]
    fn profile_equals_form() {
        // QCommandLineParser-Form --profile=name (auch einzelner Strich).
        let parsed = parse(&argv(&["--profile=default"])).expect("--profile=name muss parsen");
        assert_eq!(
            parsed,
            Parsed::Run(Args {
                profile: Some("default".to_string())
            })
        );
        let parsed = parse(&argv(&["-profile=default"])).expect("-profile=name muss parsen");
        assert_eq!(
            parsed,
            Parsed::Run(Args {
                profile: Some("default".to_string())
            })
        );
    }

    #[test]
    fn single_dash_profile_form() {
        let parsed = parse(&argv(&["-profile", "deck"])).expect("-profile <name> muss parsen");
        assert_eq!(
            parsed,
            Parsed::Run(Args {
                profile: Some("deck".to_string())
            })
        );
    }

    #[test]
    fn help_long_and_short() {
        assert_eq!(parse(&argv(&["--help"])), Ok(Parsed::Help));
        assert_eq!(parse(&argv(&["-h"])), Ok(Parsed::Help));
    }

    #[test]
    fn profile_missing_value_is_usage_error() {
        let err = parse(&argv(&["--profile"])).expect_err("fehlender Wert muss fehlschlagen");
        assert!(err.contains("--profile"), "Fehlermeldung muss Option nennen: {err}");
    }

    #[test]
    fn unknown_arg_is_usage_error() {
        // Wie QCommandLineParser::process(): unbekannte Option → Exit 1.
        let err = parse(&argv(&["--fullscreen"])).expect_err("unbekannte Option muss fehlschlagen");
        assert!(err.contains("--fullscreen"), "Fehlermeldung muss Option nennen: {err}");
    }

    #[test]
    fn later_profile_wins_like_qsettings_reopen() {
        // C++: parser.value() nimmt den letzten Vorkommnissen? QCommandLineParser
        // liefert den zuletzt gesetzten Wert — wir spiegeln das Verhalten explizit.
        let parsed = parse(&argv(&["--profile=a", "--profile=b"])).expect("muss parsen");
        assert_eq!(
            parsed,
            Parsed::Run(Args {
                profile: Some("b".to_string())
            })
        );
    }

    #[test]
    fn help_text_mentions_profile() {
        let text = help_text();
        assert!(text.contains("--profile"));
        assert!(text.contains("--help"));
    }
}
