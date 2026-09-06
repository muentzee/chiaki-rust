// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/streamconnection.c + lib/include/chiaki/streamconnection.h
// (chiaki-ng).
//
// Die Herzstück-State-Machine einer Session: baut das eigene Takion auf
// (protocol_version 12 für PS5, sonst 9), führt den ECDH-Handshake über
// Protobuf-Daten-Nachrichten durch (BIG -> BANG), aktiviert beidseitige
// GKCrypt-Verschlüsselung, tauscht LaunchSpec/StreamInfo aus und wiringt
// AudioReceiver/VideoReceiver/FeedbackSender/CongestionControl.
//
// Threading-Modell (1:1 zum C):
// - session_thread: ruft `run()` (synchron) und wartet in den State-Phasen.
// - takion-recv-thread: liefert TakionEvent an `takion_cb` (State-Übergänge,
//   AV-Dispatch, Rumble/PadInfo/TriggerEffects-Events).
// - feedbacksender-Thread (in FeedbackSender), congestion-control-Thread.
//
// Mutex-Ordnung (Deadlock-Freiheit):
// - `StreamConnectionShared::state` (C: state_mutex) wird nie zusammen mit
//   anderen Sc-Mutexen gehalten; Receiver-/Feedback-Mutexe sind Blätter.
// - `SessionShared::state` (session.rs, C: session->state_mutex) und
//   `StreamConnectionShared::state` werden NIE gleichzeitig gehalten (das C
//   hält sie ebenfalls strikt getrennt).
// - Events (`SessionCallbacks::event`) werden — wie im C — ohne gehaltene
//   StreamConnection-state-Mutex gefeuert; einzige Ausnahme ist der
//   Audio-Header (C: chiaki_audio_receiver_stream_info läuft unter der
//   state_mutex der StreamConnection, der header_cb folglich ebenso).
//
// Abweichungen zum C (dokumentiert):
// - Takion wird als `Arc<Takion>` gehalten (FeedbackSender/CongestionControl
//   brauchen `Arc<Takion>`); das explizite `chiaki_takion_close()` entspricht
//   `Takion::close()` auf dem letzten Arc (Drop schließt sonst automatisch).
// - `measured_bitrate` (CONNECTIONQUALITY-Handler) zählt Videopaket-Bytes in
//   einem eigenen StreamStats, da videoreceiver.rs seine FrameProcessor-Stats
//   nicht exponiert (rein diagnostisch, siehe stream_connection_takion_data_idle).
// - nanopb-Callbacks sind prost-Typen; `chiaki_pb_encode_zero_encrypted_key`
//   wird zu `encrypted_key = vec![0; 4]` (wire-identisch).

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use prost::Message as _;

use crate::audio::AudioHeader;
use crate::audioreceiver::{AudioReceiver, AudioSink, HapticsSink};
use crate::congestioncontrol::CongestionControl;
use crate::controller::ControllerState;
use crate::error::{ChiakiError, ChiakiResult};
use crate::feedbacksender::FeedbackSender;
use crate::frameprocessor::StreamStats;
use crate::gkcrypt::{GKCrypt, GKCRYPT_BLOCK_SIZE, GKCRYPT_KEY_BUF_BLOCKS_DEFAULT, HANDSHAKE_KEY_SIZE};
use crate::launchspec::{launchspec_format, LaunchSpec};
use crate::packetstats::PacketStats;
use crate::proto::*;
use crate::rpcrypt::Rpcrypt;
use crate::session::{SessionEvent, SessionShared};
use crate::takion::{
    Takion, TakionConnectInfo, TakionEvent, TakionMessageDataType, AVPacket,
};
use crate::video::{VideoProfile, VIDEO_BUFFER_PADDING_SIZE};
use crate::videoreceiver::{VideoReceiver, VideoReceiverCallbacks};

/// C: `#define STREAM_CONNECTION_PORT 9296`
pub const STREAM_CONNECTION_PORT: u16 = 9296;
/// C: `#define EXPECT_TIMEOUT_MS 5000`
pub const EXPECT_TIMEOUT_MS: u64 = 5000;
/// C: `#define HEARTBEAT_INTERVAL_MS 1000`
pub const HEARTBEAT_INTERVAL_MS: u64 = 1000;

/// Port von `ChiakiDualSenseEffectIntensity` (streamconnection.h).
/// Die Werte kommen 1:1 aus dem Pad-Info-Paket der Console ("must not change").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum DualSenseEffectIntensity {
    Off = 0,
    Weak = 3,
    Medium = 2,
    Strong = 1,
}

impl DualSenseEffectIntensity {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(DualSenseEffectIntensity::Off),
            3 => Some(DualSenseEffectIntensity::Weak),
            2 => Some(DualSenseEffectIntensity::Medium),
            1 => Some(DualSenseEffectIntensity::Strong),
            _ => None,
        }
    }

    /// C: `DualSenseIntensity()` — String fürs Log.
    pub fn as_str(self) -> &'static str {
        match self {
            DualSenseEffectIntensity::Strong => "Strong",
            DualSenseEffectIntensity::Weak => "Weak",
            DualSenseEffectIntensity::Medium => "Medium",
            DualSenseEffectIntensity::Off => "Off",
        }
    }
}

/// Port von `StreamConnectionState` (streamconnection.c).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum StreamConnectionState {
    #[default]
    Idle,
    TakionConnect,
    ExpectBang,
    ExpectStreaminfo,
}

/// Von `state_mutex` geschützter Zustand (C: state/state_finished/
/// state_failed/should_stop/remote_disconnected/remote_disconnect_reason
/// plus streaminfo_early_buf).
pub(crate) struct ScState {
    state: StreamConnectionState,
    state_finished: bool,
    state_failed: bool,
    should_stop: bool,
    remote_disconnected: bool,
    remote_disconnect_reason: Option<String>,
    /// C: streaminfo_early_buf — STREAMINFO, das schon vor dem BANG ankommt.
    streaminfo_early_buf: Option<Vec<u8>>,
}

impl Default for ScState {
    fn default() -> Self {
        ScState {
            state: StreamConnectionState::Idle,
            state_finished: false,
            state_failed: false,
            should_stop: false,
            remote_disconnected: false,
            remote_disconnect_reason: None,
            streaminfo_early_buf: None,
        }
    }
}

/// C: feedback_sender + feedback_sender_active + session->controller_state
/// (alles hinter `feedback_sender_mutex`).
pub(crate) struct FeedbackSlot {
    pub sender: Option<FeedbackSender>,
    /// whether feedback_sender is initialized —
    /// only if this is true, feedback_sender may be accessed!
    pub active: bool,
    /// C: session->controller_state (von derselben Mutex geschützt)
    pub controller_state: ControllerState,
}

/// Pad-Info-Zustand (C: Felder von ChiakiStreamConnection, die nur der
/// Takion-Callback-Thread anfasst — hier defensiv gekapselt).
#[derive(Default)]
pub(crate) struct PadState {
    led_state: [u8; 3],
    player_index: u8,
    haptic_intensity: Option<DualSenseEffectIntensity>,
    trigger_intensity: Option<DualSenseEffectIntensity>,
}

/// Von der StreamConnection geteilter Zustand (der Takion-Callback greift
/// über `Arc` darauf zu, auch über `run()`-Grenzen hinweg).
pub(crate) struct StreamConnectionShared {
    /// signaled on change of state_finished or should_stop
    /// (C: state_cond; `state` wird davon geschützt)
    pub state: Mutex<ScState>,
    pub state_cond: Condvar,

    pub ctx: Arc<SessionShared>,

    /// gkcrypt_remote (AV-Decrypt im Callback + Parität zum C-Feld)
    pub gkcrypt_remote: Mutex<Option<Arc<GKCrypt>>>,
    /// gkcrypt_local (Parität zum C-Feld; der Versand läuft über Takion)
    pub gkcrypt_local: Mutex<Option<Arc<GKCrypt>>>,
    /// Takion-Handle für Receiver-Callbacks (corrupt frame / IDR request)
    pub takion: Mutex<Option<Arc<Takion>>>,

    pub audio_receiver: Mutex<Option<Arc<AudioReceiver>>>,
    pub haptics_receiver: Mutex<Option<Arc<AudioReceiver>>>,
    pub video_receiver: Mutex<Option<Arc<VideoReceiver>>>,

    /// C: feedback_sender + feedback_sender_active (hinter einer Mutex)
    pub feedback: Mutex<FeedbackSlot>,

    pub packet_stats: Arc<PacketStats>,

    /// Pad-Info-Zustand (nur Takion-Callback-Thread)
    pub pad: Mutex<PadState>,

    /// Siehe Modul-Kommentar "Abweichungen": eigene Stream-Statistik für den
    /// CONNECTIONQUALITY-Handler (C: video_receiver->frame_processor.stats).
    pub stream_stats: Mutex<StreamStats>,
    /// C: measured_bitrate (f64-Bits atomar; rein diagnostisch)
    pub measured_bitrate_bits: AtomicU64,
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Port von `ChiakiStreamConnection`.
///
/// Alle Felder sind — wie im C unter Mutexen — über Interior Mutability
/// erreichbar; `run()` kommt daher mit `&self` aus und die Session kann
/// `Arc<StreamConnection>` an den session_thread geben.
pub struct StreamConnection {
    shared: Arc<StreamConnectionShared>,
    /// C: takion (besitzend über Arc; close via Arc::into_inner)
    takion: Mutex<Option<Arc<Takion>>>,
    /// C: congestion_control
    congestion_control: Mutex<Option<CongestionControl>>,
    /// C: packet_loss_max (Init-Parameter)
    packet_loss_max: f64,
    /// Takion-Protokollversion des letzten connects (Logging/Tests)
    takion_version: AtomicU8,
    /// C: audio_sender (für Mikrofon-Opus-Frames), beim Takion-Connect erzeugt
    audio_sender: Mutex<Option<crate::audiosender::AudioSender>>,
}

impl StreamConnection {
    /// Port von `chiaki_stream_connection_init()`.
    pub(crate) fn new(ctx: Arc<SessionShared>, packet_loss_max: f64) -> Self {
        StreamConnection {
            shared: Arc::new(StreamConnectionShared {
                state: Mutex::new(ScState::default()),
                state_cond: Condvar::new(),
                ctx,
                gkcrypt_remote: Mutex::new(None),
                gkcrypt_local: Mutex::new(None),
                takion: Mutex::new(None),
                audio_receiver: Mutex::new(None),
                haptics_receiver: Mutex::new(None),
                video_receiver: Mutex::new(None),
                feedback: Mutex::new(FeedbackSlot {
                    sender: None,
                    active: false,
                    controller_state: ControllerState::default(),
                }),
                packet_stats: Arc::new(PacketStats::new()),
                pad: Mutex::new(PadState {
                    haptic_intensity: Some(DualSenseEffectIntensity::Strong),
                    trigger_intensity: Some(DualSenseEffectIntensity::Strong),
                    ..Default::default()
                }),
                stream_stats: Mutex::new(StreamStats::default()),
                measured_bitrate_bits: AtomicU64::new(0.0f64.to_bits()),
            }),
            takion: Mutex::new(None),
            congestion_control: Mutex::new(None),
            packet_loss_max,
            takion_version: AtomicU8::new(0),
            audio_sender: Mutex::new(None),
        }
    }

