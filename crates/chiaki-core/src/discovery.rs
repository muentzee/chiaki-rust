// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/discovery.c + lib/include/chiaki/discovery.h (chiaki-ng).
//
// UDP-Broadcast-Protokoll: SRCH-Pings ("SRCH * HTTP/1.1\n...") gehen an
// Port 987 (PS4) bzw. 9302 (PS5); die Konsole antwortet mit einem
// HTTP/1.1-ähnlichen Textpaket (Code 200 = ready, 620 = standby), dessen
// Header die Host-Felder tragen. WAKEUP weckt Konsolen im Standby
// (user-credential = Regist-Key als Hex interpretiert).

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use super::discovery::DiscoveryCmd::{Srch, Wakeup};
use super::error::{ChiakiError, ChiakiResult};
use super::http;
use super::sock;
use super::stoppipe::StopPipe;
use super::Target;

pub const DISCOVERY_PORT_PS4: u16 = 987;
pub const DISCOVERY_PROTOCOL_VERSION_PS4: &str = "00020020";
pub const DISCOVERY_PORT_PS5: u16 = 9302;
pub const DISCOVERY_PROTOCOL_VERSION_PS5: &str = "00030010";
pub const DISCOVERY_PORT_LOCAL_MIN: u16 = 9303;
pub const DISCOVERY_PORT_LOCAL_MAX: u16 = 9319;

/// C: `sizeof(buf)` in chiaki_discovery_send / discovery_thread_func.
const DISCOVERY_BUF_SIZE: usize = 512;

/// Poll-Intervall des Empfangs-Threads für die StopPipe-Prüfung
/// (C: select() mit StopPipe-Event; Rust: kurzes SO_RCVTIMEO, s. stoppipe.rs).
const RECV_POLL: Duration = Duration::from_millis(100);

/// Port von `ChiakiDiscoveryCmd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryCmd {
    Srch,
    Wakeup,
}

/// Port von `ChiakiDiscoveryPacket`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryPacket {
    pub cmd: DiscoveryCmd,
    /// C: char*, NULL ist möglich (dann kann das Paket nicht formatiert werden)
    pub protocol_version: Option<String>,
    /// für wakeup: Regist-Key als Hex interpretiert
    pub user_credential: u64,
}

/// Port von `ChiakiDiscoveryHostState`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DiscoveryHostState {
    #[default]
    Unknown,
    Ready,
    Standby,
}

/// Port von `chiaki_discovery_host_state_string()`.
pub fn discovery_host_state_string(state: DiscoveryHostState) -> &'static str {
    match state {
        DiscoveryHostState::Ready => "ready",
        DiscoveryHostState::Standby => "standby",
        DiscoveryHostState::Unknown => "unknown",
    }
}

/// Port von `ChiakiDiscoveryHost`.
///
/// C: String-Member sind `const char*` und können NULL sein (Header fehlte in
/// der Antwort); Rust: `Option<String>`. `host_addr` kommt aus der
/// Absender-Adresse (C: `sockaddr_str`, nur die IP ohne Port).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DiscoveryHost {
    pub state: DiscoveryHostState,
    pub host_request_port: u16,
    pub host_addr: String,
    pub system_version: Option<String>,
    pub device_discovery_protocol_version: Option<String>,
    pub host_name: Option<String>,
    pub host_type: Option<String>,
    pub host_id: Option<String>,
    pub running_app_titleid: Option<String>,
    pub running_app_name: Option<String>,
}

impl DiscoveryHost {
    /// Port von `chiaki_discovery_host_is_ps5()`.
    pub fn is_ps5(&self) -> bool {
        self.device_discovery_protocol_version.as_deref() == Some(DISCOVERY_PROTOCOL_VERSION_PS5)
    }

