// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/session.c + lib/include/chiaki/session.h (chiaki-ng).
//
// Orchestrierung einer Remote-Play-Session (C: session_thread_func):
// optional PSN-Holepunch/Regist -> HTTP-Session-Request (mit RP-Version-
// Nachverhandlung) -> rpcrypt-Auth -> Ctrl-Start (+ Login-PIN-Loop) ->
// Senkusha (MTU/RTT) -> StreamConnection (Takion/ECDH/AV).
//
// Threading-Modell (1:1 zum C):
// - session_thread (hier): orchestriert die Phasen, wartet über
//   `SessionShared::state_cond` auf Ctrl-Ereignisse.
// - ctrl-Thread (ctrl.rs): Control-Verbindung zu Port 9295.
// - takion-recv-thread (in Takion) + feedbacksender/congestion-Threads.
//
// Mutex-Ordnung (Deadlock-Freiheit):
// - `SessionShared::state` (C: state_mutex) wird nie zusammen mit Mutexen
//   der StreamConnection gehalten (das C trennt das genauso strikt) und
//   nie während blockierender Netzwerkoperationen gehalten.
// - Die `ctrl`-Mutex wird nur kurz für Aufrufe an Ctrl gehalten, nie
//   während eines Wartens auf `state_cond`.
// - Events (`SessionCallbacks`) werden ohne gehaltene Mutexe gefeuert.
//
// Abweichungen zum C (dokumentiert):
// - Der PSN-Holepunch-Pfad läuft über das `HolepunchSession`-Trait: die
//   Typen (HolepunchSession/Rudp) liegen in chiaki-remote, das seinerseits
//   von chiaki-core abhängt — daher definiert core nur die Schnittstelle
//   (C: direkte Zeiger auf ChiakiHolepunchSession/ChiakiRudp). Das Regist
//   läuft deshalb synchron im session_thread (C: eigener Thread + Condwait).
// - `stop_pipe_connect` verbindet quantisiert (100 ms), damit stop()
//   reagiert; das C nutzt select auf demselben Socket (C-TODO: "this can
//   block, make cancelable somehow" betraf die DNS-Auflösung).

use std::io::Write as _;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::audio::AudioHeader;
use crate::base64;
use crate::controller::ControllerState;
use crate::ecdh::Ecdh;
use crate::error::{ChiakiError, ChiakiResult, Target};
use crate::gkcrypt::HANDSHAKE_KEY_SIZE;
use crate::http;
use crate::regist::{RegisteredHost, PSN_ACCOUNT_ID_SIZE};
use crate::rpcrypt::{Rpcrypt, RPCRYPT_KEY_SIZE};
use crate::stoppipe::StopPipe;
use crate::streamconnection::{DualSenseEffectIntensity, StreamConnection};
use crate::takion::DisableAudioVideo;
use crate::video::{ConnectVideoProfile, VideoFpsPreset, VideoResolutionPreset};

/// C: `#define SESSION_PORT 9295`
pub const SESSION_PORT: u16 = 9295;
/// C: `#define SESSION_EXPECT_TIMEOUT_MS 5000`
pub const SESSION_EXPECT_TIMEOUT_MS: u64 = 5000;
/// C: `#define SESSION_EXPECT_CTRL_START_MS 10000`
pub const SESSION_EXPECT_CTRL_START_MS: u64 = 10000;
/// C: `#define CHIAKI_RP_DID_SIZE 32`
pub const RP_DID_SIZE: usize = 32;
/// C: `#define CHIAKI_SESSION_ID_SIZE_MAX 80`
pub const SESSION_ID_SIZE_MAX: usize = 80;
/// C: `#define CHIAKI_SESSION_AUTH_SIZE 0x10`
pub const SESSION_AUTH_SIZE: usize = 0x10;

// C: CHIAKI_RP_APPLICATION_REASON_*
pub const RP_APPLICATION_REASON_REGIST_FAILED: u32 = 0x8010_8b09;
pub const RP_APPLICATION_REASON_INVALID_PSN_ID: u32 = 0x8010_8b02;
pub const RP_APPLICATION_REASON_IN_USE: u32 = 0x8010_8b10;
pub const RP_APPLICATION_REASON_CRASH: u32 = 0x8010_8b15;
pub const RP_APPLICATION_REASON_RP_VERSION: u32 = 0x8010_8b11;
pub const RP_APPLICATION_REASON_UNKNOWN: u32 = 0x8010_8bff;

/// Port von `chiaki_rp_application_reason_string()`.
pub fn rp_application_reason_string(reason: u32) -> &'static str {
    match reason {
        RP_APPLICATION_REASON_REGIST_FAILED => "Regist failed, probably invalid PIN",
        RP_APPLICATION_REASON_INVALID_PSN_ID => "Invalid PSN ID",
        RP_APPLICATION_REASON_IN_USE => "Remote is already in use",
        RP_APPLICATION_REASON_CRASH => "Remote Play on Console crashed",
        RP_APPLICATION_REASON_RP_VERSION => "RP-Version mismatch",
        _ => "unknown",
    }
}

/// Port von `chiaki_rp_version_string()`. `None` entspricht dem C-NULL.
pub fn rp_version_string(target: Target) -> Option<&'static str> {
    match target {
        Target::Ps4_8 => Some("8.0"),
        Target::Ps4_9 => Some("9.0"),
        Target::Ps4_10 => Some("10.0"),
        Target::Ps5_1 => Some("1.0"),
        _ => None,
    }
}

/// Port von `chiaki_rp_version_parse()`.
pub fn rp_version_parse(rp_version_str: &str, is_ps5: bool) -> Target {
    if is_ps5 {
        if rp_version_str == "1.0" {
            return Target::Ps5_1;
        }
        return Target::Ps5Unknown;
    }
    match rp_version_str {
        "8.0" => Target::Ps4_8,
        "9.0" => Target::Ps4_9,
        "10.0" => Target::Ps4_10,
        _ => Target::Ps4Unknown,
    }
}

/// Port von `ChiakiVideoResolutionPreset` ("values must not change").
/// (Implementierung/Golden-Tabelle: `video::connect_video_profile_preset`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum VideoResolutionPresetSession {
    P360 = 1,
    P540 = 2,
    P720 = 3,
    P1080 = 4,
}

/// Port von `ChiakiVideoFPSPreset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum VideoFpsPresetSession {
    Fps30 = 30,
    Fps60 = 60,
}

/// Port von `chiaki_connect_video_profile_preset()` (Rückgabe statt
/// Out-Param; die Golden-Tabelle lebt in video.rs).
pub fn connect_video_profile_preset_session(
    resolution: VideoResolutionPresetSession,
    fps: VideoFpsPresetSession,
) -> ConnectVideoProfile {
    let resolution = match resolution {
        VideoResolutionPresetSession::P360 => VideoResolutionPreset::Res360p,
        VideoResolutionPresetSession::P540 => VideoResolutionPreset::Res540p,
        VideoResolutionPresetSession::P720 => VideoResolutionPreset::Res720p,
        VideoResolutionPresetSession::P1080 => VideoResolutionPreset::Res1080p,
    };
    let fps = match fps {
        VideoFpsPresetSession::Fps30 => VideoFpsPreset::Fps30,
        VideoFpsPresetSession::Fps60 => VideoFpsPreset::Fps60,
    };
    let mut profile = ConnectVideoProfile::default();
    crate::video::connect_video_profile_preset(&mut profile, resolution, fps);
    profile
}

// ----------------------------------------------------------------------
// Holepunch-Schnittstelle (PSN-Pfad; Implementierung in chiaki-remote)
// ----------------------------------------------------------------------

/// C: `ChiakiHolepunchPortType` ("values must not change").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum HolepunchPortType {
    Ctrl = 0,
    Data = 1,
}

/// C: `ChiakiHolepunchRegistInfo` (holepunch.h).
#[derive(Debug, Clone, Default)]
pub struct HolepunchRegistInfo {
    pub data1: [u8; 16],
    pub data2: [u8; 16],
    pub custom_data1: [u8; 16],
    pub regist_local_ip: String,
}

/// Für den Ctrl-RUDP-Pfad (ctrl.rs) gespiegelte `RudpMessage` — chiaki-core
/// darf chiaki-remote nicht importieren (Dependency-Richtung remote → core),
/// daher definiert das Trait-Objekt diesen Mirror-Typ, den das
/// [`HolepunchSession`]-Impl in chiaki-remote aus der empfangenen
/// RUDP-Message befüllt (inkl. Sub-Message-Kette, C: `RudpMessage.subMessage`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CtrlRudpMessage {
    /// C: `message.subtype` (erstes Byte des Type-Felds im Framing).
    pub subtype: u8,
    /// C: `message.type` (roher 16-Bit-Type-Wert).
    pub type_: u16,
    /// C: `message.remote_counter` (lokaler Counter der Gegenstelle + 1,
    /// siehe `chiaki_rudp_message_parse`).
    pub remote_counter: u16,
    /// C: `message.data`/`data_size`.
    pub data: Vec<u8>,
    /// C: `message.subMessage` (rekursiv; im Wire-Format maximal eine
    /// weitere Stufe, geparst wird die Kette komplett).
    pub sub_message: Option<Box<CtrlRudpMessage>>,
}

/// Port der holepunch-/rudp-Operationen, die `session.c` an
/// `ChiakiHolepunchSession`/`ChiakiRudp` ausführt. chiaki-remote
/// implementiert dieses Trait für seine `HolepunchSession`.
pub trait HolepunchSession: Send + Sync {
    /// C: `chiaki_get_holepunch_sock()` (bereitgelegter UDP-Socket).
    fn sock(&self, port_type: HolepunchPortType) -> Option<UdpSocket>;
    /// C: `chiaki_holepunch_session_create_offer()`.
    fn create_offer(&self, port_type: HolepunchPortType) -> ChiakiResult<()>;
    /// C: `chiaki_holepunch_session_punch_hole()`.
    fn punch_hole(&self, port_type: HolepunchPortType) -> ChiakiResult<()>;
    /// C: `chiaki_get_ps_selected_addr()`.
    fn ps_selected_addr(&self) -> String;
    /// C: `chiaki_get_ps_ctrl_port()`.
    fn ps_ctrl_port(&self) -> u16;
    /// C: `chiaki_get_regist_info()`.
    fn regist_info(&self) -> ChiakiResult<HolepunchRegistInfo>;
    /// C: `chiaki_regist_start/stop/fini` über RUDP (PSN-Regist). Bei Erfolg
    /// wird der `RegisteredHost` geliefert (rp_key/rp_regist_key fließen in
    /// morning/regist_key der Session); `ChiakiError::Canceled` bei Stop.
    fn regist(
        &self,
        info: &HolepunchRegistInfo,
        target: Target,
        psn_account_id: &[u8; PSN_ACCOUNT_ID_SIZE],
        stop: &StopPipe,
    ) -> ChiakiResult<RegisteredHost>;
    /// C: `chiaki_rudp_init` + INIT/COOKIE-Handshake aus
    /// `session_thread_request_session` — liefert `remote_counter`.
    fn rudp_start_session(&self) -> ChiakiResult<u16>;
    /// C: `chiaki_send_recv_http_header_psn()` — Request senden, HTTP-Header
    /// empfangen; liefert `(header_size, received_size, remote_counter)`. Der
    /// `remote_counter` ist — wie im C (`*remote_counter = message.remote_counter`)
    /// — der für das nachfolgende `rudp_finish` zu nutende aktualisierte Wert.
    fn rudp_send_recv_http_header(
        &self,
        request: &[u8],
        remote_counter: u16,
        buf: &mut [u8],
    ) -> ChiakiResult<(usize, usize, u16)>;
    /// C: `chiaki_rudp_send_recv(..., ACK, FINISH, 0, 3)`.
    fn rudp_finish(&self, remote_counter: u16) -> ChiakiResult<()>;
    /// C: `chiaki_rudp_send_switch_to_stream_connection_message()`.
    fn rudp_send_switch_to_stream_connection(&self) -> ChiakiResult<()>;

