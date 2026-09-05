// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/discoveryservice.c + lib/include/chiaki/discoveryservice.h
// (chiaki-ng).
//
// Der Service pingt periodisch (SRCH für PS4 *und* PS5, optional zusätzlich an
// explizite Broadcast-Adressen), sammelt die Antworten des Discovery-Threads
// in einer Host-Liste (max. `hosts_max`), wirft Hosts nach `host_drop_pings`
// Pings ohne Antwort wieder raus und meldet jede Änderung der Liste über den
// Callback.
//
// Thread-Struktur wie im C:
// - Service-Thread: Ping-Schleife (`ping_initial_ms`, dann alle `ping_ms`),
//   startet/stoppt den Discovery-Empfangs-Thread.
// - Discovery-Thread (discovery.rs): empfängt Antworten -> `host_received`.
// - `state_mutex` schützt ping_index/hosts; `stop_cond` (ChiakiBoolPredCond)
//   ist hier eine StopPipe (wait_timeout liefert Canceled|Timeout, exakt die
//   Semantik von chiaki_bool_pred_cond_timedwait in dieser Schleife).

use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use super::discovery::{
    Discovery, DiscoveryCb, DiscoveryCmd, DiscoveryHost, DiscoveryPacket, DiscoveryThread,
    DISCOVERY_PORT_PS4, DISCOVERY_PORT_PS5, DISCOVERY_PROTOCOL_VERSION_PS4,
    DISCOVERY_PROTOCOL_VERSION_PS5,
};
use super::error::{ChiakiError, ChiakiResult};
use super::stoppipe::StopPipe;

/// Port von `ChiakiDiscoveryServiceCb`: erhält die komplette aktuelle
/// Host-Liste bei jeder Änderung.
pub type DiscoveryServiceCb = Arc<dyn Fn(&[DiscoveryHost]) + Send + Sync>;

/// Port von `ChiakiDiscoveryServiceOptions`.
#[derive(Clone, Debug)]
pub struct DiscoveryServiceOptions {
    pub hosts_max: usize,
    pub host_drop_pings: u64,
    pub ping_ms: u64,
    pub ping_initial_ms: u64,
    /// Zieladresse der Pings (Port wird pro Ping auf 987/9302 gesetzt).
    /// Bei 255.255.255.255 werden zusätzlich die `broadcast_addrs` gepingt.
    pub send_addr: SocketAddr,
    /// Zusätzliche Broadcast-Adressen (Interface-Broadcasts) für den Fall
    /// send_addr == 255.255.255.255.
    pub broadcast_addrs: Vec<SocketAddr>,
    /// Optionaler Hostname; wird beim ersten Ping aufgelöst und ersetzt dann
    /// `send_addr` (C: getaddrinfo im Ping, danach `send_host = NULL`).
    pub send_host: Option<String>,
}

/// Port von `ChiakiDiscoveryServiceHostDiscoveryInfo`.
#[derive(Clone, Copy, Debug, Default)]
struct HostDiscoveryInfo {
    last_ping_index: u64,
}

/// Der von `state_mutex` geschützte Zustand.
struct ServiceState {
    ping_index: u64,
    hosts: Vec<DiscoveryHost>,
    host_discovery_infos: Vec<HostDiscoveryInfo>,
}

struct Shared {
    state: Mutex<ServiceState>,
    hosts_max: usize,
    host_drop_pings: u64,
    cb: DiscoveryServiceCb,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, ServiceState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Port von `discovery_service_report_state()` — state_mutex muss
    /// gehalten werden (der Guard wird übergeben).
    fn report_state(&self, state: &ServiceState) {
        (self.cb)(&state.hosts);
    }