    /// Port von `chiaki_discovery_host_system_version_target()`:
    /// übersetzt die entdeckte `system_version` in ein [`Target`].
    pub fn system_version_target(&self) -> Target {
        // atoi(system_version)
        let version = atoi_i32(self.system_version.as_deref().unwrap_or(""));
        let is_ps5 = self.is_ps5();

        if version >= 8050001 && is_ps5 {
            // PS5 >= 1.0
            return Target::Ps5_1;
        }
        if version >= 8050000 && is_ps5 {
            // PS5 >= 0
            return Target::Ps5Unknown;
        }

        if version >= 8000000 {
            // PS4 >= 8.0
            return Target::Ps4_10;
        }
        if version >= 7000000 {
            // PS4 >= 7.0
            return Target::Ps4_9;
        }
        if version > 0 {
            return Target::Ps4_8;
        }

        Target::Ps4Unknown
    }
}

/// Port von `chiaki_discovery_packet_fmt()`.
///
/// Gibt den formatierten Pakettext (ohne das im C-Protokoll angehängte
/// NUL-Byte) zurück; `None` entspricht dem C-Rückgabewert -1 (fehlende
/// protocol_version).
pub fn packet_fmt(packet: &DiscoveryPacket) -> Option<String> {
    let protocol_version = packet.protocol_version.as_deref()?;
    match packet.cmd {
        Srch => Some(format!(
            "SRCH * HTTP/1.1\ndevice-discovery-protocol-version:{protocol_version}\n"
        )),
        Wakeup => Some(format!(
            "WAKEUP * HTTP/1.1\n\
             client-type:vr\n\
             auth-type:R\n\
             model:w\n\
             app-type:r\n\
             user-credential:{}\n\
             device-discovery-protocol-version:{protocol_version}\n",
            packet.user_credential
        )),
    }
}

/// Port von `chiaki_discovery_srch_response_parse()`.
///
/// `addr` ist die Absender-Adresse des Antwortpakets (landet als IP in
/// `host_addr`), `buf` die empfangenen Bytes.
pub fn srch_response_parse(addr: SocketAddr, buf: &[u8]) -> ChiakiResult<DiscoveryHost> {
    let http_response = http::response_parse(buf)?;

    // C: memset(0) — alle String-Felder initial NULL.
    let mut host = DiscoveryHost {
        host_addr: sockaddr_ip_str(&addr),
        ..Default::default()
    };

    host.state = match http_response.code {
        200 => DiscoveryHostState::Ready,
        620 => DiscoveryHostState::Standby,
        _ => DiscoveryHostState::Unknown,
    };

    // C iteriert die prepend-verkettete Liste rückwärts und überschreibt;
    // der letzte Schreibzugriff ist die erste Okkurrenz im Dokument —
    // hier: first-wins.
    let mut host_request_port: Option<u16> = None;
    for header in &http_response.headers {
        let set = |slot: &mut Option<String>, v: &str| {
            if slot.is_none() {
                *slot = Some(v.to_owned());
            }
        };
        match header.key.as_str() {
            "system-version" => set(&mut host.system_version, &header.value),
            "device-discovery-protocol-version" => {
                set(&mut host.device_discovery_protocol_version, &header.value)
            }
            "host-request-port" => {
                if host_request_port.is_none() {
                    host_request_port = Some(strtoul(&header.value) as u16);
                }
            }
            "host-name" => set(&mut host.host_name, &header.value),
            "host-type" => set(&mut host.host_type, &header.value),
            "host-id" => set(&mut host.host_id, &header.value),
            "running-app-titleid" => set(&mut host.running_app_titleid, &header.value),
            "running-app-name" => set(&mut host.running_app_name, &header.value),
            //_ => printf("unknown %s: %s\n", ...) (im C auskommentiert)
            _ => {}
        }
    }
    host.host_request_port = host_request_port.unwrap_or(0);

    Ok(host)
}

/// Port von `ChiakiDiscovery`.
pub struct Discovery {
    socket: UdpSocket,
    local_addr: SocketAddr,
}

