//! DiscoveryHandle: Wrapper um `chiaki_core::discoveryservice::DiscoveryService`.
//!
//! Startet den Service wie in chiaki-cli/discover.rs (Limited Broadcast,
//! PS4:987 + PS5:9302 pings der Service selbst) und hält einen lokalen
//! Host-Snapshot, aus dem `HostFound`/`HostRemoved`-Diffs erzeugt werden.
//! Der Callback läuft im Discovery-Thread → nur `UiEventSender`.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use chiaki_core::discovery::DiscoveryHost;
use chiaki_core::discoveryservice::{DiscoveryService, DiscoveryServiceOptions};

use super::events::{UiEvent, UiEventSender};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wakeup_credential_wie_cpp_sendwakeup() {
        // \0-gefüllter 16-Byte-Key, ASCII-Hex → Zahl (toULongLong base 16).
        let mut key = [0u8; 16];
        key[..8].copy_from_slice(b"1234ABCD");
        assert_eq!(wakeup_credential(&key).unwrap(), 0x1234ABCD);

        // 8 Zeichen ist die Obergrenze, 9 zu lang.
        let mut key = [0u8; 16];
        key[..9].copy_from_slice(b"1234ABCDE");
        assert!(wakeup_credential(&key).is_err());

        // Nicht-Hex-Zeichen → ungültig (wie !ok im C++).
        let key = *b"ZYXWVUTSRQPONMLK";
        assert!(wakeup_credential(&key).is_err());

        // Leerer Key (sofort \0) → ungültig.
        assert!(wakeup_credential(&[0; 16]).is_err());
    }
}

/// Fehler der Discovery-Aufsetzung.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("DiscoveryService konnte nicht gestartet werden: {0}")]
    Start(#[from] chiaki_core::error::ChiakiError),
    /// Wakeup-Paket konnte nicht gesendet werden (Port von
    /// `DiscoveryManager::SendWakeup` → Exception). Bewusst ohne
    /// `#[from]`: [`DiscoveryError::Start`] bezieht denselben
    /// ChiakiError, zwei `#[from]`-Quellen desselben Typs sind unzulässig.
    #[error("Wakeup fehlgeschlagen: {0}")]
    Wake(chiaki_core::error::ChiakiError),
}

#[derive(Clone)]
pub struct DiscoveryHandle {
    /// Snapshot der aktuellen Hosts (immer konsistent mit dem letzten Callback).
    hosts: Arc<Mutex<Vec<DiscoveryHost>>>,
    /// Für `fini()` beim App-Ende. `None` im Fallback-Fall (Start fehlgeschlagen).
    service: Arc<Mutex<Option<DiscoveryService>>>,
}

impl DiscoveryHandle {
    /// Startet den Discovery-Service mit chiaki-ng-Defaults
    /// (host_drop_pings 3, ping 500 ms, initial 50 ms — siehe cli/discover.rs).
    pub fn start(events: UiEventSender) -> Result<Self, DiscoveryError> {
        Self::start_with_options(events, 16, 3, 500, 50)
    }

