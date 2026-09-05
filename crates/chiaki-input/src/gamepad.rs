// SPDX-License-Identifier: AGPL-3.0-only
// Port von gui/src/controllermanager.cpp (SDL-Gamecontroller-Handling) auf gilrs.
//
// Das Button-Layout folgt exakt dem SDL-Mapping aus Controller::HandleButtonEvent:
//   A->CROSS, B->MOON, X->BOX, Y->PYRAMID, DPad 1:1, LEFTSHOULDER->L1,
//   RIGHTSHOULDER->R1, LEFTSTICK->L3, RIGHTSTICK->R3, START->OPTIONS,
//   BACK->SHARE, GUIDE->PS, TOUCHPAD->TOUCHPAD.
// Achsen wie Controller::HandleAxisEvent:
//   TRIGGERLEFT->l2_state (value>>7), TRIGGERRIGHT->r2_state, LEFTX/Y, RIGHTX/Y.
// (gilrs' XInput-Backend meldet Trigger als analoge Buttons LeftTrigger2/
// RightTrigger2 mit value 0..1 — Äquivalent zu SDLs Achse -32768..32767.)
//
// Wichtig (Windows): gilrs benutzt XInput und sieht deshalb nur XInput-Pads.
// Ein Xbox-Pad wird hier voll bedient; DualSense/DualShock4 erscheinen in
// dualsense.rs (native HID). Der C++-Client nutzt dafür SDL mit hidapi-Backend —
// die Rolle von SDL übernehmen hier zwei Backends.
//
// Der SDL-Event-Timer (UPDATE_INTERVAL_MS = 4) wird durch den Poll-Thread
// nachgebildet: spawn_poll_thread() ruft poll() alle `interval_ms` auf und
// verteilt die Events an den Callback (wie ControllerManager::HandleEvents).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use gilrs::ff::{BaseEffect, BaseEffectType, EffectBuilder, Replay, Ticks};
use gilrs::{Button, Event, EventType, Gamepad, Gilrs};

use chiaki_core::controller::{
    BUTTON_BOX, BUTTON_CROSS, BUTTON_DPAD_DOWN, BUTTON_DPAD_LEFT, BUTTON_DPAD_RIGHT,
    BUTTON_DPAD_UP, BUTTON_L1, BUTTON_L3, BUTTON_MOON, BUTTON_OPTIONS, BUTTON_PS, BUTTON_PYRAMID,
    BUTTON_R1, BUTTON_R3, BUTTON_SHARE, ControllerState,
};

use crate::InputError;

/// `UPDATE_INTERVAL_MS` aus controllermanager.cpp.
pub const UPDATE_INTERVAL_MS: u64 = 4;

/// Chiaki-Geräte-IDs aus controllermanager.cpp (vendor id, product id).
pub const DUALSENSE_CONTROLLER_IDS: &[(u16, u16)] = &[(0x054c, 0x0ce6)];
/// DualSense Edge.
pub const DUALSENSE_EDGE_CONTROLLER_IDS: &[(u16, u16)] = &[(0x054c, 0x0df2)];
/// Handhelds (Steam Deck, Rog Ally, Legion Go, MSI Claw).
pub const HANDHELD_CONTROLLER_IDS: &[(u16, u16)] = &[
    (0x28de, 0x1205), // Steam Deck
    (0x0b05, 0x1abe), // Rog Ally
    (0x17ef, 0x6182), // Legion Go
    (0x0db0, 0x1901), // MSI Claw
];
/// Steam Virtual Controller (von Steam Input durchgereichte Pads).
pub const STEAM_VIRTUAL_CONTROLLER_IDS: &[(u16, u16)] = &[(0x28de, 0x11ff)];

/// Stabile Identität eines Eingabegeräts über beide Backends hinweg.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DeviceId {
    /// gilrs/XInput-Gerät (GamepadId aus gilrs).
    Gilrs(u32),
    /// Natives HID-Gerät aus dualsense.rs (Pfad des hidapi-Handles).
    NativeDualSense(String),
}

