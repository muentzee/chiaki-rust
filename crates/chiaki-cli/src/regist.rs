//! `regist`-Subcommand: Registrierung an der Console über
//! `chiaki_core::regist::Regist` (der Live-Fortschritt kommt über die
//! tracing-Logs aus regist.rs), Ausgabe der Credentials am Ende.
//!
//! [`run_regist`] wird auch vom `stream`-Subcommand für den Auto-Regist-Pfad
//! (`--pin` statt `--regist-key`) genutzt.

use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use chiaki_core::error::Target;
use chiaki_core::regist::{
    Regist, RegistCb, RegistEvent, RegistInfo, RegisteredHost,
};
use chiaki_core::ChiakiError;

use crate::util;

/// Argumente von `chiaki-cli regist`.
#[derive(Debug, Clone, clap::Args)]
pub struct RegistArgs {
    /// Console-IP bzw. Hostname
    #[arg(long)]
    pub host: String,
    /// PIN, die die Console anzeigt (4..=8 Ziffern)
    #[arg(long, value_parser = util::parse_pin)]
    pub pin: u32,
    /// Console ist eine PS5 (Default: PS4)
    #[arg(long)]
    pub ps5: bool,
    /// PSN Account ID (Base64 der 8 Account-Bytes) für PS4 >= 7.0 / PS5
    #[arg(long, value_parser = util::parse_psn_account_id)]
    pub psn_account_id: Option<[u8; 8]>,
}

/// Führt einen Regist-Flow synchron aus und liefert bei Erfolg den
/// `RegisteredHost` (Blockiert, bis die Console antwortet oder Ctrl+C).
pub fn run_regist(info: RegistInfo) -> Result<RegisteredHost, ChiakiError> {
    let (tx, rx) = mpsc::channel::<RegistEvent>();
    let cb: RegistCb = Arc::new(move |ev| {
        // Der Thread sendet genau ein Final-Event; ein Send-Fehler bedeutet,
        // dass der Empfänger weg ist — egal, wir beenden danach sowieso.
        let _ = tx.send(ev);
    });

    let regist = Regist::start(info, cb)?;

    let mut stop_sent = false;
    let event = loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(ev) => break Some(ev),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if util::ctrl_c_received() && !stop_sent {
                    stop_sent = true;
                    tracing::info!("Regist canceled");
                    regist.stop();
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break None,
        }
    };

    // Thread joinen (chiaki_regist_fini).
    regist.fini();

    match event {
        Some(RegistEvent::FinishedSuccess(host)) => Ok(*host),
        Some(RegistEvent::FinishedCanceled) => Err(ChiakiError::Canceled),
        Some(RegistEvent::FinishedFailed) => {
            // Der zugrundeliegende Fehler wurde von regist.rs bereits geloggt.
            Err(ChiakiError::Unknown)
        }
        None => Err(ChiakiError::Thread),
    }
}

/// Port von `chiaki-cli regist` (das C-CLI kennt das Kommando nicht; das
/// Verhalten entspricht dem Regist-Dialog des C++-GUI).
pub fn run(args: RegistArgs) -> Result<(), String> {
    let target = if args.ps5 { Target::Ps5_1 } else { Target::Ps4_10 };
    let info = RegistInfo {
        target,
        host: args.host.clone(),
        broadcast: false,
        psn_online_id: None,
        psn_account_id: args.psn_account_id.unwrap_or([0; 8]),
        pin: args.pin,
        console_pin: 0,
    };

    println!(
        "Registering with {} (PIN {}, target {}) ...",
        args.host,
        args.pin,
        rp_version_or_unknown(target)
    );

    let host = run_regist(info).map_err(|e| format!("Regist failed: {e}"))?;

    // Credentials ausgeben — die selben Werte, die der C++-Client pro Host
    // in der Registry ablegt (rp_regist_key -> regist_key, rp_key -> morning).
    let regist_key = util::regist_key_string(&host.rp_regist_key);
    let morning_hex = hex::encode(host.rp_key);
    println!();
    println!("Successfully registered with \"{}\".", host.server_nickname);
    println!("  rp-regist-key (regist_key): {regist_key}");
    println!("  rp-key (morning, hex):      {morning_hex}");
    println!();
    println!(
        "Start a stream with:\n  chiaki-cli stream --host {} --regist-key \"{regist_key}\" --morning {morning_hex}{}",
        args.host,
        if args.ps5 { " --ps5" } else { "" }
    );
    Ok(())
}

fn rp_version_or_unknown(target: Target) -> String {
    chiaki_core::session::rp_version_string(target)
        .map(str::to_owned)
        .unwrap_or_else(|| "unknown".to_owned())
}