    /// Mit expliziten Optionen (tests).
    pub fn start_with_options(
        events: UiEventSender,
        hosts_max: usize,
        host_drop_pings: u64,
        ping_ms: u64,
        ping_initial_ms: u64,
    ) -> Result<Self, DiscoveryError> {
        // Limited Broadcast; der Service setzt pro Ping den Port (987/9302).
        let send_addr: SocketAddr = SocketAddr::new(IpAddr::from([255, 255, 255, 255]), 0);

        let hosts = Arc::new(Mutex::new(Vec::<DiscoveryHost>::new()));
        let hosts_for_cb = Arc::clone(&hosts);

        let cb: chiaki_core::discoveryservice::DiscoveryServiceCb = Arc::new(move |new_hosts| {
            let mut prev = hosts_for_cb.lock().unwrap_or_else(|e| e.into_inner());
            // Diff: neu dazugekommene Adressen → HostFound, verschwundene → HostRemoved.
            for host in new_hosts {
                let known = prev.iter().any(|h| h.host_addr == host.host_addr);
                if !known {
                    events.send(UiEvent::HostFound(host.clone()));
                }
            }
            for old in prev.iter() {
                let gone = !new_hosts.iter().any(|h| h.host_addr == old.host_addr);
                if gone {
                    events.send(UiEvent::HostRemoved { host_addr: old.host_addr.clone() });
                }
            }
            if new_hosts.len() != prev.len()
                || new_hosts.iter().zip(prev.iter()).any(|(a, b)| a.host_addr != b.host_addr)
            {
                events.send(UiEvent::HostsChanged);
            }
            *prev = new_hosts.to_vec();
        });

        let service = DiscoveryService::new(
            DiscoveryServiceOptions {
                hosts_max,
                host_drop_pings,
                ping_ms,
                ping_initial_ms,
                send_addr,
                // Port des C++ discoverymanager.cpp: neben 255.255.255.255
                // alle Subnetz-Broadcasts echter Ethernet/WLAN-Adapter pingen
                // (Multi-NIC-Systeme — VirtualBox & Co. — erreichen das
                // Limited Broadcast sonst nicht).
                broadcast_addrs: lan_broadcast_addrs(),
                send_host: None,
            },
            cb,
        )?;

        tracing::info!(
            "Discovery Service gestartet (Broadcast 255.255.255.255 + {} Interface-Broadcasts, {ping_ms} ms Ping)",
            lan_broadcast_addrs().len()
        );
        Ok(Self {
            hosts,
            service: Arc::new(Mutex::new(Some(service))),
        })
    }

    /// Fallback-Handle ohne laufenden Service (Start fehlgeschlagen) —
    /// die UI bleibt benutzbar (manuelle Hosts).
    pub fn disabled() -> Self {
        Self { hosts: Arc::new(Mutex::new(Vec::new())), service: Arc::new(Mutex::new(None)) }
    }