    /// Port von `chiaki_stream_connection_stop()`: setzt should_stop und weckt
    /// den run()-Thread. Kann aus jedem Thread gerufen werden.
    pub fn stop(&self) {
        let mut st = lock(&self.shared.state);
        st.should_stop = true;
        self.shared.state_cond.notify_all();
    }

    /// C: remote_disconnect_reason (letzte Begründung der Gegenseite).
    pub fn remote_disconnect_reason(&self) -> Option<String> {
        lock(&self.shared.state).remote_disconnect_reason.clone()
    }

    /// Takion-Protokollversion des letzten connects (für Logs/Tests).
    pub fn takion_version(&self) -> u8 {
        self.takion_version.load(Ordering::SeqCst)
    }

    // ------------------------------------------------------------------
    // run (C: chiaki_stream_connection_run)
    // ------------------------------------------------------------------

    /// Port von `chiaki_stream_connection_run()` — synchron.
    ///
    /// `socket` entspricht dem C-`socket`-Parameter (Holepunch-Datensock):
    /// wird er mitgegeben, nutzt Takion ihn (PSN-Pfad), sonst baut Takion
    /// einen eigenen Socket zur Console (Port 9296) auf.
    pub fn run(&self, socket: Option<UdpSocket>) -> ChiakiResult<()> {
        let shared = Arc::clone(&self.shared);
        let ctx = Arc::clone(&shared.ctx);

        // --- TakionConnectInfo (C: takion_info) ---
        // Host/Port nur nötig, wenn kein Socket mitgegeben wurde (C: set_port
        // 9296 auf host_addrinfo_selected); im PSN-Pfad wird der
        // Holepunch-Datensock genutzt und host_addr ignoriert.
        let host: SocketAddr = match &socket {
            None => {
                let addr = lock(&ctx.host_addr).ok_or(ChiakiError::Uninitialized)?;
                let mut addr = addr;
                addr.set_port(STREAM_CONNECTION_PORT);
                addr
            }
            Some(_) => "0.0.0.0:0".parse().unwrap(),
        };

        let takion_info = TakionConnectInfo {
            host,
            ip_dontfrag: ctx.dontfrag.load(Ordering::SeqCst),
            callback: Arc::new(move |event| takion_cb(&shared, event)),
            disable_audio_video: ctx.disable_audio_video,
            enable_crypt: true,
            enable_dualsense: ctx.enable_dualsense,
            // C: chiaki_target_is_ps5(session->target) ? 12 : 9
            protocol_version: if ctx.target().is_ps5() { 12 } else { 9 },
            av_reorder_timeout_us: ctx.av_reorder_timeout_us,
        };

        {
            let st = lock(&self.shared.state);
            if st.should_stop {
                return Err(ChiakiError::Canceled);
            }
        }

        // --- Receiver (C: audio/haptics/video_receiver_new) ---
        *lock(&self.shared.audio_receiver) = Some(Arc::new(AudioReceiver::new(
            self.make_audio_sink(),
            HapticsSink::default(),
            Some(Arc::clone(&self.shared.packet_stats)),
        )));
        *lock(&self.shared.haptics_receiver) = Some(Arc::new(AudioReceiver::new(
            AudioSink::default(),
            self.make_haptics_sink(),
            None,
        )));
        *lock(&self.shared.video_receiver) = Some(Arc::new(VideoReceiver::new(
            lock(&self.shared.ctx.video_profile).codec,
            self.shared.ctx.enable_idr_on_fec_failure,
            Some(Arc::clone(&self.shared.packet_stats)),
            self.make_video_callbacks(),
        )));

        // --- Takion connect ---
        {
            let mut st = lock(&self.shared.state);
            st.state = StreamConnectionState::TakionConnect;
            st.state_finished = false;
            st.state_failed = false;
        }

        let takion = match Takion::connect(takion_info, socket) {
            Ok(t) => {
                self.takion_version.store(t.version(), Ordering::SeqCst);
                Arc::new(t)
            }
            Err(e) => {
                tracing::error!("StreamConnection connect failed");
                self.teardown();
                return Err(e);
            }
        };
        *lock(&self.shared.takion) = Some(Arc::clone(&takion));
        *lock(&self.takion) = Some(Arc::clone(&takion));
        *lock(&self.audio_sender) = Some(crate::audiosender::AudioSender::new(
            self.shared.ctx.ps5,
            Arc::clone(&takion),
        ));

        // --- Congestion Control ---
        match CongestionControl::start(
            Arc::clone(&takion),
            Arc::clone(&self.shared.packet_stats),
            self.packet_loss_max,
        ) {
            Ok(cc) => *lock(&self.congestion_control) = Some(cc),
            Err(e) => {
                tracing::error!("StreamConnection failed to start Congestion Control");
                self.teardown();
                return Err(e);
            }
        }

        // --- Auf Takion-Handshake warten (STATE_TAKION_CONNECT) ---
        // C: err = cond_timedwait_pred(...); CHECK_STOP(close_takion);
        //    if(err != SUCCESS) -> "Takion connect failed"
        let wait = state_cond_timedwait(&self.shared, EXPECT_TIMEOUT_MS);
        // C: CHECK_STOP(close_takion) — nur should_stop zählt (Returnwert
        // bleibt SUCCESS, wie im C-Label-Durchlauf).
        if lock(&self.shared.state).should_stop {
            self.teardown();
            return Ok(());
        }
        if let Err(e) = wait {
            tracing::error!("StreamConnection Takion connect failed");
            self.teardown();
            return Err(e);
        }

        // --- BIG senden + auf BANG warten (STATE_EXPECT_BANG) ---
        tracing::info!("StreamConnection sending big");
        {
            let mut st = lock(&self.shared.state);
            st.state = StreamConnectionState::ExpectBang;
            st.state_finished = false;
            st.state_failed = false;
        }
        if let Err(e) = self.send_big(&takion) {
            tracing::error!("StreamConnection failed to send big: {e:?}");
            return self.finish_disconnect(Err(e));
        }

        match state_cond_wait_check_stop(&self.shared, EXPECT_TIMEOUT_MS) {
            (true, _) => return self.finish_disconnect(Err(ChiakiError::Canceled)),
            (false, Err(ChiakiError::Timeout)) => {
                tracing::error!("StreamConnection bang receive timeout");
            }
            (false, Err(e)) => return self.finish_disconnect(Err(e)),
            (false, Ok(())) => {}
        }
        if !lock(&self.shared.state).state_finished {
            tracing::error!("StreamConnection didn't receive bang or failed to handle it");
            return self.finish_disconnect(Err(ChiakiError::Unknown));
        }
        tracing::info!("StreamConnection successfully received bang");

        // --- Auf StreamInfo warten (STATE_EXPECT_STREAMINFO) ---
        {
            let early = {
                let mut st = lock(&self.shared.state);
                st.state = StreamConnectionState::ExpectStreaminfo;
                st.state_finished = false;
                st.state_failed = false;
                st.streaminfo_early_buf.take()
            };
            if let Some(early) = early {
                tracing::info!("StreamConnection processing streaminfo received early");
                let mut st = lock(&self.shared.state);
                stream_connection_takion_data_expect_streaminfo(&self.shared, &early, &mut st);
            }
        }
        if !lock(&self.shared.state).state_finished {
            match state_cond_wait_check_stop(&self.shared, EXPECT_TIMEOUT_MS) {
                (true, _) => return self.finish_disconnect(Err(ChiakiError::Canceled)),
                (false, Err(ChiakiError::Timeout)) => {
                    tracing::error!("StreamConnection streaminfo receive timeout");
                }
                (false, Err(e)) => return self.finish_disconnect(Err(e)),
                (false, Ok(())) => {}
            }
            if !lock(&self.shared.state).state_finished {
                tracing::error!("StreamConnection didn't receive streaminfo");
                return self.finish_disconnect(Err(ChiakiError::Unknown));
            }
        }
        tracing::info!("StreamConnection successfully received streaminfo");

        // --- Feedback Sender starten ---
        let feedback_result = {
            let mut feedback = lock(&self.shared.feedback);
            match FeedbackSender::new(Arc::clone(&takion)) {
                Ok(sender) => {
                    feedback.sender = Some(sender);
                    feedback.active = true;
                    let state = feedback.controller_state;
                    if let Some(sender) = &feedback.sender {
                        let _ = sender.set_controller_state(&state);
                    }
                    Ok(())
                }
                Err(e) => {
                    tracing::error!("StreamConnection failed to start Feedback Sender");
                    Err(e)
                }
            }
        };
        if let Err(e) = feedback_result {
            return self.finish_disconnect(Err(e));
        }

        {
            let mut st = lock(&self.shared.state);
            st.state = StreamConnectionState::Idle;
            st.state_finished = false;
            st.state_failed = false;
        }

        // Connected-Event ohne gehaltene Locks feuern (wie im C).
        self.shared.ctx.send_event(SessionEvent::Connected);

        // --- IDLE: Heartbeats bis should_stop / remote_disconnected ---
        let mut idle_err: ChiakiResult<()> = Ok(());
        loop {
            match state_cond_timedwait(&self.shared, HEARTBEAT_INTERVAL_MS) {
                Err(ChiakiError::Timeout) => {
                    if send_heartbeat(&takion).is_err() {
                        tracing::error!("StreamConnection failed to send heartbeat");
                    } else {
                        tracing::trace!("StreamConnection sent heartbeat");
                    }
                }
                Err(e) => {
                    // unerreichbar (state_cond_timedwait kennt nur Ok/Timeout)
                    idle_err = Err(e);
                    break;
                }
                Ok(()) => break,
            }
        }

        // --- Feedback Sender stoppen (C: feedback_sender_fini) ---
        {
            let mut feedback = lock(&self.shared.feedback);
            feedback.active = false;
            feedback.sender = None; // Drop: Thread stoppen + joinen
        }

        self.finish_disconnect(idle_err)
    }