impl Discovery {
    /// Port von `chiaki_discovery_init()`.
    ///
    /// `ipv6` entspricht der Adressfamilie (C: `sa_family_t`). Bindet
    /// nacheinander die lokalen Ports DISCOVERY_PORT_LOCAL_MIN..=MAX, fällt
    /// dann auf einen zufälligen Port zurück (C-Verhalten).
    pub fn new(ipv6: bool) -> ChiakiResult<Discovery> {
        let bind_ip: std::net::IpAddr = if ipv6 {
            std::net::Ipv6Addr::UNSPECIFIED.into()
        } else {
            std::net::Ipv4Addr::UNSPECIFIED.into()
        };

        // Zuerst 9303..=9319, dann 0 (zufällig)
        let mut port = DISCOVERY_PORT_LOCAL_MIN;
        let mut bound: Option<UdpSocket> = None;
        loop {
            let addr = SocketAddr::new(bind_ip, port);
            match sock::create_udp_socket(addr, &sock::UdpSocketOptions::default()) {
                Ok(s) => {
                    bound = Some(s);
                    break;
                }
                Err(_e) => {
                    if port == 0 {
                        break;
                    }
                    if port == DISCOVERY_PORT_LOCAL_MAX {
                        tracing::info!(
                            "Discovery failed to bind port {}, trying random",
                            port
                        );
                        port = 0;
                    } else {
                        port += 1;
                        tracing::info!(
                            "Discovery failed to bind port {}, trying one higher",
                            port
                        );
                    }
                }
            }
        }

        let socket = bound.ok_or(ChiakiError::Network)?;

        // C: setsockopt(SO_BROADCAST) — Fehler wird geloggt, aber nicht
        // fatal (wie im C).
        if let Err(e) = sock::set_broadcast(&socket, true) {
            tracing::error!("Discovery failed to setsockopt SO_BROADCAST: {e:?}");
        }

        let local_addr = socket
            .local_addr()
            .map_err(|_| ChiakiError::Network)?;

        Ok(Discovery { socket, local_addr })
    }

    /// Lokale Adresse (C: `discovery->local_addr`).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Port von `chiaki_discovery_send()`.
    ///
    /// Sendet das formatierte Paket (+ NUL-Byte, wie im C) an `addr`.
    pub fn send(&self, packet: &DiscoveryPacket, addr: SocketAddr) -> ChiakiResult<()> {
        if addr.is_ipv6() != self.local_addr.is_ipv6() {
            return Err(ChiakiError::InvalidData);
        }

        let formatted = packet_fmt(packet).ok_or(ChiakiError::Unknown)?;
        if formatted.len() + 1 >= DISCOVERY_BUF_SIZE {
            return Err(ChiakiError::BufTooSmall);
        }

        // sendto_broadcast() ist unter Windows (Zielplattform) sendto().
        let mut bytes = formatted.into_bytes();
        bytes.push(0); // C: sendet len + 1 inkl. NUL
        let rc = sock::send_to(&self.socket, &bytes, addr);
        if rc.is_err() && addr.is_ipv4() {
            tracing::error!("Discovery failed to send");
            return Err(ChiakiError::Network);
        }
        // IPv6-Fehler werden wie im C ignoriert.

        Ok(())
    }
}

/// Port von `ChiakiDiscoveryCb`.
pub type DiscoveryCb = Arc<dyn Fn(DiscoveryHost) + Send + Sync>;

/// Port von `ChiakiDiscoveryThread`
/// (`chiaki_discovery_thread_start` / `..._start_oneshot` / `..._stop`).
pub struct DiscoveryThread {
    stop_pipe: Arc<StopPipe>,
    thread: Option<JoinHandle<()>>,
}

impl DiscoveryThread {
    /// Port von `chiaki_discovery_thread_start()`: empfängt dauerhaft.
    pub fn start(discovery: Arc<Discovery>, cb: DiscoveryCb) -> ChiakiResult<DiscoveryThread> {
        DiscoveryThread::spawn(discovery, cb, false)
    }

    /// Port von `chiaki_discovery_thread_start_oneshot()`: bricht nach dem
    /// ersten empfangenen Host ab.
    pub fn start_oneshot(discovery: Arc<Discovery>, cb: DiscoveryCb) -> ChiakiResult<DiscoveryThread> {
        DiscoveryThread::spawn(discovery, cb, true)
    }

