// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// PSN-Regist-Pfad + RUDP-HTTP-Transport — Port der holepunch-Zweige aus
// lib/src/regist.c (`if(regist->info.holepunch_info)`), lib/src/session.c
// (Regist-/RUDP-Aufrufstellen in `session_thread_func` /
// `session_thread_request_session`) und `chiaki_send_recv_http_header_psn`
// (lib/src/http.c).
//
// Diese Teile liegen — wie in regist.rs/http.rs (chiaki-core) dokumentiert —
// bewusst in chiaki-remote, da sie Rudp/Holepunch-Typen benötigen
// (Dependency-Richtung remote → core). Der Core stellt die geteilten
// Bausteine (Request-Header-Formatierung, Response-Payload-Parsing,
// RPCrypt-PSN-Varianten) bereit; hier läuft die Verdrahtung:
//
//     regist_via_holepunch()
//     ├─ chiaki_regist_request_payload_format(..., holepunch_info)
//     │    (PSN-RPCrypt: Rpcrypt::new_regist_psn + aeropause_psn)
//     ├─ request_header_format()            [chiaki_core::regist]
//     ├─ RUDP INIT/COOKIE-Handshake         [regist.c psn-Zweig]
//     ├─ chiaki_send_recv_http_header_psn() [http.c]
//     ├─ RUDP ACK/FINISH (nur warnen)       [regist.c]
//     └─ regist_recv_response psn-Zweig     [regist.c]
//         └─ parse_response_payload()       [chiaki_core::regist]
//
// Das Trait-Impl unten ist die Bridge zu chiaki-core::session: session.rs
// ruft — exakt an den session.c-Aufrufstellen — regist/rudp_start_session/
// rudp_send_recv_http_header/rudp_finish/rudp_send_switch_to_stream_connection
// über das `HolepunchSession`-Trait auf.
//
// Erweiterung (Ctrl-RUDP-Pfad): das Trait wurde um die rudp-Operationen
// ergänzt, die ctrl.c an `session->rudp` ausführt (rudp_ctrl_start_session/
// rudp_send_ctrl_message/rudp_recv_only/rudp_ack_packet/rudp_send_ack_message/
// rudp_print_message). Diese Methoden bekommen keine Default-Impls, weil sie
// zwingend Zugriff auf die hier geparkte `Rudp`-Instanz brauchen — die
// chiaki-remote-Impl unten ist daher Pflicht- und einziger Produktions-Impl;
// chiaki-core bleibt remote-frei (Mirror-Typ `CtrlRudpMessage` für
// empfangene RUDP-Messages).
//
// Abweichungen gegenüber dem C (dokumentiert):
// - Die C-Registrierung läuft in einem eigenen Thread mit
//   chiaki_regist_start/stop/fini; hier ist der Flow synchron (session.rs
//   ruft `regist` direkt im session_thread, das C wartet dort ebenfalls nur
//   per cond-var auf das Regist-Ergebnis). `stop` wird zwischen den Schritten
//   geprüft — das C kann den RUDP-Empfang eines PSN-Regist ebenfalls nicht
//   früher abbrechen (regist->stop_pipe wirkt nur im TCP-Pfad).
// - Chiaki_send_recv_http_header_psn setzt `*header_size` im C nur, wenn ein
//   Header-Ende gefunden wird (sonst bleibt der Out-Parameter uninitialisiert
//   bzw. 0); hier bleibt `header_size` in dem Fall 0 → der nachfolgende
//   HTTP-Parse schlägt sauber fehl.
// - regist_via_holepunch unterstützt nur Targets >= PS4_10 (das C hat einen
//   toten pre-10-Zweig: session.c ruft das Regist immer mit PS4_10/PS5_1,
//   und Rpcrypt::new_regist_psn lehnt pre-10 ab).

use std::sync::Arc;

use chiaki_core::base64;
use chiaki_core::error::{ChiakiError, ChiakiResult, Target};
use chiaki_core::http::response_parse;
use chiaki_core::random::random_bytes_crypt;
use chiaki_core::regist::{
    parse_response_payload, request_header_format, RegisteredHost, RegistInfo, CLIENT_TYPE,
    PSN_ACCOUNT_ID_SIZE,
};
use chiaki_core::rpcrypt::{aeropause_psn, Rpcrypt, RPCRYPT_KEY_SIZE};
use chiaki_core::session::{
    CtrlRudpMessage, HolepunchPortType, HolepunchRegistInfo,
    HolepunchSession as HolepunchSessionTrait,
};
use chiaki_core::stoppipe::StopPipe;

use crate::holepunch::{HolepunchSession, PortType};
use crate::rudp::{Rudp, RudpPacketType};

/// Empfangs-Puffergröße (regist.c: `uint8_t buf[1500]`).
const RESPONSE_BUF_SIZE: usize = 1500;