/// Geräte-Metadaten wie `Controller::GetType/GetGUIDString/GetVIDPIDString/Is*`
/// aus controllermanager.cpp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GamepadDeviceInfo {
    pub name: String,
    /// gilrs-UUID (hex-String, Ersatz für den SDL-GUID-String).
    pub guid: String,
    pub vendor_id: Option<u16>,
    pub product_id: Option<u16>,
    pub is_dualsense: bool,
    pub is_dualsense_edge: bool,
    pub is_handheld: bool,
    pub is_steam_virtual: bool,
    pub is_steam_virtual_unmasked: bool,
}

impl GamepadDeviceInfo {
    /// Port der VID/PID-Sets aus controllermanager.cpp.
    pub fn from_vid_pid(vendor_id: Option<u16>, product_id: Option<u16>) -> Self {
        let vidpid = match (vendor_id, product_id) {
            (Some(v), Some(p)) => (v, p),
            _ => (0, 0),
        };
        GamepadDeviceInfo {
            name: String::new(),
            guid: String::new(),
            vendor_id,
            product_id,
            is_dualsense: contains(DUALSENSE_CONTROLLER_IDS, vidpid),
            is_dualsense_edge: contains(DUALSENSE_EDGE_CONTROLLER_IDS, vidpid),
            is_handheld: contains(HANDHELD_CONTROLLER_IDS, vidpid),
            is_steam_virtual: contains(STEAM_VIRTUAL_CONTROLLER_IDS, vidpid),
            is_steam_virtual_unmasked: contains(STEAM_VIRTUAL_CONTROLLER_IDS, vidpid),
        }
    }

    /// Port von `Controller::IsPS()` (PS3/PS4/PS5).
    pub fn is_ps(&self) -> bool {
        matches!(
            (self.vendor_id, self.product_id),
            (Some(0x054c), Some(_)) | (Some(0x0250), Some(_)) | (Some(0x054c), None)
        )
    }
}

fn contains(set: &[(u16, u16)], vidpid: (u16, u16)) -> bool {
    set.contains(&vidpid)
}

/// Events wie sie ControllerManager::ControllerEvent/UpdateState erzeugen.
#[derive(Debug, Clone)]
pub enum GamepadEvent {
    /// Neues Gerät (SDL_JOYDEVICEADDED / EventType::Connected).
    Connected {
        device: DeviceId,
        info: GamepadDeviceInfo,
        state: ControllerState,
    },
    /// Gerät weg (SDL_JOYDEVICEREMOVED / EventType::Disconnected).
    Disconnected(DeviceId),
    /// State hat sich geändert (emit StateChanged im C++).
    StateUpdated { device: DeviceId, state: ControllerState },
    /// Mic-Button (SDL_CONTROLLER_BUTTON_MISC1) wurde losgelassen — im C++
    /// wird `MicButtonPush()` für Push-to-Talk benutzt.
    MicButtonPush(DeviceId),
}

/// Rumble-Kommando für den gilrs-Thread (Force-Feedback braucht &mut Gilrs,
/// deshalb Marshalling in den Poll-Thread).
#[derive(Debug)]
struct RumbleCmd {
    device: DeviceId,
    /// Low-Frequency-Motor (wie `Controller::SetRumble(left, right)`).
    left: u8,
    /// High-Frequency-Motor.
    right: u8,
    duration_ms: u32,
}

/// gilrs-basierter Gamepad-Manager — Port von `ControllerManager`.
pub struct GamepadManager {
    gilrs: Arc<Mutex<Gilrs>>,
    states: Arc<Mutex<HashMap<DeviceId, ControllerState>>>,
    infos: Arc<Mutex<HashMap<DeviceId, GamepadDeviceInfo>>>,
    mic_pushed: Arc<Mutex<HashMap<DeviceId, bool>>>,
    rumble_tx: mpsc::Sender<RumbleCmd>,
    rumble_rx: Arc<Mutex<mpsc::Receiver<RumbleCmd>>>,
    running: Arc<AtomicBool>,
}

