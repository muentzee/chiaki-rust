// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/remote/stun.h (chiaki-ng).
//
// STUN (RFC 5389/3489-kompatibel, klassisches Binding-Request/Response) wird
// vom Holepunching genutzt, um
//   1. die eigene öffentliche Adresse/Port zu erkennen
//      (`stun_get_external_address`) und
//   2. das Port-Allocation-Verhalten des NATs zu messen
//      (`stun_port_allocation_test` — 4 Requests, Differenzbildung der
//      gemappten Ports → Allocation-Increment / Zufälligkeit).
//
// Im C-Code ist stun.h header-only (statische Funktionen), eingebunden nur von
// holepunch.c. Der Rust-Port behält die Funktionen und Konstanten 1:1 bei.
//
// Abweichungen:
// - Der globale `STUN_SERVERS[]`-Shuffle läuft im C auf der globalen Tabelle;
//   hier wird eine lokale Kopie gemischt (thread-safe, gleiches Verhalten:
//   Index 0 = Moonlight-Server bleibt bevorzugt).
// - getaddrinfo → `ToSocketAddrs`, inet_ntop → Formatierung über
//   Ipv4Addr/Ipv6Addr, SO_RCVTIMEO → `sock::set_read_timeout`.
// - Der Socket wird als `&mut Option<UdpSocket>` übergeben: schlägt das Senden
//   fehl, schließt der C-Code den Socket und setzt ihn auf INVALID — hier wird
//   die Option taken.

use std::net::{IpAddr, ToSocketAddrs, UdpSocket};
use std::time::Duration;

use chiaki_core::error::{ChiakiError, ChiakiResult};
use chiaki_core::random;
use chiaki_core::sock;

pub const STUN_REPLY_TIMEOUT_SEC: u64 = 5;

pub const STUN_HEADER_SIZE: usize = 20;
pub const STUN_MSG_TYPE_BINDING_REQUEST: u16 = 0x0001;
pub const STUN_MSG_TYPE_BINDING_RESPONSE: u16 = 0x0101;
pub const STUN_MAGIC_COOKIE: u32 = 0x2112A442;
pub const STUN_TRANSACTION_ID_LENGTH: usize = 12;
pub const STUN_ATTRIB_MAPPED_ADDRESS: u16 = 0x0001;
pub const STUN_ATTRIB_XOR_MAPPED_ADDRESS: u16 = 0x0020;
pub const STUN_MAPPED_ADDR_FAMILY_IPV4: u8 = 0x01;
pub const STUN_MAPPED_ADDR_FAMILY_IPV6: u8 = 0x02;

/// Port von `StunServer`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StunServer {
    pub host: String,
    pub port: u16,
}

impl StunServer {
    pub fn new(host: &str, port: u16) -> StunServer {
        StunServer {
            host: host.to_owned(),
            port,
        }
    }
}

/// Port der statischen `STUN_SERVERS[]`-Tabelle.
///
/// Bevorzugt wird der STUN-Server des Moonlight-Projekts; schlägt der fehl,
/// werden die übrigen in zufälliger Reihenfolge versucht.
pub fn default_stun_servers() -> &'static [StunServer] {
    use std::sync::OnceLock;
    static SERVERS: OnceLock<Vec<StunServer>> = OnceLock::new();
    SERVERS.get_or_init(|| {
        [
            ("stun.moonlight-stream.org", 3478),
            ("stun.l.google.com", 19302),
            ("stun.l.google.com", 19305),
            ("stun1.l.google.com", 19302),
            ("stun1.l.google.com", 19305),
            ("stun2.l.google.com", 19302),
            ("stun2.l.google.com", 19305),
            ("stun3.l.google.com", 19302),
            ("stun3.l.google.com", 19305),
            ("stun4.l.google.com", 19302),
            ("stun4.l.google.com", 19305),
        ]
        .iter()
        .map(|(h, p)| StunServer::new(h, *p))
        .collect()
    })
}

/// Port der Binding-Request-Erzeugung aus `stun_get_external_address_from_server`:
/// 20-Byte-Header, Message-Type 0x0001, Länge 0, Magic-Cookie, 12 zufällige
/// Transaction-ID-Bytes.
pub fn build_binding_request() -> [u8; STUN_HEADER_SIZE] {
    let mut req = [0u8; STUN_HEADER_SIZE];
    req[0..2].copy_from_slice(&STUN_MSG_TYPE_BINDING_REQUEST.to_be_bytes());
    // Length = 0
    req[4..8].copy_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
    random::random_bytes(&mut req[8..8 + STUN_TRANSACTION_ID_LENGTH]);
    req
}

