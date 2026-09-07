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
    /// `--virtualcam [host]` — headless Virtual-Cam-Modus (HANDOFF §8/V2):
    /// Session + Media-Pipeline ohne gpui/Sink; Video landet in der
    /// virtuellen Kamera, Ton bleibt lokal. Wert = Nickname des registrierten
    /// Hosts (ohne Wert: der erste zugeordnete manuelle Host).
    pub virtualcam: Option<Option<String>>,
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
            "--virtualcam" | "-virtualcam" => {
                // Optionaler Wert: das nächste Argument gehört nur dann dazu,
                // wenn es keine Option ist (`--virtualcam --profile x`).
                let value = match argv.get(i + 1) {
                    Some(v) if !v.starts_with('-') => {
                        i += 1;
                        Some(Some(v.clone()))
                    }
                    _ => Some(None),
                };
                args.virtualcam = value;
            }
            _ if arg.starts_with("--profile=") || arg.starts_with("-profile=") => {
                args.profile = Some(arg.split_once('=').expect("Präfix geprüft").1.to_string());
            }
            _ if arg.starts_with("--virtualcam=") || arg.starts_with("-virtualcam=") => {
                let value = arg.split_once('=').expect("Präfix geprüft").1.to_string();
                args.virtualcam = Some((!value.is_empty()).then_some(value));
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
  --virtualcam [host]
                    Headless-Virtualcam-Modus: streamt zum Host in die
                    virtuelle Kamera (OBS Virtual Camera), Ton bleibt lokal.
                    host = Nickname aus der Host-Registry oder eine IP-
                    Adresse; ohne Angabe wird der erste zugeordnete
                    manuelle Host benutzt.
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
                profile: None,
                virtualcam: None,
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
                profile: Some("PS5 Wohnzimmer".to_string()),
                virtualcam: None,
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
                profile: Some("default".to_string()),
                virtualcam: None,
            })
        );
        let parsed = parse(&argv(&["-profile=default"])).expect("-profile=name muss parsen");
        assert_eq!(
            parsed,
            Parsed::Run(Args {
                profile: Some("default".to_string()),
                virtualcam: None,
            })
        );
    }

    #[test]
    fn single_dash_profile_form() {
        let parsed = parse(&argv(&["-profile", "deck"])).expect("-profile <name> muss parsen");
        assert_eq!(
            parsed,
            Parsed::Run(Args {
                profile: Some("deck".to_string()),
                virtualcam: None,
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
                profile: Some("b".to_string()),
                virtualcam: None,
            })
        );
    }

    #[test]
    fn help_text_mentions_profile() {
        let text = help_text();
        assert!(text.contains("--profile"));
        assert!(text.contains("--help"));
    }

    #[test]
    fn virtualcam_without_host() {
        let parsed = parse(&argv(&["--virtualcam"])).expect("muss parsen");
        assert_eq!(
            parsed,
            Parsed::Run(Args { profile: None, virtualcam: Some(None) })
        );
        // =-Form
        let parsed = parse(&argv(&["--virtualcam="])).expect("muss parsen");
        assert_eq!(
            parsed,
            Parsed::Run(Args { profile: None, virtualcam: Some(None) })
        );
    }

    #[test]
    fn virtualcam_with_host_nickname() {
        let parsed = parse(&argv(&["--virtualcam", "PS5 Wohnzimmer"])).expect("muss parsen");
        assert_eq!(
            parsed,
            Parsed::Run(Args {
                profile: None,
                virtualcam: Some(Some("PS5 Wohnzimmer".to_string())),
            })
        );
        // =-Form
        let parsed = parse(&argv(&["-virtualcam=deck"])).expect("muss parsen");
        assert_eq!(
            parsed,
            Parsed::Run(Args {
                profile: None,
                virtualcam: Some(Some("deck".to_string())),
            })
        );
    }

    #[test]
    fn virtualcam_option_not_swallowed_as_value() {
        // Das nächste Argument ist eine Option → gehört NICHT zu --virtualcam.
        let parsed = parse(&argv(&["--virtualcam", "--profile", "deck"])).expect("muss parsen");
        assert_eq!(
            parsed,
            Parsed::Run(Args {
                profile: Some("deck".to_string()),
                virtualcam: Some(None),
            })
        );
    }

    #[test]
    fn help_text_mentions_virtualcam() {
        assert!(help_text().contains("--virtualcam"));
    }
}