    // ---- Ctrl-RUDP-Bedarf (RUDP-Zweige von ctrl.c; Implementierung in
    //      chiaki-remote, siehe regist_psn.rs-Trait-Impl) ----

    /// C (ctrl.c `ctrl_connect`, ctrl.c:1165-1187): "CTRL - Starting RUDP
    /// session" — INIT/COOKIE-Handshake über die bestehende Rudp-Instanz
    /// (Wire-Protokoll wie [`HolepunchSession::rudp_start_session`], eigene
    /// CTRL-Log-Texte); liefert den Remote-Counter der Cookie-Antwort.
    fn rudp_ctrl_start_session(&self) -> ChiakiResult<u16>;
    /// C: `chiaki_rudp_send_ctrl_message()` (ctrl_message_send-RUDP-Zweig,
    /// ctrl.c:683): kompletten 8-Byte-Ctrl-Frame (Header + verschlüsselter
    /// Payload) als RUDP-CTRL-Message senden und bis zum ACK in den
    /// RUDP-Send-Buffer stellen.
    fn rudp_send_ctrl_message(&self, message: &[u8]) -> ChiakiResult<()>;
    /// C: `chiaki_rudp_recv_only()` (ctrl_thread_func, ctrl.c:520).
    /// `buf_size` entspricht `sizeof(ctrl->rudp_recv_buf) - recv_buf_size`
    /// (520 - n). Liefert `ChiakiError::Timeout`, wenn innerhalb der
    /// RUDP-Empfangszeit kein Datagramm kommt (das C blockiert hinter einem
    /// select; der Ctrl-Loop behandelt Timeout als "nichts empfangen",
    /// siehe ctrl.rs-Moduldoku).
    fn rudp_recv_only(&self, buf_size: usize) -> ChiakiResult<CtrlRudpMessage>;
    /// C: `chiaki_rudp_ack_packet()` (Send-Buffer-ACK, ctrl.c:543/560/568).
    fn rudp_ack_packet(&self, counter_to_ack: u16) -> ChiakiResult<()>;
    /// C: `chiaki_rudp_send_ack_message()` (ctrl.c:545/569/1391).
    fn rudp_send_ack_message(&self, remote_counter: u16) -> ChiakiResult<()>;
    /// C: `chiaki_rudp_print_message()` (Fehlerdiagnose, ctrl.c:530).
    fn rudp_print_message(&self, message: &CtrlRudpMessage);
}

// ----------------------------------------------------------------------
// QuitReason / SessionEvent / Callbacks
// ----------------------------------------------------------------------

/// Port von `ChiakiQuitReason` (session.h; Reihenfolge wie im C).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u32)]
pub enum QuitReason {
    #[default]
    None,
    Stopped,
    SessionRequestUnknown,
    SessionRequestConnectionRefused,
    SessionRequestRpInUse,
    SessionRequestRpCrash,
    SessionRequestRpVersionMismatch,
    CtrlUnknown,
    CtrlConnectFailed,
    CtrlConnectionRefused,
    StreamConnectionUnknown,
    StreamConnectionRemoteDisconnected,
    /// like REMOTE_DISCONNECTED, but because the server shut down
    StreamConnectionRemoteShutdown,
    PsnRegistFailed,
}

/// Port von `chiaki_quit_reason_string()`.
pub fn quit_reason_string(reason: QuitReason) -> &'static str {
    match reason {
        QuitReason::Stopped => "Stopped",
        QuitReason::SessionRequestUnknown => "Unknown Session Request Error",
        QuitReason::SessionRequestConnectionRefused => "Connection Refused in Session Request",
        QuitReason::SessionRequestRpInUse => "Remote Play on Console is already in use",
        QuitReason::SessionRequestRpCrash => "Remote Play on Console has crashed",
        QuitReason::SessionRequestRpVersionMismatch => "RP-Version mismatch",
        QuitReason::CtrlUnknown => "Unknown Ctrl Error",
        QuitReason::CtrlConnectionRefused => "Connection Refused in Ctrl",
        QuitReason::CtrlConnectFailed => "Ctrl failed to connect",
        QuitReason::StreamConnectionUnknown => "Unknown Error in Stream Connection",
        QuitReason::StreamConnectionRemoteDisconnected => {
            "Remote has disconnected from Stream Connection"
        }
        QuitReason::StreamConnectionRemoteShutdown => {
            "Remote has disconnected from Stream Connection the because Server shut down"
        }
        QuitReason::PsnRegistFailed => "The Console Registration using PSN has failed",
        QuitReason::None => "Unknown",
    }
}

/// Port von `chiaki_quit_reason_is_error()`.
pub fn quit_reason_is_error(reason: QuitReason) -> bool {
    !matches!(
        reason,
        QuitReason::Stopped | QuitReason::StreamConnectionRemoteShutdown
    )
}

/// Port von `ChiakiEvent` (C: type + union) als Enum.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    Connected,
    /// `pin_incorrect`: false on first request, true if the pin entered
    /// before was incorrect
    LoginPinRequest(bool),
    /// C: `data_holepunch.finished` — false when punching hole, true when
    /// finished
    Holepunch { finished: bool },
    /// Auto-Regist erfolgreich (C: `CHIAKI_EVENT_REGIST` mit Host).
    Regist(Box<RegisteredHost>),
    /// PS4 PSN-Regist: Server-Nickname empfangen.
    NicknameReceived(String),
    Quit {
        reason: QuitReason,
        reason_str: String,
    },
    /// C: `CHIAKI_EVENT_KEYBOARD_OPEN` (text_str).
    KeyboardText(String),
    /// C: `CHIAKI_EVENT_KEYBOARD_TEXT_CHANGE`.
    KeyboardTextChange(String),
    KeyboardRemoteClose,
    /// Audio-Stream-Info (C: ChiakiAudioSink.header_cb-Pfad des
    /// AudioReceivers).
    AudioStreamInfo(AudioHeader),
    /// C: `CHIAKI_EVENT_RUMBLE`.
    Rumble { unknown: u8, left: u8, right: u8 },
    /// C: `CHIAKI_EVENT_TRIGGER_EFFECTS`.
    TriggerEffects {
        type_left: u8,
        type_right: u8,
        left: [u8; 10],
        right: [u8; 10],
    },
    /// C: `CHIAKI_EVENT_LED_COLOR`.
    LedColor([u8; 3]),
    /// C: `CHIAKI_EVENT_PLAYER_INDEX`.
    PlayerIndex(u8),
    /// C: `CHIAKI_EVENT_MOTION_RESET`.
    MotionReset,
    /// C: `CHIAKI_EVENT_HAPTIC_INTENSITY`.
    HapticIntensity(DualSenseEffectIntensity),
    /// C: `CHIAKI_EVENT_TRIGGER_INTENSITY`.
    TriggerIntensity(DualSenseEffectIntensity),
    /// C: `CHIAKI_EVENT_VIDEO_FEC_FAILURE`.
    VideoFecFailure {
        frame_index: i32,
        idr_request_sent: bool,
    },
    /// Ctrl: Stream kann derzeit nicht angezeigt werden
    /// (C: `ChiakiCtrlDisplaySink.cantdisplay_cb`).
    CantDisplay { cant: bool },
}

/// Port von `ChiakiVideoSampleCallback`: roher (decodierbarer) Frame
/// (unit-header + data, mit Padding — siehe videoreceiver.rs).
#[derive(Debug, Clone, Copy)]
pub struct VideoFrame<'a> {
    pub data: &'a [u8],
    pub frames_lost: i32,
    pub frame_recovered: bool,
}

/// Port der Session-Callbacks (C: event_cb / video_sample_cb / audio_sink /
/// haptics_sink als ein Trait).
///
/// `audio_pcm` erhält — wie der C-`ChiakiAudioSink.frame_cb` — die rohen
/// Opus-Frames des AudioReceivers (die App dekodiert; Header-Info kommt als
/// [`SessionEvent::AudioStreamInfo`]).
pub trait SessionCallbacks: Send + Sync + 'static {
    /// C: `video_sample_cb` — Rückgabe: ob der Frame übernommen wurde
    /// (false => corrupt-frame-Report für einen neuen Keyframe).
    fn video_frame(&self, _frame: VideoFrame<'_>) -> bool {
        true
    }
    /// C: audio sink frame_cb (Opus-Frames; leerer Slice = Concealment).
    fn audio_pcm(&self, _pcm: &[u8]) {}
    /// C: `event_cb`.
    fn event(&self, _ev: SessionEvent) {}
    /// C: haptics sink frame_cb.
    fn haptics(&self, _data: &[u8]) {}
}

/// Test-Helper: SessionCallbacks, die nur Events an einen Callback
/// durchreichen.
#[cfg(test)]
pub struct TestSessionCallbacks {
    f: Box<dyn Fn(SessionEvent) + Send + Sync>,
}

#[cfg(test)]
impl TestSessionCallbacks {
    pub fn new(f: impl Fn(SessionEvent) + Send + Sync + 'static) -> Self {
        TestSessionCallbacks { f: Box::new(f) }
    }
}

#[cfg(test)]
impl SessionCallbacks for TestSessionCallbacks {
    fn event(&self, ev: SessionEvent) {
        (self.f)(ev);
    }
}

// ----------------------------------------------------------------------
// ConnectInfo
// ----------------------------------------------------------------------

/// Port von `ChiakiConnectInfo` (session.h).
pub struct ConnectInfo {
    pub ps5: bool,
    /// null terminated hostname/IP
    pub host: String,
    /// must be completely filled (pad with \0)
    pub regist_key: [u8; SESSION_AUTH_SIZE],
    pub morning: [u8; 0x10],
    pub video_profile: ConnectVideoProfile,
    /// Downgrade video_profile if server does not seem to support it.
    pub video_profile_auto_downgrade: bool,
    pub enable_keyboard: bool,
    pub enable_dualsense: bool,
    pub audio_video_disabled: DisableAudioVideo,
    pub auto_regist: bool,
    /// PSN-Pfad (chiaki-remote); `None` = direkte Verbindung
    pub holepunch_session: Option<Arc<dyn HolepunchSession>>,
    /// C: `connect_info.rudp_sock` (wird von session.c selbst nicht
    /// verwendet — der Datensock kommt aus dem Holepunch; Feld aus
    /// Vertragstreue erhalten).
    pub rudp_sock: Option<UdpSocket>,
    pub psn_account_id: [u8; PSN_ACCOUNT_ID_SIZE],
    pub packet_loss_max: f64,
    pub enable_idr_on_fec_failure: bool,
    /// How long to wait (µs) for a missing head AV packet before skipping
    /// it. 0 = default (16 ms).
    pub av_reorder_timeout_us: u32,
}

// ----------------------------------------------------------------------
// SessionShared / SessionState
// ----------------------------------------------------------------------

/// Von `SessionShared::state` geschützte Felder (C: state_mutex-Bereich).
pub(crate) struct SessionState {
    pub should_stop: bool,
    pub ctrl_failed: bool,
    pub ctrl_session_id_received: bool,
    pub ctrl_login_pin_requested: bool,
    /// C: ctrl_first_heartbeat_received (wird vom ctrl-Thread gesetzt;
    /// session.c liest es nicht — Feld aus Struktur-Parität erhalten).
    #[allow(dead_code)]
    pub ctrl_first_heartbeat_received: bool,
    pub login_pin_entered: bool,
    pub psn_regist_succeeded: bool,
    pub stream_connection_switch_received: bool,
    pub login_pin: Option<Vec<u8>>,
    pub quit_reason: QuitReason,
    /// additional reason string from remote
    pub quit_reason_str: Option<String>,
}

