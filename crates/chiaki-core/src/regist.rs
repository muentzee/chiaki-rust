// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/regist.c + lib/include/chiaki/regist.h (chiaki-ng).
//
// Registrierungs-Flow gegen host:9295: UDP-Search ("SRC2"/"SRC3" -> "RES2"/"RES3"),
// TCP-Connect, HTTP-ähnlicher Request-Header + verschlüsselter Regist-Payload,
// Antwort empfangen (HTTP-Header + Content), mit dem RPCrypt entschlüsseln und
// als HTTP-Header-Liste parsen (RegisteredHost).
//
// Der Flow läuft — wie im C (chiaki_regist_start -> regist_thread_func) — in
// einem eigenen Thread und ist über die StopPipe abbrechbar (Regist::stop()).
//
// Abweichung gegenüber dem C-Code: der PSN-Regist-Pfad (ChiakiHolepunchRegistInfo
// + ChiakiRudp, regist.c: `if(regist->info.holepunch_info)`) liegt bewusst nicht
// in chiaki-core: Rudp/Holepunch gehören laut Workspace-Aufteilung nach
// chiaki-remote (gleiche Entscheidung wie in http.rs für
// chiaki_send_recv_http_header_psn). Die dafür nötigen Kryptohelfer
// (Rpcrypt::new_regist_psn, rpcrypt::aeropause_psn) existieren bereits in
// rpcrypt.rs; die RUDP-Message-Sequenz (INIT_REQUEST/INIT_RESPONSE,
// COOKIE_REQUEST/COOKIE_RESPONSE, ACK/FINISH) wird beim Port des PSN-Pfads in
// chiaki-remote ergänzt. Alle Felder/Formatierungen des regulären (LAN-)Regist
// sind 1:1.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::base64;
use super::error::{ChiakiError, ChiakiResult, Target};
use super::http::{header_parse, recv_http_header, response_parse};
use super::random::random_bytes_crypt;
use super::rpcrypt::{aeropause, aeropause_ps4_pre10, Rpcrypt, RPCRYPT_KEY_SIZE};
use super::sock::{create_udp_socket, map_io_error, recv_from_timeout, send_to, UdpSocketOptions};
use super::stoppipe::StopPipe;
use super::time::now_ms;

/// REGIST_PORT (regist.c)
const REGIST_PORT: u16 = 9295;

const SEARCH_REQUEST_SLEEP_MS: u64 = 100;
const REGIST_SEARCH_TIMEOUT_MS: u64 = 3000;
/// (sic) Name aus dem C-Code.
const REGIST_REPONSE_TIMEOUT_MS: u64 = 3000;

/// Empfangs-Puffergröße für HTTP-Header + Content (regist.c: `uint8_t buf[1500]`).
const RESPONSE_BUF_SIZE: usize = 1500;

/// Such-Puffer (regist.c: `uint8_t buf[0x100]`).
const SEARCH_BUF_SIZE: usize = 0x100;

/// Poll-Intervall für Empfangs-/Sendeschleifen (Reaktionszeit auf stop();
/// Rust-std kennt kein select() über Socket+Event, siehe stoppipe.rs-Doku).
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// CHIAKI_PSN_ACCOUNT_ID_SIZE
pub const PSN_ACCOUNT_ID_SIZE: usize = 8;
/// CHIAKI_SESSION_AUTH_SIZE
pub const SESSION_AUTH_SIZE: usize = 0x10;

/// Port von `ChiakiRegistInfo` (regist.h).
///
/// Der PSN-Pfad des C-Origins (`holepunch_info` + `rudp`) ist hier bewusst
/// nicht enthalten — siehe Moduldokumentation.
#[derive(Debug, Clone)]
pub struct RegistInfo {
    pub target: Target,
    pub host: String,
    pub broadcast: bool,

    /// may be None, in which case `psn_account_id` will be used
    pub psn_online_id: Option<String>,

    /// will be used if `psn_online_id` is None, for PS4 >= 7.0
    pub psn_account_id: [u8; PSN_ACCOUNT_ID_SIZE],

    pub pin: u32,
    pub console_pin: u32,
}

/// Port von `ChiakiRegisteredHost` (regist.h).
///
/// Die String-Felder entsprechen den C-Char-Arrays (`ap_ssid[0x30]` etc.):
/// Werte, die nicht passen, werden — wie im C (COPY_STRING-Makro) — mit
/// Fehlerlog verworfen. `rp_regist_key` muss komplett gefüllt sein
/// (mit 0 aufgefüllt).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredHost {
    pub target: Target,
    /// char[0x30]
    pub ap_ssid: String,
    /// char[0x20]
    pub ap_bssid: String,
    /// char[0x50]
    pub ap_key: String,
    /// char[0x20]
    pub ap_name: String,
    pub server_mac: [u8; 6],
    /// char[0x20]
    pub server_nickname: String,
    /// must be completely filled (pad with \0)
    pub rp_regist_key: [u8; SESSION_AUTH_SIZE],
    pub rp_key_type: u32,
    pub rp_key: [u8; 0x10],
    pub console_pin: u32,
}

impl Default for RegisteredHost {
    fn default() -> Self {
        RegisteredHost {
            target: Target::Ps4Unknown,
            ap_ssid: String::new(),
            ap_bssid: String::new(),
            ap_key: String::new(),
            ap_name: String::new(),
            server_mac: [0; 6],
            server_nickname: String::new(),
            rp_regist_key: [0; SESSION_AUTH_SIZE],
            rp_key_type: 0,
            rp_key: [0; 0x10],
            console_pin: 0,
        }
    }
}

/// Port von `ChiakiRegistEvent` (regist.h): C-Union aus type + registered_host
/// als Enum. Nur `FinishedSuccess` trägt einen Host (C: Pointer, sonst NULL).
#[derive(Debug, Clone)]
pub enum RegistEvent {
    FinishedCanceled,
    FinishedFailed,
    FinishedSuccess(Box<RegisteredHost>),
}

/// Port von `ChiakiRegistCb` (C: `void (*)(ChiakiRegistEvent*, void*)`).
pub type RegistCb = Arc<dyn Fn(RegistEvent) + Send + Sync>;

/// Port von `ChiakiRegist` (regist.h): info + cb leben im Thread,
/// `Regist` hält StopPipe + Thread-Handle.
pub struct Regist {
    stop_pipe: Arc<StopPipe>,
    thread: Option<JoinHandle<()>>,
}

impl Regist {
    /// Port von `chiaki_regist_start()`.
    pub fn start(info: RegistInfo, cb: RegistCb) -> ChiakiResult<Regist> {
        let stop_pipe = Arc::new(StopPipe::new());
        let thread_stop = Arc::clone(&stop_pipe);
        let thread = std::thread::Builder::new()
            .name("chiaki-regist".into())
            .spawn(move || regist_thread_func(info, cb, &thread_stop))
            .map_err(|_| ChiakiError::Thread)?;
        Ok(Regist {
            stop_pipe,
            thread: Some(thread),
        })
    }

    /// Port von `chiaki_regist_stop()`.
    pub fn stop(&self) {
        self.stop_pipe.stop();
    }