    /// C: `disconnect:`-Label — early buf freigeben, Disconnect senden und
    /// das Endergebnis anhand von should_stop/remote_disconnected mappen.
    /// Ruft anschließend `teardown()` (Labels err_congestion_control bis
    /// err_audio_receiver).
    fn finish_disconnect(&self, err: ChiakiResult<()>) -> ChiakiResult<()> {
        tracing::info!("StreamConnection is disconnecting");
        {
            let mut st = lock(&self.shared.state);
            st.streaminfo_early_buf = None;
        }
        if let Some(takion) = &*lock(&self.takion) {
            let _ = send_disconnect(takion);
        }

        let result = {
            let st = lock(&self.shared.state);
            if st.should_stop {
                tracing::info!("StreamConnection was requested to stop");
                Err(ChiakiError::Canceled)
            } else if st.remote_disconnected {
                tracing::info!("StreamConnection closing after Remote disconnected");
                Err(ChiakiError::Disconnected)
            } else {
                err
            }
        };

        self.teardown();
        result
    }

    /// Aufräumen (C: err_congestion_control / close_takion /
    /// err_*_receiver-Labels) — idempotent.
    fn teardown(&self) {
        // C: chiaki_congestion_control_stop
        if let Some(mut cc) = lock(&self.congestion_control).take() {
            let _ = cc.stop();
        }

        // C: chiaki_mutex_unlock(state_mutex); chiaki_takion_close(...)
        let takion = lock(&self.shared.takion).take();
        lock(&self.takion).take();
        *lock(&self.audio_sender) = None;
        if let Some(t) = takion {
            if let Some(mut t) = Arc::into_inner(t) {
                t.close();
                tracing::info!("StreamConnection closed takion");
            }
        }

        // Receiver freigeben (unter state_mutex, wie im C)
        let _st = lock(&self.shared.state);
        *lock(&self.shared.video_receiver) = None;
        *lock(&self.shared.haptics_receiver) = None;
        *lock(&self.shared.audio_receiver) = None;
        *lock(&self.shared.gkcrypt_remote) = None;
        *lock(&self.shared.gkcrypt_local) = None;
    }

    // ------------------------------------------------------------------
    // Receiver-Wiring (C: session->video_sample_cb / audio_sink /
    // haptics_sink / Event-Callback über die Session)
    // ------------------------------------------------------------------

    fn make_audio_sink(&self) -> AudioSink {
        let ctx_header = Arc::clone(&self.shared.ctx);
        AudioSink {
            header_cb: Some(Arc::new(move |header: &AudioHeader| {
                ctx_header.send_event(SessionEvent::AudioStreamInfo(header.clone()));
            })),
            frame_cb: Some(self.shared.ctx.callbacks_audio()),
        }
    }

    fn make_haptics_sink(&self) -> HapticsSink {
        HapticsSink {
            frame_cb: Some(self.shared.ctx.callbacks_haptics()),
        }
    }

    fn make_video_callbacks(&self) -> VideoReceiverCallbacks {
        let ctx = Arc::clone(&self.shared.ctx);
        let shared_corrupt = Arc::clone(&self.shared);
        let shared_idr = Arc::clone(&self.shared);
        let ctx_fec = Arc::clone(&self.shared.ctx);
        VideoReceiverCallbacks {
            video_sample: Some(Arc::new(
                move |buf: &[u8], frames_lost: i32, frame_recovered: bool| {
                    ctx.video_sample(buf, frames_lost, frame_recovered)
                },
            )),
            send_corrupt_frame: Some(Arc::new(move |start: u16, end: u16| {
                match &*lock(&shared_corrupt.takion) {
                    Some(takion) => send_corrupt_frame(takion, start, end),
                    None => Err(ChiakiError::Uninitialized),
                }
            })),
            send_idr_request: Some(Arc::new(move || match &*lock(&shared_idr.takion) {
                Some(takion) => send_idr_request(takion),
                None => Err(ChiakiError::Uninitialized),
            })),
            video_fec_failure: Some(Arc::new(
                move |frame_index: i32, idr_request_sent: bool| {
                    ctx_fec.send_event(SessionEvent::VideoFecFailure {
                        frame_index,
                        idr_request_sent,
                    });
                },
            )),
        }
    }

    // ------------------------------------------------------------------
    // Öffentliche Aktionen (von der Session aus gerufen)
    // ------------------------------------------------------------------

    /// C: `chiaki_session_set_controller_state` — Zustand merken und (falls
    /// der FeedbackSender aktiv ist) weiterreichen.
    pub fn set_controller_state(&self, state: &ControllerState) {
        let mut feedback = lock(&self.shared.feedback);
        feedback.controller_state = *state;
        if feedback.active {
            if let Some(sender) = &feedback.sender {
                let _ = sender.set_controller_state(state);
            }
        }
    }

    /// C: `stream_connection_send_idr_request()`.
    pub fn send_idr_request(&self) -> ChiakiResult<()> {
        match &*lock(&self.takion) {
            Some(takion) => send_idr_request(takion),
            None => Err(ChiakiError::Uninitialized),
        }
    }

    /// C: `stream_connection_send_corrupt_frame()`.
    pub fn send_corrupt_frame(&self, start: u16, end: u16) -> ChiakiResult<()> {
        match &*lock(&self.takion) {
            Some(takion) => send_corrupt_frame(takion, start, end),
            None => Err(ChiakiError::Uninitialized),
        }
    }

    /// C: `chiaki_audio_sender_opus_data()`-Pfad (Opus-Mikrofonframes an den
    /// AudioSender der StreamConnection).
    pub fn send_mic_data(&self, buf: &[u8]) -> ChiakiResult<()> {
        let sender = lock(&self.audio_sender);
        match &*sender {
            Some(sender) => {
                sender.opus_data(buf);
                Ok(())
            }
            None => Err(ChiakiError::Uninitialized),
        }
    }

    // ------------------------------------------------------------------
    // BIG-Payload (LaunchSpec) — C: stream_connection_send_big()
    // ------------------------------------------------------------------

    fn send_big(&self, takion: &Arc<Takion>) -> ChiakiResult<()> {
        let ctx = &self.shared.ctx;

        let target = ctx.target();
        let handshake_key = *lock(&ctx.handshake_key);
        let video_profile = lock(&ctx.video_profile).clone();

        // ChiakiLaunchSpec befüllen (C: launch_spec)
        let spec = LaunchSpec {
            target,
            mtu: ctx.mtu_in.load(Ordering::SeqCst),
            rtt: (ctx.rtt_us.load(Ordering::SeqCst) / 1000) as u32,
            handshake_key,
            width: video_profile.width,
            height: video_profile.height,
            max_fps: video_profile.max_fps,
            codec: video_profile.codec,
            bw_kbps_sent: video_profile.bitrate,
        };

        // LaunchSpec-JSON verschlüsseln (rpcrypt-Keystream XOR) + base64.
        let launch_spec_b64 = launchspec_b64(&spec, &lock(&ctx.rpcrypt))?;

        let session_key = lock(&ctx.session_id).clone();

        // ECDH pub key + sig
        let (ecdh_pub_key, ecdh_sig) = {
            let mut ecdh = lock(&ctx.ecdh);
            let ecdh = ecdh.as_mut().ok_or(ChiakiError::Uninitialized)?;
            ecdh.get_local_pub_key(&handshake_key)?
        };

        let msg = build_big_message(
            takion.version() as u32,
            &session_key,
            &launch_spec_b64,
            &ecdh_pub_key,
            &ecdh_sig,
        );
        let buf = msg.encode_to_vec();

        // Chunking gemäß MTU (C-Schleife 1:1; Rest-Sendung mit flags=1).
        let mtu = ctx
            .mtu_in
            .load(Ordering::SeqCst)
            .min(ctx.mtu_out.load(Ordering::SeqCst)) as usize;
        // Take into account overhead of network
        let mtu = mtu.checked_sub(50).ok_or(ChiakiError::Overflow)?;
        let mut plan = chunk_plan(buf.len(), mtu)?;
        let mut buf_pos = 0usize;
        let mut first = true;
        for (flags, size) in plan.drain(..) {
            let chunk = &buf[buf_pos..buf_pos + size];
            if first {
                takion.send_message_data(flags, 1, chunk)?;
                first = false;
            } else {
                takion.send_message_data_cont(flags, 1, chunk)?;
            }
            buf_pos += size;
        }
        let total_size = buf.len() - buf_pos;
        if total_size > 0 {
            let chunk = &buf[buf_pos..buf_pos + total_size];
            if first {
                takion.send_message_data(1, 1, chunk)?;
            } else {
                takion.send_message_data_cont(1, 1, chunk)?;
            }
        }
        Ok(())
    }
}

// ----------------------------------------------------------------------
// Reine Hilfsfunktionen (ohne Netzwerk testbar)
// ----------------------------------------------------------------------

/// C: `chiaki_cond_timedwait_pred(..., state_finished_cond_check)`.
///
/// Liefert `Ok(())`, wenn das Prädikat erfüllt ist (state_finished,
/// should_stop oder remote_disconnected), `Err(Timeout)` nach Ablauf.
pub(crate) fn state_cond_timedwait(
    shared: &StreamConnectionShared,
    timeout_ms: u64,
) -> ChiakiResult<()> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut guard = lock(&shared.state);
    loop {
        if guard.state_finished || guard.should_stop || guard.remote_disconnected {
            return Ok(());
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(ChiakiError::Timeout);
        }
        let (g, _res) = shared
            .state_cond
            .wait_timeout(guard, deadline - now)
            .unwrap_or_else(PoisonError::into_inner);
        guard = g;
    }
}

/// Kombination aus `chiaki_cond_timedwait_pred` + `CHECK_STOP(disconnect)`:
/// liefert `(should_stop, wait_result)`.
fn state_cond_wait_check_stop(
    shared: &StreamConnectionShared,
    timeout_ms: u64,
) -> (bool, ChiakiResult<()>) {
    let wait = state_cond_timedwait(shared, timeout_ms);
    let stopped = lock(&shared.state).should_stop;
    (stopped, wait)
}

/// C: `xor_bytes` (utils.h) für den LaunchSpec-Keystream.
fn xor_bytes(dst: &mut [u8], src: &[u8]) {
    for (d, s) in dst.iter_mut().zip(src.iter()) {
        *d ^= *s;
    }
}

/// LaunchSpec-JSON -> rpcrypt-verschlüsselt -> base64 (C-Ablauf in
/// `stream_connection_send_big`): zeros verschlüsseln (= Keystream), mit dem
/// JSON XOR-en, base64-kodieren. Das JSON enthält wie im C das abschließende
/// NUL-Byte (`launch_spec_json_size += 1`).
pub(crate) fn launchspec_b64(spec: &LaunchSpec, rpcrypt: &Option<Rpcrypt>) -> ChiakiResult<String> {
    let rpcrypt = rpcrypt.as_ref().ok_or(ChiakiError::Uninitialized)?;
    let json = launchspec_format(spec)?;
    let json_size = json.len() + 1; // wir wollen auch das trailing 0
    let mut enc = vec![0u8; json_size];
    rpcrypt.encrypt(0, &mut enc)?;
    xor_bytes(&mut enc[..json.len()], json.as_bytes());
    Ok(crate::base64::encode(&enc))
}

