//! SessionManager: Start/Stop einer Stream-Session (chiaki_core::session) mit
//! der **kompletten Media-Pipeline** (StreamView-Agent) und dem
//! Registrierungs-Flow des Wizards.
//!
//! Auflösung der Verbindungsparameter aus den Settings (je ps4/ps5 ×
//! local/remote: `video_profile_local_ps4()` etc.), Keyboard-Mapping aus
//! `keymap/*` (chiaki_input::KeyboardMapper), Session-Events → UiEvents.
//!
//! ## Video-Pfad (implementiert)
//! `SessionCallbacks::video_frame` (rohe H264/H265-Annexb-Samples, 1-Slot-
//! Queue: immer nur der letzte Frame) → Media-Thread: chiaki_media::Decoder
//! (NVDEC/FFmpeg) → NV12 → optional `chiaki_media::VsrUpscaler`
//! (settings/nv_vsr, erzwingt CUDA-Decoder) → NV12Frame →
//! `chiaki_render::presenter::VideoPresenter::set_frame` → StreamView nimmt
//! `take_image()` + `VideoFrameElement`.
//!
//! ## Audio-Pfad (implementiert)
//! `audio_pcm` liefert — wie der C-`ChiakiAudioSink.frame_cb` — **rohe
//! Opus-Frames** (leerer Slice = Concealment); der Header kommt als
//! `SessionEvent::AudioStreamInfo`. Dekodierung über
//! `chiaki_media::opus::OpusAudioDecoder`, Ausgabe über
//! `chiaki_media::AudioOutput` (Volume/Buffer/Device aus den Settings).
//!
//! ## Thread-Architektur (der kritische Teil)
//! chiaki-core feuert die Callbacks auf **drei** Thread-Gruppen (session.rs):
//! * `video_sample`/`audio frame_cb`/`haptics frame_cb` — **Takion-Recv-
//!   Thread** ("Chiaki Takion", plus dessen Entschlüsselungs-Threads),
//! * `event_cb` — **session_thread** ("Chiaki Session") und **Ctrl-Thread**
//!   ("Chiaki Ctrl").
//!
//! `chiaki_media::Decoder` ist `Send`, aber nicht `Sync`; `AudioOutput`/
//! `AudioInput` (cpal) sind nicht einmal `Send` (Erzeugen UND Droppen auf
//! demselben Thread); `VsrUpscaler` ist pro Effekt nicht threadsicher. Die
//! Callbacks dürfen deshalb nichts dekodieren — sie kopieren die Daten nur in
//! Channels (Video: 1-Slot-Queue mit Drop-Oldest) und feuern UiEvents. Ein
//! dedizierter **Media-Thread** ("chiaki-ui-media-N") besitzt Decoder, VSR,
//! AudioOutput, HapticsPlayer und AudioInput exklusiv und räumt sie beim
//! Kanal-Ende (Session-Stop) auf demselben Thread wieder ab.
//!
//! Haptics: `haptics`-Frames → `chiaki_input::HapticsPlayer`
//! (DualSense-Audiodevice, Amplituden via settings/haptic_override skaliert)
//! oder — wenn kein Haptics-Device — Rumble-Fallback gemäß
//! settings/rumble_haptics_intensity (1:1-Port des C++-Zweigs in
//! `PushHapticsFrame`). Rumble/Trigger-Effekte aus Session-Events gehen an
//! den [`SessionManager::set_feedback_sink`].
//!
//! ## Wake vor Session-Start (implementiert)
//! Meldet Discovery den Zielhost im Standby, sendet [`SessionManager::
//! wake_before_session`] vor dem eigentlichen `Session::new` ein Discovery-
//! Wakeup und wartet bis zu 25 s (C++ `WAKEUP_WAIT_SECONDS`) auf Ready —
//! Port des `connectToHost`-Standby-Zweigs + `wakeup_start_timer`
//! (qmlbackend.cpp). Ohne Discovery-Daten wird (wie im C++) direkt gestartet.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use std::collections::VecDeque;

use chiaki_core::audio::AudioHeader;
use chiaki_core::controller::ControllerState;
use chiaki_core::discovery::DiscoveryHostState;
use chiaki_core::error::ChiakiResult;
use chiaki_core::regist::{Regist, RegistEvent, RegistInfo};
use chiaki_core::session::{ConnectInfo, Session, SessionCallbacks, SessionEvent, VideoFrame};
use chiaki_core::takion::DisableAudioVideo as CoreDisableAudioVideo;
use chiaki_input::KeyboardMapper;
use chiaki_media::decoder::HwBackend;
use chiaki_render::presenter::VideoPresenter;
use chiaki_settings::hosts::{self, HostMac, ManualHost, RegisteredHost};
use chiaki_settings::settings::{RumbleHapticsIntensity, Settings};

use super::DiscoveryHandle;
use super::events::{HostId, RegistUiEvent, UiEvent, UiEventSender};

use crate::components::{ToastData, ToastKind};

/// Mutex-Lock mit Poison-Recovery (Library-Pfad ohne Panic).
fn lock<'a, T>(
    guard: Result<MutexGuard<'a, T>, PoisonError<MutexGuard<'a, T>>>,
) -> MutexGuard<'a, T> {
    guard.unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// Feedback-Sink (Rumble/Trigger-Effekte/LED → Controller-Manager)
// ---------------------------------------------------------------------------

/// Controller-Feedback, das aus Session-Events abgeleitet wird
/// (Port der CHIAKI_EVENT_RUMBLE/TRIGGER_EFFECTS/LED_COLOR/PLAYER_INDEX-
/// Handler aus streamsession.cpp — dort: controller->SetRumble(...) etc.).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FeedbackCmd {
    Rumble { left: u8, right: u8 },
    TriggerEffects { type_left: u8, data_left: [u8; 10], type_right: u8, data_right: [u8; 10] },
    HapticIntensity(u8),
    LedColor([u8; 3]),
    PlayerIndex(i32),
}

/// Senke für [`FeedbackCmd`]s (thread-safe — läuft in den Session-Callbacks).
pub type FeedbackSink = Arc<dyn Fn(FeedbackCmd) + Send + Sync>;

// ---------------------------------------------------------------------------
// Telemetrie (HUD-Quellen, Arc-geteilt zwischen Media-Thread und StreamView)
// ---------------------------------------------------------------------------

/// Zähler/Werte für das Stats-HUD. Felder sind Atomics/Mutexe — aus dem
/// UI-Thread lock-frei (bzw. mit kurzem Mutex für Strings) lesbar.
#[derive(Default)]
pub struct StreamTelemetry {
    /// Annexb-Bytes, die die Session geliefert hat (gemessene Bitrate).
    pub video_bytes: AtomicU64,
    /// Gelieferte Video-Frames (Access Units).
    pub video_frames: AtomicU64,
    /// Kumulierte `frames_lost` aus den Video-Samples (Packet-Loss).
    pub video_frames_lost: AtomicU64,
    /// VSR aktiv (nach erfolgreichem `VsrUpscaler::init`).
    pub vsr_active: AtomicBool,
    /// VSR-Skalierung in Prozent (200 = 2x).
    pub vsr_scale: AtomicU32,
    /// Letzter VSR-Fehler (Badge/Log).
    pub vsr_error: Mutex<Option<String>>,
    /// Tatsächlich genutzter Decoder-Backend-Name ("Cuda", "Software", …).
    pub decoder_backend: Mutex<Option<String>>,
    /// AudioOutput-Ring-Füllstand ×10 (ms) — Media-Thread schreibt.
    pub audio_fill_ms_x10: AtomicU32,
    /// Audio-Underflows (Stille wegen leerem Ring).
    pub audio_underflows: AtomicU64,
    /// RTT in ms ×10 — Quelle: FAKE-Telemetrie (fake.rs); der echte Wert
    /// kommt als direkter Poll aus `Session::rtt_us()` (Senkusha-RTT,
    /// StreamUiState::update_stats) und umgeht dieses Feld.
    pub rtt_ms_x10: AtomicU32,
    /// Haptics-Modus ("DualSense-Haptics" | "Rumble-Fallback" | "aus").
    pub haptics_mode: Mutex<String>,
}