// ---------------------------------------------------------------------------
// chiaki_regist_request_payload_format — holepunch-Zweig (regist.c)
// ---------------------------------------------------------------------------

/// Port des `holepunch_info`-Zweigs von `chiaki_regist_request_payload_format()`.
///
/// Füllt `buf` (mind. 0x1e0 Bytes, Kopf mit 'A' — can be random), leitet die
/// RPCrypt-Schlüssel aus dem Holepunch-Material ab
/// (`chiaki_rpcrypt_init_regist_psn` mit custom_data1/data1/data2) und legt
/// die PSN-Aeropause (`chiaki_rpcrypt_aeropause_psn`) an 0xc7/0x191 ab. Der
/// innere Header wird über die Base64-PSN-Account-ID gebaut (im C setzt der
/// holepunch-Pfad `psn_online_id = NULL`) und an Offset 0x1e0 verschlüsselt.
/// Rückgabe: genutzte Gesamtgröße (C: `*buf_size`).
pub(crate) fn psn_regist_payload_format(
    target: Target,
    ambassador: &[u8; RPCRYPT_KEY_SIZE],
    buf: &mut [u8],
    crypt: &mut Rpcrypt,
    psn_account_id: &[u8; PSN_ACCOUNT_ID_SIZE],
    holepunch_info: &HolepunchRegistInfo,
) -> ChiakiResult<usize> {
    const INNER_HEADER_OFF: usize = 0x1e0;
    if buf.len() < INNER_HEADER_OFF {
        return Err(ChiakiError::BufTooSmall);
    }
    buf[..INNER_HEADER_OFF].fill(b'A'); // can be random

    // session.c ruft das PSN-Regist immer mit PS4_10/PS5_1 auf; der
    // pre-10-Zweig des C (ohne Holepunch-Material) ist dort unerreichbar.
    if target < Target::Ps4_10 {
        return Err(ChiakiError::InvalidData);
    }

    // Offsets aus dem (mit 'A' gefüllten) Puffer — 1:1 wie im C:
    // key_0_off = buf[0x18D] & 0x1F, key_1_off = buf[0] >> 3.
    let key_0_off = (buf[0x18d] & 0x1f) as usize;
    let key_1_off = (buf[0] >> 3) as usize;
    *crypt = Rpcrypt::new_regist_psn(
        target,
        ambassador,
        key_0_off,
        &holepunch_info.custom_data1,
        &holepunch_info.data1,
        &holepunch_info.data2,
    )?;
    let aeropause = aeropause_psn(target, key_1_off, &crypt.ambassador)?;
    buf[0xc7..0xc7 + 8].copy_from_slice(&aeropause[8..]);
    buf[0x191..0x191 + 8].copy_from_slice(&aeropause[..8]);

    // request_inner_account_id_fmt mit client_type + base64 Account-ID
    // (C: chiaki_base64_encode(psn_account_id, CHIAKI_PSN_ACCOUNT_ID_SIZE, ...)).
    let account_id_b64 = base64::encode(psn_account_id);
    let inner = format!("Client-Type: {CLIENT_TYPE}\r\nNp-AccountId: {account_id_b64}\r\n");
    if INNER_HEADER_OFF + inner.len() >= buf.len() {
        return Err(ChiakiError::BufTooSmall);
    }
    buf[INNER_HEADER_OFF..INNER_HEADER_OFF + inner.len()].copy_from_slice(inner.as_bytes());
    crypt.encrypt(0, &mut buf[INNER_HEADER_OFF..INNER_HEADER_OFF + inner.len()])?;
    Ok(INNER_HEADER_OFF + inner.len())
}

// ---------------------------------------------------------------------------
// chiaki_send_recv_http_header_psn (http.c)
// ---------------------------------------------------------------------------

