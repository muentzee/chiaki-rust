//! chiaki-cli — Headless-Test-CLI des chiaki-rs-Ports (M1-Gate).
//!
//! Subcommands:
//! - `discover`: Discovery-Broadcast im LAN (chiaki-core DiscoveryService)
//! - `regist`:   Registrierung an der Console (chiaki-core Regist) und
//!               Ausgabe der Credentials (rp-regist-key + rp-key)
//! - `stream`:   Session über chiaki-core::session::Session; schreibt die
//!               ersten 100 decodierbaren H.264/H.265-Einheiten in Datei
//!               (M1-Gate), optional mit echtem Decode-Test via chiaki-media.
//!
//! Logging über `tracing` im chiaki-log-Stil (Timestamp, Level, Target);
//! Filter via `RUST_LOG` (Default: info), `-v`/`-vv` erzwingen debug/trace.

mod discover;
mod regist;
mod stream;
mod util;
mod wake;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "chiaki-cli",
    version,
    about = "Headless test CLI for chiaki-rs (PlayStation Remote Play client port)",
    after_help = "Log verbosity: RUST_LOG=... (default info) or -v (debug) / -vv (trace).\n\
                  Examples:\n  \
                  chiaki-cli discover --timeout-ms 1000\n  \
                  chiaki-cli regist --host 192.168.0.42 --pin 12345678 --ps5\n  \
                  chiaki-cli stream --host 192.168.0.42 --pin 12345678 --frames 300 --out-dir .\\capture\n  \
                  chiaki-cli stream --host 192.168.0.42 --regist-key \"...\" --morning <32 hex> --decode-test"
)]
pub struct Cli {
    /// Log-Ausführlichkeit: -v = debug, -vv = trace (überschreibt RUST_LOG)
    #[arg(short = 'v', action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Discover consoles in the LAN (broadcast)
    Discover(discover::DiscoverArgs),
    /// Register this client with a console and print the credentials
    Regist(regist::RegistArgs),
    /// Start a streaming session (M1 gate: writes the first 100 decodable
    /// H.264/H.265 units to stream.h264/stream.h265)
    Stream(stream::StreamArgs),
    /// Wake a console in standby (Discovery wakeup packet)
    Wake(wake::WakeArgs),
}

/// tracing_subscriber im chiaki-log-Stil: Timestamp, Level, Target.
fn init_tracing(verbose: u8) {
    let filter = match verbose {
        0 => std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_owned()),
        1 => "debug".to_owned(),
        _ => "trace".to_owned(),
    };
    let filter = tracing_subscriber::EnvFilter::try_new(filter)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .compact()
        .init();
}