impl Default for SessionState {
    fn default() -> Self {
        SessionState {
            should_stop: false,
            ctrl_failed: false,
            ctrl_session_id_received: false,
            ctrl_login_pin_requested: false,
            ctrl_first_heartbeat_received: false,
            login_pin_entered: false,
            psn_regist_succeeded: false,
            stream_connection_switch_received: false,
            login_pin: None,
            quit_reason: QuitReason::None,
            quit_reason_str: None,
        }
    }
}

/// Das Arc-geteilte Session-Herzstück (Muster wie TakionShared): alle
/// Zustände, die Ctrl-Thread / session_thread / StreamConnection /
/// Takion-Callback gemeinsam sehen (pub(crate) — ctrl.rs greift direkt zu).
pub(crate) struct SessionShared {
    // C: state_mutex / state_cond / stop_pipe
    pub state: Mutex<SessionState>,
    pub state_cond: Condvar,
    pub stop_pipe: StopPipe,

    pub callbacks: Arc<dyn SessionCallbacks>,

    // C: connect_info (nach Init unveränderlich)
    pub ps5: bool,
    pub enable_dualsense: bool,
    pub enable_keyboard: bool,
    pub enable_idr_on_fec_failure: bool,
    pub av_reorder_timeout_us: u32,
    pub video_profile: Mutex<ConnectVideoProfile>,
    pub video_profile_auto_downgrade: bool,
    pub disable_audio_video: DisableAudioVideo,
    pub packet_loss_max: f64,
    pub auto_regist: bool,
    pub psn_account_id: Mutex<[u8; PSN_ACCOUNT_ID_SIZE]>,
    pub morning: Mutex<[u8; 0x10]>,
    pub regist_key: Mutex<[u8; SESSION_AUTH_SIZE]>,
    pub did: Mutex<[u8; RP_DID_SIZE]>,

    pub holepunch: Option<Arc<dyn HolepunchSession>>,

    // C: target / nonce / rpcrypt / session_id / handshake_key / ecdh /
    //    mtu / rtt / dontfrag
    pub target: Mutex<Target>,
    pub nonce: Mutex<[u8; RPCRYPT_KEY_SIZE]>,
    pub rpcrypt: Mutex<Option<Rpcrypt>>,
    pub session_id: Mutex<String>,
    pub handshake_key: Mutex<[u8; HANDSHAKE_KEY_SIZE]>,
    pub ecdh: Mutex<Option<Ecdh>>,
    pub mtu_in: AtomicU32,
    pub mtu_out: AtomicU32,
    pub rtt_us: AtomicU64,
    pub dontfrag: AtomicBool,

    // C: host_addrinfo(s) / hostname
    /// Alle aufgelösten Adressen (C: host_addrinfos-Liste).
    pub host_addrs: Mutex<Vec<SocketAddr>>,
    /// Die für Ctrl/Senkusha/Takion ausgewählte Adresse (der jeweilige Nutzer
    /// setzt den Port; C: host_addrinfo_selected).
    pub host_addr: Mutex<Option<SocketAddr>>,
    pub hostname: Mutex<String>,
}

impl SessionShared {
    pub(crate) fn new(callbacks: Arc<dyn SessionCallbacks>) -> Self {
        SessionShared {
            state: Mutex::new(SessionState::default()),
            state_cond: Condvar::new(),
            stop_pipe: StopPipe::new(),
            callbacks,
            ps5: false,
            enable_dualsense: false,
            enable_keyboard: false,
            enable_idr_on_fec_failure: false,
            av_reorder_timeout_us: 0,
            video_profile: Mutex::new(ConnectVideoProfile::default()),
            video_profile_auto_downgrade: false,
            disable_audio_video: DisableAudioVideo::NoneDisabled,
            packet_loss_max: 0.0,
            auto_regist: false,
            psn_account_id: Mutex::new([0; PSN_ACCOUNT_ID_SIZE]),
            morning: Mutex::new([0; 0x10]),
            regist_key: Mutex::new([0; SESSION_AUTH_SIZE]),
            did: Mutex::new([0; RP_DID_SIZE]),
            holepunch: None,
            target: Mutex::new(Target::Ps4_10),
            nonce: Mutex::new([0; RPCRYPT_KEY_SIZE]),
            rpcrypt: Mutex::new(None),
            session_id: Mutex::new(String::new()),
            handshake_key: Mutex::new([0; HANDSHAKE_KEY_SIZE]),
            ecdh: Mutex::new(None),
            mtu_in: AtomicU32::new(0),
            mtu_out: AtomicU32::new(0),
            rtt_us: AtomicU64::new(0),
            dontfrag: AtomicBool::new(true),
            host_addrs: Mutex::new(Vec::new()),
            host_addr: Mutex::new(None),
            hostname: Mutex::new(String::new()),
        }
    }

    pub fn lock_state(&self) -> MutexGuard<'_, SessionState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// C: `chiaki_session_send_event` (Event-Callback ohne Locks).
    pub fn send_event(&self, ev: SessionEvent) {
        self.callbacks.event(ev);
    }

    /// C: `video_sample_cb`-Dispatch.
    pub fn video_sample(&self, buf: &[u8], frames_lost: i32, frame_recovered: bool) -> bool {
        self.callbacks
            .video_frame(VideoFrame { data: buf, frames_lost, frame_recovered })
    }

    /// Audio-Frame-Callback als Arc (für den AudioReceiver-Sink).
    pub fn callbacks_audio(&self) -> Arc<dyn Fn(&[u8]) + Send + Sync> {
        let cb = Arc::clone(&self.callbacks);
        Arc::new(move |data: &[u8]| cb.audio_pcm(data))
    }

    /// Haptics-Frame-Callback als Arc (für den Haptics-Sink).
    pub fn callbacks_haptics(&self) -> Arc<dyn Fn(&[u8]) + Send + Sync> {
        let cb = Arc::clone(&self.callbacks);
        Arc::new(move |data: &[u8]| cb.haptics(data))
    }

    /// Aktuelles Target (C: session->target).
    pub fn target(&self) -> Target {
        *self.target.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Quit-Reason setzen (C: session->quit_reason).
    pub fn set_quit_reason(&self, reason: QuitReason) {
        self.lock_state().quit_reason = reason;
    }

    pub fn quit_reason(&self) -> QuitReason {
        self.lock_state().quit_reason
    }
}

// ----------------------------------------------------------------------
// Session
// ----------------------------------------------------------------------

type CtrlHandle = Arc<Mutex<Option<crate::ctrl::Ctrl>>>;

/// Port von `ChiakiSession`.
pub struct Session {
    pub(crate) shared: Arc<SessionShared>,
    pub(crate) ctrl: CtrlHandle,
    pub(crate) stream_connection: Arc<StreamConnection>,
    session_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Session {
    /// Port von `chiaki_session_init()`.
    pub fn new(
        connect_info: ConnectInfo,
        callbacks: Arc<dyn SessionCallbacks>,
    ) -> ChiakiResult<Session> {
        let mut shared = SessionShared::new(callbacks);

        shared.dontfrag.store(true, Ordering::SeqCst);

        // Host-Auflösung (nur ohne Holepunch; C: getaddrinfo mit
        // AF_INET/AF_INET6-Hints je nach ':' im Hostnamen, SOCK_DGRAM).
        if connect_info.holepunch_session.is_none() {
            let addrs = resolve_host_addrs(&connect_info.host).map_err(|_| {
                tracing::error!("Failed to resolve host {}", connect_info.host);
                ChiakiError::ParseAddr
            })?;
            if addrs.is_empty() {
                tracing::error!("Failed to resolve host {}", connect_info.host);
                return Err(ChiakiError::ParseAddr);
            }
            *shared.host_addrs.lock().unwrap_or_else(PoisonError::into_inner) = addrs;
            *shared.regist_key.lock().unwrap_or_else(PoisonError::into_inner) =
                connect_info.regist_key;
            *shared.morning.lock().unwrap_or_else(PoisonError::into_inner) = connect_info.morning;
        } else {
            *shared.psn_account_id.lock().unwrap_or_else(PoisonError::into_inner) =
                connect_info.psn_account_id;
        }

        // DID bauen: prefix + random + suffix (C: did_prefix/did_suffix)
        {
            const DID_PREFIX: [u8; 10] =
                [0x00, 0x18, 0x00, 0x00, 0x00, 0x07, 0x00, 0x40, 0x00, 0x80];
            const DID_SUFFIX: [u8; 6] = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
            let mut did = [0u8; RP_DID_SIZE];
            did[..DID_PREFIX.len()].copy_from_slice(&DID_PREFIX);
            crate::random::random_bytes_crypt(
                &mut did[DID_PREFIX.len()..RP_DID_SIZE - DID_SUFFIX.len()],
            )?;
            did[RP_DID_SIZE - DID_SUFFIX.len()..].copy_from_slice(&DID_SUFFIX);
            *shared.did.lock().unwrap_or_else(PoisonError::into_inner) = did;
        }

        // Video-Profil (C: bei VIDEO_DISABLED auf das 360p-Preset fallen)
        {
            let mut profile = connect_info.video_profile;
            if matches!(
                connect_info.audio_video_disabled,
                DisableAudioVideo::VideoDisabled | DisableAudioVideo::AudioVideoDisabled
            ) {
                profile = ConnectVideoProfile {
                    width: 640,
                    height: 360,
                    max_fps: 0,
                    bitrate: 2000,
                    codec: crate::error::Codec::H264,
                };
            }
            *shared.video_profile.lock().unwrap_or_else(PoisonError::into_inner) = profile;
        }

        *shared.target.lock().unwrap_or_else(PoisonError::into_inner) =
            if connect_info.ps5 { Target::Ps5_1 } else { Target::Ps4_10 };

        shared.ps5 = connect_info.ps5;
        shared.enable_dualsense = connect_info.enable_dualsense;
        shared.enable_keyboard = connect_info.enable_keyboard;
        shared.enable_idr_on_fec_failure = connect_info.enable_idr_on_fec_failure;
        shared.av_reorder_timeout_us = connect_info.av_reorder_timeout_us;
        shared.video_profile_auto_downgrade = connect_info.video_profile_auto_downgrade;
        shared.disable_audio_video = connect_info.audio_video_disabled;
        shared.packet_loss_max = connect_info.packet_loss_max;
        shared.auto_regist = connect_info.auto_regist;
        shared.holepunch = connect_info.holepunch_session.clone();

        let shared = Arc::new(shared);

        let stream_connection = Arc::new(StreamConnection::new(
            Arc::clone(&shared),
            connect_info.packet_loss_max,
        ));

        let session = Session {
            shared,
            ctrl: Arc::new(Mutex::new(None)),
            stream_connection,
            session_thread: Mutex::new(None),
        };

        // C: chiaki_ctrl_init(&session->ctrl, session) — das Ctrl wird im
        // Rust-Port erst nach dem session_request erzeugt, da CtrlInit den
        // (erst dann initialisierten) rpcrypt by value übernimmt
        // (C: Zeiger auf session->rpcrypt).

        Ok(session)
    }

    /// Baut das Ctrl (C: chiaki_ctrl_init) nach dem session_request und legt
    /// es im geteilten Handle ab. Läuft im session_thread. Der Transport folgt
    /// `shared.holepunch` (C: ctrl.c verzweigt an allen Netzstellen auf
    /// `session->rudp`): Holepunch → `CtrlTransport::Holepunch`, sonst TCP.
    fn create_ctrl(shared: &Arc<SessionShared>) -> ChiakiResult<crate::ctrl::Ctrl> {
        let (tx, rx) = mpsc::channel::<crate::ctrl::CtrlMessage>();
        // Rpcrypt ist nicht Clone (rpcrypt.rs ist fertiggestellt); das Ctrl
        // übernimmt ihn per value (C: Zeiger auf dasselbe Objekt) — nach
        // erfolgreicher Ctrl-Erzeugung bleibt er dort.
        let rpcrypt = {
            let mut guard = shared.rpcrypt.lock().unwrap_or_else(PoisonError::into_inner);
            guard.take().ok_or(ChiakiError::Uninitialized)?
        };
        // C (ctrl.c:1299): Port/Adresse je Pfad — der Holepunch-Pfad läuft
        // über RUDP (kein TCP-Connect, Adresse ungenutzt), der TCP-Pfad
        // verbindet selbst zur aufgelösten Adresse.
        let (transport, host_addr) = match &shared.holepunch {
            Some(holepunch) => (
                crate::ctrl::CtrlTransport::Holepunch(Arc::clone(holepunch)),
                SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 0),
            ),
            None => (
                crate::ctrl::CtrlTransport::Tcp,
                shared
                    .host_addr
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .ok_or(ChiakiError::Uninitialized)?,
            ),
        };
        let codec = shared.video_profile.lock().unwrap_or_else(PoisonError::into_inner).codec;

        let init = crate::ctrl::CtrlInit {
            rpcrypt,
            target: shared.target(),
            regist_key: *shared.regist_key.lock().unwrap_or_else(PoisonError::into_inner),
            did: *shared.did.lock().unwrap_or_else(PoisonError::into_inner),
            hostname: shared.hostname.lock().unwrap_or_else(PoisonError::into_inner).clone(),
            host_addr,
            sock: None,
            transport,
            codec,
            enable_dualsense: shared.enable_dualsense,
            enable_keyboard: shared.enable_keyboard,
            msg_queue_tx: tx,
            msg_queue_rx: rx,
            event_cb: make_ctrl_event_cb(shared),
        };
        crate::ctrl::Ctrl::new(init)
    }