/// Port der Binding-Response-Auswertung aus `stun_get_external_address_from_server`
/// (Attribut-Loop: XOR-MAPPED-ADDRESS / MAPPED-ADDRESS, IPv4 + IPv6).
///
/// @param resp Empfangene Antwort (max. 256 Bytes im C).
/// @param binding_req Der zugehörige Request (für Magic-Cookie/Transaction-ID-
///                    Vergleich sowie die IPv6-XOR-Basis).
/// @return (Adresse als String, Port)
pub fn parse_binding_response(resp: &[u8], binding_req: &[u8]) -> ChiakiResult<(String, u16)> {
    if resp.len() < STUN_HEADER_SIZE {
        tracing::error!("remote/stun.h: Received message is too small");
        return Err(ChiakiError::Network);
    }

    if resp[0..2] != STUN_MSG_TYPE_BINDING_RESPONSE.to_be_bytes() {
        tracing::error!("remote/stun.h: Received STUN response with invalid message type");
        return Err(ChiakiError::InvalidData);
    }

    // Verify length stored in binding_resp[2] is correct
    let expected_size = u16::from_be_bytes([resp[2], resp[3]]) as usize + STUN_HEADER_SIZE;
    if resp.len() != expected_size {
        tracing::error!(
            "remote/stun.h: Received STUN response with invalid length: {} received, {} expected",
            resp.len(),
            expected_size
        );
        return Err(ChiakiError::InvalidData);
    }

    if resp[4..8] != STUN_MAGIC_COOKIE.to_be_bytes() {
        tracing::error!("remote/stun.h: Received STUN response with invalid magic cookie");
        return Err(ChiakiError::InvalidData);
    }

    if resp[8..8 + STUN_TRANSACTION_ID_LENGTH]
        != binding_req[8..8 + STUN_TRANSACTION_ID_LENGTH]
    {
        tracing::error!("remote/stun.h: Received STUN response with invalid transaction ID");
        return Err(ChiakiError::InvalidData);
    }

    let received = resp.len();
    let mut response_pos = STUN_HEADER_SIZE;
    // Check we can read 4 bytes of attribute data
    while response_pos < received - 3 {
        let attr_type = u16::from_be_bytes([resp[response_pos], resp[response_pos + 1]]);
        let attr_length =
            u16::from_be_bytes([resp[response_pos + 2], resp[response_pos + 3]]) as usize;
        // check that the whole advertised message has been received
        if response_pos > received - (2 + 2 + attr_length) {
            tracing::error!("remote/stun.h: Received STUN response with invalid data");
            return Err(ChiakiError::InvalidData);
        }
        if attr_type != STUN_ATTRIB_MAPPED_ADDRESS && attr_type != STUN_ATTRIB_XOR_MAPPED_ADDRESS {
            response_pos += 2 + 2 + attr_length;
            continue;
        }

        let xored = attr_type == STUN_ATTRIB_XOR_MAPPED_ADDRESS;
        let family = resp[response_pos + 5];
        if family == STUN_MAPPED_ADDR_FAMILY_IPV4 {
            if attr_length != 8 {
                tracing::error!(
                    "remote/stun.h: Received STUN_MAPPED_ADDR_FAMILY_IPV4 with invalid data! Expected atribute length: 8. Received attribute length {}",
                    attr_length
                );
                return Err(ChiakiError::InvalidData);
            }
            let port_bytes = [resp[response_pos + 6], resp[response_pos + 7]];
            let addr_bytes = [
                resp[response_pos + 8],
                resp[response_pos + 9],
                resp[response_pos + 10],
                resp[response_pos + 11],
            ];
            let port;
            let addr;
            if xored {
                // XOR mit den oberen 16 Bits des Magic-Cookie (RFC 5389):
                // das C-Code-Gefrickel mit htons/htonl ergibt exakt port ^ 0x2112
                // bzw. addr_bytes ^ cookie_bytes (network order).
                port = u16::from_be_bytes(port_bytes) ^ 0x2112;
                let mut a = addr_bytes;
                for (b, m) in a.iter_mut().zip(STUN_MAGIC_COOKIE.to_be_bytes()) {
                    *b ^= m;
                }
                addr = a;
            } else {
                port = u16::from_be_bytes(port_bytes);
                addr = addr_bytes;
            }
            return Ok((std::net::Ipv4Addr::from(addr).to_string(), port));
        } else if family == STUN_MAPPED_ADDR_FAMILY_IPV6 {
            if attr_length != 20 {
                tracing::error!(
                    "remote/stun.h: Received STUN_MAPPED_ADDR_FAMILY_IPV6 with invalid data! Expected atribute length: 20. Received attribute length {}",
                    attr_length as i16
                );
                return Err(ChiakiError::InvalidData);
            }
            let port_bytes = [resp[response_pos + 6], resp[response_pos + 7]];
            let mut addr_bytes = [0u8; 16];
            addr_bytes.copy_from_slice(&resp[response_pos + 8..response_pos + 8 + 16]);
            let port;
            if xored {
                port = u16::from_be_bytes(port_bytes) ^ 0x2112;
                // XOR address with concat(STUN_MAGIC_COOKIE, transaction_id)
                // NOTE: RFC5389 says we need to convert from network to host
                // endianness here, but this seems to be misleading, see
                // https://stackoverflow.com/a/40325004
                for i in 0..16 {
                    addr_bytes[i] ^= binding_req[4 + i];
                }
            } else {
                port = u16::from_be_bytes(port_bytes);
            }
            return Ok((std::net::Ipv6Addr::from(addr_bytes).to_string(), port));
        } else {
            tracing::error!(
                "remote/stun.h: Received STUN response with invalid address family: {}",
                family
            );
            return Err(ChiakiError::InvalidData);
        }
    }

    Err(ChiakiError::InvalidData)
}