fn main() {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    if let Err(e) = util::install_ctrlc_handler() {
        tracing::warn!("{e}");
    }

    let result = match &cli.command {
        Command::Discover(args) => discover::run(args.clone()),
        Command::Regist(args) => regist::run(args.clone()),
        Command::Stream(args) => stream::run(args.clone()),
        Command::Wake(args) => wake::run(args.clone()),
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    /// Die Hilfe muss sich bauen lassen (fängt Defekte in den Arg-Definitionen).
    #[test]
    fn cli_command_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parse_discover_defaults() {
        let cli = Cli::try_parse_from(["chiaki-cli", "discover"]).unwrap();
        let Command::Discover(args) = &cli.command else {
            panic!("wrong subcommand");
        };
        assert_eq!(args.timeout_ms, 1000);
        assert!(args.broadcast_addrs.is_empty());
    }

    #[test]
    fn parse_discover_custom() {
        let cli = Cli::try_parse_from([
            "chiaki-cli",
            "discover",
            "--timeout-ms",
            "500",
            "--broadcast-addr",
            "192.168.1.255",
            "--broadcast-addr",
            "10.0.0.255",
        ])
        .unwrap();
        let Command::Discover(args) = &cli.command else {
            panic!("wrong subcommand");
        };
        assert_eq!(args.timeout_ms, 500);
        assert_eq!(args.broadcast_addrs, ["192.168.1.255", "10.0.0.255"]);
    }

    #[test]
    fn parse_regist_args() {
        let b64 = chiaki_core::base64::encode(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let cli = Cli::try_parse_from([
            "chiaki-cli",
            "regist",
            "--host",
            "192.168.0.42",
            "--pin",
            "12345678",
            "--ps5",
            "--psn-account-id",
            &b64, // 8 Bytes base64 (mit Padding)
        ])
        .unwrap();
        let Command::Regist(args) = &cli.command else {
            panic!("wrong subcommand");
        };
        assert_eq!(args.host, "192.168.0.42");
        assert_eq!(args.pin, 12345678);
        assert!(args.ps5);
        assert_eq!(args.psn_account_id, Some([1, 2, 3, 4, 5, 6, 7, 8]));
    }

    #[test]
    fn parse_regist_rejects_bad_pin() {
        for pin in ["123", "123456789", "abcdefgh", ""] {
            let result =
                Cli::try_parse_from(["chiaki-cli", "regist", "--host", "h", "--pin", pin]);
            assert!(result.is_err(), "pin '{pin}' should be rejected");
        }
    }

    #[test]
    fn parse_stream_full() {
        let cli = Cli::try_parse_from([
            "chiaki-cli",
            "stream",
            "--host",
            "192.168.0.42",
            "--regist-key",
            "0123456789abcdef",
            "--morning",
            "000102030405060708090a0b0c0d0e0f",
            "--codec",
            "h265",
            "--resolution",
            "1080",
            "--fps",
            "30",
            "--bitrate",
            "20000",
            "--frames",
            "300",
            "--out-dir",
            "capture",
            "--ps5",
            "--pin",
            "4321",
            "--decode-test",
        ])
        .unwrap();
        let Command::Stream(args) = &cli.command else {
            panic!("wrong subcommand");
        };
        assert_eq!(args.host, "192.168.0.42");
        assert_eq!(args.regist_key, Some(*b"0123456789abcdef"));
        assert_eq!(
            args.morning,
            Some([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15])
        );
        assert_eq!(args.codec, stream::CodecArg::H265);
        assert_eq!(args.resolution, stream::ResolutionArg::P1080);
        assert_eq!(args.fps, stream::FpsArg::Fps30);
        assert_eq!(args.bitrate, 20000);
        assert_eq!(args.frames, Some(300));
        assert_eq!(args.out_dir, std::path::PathBuf::from("capture"));
        assert!(args.ps5);
        assert_eq!(args.pin.as_deref(), Some("4321"));
        assert!(args.decode_test);
    }

    #[test]
    fn parse_stream_defaults() {
        let cli = Cli::try_parse_from([
            "chiaki-cli",
            "stream",
            "--host",
            "192.168.0.42",
            "--pin",
            "12345678",
        ])
        .unwrap();
        let Command::Stream(args) = &cli.command else {
            panic!("wrong subcommand");
        };
        assert_eq!(args.codec, stream::CodecArg::H264);
        assert_eq!(args.resolution, stream::ResolutionArg::P720);
        assert_eq!(args.fps, stream::FpsArg::Fps60);
        assert_eq!(args.bitrate, 0);
        assert_eq!(args.frames, None);
        assert_eq!(args.out_dir, std::path::PathBuf::from("."));
        assert!(!args.ps5);
        assert!(!args.decode_test);
    }

    #[test]
    fn parse_stream_regist_key_pads_with_nul() {
        let cli = Cli::try_parse_from([
            "chiaki-cli",
            "stream",
            "--host",
            "h",
            "--regist-key",
            "short",
            "--morning",
            "000102030405060708090a0b0c0d0e0f",
        ])
        .unwrap();
        let Command::Stream(args) = &cli.command else {
            panic!("wrong subcommand");
        };
        let key = args.regist_key.unwrap();
        assert_eq!(&key[..5], b"short");
        assert_eq!(key[5..], [0u8; 11]);
    }

    #[test]
    fn parse_stream_rejects_bad_keys() {
        // regist-key zu lang
        assert!(Cli::try_parse_from([
            "chiaki-cli",
            "stream",
            "--host",
            "h",
            "--regist-key",
            "0123456789abcdef0",
            "--morning",
            "000102030405060708090a0b0c0d0e0f",
        ])
        .is_err());
        // morning keine Hex-Zeichen
        assert!(Cli::try_parse_from([
            "chiaki-cli",
            "stream",
            "--host",
            "h",
            "--regist-key",
            "k",
            "--morning",
            "zz000102030405060708090a0b0c0d0e0f",
        ])
        .is_err());
        // morning zu kurz
        assert!(Cli::try_parse_from([
            "chiaki-cli",
            "stream",
            "--host",
            "h",
            "--regist-key",
            "k",
            "--morning",
            "0000",
        ])
        .is_err());
    }

    #[test]
    fn parse_stream_requires_host() {
        // Die Credential-Auswahl (direkte Keys / Auto-Regist / Registry)
        // passiert in stream::run — --host ist aber schon Pflicht.
        assert!(Cli::try_parse_from(["chiaki-cli", "stream"]).is_err());
    }
}