    fn spawn(discovery: Arc<Discovery>, cb: DiscoveryCb, oneshot: bool) -> ChiakiResult<DiscoveryThread> {
        let stop_pipe = Arc::new(StopPipe::new());
        let sp = Arc::clone(&stop_pipe);
        let thread = std::thread::Builder::new()
            .name("Chiaki Discovery".to_owned())
            .spawn(move || discovery_thread_func(&sp, &discovery, &cb, oneshot))
            .map_err(|_| ChiakiError::Thread)?;
        Ok(DiscoveryThread {
            stop_pipe,
            thread: Some(thread),
        })
    }

    /// Port von `chiaki_discovery_thread_stop()`: stoppt und joint den Thread.
    pub fn stop(mut self) -> ChiakiResult<()> {
        self.stop_pipe.stop();
        match self.thread.take() {
            Some(t) => t.join().map(|_| ()).map_err(|_| ChiakiError::Thread),
            None => Ok(()),
        }
    }
}

/// Port von `discovery_thread_func` / `discovery_thread_func_oneshot`.
fn discovery_thread_func(
    stop_pipe: &StopPipe,
    discovery: &Discovery,
    cb: &DiscoveryCb,
    oneshot: bool,
) {
    let mut buf = [0u8; DISCOVERY_BUF_SIZE];
    loop {
        // C: chiaki_stop_pipe_select_single(stop_pipe, socket, false, UINT64_MAX)
        // -> Canceled beendet den Thread.
        if stop_pipe.check().is_err() {
            break;
        }

        let (n, client_addr) = match sock::recv_from_timeout(&discovery.socket, &mut buf, RECV_POLL)
        {
            Ok(r) => r,
            Err(ChiakiError::Timeout) => continue,
            Err(_e) => {
                tracing::error!("Discovery thread failed to read from socket");
                break;
            }
        };

        if n == 0 {
            continue;
        }
        let n = n.min(buf.len() - 1);

        let host = match srch_response_parse(client_addr, &buf[..n]) {
            Ok(h) => h,
            Err(_e) => {
                tracing::info!("Discovery Response invalid");
                continue;
            }
        };

        cb(host);
        if oneshot {
            // C oneshot: break nach dem Callback
            break;
        }
    }
}

/// Port von `chiaki_discovery_wakeup()`.
///
/// `discovery` darf `None` sein — dann wird eine temporäre Discovery erstellt
/// (wie im C).
pub fn wakeup(
    discovery: Option<&Discovery>,
    host: &str,
    user_credential: u64,
    ps5: bool,
) -> ChiakiResult<()> {
    // getaddrinfo-Ersatz; C erzwingt IPv4, außer der Host enthält ':' (TODO
    // im C: "this blocks, use something else" — to_socket_addrs blockt ebenso).
    let mut resolved = (host, 0u16)
        .to_socket_addrs()
        .map_err(|e| {
            tracing::error!("DiscoveryManager failed to getaddrinfo for wakeup: {e}");
            ChiakiError::Network
        })?;

    let want_ipv6 = host.contains(':');
    let Some(mut addr) = resolved.find(|a: &SocketAddr| a.is_ipv6() == want_ipv6) else {
        tracing::error!("DiscoveryManager failed to get suitable address from getaddrinfo for wakeup");
        return Err(ChiakiError::Unknown);
    };
    addr.set_port(if ps5 {
        DISCOVERY_PORT_PS5
    } else {
        DISCOVERY_PORT_PS4
    });

    let packet = DiscoveryPacket {
        cmd: Wakeup,
        protocol_version: Some(
            if ps5 {
                DISCOVERY_PROTOCOL_VERSION_PS5
            } else {
                DISCOVERY_PROTOCOL_VERSION_PS4
            }
            .to_owned(),
        ),
        user_credential,
    };

    match discovery {
        Some(d) => d.send(&packet, addr),
        None => {
            let tmp = Discovery::new(addr.is_ipv6())
                .inspect_err(|e| tracing::error!("Failed to init temporary discovery for wakeup: {e:?}"))?;
            tmp.send(&packet, addr)
        }
    }
}