/// Baut die TakionMessage-BIG (C: stream_connection_send_big, protobuf-Teil).
pub(crate) fn build_big_message(
    client_version: u32,
    session_key: &str,
    launch_spec_b64: &str,
    ecdh_pub_key: &[u8],
    ecdh_sig: &[u8],
) -> TakionMessage {
    TakionMessage {
        r#type: takion_message::PayloadType::Big.into(),
        big_payload: Some(BigPayload {
            client_version,
            session_key: session_key.to_owned(),
            launch_spec: launch_spec_b64.to_owned(),
            // C: chiaki_pb_encode_zero_encrypted_key -> 4 Null-Bytes
            encrypted_key: vec![0, 0, 0, 0],
            ecdh_pub_key: Some(ecdh_pub_key.to_vec()),
            ecdh_sig: Some(ecdh_sig.to_vec()),
        }),
        ..Default::default()
    }
}

/// C-Chunking-Schleife aus `stream_connection_send_big` als reine Funktion:
/// liefert die `(chunk_flags, chunk_size)`-Paare der Vollsendschritte; der
/// Rest (`total_size`-Rest, flags=1) wird beim Aufrufer gesendet.
pub(crate) fn chunk_plan(total_size: usize, mtu: usize) -> ChiakiResult<Vec<(u8, usize)>> {
    let mut plan = Vec::new();
    let mut total_size = total_size;
    let mut first = true;
    while (mtu < total_size + 26) || (mtu < total_size + 25 && !first) {
        let size = if first {
            mtu.checked_sub(26).ok_or(ChiakiError::Overflow)?
        } else {
            mtu.checked_sub(25).ok_or(ChiakiError::Overflow)?
        };
        plan.push((0u8, size));
        first = false;
        total_size -= size;
    }
    Ok(plan)
}

/// C: `stream_connection_send_controller_connection()`.
pub(crate) fn build_controller_connection(enable_dualsense: bool) -> TakionMessage {
    use crate::proto::controller_connection_payload::ControllerType;
    TakionMessage {
        r#type: takion_message::PayloadType::Controllerconnection.into(),
        controller_connection_payload: Some(ControllerConnectionPayload {
            controller_id: None,
            connected: Some(true),
            controller_type: Some(
                if enable_dualsense {
                    ControllerType::Dualsense
                } else {
                    ControllerType::Dualshock4
                }
                .into(),
            ),
        }),
        ..Default::default()
    }
}

/// C: `stream_connection_enable_microphone()` — STREAMINFO mit dem
/// Mikrofon-Audio-Header (16/1/48000/480 — exakt wie im C).
pub(crate) fn build_mic_streaminfo() -> ChiakiResult<TakionMessage> {
    let audio_header_input = AudioHeader::set(16, 1, 48000, 480);
    let mut audio_header = [0u8; crate::audio::AUDIO_HEADER_SIZE];
    audio_header_input.save(&mut audio_header)?;

    Ok(TakionMessage {
        r#type: takion_message::PayloadType::Streaminfo.into(),
        stream_info_payload: Some(StreamInfoPayload {
            audio_header: audio_header.to_vec(),
            resolution: Vec::new(),
            start_timeout: None,
            afk_timeout: None,
            afk_timeout_disconnect: None,
            congestion_control_interval: None,
            audio_channel: Vec::new(),
        }),
        ..Default::default()
    })
}

/// C: `stream_connection_send_streaminfo_ack()`.
pub(crate) fn build_streaminfo_ack() -> TakionMessage {
    TakionMessage {
        r#type: takion_message::PayloadType::Streaminfoack.into(),
        ..Default::default()
    }
}

/// C: `stream_connection_send_disconnect()`.
pub(crate) fn build_disconnect_message() -> TakionMessage {
    TakionMessage {
        r#type: takion_message::PayloadType::Disconnect.into(),
        disconnect_payload: Some(DisconnectPayload {
            reason: "Client Disconnecting".to_owned(),
            extended_info: None,
        }),
        ..Default::default()
    }
}

/// C: `stream_connection_send_heartbeat()`.
pub(crate) fn build_heartbeat_message() -> TakionMessage {
    TakionMessage {
        r#type: takion_message::PayloadType::Heartbeat.into(),
        ..Default::default()
    }
}

/// C: `stream_connection_send_corrupt_frame()`.
pub(crate) fn build_corrupt_frame_message(start: u16, end: u16) -> TakionMessage {
    TakionMessage {
        r#type: takion_message::PayloadType::Corruptframe.into(),
        corrupt_payload: Some(CorruptFramePayload {
            start: start as u32,
            end: end as u32,
        }),
        ..Default::default()
    }
}

/// C: `stream_connection_send_idr_request()`.
pub(crate) fn build_idr_request_message() -> TakionMessage {
    TakionMessage {
        r#type: takion_message::PayloadType::Idrrequest.into(),
        ..Default::default()
    }
}

/// Nachricht an Takion senden (C: chiaki_takion_send_message_data-Aufruf der
/// jeweiligen send_*-Funktion inkl. chunk_flags/channel).
fn send_msg(takion: &Takion, msg: &TakionMessage, chunk_flags: u8, channel: u16) -> ChiakiResult<()> {
    let buf = msg.encode_to_vec();
    takion.send_message_data(chunk_flags, channel, &buf)?;
    Ok(())
}

/// C: `stream_connection_send_controller_connection()` (channel 1).
fn send_controller_connection(takion: &Takion, enable_dualsense: bool) -> ChiakiResult<()> {
    send_msg(takion, &build_controller_connection(enable_dualsense), 1, 1)
}

/// C: `stream_connection_enable_microphone()` (channel 1).
fn send_mic_streaminfo(takion: &Takion) -> ChiakiResult<()> {
    send_msg(takion, &build_mic_streaminfo()?, 1, 1)
}

/// C: `stream_connection_send_streaminfo_ack()` (channel 9!).
fn send_streaminfo_ack(takion: &Takion) -> ChiakiResult<()> {
    send_msg(takion, &build_streaminfo_ack(), 1, 9)
}

/// C: `stream_connection_send_disconnect()` (channel 1).
pub(crate) fn send_disconnect(takion: &Takion) -> ChiakiResult<()> {
    tracing::info!("StreamConnection sending Disconnect");
    send_msg(takion, &build_disconnect_message(), 1, 1)
}

/// C: `stream_connection_send_heartbeat()` (channel 1).
fn send_heartbeat(takion: &Takion) -> ChiakiResult<()> {
    send_msg(takion, &build_heartbeat_message(), 1, 1)
}

/// C: `stream_connection_send_corrupt_frame()` (channel 2).
pub(crate) fn send_corrupt_frame(takion: &Takion, start: u16, end: u16) -> ChiakiResult<()> {
    tracing::warn!(
        "StreamConnection reporting corrupt frame(s) from {} to {}",
        start,
        end
    );
    send_msg(takion, &build_corrupt_frame_message(start, end), 1, 2)
}

/// C: `stream_connection_send_idr_request()` (channel 2).
pub(crate) fn send_idr_request(takion: &Takion) -> ChiakiResult<()> {
    tracing::info!("StreamConnection requesting IDR frame");
    send_msg(takion, &build_idr_request_message(), 1, 2)
}

/// C: `stream_connection_init_crypt()` — baut beide GKCrypt-Instanzen
/// (lokal Index 2, remote Index 3) aus handshake_key + ECDH-Secret.
pub(crate) fn init_crypt(
    handshake_key: &[u8; HANDSHAKE_KEY_SIZE],
    ecdh_secret: &[u8; crate::ecdh::ECDH_SECRET_SIZE],
) -> ChiakiResult<(Arc<GKCrypt>, Arc<GKCrypt>)> {
    let gkcrypt_local =
        GKCrypt::new(GKCRYPT_KEY_BUF_BLOCKS_DEFAULT, 2, handshake_key, ecdh_secret).map_err(|_| {
            tracing::error!("StreamConnection failed to initialize local GKCrypt with index 2");
            ChiakiError::Unknown
        })?;
    let gkcrypt_remote =
        GKCrypt::new(GKCRYPT_KEY_BUF_BLOCKS_DEFAULT, 3, handshake_key, ecdh_secret).map_err(|_| {
            tracing::error!("StreamConnection failed to initialize remote GKCrypt with index 3");
            ChiakiError::Unknown
        })?;
    Ok((gkcrypt_local, gkcrypt_remote))
}

// ----------------------------------------------------------------------
// Takion-Callback (C: stream_connection_takion_cb + Data-Handler)
// ----------------------------------------------------------------------

fn takion_cb(shared: &Arc<StreamConnectionShared>, event: TakionEvent) {
    match event {
        TakionEvent::Connected | TakionEvent::Disconnect(_) => {
            let connected = matches!(event, TakionEvent::Connected);
            let mut st = lock(&shared.state);
            if st.state == StreamConnectionState::TakionConnect {
                st.state_finished = connected;
                st.state_failed = !connected;
                shared.state_cond.notify_all();
            }
        }
        TakionEvent::Data { data_type, buf } => stream_connection_takion_data(shared, data_type, &buf),
        TakionEvent::Av(packet) => stream_connection_takion_av(shared, packet),
        TakionEvent::DataAck { .. } => {}
    }
}

/// C: `stream_connection_takion_data()`.
fn stream_connection_takion_data(
    shared: &Arc<StreamConnectionShared>,
    data_type: TakionMessageDataType,
    buf: &[u8],
) {
    match data_type {
        TakionMessageDataType::Protobuf => {
            let mut st = lock(&shared.state);
            match st.state {
                StreamConnectionState::ExpectBang => {
                    stream_connection_takion_data_expect_bang(shared, buf, &mut st)
                }
                StreamConnectionState::ExpectStreaminfo => {
                    stream_connection_takion_data_expect_streaminfo(shared, buf, &mut st)
                }
                // STATE_IDLE (und jeder andere Zustand)
                _ => stream_connection_takion_data_idle(shared, buf, &mut st),
            }
        }
        TakionMessageDataType::Rumble => stream_connection_takion_data_rumble(shared, buf),
        TakionMessageDataType::PadInfo => stream_connection_takion_data_pad_info(shared, buf),
        TakionMessageDataType::TriggerEffects => {
            stream_connection_takion_data_trigger_effects(shared, buf)
        }
    }
}

/// C: `stream_connection_takion_data_rumble()`.
fn stream_connection_takion_data_rumble(shared: &Arc<StreamConnectionShared>, buf: &[u8]) {
    if buf.len() < 3 {
        tracing::error!("StreamConnection got rumble packet with size {:#x} < 3", buf.len());
        return;
    }
    shared.ctx.send_event(SessionEvent::Rumble {
        unknown: buf[0],
        left: buf[1],
        right: buf[2],
    });
}

