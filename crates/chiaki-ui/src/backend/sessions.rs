//! SessionManager: Start/Stop einer Stream-Session (chiaki_core::session).
//!
//! Auflösung der Verbindungsparameter aus den Settings (je ps4/ps5 ×
//! local/remote: `video_profile_local_ps4()` etc.), Keyboard-Mapping aus
//! `keymap/*` (chiaki_input::KeyboardMapper), Session-Events → UiEvents.
//!
//! Video-Pfad (Schema, ausgebaut vom StreamView-Agenten):
//! `SessionCallbacks::video_frame` (rohe H264/H265-Samples)
//! → TODO chiaki_media::Decoder (NVDEC/FFmpeg) → NV12Frame
//! → optional `chiaki_media::VsrUpscaler` (settings/nv_vsr)
//! → `chiaki_render::presenter::VideoPresenter::set_frame`
//! → StreamView nimmt `take_image()` + `VideoFrameElement`.
//! Der Presenter hängt bereits an `ActiveSession` und kann sofort genutzt
//! werden, sobald die Decode-Pipeline steht.
//!
//! Audio: Opus-Frames kommen in `audio_pcm` an — Dekodierung
//! (chiaki_media::opus) + cpal-Ausgabe ist `AudioHandle`-TODO.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use chiaki_core::controller::ControllerState;
use chiaki_core::error::ChiakiResult;
use chiaki_core::session::{ConnectInfo, Session, SessionCallbacks, SessionEvent, VideoFrame};
use chiaki_core::takion::DisableAudioVideo as CoreDisableAudioVideo;
use chiaki_input::KeyboardMapper;
use chiaki_render::presenter::VideoPresenter;
use chiaki_settings::hosts::{ManualHost, RegisteredHost};
use chiaki_settings::settings::Settings;

use super::events::{HostId, UiEvent, UiEventSender};

/// Verbindungsqualität — wählt die local/remote-Settings-Gruppe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkQuality {
    /// Konsole im lokalen Netz (Discovery-Host).
    Local,
    /// Konsole hinter Internet/Remote-Play.
    Remote,
}

/// Anfrage für `SessionManager::connect`.
#[derive(Debug, Clone)]
pub struct ConnectRequest {
    pub host_id: HostId,
    /// IP/Hostname der Konsole.
    pub host: String,
    pub ps5: bool,
    /// `rp_regist_key` (16 Bytes, \0-gefüllt).
    pub regist_key: [u8; 16],
    /// `rp_key` (morning, 16 Bytes).
    pub morning: [u8; 16],
    pub link: LinkQuality,
    /// PSN-Account-ID (8 Bytes) — für PSN-Verbindungen; sonst 0en.
    pub psn_account_id: [u8; 8],
}

impl ConnectRequest {
    /// Aus einem registrierten Host + Adresse (Discovery/Manuell).
    pub fn from_registered(
        registered: &RegisteredHost,
        host: String,
        link: LinkQuality,
    ) -> Self {
        Self {
            host_id: HostId::Registered { mac: *registered.server_mac.mac() },
            host,
            ps5: registered.target.is_ps5(),
            regist_key: registered.rp_regist_key,
            morning: registered.rp_key,
            link,
            psn_account_id: [0; 8],
        }
    }

    /// Aus einem manuellen Host + dem zugehörigen registrierten Host.
    pub fn from_manual(
        manual: &ManualHost,
        registered: &RegisteredHost,
        link: LinkQuality,
    ) -> Self {
        let mut req = Self::from_registered(registered, manual.host.clone(), link);
        req.host_id = HostId::Manual { id: manual.id };
        req
    }

    /// Unregistrierte Direktverbindung (Auto-Regist-Pfad, später).
    pub fn from_address(host: String, ps5: bool, link: LinkQuality) -> Self {
        Self {
            host_id: HostId::Address { host: host.clone() },
            host,
            ps5,
            regist_key: [0; 16],
            morning: [0; 16],
            link,
            psn_account_id: [0; 8],
        }
    }
}