/// Port von `sockaddr_str()` (utils.h): nur die IP, ohne Port.
fn sockaddr_ip_str(addr: &SocketAddr) -> String {
    // C liefert NULL für andere Familien — Rust kennt nur v4/v6.
    addr.ip().to_string()
}

/// `atoi()`-Semantik: Whitespace, Vorzeichen, Dezimalziffern, sonst 0.
fn atoi_i32(s: &str) -> i32 {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    let mut sign = 1i64;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        if b[i] == b'-' {
            sign = -1;
        }
        i += 1;
    }
    let mut val = 0i64;
    while i < b.len() && b[i].is_ascii_digit() {
        val = (val * 10 + (b[i] - b'0') as i64).clamp(i32::MIN as i64, i32::MAX as i64);
        i += 1;
    }
    (sign * val) as i32
}

/// `strtoul(s, NULL, 0)`-Semantik: Whitespace/Vorzeichen überspringen,
/// `0x`/`0X` → Hex, führende `0` → Oktal, sonst Dezimal; saturierend.
pub(crate) fn strtoul(s: &str) -> u64 {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    let mut negative = false;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        negative = b[i] == b'-';
        i += 1;
    }
    let mut base = 10u32;
    if i + 1 < b.len() && b[i] == b'0' && (b[i + 1] == b'x' || b[i + 1] == b'X') {
        base = 16;
        i += 2;
    } else if i < b.len() && b[i] == b'0' {
        base = 8;
    }
    let mut val = 0u64;
    while i < b.len() {
        let Some(d) = (b[i] as char).to_digit(base) else { break };
        val = val.saturating_mul(base as u64).saturating_add(d as u64);
        i += 1;
    }
    if negative {
        val.wrapping_neg()
    } else {
        val
    }
}