    /// Port von `chiaki_session_start()`: startet den session_thread.
    pub fn start(&mut self) -> ChiakiResult<()> {
        let shared = Arc::clone(&self.shared);
        let stream_connection = Arc::clone(&self.stream_connection);
        let ctrl = Arc::clone(&self.ctrl);
        let thread = std::thread::Builder::new()
            .name("Chiaki Session".to_owned())
            .spawn(move || session_thread_func(shared, stream_connection, ctrl))
            .map_err(|_| ChiakiError::Thread)?;
        *self.session_thread.lock().unwrap_or_else(PoisonError::into_inner) = Some(thread);
        Ok(())
    }

    /// Port von `chiaki_session_stop()`.
    pub fn stop(&mut self) {
        {
            let mut st = self.shared.lock_state();
            st.should_stop = true;
        }
        self.shared.stop_pipe.stop();
        self.shared.state_cond.notify_all();

        // C: chiaki_stream_connection_stop (nicht-blockierend!)
        self.stream_connection.stop();
    }

    /// Port von `chiaki_session_join()`.
    pub fn join(&mut self) -> ChiakiResult<()> {
        let thread = self
            .session_thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        match thread {
            Some(t) => t.join().map_err(|_| ChiakiError::Thread),
            None => Ok(()),
        }
    }

    /// Port von `chiaki_session_fini()`: join + Ctrl abbauen (der Rest
    /// passiert über Drop; idempotent).
    pub fn fini(&mut self) {
        let _ = self.join();
        let ctrl = self
            .ctrl
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(mut ctrl) = ctrl {
            ctrl.stop();
            let _ = ctrl.join();
            ctrl.fini();
        }
    }

    // ------------------------------------------------------------------
    // API-Weiterleitungen (C: chiaki_session_*)
    // ------------------------------------------------------------------

    /// C: `chiaki_session_request_idr()`.
    pub fn request_idr(&self) -> ChiakiResult<()> {
        self.stream_connection.send_idr_request()
    }

    /// C: `chiaki_session_set_controller_state()`.
    pub fn send_controller_state(&self, state: &ControllerState) -> ChiakiResult<()> {
        self.stream_connection.set_controller_state(state);
        Ok(())
    }

    /// C: `chiaki_session_set_login_pin()`: PIN hinterlegen und den
    /// session_thread aufwecken (der leitet an Ctrl weiter).
    pub fn set_login_pin(&self, pin: &[u8]) -> ChiakiResult<()> {
        {
            let mut st = self.shared.lock_state();
            st.login_pin_entered = true;
            st.login_pin = Some(pin.to_vec());
        }
        self.shared.state_cond.notify_all();
        Ok(())
    }

    /// C: `chiaki_session_set_stream_connection_switch_received()`.
    pub fn set_stream_connection_switch_received(&self) -> ChiakiResult<()> {
        {
            let mut st = self.shared.lock_state();
            st.stream_connection_switch_received = true;
        }
        self.shared.state_cond.notify_all();
        Ok(())
    }

    /// C: `chiaki_session_goto_bed()`.
    pub fn goto_bed(&self) -> ChiakiResult<()> {
        self.with_ctrl(|ctrl| ctrl.goto_bed())
    }

    /// C: `chiaki_session_toggle_microphone()`.
    pub fn toggle_microphone(&self, muted: bool) -> ChiakiResult<()> {
        self.with_ctrl(|ctrl| ctrl.toggle_microphone(muted))
    }

    /// C: `chiaki_session_connect_microphone()`.
    pub fn connect_microphone(&self) -> ChiakiResult<()> {
        self.with_ctrl(|ctrl| ctrl.connect_microphone())
    }

    /// C: `chiaki_session_keyboard_set_text()`.
    pub fn keyboard_set_text(&self, text: &str) -> ChiakiResult<()> {
        self.with_ctrl(|ctrl| ctrl.keyboard_set_text(text))
    }

    /// C: `chiaki_session_keyboard_reject()`.
    pub fn keyboard_reject(&self) -> ChiakiResult<()> {
        self.with_ctrl(|ctrl| ctrl.keyboard_reject())
    }

    /// C: `chiaki_session_keyboard_accept()`.
    pub fn keyboard_accept(&self) -> ChiakiResult<()> {
        self.with_ctrl(|ctrl| ctrl.keyboard_accept())
    }

    /// C: `chiaki_session_go_home()`.
    pub fn go_home(&self) -> ChiakiResult<()> {
        self.with_ctrl(|ctrl| ctrl.go_home())
    }

    /// Ctrl-Schnittstelle `cant_display_set` (vertragsgemäße Weiterleitung).
    pub fn cant_display_set(&self, meta: u8, cant: bool) {
        self.with_ctrl(|ctrl| {
            ctrl.cant_display_set(meta, cant);
            Ok(())
        })
        .ok();
    }

    /// Opus-Mikrofonframes an den AudioSender der StreamConnection
    /// (C: AudioSender an session->stream_connection.takion).
    pub fn send_mic_data(&self, buf: &[u8]) -> ChiakiResult<()> {
        self.stream_connection.send_mic_data(buf)
    }

    /// Aktuelles Session-Target (C: session->target).
    pub fn target(&self) -> Target {
        self.shared.target()
    }

    /// RTT der Senkusha-Phase in Mikrosekunden (C: `session->rtt` — der
    /// session_thread hält das `chiaki_senkusha_run`-Out-Ergebnis in
    /// `SessionShared::rtt_us` fest, siehe session_thread-Senkusha-Block).
    /// `None`, solange die Kalibrierung noch keinen Wert geliefert hat
    /// (0 = ungemessen; nach Senkusha-Fehler bleibt der Fallback 1000 µs).
    pub fn rtt_us(&self) -> Option<u64> {
        match self.shared.rtt_us.load(Ordering::SeqCst) {
            0 => None,
            v => Some(v),
        }
    }