/// Auflösen eines STUN-Servers (getaddrinfo-Ersatz). Liefert die erste Adresse
/// der gewünschten Familie.
fn resolve_server(server: &StunServer, ipv4: bool) -> ChiakiResult<IpAddr> {
    let addrs = (server.host.as_str(), server.port)
        .to_socket_addrs()
        .map_err(|e| {
            tracing::error!(
                "remote/stun.h: Failed to resolve STUN server '{}', error was {}",
                server.host,
                e
            );
            ChiakiError::ParseAddr
        })?;
    let wanted = |a: &std::net::SocketAddr| if ipv4 { a.is_ipv4() } else { a.is_ipv6() };
    for a in addrs {
        if wanted(&a) {
            return Ok(a.ip());
        }
    }
    tracing::error!(
        "remote/stun.h: Failed to resolve STUN server '{}' to an address of the requested family",
        server.host
    );
    Err(ChiakiError::ParseAddr)
}

/// Port von `stun_get_external_address_from_server()`.
///
/// Sendet ein Binding-Request an `server`, wartet bis zu STUN_REPLY_TIMEOUT_SEC
/// auf die Antwort und extrahiert (XOR-)MAPPED-ADDRESS. Schlägt das Senden oder
/// der Sockopt fehl, wird der Socket geschlossen (`*sock = None`) — wie im C.
///
/// @return true bei Erfolg (address/port gesetzt), sonst false.
pub fn stun_get_external_address_from_server(
    server: &StunServer,
    address: &mut String,
    port: &mut u16,
    sock_: &mut Option<UdpSocket>,
    ipv4: bool,
) -> bool {
    let Some(sock_ref) = sock_.as_ref() else {
        return false;
    };

    let ip = match resolve_server(server, ipv4) {
        Ok(ip) => ip,
        Err(_) => return false,
    };
    let server_addr = std::net::SocketAddr::new(ip, server.port);

    let binding_req = build_binding_request();
    match sock::send_to(sock_ref, &binding_req, server_addr) {
        Ok(n) if n == binding_req.len() => {}
        Err(e) => {
            tracing::error!("remote/stun.h: Failed to send STUN request, error was {}", e);
            // C: Socket schließen und auf INVALID setzen
            sock_.take();
            return false;
        }
        Ok(_) => unreachable!("partial UDP sendto"),
    }

    if let Err(e) = sock::set_read_timeout(sock_ref, Some(Duration::from_secs(STUN_REPLY_TIMEOUT_SEC))) {
        tracing::error!("remote/stun.h: Failed to set socket timeout, error was {}", e);
        sock_.take();
        return false;
    }

    let mut binding_resp = [0u8; 256];
    let received = match sock::recv_from(sock_ref, &mut binding_resp) {
        Ok((n, _)) => n,
        Err(e) => {
            tracing::error!("remote/stun.h: Failed to receive STUN response, error was {}", e);
            return false;
        }
    };

    match parse_binding_response(&binding_resp[..received], &binding_req) {
        Ok((addr, p)) => {
            *address = addr;
            *port = p;
            true
        }
        Err(_) => false,
    }
}