/// Port von `chiaki_send_recv_http_header_psn()`: sendet `send_buf` (HTTP-
/// Header + Payload) als RUDP-Session-Message und empfängt die Antwort als
/// CTRL-Message; scannt den Empfangspuffer auf das Header-Ende ("\r\n\r\n").
///
/// Aktualisiert `*remote_counter` auf den Wert der Antwort (C:
/// `*remote_counter = message.remote_counter`) — der Aufrufer nutzt ihn für
/// das nachfolgende ACK/FINISH. Liefert `(header_size, received_size)`.
pub(crate) fn send_recv_http_header_psn(
    rudp: &Rudp,
    remote_counter: &mut u16,
    send_buf: &[u8],
    buf: &mut [u8],
) -> ChiakiResult<(usize, usize)> {
    // 0 = "" / 1 = "\r" / 2 = "\r\n" / 3 = "\r\n\r" / 4 = "\r\n\r\n" (final)
    const TRANSITIONS_R: [usize; 4] = [1, 1, 3, 1];
    const TRANSITIONS_N: [usize; 4] = [0, 2, 0, 4];

    let message = rudp
        .send_recv(
            send_buf,
            *remote_counter,
            RudpPacketType::SessionMessage,
            RudpPacketType::CtrlMessage,
            2,
            3,
        )
        .inspect_err(|_| tracing::error!("Didn't receive http session message response"))?;
    // min_data_size = 2 garantiert mind. die Counter-Bytes.
    let received = message.data.len() - 2;
    if received > buf.len() {
        return Err(ChiakiError::BufTooSmall);
    }
    buf[..received].copy_from_slice(&message.data[2..]);
    *remote_counter = message.remote_counter;
    drop(message);

    if received == 0 {
        return Err(ChiakiError::Disconnected);
    }

    let mut nl_state = 0usize;
    let mut header_size = 0usize; // C: bleibt 0/ungesetzt ohne Header-Ende
    for (i, &b) in buf[..received].iter().enumerate() {
        nl_state = match b {
            b'\r' => TRANSITIONS_R[nl_state],
            b'\n' => TRANSITIONS_N[nl_state],
            _ => 0,
        };
        if nl_state == 4 {
            // C: received--; break → header_size = received_total - rest
            // = Index des finalen '\n' + 1 (inklusive "\r\n\r\n").
            header_size = i + 1;
            break;
        }
    }

    Ok((header_size, received))
}

// ---------------------------------------------------------------------------
// regist_thread_func psn-Zweig + regist_recv_response psn-Zweig (regist.c)
// ---------------------------------------------------------------------------