#[cfg(test)]
mod tests {
    use chiaki_core::base64;
    use chiaki_core::error::ChiakiError;
    use chiaki_core::rpcrypt::{Rpcrypt, RPCRYPT_KEY_SIZE};
    use chiaki_core::regist::{request_payload_format, PSN_ACCOUNT_ID_SIZE};
    use chiaki_core::Target;

    /// request_payload_format: deterministisch, 'A'-Vorbelegung des äußeren
    /// Headers,innerer Header verschlüsselt, Size-Errors wie im C.
    #[test]
    fn regist_payload_format() {
        let ambassador = [0x11u8; RPCRYPT_KEY_SIZE];
        let account_id = [0x42u8; PSN_ACCOUNT_ID_SIZE];

        let run = |pin: u32| {
            let mut buf = [0u8; 0x400];
            let mut crypt = Rpcrypt::new_regist_ps4_pre10(&ambassador, 0);
            let size = request_payload_format(
                Target::Ps4_10,
                &ambassador,
                &mut buf,
                &mut crypt,
                None,
                Some(&account_id),
                pin,
            )
            .unwrap();
            (buf, size, crypt)
        };

        let (buf, size, _) = run(12345678);

        // innerer Header: Client-Type + base64-Account-Id, verschlüsselt ->
        // Gesamtgröße = 0x1e0 (äußerer Header) + innere Länge. Die innere
        // Länge ist deterministisch: "Client-Type: <64 hex>\r\nNp-AccountId: "
        // + 12 Base64-Zeichen + "\r\n".
        let account_id_b64 = base64::encode(&account_id);
        let inner_len = format!("Client-Type: {}\r\nNp-AccountId: {account_id_b64}\r\n",
            "dabfa2ec873de5839bee8d3f4c0239c4282c07c25c6077a2931afcf0adc0d34f").len();
        assert_eq!(size, 0x1e0 + inner_len);

        // Äußerer Header bis auf die Aeropause-Offsets mit 'A' gefüllt.
        let aer_ranges = [0xc7usize..0xc7 + 8, 0x191..0x191 + 8];
        for (i, &b) in buf[..0x1e0].iter().enumerate() {
            if aer_ranges.iter().any(|r| r.contains(&i)) {
                continue;
            }
            assert_eq!(b, b'A', "byte 0x{i:02x} not 'A'");
        }

        // Verschlüsselter innerer Header darf nicht dem Klartext entsprechen.
        let plain = format!("Client-Type: {}\r\nNp-AccountId: {account_id_b64}\r\n",
            "dabfa2ec873de5839bee8d3f4c0239c4282c07c25c6077a2931afcf0adc0d34f");
        assert_ne!(&buf[0x1e0..size], plain.as_bytes());

        // Deterministisch: gleicher Ambassador + PIN -> identischer Payload,
        // andere PIN -> anderer Krypt-State/Payload.
        let (buf2, size2, _) = run(12345678);
        assert_eq!(buf[..size], buf2[..size2]);
        let (buf3, _, _) = run(87654321);
        assert_ne!(buf[..size], buf3[..size]);

        // Zu kleiner Buffer -> BUF_TOO_SMALL, weder Account-Id noch
        // Online-Id -> INVALID_DATA.
        let mut small = [0u8; 0x10];
        let mut crypt = Rpcrypt::new_regist_ps4_pre10(&ambassador, 0);
        assert_eq!(
            request_payload_format(Target::Ps4_10, &ambassador, &mut small, &mut crypt, None, Some(&account_id), 1),
            Err(ChiakiError::BufTooSmall)
        );
        let mut buf = [0u8; 0x400];
        assert_eq!(
            request_payload_format(Target::Ps4_10, &ambassador, &mut buf, &mut crypt, None, None, 1),
            Err(ChiakiError::InvalidData)
        );
    }

    /// PS4-pre10-Pfad (< 10.0): Online-Id wird verwendet, payload baut.
    #[test]
    fn regist_payload_ps4_pre10() {
        let ambassador = [0x22u8; RPCRYPT_KEY_SIZE];
        let mut buf = [0u8; 0x400];
        let mut crypt = Rpcrypt::new_regist_ps4_pre10(&ambassador, 0);
        let size = request_payload_format(
            Target::Ps4_9,
            &ambassador,
            &mut buf,
            &mut crypt,
            Some("someuser"),
            Some(&[0; PSN_ACCOUNT_ID_SIZE]),
            5555,
        )
        .unwrap();
        assert!(size > 0x1e0);
    }

    /// RegistInfo-Konstruktion wie in `run` (Target-Flag-Logik).
    #[test]
    fn regist_info_target() {
        let args_target_ps5 = true;
        let target = if args_target_ps5 { Target::Ps5_1 } else { Target::Ps4_10 };
        assert!(target.is_ps5());
        assert!(!Target::Ps4_10.is_ps5());
    }
}