    /// Port von `chiaki_regist_fini()`: joint den Thread.
    pub fn fini(mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Request-Formatierung (regist.c)
// ---------------------------------------------------------------------------

/// regist.c: request_head_fmt — inkl. des doppelten " HTTP/1.1\r\n" (1:1).
const REQUEST_HEAD_FMT: &str = "POST {path} HTTP/1.1\r\n HTTP/1.1\r\n\
     HOST: {host}\r\n\
     User-Agent: remoteplay Windows\r\n\
     Connection: close\r\n\
     Content-Length: {content_length}\r\n";

const REQUEST_RP_VERSION_FMT: &str = "RP-Version: {version}\r\n";

const REQUEST_TAIL: &str = "\r\n";

/// regist.c: `client_type`
/// (öffentlich, da der PSN-Regist-Pfad in chiaki-remote denselben
/// Client-Type-Hash verwendet — siehe regist_psn.rs).
pub const CLIENT_TYPE: &str = "dabfa2ec873de5839bee8d3f4c0239c4282c07c25c6077a2931afcf0adc0d34f";
/// regist.c: `client_type_ps4_pre10`
const CLIENT_TYPE_PS4_PRE10: &str = "Windows";

const REQUEST_PATH_PS5: &str = "/sie/ps5/rp/sess/rgst";
const REQUEST_PATH_PS4: &str = "/sie/ps4/rp/sess/rgst";
const REQUEST_PATH_PS4_PRE10: &str = "/sce/rp/regist";

/// Port von `request_path()` (regist.c).
fn request_path(target: Target) -> &'static str {
    match target {
        Target::Ps5Unknown | Target::Ps5_1 => REQUEST_PATH_PS5,
        Target::Ps4_8 | Target::Ps4_9 => REQUEST_PATH_PS4_PRE10,
        _ => REQUEST_PATH_PS4,
    }
}

/// Port von `chiaki_rp_version_string()` (lib/src/session.c) — für den
/// RP-Version-Header. `None` entspricht dem C-NULL-Fall (PS4Unknown /
/// PS5Unknown); regist.c formatiert NULL-%s unter MSVC als "(null)".
pub fn rp_version_string(target: Target) -> Option<&'static str> {
    match target {
        Target::Ps4_8 => Some("8.0"),
        Target::Ps4_9 => Some("9.0"),
        Target::Ps4_10 => Some("10.0"),
        Target::Ps5_1 => Some("1.0"),
        _ => None,
    }
}

/// Port von `chiaki_rp_application_reason_string()` (lib/src/session.c).
/// Werte aus lib/include/chiaki/session.h (CHIAKI_RP_APPLICATION_REASON_*).
pub fn rp_application_reason_string(reason: u32) -> &'static str {
    match reason {
        0x8010_8b09 => "Regist failed, probably invalid PIN",
        0x8010_8b02 => "Invalid PSN ID",
        0x8010_8b10 => "Remote is already in use",
        0x8010_8b15 => "Remote Play on Console crashed",
        0x8010_8b11 => "RP-Version mismatch",
        _ => "unknown",
    }
}

/// Port von `request_header_format()` (regist.c).
///
/// Liefert `None` bei Formatier-/Größenfehlern (C: -1). Die C-Prüfung
/// `cur >= payload_size` (nicht `buf_size`!) wird 1:1 übernommen; zusätzlich
/// wird hier ein Überlauf des Zielpuffers geprüft (im C würde snprintf
/// abschneiden — bei realen Payload-Größen >= 0x1e0 nie relevant).
///
/// Öffentlich für den PSN-Regist-Pfad in chiaki-remote (regist_psn.rs), der
/// denselben HTTP-Request-Header nutzt (regist.c sendet ihn identisch, nur
/// via RUDP).
pub fn request_header_format(
    buf: &mut [u8],
    payload_size: usize,
    target: Target,
    regist_local_addr: &str,
) -> Option<usize> {
    // C: snprintf(buf, buf_size, request_head_fmt, request_path(target),
    //             regist_local_addr, (unsigned long long)payload_size)
    let head = REQUEST_HEAD_FMT
        .replace("{path}", request_path(target))
        .replace("{host}", regist_local_addr)
        .replace("{content_length}", &format!("{payload_size}"));
    let mut cur = head.len();
    if cur >= payload_size {
        return None;
    }

    if target >= Target::Ps4_9 {
        // C: snprintf(buf + cur, ..., request_rp_version_fmt, rp_version_str)
        // C: snprintf("%s", NULL) gibt unter MSVC/UCRT "(null)" — nachgebildet.
        let rp_version_str = rp_version_string(target).unwrap_or("(null)");
        let rp = REQUEST_RP_VERSION_FMT.replace("{version}", rp_version_str);
        cur += rp.len();
        if cur >= payload_size {
            return None;
        }
        if cur >= buf.len() {
            return None;
        }
        buf[..head.len()].copy_from_slice(head.as_bytes());
        buf[head.len()..cur].copy_from_slice(rp.as_bytes());
    } else {
        if cur >= buf.len() {
            return None;
        }
        buf[..cur].copy_from_slice(head.as_bytes());
    }

    // C: request_tail inkl. NUL anhängen (tail_size = strlen + 1), cur zählt
    // um tail_size - 1 weiter und wird inkl. "\r\n" zurückgegeben.
    let tail_size = REQUEST_TAIL.len() + 1;
    if cur + tail_size > payload_size {
        return None;
    }
    buf[cur..cur + REQUEST_TAIL.len()].copy_from_slice(REQUEST_TAIL.as_bytes());
    Some(cur + REQUEST_TAIL.len())
}

/// Port von `chiaki_regist_request_payload_format()` (regist.h/regist.c).
///
/// Füllt `buf` (mind. 0x1e0 Bytes für den zufallsdominierbaren Kopf) und
/// verschlüsselt den inneren Header an Offset 0x1e0. Rückgabe ist die
/// genutzte Gesamtgröße (C: `*buf_size`), der RPCrypt wird wie im C als
/// Out-Parameter befüllt.
///
/// Der `holepunch_info`-Pfad des C-Origins (PSN-Regist) ist nicht enthalten —
/// siehe Moduldokumentation. (Im C setzt dieser Pfad `psn_online_id = NULL`,
/// d. h. der innere Header wird auch dort über die Account-Id gebaut.)
pub fn request_payload_format(
    target: Target,
    ambassador: &[u8; RPCRYPT_KEY_SIZE],
    buf: &mut [u8],
    crypt: &mut Rpcrypt,
    psn_online_id: Option<&str>,
    psn_account_id: Option<&[u8; PSN_ACCOUNT_ID_SIZE]>,
    pin: u32,
) -> ChiakiResult<usize> {
    const INNER_HEADER_OFF: usize = 0x1e0;
    if buf.len() < INNER_HEADER_OFF {
        return Err(ChiakiError::BufTooSmall);
    }
    buf[..INNER_HEADER_OFF].fill(b'A'); // can be random

    // psn_online_id fällt (wie im C) im >=10-Pfad auf None
    let mut psn_online_id = psn_online_id;

    if target < Target::Ps4_10 {
        *crypt = Rpcrypt::new_regist_ps4_pre10(ambassador, pin);
        let aer = aeropause_ps4_pre10(&crypt.ambassador);
        buf[0x11c..0x11c + RPCRYPT_KEY_SIZE].copy_from_slice(&aer);
    } else {
        // Offsets aus dem (mit 'A' gefüllten) Puffer — 1:1 wie im C:
        // key_0_off = buf[0x18D] & 0x1F, key_1_off = buf[0] >> 3.
        let key_0_off = (buf[0x18d] & 0x1f) as usize;
        let key_1_off = (buf[0] >> 3) as usize;
        *crypt = Rpcrypt::new_regist(target, ambassador, key_0_off, pin)?;
        let aer = aeropause(target, key_1_off, &crypt.ambassador)?;
        buf[0xc7..0xc7 + 8].copy_from_slice(&aer[8..]);
        buf[0x191..0x191 + 8].copy_from_slice(&aer[..8]);
        psn_online_id = None; // don't need this
    }

    let inner: Vec<u8> = if let Some(online_id) = psn_online_id {
        format!("Client-Type: Windows\r\nNp-Online-Id: {online_id}\r\n").into_bytes()
    } else if let Some(account_id) = psn_account_id {
        // C: account_id_b64[CHIAKI_PSN_ACCOUNT_ID_SIZE * 2] — 8 Byte-Input
        // ergeben exakt 12 Base64-Zeichen inkl. '='-Padding.
        let account_id_b64 = base64::encode(account_id);
        let client_type = if target < Target::Ps4_10 {
            CLIENT_TYPE_PS4_PRE10
        } else {
            CLIENT_TYPE
        };
        format!("Client-Type: {client_type}\r\nNp-AccountId: {account_id_b64}\r\n").into_bytes()
    } else {
        return Err(ChiakiError::InvalidData);
    };
    // C: inner_header_size >= buf_size_val - inner_header_off -> BUF_TOO_SMALL
    if INNER_HEADER_OFF + inner.len() >= buf.len() {
        return Err(ChiakiError::BufTooSmall);
    }
    buf[INNER_HEADER_OFF..INNER_HEADER_OFF + inner.len()].copy_from_slice(&inner);
    crypt.encrypt(0, &mut buf[INNER_HEADER_OFF..INNER_HEADER_OFF + inner.len()])?;
    Ok(INNER_HEADER_OFF + inner.len())
}