/// C: `stream_connection_takion_data_trigger_effects()`.
fn stream_connection_takion_data_trigger_effects(shared: &Arc<StreamConnectionShared>, buf: &[u8]) {
    if buf.len() < 25 {
        tracing::error!(
            "StreamConnection got trigger effects packet with size {:#x} < 25",
            buf.len()
        );
        return;
    }
    let mut left = [0u8; 10];
    let mut right = [0u8; 10];
    left.copy_from_slice(&buf[5..15]);
    right.copy_from_slice(&buf[15..25]);
    shared.ctx.send_event(SessionEvent::TriggerEffects {
        type_left: buf[1],
        type_right: buf[2],
        left,
        right,
    });
}

/// C: `stream_connection_takion_data_pad_info()` — verarbeitet beide
/// Paketgrößen (0x19 mit Feedback-Seq-Num, 0x11 kompakt) und liefert die
/// Motion-Reset-/Intensity-/LED-/Player-Index-Events.
fn stream_connection_takion_data_pad_info(shared: &Arc<StreamConnectionShared>, buf: &[u8]) {
    tracing::trace!("Pad info packet: {:02x?}", buf);

    let mut led_changed = false;
    let mut player_index_changed = false;
    let mut motion_reset = false;
    let mut haptic_intensity_changed = false;
    let mut trigger_intensity_changed = false;

    let mut pad = lock(&shared.pad);

    match buf.len() {
        0x19 => {
            // sequence number of feedback packet this is responding to
            let feedback_packet_seq_num = u16::from_be_bytes([buf[0], buf[1]]);
            // int16_t unknown = i16::from_be_bytes([buf[2], buf[3]]);
            let timestamp = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
            let haptic = pad.haptic_intensity.map(|v| v as u8).unwrap_or(0xff);
            let trigger = pad.trigger_intensity.map(|v| v as u8).unwrap_or(0xff);
            if haptic != buf[20] {
                pad.haptic_intensity = DualSenseEffectIntensity::from_u8(buf[20]);
                haptic_intensity_changed = true;
            }
            if trigger != buf[21] {
                pad.trigger_intensity = DualSenseEffectIntensity::from_u8(buf[21]);
                trigger_intensity_changed = true;
            }
            if buf[12] != 0 {
                motion_reset = true;
                tracing::trace!(
                    "StreamConnection received motion reset request in response to feedback packet with seqnum {:x} , {} seconds after stream began",
                    feedback_packet_seq_num, timestamp
                );
            }
            if buf[8] != pad.player_index {
                player_index_changed = true;
                pad.player_index = buf[8];
            }
            if buf[9..12] != pad.led_state {
                led_changed = true;
                pad.led_state.copy_from_slice(&buf[9..12]);
            }
        }
        0x11 => {
            let haptic = pad.haptic_intensity.map(|v| v as u8).unwrap_or(0xff);
            let trigger = pad.trigger_intensity.map(|v| v as u8).unwrap_or(0xff);
            if haptic != buf[12] {
                pad.haptic_intensity = DualSenseEffectIntensity::from_u8(buf[12]);
                haptic_intensity_changed = true;
            }
            if trigger != buf[13] {
                pad.trigger_intensity = DualSenseEffectIntensity::from_u8(buf[13]);
                trigger_intensity_changed = true;
            }
            if buf[4] != 0 {
                motion_reset = true;
            }
            if buf[0] != pad.player_index {
                player_index_changed = true;
                pad.player_index = buf[0];
            }
            if buf[1..4] != pad.led_state {
                led_changed = true;
                pad.led_state.copy_from_slice(&buf[1..4]);
            }
        }
        _ => {
            tracing::error!(
                "StreamConnection got pad info with size {:#x} not equal to 0x19 or 0x11",
                buf.len()
            );
            return;
        }
    }

    let haptic_intensity = pad.haptic_intensity;
    let trigger_intensity = pad.trigger_intensity;
    let led_state = pad.led_state;
    let player_index = pad.player_index;
    drop(pad);

    if motion_reset {
        tracing::info!("Setting motion control origin to current position");
        shared.ctx.send_event(SessionEvent::MotionReset);
    }
    if haptic_intensity_changed {
        tracing::info!(
            "Set haptic intensity to: {}",
            haptic_intensity.map(|v| v.as_str()).unwrap_or("Invalid")
        );
        if let Some(intensity) = haptic_intensity {
            shared.ctx.send_event(SessionEvent::HapticIntensity(intensity));
        }
    }
    if trigger_intensity_changed {
        tracing::info!(
            "Set adaptive trigger intensity to: {}",
            trigger_intensity.map(|v| v.as_str()).unwrap_or("Invalid")
        );
        if let Some(intensity) = trigger_intensity {
            shared.ctx.send_event(SessionEvent::TriggerIntensity(intensity));
        }
    }
    if led_changed {
        tracing::trace!(
            "Set LED state to - red: {:x}, green: {:x}, blue: {:x}",
            led_state[0], led_state[1], led_state[2]
        );
        shared.ctx.send_event(SessionEvent::LedColor(led_state));
    }
    if player_index_changed {
        tracing::trace!("Set player index to - {}", player_index);
        shared.ctx.send_event(SessionEvent::PlayerIndex(player_index));
    }
}

/// C: `stream_connection_takion_data_handle_disconnect()`.
fn stream_connection_takion_data_handle_disconnect(
    shared: &Arc<StreamConnectionShared>,
    buf: &[u8],
    st: &mut ScState,
) {
    let msg = match decode_takion_message(buf) {
        Ok(msg) => msg,
        Err(_) => {
            tracing::error!("StreamConnection failed to decode data protobuf");
            return;
        }
    };
    let reason = msg.disconnect_payload.map(|p| p.reason).unwrap_or_default();

    tracing::info!(
        "Remote disconnected from StreamConnection with reason \"{}\"",
        reason
    );

    st.remote_disconnected = true;
    st.remote_disconnect_reason = Some(reason);
    shared.state_cond.notify_all();
}