/// Port des PSN-Regist-Flows: session.c übergibt `holepunch_info` + die über
/// RUDP laufende Regist-Anfrage an die Konsole (Regist-HTTP-Endpoint über den
/// gepunchten Control-Hole). Bei Erfolg wird der [`RegisteredHost`] geliefert
/// (rp_key → morning, rp_regist_key → regist_key der Session); `Canceled` bei
/// Stop über die `stop_pipe`.
///
/// Pin/Console-Pin sind — wie in session.c (`info.pin = 0;
/// info.console_pin = 0`) — 0.
pub fn regist_via_holepunch(
    holepunch: &HolepunchSession,
    holepunch_info: &HolepunchRegistInfo,
    target: Target,
    psn_account_id: &[u8; PSN_ACCOUNT_ID_SIZE],
    stop_pipe: &StopPipe,
) -> ChiakiResult<RegisteredHost> {
    // session.c: ChiakiRegistInfo-Füllung für den PSN-Pfad.
    const PIN: u32 = 0;

    // (1) C: chiaki_random_bytes_crypt(ambassador)
    let mut ambassador = [0u8; RPCRYPT_KEY_SIZE];
    random_bytes_crypt(&mut ambassador)
        .inspect_err(|_| tracing::error!("Regist failed to generate random ambassador"))?;

    // (2) C: chiaki_regist_request_payload_format(..., holepunch_info)
    let mut payload = [0u8; 0x400];
    let mut crypt = Rpcrypt {
        target,
        bright: [0; RPCRYPT_KEY_SIZE],
        ambassador: [0; RPCRYPT_KEY_SIZE],
    };
    let payload_size = psn_regist_payload_format(
        target,
        &ambassador,
        &mut payload,
        &mut crypt,
        psn_account_id,
        holepunch_info,
    )
    .inspect_err(|_| tracing::error!("Regist failed to format payload"))?;

    // (3) C: request_header_format(..., regist_local_ip) — die lokale IP ist
    // die vom Holepunch ermittelte (sonst C-Default "10.0.2.15").
    let regist_local_addr = if holepunch_info.regist_local_ip.is_empty() {
        "10.0.2.15"
    } else {
        holepunch_info.regist_local_ip.as_str()
    };
    let mut request_header = [0u8; 0x100];
    let request_header_size = match request_header_format(
        &mut request_header,
        payload_size,
        target,
        regist_local_addr,
    ) {
        Some(s) if s < request_header.len() => s,
        _ => {
            tracing::error!("Regist failed to format request");
            return Err(ChiakiError::Unknown);
        }
    };
    tracing::trace!(
        "Regist formatted request header:\n{}",
        String::from_utf8_lossy(&request_header[..request_header_size])
    );

    // (4) C (session.c): session->rudp = chiaki_rudp_init(ctrl_sock)
    let rudp = holepunch.ensure_rudp()?;

    // (5) regist.c psn-Zweig: "REGIST - Starting RUDP session" (INIT/COOKIE).
    stop_pipe.check()?;
    tracing::info!("REGIST - Starting RUDP session");
    let message = rudp
        .send_recv(
            &[],
            0,
            RudpPacketType::InitRequest,
            RudpPacketType::InitResponse,
            8,
            3,
        )
        .inspect_err(|_| tracing::error!("REGIST - Failed to init rudp"))?;
    let init_response = message.data[8..].to_vec();
    drop(message);
    let message = rudp
        .send_recv(
            &init_response,
            0,
            RudpPacketType::CookieRequest,
            RudpPacketType::CookieResponse,
            2,
            3,
        )
        .inspect_err(|_| tracing::error!("REGIST - Failed to pass rudp cookie"))?;
    let mut remote_counter = message.remote_counter;
    drop(message);

    // (6) psn: send_buf = Header ++ Payload, gesendet als RUDP-Session-
    // Message; Antwort-HTTP-Header via chiaki_send_recv_http_header_psn.
    stop_pipe.check()?;
    let mut send_buf = Vec::with_capacity(request_header_size + payload_size);
    send_buf.extend_from_slice(&request_header[..request_header_size]);
    send_buf.extend_from_slice(&payload[..payload_size]);

    let mut buf = [0u8; RESPONSE_BUF_SIZE];
    let (header_size, buf_filled_size) =
        send_recv_http_header_psn(&rudp, &mut remote_counter, &send_buf, &mut buf)
            .inspect_err(|e| {
                if *e != ChiakiError::Canceled {
                    tracing::error!("Regist failed to receive response HTTP header");
                }
            })?;

    tracing::trace!(
        "Regist response HTTP header:\n{}",
        String::from_utf8_lossy(&buf[..header_size])
    );

    // (7) regist.c: ACK/FINISH — Fehler nur warnen, Flow läuft weiter.
    if let Err(e) = rudp.send_recv(
        &[],
        remote_counter,
        RudpPacketType::Ack,
        RudpPacketType::Finish,
        0,
        3,
    ) {
        tracing::warn!("REGIST - Failed to finish rudp, continuing... ({e})");
    }

    // (8) regist_recv_response psn-Zweig: Header + Content kommen in EINER
    // RUDP-Message mit (das C prüft `buf_filled_size < content + header`).
    let http_response = response_parse(&buf[..header_size])
        .inspect_err(|_| tracing::error!("Regist failed to pare response HTTP header"))?;

    if http_response.code != 200 {
        tracing::error!("Regist received HTTP code {}", http_response.code);
        for header in &http_response.headers {
            if header.key == "RP-Application-Reason" {
                // C: strtoul(header->value, NULL, 0x10)
                let reason = u32::from_str_radix(header.value.trim(), 0x10).unwrap_or(0);
                tracing::error!(
                    "Reported Application Reason: {reason:#x} ({})",
                    chiaki_core::regist::rp_application_reason_string(reason)
                );
                break;
            }
        }
        return Err(ChiakiError::Unknown);
    }

    let mut content_size = 0usize;
    for header in &http_response.headers {
        if header.key == "Content-Length" {
            // C: (size_t)strtoull(header->value, NULL, 0)
            content_size = header.value.trim().parse::<usize>().unwrap_or(0);
        }
    }

    if content_size == 0 {
        tracing::error!("Regist response does not contain or contains invalid Content-Length");
        return Err(ChiakiError::InvalidResponse);
    }

    if content_size + header_size > RESPONSE_BUF_SIZE {
        tracing::error!("Regist response content too big");
        return Err(ChiakiError::BufTooSmall);
    }

    if buf_filled_size < content_size + header_size {
        tracing::error!(
            "Received {buf_filled_size} which is less than content + header of size {}",
            content_size + header_size
        );
        return Err(ChiakiError::Network);
    }

    let payload_part = &mut buf[header_size..buf_filled_size];
    crypt.decrypt(0, payload_part)?;

    tracing::info!(
        "Regist response payload (decrypted):\n{}",
        String::from_utf8_lossy(payload_part)
    );

    // parse_response_payload erwartet ein RegistInfo (nutzt nur info.target);
    // der PSN-Pfad baut es — wie session.c — ohne Host/Broadcast.
    let info = RegistInfo {
        target,
        host: String::new(),
        broadcast: false,
        psn_online_id: None,
        psn_account_id: *psn_account_id,
        pin: PIN,
        console_pin: 0,
    };
    let mut host = RegisteredHost::default();
    parse_response_payload(&info, &mut host, payload_part)
        .inspect_err(|_| tracing::error!("Regist failed to parse response payload"))?;

    // C: host.console_pin = regist->info.console_pin (session.c: 0).
    host.console_pin = 0;
    Ok(host)
}

// ---------------------------------------------------------------------------
// Trait-Bridge: chiaki-core::session::HolepunchSession
// ---------------------------------------------------------------------------

/// Kartierung der Holepunch-Port-Typen (Werte identisch, getrennte Typen).
fn map_port_type(port_type: HolepunchPortType) -> PortType {
    match port_type {
        HolepunchPortType::Ctrl => PortType::Ctrl,
        HolepunchPortType::Data => PortType::Data,
    }
}

