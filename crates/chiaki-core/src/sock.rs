// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/sock.c + lib/include/chiaki/sock.h (chiaki-ng) sowie der
// darauf aufbauenden Socket-Handgriffe, die takion.c / discovery.c / regist.c
// bisher inline erledigen (UDP-Socket anlegen, IP_DONTFRAG, SO_RCVBUF,
// SO_BROADCAST, Sende-/Empfangs-Timeouts, sendto/recvfrom mit Timeout).
//
// Umsetzung: std::net + socket2 (sicheres API für setsockopt & Co.).
// Einzige Ausnahme: IP_DONTFRAGMENT / IPV6_DONTFRAG bietet socket2 nicht an;
// dafür gibt es genau eine minimale, mit `#[allow(unsafe_code)]` markierte
// setsockopt-Funktion über die windows-Crate (siehe `set_dont_fragment_raw`).
//
// Zielplattform ist Windows (WSA). Fehlercodes werden wie in stoppipe.c /
// takion.c auf ChiakiErrorCode gemappt (`map_io_error`).
//
// Socket-Schließen: `CHIAKI_SOCKET_CLOSE(s)` entspricht dem Drop des std-Sockets
// (`close()` unten nur zur API-Parität).

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

use socket2::{Domain, Protocol, SockRef, Socket, Type};

use super::error::{ChiakiError, ChiakiResult};

/// Optionen für [`create_udp_socket`] — entsprechen den setsockopt-Aufrufen,
/// die takion/discovery/regist direkt nach `socket()` machen.
#[derive(Debug, Clone, Default)]
pub struct UdpSocketOptions {
    /// `chiaki_socket_set_nonblock(sock, true)` nach dem Bind.
    pub nonblocking: bool,
    /// SO_BROADCAST (discovery: Broadcast-Suche im LAN).
    pub broadcast: bool,
    /// IP_DONTFRAG / IP_DONTFRAGMENT (takion: keine IP-Fragmentierung, MTU-Probing).
    pub dont_fragment: bool,
    /// SO_RCVBUF (takion: großer Empfangspuffer für Videodaten).
    pub recv_buffer_size: Option<usize>,
    /// SO_REUSEADDR (vor dem Bind gesetzt).
    pub reuse_address: bool,
    /// SO_RCVTIMEO
    pub read_timeout: Option<Duration>,
    /// SO_SNDTIMEO
    pub write_timeout: Option<Duration>,
}

/// io::Error → ChiakiError, analog zu den WSA-Fehlerzuordnungen in
/// stoppipe.c (`chiaki_stop_pipe_connect`) und takion.c.
pub fn map_io_error(e: &io::Error) -> ChiakiError {
    use io::ErrorKind::*;
    match e.kind() {
        ConnectionRefused => return ChiakiError::ConnectionRefused,
        TimedOut | WouldBlock => return ChiakiError::Timeout,
        ConnectionReset | ConnectionAborted | NotConnected | BrokenPipe => {
            return ChiakiError::Disconnected
        }
        Interrupted => return ChiakiError::Canceled,
        _ => {}
    }
    // WSA-Codes, die std nicht als eigenen ErrorKind kennt
    match e.raw_os_error() {
        Some(10060) => ChiakiError::Timeout,           // WSAETIMEDOUT
        Some(10061) => ChiakiError::ConnectionRefused, // WSAECONNREFUSED
        Some(10064) => ChiakiError::HostDown,          // WSAEHOSTDOWN
        Some(10065) => ChiakiError::HostUnreach,       // WSAEHOSTUNREACH
        Some(10051) => ChiakiError::HostUnreach,       // WSAENETUNREACH
        Some(10054) => ChiakiError::Disconnected,      // WSAECONNRESET
        _ => ChiakiError::Network,
    }
}

fn io_err(context: &str, e: io::Error) -> ChiakiError {
    let code = map_io_error(&e);
    tracing::debug!("sock: {context} failed: {e} -> {code:?}");
    code
}