    fn with_ctrl<T>(
        &self,
        f: impl FnOnce(&crate::ctrl::Ctrl) -> ChiakiResult<T>,
    ) -> ChiakiResult<T> {
        let guard = self.ctrl.lock().unwrap_or_else(PoisonError::into_inner);
        match &*guard {
            Some(ctrl) => f(ctrl),
            None => Err(ChiakiError::Uninitialized),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.fini();
    }
}

// ----------------------------------------------------------------------
// Session-Thread (C: session_thread_func)
// ----------------------------------------------------------------------

/// Event-Callback des Ctrl-Threads: bildet `CtrlEvent` auf den Session-State
/// und SessionEvents ab (1:1 zu den ctrl.c-Handlern, die im C direkt auf
/// session->... zugreifen).
fn make_ctrl_event_cb(shared: &Arc<SessionShared>) -> Arc<dyn Fn(crate::ctrl::CtrlEvent) + Send + Sync> {
    let shared = Arc::clone(shared);
    Arc::new(move |ev| {
        use crate::ctrl::{CtrlEvent, CtrlQuitReason};
        match ev {
            // ctrl_message_received_session_id (ctrl.c:876-939)
            CtrlEvent::SessionId(id) => {
                {
                    let mut st = shared.lock_state();
                    if st.ctrl_session_id_received {
                        tracing::warn!("Received another Session Id Message");
                        return;
                    }
                    st.ctrl_session_id_received = true;
                }
                *shared.session_id.lock().unwrap_or_else(PoisonError::into_inner) = id;
                tracing::info!("Ctrl received valid Session Id");
                shared.state_cond.notify_all();
            }
            // ctrl_message_received_login_pin_req (ctrl.c:962-983)
            CtrlEvent::LoginPinRequested(_pin_incorrect) => {
                tracing::info!("Ctrl received Login PIN request");
                {
                    let mut st = shared.lock_state();
                    // If receive login pin request after starting session,
                    // quit session as this won't work
                    if st.ctrl_session_id_received {
                        st.quit_reason = QuitReason::CtrlUnknown;
                        st.ctrl_failed = true;
                    } else {
                        st.ctrl_login_pin_requested = true;
                    }
                }
                shared.state_cond.notify_all();
            }
            // ctrl_message_received_login CTRL_LOGIN_STATE_SUCCESS (ctrl.c:1024):
            // ctrl-internes login_pin_requested wird zurückgesetzt — am
            // Session-State ändert sich nichts.
            CtrlEvent::LoginSuccess => {}
            // display_sink.cantdisplay_cb (ctrl.c:995/1005, 1465)
            CtrlEvent::CantDisplay { cant, .. } => {
                shared.send_event(SessionEvent::CantDisplay { cant })
            }
            // chiaki_session_send_event KEYBOARD_OPEN/TEXT_CHANGE/REMOTE_CLOSE
            CtrlEvent::KeyboardOpen(text) => shared.send_event(SessionEvent::KeyboardText(text)),
            CtrlEvent::KeyboardTextChange(text) => {
                shared.send_event(SessionEvent::KeyboardTextChange(text))
            }
            CtrlEvent::KeyboardRemoteClose => shared.send_event(SessionEvent::KeyboardRemoteClose),
            // ctrl.c:1436-1464: Video-Downgrade nach RP-Server-Type.
            CtrlEvent::ServerType { server_type } => {
                tracing::info!("Ctrl got Server Type: {}", server_type);
                let mut video_profile = shared.video_profile.lock().unwrap_or_else(PoisonError::into_inner);
                // regular PS4 doesn't support >= 1080p
                if server_type == 0
                    && shared.video_profile_auto_downgrade
                    && video_profile.height == 1080
                {
                    tracing::info!("1080p was selected but server would not support it. Downgrading.");
                    let fps = if video_profile.max_fps == 60 {
                        VideoFpsPreset::Fps60
                    } else {
                        VideoFpsPreset::Fps30
                    };
                    let mut profile = ConnectVideoProfile::default();
                    crate::video::connect_video_profile_preset(
                        &mut profile,
                        VideoResolutionPreset::Res720p,
                        fps,
                    );
                    *video_profile = profile;
                }
                // PS4 doesn't support anything except h264
                if (server_type == 0 || server_type == 1)
                    && video_profile.codec != crate::error::Codec::H264
                {
                    tracing::info!(
                        "A codec other than H264 was selected but server would not support it. Downgrading."
                    );
                    video_profile.codec = crate::error::Codec::H264;
                }
            }
            // ctrl.c:1361-1366: RP-Prohibit -> cantdisplay(true)
            CtrlEvent::SwitchToStreamConnection => {
                // ctrl_message_received_switch_to_stream_connection (ctrl.c:950)
                {
                    let mut st = shared.lock_state();
                    if !st.stream_connection_switch_received {
                        st.stream_connection_switch_received = true;
                    } else {
                        tracing::info!("Received an extra stream connection switch ACK, ignoring...");
                        return;
                    }
                }
                shared.state_cond.notify_all();
            }
            // ctrl_failed (ctrl.c:309-317)
            CtrlEvent::Quit(reason) => {
                let mut st = shared.lock_state();
                st.quit_reason = match reason {
                    CtrlQuitReason::Unknown => QuitReason::CtrlUnknown,
                    CtrlQuitReason::ConnectFailed => QuitReason::CtrlConnectFailed,
                    CtrlQuitReason::ConnectionRefused => QuitReason::CtrlConnectionRefused,
                };
                st.ctrl_failed = true;
                drop(st);
                shared.state_cond.notify_all();
            }
        }
    })
}

fn session_thread_func(
    shared: Arc<SessionShared>,
    stream_connection: Arc<StreamConnection>,
    ctrl: CtrlHandle,
) {
    // CHECK_STOP(quit): sollte bereits gestoppt sein -> Quit(Stopped).
    if shared.lock_state().should_stop {
        shared.set_quit_reason(QuitReason::Stopped);
        quit(&shared);
        return;
    }

    // --- PSN-Connection (Holepunch/RUDP): Regist ---
    if let Some(holepunch) = &shared.holepunch {
        let regist_result = (|| -> ChiakiResult<RegisteredHost> {
            let info = holepunch.regist_info()?;
            holepunch.regist(
                &info,
                if shared.ps5 { Target::Ps5_1 } else { Target::Ps4_10 },
                &*shared.psn_account_id.lock().unwrap_or_else(PoisonError::into_inner),
                &shared.stop_pipe,
            )
        })();

        match regist_result {
            Ok(host) => {
                // C: regist_cb FINISHED_SUCCESS
                tracing::info!("{} successfully registered for Remote Play", host.server_nickname);
                if shared.auto_regist {
                    shared.send_event(SessionEvent::Regist(Box::new(host.clone())));
                }
                *shared.morning.lock().unwrap_or_else(PoisonError::into_inner) = host.rp_key;
                *shared.regist_key.lock().unwrap_or_else(PoisonError::into_inner) =
                    host.rp_regist_key;
                if !shared.ps5 && !shared.auto_regist {
                    shared
                        .send_event(SessionEvent::NicknameReceived(host.server_nickname.clone()));
                }
                let mut st = shared.lock_state();
                st.psn_regist_succeeded = true;
            }
            Err(e) => {
                // C: regist_cb FINISHED_CANCELED / FINISHED_FAILED
                tracing::info!("PSN regist failed, exiting... ({e})");
                {
                    let mut st = shared.lock_state();
                    st.quit_reason = QuitReason::PsnRegistFailed;
                    st.should_stop = true;
                }
                quit(&shared);
                return;
            }
        }

        if shared.auto_regist {
            tracing::info!("Console auto registered successfully");
            shared.set_quit_reason(QuitReason::Stopped);
            quit(&shared);
            return;
        }
    }

    tracing::info!(
        "Starting session request for {}",
        if shared.ps5 { "PS5" } else { "PS4" }
    );

    // --- Session-Request (mit RP-Version-Nachverhandlung) ---
    let mut server_target = Target::Ps4Unknown;
    let mut err = session_thread_request_session(&shared, Some(&mut server_target));

    if err == Err(ChiakiError::VersionMismatch) && !server_target.is_unknown() {
        tracing::info!("Attempting to re-request session with Server's RP-Version");
        *shared.target.lock().unwrap_or_else(PoisonError::into_inner) = server_target;
        err = session_thread_request_session(&shared, Some(&mut server_target));
    } else if err.is_err() {
        quit(&shared);
        return;
    }

    if err == Err(ChiakiError::VersionMismatch) && !server_target.is_unknown() {
        tracing::info!("Attempting to re-request session even harder with Server's RP-Version!!!");
        *shared.target.lock().unwrap_or_else(PoisonError::into_inner) = server_target;
        err = session_thread_request_session(&shared, None);
    } else if err.is_err() {
        quit(&shared);
        return;
    }

    if let Err(e) = err {
        tracing::error!("Session request failed: {e}");
        quit(&shared);
        return;
    }

    tracing::info!("Session request successful");

    // C: chiaki_rpcrypt_init_auth(&session->rpcrypt, target, nonce, morning)
    {
        let nonce = *shared.nonce.lock().unwrap_or_else(PoisonError::into_inner);
        let morning = *shared.morning.lock().unwrap_or_else(PoisonError::into_inner);
        let rpcrypt = match Rpcrypt::new_auth(shared.target(), &nonce, &morning) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("Session failed to init rpcrypt ({e})");
                quit(&shared);
                return;
            }
        };
        *shared.rpcrypt.lock().unwrap_or_else(PoisonError::into_inner) = Some(rpcrypt);
    }

    // PS4 doesn't always react right away, sleep a bit
    let _ = wait_state_timeout(&shared, 10, |st| st.ctrl_failed);

    tracing::info!("Starting ctrl");

    // C: chiaki_ctrl_init + chiaki_ctrl_start
    let ctrl_obj = match Session::create_ctrl(&shared) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Ctrl init failed ({e})");
            quit(&shared);
            return;
        }
    };
    *ctrl.lock().unwrap_or_else(PoisonError::into_inner) = Some(ctrl_obj);
    let start_result = {
        let mut guard = ctrl.lock().unwrap_or_else(PoisonError::into_inner);
        match guard.as_mut() {
            Some(c) => c.start(),
            None => Err(ChiakiError::Uninitialized),
        }
    };
    if let Err(e) = start_result {
        tracing::error!("Ctrl failed to start ({e})");
        quit(&shared);
        return;
    }

    // Auf Ctrl-Startup warten (Session-Id oder PIN-Request)
    let _ = wait_state_timeout(
        &shared,
        SESSION_EXPECT_CTRL_START_MS,
        |st| {
            st.should_stop
                || st.ctrl_failed
                || st.ctrl_session_id_received
                || st.ctrl_login_pin_requested
        },
    );
    // CHECK_STOP(quit_ctrl) — setzt Quit-Reason Stopped
    if check_stop(&shared) {
        stop_and_join_ctrl(&ctrl);
        quit(&shared);
        return;
    }

    if shared.lock_state().ctrl_failed {
        tracing::error!("Ctrl has failed while waiting for ctrl startup");
        ctrl_failed(&shared, &ctrl);
        return;
    }

    // --- Login-PIN-Loop ---
    let mut pin_incorrect = false;
    loop {
        if !shared.lock_state().ctrl_login_pin_requested {
            break;
        }
        shared.lock_state().ctrl_login_pin_requested = false;
        if pin_incorrect {
            tracing::info!("Login PIN was incorrect, requested again by Ctrl");
        } else {
            tracing::info!("Ctrl requested Login PIN");
        }
        shared.send_event(SessionEvent::LoginPinRequest(pin_incorrect));
        pin_incorrect = true;

        // Auf PIN-Eingabe warten
        let _ = wait_state_timeout(&shared, u64::MAX, |st| {
            st.should_stop || st.ctrl_failed || st.login_pin_entered
        });

        {
            let st = shared.lock_state();
            if st.should_stop {
                drop(st);
                stop_and_join_ctrl(&ctrl);
                quit(&shared);
                return;
            }
            if st.ctrl_failed {
                tracing::error!("Ctrl has failed while waiting for PIN entry");
                drop(st);
                ctrl_failed(&shared, &ctrl);
                return;
            }
        }

        let pin = {
            let mut st = shared.lock_state();
            let pin = st.login_pin.take();
            st.login_pin_entered = false;
            pin
        };
        match pin {
            Some(pin) => {
                tracing::info!("Session received entered Login PIN, forwarding to Ctrl");
                if let Some(c) = ctrl.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
                    c.set_login_pin(&pin);
                }
            }
            None => {
                tracing::error!("Login PIN was entered flag set but no pin present");
                ctrl_failed(&shared, &ctrl);
                return;
            }
        }

        // auf session id oder neuen PIN-Request warten
        let _ = wait_state_timeout(
            &shared,
            SESSION_EXPECT_CTRL_START_MS,
            |st| {
                st.should_stop
                    || st.ctrl_failed
                    || st.ctrl_session_id_received
                    || st.ctrl_login_pin_requested
            },
        );
        if check_stop(&shared) {
            stop_and_join_ctrl(&ctrl);
            quit(&shared);
            return;
        }
    }

    // --- Holepunch: Datenverbindung aufstöbern ---
    let mut data_sock: Option<UdpSocket> = None;
    if let Some(holepunch) = &shared.holepunch {
        if let Err(e) = holepunch.create_offer(HolepunchPortType::Data) {
            tracing::error!("!! Failed to create offer msg for data connection ({e})");
        }
        tracing::info!("Punching hole for data connection");
        shared.send_event(SessionEvent::Holepunch { finished: false });
        if let Err(e) = holepunch.punch_hole(HolepunchPortType::Data) {
            tracing::error!("!! Failed to punch hole for data connection. ({e})");
            stop_and_join_ctrl(&ctrl);
            quit(&shared);
            return;
        }
        tracing::info!(">> Punched hole for data connection!");
        data_sock = holepunch.sock(HolepunchPortType::Data);
        shared.send_event(SessionEvent::Holepunch { finished: true });
        let _ = wait_state_timeout(
            &shared,
            SESSION_EXPECT_TIMEOUT_MS,
            |st| {
                st.should_stop
                    || st.ctrl_failed
                    || st.ctrl_session_id_received
                    || st.ctrl_login_pin_requested
            },
        );
        if check_stop(&shared) {
            stop_and_join_ctrl(&ctrl);
            quit(&shared);
            return;
        }
    }

    // --- Fallback-Session-Id, falls Ctrl keine geliefert hat ---
    {
        let received = shared.lock_state().ctrl_session_id_received;
        if !received {
            tracing::error!("Ctrl did not receive session id");
            let result = {
                let guard = ctrl.lock().unwrap_or_else(PoisonError::into_inner);
                match guard.as_ref() {
                    Some(c) => c.set_fallback_session_id().and_then(|id| {
                        // Session-Id + Flag setzen (das Ctrl-Event macht das
                        // ebenfalls; hier idempotent nachziehen) ...
                        let mut st = shared.lock_state();
                        *shared.session_id.lock().unwrap_or_else(PoisonError::into_inner) = id;
                        st.ctrl_session_id_received = true;
                        drop(st);
                        shared.state_cond.notify_all();
                        c.enable_features()
                    }),
                    None => Err(ChiakiError::Uninitialized),
                }
            };
            if result.is_err() {
                ctrl_failed(&shared, &ctrl);
                return;
            }
        }

        if !shared.lock_state().ctrl_session_id_received {
            tracing::error!("Ctrl has failed, shutting down");
            {
                let mut st = shared.lock_state();
                if st.quit_reason == QuitReason::None {
                    st.quit_reason = QuitReason::CtrlUnknown;
                }
            }
            stop_and_join_ctrl(&ctrl);
            quit(&shared);
            return;
        }
    }

    // --- Senkusha (MTU/RTT) ---
    {
        tracing::info!("Starting Senkusha");

        let mut senkusha = crate::senkusha::Senkusha::new();
        let host = shared.host_addr.lock().unwrap_or_else(PoisonError::into_inner);
        let mut mtu_in = shared.mtu_in.load(Ordering::SeqCst);
        let mut mtu_out = shared.mtu_out.load(Ordering::SeqCst);
        let mut rtt_us = shared.rtt_us.load(Ordering::SeqCst);
        let run_result = match *host {
            Some(host) => senkusha.run(
                &crate::senkusha::SenkushaConnectInfo {
                    host,
                    enable_dualsense: shared.enable_dualsense,
                },
                &mut mtu_in,
                &mut mtu_out,
                &mut rtt_us,
                data_sock.take(),
            ),
            None => Err(ChiakiError::Uninitialized),
        };
        drop(host);

        if shared.lock_state().should_stop {
            stop_and_join_ctrl(&ctrl);
            quit(&shared);
            return;
        }
        if shared.lock_state().ctrl_failed {
            tracing::error!("Ctrl has failed since session started, exiting");
            stop_and_join_ctrl(&ctrl);
            quit(&shared);
            return;
        }

        match run_result {
            Ok(()) => {
                tracing::info!("Senkusha completed successfully");
                shared.mtu_in.store(mtu_in, Ordering::SeqCst);
                shared.mtu_out.store(mtu_out, Ordering::SeqCst);
                shared.rtt_us.store(rtt_us, Ordering::SeqCst);
            }
            Err(ChiakiError::Canceled) => {
                stop_and_join_ctrl(&ctrl);
                quit(&shared);
                return;
            }
            Err(e) => {
                tracing::error!(
                    "Senkusha failed, but we still try to connect with fallback values ({e})"
                );
                shared.mtu_in.store(1454, Ordering::SeqCst);
                shared.mtu_out.store(1454, Ordering::SeqCst);
                shared.rtt_us.store(1000, Ordering::SeqCst);
                shared.dontfrag.store(false, Ordering::SeqCst);
            }
        }
    }

    // --- RUDP: auf Stream-Connection umschalten (PSN-Pfad) ---
    if let Some(holepunch) = &shared.holepunch {
        if let Err(e) = holepunch.rudp_send_switch_to_stream_connection() {
            tracing::error!("Failed to send switch to stream connection message ({e})");
            stop_and_join_ctrl(&ctrl);
            quit(&shared);
            return;
        }
        let _ = wait_state_timeout(&shared, SESSION_EXPECT_TIMEOUT_MS, |st| {
            st.should_stop || st.ctrl_failed || st.stream_connection_switch_received
        });
        let (switched, stopped) = {
            let st = shared.lock_state();
            (st.stream_connection_switch_received, st.should_stop)
        };
        if !switched {
            tracing::error!("Failed to receive switch to stream connection ack!");
            stop_and_join_ctrl(&ctrl);
            quit(&shared);
            return;
        }
        if stopped {
            stop_and_join_ctrl(&ctrl);
            quit(&shared);
            return;
        }
        tracing::info!(
            "Received Switch to Stream Connection Ack... Switching to Stream Connection now"
        );
    }

    // --- Handshake-Key + ECDH ---
    {
        let mut key = [0u8; HANDSHAKE_KEY_SIZE];
        if let Err(e) = crate::random::random_bytes_crypt(&mut key) {
            tracing::error!("Session failed to generate handshake key ({e})");
            stop_and_join_ctrl(&ctrl);
            quit(&shared);
            return;
        }
        *shared.handshake_key.lock().unwrap_or_else(PoisonError::into_inner) = key;

        match Ecdh::new() {
            Ok(ecdh) => *shared.ecdh.lock().unwrap_or_else(PoisonError::into_inner) = Some(ecdh),
            Err(e) => {
                tracing::error!("Session failed to initialize ECDH ({e})");
                stop_and_join_ctrl(&ctrl);
                quit(&shared);
                return;
            }
        }
    }

    // --- StreamConnection (state_mutex dabei nicht gehalten, wie im C) ---
    let sc_result = stream_connection.run(data_sock);

    match sc_result {
        Err(ChiakiError::Disconnected) => {
            tracing::error!("Remote disconnected from StreamConnection");
            let reason = stream_connection
                .remote_disconnect_reason()
                .unwrap_or_default();
            let mut st = shared.lock_state();
            if reason == "Server shutting down" {
                st.quit_reason = QuitReason::StreamConnectionRemoteShutdown;
            } else {
                st.quit_reason = QuitReason::StreamConnectionRemoteDisconnected;
            }
            st.quit_reason_str = Some(reason);
        }
        Err(e) if e != ChiakiError::Canceled => {
            tracing::error!("StreamConnection run failed");
            shared.set_quit_reason(QuitReason::StreamConnectionUnknown);
        }
        _ => {
            tracing::info!("StreamConnection completed successfully");
            shared.set_quit_reason(QuitReason::Stopped);
        }
    }

    stop_and_join_ctrl(&ctrl);
    quit(&shared);
}