impl StreamTelemetry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn decoder_backend_name(&self) -> String {
        self.decoder_backend
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_else(|| "—".into())
    }

    pub fn haptics_mode_name(&self) -> String {
        self.haptics_mode.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

// ---------------------------------------------------------------------------
// Media-Pipeline (intern)
// ---------------------------------------------------------------------------

/// Befehle an den Media-Thread (von den Session-Callbacks geschrieben).
pub(crate) enum MediaCmd {
    /// Rohes Opus-Audioframe (leer = Concealment).
    Audio(Vec<u8>),
    /// Rohes Haptics-Frame (10 ms @ 3 kHz Stereo S16).
    Haptics(Vec<u8>),
    /// `SessionEvent::AudioStreamInfo` — AudioOutput + OpusDecoder aufbauen.
    AudioHeader(AudioHeader),
    /// `SessionEvent::Connected` — Haptics/Mic-Start.
    Connected,
    /// Mic-Mute-Umschaltung (Einblend-Panel).
    MicUnmuted(bool),
}

/// Bounded FIFO für empfangene (kodierte) Frames. H.265/H.264 referenziert
/// zeitlich voraus — jeder Frame MUSS dekodiert werden, sonst reißt das Bild
/// (P-Frames ohne Referenz). Der C++-Client dekodiert in `video_sample_cb`
/// ebenfalls jeden Frame; nur die ANZEIGE behält den neuesten.
/// Kapazität 16 ≈ 266 ms @60fps — der Media-Thread (NVDEC ~2 ms/Frame)
/// dräniert schneller als gefüllt wird.
pub(crate) struct VideoSlot {
    queue: Mutex<VecDeque<VideoSample>>,
    dropped: AtomicU64,
}

const VIDEO_SLOT_CAP: usize = 16;

struct VideoSample {
    data: Vec<u8>,
    frames_lost: i32,
    frame_recovered: bool,
}

impl VideoSlot {
    fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::with_capacity(VIDEO_SLOT_CAP + 1)),
            dropped: AtomicU64::new(0),
        }
    }

    /// Push (Takion-Thread): bei Überlauf ältesten Frame verwerfen (zählt).
    fn push(&self, sample: VideoSample) -> bool {
        let mut queue = lock(self.queue.lock());
        let mut replaced = true;
        while queue.len() >= VIDEO_SLOT_CAP {
            queue.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
            replaced = false;
        }
        queue.push_back(sample);
        replaced
    }

    /// Pop in FIFO-Reihenfolge (Media-Thread) — dekodiert ALLE Frames.
    fn pop(&self) -> Option<VideoSample> {
        lock(self.queue.lock()).pop_front()
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Snapshot der Settings für den Media-Thread (beim Session-Start eingefroren).
pub(crate) struct MediaSettings {
    pub codec: chiaki_core::Codec,
    pub max_fps: u32,
    pub hw_backend: HwBackend,
    pub nv_vsr: bool,
    pub nv_vsr_scale: u32,
    pub nv_vsr_sdk_path: Option<PathBuf>,
    pub audio_out_device: Option<String>,
    /// `settings/audio_volume` 0..=128 (SDL-Skala).
    pub audio_volume: u32,
    /// `settings/audio_buffer_size` (Bytes S16-PCM; 0 = Default).
    pub audio_buffer_size: u32,
    pub audio_in_device: Option<String>,
    pub start_mic_unmuted: bool,
    pub rumble_haptics_intensity: RumbleHapticsIntensity,
    pub haptic_override: f32,
}

impl MediaSettings {
    fn from_settings(req: &ConnectRequest, settings: &Settings) -> Self {
        let profile = video_profile_for(req, settings);
        let codec = match profile.codec {
            chiaki_settings::Codec::H264 => chiaki_core::Codec::H264,
            chiaki_settings::Codec::H265 => chiaki_core::Codec::H265,
            chiaki_settings::Codec::H265Hdr => chiaki_core::Codec::H265Hdr,
        };
        // C++ (streamsession/vsrupscaler): "If VSR is active, the client
        // forces the CUDA decoder" — sonst die hw_decoder-Settings-Auswahl.
        let nv_vsr = settings.nv_vsr_enabled();
        let hw = if nv_vsr {
            HwBackend::Cuda
        } else {
            hw_backend_from_setting(&settings.hw_decoder())
        };
        // Scale-Validation wie VsrUpscaler::init (100..=400): außerhalb →
        // Default 200, statt VSR still zu deaktivieren.
        let scale_raw = settings.nv_vsr_scale().clamp(0, i64::from(u32::MAX)) as u32;
        let nv_vsr_scale = if (100..=400).contains(&scale_raw) { scale_raw } else { 200 };
        let out_dev = settings.audio_out_device();
        let in_dev = settings.audio_in_device();
        MediaSettings {
            codec,
            max_fps: profile.max_fps,
            hw_backend: hw,
            nv_vsr,
            nv_vsr_scale,
            nv_vsr_sdk_path: {
                let p = settings.nv_vsr_sdk_path();
                if p.trim().is_empty() { None } else { Some(PathBuf::from(p)) }
            },
            audio_out_device: if out_dev.trim().is_empty() { None } else { Some(out_dev) },
            audio_volume: settings.audio_volume().clamp(0, 128) as u32,
            audio_buffer_size: settings.audio_buffer_size().min(u32::MAX as u64) as u32,
            audio_in_device: if in_dev.trim().is_empty() { None } else { Some(in_dev) },
            start_mic_unmuted: settings.start_mic_unmuted(),
            rumble_haptics_intensity: settings.rumble_haptics_intensity(),
            haptic_override: settings.haptic_override() as f32,
        }
    }
}

/// `settings/hw_decoder`-String → [`HwBackend`] (Werte wie der C-Client:
/// "auto"|"none"|"cuda"|"d3d11va"|"vulkan"; alles andere → Auto).
pub fn hw_backend_from_setting(value: &str) -> HwBackend {
    match value.trim().to_ascii_lowercase().as_str() {
        "none" => HwBackend::None,
        "cuda" => HwBackend::Cuda,
        "d3d11va" => HwBackend::D3D11Va,
        "vulkan" => HwBackend::Vulkan,
        _ => HwBackend::Auto,
    }
}

// ---------------------------------------------------------------------------
// ConnectRequest / Profil-Auflösung
// ---------------------------------------------------------------------------

/// Verbindungsqualität — wählt die local/remote-Settings-Gruppe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkQuality {
    /// Konsole im lokalen Netz (Discovery-Host).
    Local,
    /// Konsole hinter Internet/Remote-Play.
    Remote,
}

/// Anfrage für `SessionManager::connect`.
#[derive(Clone)]
pub struct ConnectRequest {
    pub host_id: HostId,
    /// IP/Hostname der Konsole (PSN-Pfad: der DUID als Platzhalter).
    pub host: String,
    pub ps5: bool,
    /// `rp_regist_key` (16 Bytes, \0-gefüllt).
    pub regist_key: [u8; 16],
    /// `rp_key` (morning, 16 Bytes).
    pub morning: [u8; 16],
    pub link: LinkQuality,
    /// PSN-Account-ID (8 Bytes) — für PSN-Verbindungen; sonst 0en.
    pub psn_account_id: [u8; 8],
    /// PSN-Remote-Pfad (Holepunch) — `Some` bei PSN-Hosts (C++:
    /// `connect_info.duid` nicht leer → `InitiatePsnConnection`).
    pub psn: Option<PsnRemoteParams>,
}

/// PSN-Remote-Parameter des Verbindungsaufbaus: DUID + die bereits mit dem
/// Settings-Token initialisierte Holepunch-Session (Port von
/// `StreamSessionConnectInfo.duid` + `psn_token`).
#[derive(Clone)]
pub struct PsnRemoteParams {
    /// DUID (Hex der 32 Geräte-Bytes).
    pub duid: String,
    /// Vorbereitete Holepunch-Session (Port-Guessing-Settings sind gesetzt).
    pub holepunch: Arc<chiaki_remote::holepunch::HolepunchSession>,
}

impl std::fmt::Debug for PsnRemoteParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PsnRemoteParams").field("duid", &self.duid).finish()
    }
}