/// `chiaki_socket_create`-Äquivalent für UDP: `socket()` + Optionen + `bind()`.
///
/// Die Adressfamilie ergibt sich aus `bind_addr` (0.0.0.0:0 bzw. [::]:0 für
/// "beliebig"). Optionen, die vor dem Bind gesetzt werden müssen
/// (SO_REUSEADDR), werden in der richtigen Reihenfolge angewandt.
pub fn create_udp_socket(bind_addr: SocketAddr, opts: &UdpSocketOptions) -> ChiakiResult<UdpSocket> {
    let domain = if bind_addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let sock = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))
        .map_err(|e| io_err("socket()", e))?;

    if opts.reuse_address {
        sock.set_reuse_address(true)
            .map_err(|e| io_err("SO_REUSEADDR", e))?;
    }
    if opts.broadcast {
        sock.set_broadcast(true)
            .map_err(|e| io_err("SO_BROADCAST", e))?;
    }
    if let Some(size) = opts.recv_buffer_size {
        sock.set_recv_buffer_size(size)
            .map_err(|e| io_err("SO_RCVBUF", e))?;
    }
    if opts.dont_fragment {
        set_dont_fragment_socket(&sock, bind_addr.is_ipv6(), true)?;
    }
    sock.set_read_timeout(opts.read_timeout)
        .map_err(|e| io_err("SO_RCVTIMEO", e))?;
    sock.set_write_timeout(opts.write_timeout)
        .map_err(|e| io_err("SO_SNDTIMEO", e))?;

    sock.bind(&bind_addr.into())
        .map_err(|e| io_err("bind()", e))?;

    if opts.nonblocking {
        sock.set_nonblocking(true)
            .map_err(|e| io_err("FIONBIO", e))?;
    }

    Ok(sock.into())
}

/// `CHIAKI_SOCKET_CLOSE(s)`: Sockets schließen per Drop — Funktion nur zur
/// Lesbarkeit an den Portierungsstellen.
pub fn close<S>(sock: S) {
    drop(sock);
}

/// Port von `chiaki_socket_set_nonblock()` (ioctlsocket FIONBIO).
/// Funktioniert für alle std-Sockets (UdpSocket, TcpStream, TcpListener).
pub fn set_nonblock<'s, S>(sock: &'s S, nonblock: bool) -> ChiakiResult<()>
where
    SockRef<'s>: From<&'s S>,
{
    SockRef::from(sock)
        .set_nonblocking(nonblock)
        .map_err(|_| ChiakiError::Unknown) // C: CHIAKI_ERR_UNKNOWN
}

/// SO_BROADCAST nachträglich setzen.
pub fn set_broadcast(sock: &UdpSocket, on: bool) -> ChiakiResult<()> {
    SockRef::from(sock)
        .set_broadcast(on)
        .map_err(|e| io_err("SO_BROADCAST", e))
}

/// SO_RCVBUF nachträglich setzen.
pub fn set_recv_buffer_size(sock: &UdpSocket, size: usize) -> ChiakiResult<()> {
    SockRef::from(sock)
        .set_recv_buffer_size(size)
        .map_err(|e| io_err("SO_RCVBUF", e))
}

/// SO_RCVBUF auslesen (zur Kontrolle, z. B. Log in takion).
pub fn recv_buffer_size(sock: &UdpSocket) -> ChiakiResult<usize> {
    SockRef::from(sock)
        .recv_buffer_size()
        .map_err(|e| io_err("SO_RCVBUF (get)", e))
}

/// SO_RCVTIMEO setzen (`None` = blockierend ohne Timeout).
pub fn set_read_timeout(sock: &UdpSocket, timeout: Option<Duration>) -> ChiakiResult<()> {
    sock.set_read_timeout(timeout)
        .map_err(|e| io_err("SO_RCVTIMEO", e))
}

/// SO_SNDTIMEO setzen (`None` = blockierend ohne Timeout).
pub fn set_write_timeout(sock: &UdpSocket, timeout: Option<Duration>) -> ChiakiResult<()> {
    sock.set_write_timeout(timeout)
        .map_err(|e| io_err("SO_SNDTIMEO", e))
}

/// IP_DONTFRAG (Windows: IP_DONTFRAGMENT / IPV6_DONTFRAG) auf einem
/// gebundenen UDP-Socket setzen. Die Adressfamilie wird über `local_addr()`
/// bestimmt (Socket muss gebunden sein).
pub fn set_dont_fragment(sock: &UdpSocket, on: bool) -> ChiakiResult<()> {
    let ipv6 = sock
        .local_addr()
        .map_err(|e| io_err("getsockname()", e))?
        .is_ipv6();
    set_dont_fragment_socket(sock, ipv6, on)
}

/// Gemeinsamer Pfad für socket2::Socket und std::net::UdpSocket.
#[cfg(windows)]
fn set_dont_fragment_socket<S: std::os::windows::io::AsRawSocket>(
    sock: &S,
    ipv6: bool,
    on: bool,
) -> ChiakiResult<()> {
    set_dont_fragment_raw(sock.as_raw_socket() as usize, ipv6, on)
}

#[cfg(not(windows))]
fn set_dont_fragment_socket<S>(_sock: &S, _ipv6: bool, _on: bool) -> ChiakiResult<()> {
    // Nur der Windows-Pfad wird unterstützt (Zielplattform des Ports).
    tracing::warn!("sock: IP_DONTFRAG ist nur unter Windows implementiert");
    Err(ChiakiError::Unknown)
}