impl GamepadManager {
    /// Port von `ControllerManager::ControllerManager()` (SDL_Init-Teil).
    pub fn new() -> Result<Self, InputError> {
        let gilrs = Gilrs::new().map_err(|e| InputError::Gilrs(e.to_string()))?;
        // Unbounded reicht: Rumble-Kommandos sind selten und klein.
        let (rumble_tx, rumble_rx) = mpsc::channel();
        Ok(Self {
            gilrs: Arc::new(Mutex::new(gilrs)),
            states: Arc::new(Mutex::new(HashMap::new())),
            infos: Arc::new(Mutex::new(HashMap::new())),
            mic_pushed: Arc::new(Mutex::new(HashMap::new())),
            rumble_tx,
            rumble_rx: Arc::new(Mutex::new(rumble_rx)),
            running: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Liest alle pending Rumble-Kommandos (Internal).
    fn take_rumble_cmds(&self) -> Vec<RumbleCmd> {
        let mut out = Vec::new();
        if let Ok(rx) = self.rumble_rx.lock() {
            while let Ok(cmd) = rx.try_recv() {
                out.push(cmd);
            }
        }
        out
    }

    /// Port von `ControllerManager::UpdateAvailableControllers()` +
    /// `HandleEvents()`: leert die gilrs-Event-Queue und erzeugt
    /// GamepadEvents. Aufrufbar aus jedem Thread.
    pub fn poll(&self) -> Vec<GamepadEvent> {
        let mut events = Vec::new();
        let mut gilrs = self
            .gilrs
            .lock()
            .expect("gilrs mutex poisoned (thread panicking while polling?)");
        let mut dirty: Vec<DeviceId> = Vec::new();

        while let Some(Event { id, event, .. }) = gilrs.next_event() {
            let device = DeviceId::Gilrs(usize::from(id) as u32);
            match event {
                EventType::Connected => {
                    let gp = gilrs.gamepad(id);
                    let info = info_from_gilrs(&gp);
                    let state = build_state(&gp);
                    self.states
                        .lock()
                        .expect("states poisoned")
                        .insert(device.clone(), state);
                    self.infos
                        .lock()
                        .expect("infos poisoned")
                        .insert(device.clone(), info.clone());
                    events.push(GamepadEvent::Connected {
                        device: device.clone(),
                        info,
                        state,
                    });
                }
                EventType::Disconnected | EventType::Dropped => {
                    self.states
                        .lock()
                        .expect("states poisoned")
                        .remove(&device);
                    self.infos
                        .lock()
                        .expect("infos poisoned")
                        .remove(&device);
                    events.push(GamepadEvent::Disconnected(device));
                }
                EventType::ButtonPressed(..)
                | EventType::ButtonReleased(..)
                | EventType::ButtonChanged(..)
                | EventType::AxisChanged(..) => {
                    // Mic-Button (SDL MISC1, gilrs Button::C): Drücken setzt
                    // keinen Button, Loslassen emittiert MicButtonPush
                    // (Controller::HandleButtonEvent micbutton_push-Logik).
                    if matches!(event, EventType::ButtonPressed(Button::C, _)) {
                        self.mic_pushed
                            .lock()
                            .expect("mic poisoned")
                            .insert(device.clone(), true);
                    } else if matches!(event, EventType::ButtonReleased(Button::C, _))
                        && self
                            .mic_pushed
                            .lock()
                            .expect("mic poisoned")
                            .remove(&device)
                            .unwrap_or(false)
                    {
                        events.push(GamepadEvent::MicButtonPush(device.clone()));
                        continue;
                    }
                    if let Some(gp) = gilrs.connected_gamepad(id) {
                        let state = build_state(&gp);
                        let mut states = self.states.lock().expect("states poisoned");
                        let changed = states
                            .get(&device)
                            .map(|old| !old.equals(&state))
                            .unwrap_or(true);
                        if changed {
                            states.insert(device.clone(), state);
                            drop(states);
                            if !dirty.contains(&device) {
                                dirty.push(device);
                            }
                        }
                    }
                }
                EventType::ButtonRepeated(..) | EventType::ForceFeedbackEffectCompleted => {}
                // #[non_exhaustive]
                _ => {}
            }
        }

        // Ein StateUpdated pro geändertem Gerät (State-Koaleszenz, das C++
        // emittiert pro Event — die Session braucht nur den letzten Stand).
        for device in dirty {
            if let Some(state) = self.states.lock().expect("states poisoned").get(&device) {
                events.push(GamepadEvent::StateUpdated {
                    device,
                    state: *state,
                });
            }
        }

        // Rumble-Kommandos abarbeiten (rumble braucht &mut Gilrs).
        for cmd in self.take_rumble_cmds() {
            if let Err(err) = self.apply_rumble(&mut gilrs, &cmd) {
                tracing::warn!("Rumble fehlgeschlagen für {:?}: {}", cmd.device, err);
            }
        }

        events
    }

    fn apply_rumble(&self, gilrs: &mut Gilrs, cmd: &RumbleCmd) -> Result<(), InputError> {
        let DeviceId::Gilrs(id) = cmd.device else {
            return Err(InputError::Other("Rumble: kein gilrs-Gerät".into()));
        };
        // DeviceId speichert den gilrs-Index als u32 (GamepadId-Feld ist
        // crate-privat) — GamepadId über die Gamepad-Liste zurückholen.
        let Some((gpid, _)) = gilrs
            .gamepads()
            .find(|(gid, _)| usize::from(*gid) == id as usize)
        else {
            return Ok(()); // Gerät schon weg — wie SDL: still ignorieren
        };
        // Port von SetRumble: SDL_GameControllerRumble(left<<8, right<<8, 5000);
        // gilrs: Strong = LF-Motor (links), Weak = HF-Motor (rechts).
        let strong = (cmd.left as u16) << 8;
        let weak = (cmd.right as u16) << 8;
        let play_for = Ticks::from_ms(cmd.duration_ms.max(1));
        let mut builder = EffectBuilder::new();
        builder
            .add_effect(BaseEffect {
                kind: BaseEffectType::Strong { magnitude: strong },
                scheduling: Replay {
                    after: Ticks::from_ms(0),
                    play_for,
                    with_delay: Ticks::from_ms(0),
                },
                envelope: Default::default(),
            })
            .add_effect(BaseEffect {
                kind: BaseEffectType::Weak { magnitude: weak },
                scheduling: Replay {
                    after: Ticks::from_ms(0),
                    play_for,
                    with_delay: Ticks::from_ms(0),
                },
                envelope: Default::default(),
            });
        let effect = builder
            .gamepads(&[gpid])
            .finish(gilrs)
            .map_err(|e| InputError::Gilrs(e.to_string()))?;
        effect
            .play()
            .map_err(|e| InputError::Gilrs(e.to_string()))
    }

    /// Port von `Controller::SetRumble()` (nur gilrs-Geräte; DualSense wird
    /// in dualsense.rs nativ bedient, dort gibt es auch die Intensität).
    pub fn set_rumble(&self, device: &DeviceId, left: u8, right: u8, duration_ms: u32) {
        let _ = self.rumble_tx.send(RumbleCmd {
            device: device.clone(),
            left,
            right,
            duration_ms,
        });
        // Ohne laufenden Poll-Thread sofort abarbeiten.
        if !self.running.load(Ordering::Relaxed) {
            if let Ok(mut gilrs) = self.gilrs.try_lock() {
                for cmd in self.take_rumble_cmds() {
                    let _ = self.apply_rumble(&mut gilrs, &cmd);
                }
            }
        }
    }

    /// Aktuellen State eines Geräts (wie `Controller::GetState()`).
    pub fn state(&self, device: &DeviceId) -> Option<ControllerState> {
        self.states.lock().ok()?.get(device).copied()
    }

    /// Geräte-Infos aller bekannten Geräte.
    pub fn devices(&self) -> Vec<(DeviceId, GamepadDeviceInfo)> {
        self.infos
            .lock()
            .map(|infos| infos.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }

    /// Port des QTimer-Event-Loops (UPDATE_INTERVAL_MS): Poll-Thread, der
    /// poll() zyklisch aufruft und Events an `callback` verteilt.
    ///
    /// Nur einmal starten; Stop über [`GamepadManager::stop_poll_thread`] oder
    /// durch Droppen des Managers.
    pub fn spawn_poll_thread<F>(&self, interval_ms: u64, callback: F) -> JoinHandle<()>
    where
        F: Fn(GamepadEvent) + Send + Sync + 'static,
    {
        self.running.store(true, Ordering::Relaxed);
        let manager = Self {
            gilrs: Arc::clone(&self.gilrs),
            states: Arc::clone(&self.states),
            infos: Arc::clone(&self.infos),
            mic_pushed: Arc::clone(&self.mic_pushed),
            rumble_tx: self.rumble_tx.clone(),
            rumble_rx: Arc::clone(&self.rumble_rx),
            running: Arc::clone(&self.running),
        };
        let callback = Arc::new(callback);
        std::thread::Builder::new()
            .name("chiaki-input-gilrs".into())
            .spawn(move || {
                let interval = Duration::from_millis(interval_ms.max(1));
                let callback = callback;
                while manager.running.load(Ordering::Relaxed) {
                    for event in manager.poll() {
                        callback(event);
                    }
                    std::thread::sleep(interval);
                }
                tracing::debug!("Gamepad-Poll-Thread beendet");
            })
            .expect("Poll-Thread konnte nicht gestartet werden")
    }

    /// Beendet den Poll-Thread (JoinHandle aus spawn_poll_thread joinen).
    pub fn stop_poll_thread(&self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

impl Drop for GamepadManager {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

// (keine prozessglobalen States — gilrs-Kontext lebt im Manager)

fn info_from_gilrs(gp: &Gamepad<'_>) -> GamepadDeviceInfo {
    let mut info = GamepadDeviceInfo::from_vid_pid(gp.vendor_id(), gp.product_id());
    info.name = gp.name().to_owned();
    info.guid = gp
        .uuid()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    info
}

/// Stick-Achse (-1..1) -> i16 wie SDL (SDL_CONTROLLER_AXIS_*-Range).
pub fn axis_to_i16(value: f32) -> i16 {
    let v = (value.clamp(-1.0, 1.0) * 32767.0) as i32;
    v.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

/// Optionaler Deadzone-Rescale (C++ macht das nicht: Default 0.0 = 1:1).
pub fn apply_deadzone(value: f32, deadzone: f32) -> f32 {
    if deadzone <= 0.0 || deadzone >= 1.0 {
        return value;
    }
    let mag = value.abs();
    if mag <= deadzone {
        0.0
    } else {
        (mag - deadzone) / (1.0 - deadzone) * value.signum()
    }
}

/// Analog-Trigger (gilrs ButtonData value 0..1) -> u8 wie `value >> 7` beim
/// SDL-Achsenwert (-32768..32767 -> 0..255).
pub fn trigger_to_u8(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Port von `Controller::HandleButtonEvent` + `HandleAxisEvent` auf einen
/// kompletten gilrs-State-Lesevorgang (Button-Layout siehe Modul-Doku).
pub fn build_state(gp: &Gamepad<'_>) -> ControllerState {
    let mut state = ControllerState::default();

    // Ziffernblock der Face-Buttons: A/B/X/Y (SDL-Konvention).
    let buttons: &[(Button, u32)] = &[
        (Button::South, BUTTON_CROSS),
        (Button::East, BUTTON_MOON),
        (Button::West, BUTTON_BOX),
        (Button::North, BUTTON_PYRAMID),
        (Button::DPadLeft, BUTTON_DPAD_LEFT),
        (Button::DPadRight, BUTTON_DPAD_RIGHT),
        (Button::DPadUp, BUTTON_DPAD_UP),
        (Button::DPadDown, BUTTON_DPAD_DOWN),
        (Button::LeftTrigger, BUTTON_L1),
        (Button::RightTrigger, BUTTON_R1),
        (Button::LeftThumb, BUTTON_L3),
        (Button::RightThumb, BUTTON_R3),
        (Button::Start, BUTTON_OPTIONS),
        (Button::Select, BUTTON_SHARE),
        (Button::Mode, BUTTON_PS),
    ];
    for (btn, mask) in buttons {
        if let Some(data) = gp.button_data(*btn) {
            if data.is_pressed() {
                state.buttons |= mask;
            }
        }
    }

    // Touchpad-Klick: XInput-Pads haben keinen Touchpad-Button; der Wert
    // bleibt unset (wie SDL ohne SDL_CONTROLLER_BUTTON_TOUCHPAD). Das
    // native DualSense setzt ihn in dualsense.rs.

    // Analog-Trigger über die Button-Daten (gilrs: LeftTrigger2/RightTrigger2).
    if let Some(data) = gp.button_data(Button::LeftTrigger2) {
        state.l2_state = trigger_to_u8(data.value());
    }
    if let Some(data) = gp.button_data(Button::RightTrigger2) {
        state.r2_state = trigger_to_u8(data.value());
    }

    // Sticks.
    for (axis, setter) in [
        (gilrs::Axis::LeftStickX, 0u8),
        (gilrs::Axis::LeftStickY, 1),
        (gilrs::Axis::RightStickX, 2),
        (gilrs::Axis::RightStickY, 3),
    ] {
        if let Some(data) = gp.axis_data(axis) {
            let v = axis_to_i16(data.value());
            match setter {
                0 => state.left_x = v,
                1 => state.left_y = v,
                2 => state.right_x = v,
                _ => state.right_y = v,
            }
        }
    }

    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use chiaki_core::controller::ANALOG_BUTTON_L2;

    #[test]
    fn axis_conversion_matches_sdl_range() {
        assert_eq!(axis_to_i16(0.0), 0);
        assert_eq!(axis_to_i16(1.0), 32767);
        assert_eq!(axis_to_i16(-1.0), -32767);
        // 0.5 -> 16383.5 -> 16383 (C-Cast-Semantik, truncation)
        assert_eq!(axis_to_i16(0.5), 16383);
    }

    #[test]
    fn trigger_conversion_matches_sdl_shift() {
        // SDL: state.l2_state = value >> 7 bei Achse -32768..32767; voll
        // gedrückt = 32767 >> 7 = 255.
        assert_eq!(trigger_to_u8(0.0), 0);
        assert_eq!(trigger_to_u8(1.0), 255);
        assert_eq!(trigger_to_u8(0.5), 128);
        assert_eq!(trigger_to_u8(-0.7), 0, "negative Werte ignorieren");
    }

    #[test]
    fn deadzone_rescales_beyond_zone() {
        assert_eq!(apply_deadzone(0.05, 0.1), 0.0);
        let v = apply_deadzone(0.55, 0.1);
        assert!((v - 0.5).abs() < 1e-6, "0.55 mit dz 0.1 -> 0.5, got {v}");
        assert_eq!(apply_deadzone(0.0, 0.0), 0.0, "1:1-Modus (Default)");
    }

    #[test]
    fn vidpid_sets_match_controllermanager() {
        let ds = GamepadDeviceInfo::from_vid_pid(Some(0x054c), Some(0x0ce6));
        assert!(ds.is_dualsense && !ds.is_dualsense_edge);
        let edge = GamepadDeviceInfo::from_vid_pid(Some(0x054c), Some(0x0df2));
        assert!(edge.is_dualsense_edge);
        let deck = GamepadDeviceInfo::from_vid_pid(Some(0x28de), Some(0x1205));
        assert!(deck.is_handheld);
        let virtual_pad = GamepadDeviceInfo::from_vid_pid(Some(0x28de), Some(0x11ff));
        assert!(virtual_pad.is_steam_virtual);
        assert!(ds.is_ps());
    }

    // Hardware-Test — braucht ein echtes XInput-Pad (z. B. Xbox-Controller):
    // verbinden, `cargo test -p chiaki-input -- --ignored` ausführen.
    #[test]
    #[ignore = "braucht ein reales XInput-Gamepad an der Hardware"]
    fn hw_gilrs_poll_events() {
        let manager = GamepadManager::new().expect("gilrs init");
        // Rumble kurz auslösen (LF/HF voll für 300 ms)
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut connected = None;
        while std::time::Instant::now() < deadline {
            for event in manager.poll() {
                match &event {
                    GamepadEvent::Connected { device, .. } => {
                        connected = Some(device.clone());
                        tracing::info!("verbunden: {:?}", event);
                    }
                    GamepadEvent::StateUpdated { state, .. } => {
                        tracing::info!("state: buttons={:#x} lx={}", state.buttons, state.left_x)
                    }
                    other => tracing::info!("{other:?}"),
                }
            }
            if connected.is_some() {
                manager.set_rumble(connected.as_ref().unwrap(), 0xFF, 0xFF, 300);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
        assert!(connected.is_some(), "kein Gamepad verbunden");
    }

    #[test]
    fn l2_bit_is_analog_not_button() {
        // Sicherstellen, dass der Trigger-Pfad ANALOG_BUTTON_L2 nicht als
        // Button-Bit setzt (sonst overlappt die Bitmaske).
        assert_eq!(ANALOG_BUTTON_L2, 1 << 16);
        assert_eq!(BUTTON_CROSS, 1 << 0);
    }
}