impl std::fmt::Debug for ConnectRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectRequest")
            .field("host_id", &self.host_id)
            .field("host", &self.host)
            .field("ps5", &self.ps5)
            .field("regist_key", &"<16 bytes>")
            .field("morning", &"<16 bytes>")
            .field("link", &self.link)
            .field("psn_account_id", &self.psn_account_id)
            .field("psn", &self.psn)
            .finish()
    }
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
            psn: None,
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
            psn: None,
        }
    }

    /// PSN-Remote-Verbindung (C++ connectToHost mit nicht-leerem DUID:
    /// leerer Host, keine Regist-Key/Morning-Daten — die liefert das
    /// PSN-Regist im session_thread —, Remote-Profil, Account-ID aus den
    /// Settings).
    pub fn from_psn(
        duid: String,
        ps5: bool,
        psn_account_id: [u8; 8],
        holepunch: Arc<chiaki_remote::holepunch::HolepunchSession>,
    ) -> Self {
        Self {
            host_id: HostId::Psn { duid: duid.clone() },
            host: duid.clone(),
            ps5,
            regist_key: [0; 16],
            morning: [0; 16],
            link: LinkQuality::Remote,
            psn_account_id,
            psn: Some(PsnRemoteParams { duid, holepunch }),
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

    // PSN-Pfad (C++ StreamSession-Konstruktor, duid nicht leer): die
    // Holepunch-Session wird als Trait-Objekt in die ConnectInfo gehängt;
    // rudp_sock bleibt — wie im C++ — ungesetzt (session.c holt den
    // Ctrl-Sock selbst über chiaki_get_holepunch_sock(..., CTRL)).
    let (holepunch_session, rudp_sock, host) = match &req.psn {
        Some(psn) => (
            Some(
                Arc::clone(&psn.holepunch)
                    as Arc<dyn chiaki_core::session::HolepunchSession>,
            ),
            None,
            psn.duid.clone(),
        ),
        None => (None, None, req.host.clone()),
    };

    Ok(ConnectInfo {
        ps5: req.ps5,
        host,
        regist_key: req.regist_key,
        morning: req.morning,
        video_profile,
        video_profile_auto_downgrade: true,
        enable_keyboard: settings.keyboard_enabled(),
        // DualSense-Features (Trigger-Effekte/Haptics-Events) an: die Effekte
        // selbst landen im Feedback-Sink bzw. HapticsPlayer.
        enable_dualsense: true,
        audio_video_disabled: map_disable_audio_video(settings.audio_video_disabled()),
        auto_regist: false,
        holepunch_session,
        rudp_sock,
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
    /// HUD-Telemetrie (Media-Thread schreibt, UI liest).
    pub telemetry: Arc<StreamTelemetry>,
    shared: Arc<SessionShared>,
}

pub(crate) struct SessionShared {
    /// `Some` solange die Session läuft (gestoppt = None).
    pub session: Mutex<Option<Session>>,
    pub stop: AtomicBool,
    /// 1-Slot-Queue: neuester Annexb-Frame für den Media-Thread.
    pub video_slot: VideoSlot,
    /// Media-Thread-Kommandos (Audio/Haptics/Header/Connected/Mic).
    pub media_tx: Mutex<Option<Sender<MediaCmd>>>,
    /// Keyboard-Mapping (für den Input-Bridge des StreamViews).
    pub keyboard: Mutex<KeyboardMapper>,
}

impl ActiveSession {
    fn with_session<T>(&self, f: impl FnOnce(&Session) -> T) -> Option<T> {
        let guard = lock(self.shared.session.lock());
        guard.as_ref().map(f)
    }

    /// PIN setzen (LoginPinRequest-Event).
    pub fn set_login_pin(&self, pin: &str) {
        if let Some(Some(err)) =
            self.with_session(|session| session.set_login_pin(pin.as_bytes()).err())
        {
            tracing::error!("set_login_pin fehlgeschlagen: {err}");
        }
    }

    /// Controller-State senden (Session-Feedback-Pfad).
    pub fn send_controller_state(&self, state: &ControllerState) {
        if let Some(Some(err)) =
            self.with_session(|session| session.send_controller_state(state).err())
        {
            tracing::warn!("send_controller_state fehlgeschlagen: {err}");
        }
    }

    /// Keyboard-Mapping der Session (aus den Settings aufgelöst).
    pub fn keyboard_mapper(&self) -> KeyboardMapper {
        self.shared.keyboard.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// C: `chiaki_session_goto_bed()` (Ruhemodus).
    pub fn goto_bed(&self) {
        if let Some(Some(err)) = self.with_session(|session| session.goto_bed().err()) {
            tracing::warn!("goto_bed fehlgeschlagen: {err}");
        }
    }

    /// RTT der Senkusha-Phase in Mikrosekunden (`Session::rtt_us`) — HUD-
    /// Quelle; `None`, solange die Kalibrierung noch keinen Wert geliefert hat.
    pub fn rtt_us(&self) -> Option<u64> {
        self.with_session(|session| session.rtt_us()).flatten()
    }

    /// C: `chiaki_session_go_home()`.
    pub fn go_home(&self) {
        if let Some(Some(err)) = self.with_session(|session| session.go_home().err()) {
            tracing::warn!("go_home fehlgeschlagen: {err}");
        }
    }

    /// C: `chiaki_session_request_idr()`.
    pub fn request_idr(&self) {
        if let Some(Some(err)) = self.with_session(|session| session.request_idr().err()) {
            tracing::warn!("request_idr fehlgeschlagen: {err}");
        }
    }

    /// Mikrofon-Stummschaltung umschalten: Media-Thread baut/verstimmt das
    /// [`AudioInput`], der Ctrl-Kanal verbindet/toggelt (C: `mic_connected`-
    /// Pflege in `ToggleMute`).
    pub fn set_mic_unmuted(&self, unmuted: bool) {
        if let Some(tx) = lock(self.shared.media_tx.lock()).as_ref() {
            let _ = tx.send(MediaCmd::MicUnmuted(unmuted));
        }
        if unmuted {
            if let Some(Some(err)) =
                self.with_session(|session| session.connect_microphone().err())
            {
                tracing::warn!("connect_microphone fehlgeschlagen: {err}");
            }
        }
        if let Some(Some(err)) =
            self.with_session(|session| session.toggle_microphone(!unmuted).err())
        {
            tracing::warn!("toggle_microphone fehlgeschlagen: {err}");
        }
    }

    /// Keyboard-Text senden (Overlay: Senden → set_text + accept).
    pub fn keyboard_submit(&self, text: &str) -> bool {
        let r = self.with_session(|session| {
            session.keyboard_set_text(text).and_then(|_| session.keyboard_accept())
        });
        match r {
            Some(Ok(())) => true,
            Some(Err(err)) => {
                tracing::error!("keyboard_submit fehlgeschlagen: {err}");
                false
            }
            None => false,
        }
    }

    /// Keyboard-Overlay abbrechen (C: `chiaki_session_keyboard_reject`).
    pub fn keyboard_reject(&self) {
        if let Some(Some(err)) = self.with_session(|session| session.keyboard_reject().err()) {
            tracing::warn!("keyboard_reject fehlgeschlagen: {err}");
        }
    }

    /// Mic-Verbindung aus dem Media-Thread (C++-ToggleMute-Pfad).
    pub(crate) fn connect_microphone_internal(&self) -> ChiakiResult<()> {
        match self.with_session(|session| session.connect_microphone()) {
            Some(r) => r,
            None => Err(chiaki_core::ChiakiError::Uninitialized),
        }
    }

    /// Opus-Mic-Frames an die Session (Media-Thread-Callback).
    pub(crate) fn send_mic_data(&self, packet: &[u8]) -> ChiakiResult<()> {
        match self.with_session(|session| session.send_mic_data(packet)) {
            Some(r) => r,
            None => Err(chiaki_core::ChiakiError::Uninitialized),
        }
    }

    /// Video-Slot (nur für den Media-Thread; pub(crate) für mod media).
    pub(crate) fn shared_video_slot(&self) -> &VideoSlot {
        &self.shared.video_slot
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
    /// Controller-Feedback-Sink (Rumble/Effekte) — die StreamView setzt ihn
    /// mit Closures über den ControllerHandle.
    feedback: Arc<Mutex<Option<FeedbackSink>>>,
    /// Discovery-Handle für den Wake vor Session-Start
    /// ([`SessionManager::wake_before_session`]).
    discovery: DiscoveryHandle,
    /// Holepunch-Session der laufenden PSN-Connect-Phase (vor `ActiveSession`
    /// existiert) — für den Abbruch (C++ `psnCancel` →
    /// `chiaki_holepunch_main_thread_cancel`).
    pending_holepunch: Arc<Mutex<Option<Arc<chiaki_remote::holepunch::HolepunchSession>>>>,
}

impl SessionManager {
    pub fn new(
        settings: Arc<Mutex<Settings>>,
        events: UiEventSender,
        discovery: DiscoveryHandle,
    ) -> Self {
        Self {
            settings,
            events,
            current: Arc::new(Mutex::new(None)),
            next_id: Arc::new(AtomicU64::new(1)),
            feedback: Arc::new(Mutex::new(None)),
            discovery,
            pending_holepunch: Arc::new(Mutex::new(None)),
        }
    }

    /// Feedback-Sink setzen (StreamView nach Backend-Start; `None` = nur Log).
    pub fn set_feedback_sink(&self, sink: Option<FeedbackSink>) {
        *lock(self.feedback.lock()) = sink;
    }

    fn feedback_sink(&self) -> Option<FeedbackSink> {
        lock(self.feedback.lock()).clone()
    }

    /// Handle der aktiven Session (falls vorhanden).
    pub fn active(&self) -> Option<ActiveSession> {
        lock(self.current.lock()).clone()
    }

    /// Startet eine Session **auf einem eigenen Thread** (non-blocking für die
    /// UI — `Session::new` löst den Hostnamen blockierend auf). Das Handle
    /// erscheint bei Erfolg in `active()`, Fehler kommen als Quit-Event.
    pub fn start_session(&self, request: ConnectRequest) {
        let this = self.clone();
        let spawn = std::thread::Builder::new()
            .name("chiaki-ui-connect".into())
            .spawn(move || {
                if let Err(err) = this.connect(request.clone()) {
                    tracing::error!("Session-Start fehlgeschlagen: {err}");
                    this.events.send(UiEvent::Session {
                        session_id: 0,
                        event: SessionEvent::Quit {
                            reason: chiaki_core::session::QuitReason::SessionRequestConnectionRefused,
                            reason_str: format!("Verbindung fehlgeschlagen: {err}"),
                        },
                    });
                }
            });
        if let Err(err) = spawn {
            tracing::error!("Connect-Thread konnte nicht gestartet werden: {err}");
        }
    }

    /// Startet eine Session (blocking — von `start_session` in einem Thread
    /// gerufen): Session-Threads + Media-Thread laufen im Hintergrund; Events
    /// kommen über den UiEvent-Stream.
    pub fn connect(&self, request: ConnectRequest) -> ChiakiResult<ActiveSession> {
        self.stop_current();

        // Wake vor Session-Start (C++ connectToHost-Standby-Zweig): meldet
        // Discovery den Host im Standby, wird er aufgeweckt und auf Ready
        // gewartet, bevor die Session-Anfrage losläuft.
        self.wake_before_session(&request);

        let session_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let settings = lock(self.settings.lock());

        // PSN-Pfad (C++ PsnConnectionWorker::ConnectPsnConnection im
        // psn_connection_thread): Holepunch-Sequenz VOR dem Session-Start —
        // der session_thread braucht den gepunchten Control-Hole für das
        // RUDP-Regist. Fortschritt als UiEvent::Psn an die UI.
        if let Some(psn) = &request.psn {
            self.psn_connect_phase(&psn.holepunch, &psn.duid, request.ps5)?;
        }

        let connect_info = build_connect_info(&request, &settings)?;
        let keyboard = keyboard_mapper_from_settings(&settings);
        let media = MediaSettings::from_settings(&request, &settings);
        drop(settings);

        tracing::info!(
            "Starte Session #{session_id} zu {} (ps5={}, link={:?}, {}x{}@{} {}, hw={:?}, vsr={} {}%)",
            request.host,
            request.ps5,
            request.link,
            connect_info.video_profile.width,
            connect_info.video_profile.height,
            connect_info.video_profile.max_fps,
            if connect_info.video_profile.codec.is_h265() { "H265" } else { "H264" },
            media.hw_backend,
            media.nv_vsr,
            media.nv_vsr_scale,
        );

        // Presenter mit Stream-Auflösung (VSR-Skalierung macht der Decode-Pfad).
        let presenter =
            VideoPresenter::new(connect_info.video_profile.width, connect_info.video_profile.height);
        let telemetry = StreamTelemetry::new();
        telemetry.vsr_scale.store(media.nv_vsr_scale, Ordering::Relaxed);
        let (media_tx, media_rx) = std::sync::mpsc::channel::<MediaCmd>();

        let shared = Arc::new(SessionShared {
            session: Mutex::new(None),
            stop: AtomicBool::new(false),
            video_slot: VideoSlot::new(),
            media_tx: Mutex::new(Some(media_tx)),
            keyboard: Mutex::new(keyboard),
        });

        let callbacks = Arc::new(BridgeCallbacks {
            session_id,
            events: self.events.clone(),
            shared: Arc::clone(&shared),
            telemetry: Arc::clone(&telemetry),
            feedback: self.feedback_sink(),
        });

        let mut session = Session::new(connect_info, callbacks)?;
        session.start()?;

        *lock(shared.session.lock()) = Some(session);

        // Media-Thread (besitzt Decoder/VSR/Audio/Haptics/Mic — siehe
        // Modul-Doku). Er endet, wenn der Cmd-Kanal geschlossen wird.
        let media_session = ActiveSession {
            id: session_id,
            presenter: presenter.clone(),
            telemetry: Arc::clone(&telemetry),
            shared: Arc::clone(&shared),
        };
        let feedback = self.feedback_sink();
        let join_media = std::thread::Builder::new()
            .name(format!("chiaki-ui-media-{session_id}"))
            .spawn({
                let session = media_session.clone();
                move || media::run(media, media_rx, session, feedback)
            })
            .ok(); // Bei Spawn-Fehler bleibt der Kanal offen — Session läuft weiter.
        std::mem::forget(join_media);

        // Join/Stop-Thread: wartet aufs Stop-Flag, stoppt dann die Session
        // und räumt auf (stop/join/fini brauchen &mut — daher take()).
        let shared_for_thread = Arc::clone(&shared);
        let events = self.events.clone();
        let telemetry_for_thread = Arc::clone(&telemetry);
        let join = std::thread::Builder::new()
            .name(format!("chiaki-ui-session-{session_id}"))
            .spawn(move || {
                while !shared_for_thread.stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(50));
                }
                let mut session = lock(shared_for_thread.session.lock()).take();
                if let Some(session) = session.as_mut() {
                    session.stop();
                    let _ = session.join();
                    session.fini();
                }
                // Media-Kanal schließen → Media-Thread räumt Audio/Haptics ab.
                *lock(shared_for_thread.media_tx.lock()) = None;
                *lock(telemetry_for_thread.haptics_mode.lock()) = "aus".into();
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

        *lock(self.current.lock()) = Some(media_session.clone());
        Ok(media_session)
    }

    /// Stoppt die aktuelle Session (falls vorhanden). Läuft gerade eine
    /// PSN-Holepunch-Connect-Phase (noch keine ActiveSession), wird diese —
    /// wie im C++ `psnCancel` — per `main_thread_cancel` abgebrochen.
    pub fn stop_current(&self) {
        if let Some(holepunch) = lock(self.pending_holepunch.lock()).take() {
            // C++ psnCancel(true): Cancel-Flags + WebSocket-Thread stoppen.
            holepunch.main_thread_cancel(true);
        }
        if let Some(active) = lock(self.current.lock()).take() {
            active.request_stop();
        }
    }

    /// Wake vor Session-Start — Port des C++-Flows (`connectToHost`-
    /// Standby-Zweig + `wakeup_start_timer`, qmlbackend.cpp): Meldet der
    /// Discovery-Service den Zielhost im **Standby**, wird ein Discovery-
    /// Wakeup gesendet und — wie im C++, das die Session erst startet, wenn
    /// `updateDiscoveryHosts` READY meldet — bis [`WAKEUP_WAIT_SECONDS`]
    /// auf Ready gewartet. Läuft die Konsole nicht mehr auf (Timeout),
    /// wird trotzdem verbunden (C++ bricht den Flow über `wakeupStartFailed`
    /// ab; unsere Connecting-Sequenz fährt — wie in stream/state.rs
    /// dokumentiert — „we'll try anyway" fort).
    ///
    /// Kein Discovery-Daten Host/Unknown-State → kein Wake, sofort starten
    /// (genau wie im C++, das nur `discovered && state == STANDBY` weckt).
    /// Läuft auf dem Connect-Thread (non-blocking für die UI); Abbruch während
    /// der Wartephase ist wie der blockierende DNS-Teil von `Session::new`
    /// nicht vorgesehen — das Stop-Flag existiert erst nach dem `Session::new`.
    fn wake_before_session(&self, request: &ConnectRequest) {
        const WAKEUP_WAIT: Duration = Duration::from_secs(WAKEUP_WAIT_SECONDS);
        // Nur registrierte Hosts tragen einen gültigen Regist-Key — ohne ihn
        // gibt es kein Wakeup-Credential (wake() würde InvalidData liefern).
        if request.regist_key == [0; 16] {
            return;
        }
        let state = self
            .discovery
            .hosts()
            .into_iter()
            .find(|h| h.host_addr == request.host)
            .map(|h| h.state);
        if state != Some(DiscoveryHostState::Standby) {
            return;
        }
        tracing::info!(
            "Konsole {} ist im Standby — Wakeup vor Session-Start (Wartezeit bis {} s)",
            request.host,
            WAKEUP_WAIT.as_secs(),
        );
        if let Err(err) = self.discovery.wake(&request.host, &request.regist_key, request.ps5) {
            tracing::warn!("Wakeup vor Session-Start fehlgeschlagen: {err} — versuche trotzdem");
            return;
        }
        let started = std::time::Instant::now();
        loop {
            let state = self
                .discovery
                .hosts()
                .into_iter()
                .find(|h| h.host_addr == request.host)
                .map(|h| h.state);
            match state {
                Some(DiscoveryHostState::Ready) => {
                    tracing::info!(
                        "Konsole {} ist bereit (nach {} ms) — Session-Start",
                        request.host,
                        started.elapsed().as_millis(),
                    );
                    return;
                }
                // Host aus der Discovery-Liste gefallen (weckt gerade auf) →
                // weiter warten, das Ready-Paket kommt nach dem Wakeup.
                _ => {}
            }
            if started.elapsed() >= WAKEUP_WAIT {
                tracing::info!(
                    "Wakeup bestätigt sich nicht — Verbindung zu {} wird trotzdem versucht",
                    request.host,
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// Port von `StreamSession::ConnectPsnConnection` (streamsession.cpp,
    /// läuft dort im PsnConnectionWorker-Thread; hier im Connect-Thread):
    ///
    /// 1. DUID-Hex → 32 Bytes (C++ `parse_hex`, Größenfehler → InvalidData)
    /// 2. `chiaki_holepunch_upnp_discover`
    /// 3. `chiaki_holepunch_session_create`        — "Created session"
    /// 4. `chiaki_holepunch_session_create_offer`  — "Created offer msg for
    ///    ctrl connection"
    /// 5. `chiaki_holepunch_session_start(duid, console_type)` — "Started
    ///    session"
    /// 6. `chiaki_holepunch_session_punch_hole(CTRL)` — "Punched hole for
    ///    control connection!"
    ///
    /// Danach startet der Aufrufer — wie das C++ `checkPsnConnection(SUCCESS)
    /// → psnSessionStart` — die eigentliche Chiaki-Session, deren
    /// session_thread das PSN-Regist über den gepunchten Control-Hole
    /// ausführt. Fortschrittsstufen fließen als `UiEvent::Psn` an die UI.
    fn psn_connect_phase(
        &self,
        holepunch: &Arc<chiaki_remote::holepunch::HolepunchSession>,
        duid: &str,
        ps5: bool,
    ) -> ChiakiResult<()> {
        use chiaki_remote::holepunch::{ConsoleType, PortType};

        self.events.send(UiEvent::Psn(super::psn::PsnUiEvent::Connecting(
            super::psn::PsnConnectState::InitiatingConnection,
        )));

        // C++: parse_hex(duid) — "Couldn't convert duid string to bytes got
        // size mismatch" → CHIAKI_ERR_INVALID_DATA.
        let duid_bytes = chiaki_remote::regist_psn::parse_duid(duid)?;
        let console_type = if ps5 { ConsoleType::Ps5 } else { ConsoleType::Ps4 };
        tracing::info!("PSN: DUID: {duid}");

        // Für den Abbruch (Stream-Abbrechen → stop_current) registrieren.
        *lock(self.pending_holepunch.lock()) = Some(Arc::clone(holepunch));

        let result = (|| -> ChiakiResult<()> {
            holepunch.upnp_discover().inspect_err(|_| {
                tracing::error!("!! Failed to run upnp discover");
            })?;
            holepunch.create().inspect_err(|_| {
                tracing::error!("!! Failed to create session");
            })?;
            tracing::info!(">> Created session");
            // C: chiaki_holepunch_session_create_offer() — ohne Port-Typ (das
            // Angebot gilt für den nächsten Hole, gesteuert über den Session-State).
            holepunch.create_offer().inspect_err(|_| {
                tracing::error!("!! Failed to create offer msg for ctrl connection");
            })?;
            tracing::info!(">> Created offer msg for ctrl connection");
            holepunch.start(&duid_bytes, console_type).inspect_err(|_| {
                tracing::error!("!! Failed to start session");
            })?;
            tracing::info!(">> Started session");
            holepunch.punch_hole(PortType::Ctrl).inspect_err(|_| {
                tracing::error!("!! Failed to punch hole for control connection.");
            })?;
            Ok(())
        })();

        // Nach der Phase (erfolgreich oder abgebrochen) kein Cancel-Pfad mehr.
        *lock(self.pending_holepunch.lock()) = None;
        result?;
        tracing::info!(">> Punched hole for control connection!");

        // C++ checkPsnConnection(SUCCESS) → PsnConnectState::LinkingConsole.
        self.events.send(UiEvent::Psn(super::psn::PsnUiEvent::Connecting(
            super::psn::PsnConnectState::LinkingConsole,
        )));
        Ok(())
    }
}

/// C++ `WAKEUP_WAIT_SECONDS` (qmlbackend.cpp): so lange wartet der Client
/// nach dem Wakeup auf den Ready-Zustand, bevor die Session startet.
const WAKEUP_WAIT_SECONDS: u64 = 25;

// ---------------------------------------------------------------------------
// Registrierungs-Flow (Registrierungs-Wizard) — Port von QmlBackend::registerHost
// ---------------------------------------------------------------------------

/// Request für [`SessionManager::regist_host`] — die Parameter des
/// Registrierungs-Wizards (qml2/RegistWizard.qml → Chiaki.registerHost).
#[derive(Debug, Clone)]
pub struct RegistRequest {
    /// Konsole-IP oder `255.255.255.255` für Broadcast im LAN.
    pub host: String,
    /// PS4 < 7.0 → `Ps4Eight` (800), 7.0–8.0 → `Ps4Nine` (900),
    /// ≥ 8.0 → `Ps4Ten` (1000), PS5 → `Ps5One` (1000100) — Werte wie im C++.
    pub target: hosts::Target,
    /// `true` → UDP-Broadcast statt Unicast (bei 255.255.255.255).
    pub broadcast: bool,
    /// PSN Online-ID — nur für PS4 < 7.0 (Target 800).
    pub psn_online_id: Option<String>,
    /// PSN Account-ID (8 Bytes, Base64-dekodiert) — PS4 ≥ 7.0 / PS5.
    pub psn_account_id: Option<[u8; 8]>,
    /// 8-stelliger Remote-Play-PIN der Konsole.
    pub pin: u32,
    /// Optionaler 4-stelliger Konsolen-Login-PIN (für künftige Streams).
    pub console_pin: u32,
}

/// Live-Zustand des Registrierungs-Flows (Wizard liest ihn 1×/Frame).
#[derive(Debug, Clone, Default)]
pub struct RegistState {
    /// Bisherige Log-Zeilen (in Zustellungsreihenfolge).
    pub lines: Vec<String>,
    /// Läuft der Regist-Thread noch?
    pub running: bool,
    /// Ergebnis: `Ok(Konsolen-Nickname)` oder `Err(Grund)`.
    pub result: Option<Result<String, String>>,
}

/// Handle auf den Flow-Zustand (vom Wizard gespeichert).
#[derive(Clone)]
pub struct RegistHandle {
    pub state: Arc<Mutex<RegistState>>,
}

impl RegistHandle {
    fn push_line(&self, events: &UiEventSender, line: String) {
        lock(self.state.lock()).lines.push(line.clone());
        events.send(UiEvent::Regist(RegistUiEvent::Log(line)));
    }

    fn finish(&self, events: &UiEventSender, result: Result<String, String>) {
        let mut st = lock(self.state.lock());
        st.running = false;
        st.result = Some(result.clone());
        drop(st);
        events.send(UiEvent::Regist(RegistUiEvent::Finished(result)));
    }

    /// Snapshot für den Render-Pfad.
    pub fn snapshot(&self) -> RegistState {
        lock(self.state.lock()).clone()
    }
}

/// chiaki_settings-Target → chiaki_core-Target (gleiche Diskriminanten).
fn core_target(target: hosts::Target) -> chiaki_core::error::Target {
    match target {
        hosts::Target::Ps4Unknown => chiaki_core::error::Target::Ps4Unknown,
        hosts::Target::Ps4Eight => chiaki_core::error::Target::Ps4_8,
        hosts::Target::Ps4Nine => chiaki_core::error::Target::Ps4_9,
        hosts::Target::Ps4Ten => chiaki_core::error::Target::Ps4_10,
        hosts::Target::Ps5Unknown => chiaki_core::error::Target::Ps5Unknown,
        hosts::Target::Ps5One => chiaki_core::error::Target::Ps5_1,
    }
}

impl SessionManager {
    /// Port von `QmlBackend::registerHost` (qmlbackend.cpp): baut die
    /// `ChiakiRegistInfo` (Target-Presets, Online-ID vs. Account-ID, PIN,
    /// Konsolen-PIN) und startet `chiaki_core::regist::Regist` in einem
    /// eigenen Thread. Log-Zeilen und Ergebnis fließen ins zurückgegebene
    /// [`RegistHandle`] (Live-Log des Wizards) und als `UiEvent::Regist`
    /// in die UI (Notify). Bei Erfolg wird der Host — wie im C++
    /// (`QmlRegist::success`) — in die Settings-Registry übernommen;
    /// bei Unicast-Zielen zusätzlich ein zugeordneter manueller Host
    /// angelegt (`regist_dialog_server.discovered == false`-Pfad).
    pub fn regist_host(&self, req: RegistRequest) -> RegistHandle {
        let handle = RegistHandle {
            state: Arc::new(Mutex::new(RegistState { running: true, ..Default::default() })),
        };
        let ret = handle.clone();
        let settings = Arc::clone(&self.settings);
        let events = self.events.clone();

        let spawn = std::thread::Builder::new()
            .name("chiaki-ui-regist".into())
            .spawn(move || {
                handle.push_line(
                    &events,
                    format!(
                        "Registrierung bei {} gestartet (Target {}, Broadcast: {})",
                        req.host,
                        req.target.as_i32(),
                        req.broadcast
                    ),
                );

                let info = RegistInfo {
                    target: core_target(req.target),
                    host: req.host.clone(),
                    broadcast: req.broadcast,
                    // PS4 < 7.0: Online-ID; sonst Account-ID (regist.rs
                    // fällt auf `psn_account_id` zurück, wenn `None`).
                    psn_online_id: req.psn_online_id.clone(),
                    psn_account_id: req.psn_account_id.unwrap_or([0; 8]),
                    pin: req.pin,
                    console_pin: req.console_pin,
                };

                let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
                let cb_handle = handle.clone();
                let cb_events = events.clone();
                let cb: chiaki_core::regist::RegistCb = Arc::new(move |ev: RegistEvent| {
                    match ev {
                        RegistEvent::FinishedCanceled => {
                            cb_handle.finish(&cb_events, Err("Registrierung abgebrochen".into()));
                        }
                        RegistEvent::FinishedFailed => {
                            cb_handle.finish(
                                &cb_events,
                                Err(
                                    "Registrierung fehlgeschlagen — PIN prüfen (Details im Log)"
                                        .into(),
                                ),
                            );
                        }
                        RegistEvent::FinishedSuccess(host) => {
                            let nickname = if host.server_nickname.is_empty() {
                                req.host.clone()
                            } else {
                                host.server_nickname.clone()
                            };
                            // core-RegisteredHost → settings-RegisteredHost
                            // (Feld-für-Feld, wie SaveToSettings es erwartet).
                            let mut registered = RegisteredHost::default();
                            registered.target = req.target;
                            registered.ap_ssid = host.ap_ssid.clone();
                            registered.ap_bssid = host.ap_bssid.clone();
                            registered.ap_key = host.ap_key.clone();
                            registered.ap_name = host.ap_name.clone();
                            registered.server_mac = HostMac::new(host.server_mac);
                            registered.server_nickname = host.server_nickname.clone();
                            registered.rp_regist_key = host.rp_regist_key;
                            registered.rp_key_type = host.rp_key_type;
                            registered.rp_key = host.rp_key;
                            registered.console_pin = if host.console_pin > 0 {
                                host.console_pin.to_string()
                            } else {
                                String::new()
                            };

                            let save = settings
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .update(|s| {
                                    s.add_registered_host(registered.clone());
                                    // C++: nicht entdeckte (Unicast-)Ziele
                                    // bekommen einen verknüpften ManualHost.
                                    if !req.broadcast {
                                        let mac = registered.server_mac;
                                        s.set_manual_host(ManualHost::new(
                                            -1,
                                            req.host.clone(),
                                            true,
                                            mac,
                                        ));
                                    }
                                });
                            match save {
                                Ok(()) => {
                                    cb_handle.push_line(
                                        &cb_events,
                                        format!("Konsole registriert: {nickname}"),
                                    );
                                    // Backend-getriebener Toast (sichtbar, auch
                                    // wenn der User den Wizard verlassen hat).
                                    cb_events.send(UiEvent::Toast(
                                        ToastData::new(ToastKind::Success, "Konsole registriert")
                                            .message(nickname.clone()),
                                    ));
                                    cb_handle.finish(&cb_events, Ok(nickname));
                                }
                                Err(err) => {
                                    cb_handle.finish(
                                        &cb_events,
                                        Err(format!("Konnte Settings nicht speichern: {err}")),
                                    );
                                }
                            }
                        }
                    }
                    // Regist-Thread ist nach genau einem Finished*-Event fertig.
                    let _ = done_tx.send(());
                });

                match Regist::start(info, cb) {
                    Ok(regist) => {
                        // Auf das Finished*-Event warten, dann joinen
                        // (Regist::fini braucht den Besitz).
                        let _ = done_rx.recv();
                        regist.fini();
                    }
                    Err(err) => {
                        handle.finish(
                            &events,
                            Err(format!("Registrierung konnte nicht gestartet werden: {err}")),
                        );
                    }
                }
            });
        // Thread detached; Fehler (nur bei OS-Ressourcenknappheit) loggen.
        if let Err(err) = spawn {
            tracing::error!("Regist-Thread konnte nicht gestartet werden: {err}");
        }
        ret
    }
}

// ---------------------------------------------------------------------------
// Callbacks-Bridge (Session-Threads → Channels / UiEvents / Feedback-Sink)
// ---------------------------------------------------------------------------

/// Die Session-Callbacks laufen auf den chiaki-core-Threads (Takion-Recv für
/// AV/Haptics, Session-/Ctrl-Thread für Events) — sie kopieren nur.
struct BridgeCallbacks {
    session_id: u64,
    events: UiEventSender,
    shared: Arc<SessionShared>,
    telemetry: Arc<StreamTelemetry>,
    feedback: Option<FeedbackSink>,
}

impl BridgeCallbacks {
    fn send_media(&self, cmd: MediaCmd) -> Result<(), ()> {
        let guard = lock(self.shared.media_tx.lock());
        match guard.as_ref() {
            Some(tx) => tx.send(cmd).map_err(|_| ()),
            None => Err(()),
        }
    }

    fn send_feedback(&self, cmd: FeedbackCmd) {
        if let Some(feedback) = &self.feedback {
            feedback(cmd);
        }
    }
}

impl SessionCallbacks for BridgeCallbacks {
    fn video_frame(&self, frame: VideoFrame<'_>) -> bool {
        self.telemetry.video_bytes.fetch_add(frame.data.len() as u64, Ordering::Relaxed);
        self.telemetry.video_frames.fetch_add(1, Ordering::Relaxed);
        if frame.frames_lost > 0 {
            self.telemetry
                .video_frames_lost
                .fetch_add(frame.frames_lost as u64, Ordering::Relaxed);
        }
        // 1-Slot-Queue: immer nur der letzte Frame (Drop-Oldest).
        self.shared.video_slot.push(VideoSample {
            data: frame.data.to_vec(),
            frames_lost: frame.frames_lost,
            frame_recovered: frame.frame_recovered,
        });
        true
    }

    fn audio_pcm(&self, pcm: &[u8]) {
        // Rohe Opus-Frames (leer = Concealment) — Dekodierung im Media-Thread.
        let _ = self.send_media(MediaCmd::Audio(pcm.to_vec()));
    }

    fn event(&self, ev: SessionEvent) {
        // Media-/Feedback-relevante Events zusätzlich weiterleiten.
        match &ev {
            SessionEvent::AudioStreamInfo(header) => {
                let _ = self.send_media(MediaCmd::AudioHeader(*header));
            }
            SessionEvent::Connected => {
                let _ = self.send_media(MediaCmd::Connected);
            }
            SessionEvent::Rumble { left, right, .. } => {
                self.send_feedback(FeedbackCmd::Rumble { left: *left, right: *right });
            }
            SessionEvent::TriggerEffects { type_left, left, type_right, right } => {
                self.send_feedback(FeedbackCmd::TriggerEffects {
                    type_left: *type_left,
                    data_left: *left,
                    type_right: *type_right,
                    data_right: *right,
                });
            }
            SessionEvent::HapticIntensity(intensity) => {
                // Nibble-Werte aus dem Pad-Info-Paket (Off=0, Strong=1,
                // Medium=2, Weak=3) direkt ans DualSense; Off deaktiviert
                // (C++: ps5_rumble_intensity = -1 → keine Rumble mehr).
                if *intensity != chiaki_core::streamconnection::DualSenseEffectIntensity::Off {
                    self.send_feedback(FeedbackCmd::HapticIntensity(*intensity as u8));
                }
            }
            SessionEvent::LedColor(color) => self.send_feedback(FeedbackCmd::LedColor(*color)),
            SessionEvent::PlayerIndex(index) => {
                self.send_feedback(FeedbackCmd::PlayerIndex(i32::from(*index)))
            }
            _ => {}
        }
        self.events.send(UiEvent::Session { session_id: self.session_id, event: ev });
    }

    fn haptics(&self, data: &[u8]) {
        let _ = self.send_media(MediaCmd::Haptics(data.to_vec()));
    }
}

// ---------------------------------------------------------------------------
// Media-Thread (Video-Decode/VSR/Audio/Haptics/Mic)
// ---------------------------------------------------------------------------

pub(crate) mod media {

    //! Besitzt alle nicht-threadsicheren Media-Ressourcen (siehe Modul-Doku
    //! des übergeordneten Moduls). Endet, wenn der `MediaCmd`-Sender
    //! gedroppt wird (Session-Stop) und räumt AudioOutput/AudioInput/
    //! HapticsPlayer **auf diesem Thread** ab (!Send-Verträge von cpal).

/// Laufende Timing-Sammlung des Media-Threads (Ø über 300-Frame-Fenster).
#[derive(Default)]
struct MediaTimings {
    samples: u64,
    frames: u64,
    decode_us: u64,
    nv12_copy_us: u64,
    vsr_us: u64,
    out_copy_us: u64,
}


    use super::{haptics_rumble_fallback, ActiveSession, FeedbackCmd, FeedbackSink, MediaCmd, MediaSettings};
    use chiaki_core::ChiakiResult;
    use chiaki_input::HapticsPlayer;
    use chiaki_media::decoder::DecodedFrame;
    use chiaki_media::opus::OpusAudioDecoder;
    use chiaki_media::vsr::FrameBuf;
    use chiaki_media::{AudioInput, AudioOutput, Decoder, VsrUpscaler};
    use chiaki_render::nv12::NV12Frame;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc::Receiver;
    use std::time::Duration;

    pub(super) fn run(
        settings: MediaSettings,
        rx: Receiver<MediaCmd>,
        session: ActiveSession,
        feedback: Option<FeedbackSink>,
    ) {
        // --- Decoder (Video) ---
        let mut decoder = match Decoder::new(settings.codec, settings.hw_backend, settings.max_fps)
        {
            Ok(d) => {
                let name = match d.used_hw_backend() {
                    Some(b) => format!("{b:?}"),
                    None => "Software".into(),
                };
                tracing::info!("Media-Thread: Decoder bereit ({name})");
                *session
                    .telemetry
                    .decoder_backend
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(name);
                Some(d)
            }
            Err(err) => {
                tracing::error!("Decoder-Init fehlgeschlagen ({err:?}) — Video deaktiviert");
                *session
                    .telemetry
                    .decoder_backend
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some("kein Decoder".into());
                None
            }
        };

        // --- VSR (init passiert beim ersten Frame, C++: init(firstFrame)) ---
        let mut vsr = if settings.nv_vsr {
            Some(VsrUpscaler::new(settings.nv_vsr_sdk_path.clone()))
        } else {
            None
        };
        let mut vsr_inited = false;
        let mut vsr_buf = FrameBuf::new();
        let mut t = MediaTimings::default();

        // --- Audio (OpusDecoder + Output entstehen mit dem AudioHeader) ---
        let mut opus = OpusAudioDecoder::new();
        let mut audio_out: Option<AudioOutput> = None;

        // --- Haptics/Mic ---
        let mut haptics: Option<HapticsPlayer> = None;
        let mut mic: Option<AudioInput> = None;

        loop {
            match rx.recv_timeout(Duration::from_millis(5)) {
                Ok(MediaCmd::AudioHeader(header)) => {
                    if let Err(err) = opus.set_header(header) {
                        tracing::error!("OpusDecoder-Init fehlgeschlagen: {err:?}");
                        continue;
                    }
                    match AudioOutput::new(
                        settings.audio_out_device.as_deref(),
                        u32::from(header.rate),
                        u16::from(header.channels),
                        settings.audio_buffer_size,
                    ) {
                        Ok(out) => {
                            out.set_volume(settings.audio_volume as f32 / 128.0);
                            tracing::info!(
                                "Audio-Ausgabe '{}' (Puffer-Ziel {} Bytes, Volume {}/128)",
                                out.device_name(),
                                settings.audio_buffer_size,
                                settings.audio_volume
                            );
                            audio_out = Some(out);
                        }
                        Err(err) => tracing::error!("AudioOutput-Init fehlgeschlagen: {err:?}"),
                    }
                }
                Ok(MediaCmd::Audio(packet)) => {
                    // Dekodieren (leeres Paket = Concealment), dann pushen.
                    if let Ok(pcm) = opus.decode_frame(&packet) {
                        if let Some(out) = &audio_out {
                            out.push(pcm);
                            session
                                .telemetry
                                .audio_fill_ms_x10
                                .store((out.current_buffer_fill_ms() * 10.0) as u32, Ordering::Relaxed);
                            session
                                .telemetry
                                .audio_underflows
                                .store(out.underflows(), Ordering::Relaxed);
                        }
                    }
                }
                Ok(MediaCmd::Connected) => {
                    // Haptics-Ausgabe (C++: InitHaptics/ConnectHaptics).
                    if haptics.is_none() {
                        match HapticsPlayer::open() {
                            Ok(player) => {
                                tracing::info!("Haptics-Ausgabe: {}", player.device_name());
                                *session
                                    .telemetry
                                    .haptics_mode
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner()) =
                                    "DualSense-Haptics".into();
                                haptics = Some(player);
                            }
                            Err(_) => {
                                // Rumble-Fallback (settings rumble_haptics_intensity).
                                let fallback = settings.rumble_haptics_intensity
                                    != chiaki_settings::settings::RumbleHapticsIntensity::Off;
                                *session
                                    .telemetry
                                    .haptics_mode
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner()) =
                                    if fallback { "Rumble-Fallback" } else { "aus" }.into();
                            }
                        }
                    }
                    // Mic-Start wie im C++: start_mic_unmuted → verbinden.
                    if settings.start_mic_unmuted {
                        start_mic(&settings, &session, &mut mic, false);
                    }
                }
                Ok(MediaCmd::MicUnmuted(unmuted)) => {
                    if unmuted {
                        start_mic(&settings, &session, &mut mic, true);
                    } else if let Some(input) = &mic {
                        input.set_muted(true);
                    }
                }
                Ok(MediaCmd::Haptics(data)) => {
                    if let Some(player) = &haptics {
                        // C++: haptic_override skaliert die Amplituden des
                        // Audio-Haptics-Pfads (außerhalb 0.99..1.01).
                        let scaled = if settings.haptic_override > 0.99
                            && settings.haptic_override < 1.01
                        {
                            data
                        } else {
                            scale_haptics(&data, settings.haptic_override)
                        };
                        if let Err(err) = player.push_haptics_frame(&scaled) {
                            tracing::warn!("Haptics-Frame verworfen: {err}");
                        }
                    } else if settings.rumble_haptics_intensity
                        != chiaki_settings::settings::RumbleHapticsIntensity::Off
                    {
                        // C++ PushHapticsFrame-Rumble-Fallback.
                        if let Some((left, right)) =
                            haptics_rumble_fallback(&data, settings.rumble_haptics_intensity)
                        {
                            if let Some(feedback) = &feedback {
                                // 16-bit → 8-bit (chiaki_input-Rumble-Skala).
                                feedback(FeedbackCmd::Rumble {
                                    left: (left >> 8) as u8,
                                    right: (right >> 8) as u8,
                                });
                            }
                        }
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }

            // --- Video: ALLE queued Frames in FIFO-Reihenfolge dekodieren ---
            // (C: video_sample_cb dekodiert jeden Frame; H.265-Referenzkette
            // bricht sonst → Bildzerreißen bei Bewegung). Angezeigt wird nur
            // der NEUESTE Frame: jeder decodierter Frame wird sofort nach
            // NV12 kopiert (DecodedFrame leiht aus dem Decoder-Pool), VSR
            // läuft auf dem kopierten neuesten Frame (process_frame_nv12).
            {
                let mut latest: Option<(NV12Frame, f64, f64, i32, bool)> = None;
                while let Some(sample) = session.shared_video_slot().pop() {
                    let Some(decoder) = decoder.as_mut() else { break };
                    let t0 = std::time::Instant::now();
                    let decoded = decoder.decode_sample(
                        &sample.data,
                        sample.frames_lost,
                        sample.frame_recovered,
                    );
                    t.decode_us += t0.elapsed().as_micros() as u64;
                    t.samples += 1;
                    match decoded {
                        Ok(Some(frame)) => {
                            // VSR einmalig mit dem ersten Frame initialisieren
                            // (C++: init(firstFrame, scalePct), CUDA-Kontext
                            // aus dem Decoder).
                            if let (Some(up), false) = (&mut vsr, vsr_inited) {
                                vsr_inited = up.init(
                                    &frame,
                                    decoder.cuda_context().unwrap_or(std::ptr::null_mut()),
                                    decoder.cuda_stream().unwrap_or(std::ptr::null_mut()),
                                    settings.nv_vsr_scale,
                                );
                                session
                                    .telemetry
                                    .vsr_active
                                    .store(up.is_active(), Ordering::Relaxed);
                                *session
                                    .telemetry
                                    .vsr_error
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner()) =
                                    up.last_error().map(str::to_string);
                                if let Some(ms) = up.engine_load_ms() {
                                    tracing::info!("VSR: Engine in {ms} ms geladen");
                                }
                            }
                            let t1 = std::time::Instant::now();
                            let nv12_res = nv12_from_planes(&frame);
                            t.nv12_copy_us += t1.elapsed().as_micros() as u64;
                            match nv12_res {
                                Ok(nv12) => {
                                    latest = Some((
                                        nv12,
                                        frame.pts,
                                        frame.duration,
                                        frame.frames_lost,
                                        frame.recovered,
                                    ));
                                }
                                Err(err) => {
                                    tracing::error!("NV12-Kopie fehlgeschlagen: {err:?}")
                                }
                            }
                        }
                        Ok(None) => {} // vor dem ersten IDR noch kein Frame
                        Err(err) => tracing::warn!("Decode-Fehler: {err:?}"),
                    }
                }
                let Some((nv12, pts, duration, frames_lost, recovered)) = latest else {
                    continue;
                };
                let vsr_used = if vsr_inited {
                    let t2 = std::time::Instant::now();
                    let used = vsr.as_mut().is_some_and(|up| {
                        up.process_frame_nv12(
                            &nv12.y_plane(),
                            &nv12.uv_plane(),
                            nv12.y_stride,
                            nv12.uv_stride,
                            nv12.width,
                            nv12.height,
                            pts,
                            duration,
                            frames_lost,
                            recovered,
                            &mut vsr_buf,
                        )
                    });
                    t.vsr_us += t2.elapsed().as_micros() as u64;
                    used
                } else {
                    false
                };
                let out = if vsr_used {
                    // Zero-Copy: der VSR-Output ist bereits NV12-layoutet
                    // (Y@0 + UV@pitch*h, contiguous) — Buffer ÜBERNEHMEN statt
                    // 12 MB zu kopieren.
                    let t3 = std::time::Instant::now();
                    let moved = NV12Frame::from_parts(
                        vsr_buf.width(),
                        vsr_buf.height(),
                        vsr_buf.pitch(),
                        vsr_buf.pitch(),
                        vsr_buf.clone().into_data(),
                    );
                    t.out_copy_us += t3.elapsed().as_micros() as u64;
                    match moved {
                        Ok(out) => out,
                        Err(err) => {
                            tracing::error!("VSR-Output-Übernahme fehlgeschlagen: {err:?}");
                            nv12
                        }
                    }
                } else {
                    nv12
                };
                t.frames += 1;
                if t.frames % 300 == 0 {
                    let n = t.frames.max(1) as f64;
                    tracing::info!(
                        "Media-Pipeline (Ø über {} Frames, {} Samples, Slot-Drops {}): decode {:.2} ms, nv12-copy {:.2} ms, vsr {:.2} ms, out-take {:.2} ms — Summe {:.2} ms/Frame (Budget 16,7)",
                        t.frames, t.samples, session.shared_video_slot().dropped(),
                        t.decode_us as f64 / n / 1000.0,
                        t.nv12_copy_us as f64 / n / 1000.0,
                        t.vsr_us as f64 / n / 1000.0,
                        t.out_copy_us as f64 / n / 1000.0,
                        (t.decode_us + t.nv12_copy_us + t.vsr_us + t.out_copy_us) as f64 / n / 1000.0,
                    );
                }
                session.presenter.set_frame(out);
            }
        }

        // --- Aufräumen auf DEMSELBEN Thread (!Send-Verträge) ---
        drop(mic); // AudioInput: Drain-Join + Capture-Stop
        drop(audio_out); // AudioOutput: cpal-Stream stoppen
        if let Some(player) = haptics {
            player.close(); // waveOut sauber schließen
        }
        drop(decoder);
        tracing::info!("Media-Thread beendet");
    }

    /// Mic aufbauen (C++ ToggleMute-Pfad: connect + AudioInput un/stumm).
    fn start_mic(
        settings: &MediaSettings,
        session: &ActiveSession,
        mic: &mut Option<AudioInput>,
        ensure_connected: bool,
    ) {
        if ensure_connected {
            if let Err(err) = session.connect_microphone_internal() {
                tracing::warn!("connect_microphone fehlgeschlagen: {err:?}");
                return;
            }
        }
        if mic.is_none() {
            let session_for_frames = session.clone();
            match AudioInput::new(
                settings.audio_in_device.as_deref(),
                settings.audio_buffer_size,
                false, // start_muted=false — das Verstummen macht der Ctrl-Toggle
                move |packet: &[u8]| {
                    // 40-Byte-Opus-Frames → Session (AudioSender/Takion).
                    let _ = session_for_frames.send_mic_data(packet);
                },
            ) {
                Ok(input) => {
                    tracing::info!("Mikrofon '{}' geöffnet", input.device_name());
                    *mic = Some(input);
                }
                Err(err) => tracing::error!(
                    "Microphone initialization failed, leaving microphone muted ({err:?})"
                ),
            }
        }
        if let Some(input) = mic {
            input.set_muted(false);
        }
    }

    /// Skaliert Haptics-S16-Samples mit `haptic_override`
    /// (C++ streamsession.cpp: adjusted = amplitude * haptic_override).
    fn scale_haptics(data: &[u8], override_factor: f32) -> Vec<u8> {
        let mut out = data.to_vec();
        for chunk in out.chunks_exact_mut(2) {
            let s = i16::from_le_bytes([chunk[0], chunk[1]]);
            let scaled = (f32::from(s) * override_factor).clamp(-32768.0, 32767.0) as i16;
            chunk.copy_from_slice(&scaled.to_le_bytes());
        }
        out
    }

    /// Kopiert die echten `DecodedFrame`-Planes in einen besitzenden
    /// [`NV12Frame`] (zeilenweise — die Quell-Planes haben NVDEC-Strides und
    /// liegen im FFmpeg-Frame-Pool, der nur bis zum nächsten decode gültig
    /// ist; siehe chiaki-media/decoder.rs Lifetime-Vertrag).
    fn nv12_from_planes(frame: &DecodedFrame) -> ChiakiResult<NV12Frame> {
        let (w, h) = (frame.width, frame.height);
        let y_stride = frame.planes[0].stride;
        let uv_stride = frame.planes[1].stride;
        let mut out = NV12Frame::with_strides(w, h, y_stride, uv_stride)
            .map_err(|_| chiaki_core::ChiakiError::InvalidData)?;
        let y_len = y_stride * h as usize;
        let uv_len = uv_stride * h as usize / 2;
        // SAFETY: Die Plane-Pointer zeigen in den FFmpeg-Frame-Pool bzw. den
        // sws-Zielbuffer des Decoders und sind für `stride * Zeilen` Bytes
        // lesbar (Lifetime-Vertrag: gültig bis zum nächsten decode — wir
        // kopieren synchron im selben Aufruf).
        unsafe {
            let y_src = std::slice::from_raw_parts(frame.planes[0].as_ptr(), y_len);
            let uv_src = std::slice::from_raw_parts(frame.planes[1].as_ptr(), uv_len);
            out.data[..y_len].copy_from_slice(y_src);
            out.data[y_len..].copy_from_slice(uv_src);
        }
        Ok(out)
    }

    /// [`chiaki_media::vsr::FrameBuf`] (kontiguierliches NV12,
    /// `aligned_height == height`) → besitzender [`NV12Frame`].
    fn nv12_from_contiguous(w: u32, h: u32, pitch: usize, buf: &FrameBuf) -> ChiakiResult<NV12Frame> {
        let mut out = NV12Frame::with_strides(w, h, pitch, pitch)
            .map_err(|_| chiaki_core::ChiakiError::InvalidData)?;
        let (y_len, uv_len) = (pitch * h as usize, pitch * h as usize / 2);
        out.data[..y_len].copy_from_slice(buf.y());
        out.data[y_len..y_len + uv_len].copy_from_slice(buf.uv());
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Haptics-Rumble-Fallback (Port des C++ PushHapticsFrame-Zweigs, pure fn)
// ---------------------------------------------------------------------------

/// C: `#define HAPTIC_RUMBLE_MIN_STRENGTH 100` (streamsession.cpp).
const HAPTIC_RUMBLE_MIN_STRENGTH: u32 = 100;

/// Port des Rumble-Fallbacks aus `StreamSession::PushHapticsFrame` (C++):
/// Mittelwert der |Amplituden|×2 je Kanal, Mindeststärke 100, Intensitäts-
/// Teiler (VeryWeak/5, Weak/2, Normal/1, Strong/×2, VeryStrong/×5), Minimum
/// 512 für Controller, die beim Rumble auf 9 Bits shiften.
/// Liefert `(left, right)` als 16-bit-Stärken oder `None` (unter Schwelle).
pub fn haptics_rumble_fallback(
    buf: &[u8],
    intensity: RumbleHapticsIntensity,
) -> Option<(u16, u16)> {
    if buf.is_empty() || buf.len() % 4 != 0 {
        return None;
    }
    let mut sum_l: u32 = 0;
    let mut sum_r: u32 = 0;
    let count = buf.len() / 4;
    for chunk in buf.chunks_exact(4) {
        let l = i16::from_le_bytes([chunk[0], chunk[1]]);
        let r = i16::from_le_bytes([chunk[2], chunk[3]]);
        sum_l += (l.unsigned_abs() as u32) * 2;
        sum_r += (r.unsigned_abs() as u32) * 2;
    }
    let mut tl = sum_l / count as u32;
    let mut tr = sum_r / count as u32;
    tl = if tl > HAPTIC_RUMBLE_MIN_STRENGTH { tl } else { 0 };
    tr = if tr > HAPTIC_RUMBLE_MIN_STRENGTH { tr } else { 0 };
    if tl == 0 && tr == 0 {
        return None;
    }
    let scaled: (u32, u32) = match intensity {
        RumbleHapticsIntensity::VeryWeak => (tl / 5, tr / 5),
        RumbleHapticsIntensity::Weak => (tl / 2, tr / 2),
        RumbleHapticsIntensity::Strong => (tl.saturating_mul(2), tr.saturating_mul(2)),
        RumbleHapticsIntensity::VeryStrong => (tl.saturating_mul(5), tr.saturating_mul(5)),
        _ => (tl, tr),
    };
    let left = scaled.0.min(u16::MAX as u32) as u16;
    let right = scaled.1.min(u16::MAX as u32) as u16;
    // Minimum, wenn über 0 (C: "controllers that shift up to 9 bits").
    let left = if left > 0 && left < (1 << 9) { 1 << 9 } else { left };
    let right = if right > 0 && right < (1 << 9) { 1 << 9 } else { right };
    Some((left, right))
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
    fn build_connect_info_psn_haengt_holepunch_an() {
        // PSN-Pfad (C++ StreamSession-Konstruktor mit duid): Holepunch-Session
        // als Trait-Objekt in der ConnectInfo, rudp_sock wie im C++ ungesetzt,
        // Regist-Daten leer (das PSN-Regist im session_thread liefert sie),
        // Account-ID gesetzt, Remote-Profil.
        let mut settings = test_settings();
        settings.set_resolution_remote_ps5(chiaki_settings::settings::ResolutionPreset::P720);

        let duid = "41".repeat(32);
        let req = ConnectRequest::from_psn(
            duid.clone(),
            true,
            [9; 8],
            Arc::new(
                chiaki_remote::holepunch::HolepunchSession::new("dummy-token")
                    .expect("HolepunchSession baubar"),
            ),
        );
        assert!(matches!(req.host_id, HostId::Psn { .. }));

        let info = build_connect_info(&req, &settings).expect("ConnectInfo baubar");
        assert!(info.holepunch_session.is_some(), "Trait-Objekt gesetzt");
        assert!(info.rudp_sock.is_none(), "C++ lässt rudp_sock ungesetzt");
        assert_eq!(info.psn_account_id, [9; 8]);
        assert_eq!(info.host, duid);
        assert!(info.ps5);
        assert_eq!(info.regist_key, [0; 16], "PSN-Regist füllt regist_key/morning");
        assert_eq!(info.morning, [0; 16]);
        assert!(!info.auto_regist);
        // Remote-Profil (duid → remote-Gruppe, wie der C++-Zweig um
        // isLocalAddress(host) im StreamSessionConnectInfo-Ctor).
        assert_eq!(
            (info.video_profile.width, info.video_profile.height),
            (1280, 720)
        );
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

    #[test]
    fn core_target_mappt_diskriminanten() {
        assert_eq!(core_target(hosts::Target::Ps4Eight).value(), 800);
        assert_eq!(core_target(hosts::Target::Ps4Nine).value(), 900);
        assert_eq!(core_target(hosts::Target::Ps4Ten).value(), 1000);
        assert_eq!(core_target(hosts::Target::Ps5One).value(), 1_000_100);
        assert!(core_target(hosts::Target::Ps5Unknown).is_ps5());
        assert!(!core_target(hosts::Target::Ps4Ten).is_ps5());
    }

    #[test]
    fn regist_host_liefert_live_state_und_fehlschlag() {
        // Loopback: Port 9295 ist dort zu (Verbindungsaufbau schlägt schnell
        // fehl) → Regist-Flow endet mit FinishedFailed → Result::Err.
        let manager = SessionManager::new(
            Arc::new(Mutex::new(test_settings())),
            event_sender_for_tests(),
            super::DiscoveryHandle::disabled(),
        );
        let handle = manager.regist_host(RegistRequest {
            host: "127.0.0.1".into(),
            target: hosts::Target::Ps5One,
            broadcast: false,
            psn_online_id: None,
            psn_account_id: Some([1; 8]),
            pin: 12345678,
            console_pin: 0,
        });
        // Auf Abschluss warten (max. 15 s — Loopback-Refusal ist schnell).
        let mut result = None;
        for _ in 0..150 {
            let snap = handle.snapshot();
            if let Some(r) = snap.result {
                result = Some(r);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(!handle.snapshot().running, "Flow muss beendet sein");
        assert!(result.is_some(), "Regist gegen Loopback muss ein Ergebnis liefern");
    }

    /// Eigener UiEventSender für den Test (wie Backend::start ihn baut).
    fn event_sender_for_tests() -> super::super::events::UiEventSender {
        let queue = super::super::events::UiEventQueue::new();
        queue.sender()
    }

    #[test]
    fn hw_backend_setting_wird_gemappt() {
        assert_eq!(hw_backend_from_setting("auto"), HwBackend::Auto);
        assert_eq!(hw_backend_from_setting(""), HwBackend::Auto);
        assert_eq!(hw_backend_from_setting("none"), HwBackend::None);
        assert_eq!(hw_backend_from_setting("CUDA"), HwBackend::Cuda);
        assert_eq!(hw_backend_from_setting("d3d11va"), HwBackend::D3D11Va);
        assert_eq!(hw_backend_from_setting("vulkan"), HwBackend::Vulkan);
        assert_eq!(hw_backend_from_setting("quatsch"), HwBackend::Auto);
    }

    #[test]
    fn media_settings_erzwingt_cuda_bei_vsr_und_clampt_scale() {
        let mut settings = test_settings();
        settings.set_nv_vsr_enabled(true);
        settings.set_nv_vsr_scale(999);
        settings.set_hardware_decoder("none".into());
        let req = ConnectRequest::from_address("10.0.0.1".into(), true, LinkQuality::Local);
        let media = MediaSettings::from_settings(&req, &settings);
        assert_eq!(media.hw_backend, HwBackend::Cuda, "VSR erzwingt CUDA-Decoder");
        assert_eq!(media.nv_vsr_scale, 200, "Scale außerhalb 100..=400 → Default 200");

        settings.set_nv_vsr_enabled(false);
        let media = MediaSettings::from_settings(&req, &settings);
        assert_eq!(media.hw_backend, HwBackend::None, "hw_decoder-Setting greift ohne VSR");
    }

    #[test]
    fn video_slot_fifo_decodiert_alle() {
        // H.265-Referenzkette: ALLE Frames müssen in FIFO-Reihenfolge beim
        // Decoder ankommen (C: video_sample_cb dekodiert jeden). Erst bei
        // Kapazitätsüberlauf wird Drop-Oldest gezählt.
        let slot = VideoSlot::new();
        for i in 0..8u8 {
            assert!(slot.push(VideoSample { data: vec![i], frames_lost: 0, frame_recovered: false }));
        }
        for i in 0..8u8 {
            assert_eq!(slot.pop().expect("FIFO-Ordnung").data, vec![i]);
        }
        assert!(slot.pop().is_none());
        assert_eq!(slot.dropped(), 0);

        // Überlauf: älteste verworfen, Reihenfolge bleibt, Zähler stimmt.
        for i in 0..(VIDEO_SLOT_CAP + 4) as u8 {
            slot.push(VideoSample { data: vec![i], frames_lost: 0, frame_recovered: false });
        }
        assert_eq!(slot.dropped(), 4);
        assert_eq!(slot.pop().unwrap().data, vec![4]);
    }

    #[test]
    fn haptics_rumble_fallback_port() {
        use chiaki_settings::settings::RumbleHapticsIntensity as I;
        // 4 Stereo-Samples mit |Amplitude| 5000 → sum 40000, Mittel 10000;
        // Normal → 10000/10000 (über 512-Minimum).
        let mut buf = Vec::new();
        for _ in 0..4 {
            buf.extend_from_slice(&5000i16.to_le_bytes());
            buf.extend_from_slice(&5000i16.to_le_bytes());
        }
        let (l, r) = haptics_rumble_fallback(&buf, I::Normal).expect("über Schwelle");
        assert_eq!((l, r), (10000, 10000));

        // Sehr schwach → /5 → 2000 (beide über dem 512-Minimum).
        let (l, r) = haptics_rumble_fallback(&buf, I::VeryWeak).expect("über Schwelle");
        assert_eq!((l, r), (2000, 2000));

        // Stumm → unter Schwelle (100) → None.
        let quiet: Vec<u8> = [0i16; 8].iter().flat_map(|s| s.to_le_bytes()).collect();
        assert!(haptics_rumble_fallback(&quiet, I::Normal).is_none());

        // Kleiner Wert knapp über Schwelle → Minimum 1<<9.
        let mut small = Vec::new();
        for _ in 0..4 {
            small.extend_from_slice(&60i16.to_le_bytes()); // |60|*2 = 120 > 100
            small.extend_from_slice(&0i16.to_le_bytes()); // 0 → 0
        }
        let (l, r) = haptics_rumble_fallback(&small, I::Normal).expect("über Schwelle");
        assert_eq!((l, r), (512, 0), "Minimum 512 links, rechts unter Schwelle → 0");
    }

    #[test]
    fn telemetry_defaults() {
        let t = StreamTelemetry::new();
        assert_eq!(t.decoder_backend_name(), "—");
        assert_eq!(t.video_bytes.load(Ordering::Relaxed), 0);
        assert!(!t.vsr_active.load(Ordering::Relaxed));
    }
}