// ---------------------------------------------------------------------------
// Thread-Flow (regist_thread_func + Hilfsfunktionen)
// ---------------------------------------------------------------------------

fn regist_thread_func(info: RegistInfo, cb: RegistCb, stop_pipe: &StopPipe) {
    // PSN-Regist (holepunch) ist in chiaki-core nicht portiert — siehe
    // Moduldokumentation.

    let mut host = RegisteredHost::default();
    let err = regist_flow(&info, stop_pipe, &mut host);

    let canceled = match err {
        Ok(()) => false,
        Err(ChiakiError::Canceled) => true,
        Err(e) => {
            tracing::error!("Regist eventually failed: {e}");
            false
        }
    };

    if canceled {
        tracing::info!("Regist canceled");
        cb(RegistEvent::FinishedCanceled);
    } else if err.is_ok() {
        let mut host = host;
        host.console_pin = info.console_pin;
        cb(RegistEvent::FinishedSuccess(Box::new(host)));
    } else {
        cb(RegistEvent::FinishedFailed);
    }
}

/// Kern von `regist_thread_func` (ohne PSN-Pfad), in einer Funktion gebündelt
/// für die goto-ähnliche Aufräumreihenfolge des C (fail_socket/fail_addrinfos
/// sind in Rust schlicht Drops).
fn regist_flow(
    info: &RegistInfo,
    stop_pipe: &StopPipe,
    host: &mut RegisteredHost,
) -> ChiakiResult<()> {
    let mut ambassador = [0u8; RPCRYPT_KEY_SIZE];
    if let Err(e) = random_bytes_crypt(&mut ambassador) {
        tracing::error!("Regist failed to generate random ambassador");
        return Err(e);
    }

    // C: uint8_t payload[0x400] (uninitialisiert; request_payload_format
    // füllt [0..0x1e0) selbst und schreibt den inneren Header dahinter).
    let mut payload = [0u8; 0x400];
    let mut crypt = Rpcrypt {
        target: info.target,
        bright: [0; RPCRYPT_KEY_SIZE],
        ambassador: [0; RPCRYPT_KEY_SIZE],
    };
    let payload_size = request_payload_format(
        info.target,
        &ambassador,
        &mut payload,
        &mut crypt,
        info.psn_online_id.as_deref(),
        Some(&info.psn_account_id),
        info.pin,
    )
    .inspect_err(|_| tracing::error!("Regist failed to format payload"))?;

    // random local addr if our local addr is not provided
    // (C: holepunch-Pfad nutzt holepunch_info->regist_local_ip)
    let regist_local_addr = "10.0.2.15";
    let mut request_header = [0u8; 0x100];
    let request_header_size = match request_header_format(
        &mut request_header,
        payload_size,
        info.target,
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

    // getaddrinfo: IPv6, wenn ':' im Host, sonst IPv4 (regist.c hints)
    let want_ipv6 = info.host.contains(':');
    let addrinfos: Vec<SocketAddr> = (info.host.as_str(), 0)
        .to_socket_addrs()
        .map_err(|_| {
            tracing::error!("Regist failed to getaddrinfo on {}", info.host);
            ChiakiError::ParseAddr
        })?
        .filter(|a| a.is_ipv6() == want_ipv6)
        .collect();

    let recv_addr = regist_search(info, &addrinfos, stop_pipe).inspect_err(|e| {
        if *e != ChiakiError::Canceled {
            tracing::error!("Regist search failed");
        }
    })?;

    // PS4 doesn't accept requests immediately
    // C: chiaki_stop_pipe_sleep(...); err != CHIAKI_ERR_TIMEOUT -> canceled
    if stop_pipe.wait_timeout(Duration::from_millis(SEARCH_REQUEST_SLEEP_MS))
        != ChiakiError::Timeout
    {
        return Err(ChiakiError::Canceled);
    }

    let mut sock = regist_request_connect(&recv_addr, stop_pipe).inspect_err(|e| {
        if *e != ChiakiError::Canceled {
            tracing::error!("Regist eventually failed to connect for request");
        }
    })?;
    tracing::info!("Regist connected to {}, sending request", info.host);

    let send_result = (|| -> ChiakiResult<()> {
        send_fully(
            stop_pipe,
            &mut sock,
            &request_header[..request_header_size],
            REGIST_REPONSE_TIMEOUT_MS,
        )
        .inspect_err(|e| {
            if *e == ChiakiError::Canceled {
                tracing::info!("Regist canceled while sending request header");
            } else {
                tracing::error!("Regist failed to send request header: {e}");
            }
        })?;
        send_fully(
            stop_pipe,
            &mut sock,
            &payload[..payload_size],
            REGIST_REPONSE_TIMEOUT_MS,
        )
        .inspect_err(|e| {
            if *e == ChiakiError::Canceled {
                tracing::info!("Regist canceled while sending payload");
            } else {
                tracing::error!("Regist failed to send payload: {e}");
            }
        })?;
        tracing::info!("Regist waiting for response");
        Ok(())
    })();

    send_result.and_then(|()| regist_recv_response(stop_pipe, info, host, &mut sock, &crypt))
}

/// Port von `regist_search_connect()`: nimmt die erste erreichbare Adresse
/// (UDP-Socket anlegen inkl. Broadcast-Option bzw. connecten).
/// Rückgabe: Socket + Sendeadresse (Port 9295).
fn regist_search_connect(
    info: &RegistInfo,
    addrinfos: &[SocketAddr],
) -> ChiakiResult<(UdpSocket, SocketAddr)> {
    let mut last_err = ChiakiError::Network;
    for ai in addrinfos {
        let mut send_addr = *ai;
        send_addr.set_port(REGIST_PORT);

        let bind_addr: SocketAddr = if send_addr.is_ipv6() {
            "[::]:0".parse().unwrap()
        } else {
            "0.0.0.0:0".parse().unwrap()
        };
        let opts = UdpSocketOptions {
            broadcast: info.broadcast,
            ..Default::default()
        };
        // C: broadcast -> SO_BROADCAST + bind(INADDR_ANY/:0);
        //    sonst connect() — Fehler: nächste Adresse.
        let sock = match create_udp_socket(bind_addr, &opts) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Regist failed to create socket for search");
                last_err = e;
                continue;
            }
        };
        if !info.broadcast {
            if let Err(e) = sock.connect(send_addr) {
                tracing::error!("Regist connect failed, error: {e}. Trying next address...");
                last_err = map_io_error(&e);
                continue;
            }
        }
        return Ok((sock, send_addr));
    }
    tracing::info!("Regist connect failed: tried all addresses");
    Err(last_err)
}