/// Der einzige unsafe-Aufruf des Moduls: `setsockopt(IPPROTO_IP, IP_DONTFRAGMENT)`
/// bzw. `setsockopt(IPPROTO_IPV6, IPV6_DONTFRAG)` — socket2 kennt diese Option
/// nicht. Minimal gehalten: gültiges Handle vom Aufrufer, `optval` ist ein
/// korrekt dimensionierter DWORD-Puffer, der die gesamte Aufrufdauer lebt.
#[cfg(windows)]
#[allow(unsafe_code)]
fn set_dont_fragment_raw(raw_socket: usize, ipv6: bool, on: bool) -> ChiakiResult<()> {
    use windows::Win32::Networking::WinSock::{
        setsockopt, IPPROTO_IP, IPPROTO_IPV6, IPV6_DONTFRAG, IP_DONTFRAGMENT, SOCKET,
    };

    let (level, optname) = if ipv6 {
        (IPPROTO_IPV6.0, IPV6_DONTFRAG)
    } else {
        (IPPROTO_IP.0, IP_DONTFRAGMENT)
    };
    let value: [u8; 4] = (if on { 1i32 } else { 0i32 }).to_ne_bytes();

    // SAFETY: `raw_socket` ist ein gültiges, offenes WinSock-Handle (vom
    // Aufrufer aus einem lebenden Socket-Objekt entnommen); `value` ist ein
    // gültiger 4-Byte-Puffer (DWORD), dessen Länge korrekt übergeben wird.
    let r = unsafe { setsockopt(SOCKET(raw_socket), level, optname, Some(&value)) };
    if r != 0 {
        let e = io::Error::last_os_error();
        return Err(io_err(
            if ipv6 { "IPV6_DONTFRAG" } else { "IP_DONTFRAGMENT" },
            e,
        ));
    }
    Ok(())
}

/// `sendto()` mit ChiakiError-Mapping (Interrupted wird wiederholt).
pub fn send_to(sock: &UdpSocket, buf: &[u8], addr: SocketAddr) -> ChiakiResult<usize> {
    loop {
        match sock.send_to(buf, addr) {
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(io_err("sendto()", e)),
        }
    }
}

/// `recvfrom()` mit ChiakiError-Mapping; WouldBlock/TimedOut → `Timeout`.
pub fn recv_from(sock: &UdpSocket, buf: &mut [u8]) -> ChiakiResult<(usize, SocketAddr)> {
    loop {
        match sock.recv_from(buf) {
            Ok(r) => return Ok(r),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::TimedOut =>
            {
                return Err(ChiakiError::Timeout)
            }
            Err(e) => return Err(io_err("recvfrom()", e)),
        }
    }
}

/// `sendto()` mit Timeout (setzt SO_SNDTIMEO für diesen Aufruf; bleibt auf dem
/// Socket bestehen). Entspricht dem Muster select(write)+sendto aus takion.c.
pub fn send_to_timeout(
    sock: &UdpSocket,
    buf: &[u8],
    addr: SocketAddr,
    timeout: Duration,
) -> ChiakiResult<usize> {
    set_write_timeout(sock, Some(clamp_timeout(timeout)))?;
    send_to(sock, buf, addr)
}

/// `recvfrom()` mit Timeout (setzt SO_RCVTIMEO für diesen Aufruf; bleibt auf dem
/// Socket bestehen). Entspricht dem Muster select(read)+recvfrom aus takion.c:
/// `Err(Timeout)` = nichts empfangen innerhalb `timeout`.
///
/// Für Poll-Schleifen mit StopPipe: blockierenden Socket verwenden und
/// `timeout` als Poll-Intervall wählen; zwischen den Aufrufen `stop_pipe.check()`.
pub fn recv_from_timeout(
    sock: &UdpSocket,
    buf: &mut [u8],
    timeout: Duration,
) -> ChiakiResult<(usize, SocketAddr)> {
    set_read_timeout(sock, Some(clamp_timeout(timeout)))?;
    recv_from(sock, buf)
}