/// C: CHECK_STOP — liefert true, wenn gestoppt werden soll (und setzt den
/// Quit-Reason auf Stopped, wie im C-Makro).
fn check_stop(shared: &Arc<SessionShared>) -> bool {
    let mut st = shared.lock_state();
    if st.should_stop {
        st.quit_reason = QuitReason::Stopped;
        return true;
    }
    false
}

/// C: `ctrl_failed()` + quit_ctrl — Ctrl stoppen+joinen und Quit feuern.
fn ctrl_failed(shared: &Arc<SessionShared>, ctrl: &CtrlHandle) {
    tracing::error!("Ctrl has failed, shutting down");
    {
        let mut st = shared.lock_state();
        if st.quit_reason == QuitReason::None {
            st.quit_reason = QuitReason::CtrlUnknown;
        }
    }
    stop_and_join_ctrl(ctrl);
    quit(shared);
}

/// C: quit_ctrl-Label — Ctrl stoppen + joinen.
fn stop_and_join_ctrl(ctrl: &CtrlHandle) {
    {
        let guard = ctrl.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(c) = guard.as_ref() {
            c.stop();
        }
    }
    let taken = {
        let mut guard = ctrl.lock().unwrap_or_else(PoisonError::into_inner);
        guard.take()
    };
    if let Some(mut c) = taken {
        let _ = c.join();
        tracing::info!("Ctrl stopped");
        c.fini();
    }
}

/// C: quit-Label — Quit-Event feuern.
fn quit(shared: &Arc<SessionShared>) {
    tracing::info!("Session has quit");
    let (reason, reason_str) = {
        let st = shared.lock_state();
        (
            st.quit_reason,
            st.quit_reason_str.clone().unwrap_or_default(),
        )
    };
    shared.send_event(SessionEvent::Quit { reason, reason_str });
}

/// Wartet bis zu `timeout_ms` auf ein Prädikat über dem Session-State
/// (C: chiaki_cond_timedwait_pred; should_stop erfüllt — wie im C — alle
/// Prädikate). `u64::MAX` = unbegrenzt (quantisiert, damit stop() wirkt).
fn wait_state_timeout(
    shared: &Arc<SessionShared>,
    timeout_ms: u64,
    pred: impl Fn(&SessionState) -> bool,
) -> ChiakiResult<()> {
    const QUANTUM: Duration = Duration::from_millis(200);
    let deadline = if timeout_ms == u64::MAX {
        None
    } else {
        Some(Instant::now() + Duration::from_millis(timeout_ms))
    };
    let mut guard = shared.lock_state();
    loop {
        if guard.should_stop || pred(&guard) {
            return Ok(());
        }
        let now = Instant::now();
        match deadline {
            None => {
                let (g, _) = shared
                    .state_cond
                    .wait_timeout(guard, QUANTUM)
                    .unwrap_or_else(PoisonError::into_inner);
                guard = g;
            }
            Some(d) => {
                if now >= d {
                    return Err(ChiakiError::Timeout);
                }
                let (g, _) = shared
                    .state_cond
                    .wait_timeout(guard, QUANTUM.min(d - now))
                    .unwrap_or_else(PoisonError::into_inner);
                guard = g;
            }
        }
    }
}

// ----------------------------------------------------------------------
// Session-Request (C: session_thread_request_session)
// ----------------------------------------------------------------------

/// C: `struct session_response_t`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SessionResponse {
    pub error_code: u32,
    pub nonce: Option<String>,
    pub rp_version: Option<String>,
    pub success: bool,
}

/// Port von `parse_session_response()`.
///
/// `RP-Nonce` wird case-sensitiv verglichen, `RP-Version` case-insensitiv
/// (C: strcasecmp); `RP-Application-Reason` wird hex-dekodiert (strtoul
/// base 16; ungültig => 0).
pub fn parse_session_response(http_response: &http::HttpResponse) -> SessionResponse {
    let mut response = SessionResponse::default();

    for header in &http_response.headers {
        if header.key == "RP-Nonce" {
            response.nonce = Some(header.value.clone());
        } else if header.key.eq_ignore_ascii_case("rp-version") {
            response.rp_version = Some(header.value.clone());
        } else if header.key == "RP-Application-Reason" {
            response.error_code = strtoul_hex(&header.value);
        }
    }

    response.success = http_response.code == 200 && response.nonce.is_some();
    response
}

/// `strtoul(s, NULL, 0x10)`-Semantik (Whitespace/0x-Präfix erlaubt,
/// ungültige Eingabe => 0, saturierend).
fn strtoul_hex(s: &str) -> u32 {
    let s = s.trim();
    let s = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    let mut val: u64 = 0;
    for c in s.chars() {
        let d = match c.to_digit(16) {
            Some(d) => d as u64,
            None => break,
        };
        val = val.saturating_mul(16).saturating_add(d);
        if val > u32::MAX as u64 {
            val = u32::MAX as u64;
        }
    }
    val as u32
}

/// C: `format_hex()` (utils.h) für den Regist-Key (lowercase hex).
pub fn format_regist_key_hex(regist_key: &[u8]) -> String {
    let mut out = String::with_capacity(regist_key.len() * 2);
    for b in regist_key {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((b & 0xf) as u32, 16).unwrap_or('0'));
    }
    out
}

/// C: Pfadwahl des Session-Requests.
pub fn session_request_path(target: Target) -> &'static str {
    if target == Target::Ps4_8 || target == Target::Ps4_9 {
        "/sce/rp/session"
    } else if target.is_ps5() {
        "/sie/ps5/rp/sess/init"
    } else {
        "/sie/ps4/rp/sess/init"
    }
}

/// Baut den HTTP-Request-Header exakt wie `session_request_fmt` im C:
///
/// ```text
/// GET %s HTTP/1.1\r\n
/// Host: %s:%d\r\n
/// User-Agent: remoteplay Windows\r\n
/// Connection: close\r\n
/// Content-Length: 0\r\n
/// RP-Registkey: %s\r\n
/// Rp-Version: %s\r\n
/// \r\n
/// ```
pub fn build_session_request(
    path: &str,
    hostname: &str,
    port: u16,
    regist_key_hex: &str,
    rp_version: &str,
) -> String {
    format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {hostname}:{port}\r\n\
         User-Agent: remoteplay Windows\r\n\
         Connection: close\r\n\
         Content-Length: 0\r\n\
         RP-Registkey: {regist_key_hex}\r\n\
         Rp-Version: {rp_version}\r\n\
         \r\n"
    )
}

/// C: getaddrinfo-Hints in `chiaki_session_init`: IPv4, außer der Host
/// enthält ':' (dann IPv6).
fn resolve_host_addrs(host: &str) -> std::io::Result<Vec<SocketAddr>> {
    let want_v6 = host.contains(':');
    let mut v4 = Vec::new();
    let mut v6 = Vec::new();
    for addr in (host, 0u16).to_socket_addrs()? {
        if addr.is_ipv6() {
            v6.push(addr);
        } else {
            v4.push(addr);
        }
    }
    Ok(if want_v6 { v6 } else { v4 })
}

/// Nicht-blockierender, per StopPipe abbrechbarer TCP-Connect
/// (C: chiaki_stop_pipe_connect). Quantisiert (100 ms), damit stop()
/// zeitnah reagiert.
fn stop_pipe_connect(
    stop_pipe: &StopPipe,
    addr: SocketAddr,
    timeout: Duration,
) -> ChiakiResult<TcpStream> {
    const QUANTUM: Duration = Duration::from_millis(100);
    let deadline = Instant::now() + timeout;
    loop {
        stop_pipe.check()?; // ChiakiError::Canceled
        let now = Instant::now();
        if now >= deadline {
            return Err(ChiakiError::Timeout);
        }
        let remaining = deadline - now;
        match TcpStream::connect_timeout(&addr, remaining.min(QUANTUM)) {
            Ok(sock) => return Ok(sock),
            Err(e)
                if e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                return Err(ChiakiError::ConnectionRefused)
            }
            Err(e) => return Err(crate::sock::map_io_error(&e)),
        }
    }
}