/// Settings-DisableAudioVideo → chiaki_core-Enum.
pub fn map_disable_audio_video(
    value: chiaki_settings::settings::DisableAudioVideo,
) -> CoreDisableAudioVideo {
    match value {
        chiaki_settings::settings::DisableAudioVideo::None => CoreDisableAudioVideo::NoneDisabled,
        chiaki_settings::settings::DisableAudioVideo::Audio => CoreDisableAudioVideo::AudioDisabled,
        chiaki_settings::settings::DisableAudioVideo::Video => CoreDisableAudioVideo::VideoDisabled,
        chiaki_settings::settings::DisableAudioVideo::AudioVideo => {
            CoreDisableAudioVideo::AudioVideoDisabled
        }
    }
}

/// Video-Profil aus den Settings je ps4/ps5 × local/remote
/// (settings.video_profile_*_ps* kapseln resolution/fps/bitrate/codec).
pub fn video_profile_for(req: &ConnectRequest, settings: &Settings) -> chiaki_settings::settings::ConnectVideoProfile {
    match (req.ps5, req.link) {
        (false, LinkQuality::Local) => settings.video_profile_local_ps4(),
        (false, LinkQuality::Remote) => settings.video_profile_remote_ps4(),
        (true, LinkQuality::Local) => settings.video_profile_local_ps5(),
        (true, LinkQuality::Remote) => settings.video_profile_remote_ps5(),
    }
}

/// Port der C++-StreamSession-Keyboard-Vorbereitung: `GetControllerMapping()`
/// (keymap/*-Overlay über den Defaults) → `KeyboardMapper`.
pub fn keyboard_mapper_from_settings(settings: &Settings) -> KeyboardMapper {
    let mut mapper = KeyboardMapper::default();
    for (button, key_name) in settings.controller_mapping() {
        let (Some(key), Some(target)) = (
            chiaki_input::Key::from_qt_name(&key_name),
            chiaki_input::ButtonOrAxis::from_ext(button),
        ) else {
            tracing::warn!("Keymap-Eintrag ignoriert: button={button} key='{key_name}'");
            continue;
        };
        mapper.set_mapping(key, target);
    }
    mapper
}

/// Testbarer Kern von `connect`: baut die `ConnectInfo` aus Request + Settings.
pub fn build_connect_info(req: &ConnectRequest, settings: &Settings) -> ChiakiResult<ConnectInfo> {
    let profile = video_profile_for(req, settings);
    let codec = match profile.codec {
        chiaki_settings::Codec::H264 => chiaki_core::Codec::H264,
        chiaki_settings::Codec::H265 => chiaki_core::Codec::H265,
        chiaki_settings::Codec::H265Hdr => chiaki_core::Codec::H265Hdr,
    };
    let video_profile = chiaki_core::video::ConnectVideoProfile {
        width: profile.width,
        height: profile.height,
        max_fps: profile.max_fps,
        bitrate: profile.bitrate,
        codec,
    };

    Ok(ConnectInfo {
        ps5: req.ps5,
        host: req.host.clone(),
        regist_key: req.regist_key,
        morning: req.morning,
        video_profile,
        video_profile_auto_downgrade: true,
        enable_keyboard: settings.keyboard_enabled(),
        enable_dualsense: true,
        audio_video_disabled: map_disable_audio_video(settings.audio_video_disabled()),
        auto_regist: false,
        holepunch_session: None,
        rudp_sock: None,
        psn_account_id: req.psn_account_id,
        packet_loss_max: settings.packet_loss_reported_max(),
        enable_idr_on_fec_failure: settings.idr_on_fec_failure_enabled(),
        av_reorder_timeout_us: (settings.reorder_timeout_ms().max(0) as u32).saturating_mul(1000),
    })
}

// ---------------------------------------------------------------------------
// Aktive Session
// ---------------------------------------------------------------------------

/// Steuerhandle der laufenden Session (von StreamView genutzt).
#[derive(Clone)]
pub struct ActiveSession {
    pub id: u64,
    /// Video-Presenter (StreamView: take_image + VideoFrameElement).
    pub presenter: VideoPresenter,
    shared: Arc<SessionShared>,
}

