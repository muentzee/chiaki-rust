//! ControllerHandle: `GamepadManager` (gilrs) + `DualSenseManager` (hidapi)
//! mit Poll-Threads → UiEvents; `active_controller_state()` für die Session.
//!
//! Beide Manager sind optional: schlägt gilrs/hidapi fehl (kein Gerät/
//! Treiber), läuft die UI ohne den jeweiligen Teil weiter.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use chiaki_core::controller::ControllerState;
use chiaki_input::{combine_states, DualSenseManager, GamepadManager};

use super::events::{UiEvent, UiEventSender};

#[derive(Clone)]
pub struct ControllerHandle {
    gamepad: Arc<Mutex<Option<GamepadManager>>>,
    dualsense: Arc<Mutex<Option<DualSenseManager>>>,
    stop: Arc<AtomicBool>,
}

impl ControllerHandle {
    /// Startet beide Manager + Poll-Threads. Fehler werden geloggt und
    /// führen NICHT zum Fehler (UI bleibt lauffähig).
    pub fn start(events: UiEventSender, poll_interval_ms: u64) -> Self {
        let stop = Arc::new(AtomicBool::new(false));

        let gamepad = match GamepadManager::new() {
            Ok(m) => {
                let events = events.clone();
                // Callback läuft im Poll-Thread: Fn(GamepadEvent).
                let handle = m.spawn_poll_thread(poll_interval_ms, move |ev| {
                    events.send(UiEvent::Controller(ev));
                });
                // JoinHandle droppen = Thread detachiert; Stop läuft über
                // stop_poll_thread() (der Thread hält seine eigenen Arc-Klone).
                std::mem::forget(handle);
                Some(m)
            }
            Err(err) => {
                tracing::warn!("GamepadManager nicht verfügbar: {err}");
                None
            }
        };

        let dualsense = match DualSenseManager::new() {
            Ok(m) => {
                let events = events.clone();
                let handle = m.spawn_read_thread(poll_interval_ms, move |ev| {
                    events.send(UiEvent::Controller(ev));
                });
                std::mem::forget(handle);
                Some(m)
            }
            Err(err) => {
                tracing::warn!("DualSenseManager nicht verfügbar: {err}");
                None
            }
        };

        Self {
            gamepad: Arc::new(Mutex::new(gamepad)),
            dualsense: Arc::new(Mutex::new(dualsense)),
            stop,
        }
    }

    /// Geräteseite (UI: Controller-Liste).
    pub fn devices(&self) -> Vec<(chiaki_input::DeviceId, chiaki_input::GamepadDeviceInfo)> {
        let mut out = Vec::new();
        if let Some(gp) = self.gamepad.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            out.extend(gp.devices());
        }
        if let Some(ds) = self.dualsense.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            out.extend(ds.devices());
        }
        out
    }

    /// Kombinierter Controller-State aller Geräte (chiaki_input::combine_states).
    /// Für `Session::send_controller_state()`.
    pub fn active_controller_state(&self) -> ControllerState {
        let mut states = Vec::new();
        if let Some(gp) = self.gamepad.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            for (device, _) in gp.devices() {
                if let Some(state) = gp.state(&device) {
                    states.push(state);
                }
            }
        }
        if let Some(ds) = self.dualsense.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            for (device, _) in ds.devices() {
                if let Some(state) = ds.state(&device) {
                    states.push(state);
                }
            }
        }
        combine_states(&states)
    }

    /// Poll-Threads stoppen (App-Ende).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(gp) = self.gamepad.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            gp.stop_poll_thread();
        }
        if let Some(ds) = self.dualsense.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            ds.stop_read_thread();
        }
    }
}