/// C: `chiaki_send_fully` — vollständig senden, stoppbar (quantisiert).
fn send_fully(
    stop_pipe: &StopPipe,
    sock: &mut TcpStream,
    mut buf: &[u8],
    timeout_ms: u64,
) -> ChiakiResult<()> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    sock.set_write_timeout(Some(Duration::from_millis(50)))
        .map_err(|_| ChiakiError::Network)?;
    while !buf.is_empty() {
        stop_pipe.check()?;
        if Instant::now() >= deadline {
            return Err(ChiakiError::Timeout);
        }
        match sock.write(buf) {
            Ok(0) => return Err(ChiakiError::Disconnected),
            Ok(n) => buf = &buf[n..],
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(e) => return Err(crate::sock::map_io_error(&e)),
        }
    }
    Ok(())
}

/// Port von `session_thread_request_session()`.
///
/// @param target_out wenn `None`, bedeutet Version-Mismatch das Scheitern
/// der gesamten Session, sonst wird der Server-Target hier reportet.
fn session_thread_request_session(
    shared: &Arc<SessionShared>,
    target_out: Option<&mut Target>,
) -> ChiakiResult<()> {
    let mut remote_counter = 0u16;
    let holepunch = shared.holepunch.clone();

    // --- Verbindung aufbauen ---
    let mut session_sock: Option<TcpStream> = None;
    if let Some(holepunch) = &holepunch {
        tracing::info!("SESSION START THREAD - Starting RUDP session");
        remote_counter = holepunch.rudp_start_session().map_err(|e| {
            tracing::error!("SESSION START THREAD - Failed to init rudp");
            e
        })?;
    } else {
        let addrs = shared
            .host_addrs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        for mut addr in addrs {
            addr.set_port(SESSION_PORT);

            // C: getnameinfo(NI_NUMERICHOST) -> hostname für Logs + Host-Header
            let hostname = addr.ip().to_string();
            *shared.hostname.lock().unwrap_or_else(PoisonError::into_inner) = hostname.clone();

            tracing::info!("Trying to request session from {}:{}", hostname, SESSION_PORT);

            match stop_pipe_connect(&shared.stop_pipe, addr, Duration::from_millis(5000)) {
                Ok(sock) => {
                    *shared.host_addr.lock().unwrap_or_else(PoisonError::into_inner) = Some(addr);
                    session_sock = Some(sock);
                    break;
                }
                Err(ChiakiError::Canceled) => {
                    tracing::info!("Session stopped while connecting for session request");
                    shared.set_quit_reason(QuitReason::Stopped);
                    break;
                }
                Err(e) => {
                    tracing::error!("Session request connect failed: {}", e.as_str());
                    if e == ChiakiError::ConnectionRefused {
                        shared.set_quit_reason(QuitReason::SessionRequestConnectionRefused);
                    } else {
                        shared.set_quit_reason(QuitReason::None);
                    }
                    continue;
                }
            }
        }

        if session_sock.is_none() {
            tracing::error!("Session request connect failed eventually.");
            if shared.quit_reason() == QuitReason::None {
                shared.set_quit_reason(QuitReason::SessionRequestUnknown);
            }
            return Err(ChiakiError::Network);
        }
        tracing::info!(
            "Connected to {}:{}",
            shared.hostname.lock().unwrap_or_else(PoisonError::into_inner),
            SESSION_PORT
        );
    }

    // --- Request bauen ---
    let target = shared.target();
    let path = session_request_path(target);

    let regist_key_hex = {
        let regist_key = *shared.regist_key.lock().unwrap_or_else(PoisonError::into_inner);
        let len = regist_key
            .iter()
            .position(|b| *b == 0)
            .unwrap_or(regist_key.len());
        format_regist_key_hex(&regist_key[..len])
    };

    let rp_version_str = match rp_version_string(target) {
        Some(v) => v,
        None => {
            tracing::error!("Failed to get version for target, probably invalid target value");
            shared.set_quit_reason(QuitReason::SessionRequestUnknown);
            return Err(ChiakiError::InvalidData);
        }
    };

    let (hostname, port) = match &holepunch {
        Some(hp) => {
            let hostname = hp.ps_selected_addr();
            // C (session.c:939): chiaki_get_ps_selected_addr schreibt in
            // connect_info.hostname — das Ctrl nutzt denselben Host-Namen
            // für den HTTP Host-Header (ctrl.c:1302), also hier nachziehen.
            *shared.hostname.lock().unwrap_or_else(PoisonError::into_inner) = hostname.clone();
            (hostname, hp.ps_ctrl_port())
        }
        None => (
            shared
                .hostname
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
            SESSION_PORT,
        ),
    };

    let request = build_session_request(path, &hostname, port, &regist_key_hex, rp_version_str);

    tracing::info!("Sending session request");
    tracing::trace!("Session request:\n{}", request);

    // --- Senden + Header empfangen (C: send_fully + recv_http_header bzw.
    //     rudp-Varianten) ---
    let header_buf: Vec<u8>;
    let header_size: usize;
    match (&holepunch, session_sock.as_mut()) {
        (None, Some(sock)) => {
            if let Err(e) =
                send_fully(&shared.stop_pipe, sock, request.as_bytes(), SESSION_EXPECT_TIMEOUT_MS)
            {
                if e == ChiakiError::Canceled {
                    tracing::info!("Session stopped while sending session request");
                } else {
                    tracing::error!("Failed to send session request");
                }
                shared.set_quit_reason(if e == ChiakiError::Canceled {
                    QuitReason::Stopped
                } else {
                    QuitReason::SessionRequestUnknown
                });
                return Err(e);
            }

            let mut buf = [0u8; 512];
            match http::recv_http_header(
                sock,
                &mut buf,
                Some(&shared.stop_pipe),
                SESSION_EXPECT_TIMEOUT_MS,
            ) {
                Ok((h, r)) => {
                    header_size = h;
                    header_buf = buf[..r].to_vec();
                }
                Err(e) => {
                    if e == ChiakiError::Canceled {
                        shared.set_quit_reason(QuitReason::Stopped);
                    } else {
                        tracing::error!("Failed to receive session request response");
                        shared.set_quit_reason(QuitReason::SessionRequestUnknown);
                    }
                    return Err(ChiakiError::Network);
                }
            }
        }
        (Some(hp), _) => {
            let mut buf = vec![0u8; 512];
            match hp.rudp_send_recv_http_header(request.as_bytes(), remote_counter, &mut buf) {
                Ok((h, r, new_remote_counter)) => {
                    header_size = h;
                    header_buf = buf[..r].to_vec();
                    // C: chiaki_send_recv_http_header_psn aktualisiert
                    // *remote_counter — der ACK/FINISH nutzt den neuen Wert.
                    remote_counter = new_remote_counter;
                }
                Err(e) => {
                    if e == ChiakiError::Canceled {
                        shared.set_quit_reason(QuitReason::Stopped);
                    } else {
                        tracing::error!("Failed to receive session request response");
                        shared.set_quit_reason(QuitReason::SessionRequestUnknown);
                    }
                    return Err(ChiakiError::Network);
                }
            }
        }
        _ => return Err(ChiakiError::Network),
    }

    // C: rudp ACK/FINISH (Fehler nur warnen)
    if let Some(hp) = &holepunch {
        if let Err(e) = hp.rudp_finish(remote_counter) {
            tracing::warn!("SESSION START THREAD - Failed to finish rudp, continuing... ({e})");
        }
    }

    // --- Antwort parsen (C: chiaki_http_response_parse) ---
    tracing::trace!("Session Response Header:\n{}", String::from_utf8_lossy(&header_buf[..header_size]));
    let http_response = match http::response_parse(&header_buf[..header_size]) {
        Ok(r) => r,
        Err(_) => {
            tracing::error!("Failed to parse session request response");
            shared.set_quit_reason(QuitReason::SessionRequestUnknown);
            return Err(ChiakiError::Network);
        }
    };

    let response = parse_session_response(&http_response);

    // C: `ChiakiErrorCode r = CHIAKI_ERR_UNKNOWN;` + Verzweigung
    let result;
    if response.success {
        let nonce = base64::decode(response.nonce.as_deref().unwrap_or("").as_bytes());
        match nonce {
            Ok(n) if n.len() == RPCRYPT_KEY_SIZE => {
                let mut key = [0u8; RPCRYPT_KEY_SIZE];
                key.copy_from_slice(&n);
                *shared.nonce.lock().unwrap_or_else(PoisonError::into_inner) = key;
                result = Ok(());
            }
            _ => {
                tracing::error!("Nonce invalid");
                shared.set_quit_reason(QuitReason::SessionRequestUnknown);
                result = Err(ChiakiError::Unknown);
            }
        }
    } else if (response.error_code == RP_APPLICATION_REASON_RP_VERSION
        || response.error_code == RP_APPLICATION_REASON_UNKNOWN)
        && target_out.is_some()
        && response.rp_version.is_some()
        && response.rp_version.as_deref() != Some(rp_version_str)
    {
        let server_rp_version = response.rp_version.as_deref().unwrap_or("");
        tracing::info!(
            "Reported RP-Version mismatch. ours = {}, server = {}",
            rp_version_str,
            server_rp_version
        );
        let parsed = rp_version_parse(server_rp_version, shared.ps5);
        let target_out = target_out.unwrap();
        if !parsed.is_unknown() {
            tracing::info!(
                "Detected Server RP-Version {}",
                rp_version_string(parsed).unwrap_or("")
            );
            *target_out = parsed;
        } else if server_rp_version == "5.0" {
            tracing::info!(
                "Reported Server RP-Version is 5.0. This is probably nonsense, let's try with 9.0"
            );
            *target_out = Target::Ps4_9;
        } else {
            tracing::error!("Server RP-Version is unknown");
            shared.set_quit_reason(QuitReason::SessionRequestRpVersionMismatch);
        }
        result = Err(ChiakiError::VersionMismatch);
    } else {
        tracing::error!(
            "Reported Application Reason: {:#x} ({})",
            response.error_code,
            rp_application_reason_string(response.error_code)
        );
        match response.error_code {
            RP_APPLICATION_REASON_IN_USE => {
                shared.set_quit_reason(QuitReason::SessionRequestRpInUse)
            }
            RP_APPLICATION_REASON_CRASH => {
                shared.set_quit_reason(QuitReason::SessionRequestRpCrash)
            }
            RP_APPLICATION_REASON_RP_VERSION => {
                shared.set_quit_reason(QuitReason::SessionRequestRpVersionMismatch);
                result = Err(ChiakiError::VersionMismatch);
                return result;
            }
            _ => shared.set_quit_reason(QuitReason::SessionRequestUnknown),
        }
        result = Err(ChiakiError::Unknown);
    }

    // Socket wird per Drop geschlossen (C: CHIAKI_SOCKET_CLOSE).
    result
}