    /// Port von `discovery_service_drop_old_hosts()` — state_mutex gehalten.
    fn drop_old_hosts(&self, state: &mut ServiceState) {
        let mut change = false;

        // Der C-Loop läuft mit `for(i=0; i<count; i++)` und macht nach einem
        // Entfernen `if(i > 0) i--;` — bei i == 0 wird das nachgerückte
        // Element damit übersprungen (erst beim nächsten Ping geprüft). Das
        // Verhalten wird hier 1:1 nachgebildet.
        let mut i = 0usize;
        while i < state.hosts.len() {
            let info = state.host_discovery_infos[i];
            if info
                .last_ping_index
                .saturating_add(self.host_drop_pings)
                >= state.ping_index
            {
                i += 1;
                continue;
            }

            let host = &state.hosts[i];
            tracing::info!(
                "Discovery Service: Host with id {} is no longer available",
                host.host_id.as_deref().unwrap_or("")
            );

            state.hosts.remove(i);
            state.host_discovery_infos.remove(i);

            change = true;
            i = i.saturating_sub(1);
            i += 1; // for-Schleifen-Inkrement
        }

        if change {
            self.report_state(state);
        }
    }

    /// Port von `discovery_service_host_received()` (Callback des
    /// Discovery-Threads).
    fn host_received(&self, host: DiscoveryHost) {
        let mut state = self.lock();

        let Some(host_id) = host.host_id.as_deref() else {
            tracing::error!("Discovery Service received host without id");
            return;
        };
        // tracing::trace!("Discovery Service Received host with id {host_id}");

        let mut change = false;

        let index = state
            .hosts
            .iter()
            .position(|h| h.host_id.as_deref() == Some(host_id));

        let index = match index {
            Some(i) => i,
            None => {
                if state.hosts.len() == self.hosts_max {
                    tracing::error!("Discovery Service received new host, but no space available");
                    return;
                }

                tracing::info!("Discovery Service detected new host with id {host_id}");

                change = true;
                state.hosts.push(DiscoveryHost::default());
                state.host_discovery_infos.push(HostDiscoveryInfo::default());
                state.hosts.len() - 1
            }
        };

        let ping_index = state.ping_index;
        state.host_discovery_infos[index].last_ping_index = ping_index;

        let host_slot = &mut state.hosts[index];

        if host_slot.state != host.state || host_slot.host_request_port != host.host_request_port {
            change = true;
        }

        host_slot.state = host.state;
        host_slot.host_request_port = host.host_request_port;

        // UPDATE_STRING für alle String-Member (CHIAKI_DISCOVERY_HOST_STRING_FOREACH)
        fn update_string(slot: &mut Option<String>, new: &Option<String>, change: &mut bool) {
            if slot.as_deref() == new.as_deref() {
                // beide gleich (oder beide NULL) -> nichts zu tun
                return;
            }
            *change = true;
            *slot = new.clone();
        }
        // host_addr ist in Rust immer gesetzt (C: const char*, ggf. NULL)
        if host_slot.host_addr != host.host_addr {
            change = true;
            host_slot.host_addr = host.host_addr.clone();
        }
        update_string(&mut host_slot.system_version, &host.system_version, &mut change);
        update_string(
            &mut host_slot.device_discovery_protocol_version,
            &host.device_discovery_protocol_version,
            &mut change,
        );
        update_string(&mut host_slot.host_name, &host.host_name, &mut change);
        update_string(&mut host_slot.host_type, &host.host_type, &mut change);
        update_string(&mut host_slot.host_id, &host.host_id, &mut change);
        update_string(
            &mut host_slot.running_app_titleid,
            &host.running_app_titleid,
            &mut change,
        );
        update_string(
            &mut host_slot.running_app_name,
            &host.running_app_name,
            &mut change,
        );

        if change {
            self.report_state(&state);
        }
    }
}

/// Port von `ChiakiDiscoveryService`.
///
/// `new()` startet den Service-Thread (C: `chiaki_discovery_service_init`),
/// `Drop`/`fini()` stoppt und joint ihn (C: `chiaki_discovery_service_fini`).
pub struct DiscoveryService {
    stop_cond: Arc<StopPipe>,
    thread: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
}