/// Fisher-Yates-Shuffle der Server-Liste ab Index 1 (Index 0 = Moonlight-Server
/// bleibt fix) — Port des C-Shuffles `for(i = n-1; i > 1; i--) { j = 1 + rand % (i-1); }`.
fn shuffle_servers(servers: &mut [StunServer]) {
    let n = servers.len();
    if n < 3 {
        return;
    }
    for i in (2..n).rev() {
        let j = 1 + (random::random_32() as usize % (i - 1));
        servers.swap(i, j);
    }
}

/// Port von `stun_get_external_address()`.
///
/// Versucht zuerst die übergebenen Server (im C: vom Nutzer bevorzugt bzw. der
/// von der PSN-Liste bekannte online-Server), danach die Default-Liste in
/// zufälliger Reihenfolge (Moonlight-Server zuerst). Für IPv6 wird nur ein
/// einziger Server versucht.
pub fn stun_get_external_address(
    address: &mut String,
    port: &mut u16,
    passed_servers: &[StunServer],
    sock_: &mut Option<UdpSocket>,
    ipv4: bool,
) -> bool {
    // Try servers preferred by user (i.e., known to be online)
    let mut ipv6_tried = false;
    for server in passed_servers {
        if sock_.is_none() {
            return false;
        }
        if stun_get_external_address_from_server(server, address, port, sock_, ipv4) {
            return true;
        }
        tracing::warn!(
            "Failed to get external address from {}:{}, retrying with another STUN server...",
            server.host,
            server.port
        );
        // Only try 1 IPV6 server
        if !ipv4 {
            ipv6_tried = true;
            break;
        }
    }
    if ipv4 || !ipv6_tried {
        // Shuffle order of servers other than moonlight server
        let mut servers = default_stun_servers().to_vec();
        shuffle_servers(&mut servers);
        // Try other servers
        for server in &servers {
            if sock_.is_none() {
                return false;
            }
            if stun_get_external_address_from_server(server, address, port, sock_, ipv4) {
                return true;
            }
            tracing::warn!(
                "Failed to get external address from {}:{}, retrying with another STUN server...",
                server.host,
                server.port
            );
            // Only try 1 IPV6 server
            if !ipv4 {
                break;
            }
        }
    }
    tracing::error!("Failed to get external address from any STUN server.");
    false
}