// ----------------------------------------------------------------------
// Tests: Formatter/Parser/State-Logik, die ohne Console isolierbar sind.
// ----------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rp_version_string_matches_c() {
        assert_eq!(rp_version_string(Target::Ps4_8), Some("8.0"));
        assert_eq!(rp_version_string(Target::Ps4_9), Some("9.0"));
        assert_eq!(rp_version_string(Target::Ps4_10), Some("10.0"));
        assert_eq!(rp_version_string(Target::Ps5_1), Some("1.0"));
        assert_eq!(rp_version_string(Target::Ps4Unknown), None);
        assert_eq!(rp_version_string(Target::Ps5Unknown), None);
    }

    #[test]
    fn rp_version_parse_matches_c() {
        assert_eq!(rp_version_parse("1.0", true), Target::Ps5_1);
        assert_eq!(rp_version_parse("2.0", true), Target::Ps5Unknown);
        assert_eq!(rp_version_parse("8.0", false), Target::Ps4_8);
        assert_eq!(rp_version_parse("9.0", false), Target::Ps4_9);
        assert_eq!(rp_version_parse("10.0", false), Target::Ps4_10);
        assert_eq!(rp_version_parse("11.0", false), Target::Ps4Unknown);
    }

    #[test]
    fn rp_application_reason_values_and_strings() {
        assert_eq!(RP_APPLICATION_REASON_REGIST_FAILED, 0x8010_8b09);
        assert_eq!(RP_APPLICATION_REASON_INVALID_PSN_ID, 0x8010_8b02);
        assert_eq!(RP_APPLICATION_REASON_IN_USE, 0x8010_8b10);
        assert_eq!(RP_APPLICATION_REASON_CRASH, 0x8010_8b15);
        assert_eq!(RP_APPLICATION_REASON_RP_VERSION, 0x8010_8b11);
        assert_eq!(RP_APPLICATION_REASON_UNKNOWN, 0x8010_8bff);

        assert_eq!(
            rp_application_reason_string(RP_APPLICATION_REASON_REGIST_FAILED),
            "Regist failed, probably invalid PIN"
        );
        assert_eq!(
            rp_application_reason_string(RP_APPLICATION_REASON_INVALID_PSN_ID),
            "Invalid PSN ID"
        );
        assert_eq!(
            rp_application_reason_string(RP_APPLICATION_REASON_IN_USE),
            "Remote is already in use"
        );
        assert_eq!(
            rp_application_reason_string(RP_APPLICATION_REASON_CRASH),
            "Remote Play on Console crashed"
        );
        assert_eq!(
            rp_application_reason_string(RP_APPLICATION_REASON_RP_VERSION),
            "RP-Version mismatch"
        );
        assert_eq!(rp_application_reason_string(0x1234), "unknown");
    }

    #[test]
    fn quit_reason_strings_match_c() {
        assert_eq!(quit_reason_string(QuitReason::None), "Unknown");
        assert_eq!(quit_reason_string(QuitReason::Stopped), "Stopped");
        assert_eq!(
            quit_reason_string(QuitReason::SessionRequestUnknown),
            "Unknown Session Request Error"
        );
        assert_eq!(
            quit_reason_string(QuitReason::SessionRequestConnectionRefused),
            "Connection Refused in Session Request"
        );
        assert_eq!(
            quit_reason_string(QuitReason::SessionRequestRpInUse),
            "Remote Play on Console is already in use"
        );
        assert_eq!(
            quit_reason_string(QuitReason::SessionRequestRpCrash),
            "Remote Play on Console has crashed"
        );
        assert_eq!(
            quit_reason_string(QuitReason::SessionRequestRpVersionMismatch),
            "RP-Version mismatch"
        );
        assert_eq!(quit_reason_string(QuitReason::CtrlUnknown), "Unknown Ctrl Error");
        assert_eq!(
            quit_reason_string(QuitReason::CtrlConnectionRefused),
            "Connection Refused in Ctrl"
        );
        assert_eq!(
            quit_reason_string(QuitReason::CtrlConnectFailed),
            "Ctrl failed to connect"
        );
        assert_eq!(
            quit_reason_string(QuitReason::StreamConnectionUnknown),
            "Unknown Error in Stream Connection"
        );
        assert_eq!(
            quit_reason_string(QuitReason::StreamConnectionRemoteDisconnected),
            "Remote has disconnected from Stream Connection"
        );
        assert_eq!(
            quit_reason_string(QuitReason::StreamConnectionRemoteShutdown),
            "Remote has disconnected from Stream Connection the because Server shut down"
        );
        assert_eq!(
            quit_reason_string(QuitReason::PsnRegistFailed),
            "The Console Registration using PSN has failed"
        );
    }

    #[test]
    fn quit_reason_is_error_matches_c() {
        assert!(!quit_reason_is_error(QuitReason::Stopped));
        assert!(!quit_reason_is_error(QuitReason::StreamConnectionRemoteShutdown));
        for reason in [
            QuitReason::None,
            QuitReason::SessionRequestUnknown,
            QuitReason::SessionRequestConnectionRefused,
            QuitReason::SessionRequestRpInUse,
            QuitReason::SessionRequestRpCrash,
            QuitReason::SessionRequestRpVersionMismatch,
            QuitReason::CtrlUnknown,
            QuitReason::CtrlConnectFailed,
            QuitReason::CtrlConnectionRefused,
            QuitReason::StreamConnectionUnknown,
            QuitReason::StreamConnectionRemoteDisconnected,
            QuitReason::PsnRegistFailed,
        ] {
            assert!(quit_reason_is_error(reason), "{reason:?}");
        }
    }

    #[test]
    fn session_request_paths_match_c() {
        assert_eq!(session_request_path(Target::Ps4_8), "/sce/rp/session");
        assert_eq!(session_request_path(Target::Ps4_9), "/sce/rp/session");
        assert_eq!(session_request_path(Target::Ps5_1), "/sie/ps5/rp/sess/init");
        assert_eq!(session_request_path(Target::Ps5Unknown), "/sie/ps5/rp/sess/init");
        assert_eq!(session_request_path(Target::Ps4_10), "/sie/ps4/rp/sess/init");
        assert_eq!(session_request_path(Target::Ps4Unknown), "/sie/ps4/rp/sess/init");
    }

    /// Golden-Bytes des HTTP-Session-Requests (session_request_fmt).
    #[test]
    fn session_request_header_golden() {
        let req = build_session_request(
            "/sie/ps5/rp/sess/init",
            "192.168.1.12",
            9295,
            "aabbccdd00112233",
            "1.0",
        );
        assert_eq!(
            req,
            "GET /sie/ps5/rp/sess/init HTTP/1.1\r\n\
             Host: 192.168.1.12:9295\r\n\
             User-Agent: remoteplay Windows\r\n\
             Connection: close\r\n\
             Content-Length: 0\r\n\
             RP-Registkey: aabbccdd00112233\r\n\
             Rp-Version: 1.0\r\n\
             \r\n"
        );

        // PS4-Pre-10-Pfad
        let req = build_session_request(
            session_request_path(Target::Ps4_9),
            "10.0.0.7",
            9295,
            "00ff",
            "9.0",
        );
        assert!(req.starts_with("GET /sce/rp/session HTTP/1.1\r\nHost: 10.0.0.7:9295\r\n"));
    }

    #[test]
    fn format_regist_key_hex_is_lowercase_and_stops_at_nul() {
        let mut key = [0u8; SESSION_AUTH_SIZE];
        key[..6].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef, 0x01, 0x0f]);
        let len = key.iter().position(|b| *b == 0).unwrap_or(key.len());
        assert_eq!(format_regist_key_hex(&key[..len]), "deadbeef010f");
        // Komplett gefüllter Key (kein NUL)
        let full = [0xab; SESSION_AUTH_SIZE];
        assert_eq!(
            format_regist_key_hex(&full),
            "ab".repeat(SESSION_AUTH_SIZE)
        );
    }

    #[test]
    fn parse_session_response_headers() {
        let resp = http::HttpResponse {
            code: 200,
            headers: vec![
                http::HttpHeader {
                    key: "RP-Nonce".to_owned(),
                    value: "dGhpcyBpcyBhIG5vbmNlIQ==".to_owned(),
                },
                http::HttpHeader {
                    key: "rp-version".to_owned(),
                    value: "9.0".to_owned(), // case-insensitiver Key (C: strcasecmp)
                },
            ],
        };
        let parsed = parse_session_response(&resp);
        assert!(parsed.success);
        assert_eq!(
            parsed.nonce.as_deref(),
            Some("dGhpcyBpcyBhIG5vbmNlIQ==")
        );
        assert_eq!(parsed.rp_version.as_deref(), Some("9.0"));
        assert_eq!(parsed.error_code, 0);

        // RP-Nonce case-sensitiv (C: strcmp) -> kein Success
        let resp = http::HttpResponse {
            code: 200,
            headers: vec![http::HttpHeader {
                key: "rp-nonce".to_owned(),
                value: "x".to_owned(),
            }],
        };
        assert!(!parse_session_response(&resp).success);

        // Nicht-200 ist nie success
        let resp = http::HttpResponse {
            code: 403,
            headers: vec![http::HttpHeader {
                key: "RP-Nonce".to_owned(),
                value: "x".to_owned(),
            }],
        };
        assert!(!parse_session_response(&resp).success);
    }

    #[test]
    fn parse_session_response_application_reason_hex() {
        let resp = http::HttpResponse {
            code: 403,
            headers: vec![
                http::HttpHeader {
                    key: "RP-Application-Reason".to_owned(),
                    value: "80108b10".to_owned(),
                },
                http::HttpHeader {
                    key: "RP-Version".to_owned(),
                    value: "10.0".to_owned(),
                },
            ],
        };
        let parsed = parse_session_response(&resp);
        assert!(!parsed.success);
        assert_eq!(parsed.error_code, RP_APPLICATION_REASON_IN_USE);
        assert_eq!(parsed.rp_version.as_deref(), Some("10.0"));

        // Ungültige/leere Werte -> 0 (strtoul-Semantik)
        for bad in ["", "zzz", "0x"] {
            let resp = http::HttpResponse {
                code: 500,
                headers: vec![http::HttpHeader {
                    key: "RP-Application-Reason".to_owned(),
                    value: bad.to_owned(),
                }],
            };
            assert_eq!(parse_session_response(&resp).error_code, 0, "{bad:?}");
        }
        // 0x-Präfix wird akzeptiert (strtoul base 16)
        let resp = http::HttpResponse {
            code: 500,
            headers: vec![http::HttpHeader {
                key: "RP-Application-Reason".to_owned(),
                value: "0x80108b15".to_owned(),
            }],
        };
        assert_eq!(
            parse_session_response(&resp).error_code,
            RP_APPLICATION_REASON_CRASH
        );
    }

    #[test]
    fn connect_video_profile_preset_session_table() {
        let p = connect_video_profile_preset_session(
            VideoResolutionPresetSession::P1080,
            VideoFpsPresetSession::Fps60,
        );
        assert_eq!(
            (p.width, p.height, p.max_fps, p.bitrate),
            (1920, 1080, 60, 15000)
        );
        assert_eq!(p.codec, crate::error::Codec::H264);

        let p = connect_video_profile_preset_session(
            VideoResolutionPresetSession::P360,
            VideoFpsPresetSession::Fps30,
        );
        assert_eq!((p.width, p.height, p.max_fps, p.bitrate), (640, 360, 30, 2000));
    }

    #[test]
    fn did_layout_prefix_random_suffix() {
        // Struktur aus chiaki_session_init: 10 Byte Prefix, 16 random,
        // 6 Byte Suffix.
        const DID_PREFIX: [u8; 10] = [0x00, 0x18, 0x00, 0x00, 0x00, 0x07, 0x00, 0x40, 0x00, 0x80];
        const DID_SUFFIX: [u8; 6] = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let mut did = [0xffu8; RP_DID_SIZE];
        did[..10].copy_from_slice(&DID_PREFIX);
        crate::random::random_bytes_crypt(&mut did[10..26]).unwrap();
        did[26..].copy_from_slice(&DID_SUFFIX);
        assert_eq!(&did[..10], &DID_PREFIX[..]);
        assert_eq!(&did[26..], &DID_SUFFIX[..]);
        assert!(did[10..26].iter().any(|b| *b != 0xff));
    }

    #[test]
    fn resolve_host_addrs_prefers_ipv4_unless_colon() {
        // "localhost" -> ipv4 bevorzugt (C: hints AF_INET)
        let addrs = resolve_host_addrs("localhost").unwrap();
        assert!(!addrs.is_empty());
        assert!(addrs.iter().all(|a| a.is_ipv4()));
    }
}