impl DiscoveryService {
    /// Port von `chiaki_discovery_service_init()`.
    pub fn new(options: DiscoveryServiceOptions, cb: DiscoveryServiceCb) -> ChiakiResult<DiscoveryService> {
        let shared = Arc::new(Shared {
            state: Mutex::new(ServiceState {
                ping_index: 0,
                hosts: Vec::with_capacity(options.hosts_max),
                host_discovery_infos: Vec::with_capacity(options.hosts_max),
            }),
            hosts_max: options.hosts_max,
            host_drop_pings: options.host_drop_pings,
            cb,
        });

        let discovery = Arc::new(Discovery::new(options.send_addr.is_ipv6())?);

        let stop_cond = Arc::new(StopPipe::new());

        let thread_shared = Arc::clone(&shared);
        let thread_stop = Arc::clone(&stop_cond);
        let thread = std::thread::Builder::new()
            .name("Chiaki Discovery Service".to_owned())
            .spawn(move || discovery_service_thread_func(thread_shared, thread_stop, discovery, options))
            .map_err(|_| ChiakiError::Thread)?;

        Ok(DiscoveryService {
            stop_cond,
            thread: Some(thread),
            shared,
        })
    }

    /// Port von `chiaki_discovery_service_fini()`: stoppt den Service-Thread
    /// (und damit den Discovery-Thread) und joint ihn.
    pub fn fini(mut self) {
        self.stop_and_join();
    }

    /// Aktuelle Host-Liste (Kopie) — praktisch für Aufrufer/Tests; im C wird
    /// die Liste ausschließlich über den Callback gemeldet.
    pub fn hosts(&self) -> Vec<DiscoveryHost> {
        self.shared.lock().hosts.clone()
    }