/// Wartet (mit StopPipe-Poll) auf ein UDP-Paket; entspricht im C dem Paar
/// chiaki_stop_pipe_select_single() + recvfrom().
fn recv_select(
    stop_pipe: &StopPipe,
    sock: &UdpSocket,
    timeout: Duration,
    buf: &mut [u8],
) -> ChiakiResult<(usize, SocketAddr)> {
    let deadline = Instant::now() + timeout;
    loop {
        stop_pipe.check()?;
        let rest = deadline.saturating_duration_since(Instant::now());
        if rest.is_zero() {
            return Err(ChiakiError::Timeout);
        }
        match recv_from_timeout(sock, buf, rest.min(POLL_INTERVAL)) {
            Ok(r) => return Ok(r),
            Err(ChiakiError::Timeout) => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Port von `regist_search()`: sendet SRC2/SRC3 und wartet auf RES2/RES3.
/// Liefert die Adresse, von der die Antwort kam (für den TCP-Connect).
fn regist_search(
    info: &RegistInfo,
    addrinfos: &[SocketAddr],
    stop_pipe: &StopPipe,
) -> ChiakiResult<SocketAddr> {
    tracing::info!("Regist starting search");
    let (sock, send_addr) = match regist_search_connect(info, addrinfos) {
        Ok(r) => r,
        Err(_) => {
            tracing::error!("Regist eventually failed to connect for search");
            return Err(ChiakiError::Network);
        }
    };

    let (src, res): (&[u8], &[u8]) = if info.target.is_ps5() {
        (b"SRC3\0", b"RES3")
    } else {
        (b"SRC2\0", b"RES2")
    };

    tracing::info!("Regist sending search packet");
    let send_res = if info.broadcast {
        send_to(&sock, src, send_addr)
    } else {
        // connected socket: send()
        loop {
            match sock.send(src) {
                Ok(n) => break Ok(n),
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => {
                    tracing::error!("Regist failed to send search: {e}");
                    break Err(map_io_error(&e));
                }
            }
        }
    };
    send_res?;

    let timeout_abs_ms = now_ms() + REGIST_SEARCH_TIMEOUT_MS;
    let mut buf = [0u8; SEARCH_BUF_SIZE];
    loop {
        let now = now_ms();
        if now > timeout_abs_ms {
            tracing::error!("Regist timed out waiting for search response");
            return Err(ChiakiError::Timeout);
        }
        let (n, from) = {
            // C: recvfrom(sock, buf, sizeof(buf) - 1, ...)
            let recv_len = buf.len() - 1;
            recv_select(
                stop_pipe,
                &sock,
                Duration::from_millis(timeout_abs_ms - now),
                &mut buf[..recv_len],
            )?
        };
        // C: n <= 0 -> NETWORK (auch das 0-Byte-Datagram)
        if n == 0 {
            tracing::error!("Regist failed to receive search response");
            return Err(ChiakiError::Network);
        }

        tracing::trace!("Regist received packet: {n} >= {}", res.len());
        if n >= res.len() && &buf[..res.len()] == res {
            tracing::info!("Regist received search response from {from}");
            return Ok(from);
        }
    }
}

/// Port von `regist_request_connect()`: TCP-Connect mit StopPipe + Timeout.
fn regist_request_connect(addr: &SocketAddr, stop_pipe: &StopPipe) -> ChiakiResult<TcpStream> {
    let err = stop_pipe_connect(addr, stop_pipe, REGIST_REPONSE_TIMEOUT_MS);
    match err {
        Ok(sock) => Ok(sock),
        Err(e) => {
            if e == ChiakiError::Canceled {
                tracing::info!("Regist canceled while connecting request socket");
            } else {
                tracing::error!("Regist connect failed: {e}");
            }
            Err(e)
        }
    }
}

/// `chiaki_stop_pipe_connect`-Äquivalent für TCP (siehe stoppipe.rs-Doku):
/// `TcpStream::connect_timeout` in Teilstücken, dazwischen Stop-Flag prüfen.
fn stop_pipe_connect(
    addr: &SocketAddr,
    stop_pipe: &StopPipe,
    timeout_ms: u64,
) -> ChiakiResult<TcpStream> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        stop_pipe.check()?;
        let now = Instant::now();
        if now >= deadline {
            return Err(ChiakiError::Timeout);
        }
        let slice = (deadline - now).min(POLL_INTERVAL);
        match TcpStream::connect_timeout(addr, slice) {
            Ok(sock) => return Ok(sock),
            Err(e) if e.kind() == ErrorKind::TimedOut => return Err(ChiakiError::Timeout),
            Err(e) if e.kind() == ErrorKind::Interrupted || e.kind() == ErrorKind::WouldBlock => {
                continue
            }
            Err(e) => return Err(map_io_error(&e)),
        }
    }
}

/// Port von `chiaki_send_fully()` (utils.h) über einen TCP-Stream:
/// sendet alles; bei WouldBlock wird mit StopPipe + Deadline gewartet.
fn send_fully(
    stop_pipe: &StopPipe,
    sock: &mut TcpStream,
    mut buf: &[u8],
    timeout_ms: u64,
) -> ChiakiResult<()> {
    let deadline = if timeout_ms == u64::MAX {
        None
    } else {
        Some(Instant::now() + Duration::from_millis(timeout_ms))
    };
    while !buf.is_empty() {
        stop_pipe.check()?;
        let remaining = match deadline {
            None => Duration::MAX,
            Some(d) => d.saturating_duration_since(Instant::now()),
        };
        if remaining.is_zero() {
            return Err(ChiakiError::Timeout);
        }
        sock.set_write_timeout(Some(remaining.min(POLL_INTERVAL)))
            .map_err(|_| ChiakiError::Network)?;
        match sock.write(buf) {
            Ok(0) => return Err(ChiakiError::Network),
            Ok(n) => buf = &buf[n..],
            // C: WSAEINTR -> continue; WSAEWOULDBLOCK -> select(stop_pipe)+continue
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                continue
            }
            Err(e) => return Err(map_io_error(&e)),
        }
    }
    Ok(())
}