pub(crate) struct SessionShared {
    /// `Some` solange die Session läuft (gestoppt = None).
    pub session: Mutex<Option<Session>>,
    pub stop: AtomicBool,
    /// rohe Samples bis zur Decode-Pipeline (Diagnose/Statistik).
    pub video_samples: AtomicU64,
    /// Keyboard-Mapping (für den Input-Bridge des StreamViews).
    pub keyboard: Mutex<KeyboardMapper>,
}

impl ActiveSession {
    /// PIN setzen (LoginPinRequest-Event).
    pub fn set_login_pin(&self, pin: &str) {
        if let Some(session) = self.shared.session.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
        {
            if let Err(err) = session.set_login_pin(pin.as_bytes()) {
                tracing::error!("set_login_pin fehlgeschlagen: {err}");
            }
        }
    }

    /// Controller-State senden (Session-Feedback-Pfad).
    pub fn send_controller_state(&self, state: &ControllerState) {
        if let Some(session) = self.shared.session.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
        {
            if let Err(err) = session.send_controller_state(state) {
                tracing::warn!("send_controller_state fehlgeschlagen: {err}");
            }
        }
    }

    /// Keyboard-Mapping der Session (aus den Settings aufgelöst).
    pub fn keyboard_mapper(&self) -> KeyboardMapper {
        self.shared.keyboard.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Sauber stoppen: setzt das Stop-Flag; der Aufräum-Thread ruft
    /// `Session::stop/join/fini` und meldet das Quit-Event.
    pub fn request_stop(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }

    /// Läuft die Session noch?
    pub fn is_running(&self) -> bool {
        !self.shared.stop.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// SessionManager
// ---------------------------------------------------------------------------

/// Verwaltet maximal eine aktive Session (StreamView kümmert sich ums UI).
#[derive(Clone)]
pub struct SessionManager {
    settings: Arc<Mutex<Settings>>,
    events: UiEventSender,
    current: Arc<Mutex<Option<ActiveSession>>>,
    next_id: Arc<AtomicU64>,
}

impl SessionManager {
    pub fn new(settings: Arc<Mutex<Settings>>, events: UiEventSender) -> Self {
        Self {
            settings,
            events,
            current: Arc::new(Mutex::new(None)),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Handle der aktiven Session (falls vorhanden).
    pub fn active(&self) -> Option<ActiveSession> {
        self.current.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Startet eine Session (non-blocking): Session-Threads laufen im
    /// Hintergrund; Events kommen über den UiEvent-Stream.
    pub fn connect(&self, request: ConnectRequest) -> ChiakiResult<ActiveSession> {
        self.stop_current();

        let session_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let settings = self
            .settings
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let connect_info = build_connect_info(&request, &settings)?;
        let keyboard = keyboard_mapper_from_settings(&settings);
        let nv_vsr = settings.nv_vsr_enabled();
        let vsr_scale = settings.nv_vsr_scale();
        drop(settings);

        tracing::info!(
            "Starte Session #{session_id} zu {} (ps5={}, link={:?}, {}x{}@{} {})",
            request.host,
            request.ps5,
            request.link,
            connect_info.video_profile.width,
            connect_info.video_profile.height,
            connect_info.video_profile.max_fps,
            if connect_info.video_profile.codec.is_h265() { "H265" } else { "H264" },
        );

        // Presenter mit Stream-Auflösung (VSR-Skalierung macht der Decode-Pfad).
        let presenter = VideoPresenter::new(connect_info.video_profile.width, connect_info.video_profile.height);
        let shared = Arc::new(SessionShared {
            session: Mutex::new(None),
            stop: AtomicBool::new(false),
            video_samples: AtomicU64::new(0),
            keyboard: Mutex::new(keyboard),
        });

        let callbacks = Arc::new(BridgeCallbacks {
            session_id,
            events: self.events.clone(),
            presenter: presenter.clone(),
            shared: Arc::clone(&shared),
            nv_vsr,
            _vsr_scale: vsr_scale,
        });

        let mut session = Session::new(connect_info, callbacks)?;
        session.start()?;

        let shared_for_ctrl = Arc::clone(&shared);
        *shared_for_ctrl.session.lock().unwrap_or_else(|e| e.into_inner()) = Some(session);

        // Join/Stop-Thread: wartet aufs Stop-Flag, stoppt dann die Session
        // und räumt auf (stop/join/fini brauchen &mut — daher take()).
        let shared_for_thread = Arc::clone(&shared);
        let events = self.events.clone();
        let join = std::thread::Builder::new()
            .name(format!("chiaki-ui-session-{session_id}"))
            .spawn(move || {
                while !shared_for_thread.stop.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                let mut session = shared_for_thread
                    .session
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
                if let Some(session) = session.as_mut() {
                    session.stop();
                    let _ = session.join();
                    session.fini();
                }
                tracing::info!("Session #{session_id} beendet");
                events.send(UiEvent::Session {
                    session_id,
                    event: SessionEvent::Quit {
                        reason: chiaki_core::session::QuitReason::Stopped,
                        reason_str: "Session beendet".to_string(),
                    },
                });
            })
            .ok(); // JoinHandle wird detachiert; Stopp läuft über das Stop-Flag.
        std::mem::forget(join);

        let active = ActiveSession {
            id: session_id,
            presenter,
            shared: Arc::clone(&shared),
        };
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = Some(active.clone());
        Ok(active)
    }

    /// Stoppt die aktuelle Session (falls vorhanden).
    pub fn stop_current(&self) {
        if let Some(active) = self.current.lock().unwrap_or_else(|e| e.into_inner()).take() {
            active.request_stop();
        }
    }
}

// ---------------------------------------------------------------------------
// Callbacks-Bridge (Session-Threads → UiEvents / Presenter)
// ---------------------------------------------------------------------------

#[allow(dead_code)] // presenter/nv_vsr: vom Decode-Pfad des StreamViews genutzt
struct BridgeCallbacks {
    session_id: u64,
    events: UiEventSender,
    presenter: VideoPresenter,
    shared: Arc<SessionShared>,
    nv_vsr: bool,
    _vsr_scale: i64,
}

impl SessionCallbacks for BridgeCallbacks {
    fn video_frame(&self, _frame: VideoFrame<'_>) -> bool {
        // Zähler für Diagnose; die Decode-Pipeline (chiaki_media::Decoder →
        // NV12 → optional VsrUpscaler wenn settings/nv_vsr → presenter)
        // liefert der StreamView-Agent nach.
        self.shared.video_samples.fetch_add(1, Ordering::Relaxed);
        true
    }

    fn audio_pcm(&self, _pcm: &[u8]) {
        // TODO(AudioHandle): chiaki_media::opus::OpusDecoder → cpal-Ausgabe.
    }

    fn event(&self, ev: SessionEvent) {
        self.events.send(UiEvent::Session { session_id: self.session_id, event: ev });
    }

    fn haptics(&self, _data: &[u8]) {
        // TODO: HapticsPlayer (chiaki_input::HapticsPlayer).
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::tests::test_settings;

    #[test]
    fn connect_request_from_registered_uebernimmt_keys() {
        let mut host = RegisteredHost::default();
        host.target = chiaki_settings::hosts::Target::Ps5One;
        host.rp_regist_key = [7u8; 16];
        host.rp_key = [9u8; 16];
        host.server_nickname = "Wohnzimmer".into();

        let req = ConnectRequest::from_registered(&host, "192.168.1.50".into(), LinkQuality::Local);
        assert_eq!(req.host, "192.168.1.50");
        assert!(req.ps5, "Target::Ps5_1 → ps5");
        assert_eq!(req.regist_key, [7; 16]);
        assert_eq!(req.morning, [9; 16]);
        assert!(matches!(req.host_id, HostId::Registered { .. }));
    }

    #[test]
    fn video_profile_je_ps5_und_link() {
        let mut settings = test_settings();
        settings.set_resolution_local_ps4(chiaki_settings::settings::ResolutionPreset::P540);
        settings.set_resolution_local_ps5(chiaki_settings::settings::ResolutionPreset::P1080);
        settings.set_resolution_remote_ps5(chiaki_settings::settings::ResolutionPreset::P720);
        settings.set_fps_local_ps5(chiaki_settings::settings::FpsPreset::Fps60);
        settings.set_codec_local_ps5(chiaki_settings::settings::Codec::H265Hdr);
        settings.set_bitrate_local_ps5(20_000);

        let ps4_local = ConnectRequest::from_address("10.0.0.1".into(), false, LinkQuality::Local);
        let ps5_local = ConnectRequest::from_address("10.0.0.2".into(), true, LinkQuality::Local);
        let ps5_remote =
            ConnectRequest::from_address("88.1.2.3".into(), true, LinkQuality::Remote);

        let p = video_profile_for(&ps4_local, &settings);
        assert_eq!((p.width, p.height), (960, 540), "PS4 local → 540p-Preset");

        let p = video_profile_for(&ps5_local, &settings);
        assert_eq!((p.width, p.height, p.max_fps), (1920, 1080, 60), "PS5 local");
        assert_eq!(p.codec, chiaki_settings::settings::Codec::H265Hdr);
        assert_eq!(p.bitrate, 20_000, "Bitrate-Override greift");

        let p = video_profile_for(&ps5_remote, &settings);
        assert_eq!((p.width, p.height), (1280, 720), "PS5 remote → remote-Gruppe");
    }

    #[test]
    fn build_connect_info_mappt_settings_felder() {
        let mut settings = test_settings();
        settings.set_resolution_local_ps5(chiaki_settings::settings::ResolutionPreset::P1080);
        settings.set_fps_local_ps5(chiaki_settings::settings::FpsPreset::Fps60);
        settings.set_codec_local_ps5(chiaki_settings::settings::Codec::H265);
        settings.set_audio_video_disabled(
            chiaki_settings::settings::DisableAudioVideo::Audio,
        );
        settings.set_packet_loss_reported_max(0.25);
        settings.set_reorder_timeout_ms(32);
        settings.set_keyboard_enabled(false);
        settings.set_idr_on_fec_failure_enabled(true);

        let mut req = ConnectRequest::from_address("10.0.0.9".into(), true, LinkQuality::Local);
        req.regist_key = [1; 16];
        req.morning = [2; 16];

        let info = build_connect_info(&req, &settings).expect("ConnectInfo baubar");
        assert_eq!(info.host, "10.0.0.9");
        assert!(info.ps5);
        assert_eq!(info.regist_key, [1; 16]);
        assert_eq!(info.morning, [2; 16]);
        assert_eq!(
            (info.video_profile.width, info.video_profile.height, info.video_profile.max_fps),
            (1920, 1080, 60)
        );
        assert!(info.video_profile.codec.is_h265());
        assert_eq!(
            info.audio_video_disabled,
            CoreDisableAudioVideo::AudioDisabled,
            "settings-Enum → core-Enum"
        );
        assert!((info.packet_loss_max - 0.25).abs() < 1e-9);
        assert_eq!(info.av_reorder_timeout_us, 32_000);
        assert!(!info.enable_keyboard);
        assert!(info.enable_idr_on_fec_failure);
        assert!(!info.auto_regist);
        assert!(info.holepunch_session.is_none());
    }

    #[test]
    fn keyboard_mapper_aus_settings() {
        use chiaki_input::{ButtonOrAxis, Key};
        use chiaki_core::controller::BUTTON_CROSS;

        let mut settings = test_settings();
        // CROSS auf "Space" umlegen (Default wäre Return).
        settings.set_controller_button_mapping(BUTTON_CROSS, "Space");

        let mapper = keyboard_mapper_from_settings(&settings);
        let mut pressed = std::collections::HashSet::new();
        pressed.insert(Key::Space);
        let state = mapper.apply_keyboard_state(&pressed);
        assert_eq!(state.buttons & BUTTON_CROSS, BUTTON_CROSS, "Space → CROSS");
        // Alter Default (Return) darf CROSS nicht mehr feuern:
        let mut pressed = std::collections::HashSet::new();
        pressed.insert(Key::Return);
        let state = mapper.apply_keyboard_state(&pressed);
        assert_eq!(state.buttons & BUTTON_CROSS, 0);
        let _ = ButtonOrAxis::Button(BUTTON_CROSS);
    }
}