    /// Aktueller Host-Snapshot.
    pub fn hosts(&self) -> Vec<DiscoveryHost> {
        self.hosts.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Port von `DiscoveryManager::SendWakeup` (discoverymanager.cpp):
    /// Regist-Key (16 Bytes, \0-gefüllt) an der ersten 0 kappen, als
    /// Hex-Zahl interpretieren (max. 8 Zeichen — länger/unültig ist ein
    /// ungültiger Key) und ein Discovery-Wakeup-Paket an `host:987/9302`
    /// senden. Der laufende Service-Socket wird vom DiscoveryService
    /// gehalten und ist hier nicht erreichbar — wie im C-Fallback
    /// (`service_active == false`) wird ein temporärer Discovery-Socket
    /// für das eine Paket aufgesetzt.
    pub fn wake(&self, host: &str, regist_key: &[u8; 16], ps5: bool) -> Result<(), DiscoveryError> {
        let credential = wakeup_credential(regist_key)?;
        chiaki_core::discovery::wakeup(None, host, credential, ps5)
            .map_err(DiscoveryError::Wake)
    }

    /// Stoppt den Service (App-Ende). Wird von `Backend::shutdown` gerufen;
    /// absichtlich KEIN `Drop`-Impl: `DiscoveryHandle` ist ein Clone-Handle
    /// (`Arc` geteilt) — ein Drop eines Klons darf den Service nicht stoppen.
    pub fn stop(&self) {
        if let Some(service) = self.service.lock().unwrap_or_else(|e| e.into_inner()).take() {
            service.fini();
            tracing::info!("Discovery Service gestoppt");
        }
    }
}

/// Port der Credential-Ableitung aus `DiscoveryManager::SendWakeup`
/// (discoverymanager.cpp): Key an der ersten 0 kappen, Bytes als
/// ASCII-String lesen und als Hex-Zahl parsen (`toULongLong(&ok, 16)`).
/// Keys länger als 8 Zeichen oder mit Nicht-Hex-Zeichen sind ungültig.
pub(crate) fn wakeup_credential(regist_key: &[u8; 16]) -> Result<u64, DiscoveryError> {
    let key_len = regist_key.iter().position(|&b| b == 0).unwrap_or(regist_key.len());
    let key = &regist_key[..key_len];
    let invalid = || {
        tracing::error!("DiscoveryManager got invalid regist key for wakeup");
        DiscoveryError::Wake(chiaki_core::error::ChiakiError::InvalidData)
    };
    if key.is_empty() || key.len() > 8 {
        return Err(invalid());
    }
    let key_str = std::str::from_utf8(key).map_err(|_| invalid())?;
    u64::from_str_radix(key_str, 16).map_err(|_| invalid())
}


/// Port des C++ discoverymanager.cpp (Windows-Zweig): Subnetz-Broadcast-
/// Adressen aller echten Ethernet/WLAN-Adapter via `GetAdaptersInfo`
/// (`ip | !mask`). Virtuelle/sonstige Adapter (VirtualBox, VPN, Loopback)
/// werden wie im C++ über den Typ gefiltert (Ethernet=6, 802.11=71).
/// Fehler führen zu einer leeren Liste (Limited Broadcast bleibt aktiv).
fn lan_broadcast_addrs() -> Vec<SocketAddr> {
    use windows::Win32::NetworkManagement::IpHelper::{GetAdaptersInfo, IP_ADAPTER_INFO};

    const MIB_IF_TYPE_ETHERNET: u32 = 6;
    const IF_TYPE_IEEE80211: u32 = 71;
    const NO_ERROR: u32 = 0;

    let mut out: Vec<SocketAddr> = Vec::new();
    unsafe {
        // Größenaufruf → Buffer → tatsächlicher Aufruf (C: MALLOC/OVERFLOW).
        let mut len: u32 = 0;
        // Erster Aufruf liefert die nötige Buffergröße; je nach Windows-
        // Version kommt ERROR_BUFFER_OVERFLOW oder ein anderer Code zurück —
        // entscheidend ist nur, dass len gesetzt wurde.
        GetAdaptersInfo(None, &mut len);
        if len == 0 {
            tracing::error!("DiscoveryManager: GetAdaptersInfo size query failed");
            return out;
        }
        let mut buf = vec![0u8; len as usize];
        let adapters = buf.as_mut_ptr() as *mut IP_ADAPTER_INFO;
        if GetAdaptersInfo(Some(adapters), &mut len) != NO_ERROR {
            tracing::error!("DiscoveryManager: GetAdaptersInfo failed");
            return out;
        }

        let i8_to_str = |arr: &[i8]| -> String {
            arr.iter()
                .take_while(|&&b| b != 0)
                .map(|&b| b as u8)
                .collect::<Vec<u8>>()
                .into_iter()
                .map(|b| b as char)
                .collect()
        };

        let mut p = adapters;
        loop {
            let adapter: &IP_ADAPTER_INFO = &*p;
            // C++: nur Ethernet und WLAN.
            if adapter.Type == IF_TYPE_IEEE80211 || adapter.Type == MIB_IF_TYPE_ETHERNET {
                let ip_str = i8_to_str(&adapter.IpAddressList.IpAddress.String);
                let ip_str: &str = &ip_str;
                let mask_str: String = i8_to_str(&adapter.IpAddressList.IpMask.String);
                let mask_str: &str = &mask_str;
                if !ip_str.is_empty() && ip_str != "0.0.0.0" {
                    if let (Ok(ip), Ok(mask)) = (
                        ip_str.parse::<std::net::Ipv4Addr>(),
                        mask_str.parse::<std::net::Ipv4Addr>(),
                    ) {
                        // broadcast = ip | !mask (C++-Formel)
                        let broadcast = std::net::Ipv4Addr::from(
                            u32::from(ip) | !u32::from(mask),
                        );
                        let sock = SocketAddr::new(IpAddr::V4(broadcast), 0);
                        if !out.contains(&sock) {
                            tracing::info!(
                                "DiscoveryManager: Interface-Broadcast {broadcast} ({ip_str})"
                            );
                            out.push(sock);
                        }
                    }
                }
            }
            let next = adapter.Next;
            if next.is_null() {
                break;
            }
            p = next;
        }
    }
    out
}
