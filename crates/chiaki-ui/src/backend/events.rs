//! UiEvent + Event-Queue: der einzige Kanal von Backend-Threads zur UI.
//!
//! gpui hat keine Blocking-Events: alle Backend-Threads (Discovery,
//! Controller-Poll, Session) schieben [`UiEvent`]s in eine
//! [`UiEventQueue`]; die UI polllt 1×/Frame via
//! [`Backend::poll_events`](super::Backend::poll_events) und ruft bei
//! Treffern `cx.notify()` (Event-Loop in app.rs). `mpsc` statt
//! crossbeam — Reihenfolge = Zustellung, kein Extra-Dependency.

use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::Mutex;

use chiaki_core::discovery::DiscoveryHost;
use chiaki_core::session::SessionEvent;
use chiaki_input::GamepadEvent;

use crate::components::ToastData;

/// Ein stabiler Host-Bezug (Settings-Registry vs. Discovery vs. PSN).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum HostId {
    /// Registrierter Host (Settings-Registry) — Schlüssel ist die MAC.
    Registered { mac: [u8; 6] },
    /// Manueller Host — die Settings-ID.
    Manual { id: i32 },
    /// PSN-Remote-Host — die DUID.
    Psn { duid: String },
    /// Nur-Adresse (unregistriert entdeckt / Direktverbindung).
    Address { host: String },
}

impl HostId {
    /// Stabile Debug-Anzeige für Logs.
    pub fn describe(&self) -> String {
        match self {
            HostId::Registered { mac } => {
                format!("registered:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                    mac[0], mac[1], mac[2], mac[3], mac[4], mac[5])
            }
            HostId::Manual { id } => format!("manual:{id}"),
            HostId::Psn { duid } => format!("psn:{duid}"),
            HostId::Address { host } => format!("address:{host}"),
        }
    }
}

/// Alles, was die UI vom Backend wissen muss.
#[derive(Debug, Clone)]
pub enum UiEvent {
    /// Discovery hat einen (neuen) Host gesehen.
    HostFound(DiscoveryHost),
    /// Ein bekannter Discovery-Host antwortet nicht mehr.
    HostRemoved { host_addr: String },
    /// Discovery-Liste wurde aktualisiert (Batch-Notify nach Diff).
    HostsChanged,
    /// Session-Event der aktiven Stream-Session.
    Session { session_id: u64, event: SessionEvent },
    /// Gamepad/DualSense-Event.
    Controller(GamepadEvent),
    /// Toast anzeigen (Backend-getrieben, z. B. Verbindungsfehler).
    Toast(ToastData),
}

/// Sender-Handle für Backend-Threads (Clone-fähig).
#[derive(Clone)]
pub struct UiEventSender {
    tx: Sender<UiEvent>,
}

impl UiEventSender {
    pub fn send(&self, event: UiEvent) {
        // Ein geschlossener Receiver (App fährt runter) ist kein Fehler.
        let _ = self.tx.send(event);
    }
}

/// Empfangsseite: die UI polllt `poll()` 1×/Frame.
pub struct UiEventQueue {
    rx: Mutex<Receiver<UiEvent>>,
    tx_proxy: UiEventSender,
}

impl Default for UiEventQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl UiEventQueue {
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self { rx: Mutex::new(rx), tx_proxy: UiEventSender { tx } }
    }

    /// Sender-Handle (unabhängig klonbar, auch für `Arc<dyn Fn>`-Callbacks).
    pub fn sender(&self) -> UiEventSender {
        self.tx_proxy.clone()
    }

    /// Holt alle pending Events (nicht-blockierend), in Zustellungsreihenfolge.
    pub fn poll(&self) -> Vec<UiEvent> {
        let rx = self.rx.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(event) => out.push(event),
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_liefert_in_reihenfolge_und_leer_danach() {
        let queue = UiEventQueue::new();
        let tx = queue.sender();
        tx.send(UiEvent::HostsChanged);
        tx.send(UiEvent::HostRemoved { host_addr: "1.2.3.4".into() });

        let events = queue.poll();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], UiEvent::HostsChanged));
        assert!(matches!(&events[1], UiEvent::HostRemoved { host_addr } if host_addr == "1.2.3.4"));
        assert!(queue.poll().is_empty(), "zweites poll muss leer sein");
    }

    #[test]
    fn queue_nach_sender_drop_noch_lesbar() {
        let queue = UiEventQueue::new();
        let tx = queue.sender();
        tx.send(UiEvent::HostsChanged);
        drop(tx);
        assert_eq!(queue.poll().len(), 1);
        assert!(queue.poll().is_empty());
    }

    #[test]
    fn host_id_beschreibungen() {
        assert_eq!(HostId::Manual { id: 3 }.describe(), "manual:3");
        assert_eq!(HostId::Psn { duid: "abc".into() }.describe(), "psn:abc");
        assert_eq!(
            HostId::Registered { mac: [0, 1, 2, 3, 4, 5] }.describe(),
            "registered:00:01:02:03:04:05"
        );
        assert_eq!(HostId::Address { host: "10.0.0.5".into() }.describe(), "address:10.0.0.5");
    }
}