/// Port von `stun_port_allocation_test()`.
///
/// Sendet 4 Binding-Requests (bevorzugt an die übergebenen Server, sonst an die
/// Default-Liste) und berechnet aus den gemappten Ports das
/// Allocation-Increment des NATs sowie, ob die Allocation zufällig wirkt.
/// Der zurückgegebene Port ist die Vorhersage für den nächsten Allocation-Slot.
#[allow(clippy::too_many_arguments)]
pub fn stun_port_allocation_test(
    address: &mut String,
    port: &mut u16,
    allocation_increment: &mut i32,
    random_allocation: &mut bool,
    passed_servers: &[StunServer],
    sock_: &mut Option<UdpSocket>,
) -> bool {
    // skip testing if outgoing port changes with same internal ip and port if send to same ip and different port bc that doesn't apply in our case (we will be using a different address anyway)
    let mut ports = [0u16; 4];
    let mut addrs: [String; 4] = [String::new(), String::new(), String::new(), String::new()];

    // Pro Server genau ein freies Slot befüllen (C: if/else-if-Kette), wie im C
    fn try_one_server(
        server: &StunServer,
        ports: &mut [u16; 4],
        addrs: &mut [String; 4],
        sock_: &mut Option<UdpSocket>,
    ) {
        let Some(slot) = (0..4).find(|&s| ports[s] == 0) else {
            return;
        };
        if sock_.is_none() {
            return;
        }
        let mut p = 0u16;
        let mut a = String::new();
        if stun_get_external_address_from_server(server, &mut a, &mut p, sock_, true) {
            tracing::trace!("Got response from STUN server {}:{}", server.host, server.port);
            ports[slot] = p;
            addrs[slot] = a;
        } else {
            tracing::warn!(
                "Failed to get external address from {}:{}, retrying with another STUN server...",
                server.host,
                server.port
            );
        }
    }

    // Try servers preferred by user (i.e., known to be online)
    for server in passed_servers {
        try_one_server(server, &mut ports, &mut addrs, sock_);
        if ports[3] != 0 {
            break;
        }
    }
    if ports[3] == 0 {
        let mut servers = default_stun_servers().to_vec();
        // Shuffle order of servers other than moonlight server
        shuffle_servers(&mut servers);
        // Try other servers
        for server in &servers {
            try_one_server(server, &mut ports, &mut addrs, sock_);
            if ports[3] != 0 {
                break;
            }
        }
    }

    let [port1, port2, port3, port4] = ports;
    let [addr1, addr2, addr3, addr4] = &addrs;

    // No servers returned
    if port1 == 0 {
        tracing::error!("Failed to get external address from any STUN server.");
        return false;
    }
    // 1 server returned
    else if port2 == 0 {
        tracing::warn!("Only 1 STUN server responded for packet allocation calculation.");
        tracing::warn!("Couldn't determine packet allocation because not enough STUN servers responded.");
        *address = addr1.clone();
        *port = port1;
        *allocation_increment = 0;
    }
    // 2 servers returned
    else if port3 == 0 {
        *address = addr2.clone();
        tracing::warn!("Only 2 STUN servers responded for packet allocation calculation.");
        // address changed between requests use most recent one
        if addr1 != addr2 {
            tracing::warn!("Got different addresses between 2 responses, using 2nd one...");
            tracing::warn!("Couldn't determine packet allocation because not enough STUN servers responded with the same address defaulting to 0.");
            *allocation_increment = 0;
        } else {
            *allocation_increment = i32::from(port2) - i32::from(port1);
        }
        *port = (i32::from(port2) + 2 * *allocation_increment) as u16;
    }
    // 3 servers returned
    else if port4 == 0 {
        tracing::warn!("Only 3 STUN servers responded for packet allocation calculation.");
        *address = addr3.clone();
        if addr1 != addr2 {
            if addr1 == addr3 {
                tracing::warn!("Calculating packet allocation based on 2 responses with the same address");
                *allocation_increment = (i32::from(port3) - i32::from(port1)) / 2;
            } else if addr2 == addr3 {
                tracing::warn!("Calculating packet allocation based on 2 responses with the same address");
                *allocation_increment = i32::from(port3) - i32::from(port2);
            } else {
                tracing::warn!("Got 3 different addresses between 3 responses, using 3rd one...");
                tracing::warn!("Couldn't determine packet allocation because not enough STUN servers responded with the same address.");
                *allocation_increment = 0;
            }
        } else if addr1 != addr3 {
            tracing::warn!("Calculating packet allocation based on 2 responses with the same address");
            *allocation_increment = i32::from(port2) - i32::from(port1);
        } else {
            tracing::warn!("Calculating packet allocation based on 3 responses with the same address");
            *allocation_increment = i32::from(port2) - i32::from(port1);
            let allocation_increment1 = i32::from(port3) - i32::from(port2);
            if allocation_increment1 != *allocation_increment {
                *random_allocation = true;
                if *allocation_increment == 0 {
                    *allocation_increment = allocation_increment1;
                }
                tracing::warn!(
                    "Got different allocation increment calculations from different ports.\nIncrement0: {}, Increment1: {}",
                    *allocation_increment,
                    allocation_increment1
                );
            }
        }
        *port = (i32::from(port3) + *allocation_increment) as u16;
    }
    // all 4 servers returned
    else {
        *address = addr4.clone();
        *port = port4;
        if addr1 != addr2 {
            if addr1 == addr3 || addr1 == addr4 {
                if addr1 == addr4 {
                    if addr1 == addr3 {
                        tracing::warn!("Calculating packet allocation based on 3 responses with the same address");
                        *allocation_increment = i32::from(port4) - i32::from(port3);
                        let allocation_increment1 = (i32::from(port3) - i32::from(port1)) / 2;
                        if allocation_increment1 != *allocation_increment {
                            *random_allocation = true;
                            if *allocation_increment == 0 {
                                *allocation_increment = allocation_increment1;
                            }
                            tracing::warn!(
                                "Got different allocation increment calculations from different ports.\nIncrement0: {}, Increment1: {}",
                                *allocation_increment,
                                allocation_increment1
                            );
                        }
                    } else {
                        tracing::warn!("Calculating packet allocation based on 2 responses with the same address");
                        *allocation_increment = (i32::from(port4) - i32::from(port1)) / 4;
                    }
                } else {
                    tracing::warn!("Calculating packet allocation based on 2 responses with the same address");
                    *allocation_increment = (i32::from(port3) - i32::from(port1)) / 2;
                }
            } else if addr2 == addr3 || addr2 == addr4 {
                if addr2 == addr4 {
                    if addr2 == addr3 {
                        tracing::warn!("Calculating packet allocation based on 3 responses with the same address");
                        *allocation_increment = i32::from(port4) - i32::from(port3);
                        let allocation_increment1 = i32::from(port3) - i32::from(port2);
                        if allocation_increment1 != *allocation_increment {
                            *random_allocation = true;
                            if *allocation_increment == 0 {
                                *allocation_increment = allocation_increment1;
                            }
                            tracing::warn!(
                                "Got different allocation increment calculations from different ports.\nIncrement0: {}, Increment1: {}",
                                *allocation_increment,
                                allocation_increment1
                            );
                        }
                    } else {
                        tracing::warn!("Calculating packet allocation based on 2 responses with the same address");
                        *allocation_increment = (i32::from(port4) - i32::from(port1)) / 4;
                    }
                } else {
                    tracing::warn!("Calculating packet allocation based on 2 responses with the same address");
                    *allocation_increment = (i32::from(port3) - i32::from(port1)) / 2;
                }
            } else if addr3 == addr4 {
                tracing::warn!("Calculating packet allocation based on 2 responses with the same address");
                *allocation_increment = i32::from(port3) - i32::from(port4);
            } else {
                tracing::warn!("Got 4 different addresses between 4 responses, using 4th one...");
                tracing::warn!("Couldn't determine packet allocation because not enough STUN servers responded with the same address.");
                *allocation_increment = 0;
            }
        } else if addr2 != addr3 {
            *allocation_increment = i32::from(port2) - i32::from(port1);
            if addr2 == addr4 {
                tracing::warn!("Calculating packet allocation based on 3 responses with the same address");
                let allocation_increment1 = (i32::from(port4) - i32::from(port2)) / 2;
                if allocation_increment1 != *allocation_increment {
                    *random_allocation = true;
                    if *allocation_increment == 0 {
                        *allocation_increment = allocation_increment1;
                    }
                    tracing::warn!(
                        "Got different allocation increment calculations from different ports.\nIncrement0: {}, Increment1: {}",
                        *allocation_increment,
                        allocation_increment1
                    );
                }
            } else {
                tracing::warn!("Calculating packet allocation based on 2 responses with the same address");
            }
        } else if addr3 != addr4 {
            tracing::warn!("Calculating packet allocation based on 3 responses with the same address");
            *allocation_increment = i32::from(port2) - i32::from(port1);
            let allocation_increment1 = i32::from(port3) - i32::from(port2);
            if allocation_increment1 != *allocation_increment {
                *random_allocation = true;
                if *allocation_increment == 0 {
                    *allocation_increment = allocation_increment1;
                }
                tracing::warn!(
                    "Got different allocation increment calculations from different ports.\nIncrement0: {}, Increment1: {}",
                    *allocation_increment,
                    allocation_increment1
                );
            }
        } else {
            tracing::info!("Calculating packet allocation based on 4 responses with the same address");
            // C rechnet hier mit uint16_t-Wraparound
            let increment0: u16 = port2.wrapping_sub(port1);
            let increment1: u16 = port3.wrapping_sub(port2);
            let increment2: u16 = port4.wrapping_sub(port3);
            if increment0 == increment1 && increment1 == increment2 {
                *allocation_increment = i32::from(increment0);
                tracing::trace!(
                    "Got 3 idential allocation increment calculations out of 3,\nIncrement {}",
                    increment0
                );
            } else if increment0 == increment1 || increment0 == increment2 {
                tracing::trace!(
                    "Got 2 idential allocation increment calculations out of 3 from different ports.\nIncrement 0: {}, Increment 1: {}, Increment 2: {}",
                    increment0,
                    increment1,
                    increment2
                );
                *allocation_increment = i32::from(increment0);
            } else if increment1 == increment2 {
                tracing::trace!(
                    "Got 2 idential allocation increment calculations out of 3 from different ports.\nIncrement 0: {}, Increment 1: {}, Increment 2: {}",
                    increment0,
                    increment1,
                    increment2
                );
                *allocation_increment = i32::from(increment1);
            } else {
                *random_allocation = true;
                tracing::warn!(
                    "Got different allocation increment calculations from different ports.\nIncrement 0: {}, Increment 1: {}, Increment 2: {}",
                    increment0,
                    increment1,
                    increment2
                );
                *allocation_increment = if increment0 != 0 {
                    i32::from(increment0)
                } else {
                    i32::from(increment1)
                };
            }
        }
    }

    true
}