/// C: `stream_connection_takion_data_idle()` (Protobuf im STATE_IDLE).
fn stream_connection_takion_data_idle(
    shared: &Arc<StreamConnectionShared>,
    buf: &[u8],
    st: &mut ScState,
) {
    let msg = match decode_takion_message(buf) {
        Ok(msg) => msg,
        Err(_) => {
            tracing::error!("StreamConnection failed to decode data protobuf");
            return;
        }
    };

    tracing::trace!("StreamConnection received data with msg.type == {}", msg.r#type);

    use takion_message::PayloadType;
    match PayloadType::try_from(msg.r#type) {
        Ok(PayloadType::Disconnect) => stream_connection_takion_data_handle_disconnect(shared, buf, st),
        Ok(PayloadType::Connectionquality) => {
            if let Some(q) = &msg.connection_quality_payload {
                tracing::trace!(
                    "StreamConnection received connection quality: target_bitrate={}, upstream_bitrate={}, upstream_loss={:.4}, disable_upstream_audio={}, rtt={:.4}, loss={}",
                    q.target_bitrate.unwrap_or(0),
                    q.upstream_bitrate.unwrap_or(0),
                    q.upstream_loss.unwrap_or(0.0),
                    q.disable_upstream_audio.unwrap_or(false) as u8,
                    q.rtt.unwrap_or(0.0),
                    q.loss.unwrap_or(0)
                );
            }
            // C: measured bitrate aus den FrameProcessor-Stream-Stats; hier
            // aus der eigenen Statistik der StreamConnection (siehe Modul-
            // kommentar) mit anschließendem Reset.
            let max_fps = lock(&shared.ctx.video_profile).max_fps as u64;
            let mut stats = lock(&shared.stream_stats);
            let bitrate = stats.bitrate(max_fps);
            let measured = bitrate as f64 / 1_000_000.0;
            shared
                .measured_bitrate_bits
                .store(measured.to_bits(), Ordering::Relaxed);
            tracing::trace!("StreamConnection measured bitrate: {:.4} MBit/s", measured);
            stats.reset();
        }
        Ok(PayloadType::Corruptframe) => {
            if let Some(corrupt) = &msg.corrupt_payload {
                tracing::error!(
                    "StreamConnection received corrupt frame from {} to {}",
                    corrupt.start,
                    corrupt.end
                );
            }
        }
        Ok(PayloadType::Streaminfoack) => {
            tracing::trace!("StreamConnection received streaminfo ack");
        }
        _ => {}
    }
}

/// C: `stream_connection_takion_data_expect_bang()` — verarbeitet den
/// BANG-Payload (ECDH pub key + sig), leitet das Secret ab und aktiviert
/// beidseitig die Verschlüsselung. Erwartet gehaltene `state`-Mutex
/// (wie im C).
fn stream_connection_takion_data_expect_bang(
    shared: &Arc<StreamConnectionShared>,
    buf: &[u8],
    st: &mut ScState,
) {
    let msg = match decode_takion_message(buf) {
        Ok(msg) => msg,
        Err(_) => {
            tracing::error!("StreamConnection failed to decode data protobuf");
            return;
        }
    };

    use takion_message::PayloadType;
    let payload_type = PayloadType::try_from(msg.r#type).ok();
    if payload_type != Some(PayloadType::Bang) || msg.bang_payload.is_none() {
        if payload_type == Some(PayloadType::Disconnect) {
            stream_connection_takion_data_handle_disconnect(shared, buf, st);
            return;
        }
        if payload_type == Some(PayloadType::Streaminfo) && st.streaminfo_early_buf.is_none() {
            st.streaminfo_early_buf = Some(buf.to_vec());
            tracing::info!("StreamConnection received streaminfo early, saving ...");
            return;
        }
        tracing::warn!(
            "StreamConnection expected bang payload but received something else: {}",
            msg.r#type
        );
        return;
    }

    let bang = msg.bang_payload.expect("checked above");
    tracing::info!("BANG received");

    // Fehlerpfad des C (goto error): state_failed setzen + wecken.
    macro_rules! bang_error {
        ($($arg:tt)*) => {{
            tracing::error!($($arg)*);
            st.state_failed = true;
            shared.state_cond.notify_all();
            return;
        }};
    }

    if !bang.version_accepted {
        bang_error!("StreamConnection bang remote didn't accept version");
    }
    if !bang.encrypted_key_accepted {
        bang_error!("StreamConnection bang remote didn't accept encrypted key");
    }
    let ecdh_pub_key = match bang.ecdh_pub_key.as_deref() {
        Some(k) if !k.is_empty() => k,
        _ => bang_error!("StreamConnection didn't get remote ECDH pub key from bang"),
    };
    let ecdh_sig = match bang.ecdh_sig.as_deref() {
        Some(s) if !s.is_empty() => s,
        _ => bang_error!("StreamConnection didn't get remote ECDH sig from bang"),
    };

    // ECDH-Secret ableiten (C: chiaki_ecdh_derive_secret mit dem
    // handshake_key der Session).
    let handshake_key = *lock(&shared.ctx.handshake_key);
    let derived = {
        let mut ecdh_guard = lock(&shared.ctx.ecdh);
        match ecdh_guard.as_mut() {
            Some(ecdh) => ecdh.derive_secret(ecdh_pub_key, &handshake_key, ecdh_sig),
            None => Err(ChiakiError::Uninitialized),
        }
    };
    let ecdh_secret = match derived {
        Ok(secret) => secret,
        Err(e) => bang_error!("StreamConnection failed to derive secret from bang: {}", e),
    };

    // Verschlüsselung initialisieren (C: stream_connection_init_crypt)
    match init_crypt(&handshake_key, &ecdh_secret) {
        Ok((local, remote)) => {
            *lock(&shared.gkcrypt_local) = Some(Arc::clone(&local));
            *lock(&shared.gkcrypt_remote) = Some(Arc::clone(&remote));
            if let Some(takion) = &*lock(&shared.takion) {
                takion.set_crypt(Some(local), Some(remote));
            }
        }
        Err(_) => {
            bang_error!("StreamConnection failed to init crypt after receiving bang");
        }
    }

    // stream_connection->state_mutex is expected to be locked by the caller
    st.state_finished = true;
    shared.state_cond.notify_all();
}

/// STREAMINFO-Payload parsen (C-Teil von
/// `stream_connection_takion_data_expect_streaminfo`): Audio-Header (exakt
/// `AUDIO_HEADER_SIZE` Bytes) + Video-Profile (Header werden — wie im C —
/// mit `VIDEO_BUFFER_PADDING_SIZE` Null-Bytes aufgefüllt).
pub(crate) fn parse_streaminfo_payload(buf: &[u8]) -> ChiakiResult<(AudioHeader, Vec<VideoProfile>)> {
    let msg = decode_takion_message(buf).map_err(|_| ChiakiError::InvalidData)?;
    let payload = msg.stream_info_payload.ok_or(ChiakiError::InvalidData)?;

    if payload.audio_header.len() != crate::audio::AUDIO_HEADER_SIZE {
        return Err(ChiakiError::InvalidData);
    }
    let audio_header = AudioHeader::load(&payload.audio_header)?;

    let mut profiles = Vec::new();
    for resolution in &payload.resolution {
        let mut header = resolution.video_header.clone();
        header.resize(header.len() + VIDEO_BUFFER_PADDING_SIZE, 0);
        profiles.push(VideoProfile {
            width: resolution.width,
            height: resolution.height,
            header,
        });
    }

    Ok((audio_header, profiles))
}

/// C: `stream_connection_takion_data_expect_streaminfo()` — erwartet
/// gehaltene `state`-Mutex (wie im C).
fn stream_connection_takion_data_expect_streaminfo(
    shared: &Arc<StreamConnectionShared>,
    buf: &[u8],
    st: &mut ScState,
) {
    let msg = match decode_takion_message(buf) {
        Ok(msg) => msg,
        Err(_) => {
            tracing::error!("StreamConnection failed to decode data protobuf");
            return;
        }
    };

    use takion_message::PayloadType;
    let payload_type = PayloadType::try_from(msg.r#type).ok();
    if payload_type != Some(PayloadType::Streaminfo) || msg.stream_info_payload.is_none() {
        if payload_type == Some(PayloadType::Disconnect) {
            stream_connection_takion_data_handle_disconnect(shared, buf, st);
            return;
        }
        tracing::warn!("StreamConnection expected streaminfo payload but received something else");
        return;
    }

    let (audio_header, profiles) = match parse_streaminfo_payload(buf) {
        Ok(v) => v,
        Err(_) => {
            tracing::error!("StreamConnection received invalid audio header in streaminfo");
            st.state_failed = true;
            shared.state_cond.notify_all();
            return;
        }
    };

    tracing::info!("StreamConnection received audio header:");

    // Receiver mit Stream-Info versorgen (das Audio-Header-Event feuert
    // dabei unter der state-Mutex — wie im C).
    if let Some(receiver) = &*lock(&shared.audio_receiver) {
        receiver.stream_info(&audio_header);
    }
    if let Some(receiver) = &*lock(&shared.video_receiver) {
        receiver.stream_info(profiles);
    }

    // TODO: do some checks?

    let takion = lock(&shared.takion).clone();
    let Some(takion) = takion else {
        // kann im Produktivbetrieb nicht passieren (Handler läuft im
        // Takion-Callback); sauber als Fehler behandeln.
        tracing::error!("StreamConnection has no takion while handling streaminfo");
        st.state_failed = true;
        shared.state_cond.notify_all();
        return;
    };

    if let Err(_e) = send_streaminfo_ack(&takion) {
        tracing::error!("StreamConnection failed to send streaminfo ack");
        st.state_failed = true;
        shared.state_cond.notify_all();
        return;
    }

    if let Err(_e) = send_controller_connection(&takion, shared.ctx.enable_dualsense) {
        tracing::error!("StreamConnection failed to send controller connection");
        st.state_failed = true;
        shared.state_cond.notify_all();
        return;
    }

    if let Err(_e) = send_mic_streaminfo(&takion) {
        tracing::error!("StreamConnection failed to enable microphone input");
        st.state_failed = true;
        shared.state_cond.notify_all();
        return;
    }

    // stream_connection->state_mutex is expected to be locked by the caller
    st.state_finished = true;
    shared.state_cond.notify_all();
}

/// C: `stream_connection_takion_av()` — Decrypt mit gkcrypt_remote und
/// Verteilung an die Receiver; zählt zusätzlich die Video-Bytes für die
/// CONNECTIONQUALITY-Statistik (siehe Modul-Kommentar).
fn stream_connection_takion_av(shared: &Arc<StreamConnectionShared>, mut packet: Box<AVPacket>) {
    let gkcrypt_remote = lock(&shared.gkcrypt_remote).clone();
    if let Some(crypt) = &gkcrypt_remote {
        if let Err(e) = crypt.decrypt(packet.key_pos + GKCRYPT_BLOCK_SIZE as u64, &mut packet.data)
        {
            tracing::error!("StreamConnection failed to decrypt AV packet: {e}");
        }
    }

    if packet.is_video {
        {
            let mut stats = lock(&shared.stream_stats);
            stats.frame(packet.data.len() as u64);
        }
        if let Some(receiver) = &*lock(&shared.video_receiver) {
            receiver.av_packet(&packet);
        }
    } else if packet.is_haptics {
        if let Some(receiver) = &*lock(&shared.haptics_receiver) {
            receiver.av_packet(&packet);
        }
    } else if let Some(receiver) = &*lock(&shared.audio_receiver) {
        receiver.av_packet(&packet);
    }
}

// ----------------------------------------------------------------------
// Tests: Formatter/Parser/State-Übergänge, die ohne PS5 isolierbar sind.
// ----------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::ControllerState;
    use std::sync::Mutex as StdMutex;

    /// Sammelt alle SessionEvents zum Auswerten.
    #[derive(Default, Clone)]
    struct EventCollector {
        events: Arc<StdMutex<Vec<SessionEvent>>>,
    }

    impl EventCollector {
        fn get(&self) -> Vec<SessionEvent> {
            self.events.lock().unwrap().clone()
        }

        fn clear(&self) {
            self.events.lock().unwrap().clear();
        }
    }

    fn test_callbacks(collector: &EventCollector) -> Arc<dyn crate::session::SessionCallbacks> {
        let events = collector.events.clone();
        Arc::new(crate::session::TestSessionCallbacks::new(move |ev| {
            events.lock().unwrap().push(ev);
        }))
    }

    fn make_shared() -> (Arc<SessionShared>, EventCollector) {
        let collector = EventCollector::default();
        let ctx = Arc::new(SessionShared::new(test_callbacks(&collector)));
        (ctx, collector)
    }

    fn build_sc_shared(ctx: &Arc<SessionShared>) -> Arc<StreamConnectionShared> {
        Arc::new(StreamConnectionShared {
            state: Mutex::new(ScState::default()),
            state_cond: Condvar::new(),
            ctx: Arc::clone(ctx),
            gkcrypt_remote: Mutex::new(None),
            gkcrypt_local: Mutex::new(None),
            takion: Mutex::new(None),
            audio_receiver: Mutex::new(None),
            haptics_receiver: Mutex::new(None),
            video_receiver: Mutex::new(None),
            feedback: Mutex::new(FeedbackSlot {
                sender: None,
                active: false,
                controller_state: ControllerState::default(),
            }),
            packet_stats: Arc::new(PacketStats::new()),
            pad: Mutex::new(PadState {
                haptic_intensity: Some(DualSenseEffectIntensity::Strong),
                trigger_intensity: Some(DualSenseEffectIntensity::Strong),
                ..Default::default()
            }),
            stream_stats: Mutex::new(StreamStats::default()),
            measured_bitrate_bits: AtomicU64::new(0.0f64.to_bits()),
        })
    }

    /// Handgebaute nanopb-kompatible protobuf-Bytes (siehe proto.rs-Tests).
    mod pb {
        pub fn push_varint_field(out: &mut Vec<u8>, field_no: u32, value: u64) {
            push_varint(out, u64::from(field_no) << 3);
            push_varint(out, value);
        }

        pub fn push_len_delimited(out: &mut Vec<u8>, field_no: u32, payload: &[u8]) {
            push_varint(out, (u64::from(field_no) << 3) | 2);
            push_varint(out, payload.len() as u64);
            out.extend_from_slice(payload);
        }

        fn push_varint(out: &mut Vec<u8>, mut value: u64) {
            loop {
                let byte = (value & 0x7f) as u8;
                value >>= 7;
                if value == 0 {
                    out.push(byte);
                    break;
                }
                out.push(byte | 0x80);
            }
        }
    }

    #[test]
    fn heartbeat_message_is_single_field() {
        // TakionMessage{type=HEARTBEAT(3)}: Feld 1 varint 3.
        let mut expected = Vec::new();
        pb::push_varint_field(&mut expected, 1, 3);
        assert_eq!(build_heartbeat_message().encode_to_vec(), expected);
    }

    #[test]
    fn streaminfo_ack_message_is_single_field() {
        // TakionMessage{type=STREAMINFOACK(14)}.
        let mut expected = Vec::new();
        pb::push_varint_field(&mut expected, 1, 14);
        assert_eq!(build_streaminfo_ack().encode_to_vec(), expected);
    }

    #[test]
    fn idr_request_message_is_single_field() {
        // TakionMessage{type=IDRREQUEST(25)}.
        let mut expected = Vec::new();
        pb::push_varint_field(&mut expected, 1, 25);
        assert_eq!(build_idr_request_message().encode_to_vec(), expected);
    }

    #[test]
    fn disconnect_message_reason_is_wire_exact() {
        // TakionMessage{type=DISCONNECT(8), 10: {1: "Client Disconnecting"}}.
        let mut inner = Vec::new();
        pb::push_len_delimited(&mut inner, 1, b"Client Disconnecting");
        let mut expected = Vec::new();
        pb::push_varint_field(&mut expected, 1, 8);
        pb::push_len_delimited(&mut expected, 10, &inner);
        assert_eq!(build_disconnect_message().encode_to_vec(), expected);
    }

    #[test]
    fn corrupt_frame_message_fields_1_and_2() {
        // CorruptFramePayload{start=5, end=9} innerhalb type=CORRUPTFRAME(5).
        let mut inner = Vec::new();
        pb::push_varint_field(&mut inner, 1, 5);
        pb::push_varint_field(&mut inner, 2, 9);
        let mut expected = Vec::new();
        pb::push_varint_field(&mut expected, 1, 5);
        pb::push_len_delimited(&mut expected, 6, &inner);
        assert_eq!(build_corrupt_frame_message(5, 9).encode_to_vec(), expected);
    }

    #[test]
    fn controller_connection_matches_c_semantics() {
        // CONTROLLERCONNECTION(21), Payload{connected=true(2),
        // controller_type(3)}: DUALSENSE=6 / DUALSHOCK4=2.
        let build = |dualsense: bool| {
            let mut inner = Vec::new();
            pb::push_varint_field(&mut inner, 2, 1); // connected = true
            pb::push_varint_field(&mut inner, 3, if dualsense { 6 } else { 2 });
            let mut expected = Vec::new();
            pb::push_varint_field(&mut expected, 1, 21);
            pb::push_len_delimited(&mut expected, 22, &inner);
            build_controller_connection(dualsense).encode_to_vec()
        };
        assert_eq!(build(true), build(true));
        assert_eq!(build(false), build(false));
        assert_ne!(build(true), build(false));
    }

    #[test]
    fn mic_streaminfo_contains_audio_header_16_1_48000_480() {
        // STREAMINFO(13) mit audio_header(2) = 16 Kanäle/1 Bit?/48000/480
        // — exakt die C-Werte chiaki_audio_header_set(16, 1, 48000, 480).
        let mut header = [0u8; crate::audio::AUDIO_HEADER_SIZE];
        AudioHeader::set(16, 1, 48000, 480)
            .save(&mut header)
            .unwrap();

        let mut inner = Vec::new();
        pb::push_len_delimited(&mut inner, 2, &header);
        let mut expected = Vec::new();
        pb::push_varint_field(&mut expected, 1, 13);
        pb::push_len_delimited(&mut expected, 15, &inner);

        let msg = build_mic_streaminfo().unwrap().encode_to_vec();
        assert_eq!(msg, expected);
    }

    #[test]
    fn big_message_matches_nanopb_layout() {
        // BigPayload: 1 client_version, 2 session_key, 3 launch_spec,
        // 4 encrypted_key (4 Null-Bytes!), 5 ecdh_pub_key, 6 ecdh_sig.
        let mut inner = Vec::new();
        pb::push_varint_field(&mut inner, 1, 9);
        pb::push_len_delimited(&mut inner, 2, b"sessionid");
        pb::push_len_delimited(&mut inner, 3, b"b64spec");
        pb::push_len_delimited(&mut inner, 4, &[0, 0, 0, 0]);
        pb::push_len_delimited(&mut inner, 5, &[0xaa; 65]);
        pb::push_len_delimited(&mut inner, 6, &[0xbb; 32]);
        let mut expected = Vec::new();
        pb::push_varint_field(&mut expected, 1, 0); // BIG
        pb::push_len_delimited(&mut expected, 2, &inner);

        let msg = build_big_message(9, "sessionid", "b64spec", &[0xaa; 65], &[0xbb; 32]);
        assert_eq!(msg.encode_to_vec(), expected);
    }

    #[test]
    fn chunk_plan_single_chunk_when_small() {
        // 100 Bytes bei mtu=1400: 1400 < 126/125 nie wahr -> keine
        // Voll-Chunks, Rest-Sendung mit flags=1.
        assert_eq!(chunk_plan(100, 1400).unwrap(), vec![]);
    }

    #[test]
    fn chunk_plan_splits_large_payloads_like_c_loop() {
        // mtu=100: erste Runde: 100 < total+26 -> chunk 74 (mtu-26).
        // Danach (first=false): 100 < total+25 -> chunks von 75 (mtu-25).
        // Gesamt: ein 100-Byte-Payload wird nie gesplittet (100<126 falsch),
        // also 601 Bytes testen:
        let plan = chunk_plan(601, 100).unwrap();
        // Runde 1: 601 -> 527 (chunk 74)
        // Runde 2: 527 -> 452 (chunk 75) ... bis total=77:
        // 100 < 77+26=103 wahr -> chunk 75 -> total=2; dann
        // 100 < 2+26 falsch, 100 < 2+25 falsch -> Ende. Rest 2 -> flags=1.
        let sizes: Vec<usize> = plan.iter().map(|(_, s)| *s).collect();
        let sum: usize = sizes.iter().sum();
        assert_eq!(sum, 601 - 2);
        assert_eq!(sizes[0], 74);
        for s in &sizes[1..] {
            assert_eq!(*s, 75);
        }
        // Und für einen exakt passenden Payload bleibt nichts übrig:
        let plan = chunk_plan(74 + 75, 100).unwrap();
        let sum: usize = plan.iter().map(|(_, s)| s).sum();
        assert_eq!(sum, 74 + 75);
    }

    #[test]
    fn chunk_plan_rejects_tiny_mtu() {
        // mtu < 26 würde im C unterlaufen (UB; bei mtu==26 Endlosschleife
        // mit 0-Byte-Chunks); hier Overflow-Fehler. mtu==26 selbst ergäbe im
        // C einen 0-Byte-Chunk + 1-Byte-Followups — auch das verhindern wir:
        // der Plan liefert dann schlicht die C- Schleifensemantik mit
        // 0-Byte-Eintrag (kein Underflow).
        assert_eq!(chunk_plan(200, 25).unwrap_err(), ChiakiError::Overflow);
    }

    #[test]
    fn launchspec_b64_roundtrip_over_rpcrypt() {
        // Kette: JSON (+ trailing 0) -> rpcrypt-Keystream-XOR -> base64.
        // Gegenprobe: base64-Decodieren, XOR mit dem Klartext muss exakt den
        // rpcrypt-Keystream (encrypt(0, zeros)) ergeben — der Keystream ist
        // deterministisch, daher prüfen wir gegen eine zweite Instanz mit
        // denselben Keys.
        use crate::error::Target;
        use crate::rpcrypt::RPCRYPT_KEY_SIZE;
        let nonce = [7u8; RPCRYPT_KEY_SIZE];
        let morning = [9u8; RPCRYPT_KEY_SIZE];
        let spec = LaunchSpec {
            target: Target::Ps5_1,
            mtu: 1454,
            rtt: 30,
            handshake_key: [1; 16],
            width: 1920,
            height: 1080,
            max_fps: 60,
            codec: crate::error::Codec::H264,
            bw_kbps_sent: 15000,
        };

        let b64 = {
            let rpcrypt = Rpcrypt::new_auth(Target::Ps5_1, &nonce, &morning).unwrap();
            launchspec_b64(&spec, &Some(rpcrypt)).unwrap()
        };
        let enc = crate::base64::decode(b64.as_bytes()).unwrap();
        let json = launchspec_format(&spec).unwrap();
        assert_eq!(enc.len(), json.len() + 1); // inkl. trailing 0

        let mut plain = json.clone().into_bytes();
        plain.push(0);
        let keystream_rpcrypt = Rpcrypt::new_auth(Target::Ps5_1, &nonce, &morning).unwrap();
        let mut keystream = vec![0u8; enc.len()];
        keystream_rpcrypt.encrypt(0, &mut keystream).unwrap();
        let mut recovered = enc;
        xor_bytes(&mut recovered, &plain);
        assert_eq!(recovered, keystream);
    }

    #[test]
    fn rumble_event_dispatch_from_data_packet() {
        let (ctx, collector) = make_shared();
        let shared = build_sc_shared(&ctx);
        stream_connection_takion_data_rumble(&shared, &[0x11, 0x22, 0x33]);
        match &collector.get()[..] {
            [SessionEvent::Rumble { unknown, left, right }] => {
                assert_eq!((*unknown, *left, *right), (0x11, 0x22, 0x33));
            }
            other => panic!("unexpected events: {other:?}"),
        }

        collector.clear();
        stream_connection_takion_data_rumble(&shared, &[1, 2]); // zu kurz
        assert!(collector.get().is_empty());
    }

    #[test]
    fn trigger_effects_event_dispatch_from_data_packet() {
        let (ctx, collector) = make_shared();
        let shared = build_sc_shared(&ctx);

        let mut buf = [0u8; 25];
        buf[1] = 0x02; // type_left
        buf[2] = 0x01; // type_right
        for (i, b) in buf[5..15].iter_mut().enumerate() {
            *b = i as u8 + 1;
        }
        for (i, b) in buf[15..25].iter_mut().enumerate() {
            *b = i as u8 + 20;
        }
        stream_connection_takion_data_trigger_effects(&shared, &buf);
        match &collector.get()[..] {
            [SessionEvent::TriggerEffects {
                type_left,
                type_right,
                left,
                right,
            }] => {
                assert_eq!(*type_left, 0x02);
                assert_eq!(*type_right, 0x01);
                assert_eq!(*left, (1..=10u8).collect::<Vec<_>>().as_slice());
                assert_eq!(*right, (20..=29u8).collect::<Vec<_>>().as_slice());
            }
            other => panic!("unexpected events: {other:?}"),
        }

        collector.clear();
        stream_connection_takion_data_trigger_effects(&shared, &[0; 24]); // zu kurz
        assert!(collector.get().is_empty());
    }

    #[test]
    fn pad_info_0x19_dispatches_led_player_motion_intensity() {
        let (ctx, collector) = make_shared();
        let shared = build_sc_shared(&ctx);

        let mut buf = [0u8; 0x19];
        buf[0..2].copy_from_slice(&0x1234u16.to_be_bytes()); // feedback seq
        buf[4..8].copy_from_slice(&60u32.to_be_bytes()); // timestamp
        buf[8] = 1; // player index
        buf[9..12].copy_from_slice(&[0x10, 0x20, 0x30]); // LED
        buf[12] = 1; // motion reset
        buf[20] = DualSenseEffectIntensity::Weak as u8; // haptic
        buf[21] = DualSenseEffectIntensity::Medium as u8; // trigger

        stream_connection_takion_data_pad_info(&shared, &buf);

        let events = collector.get();
        assert!(events.contains(&SessionEvent::MotionReset));
        assert!(events.contains(&SessionEvent::HapticIntensity(DualSenseEffectIntensity::Weak)));
        assert!(events.contains(&SessionEvent::TriggerIntensity(DualSenseEffectIntensity::Medium)));
        assert!(events.contains(&SessionEvent::LedColor([0x10, 0x20, 0x30])));
        assert!(events.contains(&SessionEvent::PlayerIndex(1)));
        assert_eq!(events.len(), 5);
    }

    #[test]
    fn pad_info_0x11_dispatches_changes_only() {
        let (ctx, collector) = make_shared();
        let shared = build_sc_shared(&ctx);

        // Keine Änderungen: Player 0, LED 000, Intensities Strong (Initial),
        // kein Motion-Reset -> gar kein Event.
        let mut buf = [0u8; 0x11];
        buf[0] = 0;
        buf[1..4].copy_from_slice(&[0, 0, 0]);
        buf[12] = DualSenseEffectIntensity::Strong as u8;
        buf[13] = DualSenseEffectIntensity::Strong as u8;
        stream_connection_takion_data_pad_info(&shared, &buf);
        assert!(collector.get().is_empty());

        // Zweites Paket: nur LED geändert.
        buf[1..4].copy_from_slice(&[1, 2, 3]);
        stream_connection_takion_data_pad_info(&shared, &buf);
        match &collector.get()[..] {
            [SessionEvent::LedColor(led)] => assert_eq!(*led, [1, 2, 3]),
            other => panic!("unexpected events: {other:?}"),
        }

        // Falsche Größe -> Fehler, kein Event.
        collector.clear();
        stream_connection_takion_data_pad_info(&shared, &[0; 5]);
        assert!(collector.get().is_empty());
    }

    #[test]
    fn expect_bang_derives_secret_and_enables_crypt() {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        let (ctx, _collector) = make_shared();
        let shared = build_sc_shared(&ctx);

        let handshake_key = [0x42u8; HANDSHAKE_KEY_SIZE];
        *lock(&ctx.handshake_key) = handshake_key;
        *lock(&ctx.ecdh) = Some(crate::ecdh::Ecdh::new().unwrap());
        let (pub_key, _sig) = lock(&ctx.ecdh)
            .as_ref()
            .unwrap()
            .get_local_pub_key(&handshake_key)
            .unwrap();

        // Signatur wie die Console: HMAC-SHA256(handshake_key, pub_key).
        let mut mac = Hmac::<Sha256>::new_from_slice(&handshake_key).unwrap();
        mac.update(&pub_key);
        let sig = mac.finalize().into_bytes().to_vec();

        let bang = TakionMessage {
            r#type: takion_message::PayloadType::Bang.into(),
            bang_payload: Some(BangPayload {
                server_version: 12,
                token: 0,
                encrypted_key_accepted: true,
                version_accepted: true,
                session_key: "session".to_owned(),
                extended_info: None,
                server_version_string: None,
                ecdh_pub_key: Some(pub_key.to_vec()),
                ecdh_sig: Some(sig),
            }),
            ..Default::default()
        };
        let buf = bang.encode_to_vec();

        {
            let mut st = lock(&shared.state);
            st.state = StreamConnectionState::ExpectBang;
            stream_connection_takion_data_expect_bang(&shared, &buf, &mut st);
            assert!(st.state_finished, "bang must finish the state");
            assert!(!st.state_failed);
        }

        let local = lock(&shared.gkcrypt_local).clone().expect("local crypt");
        let remote = lock(&shared.gkcrypt_remote).clone().expect("remote crypt");
        assert_eq!(local.index(), 2);
        assert_eq!(remote.index(), 3);

        // Bang ablehnen bei nicht akzeptierter Version.
        let shared2 = build_sc_shared(&ctx);
        let bang_reject = TakionMessage {
            r#type: takion_message::PayloadType::Bang.into(),
            bang_payload: Some(BangPayload {
                version_accepted: false,
                encrypted_key_accepted: true,
                ..bang.bang_payload.clone().unwrap()
            }),
            ..Default::default()
        };
        {
            let mut st = lock(&shared2.state);
            st.state = StreamConnectionState::ExpectBang;
            stream_connection_takion_data_expect_bang(
                &shared2,
                &bang_reject.encode_to_vec(),
                &mut st,
            );
            assert!(!st.state_finished);
            assert!(st.state_failed, "rejected version must fail the state");
        }
    }

    #[test]
    fn expect_bang_saves_early_streaminfo() {
        let (ctx, _collector) = make_shared();
        let shared = build_sc_shared(&ctx);

        let msg = TakionMessage {
            r#type: takion_message::PayloadType::Streaminfo.into(),
            stream_info_payload: Some(StreamInfoPayload {
                audio_header: vec![0; crate::audio::AUDIO_HEADER_SIZE],
                ..Default::default()
            }),
            ..Default::default()
        };
        let buf = msg.encode_to_vec();

        {
            let mut st = lock(&shared.state);
            st.state = StreamConnectionState::ExpectBang;
            stream_connection_takion_data_expect_bang(&shared, &buf, &mut st);
            assert!(st.streaminfo_early_buf.is_some());
            assert!(!st.state_finished);
        }
    }

    #[test]
    fn disconnect_payload_sets_remote_disconnected() {
        let (ctx, _collector) = make_shared();
        let shared = build_sc_shared(&ctx);

        let msg = TakionMessage {
            r#type: takion_message::PayloadType::Disconnect.into(),
            disconnect_payload: Some(DisconnectPayload {
                reason: "Server shutting down".to_owned(),
                extended_info: None,
            }),
            ..Default::default()
        };

        {
            let mut st = lock(&shared.state);
            stream_connection_takion_data_handle_disconnect(
                &shared,
                &msg.encode_to_vec(),
                &mut st,
            );
            assert!(st.remote_disconnected);
            assert_eq!(st.remote_disconnect_reason.as_deref(), Some("Server shutting down"));
        }
    }

    #[test]
    fn parse_streaminfo_payload_audio_and_profiles() {
        // Header-Bytes im load-Layout (buf[0]=channels, buf[1]=bits, ...):
        // Achtung — AudioHeader::save schreibt (wie im C) bits/channels
        // vertauscht, daher hier die Bytes direkt aufbauen.
        let mut header = [0u8; crate::audio::AUDIO_HEADER_SIZE];
        header[0] = 2; // channels
        header[1] = 16; // bits
        header[2..6].copy_from_slice(&48000u32.to_be_bytes());
        header[6..10].copy_from_slice(&480u32.to_be_bytes());
        header[0xa..0xe].copy_from_slice(&1u32.to_be_bytes()); // unknown = 1

        let mut res0 = Vec::new();
        pb::push_varint_field(&mut res0, 1, 1280);
        pb::push_varint_field(&mut res0, 2, 720);
        pb::push_len_delimited(&mut res0, 3, &[0, 0, 0, 1, 0x67]);

        let mut payload = Vec::new();
        pb::push_len_delimited(&mut payload, 1, &res0);
        pb::push_len_delimited(&mut payload, 2, &header);

        let mut msg = Vec::new();
        pb::push_varint_field(&mut msg, 1, 13);
        pb::push_len_delimited(&mut msg, 15, &payload);

        let (audio, profiles) = parse_streaminfo_payload(&msg).unwrap();
        assert_eq!((audio.channels, audio.bits, audio.rate, audio.frame_size), (2, 16, 48000, 480));
        assert_eq!(profiles.len(), 1);
        assert_eq!((profiles[0].width, profiles[0].height), (1280, 720));
        // Padding: 5 Nutzbytes + VIDEO_BUFFER_PADDING_SIZE Null-Bytes.
        assert_eq!(profiles[0].header.len(), 5 + VIDEO_BUFFER_PADDING_SIZE);
        assert_eq!(&profiles[0].header[..5], &[0, 0, 0, 1, 0x67]);
        assert!(profiles[0].header[5..].iter().all(|b| *b == 0));
    }

    #[test]
    fn parse_streaminfo_rejects_bad_audio_header() {
        let mut msg = Vec::new();
        pb::push_varint_field(&mut msg, 1, 13);
        pb::push_len_delimited(&mut msg, 15, &[0x0a, 0x02, 1, 2]); // payload mit 2-Byte-Header
        assert_eq!(
            parse_streaminfo_payload(&msg).unwrap_err(),
            ChiakiError::InvalidData
        );
    }

    #[test]
    fn state_cond_wait_reports_stop_finished_and_timeout() {
        let (ctx, _collector) = make_shared();
        let shared = build_sc_shared(&ctx);

        // Timeout ohne Änderung
        assert_eq!(
            state_cond_timedwait(&shared, 20).unwrap_err(),
            ChiakiError::Timeout
        );

        // should_stop erfüllt das Prädikat (wie im C)
        lock(&shared.state).should_stop = true;
        assert_eq!(state_cond_timedwait(&shared, 20), Ok(()));

        // state_finished erfüllt das Prädikat
        let shared2 = build_sc_shared(&ctx);
        lock(&shared2.state).state_finished = true;
        assert_eq!(state_cond_timedwait(&shared2, 20), Ok(()));

        // remote_disconnected erfüllt das Prädikat
        let shared3 = build_sc_shared(&ctx);
        lock(&shared3.state).remote_disconnected = true;
        assert_eq!(state_cond_timedwait(&shared3, 20), Ok(()));
    }

    #[test]
    fn init_crypt_builds_indexes_2_and_3() {
        let handshake_key = [3u8; HANDSHAKE_KEY_SIZE];
        let secret = [4u8; crate::ecdh::ECDH_SECRET_SIZE];
        let (local, remote) = init_crypt(&handshake_key, &secret).unwrap();
        assert_eq!(local.index(), 2);
        assert_eq!(remote.index(), 3);
        // Der GKCrypt-Index geht in die Key-Ableitung ein (C: gen_key_iv) —
        // lokal (2) und remote (3) haben daher unterschiedliche Keys/IVs.
        assert_ne!(local.key_base(), remote.key_base());
        assert_ne!(local.iv(), remote.iv());
    }

    #[test]
    fn dualsense_intensity_values_must_not_change() {
        assert_eq!(DualSenseEffectIntensity::Off as u8, 0);
        assert_eq!(DualSenseEffectIntensity::Weak as u8, 3);
        assert_eq!(DualSenseEffectIntensity::Medium as u8, 2);
        assert_eq!(DualSenseEffectIntensity::Strong as u8, 1);
        assert_eq!(DualSenseEffectIntensity::from_u8(3), Some(DualSenseEffectIntensity::Weak));
        assert_eq!(DualSenseEffectIntensity::from_u8(7), None);
        assert_eq!(DualSenseEffectIntensity::Strong.as_str(), "Strong");
    }
}