/// std lehnt `Some(Duration::ZERO)` als Timeout ab; C-Timeouts von 0 ms
/// bedeuten "sofort" — hier als 1 ms (WinSock-Granularität) abgebildet.
fn clamp_timeout(timeout: Duration) -> Duration {
    timeout.max(Duration::from_millis(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};
    use std::time::Instant;

    fn any_v4() -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
    }

    #[test]
    fn create_udp_socket_with_options() {
        let opts = UdpSocketOptions {
            nonblocking: false,
            broadcast: true,
            dont_fragment: true,
            recv_buffer_size: Some(262_144),
            reuse_address: false,
            read_timeout: Some(Duration::from_millis(100)),
            write_timeout: None,
        };
        let sock = create_udp_socket(any_v4(), &opts).expect("UDP socket create");
        let local = sock.local_addr().unwrap();
        assert!(local.is_ipv4());
        assert_ne!(local.port(), 0);
        // SO_RCVBUF wurde übernommen (Windows setzt exakt, andere OS ggf. verdoppeln)
        assert!(recv_buffer_size(&sock).unwrap() >= 262_144);
        assert_eq!(sock.read_timeout().unwrap(), Some(Duration::from_millis(100)));
        assert!(SockRef::from(&sock).broadcast().unwrap());
        close(sock);
    }

    #[test]
    fn send_recv_roundtrip_with_timeout() {
        let a = create_udp_socket(any_v4(), &UdpSocketOptions::default()).unwrap();
        let b = create_udp_socket(any_v4(), &UdpSocketOptions::default()).unwrap();
        let b_addr = b.local_addr().unwrap();

        let payload = b"chiaki takion ping";
        let sent = send_to_timeout(&a, payload, b_addr, Duration::from_secs(1)).unwrap();
        assert_eq!(sent, payload.len());

        let mut buf = [0u8; 64];
        let (n, from) = recv_from_timeout(&b, &mut buf, Duration::from_secs(2)).unwrap();
        assert_eq!(&buf[..n], payload);
        assert_eq!(from, a.local_addr().unwrap());
    }

    #[test]
    fn recv_from_timeout_reports_timeout() {
        let sock = create_udp_socket(any_v4(), &UdpSocketOptions::default()).unwrap();
        let mut buf = [0u8; 16];
        let t = Instant::now();
        let r = recv_from_timeout(&sock, &mut buf, Duration::from_millis(50));
        assert_eq!(r.unwrap_err(), ChiakiError::Timeout);
        assert!(t.elapsed() >= Duration::from_millis(30));
    }

    #[test]
    fn nonblocking_recv_maps_to_timeout() {
        let sock = create_udp_socket(any_v4(), &UdpSocketOptions::default()).unwrap();
        set_nonblock(&sock, true).unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(recv_from(&sock, &mut buf).unwrap_err(), ChiakiError::Timeout);
        set_nonblock(&sock, false).unwrap();
    }

    #[test]
    fn post_creation_setters() {
        let sock = create_udp_socket(any_v4(), &UdpSocketOptions::default()).unwrap();
        set_broadcast(&sock, true).unwrap();
        assert!(SockRef::from(&sock).broadcast().unwrap());
        set_recv_buffer_size(&sock, 131_072).unwrap();
        assert!(recv_buffer_size(&sock).unwrap() >= 131_072);
        set_read_timeout(&sock, Some(Duration::from_millis(5))).unwrap();
        set_write_timeout(&sock, None).unwrap();
        #[cfg(windows)]
        {
            set_dont_fragment(&sock, true).unwrap();
            set_dont_fragment(&sock, false).unwrap();
        }
    }

    #[cfg(windows)]
    #[test]
    fn dont_fragment_ipv6_if_available() {
        let addr: SocketAddr = "[::1]:0".parse().unwrap();
        let opts = UdpSocketOptions {
            dont_fragment: true,
            ..Default::default()
        };
        // Ohne IPv6-Loopback schlägt bereits das Bind fehl — dann kein Fehler im Test.
        if let Ok(sock) = create_udp_socket(addr, &opts) {
            set_dont_fragment(&sock, true).unwrap();
        }
    }

    #[test]
    fn io_error_mapping() {
        assert_eq!(
            map_io_error(&io::Error::from(io::ErrorKind::ConnectionRefused)),
            ChiakiError::ConnectionRefused
        );
        assert_eq!(
            map_io_error(&io::Error::from(io::ErrorKind::TimedOut)),
            ChiakiError::Timeout
        );
        assert_eq!(
            map_io_error(&io::Error::from(io::ErrorKind::WouldBlock)),
            ChiakiError::Timeout
        );
        assert_eq!(
            map_io_error(&io::Error::from(io::ErrorKind::ConnectionReset)),
            ChiakiError::Disconnected
        );
        assert_eq!(
            map_io_error(&io::Error::from(io::ErrorKind::Other)),
            ChiakiError::Network
        );
        #[cfg(windows)]
        {
            assert_eq!(map_io_error(&io::Error::from_raw_os_error(10065)), ChiakiError::HostUnreach);
            assert_eq!(map_io_error(&io::Error::from_raw_os_error(10064)), ChiakiError::HostDown);
            assert_eq!(map_io_error(&io::Error::from_raw_os_error(10061)), ChiakiError::ConnectionRefused);
        }
    }
}
