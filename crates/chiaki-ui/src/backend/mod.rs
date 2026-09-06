//! Backend-Layer der chiaki-ui: settings/discovery/controllers/session —
//! komplett ohne gpui-Bezug und damit Unit-testbar (`cargo test -p chiaki-ui`).
//!
//! Die UI kennt nur [`Backend`] + [`UiEvent`]. Backend-Threads schieben
//! Events in die [`UiEventQueue`]; die gpui-Event-Loop (app.rs) polllt
//! 1× pro Frame und ruft bei Treffern `cx.notify()`.

pub mod controllers;
pub mod discovery;
pub mod events;
pub mod psn;
pub mod sessions;

use std::sync::{Arc, Mutex};

use chiaki_settings::settings::Settings;

pub use controllers::ControllerHandle;
pub use discovery::{DiscoveryError, DiscoveryHandle};
pub use events::{HostId, UiEvent, UiEventQueue, UiEventSender};
pub use psn::{PsnConnectState, PsnDeviceInfo, PsnHandle, PsnUiEvent};
pub use sessions::{
    ActiveSession, ConnectRequest, LinkQuality, RegistHandle, RegistRequest, RegistState,
    SessionManager,
};

use crate::components::ToastData;

/// Fehler beim Backend-Aufbau.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("Settings-Fehler: {0}")]
    Settings(#[from] chiaki_settings::settings::Error),
    #[error("Discovery-Fehler: {0}")]
    Discovery(#[from] DiscoveryError),
}

/// Das komplette Backend (Clone-fähig, alle Handles geteilt).
#[derive(Clone)]
pub struct Backend {
    settings: Arc<Mutex<Settings>>,
    discovery: DiscoveryHandle,
    controllers: ControllerHandle,
    sessions: SessionManager,
    psn: PsnHandle,
    queue: Arc<UiEventQueue>,
}

impl Backend {
    /// Startet Discovery + Controller-Poll mit den Settings.
    /// Läuft Discovery nicht auf (keine Broadcast-Route), wird er als
    /// `disabled()`-Handle geführt — die UI bleibt benutzbar.
    pub fn start(settings: Arc<Mutex<Settings>>) -> Result<Self, BackendError> {
        let queue = Arc::new(UiEventQueue::new());
        let sender = queue.sender();

        let discovery = match DiscoveryHandle::start(sender.clone()) {
            Ok(handle) => handle,
            Err(err) => {
                tracing::error!("{err} — Discovery deaktiviert");
                DiscoveryHandle::disabled()
            }
        };

        let controllers = ControllerHandle::start(sender.clone(), 50);
        let sessions = SessionManager::new(Arc::clone(&settings), sender.clone(), discovery.clone());
        let psn = PsnHandle::new();

        // PSN-Token-Refresh beim App-Start (C++: refreshPsnToken im
        // QmlBackend-Aufbau) — ohne PSN-Login ein stiller No-Op; bei gültigen
        // Tokens wird danach die Geräteliste geladen.
        psn.refresh_tokens_if_needed(Arc::clone(&settings), sender.clone());

        Ok(Self { settings, discovery, controllers, sessions, psn, queue })
    }

    /// Settings-Handle (UI sperrt kurz für Getter/Setter).
    pub fn settings(&self) -> &Arc<Mutex<Settings>> {
        &self.settings
    }

    pub fn discovery(&self) -> &DiscoveryHandle {
        &self.discovery
    }

    pub fn controllers(&self) -> &ControllerHandle {
        &self.controllers
    }

    pub fn sessions(&self) -> &SessionManager {
        &self.sessions
    }

    /// PSN-Remote-Handle (Geräteliste, Token-Refresh, Connect-State).
    pub fn psn(&self) -> &PsnHandle {
        &self.psn
    }

    /// Alle pending Events (1×/Frame aus der gpui-Event-Loop).
    pub fn poll_events(&self) -> Vec<UiEvent> {
        self.queue.poll()
    }

    /// Sender-Handle für Hintergrund-Threads, die später Events in die Queue
    /// schieben (z. B. der PSN-Login-Thread in [`crate::psn_login`]).
    pub fn event_sender(&self) -> UiEventSender {
        self.queue.sender()
    }

    /// Convenience: Toast in den Event-Stream schieben (Backend-getrieben).
    pub fn push_toast(&self, toast: ToastData) {
        self.queue.sender().send(UiEvent::Toast(toast));
    }

    /// Aufräumen (App-Ende): Discovery/Controller-Threads stoppen,
    /// Session sauber beenden.
    pub fn shutdown(&self) {
        self.sessions.stop_current();
        self.controllers.stop();
        self.discovery.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_events_leer_ohne_events() {
        let settings = Arc::new(Mutex::new(test_settings()));
        let backend = Backend::start(settings).expect("Backend-Start");
        // Threads stoppen, dann Pending-Events abwarten/auffressen (Discovery
        // und Controller-Poll können kurz nach dem Start Connected-Events
        // liefern — das ist kein Testgegenstand hier).
        backend.shutdown();
        for _ in 0..10 {
            backend.poll_events();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        backend.push_toast(ToastData::new(crate::components::ToastKind::Info, "Test"));
        let events = backend.poll_events();
        let toasts = events
            .iter()
            .filter(|e| matches!(e, UiEvent::Toast(_)))
            .count();
        assert_eq!(toasts, 1, "genau unser Toast, events={events:?}");
        assert!(backend.poll_events().is_empty(), "zweites poll muss leer sein");
    }

    /// Settings mit Temp-Pfaden (kein Schreiben ins echte Profil).
    pub(crate) fn test_settings() -> Settings {
        let base = std::env::temp_dir().join(format!(
            "chiaki-ui-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        let paths = chiaki_settings::settings::SettingsPaths {
            settings: base.join("settings.ini"),
            default_settings: base.join("settings.default.ini"),
            placebo: base.join("placebo_render_params.ini"),
            base,
        };
        Settings::open_at(paths).unwrap()
    }
}