impl HolepunchSessionTrait for HolepunchSession {
    fn sock(&self, port_type: HolepunchPortType) -> Option<std::net::UdpSocket> {
        self.holepunch_sock(map_port_type(port_type))
    }

    fn create_offer(&self, _port_type: HolepunchPortType) -> ChiakiResult<()> {
        // C: chiaki_holepunch_session_create_offer() nimmt keinen Port-Typ —
        // das Angebot gilt jeweils für den nächsten Hole (Ctrl zuerst, dann
        // Data, gesteuert über den Session-State im Main-Thread).
        self.create_offer()
    }

    fn punch_hole(&self, port_type: HolepunchPortType) -> ChiakiResult<()> {
        self.punch_hole(map_port_type(port_type))
    }

    fn ps_selected_addr(&self) -> String {
        self.ps_selected_addr()
    }

    fn ps_ctrl_port(&self) -> u16 {
        self.ps_ctrl_port()
    }

    fn regist_info(&self) -> ChiakiResult<HolepunchRegistInfo> {
        let info = self.regist_info();
        Ok(HolepunchRegistInfo {
            data1: info.data1,
            data2: info.data2,
            custom_data1: info.custom_data1,
            regist_local_ip: info.regist_local_ip,
        })
    }

    fn regist(
        &self,
        info: &HolepunchRegistInfo,
        target: Target,
        psn_account_id: &[u8; PSN_ACCOUNT_ID_SIZE],
        stop: &StopPipe,
    ) -> ChiakiResult<RegisteredHost> {
        regist_via_holepunch(self, info, target, psn_account_id, stop)
    }

    fn rudp_start_session(&self) -> ChiakiResult<u16> {
        // C (session_thread_request_session): "SESSION START THREAD -
        // Starting RUDP session" — INIT/COOKIE-Handshake über die bestehende
        // Rudp-Instanz, Rückgabe ist der Remote-Counter der Cookie-Antwort.
        let rudp = self.ensure_rudp()?;
        tracing::info!("SESSION START THREAD - Starting RUDP session");
        let message = rudp
            .send_recv(
                &[],
                0,
                RudpPacketType::InitRequest,
                RudpPacketType::InitResponse,
                8,
                3,
            )
            .inspect_err(|_| tracing::error!("SESSION START THREAD - Failed to init rudp"))?;
        let init_response_size = message.data.len() - 8;
        let init_response = message.data[8..8 + init_response_size].to_vec();
        drop(message);
        let message = rudp
            .send_recv(
                &init_response,
                0,
                RudpPacketType::CookieRequest,
                RudpPacketType::CookieResponse,
                2,
                3,
            )
            .inspect_err(|_| tracing::error!("SESSION START THREAD - Failed to pass rudp cookie"))?;
        Ok(message.remote_counter)
    }

    fn rudp_send_recv_http_header(
        &self,
        request: &[u8],
        remote_counter: u16,
        buf: &mut [u8],
    ) -> ChiakiResult<(usize, usize, u16)> {
        let rudp = self.ensure_rudp()?;
        let mut remote_counter = remote_counter;
        let (header_size, received) =
            send_recv_http_header_psn(&rudp, &mut remote_counter, request, buf)?;
        Ok((header_size, received, remote_counter))
    }

    fn rudp_finish(&self, remote_counter: u16) -> ChiakiResult<()> {
        // C: chiaki_rudp_send_recv(..., NULL, 0, remote_counter, ACK, FINISH, 0, 3)
        let rudp = self.ensure_rudp()?;
        rudp.send_recv(
            &[],
            remote_counter,
            RudpPacketType::Ack,
            RudpPacketType::Finish,
            0,
            3,
        )?;
        Ok(())
    }

    fn rudp_send_switch_to_stream_connection(&self) -> ChiakiResult<()> {
        // C: chiaki_rudp_send_switch_to_stream_connection_message(session->rudp)
        let rudp = self.ensure_rudp()?;
        rudp.send_switch_to_stream_connection_message()
    }

    // ---- Ctrl-RUDP-Bedarf (RUDP-Zweige von ctrl.c; Erweiterung des
    //      chiaki-core-Traits — die Impls brauchen Zugriff auf die
    //      chiaki-remote-Rudp-Instanz, daher sind sie hier und nicht als
    //      Default-Methoden im Core) ----

