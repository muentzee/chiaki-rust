// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// chiaki-remote: Remote-Play-über-Internet-Protokollteile des chiaki-ng-Ports.
//
// - [`stun`]:  Port von lib/src/remote/stun.h (STUN-Binding-Requests,
//              Port-Allocation-Test)
// - [`rudp`]:  Port von lib/src/remote/rudp.c (SCE-RUDP-Framing/State-Machine)
// - [`rudpsendbuffer`]: Port von lib/src/remote/rudpsendbuffer.c (Re-Transmits)
// - [`holepunch`]: Port von lib/src/remote/holepunch.c (PSN-Holepunching)
// - [`psn`]:   HTTP-Endpunkte/-Payloads aus holepunch.c gebündelt (ureq)
// - [`psn_auth`]: Port von gui/src/psntoken.cpp + psnaccountid.cpp
//              (PSN-OAuth2: Token-Tausch/Refresh, Account-ID)
//
// Windows-only; blocking I/O (std::thread, kein tokio), kein unsafe.

#![deny(unsafe_code)]

pub mod holepunch;
pub mod psn;
pub mod psn_auth;
pub mod rudp;
pub mod rudpsendbuffer;
pub mod stun;

// Re-Exports (ergonomische Namen für die Haupttypen)
pub use holepunch::{
    Candidate, CandidateType, ConnectionRequest, ConsoleType, DeviceInfo, HolepunchSession,
    PortType, RegistInfo, SessionMessage,
};
pub use psn_auth::{exchange_authorization_code, fetch_psn_account_id, refresh_psn_token, RefreshedPsnToken};
pub use rudp::{Rudp, RudpMessage, RudpPacketType};
pub use rudpsendbuffer::RudpSendBuffer;
pub use stun::{StunServer, STUN_MAGIC_COOKIE};

/// Hexdump im Stil von `chiaki_log_hexdump` (für trace-Logs in rudp/holepunch).
pub(crate) fn hex_dump(buf: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for (i, chunk) in buf.chunks(16).enumerate() {
        let _ = write!(out, "{:08x}  ", i * 16);
        for j in 0..16 {
            match chunk.get(j) {
                Some(b) => {
                    let _ = write!(out, "{:02x} ", b);
                }
                None => out.push_str("   "),
            }
            if j == 7 {
                out.push(' ');
            }
        }
        out.push_str(" |");
        for b in chunk {
            let c = if b.is_ascii_graphic() || *b == b' ' {
                *b as char
            } else {
                '.'
            };
            out.push(c);
        }
        out.push('|');
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_dump_format() {
        let s = hex_dump(&[0x00, 0x01, 0x41, 0x7f, 0x80, 0xff]);
        assert!(s.starts_with("00000000  00 01 41 7f 80 ff "));
        assert!(s.contains("|..A...|"));
    }
}