    fn stop_and_join(&mut self) {
        self.stop_cond.stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for DiscoveryService {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

/// Port von `discovery_service_thread_func()`.
fn discovery_service_thread_func(
    shared: Arc<Shared>,
    stop_cond: Arc<StopPipe>,
    discovery: Arc<Discovery>,
    mut options: DiscoveryServiceOptions,
) {
    let recv_shared = Arc::clone(&shared);
    let host_cb: DiscoveryCb = Arc::new(move |host| recv_shared.host_received(host));
    let discovery_thread = match DiscoveryThread::start(Arc::clone(&discovery), host_cb) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("Discovery Service failed to start discovery thread: {e:?}");
            return;
        }
    };

    let mut err = stop_cond.wait_timeout(Duration::from_millis(options.ping_initial_ms));

    while err == ChiakiError::Timeout {
        discovery_service_ping(&shared, &discovery, &mut options);
        err = stop_cond.wait_timeout(Duration::from_millis(options.ping_ms));
    }

    let _ = discovery_thread.stop();
}

/// Port von `discovery_service_ping()`.
fn discovery_service_ping(shared: &Shared, discovery: &Discovery, options: &mut DiscoveryServiceOptions) {
    {
        let mut state = shared.lock();
        state.ping_index += 1;
        shared.drop_old_hosts(&mut state);
    }

    if let Some(send_host) = options.send_host.take() {
        // getaddrinfo(send_host, hints.ai_family = send_addr-Familie)
        let want_ipv6 = options.send_addr.is_ipv6();
        let resolved = match (send_host.as_str(), 0u16).to_socket_addrs() {
            Ok(r) => r,
            Err(_) => {
                tracing::error!("getaddrinfo failed");
                // C: return ohne send_host freizugeben -> nächster Ping
                // versucht es erneut.
                options.send_host = Some(send_host);
                return;
            }
        };

        // C nimmt das *letzte* passende addrinfo (memcpy in jeder Iteration)
        let mut ok = false;
        for ai in resolved {
            if ai.is_ipv6() != want_ipv6 {
                continue;
            }
            ok = true;
            options.send_addr = SocketAddr::new(ai.ip(), options.send_addr.port());
        }

        if !ok {
            tracing::error!("Failed to get addr for hostname");
            options.send_host = Some(send_host);
            return;
        }
        // send_host wurde konsumiert (C: free + NULL)
    }

    // tracing::trace!("Discovery Service sending ping");
    let mut packet = DiscoveryPacket {
        cmd: DiscoveryCmd::Srch,
        protocol_version: Some(DISCOVERY_PROTOCOL_VERSION_PS4.to_owned()),
        user_credential: 0,
    };

    let mut send_extra_broadcast = false;
    match options.send_addr {
        SocketAddr::V4(ref mut v4) => {
            v4.set_port(DISCOVERY_PORT_PS4);
            if v4.ip().octets() == [0xff, 0xff, 0xff, 0xff] {
                send_extra_broadcast = true;
            }
        }
        SocketAddr::V6(ref mut v6) => {
            v6.set_port(DISCOVERY_PORT_PS4);
        }
    }

    if discovery.send(&packet, options.send_addr).is_err() {
        tracing::error!("Discovery Service failed to send ping for PS4");
    }
    if send_extra_broadcast {
        for addr in options.broadcast_addrs.iter_mut() {
            addr.set_port(DISCOVERY_PORT_PS4);
            if discovery.send(&packet, *addr).is_err() {
                tracing::error!("Discovery Service failed to send extra broadcast ping for PS4");
            }
            // else tracing::trace!("Discovery Service pinged {}", addr.ip());
        }
    }

    packet.protocol_version = Some(DISCOVERY_PROTOCOL_VERSION_PS5.to_owned());
    options.send_addr.set_port(DISCOVERY_PORT_PS5);
    if discovery.send(&packet, options.send_addr).is_err() {
        tracing::error!("Discovery Service failed to send ping for PS5");
    }
    if send_extra_broadcast {
        for addr in options.broadcast_addrs.iter_mut() {
            addr.set_port(DISCOVERY_PORT_PS5);
            if discovery.send(&packet, *addr).is_err() {
                tracing::error!("Discovery Service failed to send extra broadcast ping for PS5");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::DiscoveryHostState;
    use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
    use std::sync::mpsc;
    use std::thread;

    fn shared_for_test(hosts_max: usize, drop_pings: u64, cb: DiscoveryServiceCb) -> Shared {
        Shared {
            state: Mutex::new(ServiceState {
                ping_index: 0,
                hosts: Vec::new(),
                host_discovery_infos: Vec::new(),
            }),
            hosts_max,
            host_drop_pings: drop_pings,
            cb,
        }
    }

    fn host(id: &str, state: DiscoveryHostState) -> DiscoveryHost {
        DiscoveryHost {
            state,
            host_request_port: 9295,
            host_addr: "192.168.0.10".to_owned(),
            host_id: Some(id.to_owned()),
            host_name: Some(format!("name-{id}")),
            ..Default::default()
        }
    }

    /// host_received: neue Hosts, Updates, hosts_max, fehlende host_id.
    #[test]
    fn host_received_add_update_and_limits() {
        let (tx, rx) = mpsc::channel::<Vec<DiscoveryHost>>();
        let cb: DiscoveryServiceCb = Arc::new(move |hosts| {
            let _ = tx.send(hosts.to_vec());
        });
        let shared = shared_for_test(2, 3, cb);

        // ohne host_id -> ignoriert, kein Callback
        shared.host_received(DiscoveryHost::default());
        assert!(rx.try_recv().is_err());

        // neuer Host -> Callback mit 1 Host
        shared.host_received(host("A", DiscoveryHostState::Ready));
        let list = rx.try_recv().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].host_id.as_deref(), Some("A"));
        assert_eq!(list[0].host_name.as_deref(), Some("name-A"));

        // gleicher Host, unverändert -> KEIN Callback
        shared.host_received(host("A", DiscoveryHostState::Ready));
        assert!(rx.try_recv().is_err());

        // Zustandsänderung -> Callback
        shared.host_received(host("A", DiscoveryHostState::Standby));
        let list = rx.try_recv().unwrap();
        assert_eq!(list[0].state, DiscoveryHostState::Standby);

        // String-Änderung -> Callback
        let mut a2 = host("A", DiscoveryHostState::Standby);
        a2.running_app_name = Some("Game".to_owned());
        shared.host_received(a2);
        let list = rx.try_recv().unwrap();
        assert_eq!(list[0].running_app_name.as_deref(), Some("Game"));

        // String wieder NULL -> Callback (C: UPDATE_STRING setzt NULL)
        shared.host_received(host("A", DiscoveryHostState::Standby));
        let list = rx.try_recv().unwrap();
        assert_eq!(list[0].running_app_name, None);

        // zweiter Host
        shared.host_received(host("B", DiscoveryHostState::Ready));
        let list = rx.try_recv().unwrap();
        assert_eq!(list.len(), 2);

        // hosts_max erreicht -> dritter Host wird verworfen
        shared.host_received(host("C", DiscoveryHostState::Ready));
        assert!(rx.try_recv().is_err());
        assert_eq!(shared.lock().hosts.len(), 2);
    }

    /// drop_old_hosts: Host verschwindet nach host_drop_pings Pings ohne
    /// Antwort; inkl. des C-Index-Verhaltens bei Entfernen an Position 0.
    #[test]
    fn drop_old_hosts_after_missing_pings() {
        let (tx, rx) = mpsc::channel::<Vec<DiscoveryHost>>();
        let cb: DiscoveryServiceCb = Arc::new(move |hosts| {
            let _ = tx.send(hosts.to_vec());
        });
        let shared = shared_for_test(8, 2, cb);

        shared.host_received(host("A", DiscoveryHostState::Ready));
        rx.try_recv().unwrap();
        // last_ping_index(A) = 0, drop_pings = 2 -> bleibt bis ping_index 2

        for expected_len in [1usize, 1] {
            let mut st = shared.lock();
            st.ping_index += 1;
            shared.drop_old_hosts(&mut st);
            assert_eq!(st.hosts.len(), expected_len);
            drop(st);
            assert!(rx.try_recv().is_err(), "kein Change erwartet");
        }

        // ping_index 3: 0 + 2 >= 3 falsch -> drop
        {
            let mut st = shared.lock();
            st.ping_index += 1;
            shared.drop_old_hosts(&mut st);
            assert_eq!(st.hosts.len(), 0);
        }
        let list = rx.try_recv().unwrap();
        assert!(list.is_empty());

        // C-Quirk: zwei abgelaufene Hosts, der an Index 0 wird entfernt, der
        // nachgerückte an Index 0 wird in DIESEM Durchlauf übersprungen.
        shared.host_received(host("X", DiscoveryHostState::Ready));
        shared.host_received(host("Y", DiscoveryHostState::Ready));
        while rx.try_recv().is_ok() {}
        {
            let mut st = shared.lock();
            st.ping_index += 10;
            shared.drop_old_hosts(&mut st);
            assert_eq!(st.hosts.len(), 1);
            assert_eq!(st.hosts[0].host_id.as_deref(), Some("Y"));
            // nächster Durchlauf räumt den Rest weg
            shared.drop_old_hosts(&mut st);
            assert!(st.hosts.is_empty());
        }
    }

    /// Ende-zu-Ende: Fake-Konsole auf localhost beantwortet SRCH-Pings;
    /// Service meldet den Host per Callback; nach Stop des Fakes verschwindet
    /// er wieder (host_drop_pings).
    #[test]
    fn service_end_to_end_localhost() {
        let fake = UdpSocket::bind("127.0.0.1:0").unwrap();
        let fake_port = fake.local_addr().unwrap().port();
        let _ = fake_port;

        // Der Service pingt an feste Ports (987/9302) — auf localhost lauscht
        // der Fake daher auf 9302 (PS5-Port). Ist der Port belegt, Test
        // überspringen. Der Dummy auf 987 schluckt die PS4-Pings, damit kein
        // ICMP-Port-unreachable (Windows: WSAECONNRESET auf dem Socket)
        // den Discovery-Empfangs-Thread beendet. Schlägt die Bindung fehl
        // (Port belegt), Test ebenfalls überspringen — sonst droht genau
        // dieser Reset unter Volllast.
        drop(fake);
        let Ok(fake) = UdpSocket::bind("127.0.0.1:9302") else {
            return;
        };
        fake.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        let Ok(_ps4_dummy) = UdpSocket::bind("127.0.0.1:987") else {
            return;
        };

        let responder_stop = Arc::new(StopPipe::new());
        let responder_stop2 = Arc::clone(&responder_stop);
        let responder = thread::spawn(move || {
            let mut buf = [0u8; 512];
            let mut answered = 0u32;
            while responder_stop2.check().is_ok() {
                let Ok((n, from)) = fake.recv_from(&mut buf) else {
                    continue;
                };
                assert!(buf[..n].starts_with(b"SRCH * HTTP/1.1\n"));
                let resp: &[u8] = b"HTTP/1.1 200 Ok\n\
                     host-type:PS5\n\
                     system-version:09000000\n\
                     device-discovery-protocol-version:00030010\n\
                     host-request-port:9295\n\
                     host-name:FakePS5\n\
                     host-id:FAKE5\n";
                let mut pkt = resp.to_vec();
                pkt.push(0);
                let _ = fake.send_to(&pkt, from);
                answered += 1;
            }
            answered
        });

        let (tx, rx) = mpsc::channel::<Vec<DiscoveryHost>>();
        let cb: DiscoveryServiceCb = Arc::new(move |hosts| {
            let _ = tx.send(hosts.to_vec());
        });

        let options = DiscoveryServiceOptions {
            hosts_max: 16,
            host_drop_pings: 2,
            ping_ms: 100,
            ping_initial_ms: 10,
            send_addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)),
            broadcast_addrs: Vec::new(),
            send_host: None,
        };
        let service = DiscoveryService::new(options, cb).unwrap();

        // Host taucht auf
        let list = rx.recv_timeout(Duration::from_secs(3)).expect("host appears");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].host_id.as_deref(), Some("FAKE5"));
        assert_eq!(list[0].host_name.as_deref(), Some("FakePS5"));
        assert_eq!(list[0].host_addr, "127.0.0.1");
        assert!(list[0].is_ps5());
        assert_eq!(service.hosts().len(), 1);

        // Fake stoppen -> nach host_drop_pings Pings verschwindet der Host
        responder_stop.stop();
        let answered = responder.join().unwrap();
        assert!(answered >= 1);

        let mut gone = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(list) if list.is_empty() => {
                    gone = true;
                    break;
                }
                Ok(_) => continue,
                Err(_) => continue,
            }
        }
        assert!(gone, "host should be dropped after missing pings");

        service.fini();
    }

    /// send_host wird beim ersten Ping aufgelöst und ersetzt send_addr.
    #[test]
    fn ping_resolves_send_host() {
        let cb: DiscoveryServiceCb = Arc::new(|_| {});
        let shared = shared_for_test(4, 2, cb);
        let discovery = Discovery::new(false).unwrap();
        let mut options = DiscoveryServiceOptions {
            hosts_max: 4,
            host_drop_pings: 2,
            ping_ms: 100,
            ping_initial_ms: 10,
            send_addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)),
            broadcast_addrs: Vec::new(),
            send_host: Some("localhost".to_owned()),
        };
        discovery_service_ping(&shared, &discovery, &mut options);
        assert_eq!(options.send_host, None);
        assert_eq!(options.send_addr.ip(), std::net::IpAddr::V4(Ipv4Addr::LOCALHOST));
        // nach dem Ping steht der zuletzt gesetzte Port (PS5)
        assert_eq!(options.send_addr.port(), DISCOVERY_PORT_PS5);
        assert_eq!(shared.lock().ping_index, 1);

        // nicht auflösbarer Host: send_host bleibt stehen, ping_index zählt
        options.send_host = Some("this-host-does-not-exist.invalid".to_owned());
        discovery_service_ping(&shared, &discovery, &mut options);
        assert!(options.send_host.is_some());
        assert_eq!(shared.lock().ping_index, 2);
    }
}