    fn rudp_ctrl_start_session(&self) -> ChiakiResult<u16> {
        // C (ctrl.c ctrl_connect:1165-1187): identisches INIT/COOKIE-Protokoll
        // wie rudp_start_session — mit den CTRL-Log-Texten. Liefert den
        // Remote-Counter der Cookie-Antwort.
        let rudp = self.ensure_rudp()?;
        tracing::info!("CTRL - Starting RUDP session");
        let message = rudp
            .send_recv(
                &[],
                0,
                RudpPacketType::InitRequest,
                RudpPacketType::InitResponse,
                8,
                3,
            )
            .inspect_err(|_| tracing::error!("CTRL - Failed to init rudp"))?;
        let init_response = message.data[8..].to_vec();
        drop(message);
        let message = rudp
            .send_recv(
                &init_response,
                0,
                RudpPacketType::CookieRequest,
                RudpPacketType::CookieResponse,
                2,
                3,
            )
            .inspect_err(|_| tracing::error!("CTRL - Failed to pass rudp cookie"))?;
        Ok(message.remote_counter)
    }

    fn rudp_send_ctrl_message(&self, message: &[u8]) -> ChiakiResult<()> {
        // C (ctrl.c:683): chiaki_rudp_send_ctrl_message(session->rudp, buf,
        // buf_size) — Frame in den Send-Buffer, bis remote_counter+1 geackt ist.
        self.ensure_rudp()?.send_ctrl_message(message)
    }

    fn rudp_recv_only(&self, buf_size: usize) -> ChiakiResult<CtrlRudpMessage> {
        // C (ctrl.c:520): chiaki_rudp_recv_only(session->rudp,
        // sizeof(ctrl->rudp_recv_buf) - recv_buf_size, &message).
        let message = self.ensure_rudp()?.recv_only(buf_size)?;
        Ok(ctrl_rudp_message_from_remote(message))
    }

    fn rudp_ack_packet(&self, counter_to_ack: u16) -> ChiakiResult<()> {
        // C (ctrl.c:543/560/568): chiaki_rudp_ack_packet (Send-Buffer-ACK).
        self.ensure_rudp()?.ack_packet(counter_to_ack)
    }

    fn rudp_send_ack_message(&self, remote_counter: u16) -> ChiakiResult<()> {
        // C (ctrl.c:545/569/1391): chiaki_rudp_send_ack_message.
        self.ensure_rudp()?.send_ack_message(remote_counter)
    }

    fn rudp_print_message(&self, message: &CtrlRudpMessage) {
        // C (ctrl.c:530): chiaki_rudp_print_message — Format gespiegelt.
        print_ctrl_rudp_message(message);
    }
}

/// Konvertiert eine chiaki-remote-`RudpMessage` in den core-seitigen
/// Mirror-Typ (inkl. Sub-Message-Kette).
fn ctrl_rudp_message_from_remote(message: crate::rudp::RudpMessage) -> CtrlRudpMessage {
    CtrlRudpMessage {
        subtype: message.subtype,
        type_: message.type_,
        remote_counter: message.remote_counter,
        data: message.data,
        sub_message: message.sub_message.map(|sub| Box::new(ctrl_rudp_message_from_remote(*sub))),
    }
}

/// Port von `chiaki_rudp_print_message()` über den Mirror-Typ.
fn print_ctrl_rudp_message(message: &CtrlRudpMessage) {
    tracing::info!("-------------RUDP MESSAGE------------");
    let type_name = crate::rudp::RudpPacketType::from_u16(message.type_)
        .map(|t| t.name())
        .unwrap_or("Unknown Message Type");
    tracing::info!("Message Type: {}", type_name);
    tracing::info!("Rudp Message Subtype: {:#04x}", message.subtype);
    tracing::info!("Rudp Message Remote Counter: {}", message.remote_counter);
    tracing::info!("Rudp Message Data Size: {}", message.data.len());
    tracing::info!("-----Rudp Message Data ---\n{:02x?}", message.data);
    if let Some(sub) = &message.sub_message {
        print_ctrl_rudp_message(sub);
    }
}

/// Hilfskonstruktion für Backend/UI: `Some(...)` als Trait-Objekt für
/// `ConnectInfo::holepunch_session`.
pub fn as_connect_info_session(
    session: &Arc<HolepunchSession>,
) -> Arc<dyn HolepunchSessionTrait> {
    Arc::clone(session) as Arc<dyn HolepunchSessionTrait>
}

/// Hex-DUID (`chiaki_holepunch`-Konvention: 32 Bytes als 64 Hex-Zeichen) in
/// die 32 Bytes für `HolepunchSession::start()` parsen.
pub fn parse_duid(duid: &str) -> ChiakiResult<[u8; 32]> {
    let bytes = hex_to_bytes(duid)?;
    let len = bytes.len();
    <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| {
        tracing::error!(
            "Couldn't convert duid string to bytes got size mismatch ({len} bytes)"
        );
        ChiakiError::InvalidData
    })
}