// Fehlerkind-Helper für Tests: WouldBlock/TimedOut → Timeout.
#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
    use std::thread;
    use std::time::Instant;

    fn local_v4(port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
    }

    /// SRCH-Format stringidentisch zum C-snprintf.
    #[test]
    fn packet_fmt_srch() {
        let p = DiscoveryPacket {
            cmd: Srch,
            protocol_version: Some(DISCOVERY_PROTOCOL_VERSION_PS4.to_owned()),
            user_credential: 0,
        };
        assert_eq!(
            packet_fmt(&p).unwrap(),
            "SRCH * HTTP/1.1\ndevice-discovery-protocol-version:00020020\n"
        );

        let p5 = DiscoveryPacket {
            cmd: Srch,
            protocol_version: Some(DISCOVERY_PROTOCOL_VERSION_PS5.to_owned()),
            user_credential: 0,
        };
        assert_eq!(
            packet_fmt(&p5).unwrap(),
            "SRCH * HTTP/1.1\ndevice-discovery-protocol-version:00030010\n"
        );
    }

    /// WAKEUP-Format inkl. user-credential (%llu).
    #[test]
    fn packet_fmt_wakeup() {
        let p = DiscoveryPacket {
            cmd: Wakeup,
            protocol_version: Some(DISCOVERY_PROTOCOL_VERSION_PS4.to_owned()),
            user_credential: 0x1122334455667788,
        };
        assert_eq!(
            packet_fmt(&p).unwrap(),
            "WAKEUP * HTTP/1.1\n\
             client-type:vr\n\
             auth-type:R\n\
             model:w\n\
             app-type:r\n\
             user-credential:1234605616436508552\n\
             device-discovery-protocol-version:00020020\n"
        );

        // fehlende protocol_version -> C: -1
        let p = DiscoveryPacket { cmd: Srch, protocol_version: None, user_credential: 0 };
        assert!(packet_fmt(&p).is_none());
    }

    /// Synthetische PS4-SRCH-Antwort (Layout wie von der Konsole).
    #[test]
    fn srch_response_parse_ps4() {
        let resp: &[u8] = b"HTTP/1.1 200 Ok\n\
             host-type:PS4\n\
             system-version:07050001\n\
             device-discovery-protocol-version:00020020\n\
             host-request-port:9295\n\
             host-name:My PS4\n\
             host-id:C0FFEE1122334455\n\
             running-app-titleid:CUSA00999\n\
             running-app-name:Bloodborne\n";
        let host = srch_response_parse(local_v4(9296), resp).unwrap();

        assert_eq!(host.state, DiscoveryHostState::Ready);
        assert_eq!(host.host_addr, "127.0.0.1");
        assert_eq!(host.host_request_port, 9295);
        assert_eq!(host.host_type.as_deref(), Some("PS4"));
        assert_eq!(host.system_version.as_deref(), Some("07050001"));
        assert_eq!(
            host.device_discovery_protocol_version.as_deref(),
            Some("00020020")
        );
        assert_eq!(host.host_name.as_deref(), Some("My PS4"));
        assert_eq!(host.host_id.as_deref(), Some("C0FFEE1122334455"));
        assert_eq!(host.running_app_titleid.as_deref(), Some("CUSA00999"));
        assert_eq!(host.running_app_name.as_deref(), Some("Bloodborne"));

        assert!(!host.is_ps5());
        // 07050001 -> PS4 >= 7.0
        assert_eq!(host.system_version_target(), Target::Ps4_9);
    }

    /// PS5-Antwort: Standby (620), PS5-Erkennung + Target-Mapping.
    #[test]
    fn srch_response_parse_ps5_standby() {
        let resp: &[u8] = b"HTTP/1.1 620 Bulk Only\n\
             host-type:PS5\n\
             system-version:09050000\n\
             device-discovery-protocol-version:00030010\n\
             host-request-port:9295\n\
             host-name:My PS5\n\
             host-id:0123456789ABCDEF\n";
        let host = srch_response_parse(local_v4(1000), resp).unwrap();

        assert_eq!(host.state, DiscoveryHostState::Standby);
        assert!(host.is_ps5());
        // 09050000 >= 8050001 && ps5 -> PS5_1
        assert_eq!(host.system_version_target(), Target::Ps5_1);
        assert_eq!(host.running_app_name, None);
        assert_eq!(host.host_request_port, 9295);
    }

    /// Unbekannter Code + fehlende Header -> Default-Werte.
    #[test]
    fn srch_response_parse_defaults() {
        let resp: &[u8] = b"HTTP/1.1 500 Oops\nsome-random:x\n";
        let host = srch_response_parse(local_v4(1), resp).unwrap();
        assert_eq!(host.state, DiscoveryHostState::Unknown);
        assert_eq!(host.host_request_port, 0);
        assert_eq!(host.host_addr, "127.0.0.1");
        assert!(!host.is_ps5());
        // system_version fehlt -> atoi("") = 0 -> PS4_UNKNOWN
        assert_eq!(host.system_version_target(), Target::Ps4Unknown);

        // komplett invalid
        assert!(srch_response_parse(local_v4(1), b"garbage").is_err());
    }

    /// system_version_target-Matrix (aus discovery.c abgeleitet).
    #[test]
    fn system_version_target_matrix() {
        let mk = |sys: &str, proto: &str| DiscoveryHost {
            system_version: Some(sys.to_owned()),
            device_discovery_protocol_version: Some(proto.to_owned()),
            ..Default::default()
        };
        let ps4 = DISCOVERY_PROTOCOL_VERSION_PS4;
        let ps5 = DISCOVERY_PROTOCOL_VERSION_PS5;

        assert_eq!(mk("08050000", ps4).system_version_target(), Target::Ps4_10);
        assert_eq!(mk("07099999", ps4).system_version_target(), Target::Ps4_9);
        assert_eq!(mk("06500000", ps4).system_version_target(), Target::Ps4_8);
        assert_eq!(mk("00000000", ps4).system_version_target(), Target::Ps4Unknown);
        // PS5-Firmware-Nummern sind >= 8050000, aber nur mit PS5-Protokoll PS5:
        assert_eq!(mk("08050000", ps4).system_version_target(), Target::Ps4_10);
        assert_eq!(mk("08050001", ps5).system_version_target(), Target::Ps5_1);
        assert_eq!(mk("08050000", ps5).system_version_target(), Target::Ps5Unknown);
        assert_eq!(mk("00000000", ps5).system_version_target(), Target::Ps4Unknown);
    }

    /// state_string-Werte.
    #[test]
    fn host_state_strings() {
        assert_eq!(discovery_host_state_string(DiscoveryHostState::Ready), "ready");
        assert_eq!(discovery_host_state_string(DiscoveryHostState::Standby), "standby");
        assert_eq!(discovery_host_state_string(DiscoveryHostState::Unknown), "unknown");
    }

    /// strtoul()-Semantik (base 0) für Header-Werte.
    #[test]
    fn strtoul_semantics() {
        assert_eq!(strtoul("9295"), 9295);
        assert_eq!(strtoul("0x10"), 16);
        assert_eq!(strtoul("0Xff"), 255);
        assert_eq!(strtoul("010"), 8); // C: Oktal bei base 0
        assert_eq!(strtoul(" 42abc "), 42);
        assert_eq!(strtoul("xyz"), 0);
        assert_eq!(strtoul(""), 0);
    }

    /// Ende-zu-Ende über localhost: Discovery.send -> UdpSocket -> Parse.
    #[test]
    fn send_and_parse_over_socket() {
        let discovery = Discovery::new(false).unwrap();
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = receiver.local_addr().unwrap();

        let packet = DiscoveryPacket {
            cmd: Srch,
            protocol_version: Some(DISCOVERY_PROTOCOL_VERSION_PS4.to_owned()),
            user_credential: 0,
        };
        discovery.send(&packet, addr).unwrap();

        let mut buf = [0u8; 512];
        receiver
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (n, from) = receiver.recv_from(&mut buf).unwrap();
        // Paket inkl. NUL-Byte (wie im C gesendet)
        assert_eq!(n, buf[..n].len());
        assert_eq!(buf[n - 1], 0);

        // Absender-Port der Discovery liegt in 9303..=9319 oder zufällig
        // (gebunden an 0.0.0.0, daher nur Port-Vergleich)
        assert_eq!(from.port(), discovery.local_addr().port());

        let resp: &[u8] = b"HTTP/1.1 200 Ok\nhost-request-port:9295\nhost-id:ABCD\n";
        receiver.send_to(resp, from).unwrap();

        let mut rbuf = [0u8; 512];
        let (n2, _) = sock::recv_from_timeout(&discovery.socket, &mut rbuf, Duration::from_secs(2))
            .unwrap();
        let host = srch_response_parse(addr, &rbuf[..n2]).unwrap();
        assert_eq!(host.state, DiscoveryHostState::Ready);
        assert_eq!(host.host_id.as_deref(), Some("ABCD"));
        assert_eq!(host.host_addr, "127.0.0.1");
    }

    /// DiscoveryThread end-to-end: Fake-Konsole antwortet auf SRCH.
    #[test]
    fn discovery_thread_receives_host() {
        let discovery = Arc::new(Discovery::new(false).unwrap());
        let fake = UdpSocket::bind("127.0.0.1:0").unwrap();
        let fake_addr = fake.local_addr().unwrap();

        let responder = std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            fake.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let (n, from) = fake.recv_from(&mut buf).unwrap();
            let req = &buf[..n];
            assert!(req.starts_with(b"SRCH * HTTP/1.1\n"));
            let resp: &[u8] = b"HTTP/1.1 200 Ok\n\
                 host-type:PS4\n\
                 system-version:07000001\n\
                 device-discovery-protocol-version:00020020\n\
                 host-request-port:9295\n\
                 host-name:Fake\n\
                 host-id:DEADBEEF\n";
            let mut pkt = resp.to_vec();
            pkt.push(0);
            fake.send_to(&pkt, from).unwrap();
            // Socket offen halten
            thread::sleep(Duration::from_millis(600));
        });

        let (tx, rx) = std::sync::mpsc::channel();
        let cb: DiscoveryCb = Arc::new(move |host| {
            let _ = tx.send(host);
        });

        let thread = DiscoveryThread::start(Arc::clone(&discovery), cb).unwrap();
        discovery.send(
            &DiscoveryPacket {
                cmd: Srch,
                protocol_version: Some(DISCOVERY_PROTOCOL_VERSION_PS4.to_owned()),
                user_credential: 0,
            },
            fake_addr,
        )
        .unwrap();

        let host = rx.recv_timeout(Duration::from_secs(3)).expect("host received");
        assert_eq!(host.host_name.as_deref(), Some("Fake"));
        assert_eq!(host.host_id.as_deref(), Some("DEADBEEF"));
        assert_eq!(host.state, DiscoveryHostState::Ready);

        thread.stop().unwrap();
        responder.join().unwrap();
    }

    /// One-Shot-Thread bricht nach dem ersten Host ab.
    #[test]
    fn discovery_thread_oneshot_stops_after_first() {
        let discovery = Arc::new(Discovery::new(false).unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        let cb: DiscoveryCb = Arc::new(move |host| {
            let _ = tx.send(host);
        });
        let thread = DiscoveryThread::start_oneshot(Arc::clone(&discovery), cb).unwrap();

        // Direkt von "unten" ein gültiges Antwortpaket an die Discovery senden
        // (Discovery ist an 0.0.0.0 gebunden -> über loopback adressieren)
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let resp: &[u8] = b"HTTP/1.1 620 x\nhost-id:ONESHOT\n";
        sender
            .send_to(resp, local_v4(discovery.local_addr().port()))
            .unwrap();

        let host = rx.recv_timeout(Duration::from_secs(3)).expect("one host");
        assert_eq!(host.host_id.as_deref(), Some("ONESHOT"));

        // Der Thread ist nach dem Callback beendet; stop() muss trotzdem
        // sauber joints.
        thread.stop().unwrap();
    }

    /// wakeup(): temporäre Discovery, WAKEUP-Paket an lokalen Empfänger.
    ///
    /// wakeup() adressiert den festen Discovery-Port (987/9302); der Test
    /// bindet daher kurz 127.0.0.1:987 und überspringt sich, wenn der Port
    /// belegt ist.
    #[test]
    fn wakeup_sends_packet() {
        let Ok(receiver) = UdpSocket::bind("127.0.0.1:987") else {
            // Port belegt (z. B. laufender Dienst) -> Test überspringen
            return;
        };
        let handle = std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            receiver.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let (n, _) = receiver.recv_from(&mut buf).unwrap();
            buf[..n].to_vec()
        });

        wakeup(None, "127.0.0.1", 0xdeadbeef, false).unwrap();

        let received = handle.join().unwrap();
        let packet = DiscoveryPacket {
            cmd: Wakeup,
            protocol_version: Some(DISCOVERY_PROTOCOL_VERSION_PS4.to_owned()),
            user_credential: 0xdeadbeef,
        };
        let mut expected = packet_fmt(&packet).unwrap().into_bytes();
        expected.push(0);
        assert_eq!(received, expected);

        // Portkonstanten, die wakeup() verwendet:
        assert_eq!(DISCOVERY_PORT_PS4, 987);
        assert_eq!(DISCOVERY_PORT_PS5, 9302);
    }

    /// recv-Poll: StopPipe beendet den Thread auch ohne Paketverkehr.
    #[test]
    fn discovery_thread_stop_without_traffic() {
        let discovery = Arc::new(Discovery::new(false).unwrap());
        let (tx, _rx) = std::sync::mpsc::channel::<DiscoveryHost>();
        let cb: DiscoveryCb = Arc::new(move |h| {
            let _ = tx.send(h);
        });
        let thread = DiscoveryThread::start(discovery, cb).unwrap();
        thread::sleep(Duration::from_millis(50));
        let t = Instant::now();
        thread.stop().unwrap();
        assert!(t.elapsed() < Duration::from_secs(2));
    }
}