/// Port von `regist_recv_response()` (ohne den PSN/holepunch-Pfad).
fn regist_recv_response(
    stop_pipe: &StopPipe,
    info: &RegistInfo,
    host: &mut RegisteredHost,
    sock: &mut TcpStream,
    rpcrypt: &Rpcrypt,
) -> ChiakiResult<()> {
    let mut buf = [0u8; RESPONSE_BUF_SIZE];

    let (header_size, mut buf_filled_size) =
        recv_http_header(sock, &mut buf, Some(stop_pipe), REGIST_REPONSE_TIMEOUT_MS).inspect_err(
            |e| {
                if *e != ChiakiError::Canceled {
                    tracing::error!("Regist failed to receive response HTTP header");
                }
            },
        )?;

    tracing::trace!(
        "Regist response HTTP header:\n{}",
        String::from_utf8_lossy(&buf[..header_size])
    );

    let http_response = response_parse(&buf[..header_size])
        .inspect_err(|_| tracing::error!("Regist failed to pare response HTTP header"))?;

    if http_response.code != 200 {
        tracing::error!("Regist received HTTP code {}", http_response.code);
        for header in &http_response.headers {
            if header.key == "RP-Application-Reason" {
                // C: strtoul(value, NULL, 0x10)
                let reason = strtoul_u32(header.value.as_bytes(), 0x10);
                tracing::error!(
                    "Reported Application Reason: {:#x} ({})",
                    reason,
                    rp_application_reason_string(reason)
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
            content_size = strtoull_usize(header.value.as_bytes());
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

    // Content bis content_size + header_size empfangen (Deadline 3 s;
    // C: select(stop_pipe, sock) + recv, WSAEWOULDBLOCK -> weiter).
    let deadline_ms = now_ms() + REGIST_REPONSE_TIMEOUT_MS;
    while buf_filled_size < content_size + header_size {
        let now = now_ms();
        if now >= deadline_ms {
            tracing::error!("Regist timed out receiving response content");
            return Err(ChiakiError::Timeout);
        }
        stop_pipe.check()?;
        let rest = Duration::from_millis(deadline_ms - now);
        sock.set_read_timeout(Some(rest.min(POLL_INTERVAL)))
            .map_err(|_| ChiakiError::Network)?;
        let received = match sock.read(&mut buf[buf_filled_size..content_size + header_size]) {
            Ok(n) => n,
            Err(e)
                if e.kind() == ErrorKind::WouldBlock
                    || e.kind() == ErrorKind::TimedOut
                    || e.kind() == ErrorKind::Interrupted =>
            {
                continue
            }
            // C: received < 0 (außer WOULDBLOCK) bzw. == 0 -> NETWORK
            Err(_) => 0,
        };
        if received == 0 {
            tracing::error!("Regist failed to receive response content");
            return Err(ChiakiError::Network);
        }
        buf_filled_size += received;
    }

    let payload = &mut buf[header_size..buf_filled_size];
    rpcrypt.decrypt(0, payload)?;

    tracing::info!(
        "Regist response payload (decrypted):\n{}",
        String::from_utf8_lossy(payload)
    );

    parse_response_payload(info, host, payload)
        .inspect_err(|_| tracing::error!("Regist failed to parse response payload"))
}

/// Port von `regist_parse_response_payload()`: parst die entschlüsselte
/// Payload als HTTP-Header und füllt `host`.
///
/// Öffentlich für den PSN-Regist-Pfad in chiaki-remote (regist_psn.rs) —
/// die Response-Struktur ist identisch, nur der Transport (RUDP statt TCP).
pub fn parse_response_payload(
    info: &RegistInfo,
    host: &mut RegisteredHost,
    buf: &[u8],
) -> ChiakiResult<()> {
    let headers = header_parse(buf)
        .inspect_err(|_| tracing::error!("Regist failed to parse response payload HTTP header"))?;

    *host = RegisteredHost::default();
    host.target = info.target;

    let mut mac_found = false;
    let mut regist_key_found = false;
    let mut key_found = false;
    let ps5 = info.target.is_ps5();

    for header in &headers {
        // COPY_STRING-Makro: String-Felder mit Größenlimit aus dem C-Struct;
        // zu lange Werte werden mit Fehlerlog verworfen (C: continue).
        let mut copied = false;
        macro_rules! copy_string {
            ($dst:expr, $cap:expr, $key:expr) => {
                if !copied && header.key == $key {
                    copied = true;
                    if header.value.len() >= $cap {
                        tracing::error!("Regist value for {} in response is too long", $key);
                    } else {
                        $dst = header.value.clone();
                    }
                }
            };
        }
        copy_string!(host.ap_ssid, 0x30, "AP-Ssid");
        copy_string!(host.ap_bssid, 0x20, "AP-Bssid");
        copy_string!(host.ap_key, 0x50, "AP-Key");
        copy_string!(host.ap_name, 0x20, "AP-Name");
        copy_string!(
            host.server_nickname,
            0x20,
            if ps5 { "PS5-Nickname" } else { "PS4-Nickname" }
        );
        if copied {
            continue;
        }

        let regist_key_header = if ps5 { "PS5-RegistKey" } else { "PS4-RegistKey" };
        if header.key == regist_key_header {
            host.rp_regist_key = [0; SESSION_AUTH_SIZE];
            match parse_hex(&mut host.rp_regist_key, header.value.as_bytes()) {
                Ok(_) => regist_key_found = true,
                Err(_) => {
                    tracing::error!("Regist received invalid RegistKey in response");
                    host.rp_regist_key = [0; SESSION_AUTH_SIZE];
                }
            }
        } else if header.key == "RP-KeyType" {
            host.rp_key_type = strtoul_u32(header.value.as_bytes(), 0);
        } else if header.key == "RP-Key" {
            host.rp_key = [0; 0x10];
            match parse_hex(&mut host.rp_key, header.value.as_bytes()) {
                Ok(n) if n == host.rp_key.len() => key_found = true,
                _ => {
                    tracing::error!("Regist received invalid key in response");
                    host.rp_key = [0; 0x10];
                }
            }
        } else {
            let mac_header = if ps5 { "PS5-Mac" } else { "PS4-Mac" };
            if header.key == mac_header {
                host.server_mac = [0; 6];
                match parse_hex(&mut host.server_mac, header.value.as_bytes()) {
                    Ok(n) if n == host.server_mac.len() => mac_found = true,
                    _ => {
                        tracing::error!("Regist received invalid MAC Address in response");
                        host.server_mac = [0; 6];
                    }
                }
            } else if header.key == "RP-SupportCmd" {
                let support_cmd = strtoul_u32(header.value.as_bytes(), 0);
                tracing::info!("RP-Support Cmd: {support_cmd}");
            } else {
                tracing::info!(
                    "Regist received unknown key {} in response payload",
                    header.key
                );
            }
        }
    }

    if !regist_key_found {
        tracing::error!("Regist response is missing RegistKey (or it was invalid)");
        return Err(ChiakiError::InvalidResponse);
    }
    if !key_found {
        tracing::error!("Regist response is missing key (or it was invalid)");
        return Err(ChiakiError::InvalidResponse);
    }
    if !mac_found {
        tracing::error!("Regist response is missing MAC Adress (or it was invalid)");
        return Err(ChiakiError::InvalidResponse);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// utils.h-Portierungsteile (parse_hex, strtoul-Äquivalente)
// ---------------------------------------------------------------------------

/// Port von `parse_hex()` (utils.h): schreibt `hex.len()/2` Bytes nach `buf`,
/// liefert die geschriebene Anzahl (C: `*buf_size`).
pub(crate) fn parse_hex(buf: &mut [u8], hex: &[u8]) -> ChiakiResult<usize> {
    if hex.len() % 2 != 0 {
        return Err(ChiakiError::InvalidData);
    }
    if hex.len() / 2 > buf.len() {
        return Err(ChiakiError::BufTooSmall);
    }
    for i in (0..hex.len()).step_by(2) {
        let h = nibble_value(hex[i]).ok_or(ChiakiError::InvalidData)?;
        let l = nibble_value(hex[i + 1]).ok_or(ChiakiError::InvalidData)?;
        buf[i / 2] = (h << 4) | l;
    }
    Ok(hex.len() / 2)
}

/// Port von `nibble_value()` (utils.h).
fn nibble_value(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 0xa),
        b'A'..=b'F' => Some(c - b'A' + 0xa),
        _ => None,
    }
}

/// `strtoul(value, NULL, base)`-Semantik (saturierend, auf u32 gekürzt).
fn strtoul_u32(buf: &[u8], base: u32) -> u32 {
    strtoull_u64(buf, base) as u32
}

/// `strtoull(value, NULL, 0)`-Semantik (saturierend, auf usize gekürzt).
fn strtoull_usize(buf: &[u8]) -> usize {
    strtoull_u64(buf, 0) as usize
}

/// Gemeinsame strtol-Familien-Logik: Whitespace überspringen, optionales
/// Vorzeichen (negativ -> wrapping_neg, wie im C), 0x-/0-Präfixe bei base 0,
/// Ziffern der Basis, saturierend.
fn strtoull_u64(buf: &[u8], base: u32) -> u64 {
    let mut i = 0usize;
    while i < buf.len() && buf[i].is_ascii_whitespace() {
        i += 1;
    }
    let negative = if i < buf.len() && (buf[i] == b'+' || buf[i] == b'-') {
        let neg = buf[i] == b'-';
        i += 1;
        neg
    } else {
        false
    };
    let mut base = base;
    if (base == 0 || base == 16)
        && i + 1 < buf.len()
        && buf[i] == b'0'
        && (buf[i + 1] | 0x20) == b'x'
    {
        i += 2;
        base = 16;
    } else if base == 0 {
        base = if i < buf.len() && buf[i] == b'0' { 8 } else { 10 };
    }
    let mut val = 0u64;
    while i < buf.len() {
        let d = match buf[i] {
            b'0'..=b'9' => (buf[i] - b'0') as u64,
            b'a'..=b'z' => (buf[i] - b'a' + 0xa) as u64,
            b'A'..=b'Z' => (buf[i] - b'A' + 0xa) as u64,
            _ => break,
        };
        if d >= base as u64 {
            break;
        }
        val = val.saturating_mul(base as u64).saturating_add(d);
        i += 1;
    }
    if negative {
        val.wrapping_neg()
    } else {
        val
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    fn hx(s: &str) -> Vec<u8> {
        hex::decode(s).expect("valid hex")
    }

    const AMBASSADOR: [u8; 16] = [
        0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e,
        0x1f,
    ];
    const PIN: u32 = 0x12345678;

    fn fresh_crypt(target: Target) -> Rpcrypt {
        Rpcrypt {
            target,
            bright: [0; RPCRYPT_KEY_SIZE],
            ambassador: [0; RPCRYPT_KEY_SIZE],
        }
    }

    // Golden-Vektoren: mit den Tabellen aus rpcrypt_tables.rs und
    // AES-128-CFB128 (OpenSSL, identisch zur CFB128-Routine in rpcrypt.rs)
    // unabhängig nach dem C-Algorithmus berechnet.

    /// PS4-pre10-Pfad (target < PS4_10, Online-Id): aeropause_ps4_pre10 an
    /// 0x11c, innerer Header mit REGIST_AES_KEY^pin verschlüsselt.
    #[test]
    fn request_payload_format_ps4_pre10_online_id() {
        let target = Target::Ps4_9;
        let mut buf = [0u8; 0x400];
        let mut crypt = fresh_crypt(target);
        let size = request_payload_format(
            target,
            &AMBASSADOR,
            &mut buf,
            &mut crypt,
            Some("chiaki-rs"),
            Some(&[1, 2, 3, 4, 5, 6, 7, 8]),
            PIN,
        )
        .expect("format");

        let inner_plain = b"Client-Type: Windows\r\nNp-Online-Id: chiaki-rs\r\n";
        assert_eq!(size, 0x1e0 + inner_plain.len());

        // Kopf mit 'A' gefüllt …
        assert!(buf[..0x11c].iter().all(|&b| b == b'A'));
        assert!(buf[0x12c..0x1e0].iter().all(|&b| b == b'A'));
        // … außer der Aeropause an 0x11c.
        assert_eq!(
            &buf[0x11c..0x12c],
            hx("060b7bdd3a5aef621be9fa9f77d527e3").as_slice()
        );
        // verschlüsselter innerer Header (hardcodiert)
        assert_eq!(
            &buf[0x1e0..size],
            hx("ea8aefdcfc91c2b9f9be638bdcc2e8895b2940d0da0bfefc5cd82d6f664e8995747c85e70f5cc638975f844a46ef2c")
                .as_slice()
        );

        // RPCrypt-Out-Parameter: pre10-Ziel, Ambassador übernommen.
        assert_eq!(crypt.target, Target::Ps4_9);
        assert_eq!(crypt.ambassador, AMBASSADOR);
        // Entschlüsseln des inneren Headers liefert den Klartext zurück.
        let mut inner = buf[0x1e0..size].to_vec();
        crypt.decrypt(0, &mut inner).unwrap();
        assert_eq!(inner, inner_plain);
    }

    /// PS4 10.0-Pfad mit Account-Id: key_0_off = 'A' & 0x1f = 1,
    /// key_1_off = 'A' >> 3 = 8, Aeropause auf 0xc7/0x191 gesplittet.
    #[test]
    fn request_payload_format_ps4_10_account_id() {
        let target = Target::Ps4_10;
        let mut buf = [0u8; 0x400];
        let mut crypt = fresh_crypt(target);
        let size = request_payload_format(
            target,
            &AMBASSADOR,
            &mut buf,
            &mut crypt,
            Some("ignored-on-ps4-10"),
            Some(&[1, 2, 3, 4, 5, 6, 7, 8]),
            PIN,
        )
        .expect("format");

        let inner_plain = b"Client-Type: dabfa2ec873de5839bee8d3f4c0239c4282c07c25c6077a2931afcf0adc0d34f\r\nNp-AccountId: AQIDBAUGBwg=\r\n";
        assert_eq!(size, 0x1e0 + inner_plain.len());

        assert_eq!(&buf[0xc7..0xcf], hx("ef6ee595ac078e7f").as_slice()); // aeropause[8..]
        assert_eq!(&buf[0x191..0x199], hx("78241717b2d38381").as_slice()); // aeropause[0..8]
        assert_eq!(
            &buf[0x1e0..size],
            hx("d75f8e3f60efa2072eb8ffb4049c3508ee2a5ea38bba761e0e2cb7ea4a1f85903961b71185b3b8dce1db9660a70136b1fa015ddef6f6066daf6e174deb089830d06bc85347b5a92a083e64827238f5bac93fdd0205ecfa6d859a07c2eaf363fee374cec0adba08b0c7959a")
                .as_slice()
        );

        // key_0_off = 1 -> Bright-Key aus PS4_KEYS_0 mit Pin-Xor an [0xc..0x10]
        assert_eq!(crypt.bright, hx("cebcb640080776047b85e85be1d8c21b").as_slice());
        let mut inner = buf[0x1e0..size].to_vec();
        crypt.decrypt(0, &mut inner).unwrap();
        assert_eq!(inner, inner_plain);
    }

    /// PS5-Pfad (>= 10): psn_online_id wird ignoriert (C:
    /// `psn_online_id = NULL; // don't need this`) -> Account-Id-Format.
    #[test]
    fn request_payload_format_ps5_account_id_golden() {
        let target = Target::Ps5_1;
        let mut buf = [0u8; 0x400];
        let mut crypt = fresh_crypt(target);
        let size = request_payload_format(
            target,
            &AMBASSADOR,
            &mut buf,
            &mut crypt,
            Some("ignored"),
            Some(&[1, 2, 3, 4, 5, 6, 7, 8]),
            PIN,
        )
        .expect("format");

        // Aeropause (PS5_KEYS_1, wurzelbert -0x2d): [8..] an 0xc7, [0..8] an 0x191
        assert_eq!(&buf[0xc7..0xcf], hx("5536fd9afc68757c").as_slice());
        assert_eq!(&buf[0x191..0x199], hx("8bfa487c0ac1ea53").as_slice());
        assert_eq!(
            &buf[0x1e0..size],
            hx("11e2442f2da979a2583c47fc7e387091dafe16ef6c0a58999473e06c14fa4058e478ac0ff332c6fea19ee7423f80a38886f43847b3b845c8386f1667b1b89cfbab887388c7e3b760ad8adaff3a127fd8e901c5f98ddbddd20a6968e285fd1f7688ba8a83146dac04c6c0fc")
                .as_slice()
        );
        assert_eq!(crypt.bright, hx("d860e146bdb0bd94b460070ba37bb15b").as_slice());

        // Entschlüsselung liefert den erwarteten inneren Header
        let mut inner = buf[0x1e0..size].to_vec();
        crypt.decrypt(0, &mut inner).unwrap();
        assert_eq!(
            inner,
            b"Client-Type: dabfa2ec873de5839bee8d3f4c0239c4282c07c25c6077a2931afcf0adc0d34f\r\nNp-AccountId: AQIDBAUGBwg=\r\n"
        );
    }

    #[test]
    fn request_payload_format_requires_id() {
        let mut buf = [0u8; 0x400];
        let mut crypt = fresh_crypt(Target::Ps4_10);
        // weder online_id noch account_id -> InvalidData (wie im C)
        let err = request_payload_format(
            Target::Ps4_10,
            &AMBASSADOR,
            &mut buf,
            &mut crypt,
            None,
            None,
            PIN,
        )
        .unwrap_err();
        assert_eq!(err, ChiakiError::InvalidData);

        // Puffer zu klein -> BufTooSmall
        let mut small = [0u8; 0x1df];
        let err = request_payload_format(
            Target::Ps4_10,
            &AMBASSADOR,
            &mut small,
            &mut crypt,
            None,
            Some(&[0; 8]),
            PIN,
        )
        .unwrap_err();
        assert_eq!(err, ChiakiError::BufTooSmall);
    }

    /// Header-Formatierung: Pfade je Target, RP-Version nur ab PS4 9.
    #[test]
    fn request_header_format_golden() {
        let payload_size = 0x1e6usize;
        let mut buf = [0u8; 0x100];

        // PS5_1: /sie/ps5/rp/sess/rgst + RP-Version: 1.0
        let n = request_header_format(&mut buf, payload_size, Target::Ps5_1, "10.0.2.15")
            .expect("format");
        assert_eq!(
            String::from_utf8_lossy(&buf[..n]),
            "POST /sie/ps5/rp/sess/rgst HTTP/1.1\r\n HTTP/1.1\r\n\
             HOST: 10.0.2.15\r\n\
             User-Agent: remoteplay Windows\r\n\
             Connection: close\r\n\
             Content-Length: 486\r\n\
             RP-Version: 1.0\r\n\r\n"
        );

        // PS4_8: /sce/rp/regist, KEINE RP-Version (target < PS4_9)
        let n = request_header_format(&mut buf, payload_size, Target::Ps4_8, "10.0.2.15")
            .expect("format");
        assert_eq!(
            String::from_utf8_lossy(&buf[..n]),
            "POST /sce/rp/regist HTTP/1.1\r\n HTTP/1.1\r\n\
             HOST: 10.0.2.15\r\n\
             User-Agent: remoteplay Windows\r\n\
             Connection: close\r\n\
             Content-Length: 486\r\n\r\n"
        );

        // PS4_9: /sce/rp/regist + RP-Version: 9.0
        let n = request_header_format(&mut buf, payload_size, Target::Ps4_9, "host")
            .expect("format");
        let s = String::from_utf8_lossy(&buf[..n]).into_owned();
        assert!(s.starts_with("POST /sce/rp/regist HTTP/1.1\r\n HTTP/1.1\r\n"));
        assert!(s.contains("RP-Version: 9.0\r\n\r\n"));

        // PS4_10: /sie/ps4/rp/sess/rgst + RP-Version: 10.0
        let n = request_header_format(&mut buf, payload_size, Target::Ps4_10, "host")
            .expect("format");
        let s = String::from_utf8_lossy(&buf[..n]).into_owned();
        assert!(s.starts_with("POST /sie/ps4/rp/sess/rgst HTTP/1.1\r\n HTTP/1.1\r\n"));
        assert!(s.contains("RP-Version: 10.0\r\n\r\n"));

        // Zu kleine payload_size -> None (C: cur >= payload_size -> -1)
        assert_eq!(
            request_header_format(&mut buf, 16, Target::Ps5_1, "h"),
            None
        );
    }

    /// parse_hex (utils.h) inkl. Fehlerfälle.
    #[test]
    fn parse_hex_semantics() {
        let mut buf = [0u8; 6];
        assert_eq!(parse_hex(&mut buf, b"001122334455").unwrap(), 6);
        assert_eq!(buf, [0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
        // ungerade Länge
        assert_eq!(
            parse_hex(&mut buf, b"001").unwrap_err(),
            ChiakiError::InvalidData
        );
        // ungültiges Zeichen
        assert_eq!(
            parse_hex(&mut buf, b"00zz").unwrap_err(),
            ChiakiError::InvalidData
        );
        // Puffer zu klein
        assert_eq!(
            parse_hex(&mut buf, b"001122334455667788").unwrap_err(),
            ChiakiError::BufTooSmall
        );
    }

    /// Response-Parser mit synthetischer (entschlüsselter) Payload nach dem
    /// C-Layout (PS4-Varianten der Headernamen).
    #[test]
    fn parse_response_payload_ps4() {
        let info = RegistInfo {
            target: Target::Ps4_10,
            host: "10.0.2.15".into(),
            broadcast: false,
            psn_online_id: None,
            psn_account_id: [0; 8],
            pin: 0,
            console_pin: 424242,
        };
        // MAC als reine Hex-Sequenz (parse_hex parst — wie im C — keine
        // Doppelpunkte).
        let payload = b"PS4-Mac: 001122334455\r\n\
             PS4-RegistKey: 00112233445566778899aabbccddeeff\r\n\
             RP-KeyType: 2\r\n\
             RP-Key: aabbccddeeff00112233445566778899\r\n\
             PS4-Nickname: TestPS4\r\n\
             AP-Ssid: myssid\r\n\
             AP-Bssid: 66:55:44:33:22:11\r\n\
             AP-Key: mykey\r\n\
             AP-Name: myname\r\n\
             RP-SupportCmd: 1\r\n";
        let mut host = RegisteredHost::default();
        parse_response_payload(&info, &mut host, payload).expect("parse");

        assert_eq!(host.target, Target::Ps4_10);
        assert_eq!(host.server_mac, [0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
        assert_eq!(
            host.rp_regist_key,
            [
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc,
                0xdd, 0xee, 0xff
            ]
        );
        assert_eq!(host.rp_key_type, 2);
        assert_eq!(
            host.rp_key,
            [
                0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66,
                0x77, 0x88, 0x99
            ]
        );
        assert_eq!(host.server_nickname, "TestPS4");
        assert_eq!(host.ap_ssid, "myssid");
        assert_eq!(host.ap_bssid, "66:55:44:33:22:11");
        assert_eq!(host.ap_key, "mykey");
        assert_eq!(host.ap_name, "myname");
        // console_pin wird erst im Thread-Func übernommen
        // (C: host.console_pin = regist->info.console_pin)
        assert_eq!(host.console_pin, 0);
    }

    #[test]
    fn parse_response_payload_ps5_header_names() {
        let info = RegistInfo {
            target: Target::Ps5_1,
            host: String::new(),
            broadcast: false,
            psn_online_id: None,
            psn_account_id: [0; 8],
            pin: 0,
            console_pin: 0,
        };
        let payload = b"PS5-Mac: aabbccddeeff\r\n\
             PS5-RegistKey: 0f0e0d0c0b0a09080706050403020100\r\n\
             RP-KeyType: 2\r\n\
             RP-Key: 000102030405060708090a0b0c0d0e0f\r\n\
             PS5-Nickname: PS5-Zimmer\r\n";
        let mut host = RegisteredHost::default();
        parse_response_payload(&info, &mut host, payload).expect("parse");
        assert_eq!(host.server_mac, [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
        assert_eq!(host.rp_regist_key[0], 0x0f);
        assert_eq!(host.rp_regist_key[15], 0x00);
        assert_eq!(host.server_nickname, "PS5-Zimmer");
    }

    #[test]
    fn parse_response_payload_missing_fields() {
        let info = RegistInfo {
            target: Target::Ps4_10,
            host: String::new(),
            broadcast: false,
            psn_online_id: None,
            psn_account_id: [0; 8],
            pin: 0,
            console_pin: 0,
        };
        let mut host = RegisteredHost::default();

        // MAC fehlt -> InvalidResponse
        let payload = b"PS4-RegistKey: 00112233445566778899aabbccddeeff\r\n\
             RP-Key: aabbccddeeff00112233445566778899\r\n";
        assert_eq!(
            parse_response_payload(&info, &mut host, payload).unwrap_err(),
            ChiakiError::InvalidResponse
        );

        // RegistKey-Hex ungültig -> zählt als fehlend
        let payload = b"PS4-Mac: 001122334455\r\n\
             PS4-RegistKey: zzzz\r\n\
             RP-Key: aabbccddeeff00112233445566778899\r\n";
        assert_eq!(
            parse_response_payload(&info, &mut host, payload).unwrap_err(),
            ChiakiError::InvalidResponse
        );

        // RP-Key mit falscher Länge -> zählt als fehlend
        let payload = b"PS4-Mac: 001122334455\r\n\
             PS4-RegistKey: 00112233445566778899aabbccddeeff\r\n\
             RP-Key: aabb\r\n";
        assert_eq!(
            parse_response_payload(&info, &mut host, payload).unwrap_err(),
            ChiakiError::InvalidResponse
        );

        // zu langer String wird verworfen, Rest bleibt valide (C: continue)
        let long = "x".repeat(0x30);
        let payload = format!(
            "AP-Ssid: {}\r\nPS4-Mac: 001122334455\r\nPS4-RegistKey: 00112233445566778899aabbccddeeff\r\nRP-Key: aabbccddeeff00112233445566778899\r\n",
            long
        );
        parse_response_payload(&info, &mut host, payload.as_bytes()).expect("parse");
        assert_eq!(host.ap_ssid, "");
        assert!(host.ap_key.is_empty());
    }

    /// rp_application_reason_string (session.c).
    #[test]
    fn application_reason_strings() {
        assert_eq!(
            rp_application_reason_string(0x80108b09),
            "Regist failed, probably invalid PIN"
        );
        assert_eq!(rp_application_reason_string(0x80108b02), "Invalid PSN ID");
        assert_eq!(
            rp_application_reason_string(0x80108b10),
            "Remote is already in use"
        );
        assert_eq!(
            rp_application_reason_string(0x80108b15),
            "Remote Play on Console crashed"
        );
        assert_eq!(rp_application_reason_string(0x80108b11), "RP-Version mismatch");
        assert_eq!(rp_application_reason_string(0x80108bff), "unknown");
        assert_eq!(rp_application_reason_string(0), "unknown");
    }

    #[test]
    fn rp_version_strings() {
        assert_eq!(rp_version_string(Target::Ps4_8), Some("8.0"));
        assert_eq!(rp_version_string(Target::Ps4_9), Some("9.0"));
        assert_eq!(rp_version_string(Target::Ps4_10), Some("10.0"));
        assert_eq!(rp_version_string(Target::Ps5_1), Some("1.0"));
        assert_eq!(rp_version_string(Target::Ps4Unknown), None);
        assert_eq!(rp_version_string(Target::Ps5Unknown), None);
    }

    /// strtoul/strtoull-Semantik der Header-Parsierung.
    #[test]
    fn strtoul_semantics() {
        assert_eq!(strtoul_u32(b"2\r\n", 0), 2);
        assert_eq!(strtoul_u32(b"80108b09", 0x10), 0x8010_8b09);
        assert_eq!(strtoul_u32(b"0x10", 0), 0x10);
        assert_eq!(strtoul_u32(b"  42 junk", 0), 42);
        assert_eq!(strtoull_usize(b"1500"), 1500);
        assert_eq!(strtoull_usize(b"junk"), 0);
    }

    /// End-to-End eines Regist-Response-Empfangs: lokaler TCP-"Server" schickt
    /// HTTP-Header + verschlüsselten Content (getrennt/verzögert), dann
    /// regist_recv_response gegen den Rpcrypt der PS4-pre10-Registierung.
    #[test]
    fn recv_response_full_flow() {
        let crypt = Rpcrypt::new_regist_ps4_pre10(&AMBASSADOR, PIN);

        let plain = b"PS4-Mac: 001122334455\r\n\
             PS4-RegistKey: 00112233445566778899aabbccddeeff\r\n\
             RP-KeyType: 2\r\n\
             RP-Key: aabbccddeeff00112233445566778899\r\n\
             PS4-Nickname: TestPS4\r\n";
        let mut payload = plain.to_vec();
        crypt.encrypt(0, &mut payload).unwrap();

        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            payload.len()
        );

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let payload_clone = payload.clone();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.write_all(header.as_bytes()).unwrap();
            thread::sleep(Duration::from_millis(30));
            sock.write_all(&payload_clone[..10]).unwrap();
            thread::sleep(Duration::from_millis(30));
            sock.write_all(&payload_clone[10..]).unwrap();
            thread::sleep(Duration::from_millis(400));
        });
        let mut sock = TcpStream::connect(addr).unwrap();

        let info = RegistInfo {
            target: Target::Ps4_9,
            host: "127.0.0.1".into(),
            broadcast: false,
            psn_online_id: Some("chiaki-rs".into()),
            psn_account_id: [0; 8],
            pin: PIN,
            console_pin: 4711,
        };
        let mut host = RegisteredHost::default();
        regist_recv_response(&StopPipe::new(), &info, &mut host, &mut sock, &crypt)
            .expect("recv");

        assert_eq!(host.target, Target::Ps4_9);
        assert_eq!(host.server_mac, [0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
        assert_eq!(host.rp_key_type, 2);
        assert_eq!(host.server_nickname, "TestPS4");
        assert_eq!(
            host.rp_key,
            [
                0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66,
                0x77, 0x88, 0x99
            ]
        );
        assert_eq!(host.rp_regist_key[0], 0x00);
        assert_eq!(host.rp_regist_key[1], 0x11);
    }

    /// HTTP != 200 mit RP-Application-Reason -> Unknown.
    #[test]
    fn recv_response_http_error_with_reason() {
        let crypt = Rpcrypt::new_regist_ps4_pre10(&AMBASSADOR, PIN);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.write_all(
                b"HTTP/1.1 403 Forbidden\r\nRP-Application-Reason: 80108b09\r\nContent-Length: 0\r\n\r\n",
            )
            .unwrap();
            thread::sleep(Duration::from_millis(400));
        });
        let mut sock = TcpStream::connect(addr).unwrap();
        let info = RegistInfo {
            target: Target::Ps4_9,
            host: String::new(),
            broadcast: false,
            psn_online_id: None,
            psn_account_id: [0; 8],
            pin: 0,
            console_pin: 0,
        };
        let mut host = RegisteredHost::default();
        let err =
            regist_recv_response(&StopPipe::new(), &info, &mut host, &mut sock, &crypt)
                .unwrap_err();
        assert_eq!(err, ChiakiError::Unknown);
    }

    /// Regist::start/stop/fini: Abbruch über die StopPipe liefert
    /// FinishedCanceled. Ein kleiner "RES3"-Responder lässt die Suche sofort
    /// gelingen; der Flow hängt dann im 100-ms-Sleep (regist.c: "PS4 doesn't
    /// accept requests immediately"), wo stop() ihn mit Canceled abbricht —
    /// bevor der nachfolgende TCP-Connect (refused) mit Network endet.
    #[test]
    fn regist_start_stop_canceled() {
        // Such-Responder: antwortet auf jedes Paket mit "RES3\0" (PS5).
        let responder = create_udp_socket(
            "127.0.0.1:9295".parse().unwrap(),
            &UdpSocketOptions::default(),
        )
        .expect("bind search responder");
        let responder_thread = thread::spawn(move || {
            let mut buf = [0u8; 0x100];
            // blockierend; Test bricht den Socket per Drop ab
            for _ in 0..16 {
                match responder.recv_from(&mut buf) {
                    Ok((_, from)) => {
                        let _ = responder.send_to(b"RES3\0", from);
                    }
                    Err(_) => break,
                }
            }
        });

        let (tx, rx) = mpsc::channel();
        let info = RegistInfo {
            target: Target::Ps5_1,
            host: "127.0.0.1".into(),
            broadcast: false,
            psn_online_id: Some("chiaki-rs".into()),
            psn_account_id: [0; 8],
            pin: 1234,
            console_pin: 0,
        };
        let regist = Regist::start(
            info,
            Arc::new(move |ev| {
                let _ = tx.send(match ev {
                    RegistEvent::FinishedCanceled => "canceled",
                    RegistEvent::FinishedFailed => "failed",
                    RegistEvent::FinishedSuccess(_) => "success",
                });
            }),
        )
        .expect("start");

        // Suche läuft (lokal in wenigen ms), Flow landet im 100-ms-Sleep;
        // stop() bricht diesen mit Canceled ab.
        thread::sleep(Duration::from_millis(50));
        regist.stop();
        regist.fini();

        let ev = rx.recv_timeout(Duration::from_secs(5)).expect("event");
        assert_eq!(ev, "canceled");
        drop(responder_thread);
    }
}