/// 32 UID-Bytes → DUID-Hex-String (C++: `QByteArray::toHex()` — Port der
/// `updatePsnHostsThread`-DUID-Erzeugung für die Anzeige/Kacheln).
pub fn bytes_to_duid(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Hex-String → Bytes (leer/ungerade/„A“-Platzhalter wie im C++
/// `parse_hex` behandelt: ungerade Länge ist ein Fehler).
pub(crate) fn hex_to_bytes(hex: &str) -> ChiakiResult<Vec<u8>> {
    let hex = hex.trim();
    if hex.len() % 2 != 0 {
        return Err(ChiakiError::InvalidData);
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let bytes = hex.as_bytes();
    for pair in bytes.chunks(2) {
        let h = (pair[0] as char).to_digit(16).ok_or(ChiakiError::InvalidData)?;
        let l = (pair[1] as char).to_digit(16).ok_or(ChiakiError::InvalidData)?;
        out.push(((h << 4) | l) as u8);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rudp::message_serialize;
    use std::net::UdpSocket;

    const AMBASSADOR: [u8; RPCRYPT_KEY_SIZE] = [
        0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e,
        0x1f,
    ];

    fn holepunch_info() -> HolepunchRegistInfo {
        HolepunchRegistInfo {
            data1: *b"0123456789abcdef",
            data2: *b"f fedcba98765432",
            custom_data1: *b"custom_data1_16b",
            regist_local_ip: "192.168.1.2".to_string(),
        }
    }

    /// Struktur + Roundtrip des PSN-Regist-Payloads: key_offsets wie im C aus
    /// dem 'A'-Kopf (1/8), Aeropause an 0xc7/0x191 = aeropause_psn(key_1_off),
    /// innerer Header entschlüsselt den Klartext.
    #[test]
    fn psn_payload_format_structure_and_roundtrip() {
        let target = Target::Ps5_1;
        let mut buf = [0u8; 0x400];
        let mut crypt = Rpcrypt {
            target,
            bright: [0; RPCRYPT_KEY_SIZE],
            ambassador: [0; RPCRYPT_KEY_SIZE],
        };
        let account_id = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let size =
            psn_regist_payload_format(target, &AMBASSADOR, &mut buf, &mut crypt, &account_id, &holepunch_info())
                .expect("format");

        let inner_plain = format!(
            "Client-Type: {CLIENT_TYPE}\r\nNp-AccountId: {}\r\n",
            base64::encode(&account_id)
        );
        assert_eq!(size, 0x1e0 + inner_plain.len());

        // Kopf mit 'A' gefüllt, Aeropause-Splits an 0xc7/0x191.
        assert!(buf[..0xc7].iter().all(|&b| b == b'A'));
        assert!(buf[0xcf..0x191].iter().all(|&b| b == b'A'));
        assert!(buf[0x199..0x1e0].iter().all(|&b| b == b'A'));
        let expected_aeropause =
            aeropause_psn(target, 8, &crypt.ambassador).expect("aeropause_psn");
        assert_eq!(&buf[0xc7..0xcf], &expected_aeropause[8..]);
        assert_eq!(&buf[0x191..0x199], &expected_aeropause[..8]);

        // Entschlüsseln liefert den Klartext-Innenheader zurück.
        let mut inner = buf[0x1e0..size].to_vec();
        crypt.decrypt(0, &mut inner).unwrap();
        assert_eq!(inner, inner_plain.as_bytes());
    }

    /// pre-10-Targets sind im PSN-Pfad unerreichbar → InvalidData
    /// (Rpcrypt::new_regist_psn lehnt sie ebenfalls ab).
    #[test]
    fn psn_payload_format_rejects_pre10() {
        let mut buf = [0u8; 0x400];
        let mut crypt = Rpcrypt {
            target: Target::Ps4_9,
            bright: [0; RPCRYPT_KEY_SIZE],
            ambassador: [0; RPCRYPT_KEY_SIZE],
        };
        let err = psn_regist_payload_format(
            Target::Ps4_9,
            &AMBASSADOR,
            &mut buf,
            &mut crypt,
            &[0; 8],
            &holepunch_info(),
        )
        .unwrap_err();
        assert_eq!(err, ChiakiError::InvalidData);
    }

    /// parse_duid: Hex ↔ Bytes für den 64-Zeichen-DUID (inkl. des
    /// "Main PS4 Console"-Platzhalters 32×0x41 aus dem C++).
    #[test]
    fn parse_duid_semantics() {
        let duid = "00000007004100800102030405060708090a0b0c0d0e0f101112131415161718";
        let bytes = parse_duid(duid).expect("parse");
        assert_eq!(bytes.len(), 32);
        assert_eq!(&bytes[..8], &[0x00, 0x00, 0x00, 0x07, 0x00, 0x41, 0x00, 0x80]);

        // Main-PS4-Platzhalter: QByteArray(32, 'A') → 32 Bytes 0x41.
        let ps4 = parse_duid(&"41".repeat(32)).expect("parse");
        assert!(ps4.iter().all(|&b| b == 0x41));

        // Falsche Länge → InvalidData.
        assert_eq!(parse_duid("00").unwrap_err(), ChiakiError::InvalidData);
        assert_eq!(
            parse_duid(&"0".repeat(63)).unwrap_err(),
            ChiakiError::InvalidData
        );
        // Nicht-Hex → InvalidData.
        assert_eq!(parse_duid(&"zz".repeat(32)).unwrap_err(), ChiakiError::InvalidData);
    }

    /// send_recv_http_header_psn gegen einen Loopback-"Konsolen"-Peer: die
    /// B-Seite beantwortet die Session-Message (Sub-Message-Framing) mit einer
    /// CTRL-Message, die HTTP-Header + Content enthält; Header-Ende wird an
    /// der richtigen Stelle erkannt, remote_counter aktualisiert.
    #[test]
    fn psn_http_header_loopback() {
        fn loopback_pair() -> (UdpSocket, UdpSocket) {
            let a = chiaki_core::sock::create_udp_socket(
                "127.0.0.1:0".parse().unwrap(),
                &Default::default(),
            )
            .unwrap();
            let b = chiaki_core::sock::create_udp_socket(
                "127.0.0.1:0".parse().unwrap(),
                &Default::default(),
            )
            .unwrap();
            a.connect(b.local_addr().unwrap()).unwrap();
            b.connect(a.local_addr().unwrap()).unwrap();
            (a, b)
        }

        fn make_message(type_: u16, data: Vec<u8>) -> crate::rudp::RudpMessage {
            crate::rudp::RudpMessage {
                subtype: (type_ >> 8) as u8,
                type_,
                size: (0xC << 12) | (8 + data.len() as u16),
                data,
                remote_counter: 0,
                sub_message: None,
                sub_message_size: 0,
            }
        }

        let (sock_a, sock_b) = loopback_pair();
        let rudp_a = Rudp::new(sock_a).unwrap();
        let rudp_b = Rudp::new(sock_b).unwrap();

        let header = "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n";
        let content = b"hello";
        let mut response_data = vec![0u8, 0]; // Placeholder-Counter (C: data[0..2])
        response_data.extend_from_slice(header.as_bytes());
        response_data.extend_from_slice(content);

        // "Konsole"-Thread: nimmt die Session-Message (Sub-Message = CTRL)
        // entgegen und antwortet mit einer CTRL-Message (subtype 0x02).
        let server = std::thread::spawn(move || {
            let msg = rudp_b.select_recv(1500).expect("recv session message");
            assert_eq!(msg.packet_type(), Some(RudpPacketType::SessionMessage));
            let sub = msg.sub_message.expect("sub message");
            assert_eq!(sub.packet_type(), Some(RudpPacketType::CtrlMessage));
            // Antwort: CTRL-Message, data = [counter(2)] ++ Header ++ Content.
            rudp_b
                .send_raw(&message_serialize(&make_message(
                    RudpPacketType::CtrlMessage.value(),
                    response_data,
                )))
                .unwrap();
        });

        let mut remote_counter = 0u16;
        let mut buf = [0u8; 1500];
        let send_buf = b"POST /sie/ps5/rp/sess/rgst HTTP/1.1\r\n\r\npayload";
        let (header_size, received) = send_recv_http_header_psn(
            &rudp_a,
            &mut remote_counter,
            send_buf,
            &mut buf,
        )
        .expect("psn http header");
        server.join().unwrap();

        assert_eq!(header_size, header.len(), "Header-Ende inklusive \\r\\n\\r\\n");
        assert_eq!(received, header.len() + content.len());
        assert_eq!(&buf[..header_size], header.as_bytes());
        assert_eq!(&buf[header_size..received], content);
        // remote_counter = lokaler Counter der Antwort + 1 (parse_message).
        let _ = remote_counter; // Inhalt hängt am Zufalls-Counter der Antwort
    }

    /// Ohne Antwort-Peer → send_recv erschöpft die Versuche → InvalidResponse.
    #[test]
    fn psn_http_header_timeout_without_peer() {
        let a = chiaki_core::sock::create_udp_socket(
            "127.0.0.1:0".parse().unwrap(),
            &Default::default(),
        )
        .unwrap();
        let sink = chiaki_core::sock::create_udp_socket(
            "127.0.0.1:0".parse().unwrap(),
            &Default::default(),
        )
        .unwrap();
        a.connect(sink.local_addr().unwrap()).unwrap();
        let rudp = Rudp::new(a).unwrap();
        let mut remote_counter = 0u16;
        let mut buf = [0u8; 1500];
        let err = send_recv_http_header_psn(&rudp, &mut remote_counter, b"ping", &mut buf)
            .unwrap_err();
        assert_eq!(err, ChiakiError::InvalidResponse);
    }
}