/// Für Tests: Binding-Response mit einer (XOR-)MAPPED-ADDRESS-IPv4-Attribute
/// zusammenbauen (vom C-Code nicht angebotene Richtung, hier nur testintern).
#[cfg(test)]
pub(crate) fn build_test_binding_response_ipv4(
    txid: &[u8; 12],
    addr: [u8; 4],
    port: u16,
    xored: bool,
) -> Vec<u8> {
    let mut resp = Vec::with_capacity(32);
    resp.extend_from_slice(&STUN_MSG_TYPE_BINDING_RESPONSE.to_be_bytes());
    resp.extend_from_slice(&12u16.to_be_bytes()); // ein Attribut: 4 + 8
    resp.extend_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
    resp.extend_from_slice(txid);
    resp.extend_from_slice(
        &(if xored {
            STUN_ATTRIB_XOR_MAPPED_ADDRESS
        } else {
            STUN_ATTRIB_MAPPED_ADDRESS
        })
        .to_be_bytes(),
    );
    resp.extend_from_slice(&8u16.to_be_bytes());
    resp.push(0); // reserved
    resp.push(STUN_MAPPED_ADDR_FAMILY_IPV4);
    let (p, a): (u16, [u8; 4]) = if xored {
        (
            port ^ 0x2112,
            [
                addr[0] ^ 0x21,
                addr[1] ^ 0x12,
                addr[2] ^ 0xA4,
                addr[3] ^ 0x42,
            ],
        )
    } else {
        (port, addr)
    };
    resp.extend_from_slice(&p.to_be_bytes());
    resp.extend_from_slice(&a);
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_txid() -> [u8; 12] {
        [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c]
    }

    fn test_request() -> [u8; 20] {
        let mut req = [0u8; 20];
        req[0..2].copy_from_slice(&STUN_MSG_TYPE_BINDING_REQUEST.to_be_bytes());
        req[4..8].copy_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
        req[8..20].copy_from_slice(&test_txid());
        req
    }

    #[test]
    fn binding_request_golden() {
        let req = build_binding_request();
        assert_eq!(req.len(), STUN_HEADER_SIZE);
        assert_eq!(&req[0..2], &0x0001u16.to_be_bytes(), "Message Type");
        assert_eq!(&req[2..4], &[0, 0], "Message Length");
        assert_eq!(&req[4..8], &STUN_MAGIC_COOKIE.to_be_bytes(), "Magic Cookie");
        // Transaction-ID zufällig, aber nicht alle Null
        assert!(req[8..20].iter().any(|&b| b != 0));
    }

    #[test]
    fn parse_xor_mapped_ipv4_golden() {
        // 203.0.113.5:54321 (TEST-NET-3), XOR-verschlüsselt
        let addr = [203, 0, 113, 5];
        let port = 54321u16;
        let resp = build_test_binding_response_ipv4(&test_txid(), addr, port, true);
        let (got_addr, got_port) =
            parse_binding_response(&resp, &test_request()).expect("parse");
        assert_eq!(got_addr, "203.0.113.5");
        assert_eq!(got_port, 54321);
    }

    #[test]
    fn parse_mapped_ipv4_golden() {
        let addr = [192, 0, 2, 33];
        let port = 1234u16;
        let resp = build_test_binding_response_ipv4(&test_txid(), addr, port, false);
        let (got_addr, got_port) =
            parse_binding_response(&resp, &test_request()).expect("parse");
        assert_eq!(got_addr, "192.0.2.33");
        assert_eq!(got_port, 1234);
    }

    #[test]
    fn parse_skips_unknown_attributes() {
        // FINGERPRINT-artiges unbekanntes Attribut vor MAPPED-ADDRESS
        let mut resp = Vec::new();
        resp.extend_from_slice(&STUN_MSG_TYPE_BINDING_RESPONSE.to_be_bytes());
        resp.extend_from_slice(&(4u16 + 8 + 8).to_be_bytes());
        resp.extend_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
        resp.extend_from_slice(&test_txid());
        // unbekanntes Attribut type=0x8028, len=4
        resp.extend_from_slice(&0x8028u16.to_be_bytes());
        resp.extend_from_slice(&4u16.to_be_bytes());
        resp.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        // MAPPED-ADDRESS
        resp.extend_from_slice(&STUN_ATTRIB_MAPPED_ADDRESS.to_be_bytes());
        resp.extend_from_slice(&8u16.to_be_bytes());
        resp.push(0);
        resp.push(STUN_MAPPED_ADDR_FAMILY_IPV4);
        resp.extend_from_slice(&7777u16.to_be_bytes());
        resp.extend_from_slice(&[10, 1, 2, 3]);
        let (got_addr, got_port) = parse_binding_response(&resp, &test_request()).expect("parse");
        assert_eq!(got_addr, "10.1.2.3");
        assert_eq!(got_port, 7777);
    }

    #[test]
    fn parse_rejects_bad_responses() {
        let good = build_test_binding_response_ipv4(&test_txid(), [1, 2, 3, 4], 100, true);

        // zu klein
        assert_eq!(parse_binding_response(&good[..8], &test_request()), Err(ChiakiError::Network));

        // falscher Message-Type
        let mut bad = good.clone();
        bad[1] = 0x02;
        assert_eq!(parse_binding_response(&bad, &test_request()), Err(ChiakiError::InvalidData));

        // falsche Länge
        let mut bad = good.clone();
        bad[3] = 0x42;
        assert_eq!(parse_binding_response(&bad, &test_request()), Err(ChiakiError::InvalidData));

        // falscher Cookie
        let mut bad = good.clone();
        bad[4] ^= 0xff;
        assert_eq!(parse_binding_response(&bad, &test_request()), Err(ChiakiError::InvalidData));

        // falsche Transaction-ID
        let mut other_req = test_request();
        other_req[8] ^= 0xff;
        assert_eq!(parse_binding_response(&good, &other_req), Err(ChiakiError::InvalidData));
    }

    #[test]
    fn parse_xor_mapped_ipv6_golden() {
        let mut resp = Vec::new();
        resp.extend_from_slice(&STUN_MSG_TYPE_BINDING_RESPONSE.to_be_bytes());
        resp.extend_from_slice(&(4u16 + 20).to_be_bytes());
        resp.extend_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
        resp.extend_from_slice(&test_txid());
        resp.extend_from_slice(&STUN_ATTRIB_XOR_MAPPED_ADDRESS.to_be_bytes());
        resp.extend_from_slice(&20u16.to_be_bytes());
        resp.push(0);
        resp.push(STUN_MAPPED_ADDR_FAMILY_IPV6);
        let addr: [u8; 16] = [
            0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01,
        ];
        let port = 9295u16;
        let mut xport = port.to_be_bytes();
        xport[0] ^= 0x21;
        xport[1] ^= 0x12;
        resp.extend_from_slice(&xport);
        for i in 0..16 {
            resp.push(addr[i] ^ test_request()[4 + i]);
        }
        let (got_addr, got_port) = parse_binding_response(&resp, &test_request()).expect("parse");
        assert_eq!(got_addr, "2001:db8::1");
        assert_eq!(got_port, 9295);
    }

    #[test]
    fn default_servers_match_c_table() {
        let servers = default_stun_servers();
        assert_eq!(servers.len(), 11);
        assert_eq!(servers[0], StunServer::new("stun.moonlight-stream.org", 3478));
        assert_eq!(servers[1], StunServer::new("stun.l.google.com", 19302));
        assert_eq!(servers[10], StunServer::new("stun4.l.google.com", 19305));
    }

    #[test]
    fn shuffle_keeps_moonlight_first() {
        let mut servers = default_stun_servers().to_vec();
        shuffle_servers(&mut servers);
        assert_eq!(servers[0], StunServer::new("stun.moonlight-stream.org", 3478));
        // Gesamtheit bleibt erhalten (Host+Port-Paare; Hostnamen allein
        // kommen mehrfach vor — stun{l}.l.google.com mit 2 Ports)
        let mut pairs: Vec<(String, u16)> =
            servers.iter().map(|s| (s.host.clone(), s.port)).collect();
        pairs.sort();
        let mut expected: Vec<(String, u16)> = default_stun_servers()
            .iter()
            .map(|s| (s.host.clone(), s.port))
            .collect();
        expected.sort();
        assert_eq!(pairs, expected);
    }
}
