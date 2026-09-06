// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
//
// Port of chiaki-ng lib/src/ctrl.c + lib/include/chiaki/ctrl.h.
//
// # Drahtformat (KEIN protobuf!)
// Der Ctrl-Kanal nutzt ein einfaches binäres Framing über TCP (Port 9295),
// siehe ctrl_thread_func()/ctrl_message_send() in ctrl.c:
//
// ```text
// uint32 BE  payload_size
// uint16 BE  message type   (CtrlMessageType)
// uint16 BE  0 (reserved)
// [payload — rpcrypt-verschlüsselt, CFB128, IV je crypt_counter]
// ```
//
// Die Verschlüsselung läuft über den session-Rpcrypt (rpcrypt.rs) mit
// unabhängigen Zählern je Richtung: `crypt_counter_local` (post-inkrement je
// verschlüsseltem Payload) und `crypt_counter_remote` (post-inkrement je
// entschlüsseltem Empfangs-Payload, nur wenn payload_size > 0). Der RP-Auth /
// RP-Did / RP-OSType / RP-StartBitrate / RP-StreamingType-Handshake (HTTP GET
// auf /sce/rp/session/ctrl bzw. /sie/ps{4,5}/rp/sess/ctrl) verbraucht die
// ersten lokalen Zähler (0,1,2[,3][,4]).
//
// # Entkopplung von session.rs (Agent J)
// Um Zirkularität zu vermeiden, definiert ctrl.rs seinen eigenen Event-Typ
// `CtrlEvent` und einen generischen Callback `Arc<dyn Fn(CtrlEvent)+Send+Sync>`
// in `CtrlInit`. `Session` baut den `Ctrl` mit `Ctrl::new(CtrlInit)` — das ist
// die vereinbarte Abweichung vom Contract (`Ctrl::new(session: &mut Session)`).
// Agent J muss im Event-Callback Folgendes an session.c anbinden:
//
// | CtrlEvent                         | session.c / ctrl.c-Entsprechung                    |
// |-----------------------------------|----------------------------------------------------|
// | SessionId(id)                     | session_id kopieren, ctrl_session_id_received=true, state_cond signalisieren (ctrl.c:876-939) |
// | LoginPinRequested(pin_incorrect)  | ctrl_login_pin_requested=true → Session-Thread emittet CHIAKI_EVENT_LOGIN_PIN_REQUEST (session.c:537-567); pin_incorrect wie dort |
// | LoginSuccess                      | ctrl->login_pin_requested=false (ctrl.c:1026-1029) — PIN-Warteschleife darf weiterlaufen |
// | CantDisplay { cant }              | display_sink.cantdisplay_cb(user, cant) (ctrl.c:995/1005/1467) |
// | KeyboardOpen(text)                | CHIAKI_EVENT_KEYBOARD_OPEN (ctrl.c:1050-1076)       |
// | KeyboardTextChange(text)          | CHIAKI_EVENT_KEYBOARD_TEXT_CHANGE (ctrl.c:1089-1115)|
// | KeyboardRemoteClose               | CHIAKI_EVENT_KEYBOARD_REMOTE_CLOSE (ctrl.c:1078)    |
// | ServerType { server_type }        | Downgrade-Logik aus ctrl.c:1424-1464 im Callback: server_type==0 && auto_downgrade && height==1080 → 720p-Preset; (server_type==0||1) && codec!=H264 → H264 (das C mutiert session->connect_info.video_profile direkt) |
// | SwitchToStreamConnection          | chiaki_session_set_stream_connection_switch_received() (Dedupe wie ctrl.c:950-960) |
// | Quit(reason)                      | ctrl_failed(): quit_reason=..., ctrl_failed=true, state_cond (ctrl.c:309-317). Kann je Fehler mehrfach kommen (innerer Grund + ConnectFailed) — C überschreibt quit_reason, also: letzter Quit gewinnt. Mapping: Unknown→QuitReason::CtrlUnknown, ConnectFailed→CtrlConnectFailed, ConnectionRefused→CtrlConnectionRefused |
//
// Beim Session-Timeout ohne Session-Id ruft session.c zusätzlich
// ctrl_message_set_fallback_session_id() + ctrl_enable_features() — in Rust:
// `Ctrl::generate_fallback_session_id()` (String selbst speichern) und
// `Ctrl::enable_features()` (enqueue, Reihenfolge wie ctrl.c:851-874).
//
// # Transportwahl (RUDP / Holepunch-Pfad)
// ctrl.c verzweigt an allen Netzstellen auf `session->rudp` (gesetzt, wenn
// `holepunch_session` existiert — PSN Remote Play over Internet): dann läuft
// der Ctrl-Kanal als SCE-RUDP-CTRL-Messages über den gepunchten Control-Hole
// statt über TCP. In Rust wählt `CtrlTransport` den Pfad; die RUDP-
// Protokollfunktionen kommen über das `session::HolepunchSession`-Trait
// (Implementierung: chiaki-remote, Dependency-Richtung remote → core):
//
// | ctrl.c-Stelle                              | RUDP-Verhalten                                                            |
// |--------------------------------------------|---------------------------------------------------------------------------|
// | ctrl_connect (1165-1187)                   | INIT/COOKIE-Handshake ("CTRL - Starting RUDP session"), remote_counter    |
// | ctrl.c:1299                                | HTTP-Port = chiaki_get_ps_ctrl_port (statt SESSION_CTRL_PORT)             |
// | ctrl.c:1316-1320                           | PS5: crypt_counter_local++ vor dem Request                                |
// | ctrl.c:1329-1331                           | HTTP-Request/Antwort via chiaki_send_recv_http_header_psn; Timeout-Retry ohne Reconnect |
// | ctrl.c:1389-1398                           | ACK-Message auf die HTTP-Antwort (remote_counter)                         |
// | ctrl_thread_func select (466-467)          | Empfang wartet am RUDP-Socket (hier: Empfangs-Timeout als Poll, siehe unten) |
// | ctrl_thread_func recv (514-600)            | chiaki_rudp_recv_only + Subtype-Dispatch (0x12/0x26/0x36/0x02/0x24/0xC0/default), Ctrl-Frames in recv_buf |
// | ctrl_message_send (651-655, 674-689)       | Frame via chiaki_rudp_send_ctrl_message ( LOGIN_PIN_REP-Sonderfall: zähleridentisch, ein Pfad) |
//
// # Bekannte Abweichungen vom C (alle dokumentiert)
// - RUDP-Empfang: das C selectiert blockierend (UINT64_MAX) und recv't dann;
//   hier pollt der Loop mit Empfangs-Timeout (TCP: RECV_POLL, RUDP:
//   RUDP-recv-Timeout der Trait-Impl), damit stop()/Queue/PIN weiter bedient
//   werden — `Err(Timeout)` wird wie "nichts empfangen" behandelt (das C
//   bricht bei recv-Fehlern ab, kann dort aber nie Timeout sehen).
// - ctrl_message_send RUDP-Zweig: das C trunciert `uint8_t buf_size =
//   8 + payload_size` (Überlauf/UB für Payloads > 247 Bytes); hier läuft der
//   volle Frame raus (Ctrl-Payloads sind klein — recv_buf-Grenze 512).
// - ctrl.c:651-655 (LOGIN_PIN_REP im RUDP-Pfad): `local_counter =
//   crypt_counter_local++; encrypt(local_counter - 1, ...)` ist mathematisch
//   identisch zum sonstigen Pfad (`encrypt(crypt_counter_local++)`) — ein
//   gemeinsamer Codepfad.
// - recv_buf-Memcpy-Guard (ctrl.c:554/578): das C kopiert die Ctrl-Frames
//   ohne Größenprüfung in recv_buf[512] (mögliches Overflow-UB); hier wird
//   wie beim Framing-Overflow "Ctrl buffer overflow!" + ctrl_failed(Unknown)
//   gemeldet.
// - SUB-Message-/ACK-Lesezugriffe (data[2..4]) sind im C ungeprüft; hier
//   bounds-geprüft (Überspringen statt OOB).
// - ctrl_enable_features/ctrl_message_toggle_microphone senden im C direkt
//   auf ctrl->sock (aus einem fremden Thread — Race im Original); in Rust
//   wird alles über die Message-Queue an den Ctrl-Thread gegeben
//   (wire-identisch, Latenz <= RECV_POLL).
// - notif_pipe: entfällt; stattdessen pollt der Ctrl-Loop Socket + Queue mit
//   `RECV_POLL` (Reaktionszeit auf stop()/Nachrichten <= RECV_POLL bzw.
//   RUDP-Empfangs-Timeout), wie in stoppipe.rs für std ohne select() dokumentiert.
// - Memory-Safety-Guards: DISPLAYA/DISPLAYB lesen im C payload[0]/[1] ohne
   // Größenprüfung (mögliches OOB); hier >=1 bzw. >=2 Bytes erzwungen.
// - Keyboard open/text change: C assert(payload_size == header+text_length);
//   hier nicht-fatal: text_length wird auf den vorhandenen Payload begrenzt.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::base64;
use super::error::{ChiakiError, ChiakiResult, Codec, Target};
use super::http::{recv_http_header, response_parse, HttpResponse};
use super::random::random_bytes_crypt;
use super::rpcrypt::{Rpcrypt, RPCRYPT_KEY_SIZE};
use super::session::HolepunchSession;
use super::sock::map_io_error;
use super::stoppipe::StopPipe;
use super::time::now_ms;

/// `SESSION_CTRL_PORT` (ctrl.c)
pub const SESSION_CTRL_PORT: u16 = 9295;
/// `CHIAKI_RP_DID_SIZE` (session.h)
pub const RP_DID_SIZE: usize = 32;
/// `CHIAKI_SESSION_ID_SIZE_MAX` (session.h)
pub const SESSION_ID_SIZE_MAX: usize = 80;
/// `CTRL_EXPECT_TIMEOUT` (ms, ctrl.c)
pub const CTRL_EXPECT_TIMEOUT_MS: u64 = 5000;

/// C: `uint8_t recv_buf[512]` — harte Protokollgrenze für Frames.
const CTRL_RECV_BUF_SIZE: usize = 512;
/// C: `uint8_t rudp_recv_buf[520]` (ctrl.h) — Empfangspuffergröße für
/// `chiaki_rudp_recv_only` (genutzt wird `520 - recv_buf_size`).
const CTRL_RUDP_RECV_BUF_SIZE: usize = 520;
/// Poll-Intervall des Ctrl-Loops (std kennt kein select(); siehe Moduldoku).
const RECV_POLL: Duration = Duration::from_millis(50);

/// C: `SESSION_OSTYPE` (ctrl.c) — wird inkl. NUL verschlüsselt gesendet.
const SESSION_OSTYPE: &str = "Win10.0.0";

/// Transport des Ctrl-Kanals (C: die `session->rudp`-Verzweigungen).
#[derive(Clone)]
pub enum CtrlTransport {
    /// TCP-Pfad: Ctrl verbindet selbst zu `CtrlInit::host_addr` (Port 9295)
    /// bzw. übernimmt `CtrlInit::sock` (ctrl_connect_tcp, ctrl.c:328-405).
    Tcp,
    /// PSN-Holepunch-Pfad: RUDP über die `HolepunchSession` (ctrl.c-Zweige an
    /// 466, 514, 651, 674, 1165, 1316, 1389 — siehe Moduldoku-Tabelle).
    Holepunch(Arc<dyn HolepunchSession>),
}

impl CtrlTransport {
    /// C: `if(ctrl->session->rudp)`.
    fn is_rudp(&self) -> bool {
        matches!(self, CtrlTransport::Holepunch(_))
    }
}

/// `ctrl_message_type_t` (ctrl.c) — Werte sind protokollrelevant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum CtrlMessageType {
    SessionId = 0x33,
    HeartbeatReq = 0xfe,
    HeartbeatRep = 0x1fe,
    LoginPinReq = 0x4,
    LoginPinRep = 0x8004,
    Login = 0x5,
    GotoBed = 0x50,
    KeyboardEnable = 0xd,
    KeyboardEnableToggle = 0x20,
    KeyboardOpen = 0x21,
    KeyboardCloseRemote = 0x22,
    KeyboardTextChangeReq = 0x23,
    KeyboardTextChangeRes = 0x24,
    KeyboardCloseReq = 0x25,
    EnableDualsenseFeatures = 0x13,
    GoHome = 0x14,
    Displaya = 0x1,
    Displayb = 0x16,
    MicConnect = 0x30,
    MicToggle = 0x36,
    DisplayDevices = 0x910,
    SwitchToStreamConnection = 0x34,
}

impl From<CtrlMessageType> for u16 {
    fn from(t: CtrlMessageType) -> u16 {
        t as u16
    }
}

/// `ctrl_login_state_t` (ctrl.c)
const CTRL_LOGIN_STATE_SUCCESS: u8 = 0x0;
const CTRL_LOGIN_STATE_PIN_INCORRECT: u8 = 0x1;

/// Ctrl-Payload-Nachricht für die Queue Session-Thread → Ctrl-Thread
/// (C: ChiakiCtrlMessageQueue, verkettete Liste unter notif_mutex;
/// Rust: mpsc-Kanal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtrlMessage {
    pub msg_type: u16,
    pub payload: Vec<u8>,
}

/// Quit-Gründe, die ctrl.c an die Session meldet (ctrl_failed(), ctrl.c:309).
/// Mapping auf session::QuitReason siehe Moduldoku (Agent J).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtrlQuitReason {
    /// CHIAKI_QUIT_REASON_CTRL_UNKNOWN
    Unknown,
    /// CHIAKI_QUIT_REASON_CTRL_CONNECT_FAILED
    ConnectFailed,
    /// CHIAKI_QUIT_REASON_CTRL_CONNECTION_REFUSED
    ConnectionRefused,
}

/// Events, die der Ctrl-Thread an die Session meldet (siehe Moduldoku für
/// die 1:1-Zuordnung zu session.c/ctrl.c).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CtrlEvent {
    /// Gültige Session-Id empfangen oder Fallback erzeugt (ctrl.c:876-939).
    SessionId(String),
    /// Login-PIN angefordert; `pin_incorrect` = true beim Re-Request nach
    /// CTRL_LOGIN_STATE_PIN_INCORRECT (ctrl.c:962-983, 1030-1043).
    LoginPinRequested(bool),
    /// Login erfolgreich (ctrl.c:1024-1029): PIN-Wartung zurücksetzen.
    LoginSuccess,
    /// cantdisplay-Callback (ctrl.c:995/1005/1467). `meta` existiert im C
    /// nicht (nur bool) — immer 0; Feld nur für Contract-Kompatibilität
    /// (SessionEvent::CantDisplay).
    CantDisplay { meta: u8, cant: bool },
    /// CHIAKI_EVENT_KEYBOARD_OPEN mit Text (ctrl.c:1050-1076).
    KeyboardOpen(String),
    /// CHIAKI_EVENT_KEYBOARD_TEXT_CHANGE mit Text (ctrl.c:1089-1115).
    KeyboardTextChange(String),
    /// CHIAKI_EVENT_KEYBOARD_REMOTE_CLOSE (ctrl.c:1078-1087).
    KeyboardRemoteClose,
    /// Validierter, entschlüsselter RP-Server-Type aus dem HTTP-Handshake
    /// (0 = PS4, 1 = PS4 Pro, 2 = PS5) — Agent J macht das Video-Downgrade
    /// (ctrl.c:1438-1464), siehe Moduldoku.
    ServerType { server_type: u8 },
    /// SWITCH_TO_STREAM_CONNECTION-ACK empfangen (ctrl.c:950-960).
    SwitchToStreamConnection,
    /// ctrl_failed() (ctrl.c:309-317).
    Quit(CtrlQuitReason),
}

/// Initialisierung für [`Ctrl::new`] — wird von der Session (Agent J) nach dem
/// HTTP session_request befüllt (siehe Moduldoku).
pub struct CtrlInit {
    /// session->rpcrypt — nach `Rpcrypt::new_auth(target, nonce, morning)`.
    pub rpcrypt: Rpcrypt,
    /// session->target.
    pub target: Target,
    /// session->connect_info.regist_key (16 Bytes, \\0-gefüllt).
    pub regist_key: [u8; RPCRYPT_KEY_SIZE],
    /// session->connect_info.did (32 Bytes).
    pub did: [u8; RP_DID_SIZE],
    /// session->connect_info.hostname (für den HTTP Host-Header).
    pub hostname: String,
    /// Aufgelöste Host-Adresse mit Ctrl-Port (0.0.0.0/:: durch den Aufrufer auf
    /// `SESSION_CTRL_PORT` gesetzt). Wirklich relevant nur für (Re-)Connect im
    /// TCP-Pfad; im Holepunch-Pfad ungenutzt (C: Port aus der Holepunch-
    /// Session, ctrl.c:1299).
    pub host_addr: SocketAddr,
    /// Bereits verbundener Ctrl-Socket (falls die Session den Connect selbst
    /// macht); `None` = Ctrl verbindet selbst zu `host_addr` (wie ctrl.c).
    /// Nur im TCP-Pfad (`transport: CtrlTransport::Tcp`) relevant.
    pub sock: Option<TcpStream>,
    /// Transportwahl (C: die `session->rudp`-Verzweigungen): TCP oder RUDP
    /// über die Holepunch-Session.
    pub transport: CtrlTransport,
    /// connect_info.video_profile.codec (RP-StreamingType-Header, PS5).
    pub codec: Codec,
    /// connect_info.enable_dualsense.
    pub enable_dualsense: bool,
    /// connect_info.enable_keyboard.
    pub enable_keyboard: bool,
    /// msg_queue: Sender+Receiver des Kanals Session/UI → Ctrl-Thread.
    /// (Der Receiver wird bei `start()` in den Ctrl-Thread bewegt, der Sender
    /// bleibt bei der Session für `send_message`-artige Aufrufe.)
    pub msg_queue_tx: Sender<CtrlMessage>,
    pub msg_queue_rx: Receiver<CtrlMessage>,
    /// Event-Callback (läuft auf dem Ctrl-Thread!).
    pub event_cb: Arc<dyn Fn(CtrlEvent) + Send + Sync>,
}

/// Von allen Threads geteilter Ctrl-Zustand (C: ctrl->notif_mutex-Schutz).
struct CtrlShared {
    rpcrypt: Rpcrypt,
    target: Target,
    regist_key: [u8; RPCRYPT_KEY_SIZE],
    did: [u8; RP_DID_SIZE],
    hostname: String,
    host_addr: SocketAddr,
    codec: Codec,
    enable_dualsense: bool,
    enable_keyboard: bool,
    event_cb: Arc<dyn Fn(CtrlEvent) + Send + Sync>,
    /// ctrl->stop_pipe: Stop-Anforderung an den Ctrl-Thread.
    stop_pipe: StopPipe,
    /// ctrl->login_pin: Session-Thread → Ctrl-Thread (C: login_pin_entered +
    /// login_pin unter notif_mutex, geweckt via notif_pipe; hier Mutex +
    /// Poll-Intervall).
    login_pin: Mutex<Option<Vec<u8>>>,
    /// C: session->ctrl_session_id_received — auch ctrl-intern relevant
    /// (Login-PIN-Request nach Session-Id → ctrl_failed, ctrl.c:971-979).
    session_id_received: AtomicBool,
    /// ctrl->keyboard_text_counter (Session-Thread, keyboard_set_text).
    keyboard_text_counter: AtomicU32,
}

/// Port von `ChiakiCtrl`.
///
/// Thread-Modell wie im C: ein Ctrl-Thread besitzt den Socket (bzw. die
/// Holepunch-Session) und verarbeitet Framing/Queue; die öffentliche API
/// (`&self`) stellt Nachrichten ein bzw. setzt Flags.
pub struct Ctrl {
    shared: Arc<CtrlShared>,
    msg_queue_tx: Sender<CtrlMessage>,
    msg_queue_rx: Option<Receiver<CtrlMessage>>,
    /// Vor-verbundener Socket aus CtrlInit (wird bei `start()` an den Thread
    /// übergeben).
    init_sock: Option<TcpStream>,
    /// Transportwahl aus CtrlInit (wird bei `start()` an den Thread
    /// übergeben).
    transport: Option<CtrlTransport>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Ctrl {
    /// Port von `chiaki_ctrl_init()` — ACHTUNG Contract-Abweichung: nimmt
    /// `CtrlInit` statt `&mut Session` (siehe Moduldoku).
    pub fn new(init: CtrlInit) -> ChiakiResult<Self> {
        let shared = Arc::new(CtrlShared {
            rpcrypt: init.rpcrypt,
            target: init.target,
            regist_key: init.regist_key,
            did: init.did,
            hostname: init.hostname,
            host_addr: init.host_addr,
            codec: init.codec,
            enable_dualsense: init.enable_dualsense,
            enable_keyboard: init.enable_keyboard,
            event_cb: init.event_cb,
            stop_pipe: StopPipe::new(),
            login_pin: Mutex::new(None),
            session_id_received: AtomicBool::new(false),
            keyboard_text_counter: AtomicU32::new(0),
        });
        Ok(Ctrl {
            shared,
            msg_queue_tx: init.msg_queue_tx,
            msg_queue_rx: Some(init.msg_queue_rx),
            init_sock: init.sock,
            transport: Some(init.transport),
            thread: Mutex::new(None),
        })
    }

    /// Port von `chiaki_ctrl_start()`: startet den Ctrl-Thread.
    pub fn start(&mut self) -> ChiakiResult<()> {
        let mut thread = self.thread.lock().unwrap_or_else(|e| e.into_inner());
        if thread.is_some() {
            return Err(ChiakiError::Thread);
        }
        let rx = self
            .msg_queue_rx
            .take()
            .ok_or(ChiakiError::Uninitialized)?;
        let shared = Arc::clone(&self.shared);
        let init_sock = self.init_sock.take();
        let transport = self.transport.take().ok_or(ChiakiError::Uninitialized)?;
        *thread = Some(std::thread::Builder::new()
            .name("Chiaki Ctrl".to_string())
            .spawn(move || ctrl_thread_func(shared, transport, init_sock, rx))
            .map_err(|_| ChiakiError::Thread)?);
        Ok(())
    }

    /// Port von `chiaki_ctrl_stop()`.
    pub fn stop(&self) {
        self.shared.stop_pipe.stop();
    }

    /// Port von `chiaki_ctrl_join()`.
    pub fn join(&mut self) -> ChiakiResult<()> {
        let handle = self
            .thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        match handle {
            Some(handle) => handle.join().map_err(|_| ChiakiError::Thread),
            None => Ok(()),
        }
    }

    /// Port von `chiaki_ctrl_fini()`: Speicherfreigabe macht RAII; hier wird
    /// nur sicher gestoppt+gejoint (idempotent, falls schon geschehen).
    pub fn fini(&mut self) {
        self.stop();
        let _ = self.join();
    }

    /// Port von `chiaki_ctrl_send_message()`: Nachricht in die Queue stellen;
    /// der Ctrl-Thread verschlüsselt und sendet (Reihenfolge wie im C).
    pub fn send_message(&self, type_: u16, payload: &[u8]) -> ChiakiResult<()> {
        self.msg_queue_tx
            .send(CtrlMessage {
                msg_type: type_,
                payload: payload.to_vec(),
            })
            .map_err(|_| ChiakiError::Unknown)
    }

    /// Port von `chiaki_ctrl_set_login_pin()`.
    pub fn set_login_pin(&self, pin: &[u8]) {
        *self.shared.login_pin.lock().unwrap_or_else(|e| e.into_inner()) = Some(pin.to_vec());
    }

    /// Port von `chiaki_ctrl_goto_bed()`.
    pub fn goto_bed(&self) -> ChiakiResult<()> {
        self.send_message(CtrlMessageType::GotoBed as u16, &[])
    }

    /// Port von `ctrl_message_toggle_microphone()` (als Queue-Nachricht,
    /// siehe Moduldoku — das C sendet hier direkt aus dem Fremd-Thread).
    pub fn toggle_microphone(&self, muted: bool) -> ChiakiResult<()> {
        self.send_message(CtrlMessageType::MicToggle as u16, &mic_toggle_payload(muted))
    }

    /// Port von `ctrl_message_connect_microphone()`.
    pub fn connect_microphone(&self) -> ChiakiResult<()> {
        const CONNECT: [u8; 2] = [0x00, 0x00];
        self.send_message(CtrlMessageType::MicConnect as u16, &CONNECT)
    }

    /// Port von `chiaki_ctrl_keyboard_set_text()`: 36-Byte-Header
    /// (`CtrlKeyboardTextRequestMessage`: counter, text_length1, unk1[8],
    /// unk2[16], text_length2 — alles Big-Endian) + Text-Bytes.
    pub fn keyboard_set_text(&self, text: &str) -> ChiakiResult<()> {
        let counter = self.shared.keyboard_text_counter.fetch_add(1, Ordering::SeqCst) + 1;
        let payload = build_keyboard_text_request(counter, text);
        self.send_message(CtrlMessageType::KeyboardTextChangeReq as u16, &payload)
    }

    /// Port von `chiaki_ctrl_keyboard_accept()`.
    pub fn keyboard_accept(&self) -> ChiakiResult<()> {
        const ACCEPT: [u8; 4] = [0x00, 0x00, 0x00, 0x00];
        self.send_message(CtrlMessageType::KeyboardCloseReq as u16, &ACCEPT)
    }

    /// Port von `chiaki_ctrl_keyboard_reject()`.
    pub fn keyboard_reject(&self) -> ChiakiResult<()> {
        const REJECT: [u8; 4] = [0x00, 0x00, 0x00, 0x01];
        self.send_message(CtrlMessageType::KeyboardCloseReq as u16, &REJECT)
    }

    /// Port von `ctrl_message_go_home()`.
    pub fn go_home(&self) -> ChiakiResult<()> {
        const HOME: [u8; 0x10] = [
            0x00, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00,
        ];
        self.send_message(CtrlMessageType::GoHome as u16, &HOME)
    }

    /// Contract-Platzhalter: existiert im C so nicht — cant_display kommt
    /// ausschließlich vom Server (DISPLAYA/DISPLAYB/RP-Prohibit). Ohne Funktion.
    pub fn cant_display_set(&self, _meta: u8, _val: bool) {}

    /// Port von `ctrl_enable_features()`: Feature-Activierung als Queue-
    /// Nachrichten in exakter C-Reihenfolge (ctrl.c:851-874). Wird nach dem
    /// Empfang der Session-Id automatisch ausgeführt; die Session ruft sie
    /// zusätzlich im Fallback-Pfad (session.c:599-608).
    pub fn enable_features(&self) -> ChiakiResult<()> {
        enqueue_enable_features(
            &self.msg_queue_tx,
            self.shared.enable_dualsense,
            self.shared.enable_keyboard,
        )
    }

    /// Port von `ctrl_message_set_fallback_session_id()` (Erzeugung): dezimaler
    /// Monotonic-Sekundenstempel + base64 von 48 Zufallsbytes, total <= 79
    /// Zeichen (C: char[80]). Die Session speichert den String selbst und
    /// setzt ihr ctrl_session_id_received (siehe session.c:599-608).
    pub fn generate_fallback_session_id() -> ChiakiResult<String> {
        generate_fallback_session_id()
    }

    /// Port von `ctrl_message_set_fallback_session_id()` (Seiten-Effekte, für
    /// den Fallback-Pfad des Session-Threads, session.c:599-608): erzeugt die
    /// Fallback-Id, setzt das interne Session-Id-Flag und emittet
    /// `CtrlEvent::SessionId(id)` — entsprechend dem C, das session->session_id
    /// befüllt, ctrl_session_id_received setzt und state_cond signalisiert.
    /// Liefert die erzeugte Id zurück.
    pub fn set_fallback_session_id(&self) -> ChiakiResult<String> {
        if self.shared.session_id_received.load(Ordering::SeqCst) {
            tracing::warn!("Aleady received session Id don't need fallback.");
            return Err(ChiakiError::Unknown);
        }
        let id = generate_fallback_session_id()?;
        tracing::info!("Ctrl set fallback session Id {}", id);
        self.shared.session_id_received.store(true, Ordering::SeqCst);
        (self.shared.event_cb)(CtrlEvent::SessionId(id.clone()));
        Ok(id)
    }

    /// Interner Zustand: wurde bereits eine Session-Id empfangen/gemerkt?
    /// (Entspricht session->ctrl_session_id_received; für Agent J, um den
    /// Fallback-Pfad konsistent zu halten — das C setzt das Flag in
    /// ctrl_message_set_fallback_session_id ebenfalls.)
    pub fn session_id_received(&self) -> bool {
        self.shared.session_id_received.load(Ordering::SeqCst)
    }

    /// Session-Id-Flag von außen setzen (Fallback-Pfad der Session).
    pub fn set_session_id_received(&self) {
        self.shared.session_id_received.store(true, Ordering::SeqCst);
    }
}

impl Drop for Ctrl {
    fn drop(&mut self) {
        // Sicherer Stop (kein join im Drop — könnte blockieren); das C
        // verlangt explizite stop/join/fini-Aufrufe, das tut Agent J.
        self.stop();
    }
}

/// `ctrl_message_toggle_microphone`-Payload (ctrl.c:741-755).
fn mic_toggle_payload(muted: bool) -> [u8; 4] {
    // uint8_t toggle[0x4] = {0, 1, 1, 89}; if(muted) toggle[2] = 0;
    let mut toggle = [0u8, 1u8, 1u8, 89u8];
    if muted {
        toggle[2] = 0;
    }
    toggle
}

/// 8-Byte-Frame-Header: payload_size BE u32, type BE u16, 0 BE u16.
fn build_header(payload_size: usize, msg_type: u16) -> [u8; 8] {
    let mut header = [0u8; 8];
    header[..4].copy_from_slice(&(payload_size as u32).to_be_bytes());
    header[4..6].copy_from_slice(&msg_type.to_be_bytes());
    header[6..8].copy_from_slice(&0u16.to_be_bytes());
    header
}

/// Komplette verschlüsselte Nachricht (Header + Payload) bauen.
///
/// 1:1 zu ctrl_message_send(): Payload wird mit rpcrypt.encrypt(counter)
/// verschlüsselt, Header bleibt Klartext. (Nur für Tests/Treiber sichtbar —
/// der Thread nutzt sie über [`encrypt_message`].)
fn build_encrypted_message(
    rpcrypt: &Rpcrypt,
    counter: u64,
    msg_type: u16,
    payload: &[u8],
) -> ChiakiResult<Vec<u8>> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&build_header(payload.len(), msg_type));
    if !payload.is_empty() {
        out.extend_from_slice(&rpcrypt.encrypt_buf(counter, payload)?);
    }
    Ok(out)
}

/// Verschlüsseln mit Zähler-Advance (C: `ctrl->crypt_counter_local++` als
/// Funktionsargument — NUR wenn ein Payload vorhanden ist; bei leerem Payload
/// wird im C weder verschlüsselt noch der Zähler erhöht).
fn encrypt_message(
    counter: &mut u64,
    rpcrypt: &Rpcrypt,
    msg_type: u16,
    payload: &[u8],
) -> ChiakiResult<Vec<u8>> {
    if payload.is_empty() {
        // C: enc bleibt NULL -> nur der 8-Byte-Header geht raus.
        return Ok(build_header(0, msg_type).to_vec());
    }
    let frame = build_encrypted_message(rpcrypt, *counter, msg_type, payload);
    *counter += 1;
    frame
}

/// Payload für `chiaki_ctrl_keyboard_set_text()`:
/// `CtrlKeyboardTextRequestMessage { u32 counter; u32 text_length1;
///  uint8 unk1[8]; uint8 unk2[16]; u32 text_length2; }` + Text (alles BE,
///  Header memset-0 wie im C).
fn build_keyboard_text_request(counter: u32, text: &str) -> Vec<u8> {
    let length = text.len() as u32;
    let mut payload = vec![0u8; 36 + text.len()];
    payload[0..4].copy_from_slice(&counter.to_be_bytes());
    payload[4..8].copy_from_slice(&length.to_be_bytes());
    payload[32..36].copy_from_slice(&length.to_be_bytes());
    payload[36..].copy_from_slice(text.as_bytes());
    payload
}

/// `rudp_packet_type_data_offset()` (ctrl.c:103-114): Offset des MACs bzw.
/// des Ctrl-Frames innerhalb der Data einer RUDP-Message je Subtype
/// (default 2 = hinter dem lokalen Counter; das C liefert "unbekannt" als
/// -1, benutzt den Wert aber nie -1 — hier direkt 2).
fn rudp_packet_type_data_offset(subtype: u8) -> usize {
    match subtype {
        0x12 => 8,
        0x26 => 6,
        _ => 2,
    }
}

/// `ctrl_message_set_fallback_session_id()` — Erzeugungsteil:
/// snprintf(fallback, 16, "%lld", monotonic_ms / 1000) + base64(48 random
/// bytes) (64 Zeichen) → String mit 65..=79 Zeichen.
fn generate_fallback_session_id() -> ChiakiResult<String> {
    let time_seconds = (now_ms() / 1000) as i64;
    let mut rand_bytes = [0u8; 48];
    random_bytes_crypt(&mut rand_bytes)?;
    let mut id = time_seconds.to_string();
    id.push_str(&base64::encode(&rand_bytes));
    Ok(id)
}

/// Session-Id-Validierung aus ctrl_message_received_session_id()
/// (ctrl.c:876-939): Byte 0 überspringen ("size"), dann 24..79 Bytes aus
/// [A-Za-z0-9]. Gibt die validierte Id zurück.
fn validate_session_id_payload(payload: &[u8]) -> Option<String> {
    if payload.len() < 2 {
        return None;
    }
    let payload = &payload[1..]; // skip the size
    if payload.len() >= SESSION_ID_SIZE_MAX - 1 {
        return None; // too long
    }
    if payload.len() < 24 {
        return None; // too short
    }
    for &c in payload {
        let ok = c.is_ascii_lowercase() || c.is_ascii_uppercase() || c.is_ascii_digit();
        if !ok {
            return None; // invalid characters
        }
    }
    Some(String::from_utf8_lossy(payload).into_owned())
}

/// C atoi(): führender Dezimal-Integer, 0 bei Nicht-Parsbarkeit.
fn atoi_i32(s: &str) -> i32 {
    let trimmed = s.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let (sign, digits) = match trimmed.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    let mut val: i64 = 0;
    for c in digits.chars().take_while(|c| c.is_ascii_digit()) {
        val = val.saturating_mul(10).saturating_add((c as u8 - b'0') as i64);
    }
    (sign * val).clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

/// `ctrl_response_t` + `parse_ctrl_response()` (ctrl.c:1117-1154).
#[derive(Debug, Default)]
struct CtrlResponse {
    server_type_valid: bool,
    rp_server_type: [u8; 0x10],
    rp_prohibit: bool,
    success: bool,
}

fn parse_ctrl_response(response: &mut CtrlResponse, http_response: &HttpResponse) {
    if http_response.code != 200 {
        response.success = false;
        return;
    }
    response.success = true;
    response.server_type_valid = false;
    response.rp_prohibit = false;
    for header in &http_response.headers {
        if header.key == "RP-Server-Type" {
            // C: chiaki_base64_decode(value, strlen(value) + 1, ...) — inkl.
            // des NUL-Terminators. Bei gepaddetem base64 (16 Bytes -> "…==")
            // terminiert '=' das Parsen vor dem NUL, daher dekodiert das C
            // hier regulär; nur ungepadte Werte würden am NUL scheitern
            // (1:1-Verhalten durch denselben Decoder erhalten).
            let mut input = header.value.as_bytes().to_vec();
            input.push(0);
            let decoded = match base64::decode(&input) {
                Ok(d) => d,
                Err(_) => {
                    response.success = false;
                    return;
                }
            };
            // C: server_type_valid = (out_size == sizeof(rp_server_type)),
            // zu lange Werte -> BUF_TOO_SMALL -> success=false.
            if decoded.len() > response.rp_server_type.len() {
                response.success = false;
                return;
            }
            response.rp_server_type[..decoded.len()].copy_from_slice(&decoded);
            response.server_type_valid = decoded.len() == response.rp_server_type.len();
        } else if header.key == "RP-Prohibit" {
            response.rp_prohibit = atoi_i32(&header.value) == 1;
        }
    }
}

/// `rp_version_string` (session.c:58-73) — private Kopie, weil ctrl.rs nicht
/// von session.rs abhängen darf (Agent J besitzt die öffentliche Funktion mit
/// derselben Tabelle).
fn rp_version_string(target: Target) -> Option<&'static str> {
    match target {
        Target::Ps4_8 => Some("8.0"),
        Target::Ps4_9 => Some("9.0"),
        Target::Ps4_10 => Some("10.0"),
        Target::Ps5_1 => Some("1.0"),
        _ => None,
    }
}

/// `ctrl_enable_features()` als Queue-Nachrichten (ctrl.c:851-874).
fn enqueue_enable_features(
    tx: &Sender<CtrlMessage>,
    enable_dualsense: bool,
    enable_keyboard: bool,
) -> ChiakiResult<()> {
    let send = |msg_type: u16, payload: &[u8]| -> ChiakiResult<()> {
        tx.send(CtrlMessage {
            msg_type,
            payload: payload.to_vec(),
        })
        .map_err(|_| ChiakiError::Unknown)
    };
    if enable_dualsense {
        tracing::info!("Enabling DualSense features");
        const ENABLE: [u8; 3] = [0x00, 0x40, 0x00];
        send(CtrlMessageType::EnableDualsenseFeatures as u16, &ENABLE)?;
        // C: uint8_t connect[0x10] = {0xa0, ..., 0x00} — 15 Initialisierer,
        // letztes Byte implizit 0.
        const CONNECT: [u8; 0x10] = [
            0xa0, 0xab, 0x51, 0xbd, 0xd1, 0x7e, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00,
            0x00, 0x00,
        ];
        send(0x11, &CONNECT)?;
    }
    if enable_keyboard {
        tracing::info!("Enabling Keyboard");
        // TODO: Signature ?!
        const SIGNATURE: [u8; 0x10] = [
            0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x05, 0xAE, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00,
        ];
        send(CtrlMessageType::KeyboardEnable as u16, &SIGNATURE)?;
        send(CtrlMessageType::KeyboardEnableToggle as u16, &[1u8])?;
    }
    send(
        CtrlMessageType::MicToggle as u16,
        &mic_toggle_payload(false),
    )?;
    send(
        CtrlMessageType::MicToggle as u16,
        &mic_toggle_payload(false),
    )?;
    const DISPLAY: [u8; 0x4] = [0x00, 0x00, 0x00, 0x00];
    send(CtrlMessageType::DisplayDevices as u16, &DISPLAY)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Ctrl-Thread
// ---------------------------------------------------------------------------

/// Ergebnis eines Empfangsvorgangs im Ctrl-Loop.
enum Receive {
    /// recv_buf hat Zuwachs (TCP: n rohe Bytes; RUDP: Ctrl-Frames angehängt).
    Data,
    /// Nichts empfangen (Poll-/Empfangs-Timeout) — Schleife fortsetzen.
    Nothing,
    /// Sauberes EOF (nur TCP-Pfad) — Schleife beenden ohne ctrl_failed.
    Eof,
    /// recv_buf-Überlauf — ctrl_failed(Unknown) + Ende (siehe
    /// [`CtrlThread::rudp_try_append_ctrl_frame`]).
    Overflow,
}

/// Thread-lokaler Zustand (C: ctrl->sock, recv_buf/rudp_recv_buf, counters,
/// cant_display*).
struct CtrlThread {
    shared: Arc<CtrlShared>,
    transport: CtrlTransport,
    init_sock: Option<TcpStream>,
    sock: Option<TcpStream>,
    recv_buf: [u8; CTRL_RECV_BUF_SIZE],
    recv_buf_size: usize,
    crypt_counter_local: u64,
    crypt_counter_remote: u64,
    login_pin_requested: bool,
    cant_displaya: bool,
    cant_displayb: bool,
}

fn ctrl_thread_func(
    shared: Arc<CtrlShared>,
    transport: CtrlTransport,
    init_sock: Option<TcpStream>,
    msg_queue_rx: Receiver<CtrlMessage>,
) {
    let mut thread = CtrlThread {
        shared,
        transport,
        init_sock,
        sock: None,
        recv_buf: [0u8; CTRL_RECV_BUF_SIZE],
        recv_buf_size: 0,
        crypt_counter_local: 0,
        crypt_counter_remote: 0,
        login_pin_requested: false,
        cant_displaya: false,
        cant_displayb: false,
    };

    match thread.connect_and_handshake() {
        Ok(()) => {}
        Err(_err) => {
            // ctrl_thread_func (ctrl.c:415-421):
            // ctrl_connect Fehler -> quit_reason CTRL_CONNECT_FAILED.
            thread.ctrl_failed(CtrlQuitReason::ConnectFailed);
            return;
        }
    }

    tracing::info!("Ctrl connected");
    thread.message_loop(&msg_queue_rx);
    // C (ctrl.c:622-629): nur der TCP-Pfad schließt den Socket; die
    // Rudp-Instanz gehört der Holepunch-Session und bleibt offen.
    thread.sock = None;
}

impl CtrlThread {
    fn emit(&self, event: CtrlEvent) {
        (self.shared.event_cb)(event);
    }

    /// `ctrl_failed()` (ctrl.c:309-317): Quit-Event an die Session.
    fn ctrl_failed(&self, reason: CtrlQuitReason) {
        self.emit(CtrlEvent::Quit(reason));
    }

    /// `ctrl_connect_tcp()` (ctrl.c:328-405) für den TCP-Pfad.
    fn ctrl_connect_tcp(&mut self) -> ChiakiResult<()> {
        let addr = self.shared.host_addr;
        if addr.port() == 0 {
            tracing::error!("Ctrl got invalid sockaddr");
            return Err(ChiakiError::InvalidData);
        }
        let sock = match stop_pipe_connect(&addr, &self.shared.stop_pipe, 5000) {
            Ok(sock) => sock,
            Err(e) => {
                if e == ChiakiError::ConnectionRefused {
                    tracing::error!("Ctrl connect failed: {e}");
                    self.ctrl_failed(CtrlQuitReason::ConnectionRefused);
                } else {
                    tracing::error!("Ctrl connect failed: {e}");
                    self.ctrl_failed(CtrlQuitReason::Unknown);
                }
                return Err(e);
            }
        };
        tracing::info!(
            "Ctrl connected to {}:{}",
            self.shared.hostname,
            SESSION_CTRL_PORT
        );
        self.sock = Some(sock);
        Ok(())
    }

    fn ctrl_disconnect_tcp(&mut self) {
        self.sock = None; // CHIAKI_SOCKET_CLOSE
    }

    /// `ctrl_connect()` (ctrl.c:1156-1486): Verbindung je Transport
    /// (TCP: ctrl_connect_tcp — RUDP: INIT/COOKIE-Handshake) +
    /// HTTP-Handshake (RP-Auth etc.) + RP-Server-Type-Auswertung.
    fn connect_and_handshake(&mut self) -> ChiakiResult<()> {
        self.crypt_counter_local = 0;
        self.crypt_counter_remote = 0;

        // C: `uint16_t remote_counter = 0;` — nur im RUDP-Pfad relevant.
        let mut remote_counter = 0u16;

        // Verbindung (ctrl.c:1165-1193).
        match &self.transport {
            CtrlTransport::Holepunch(holepunch) => {
                // C: "CTRL - Starting RUDP session" + INIT/COOKIE-Handshake;
                // remote_counter ist der der Cookie-Antwort.
                remote_counter = holepunch.rudp_ctrl_start_session()?;
            }
            CtrlTransport::Tcp => {
                // Vor-verbundener Socket aus CtrlInit übernehmen oder selbst
                // verbinden (wie ctrl.c).
                if let Some(sock) = self.init_sock.take() {
                    self.sock = Some(sock);
                } else {
                    self.ctrl_connect_tcp()?;
                }
            }
        }

        // uint8_t auth_enc[CHIAKI_RPCRYPT_KEY_SIZE] <- regist_key
        let auth_enc = self
            .shared
            .rpcrypt
            .encrypt_buf(self.crypt_counter_local, &self.shared.regist_key)?;
        self.crypt_counter_local += 1;
        let auth_b64 = base64::encode(&auth_enc);

        // uint8_t did_enc[CHIAKI_RP_DID_SIZE] <- did
        let did_enc = self
            .shared
            .rpcrypt
            .encrypt_buf(self.crypt_counter_local, &self.shared.did)?;
        self.crypt_counter_local += 1;
        let did_b64 = base64::encode(&did_enc);

        // SESSION_OSTYPE inkl. NUL-Terminator (strlen + 1)
        let mut ostype = Vec::with_capacity(SESSION_OSTYPE.len() + 1);
        ostype.extend_from_slice(SESSION_OSTYPE.as_bytes());
        ostype.push(0);
        let ostype_enc = self
            .shared
            .rpcrypt
            .encrypt_buf(self.crypt_counter_local, &ostype)?;
        self.crypt_counter_local += 1;
        let ostype_b64 = base64::encode(&ostype_enc);

        // RP-StartBitrate für target >= PS4_10 (4 NUL-Bytes)
        let have_bitrate = self.shared.target >= Target::Ps4_10;
        let mut bitrate_hdr = String::new();
        if have_bitrate {
            let bitrate_enc = self
                .shared
                .rpcrypt
                .encrypt_buf(self.crypt_counter_local, &[0u8; 4])?;
            self.crypt_counter_local += 1;
            bitrate_hdr = format!("RP-StartBitrate: {}\r\n", base64::encode(&bitrate_enc));
        }

        // RP-StreamingType für PS5 (1 = h264, 2 = h265, 3 = h265+hdr, LE u32)
        let have_streaming_type = self.shared.target.is_ps5();
        let mut streaming_type_hdr = String::new();
        if have_streaming_type {
            let streaming_type: u32 = match self.shared.codec {
                Codec::H265 => 2,
                Codec::H265Hdr => 3,
                _ => 1,
            };
            let streaming_type_buf = streaming_type.to_le_bytes();
            let streaming_type_enc = self
                .shared
                .rpcrypt
                .encrypt_buf(self.crypt_counter_local, &streaming_type_buf)?;
            self.crypt_counter_local += 1;
            streaming_type_hdr = format!(
                "RP-StreamingType: {}\r\n",
                base64::encode(&streaming_type_enc)
            );
        }

        let path = if self.shared.target == Target::Ps4_8 || self.shared.target == Target::Ps4_9 {
            "/sce/rp/session/ctrl"
        } else if self.shared.target.is_ps5() {
            "/sie/ps5/rp/sess/ctrl"
        } else {
            "/sie/ps4/rp/sess/ctrl"
        };
        let rp_version = rp_version_string(self.shared.target).unwrap_or("");
        // C (ctrl.c:1299): int port = session->holepunch_session
        //     ? chiaki_get_ps_ctrl_port(session->holepunch_session)
        //     : SESSION_CTRL_PORT;
        let port = match &self.transport {
            CtrlTransport::Holepunch(holepunch) => holepunch.ps_ctrl_port(),
            CtrlTransport::Tcp => SESSION_CTRL_PORT,
        };

        // request_fmt (ctrl.c:1274-1289), exakt gleiche Zeilen/Reihenfolge.
        let request = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {hostname}:{port}\r\n\
             User-Agent: remoteplay Windows\r\n\
             Connection: keep-alive\r\n\
             Content-Length: 0\r\n\
             RP-Auth: {auth_b64}\r\n\
             RP-Version: {rp_version}\r\n\
             RP-Did: {did_b64}\r\n\
             RP-ControllerType: 3\r\n\
             RP-ClientType: 11\r\n\
             RP-OSType: {ostype_b64}\r\n\
             RP-ConPath: 1\r\n\
             {bitrate_hdr}{streaming_type_hdr}\
             \r\n",
            path = path,
            hostname = self.shared.hostname,
            port = port,
            auth_b64 = auth_b64,
            rp_version = rp_version,
            did_b64 = did_b64,
            ostype_b64 = ostype_b64,
            bitrate_hdr = bitrate_hdr,
            streaming_type_hdr = streaming_type_hdr,
        );

        tracing::info!("Sending ctrl request");
        tracing::trace!("Ctrl request:\n{request}");

        // C (ctrl.c:1316-1320): im RUDP-Pfad verbraucht PS5 einen zusätzlichen
        // crypt_counter_local vor dem HTTP-Request.
        if self.transport.is_rudp() && self.shared.target.is_ps5() {
            self.crypt_counter_local += 1;
        }

        let mut ctrl_request_retry = false;
        let mut buf = [0u8; 512];
        let (header_size, received_size) = loop {
            // C (ctrl.c:1327-1343): RUDP — Request/Antwort über
            // chiaki_send_recv_http_header_psn; TCP — send + recv_http_header.
            let result = match &self.transport {
                CtrlTransport::Holepunch(holepunch) => holepunch
                    .rudp_send_recv_http_header(request.as_bytes(), remote_counter, &mut buf)
                    .map(|(header_size, received_size, new_remote_counter)| {
                        // C: chiaki_send_recv_http_header_psn aktualisiert
                        // *remote_counter (für das ACK nach der Antwort).
                        remote_counter = new_remote_counter;
                        (header_size, received_size)
                    }),
                CtrlTransport::Tcp => match self.sock.as_mut() {
                    Some(sock) => send_fully(
                        &self.shared.stop_pipe,
                        sock,
                        request.as_bytes(),
                        CTRL_EXPECT_TIMEOUT_MS,
                    )
                    .and_then(|()| {
                        recv_http_header(
                            sock,
                            &mut buf,
                            Some(&self.shared.stop_pipe),
                            CTRL_EXPECT_TIMEOUT_MS,
                        )
                    }),
                    None => Err(ChiakiError::Disconnected),
                },
            };

            match result {
                Err(ChiakiError::Timeout) if !ctrl_request_retry => {
                    tracing::info!("Initial ctrl startup request timed out, resending ...");
                    ctrl_request_retry = true;
                    // C (ctrl.c:1350-1356): nur der TCP-Pfad verbindet neu;
                    // der RUDP-Pfad sendet einfach erneut.
                    if matches!(self.transport, CtrlTransport::Tcp) && !self.shared.stop_pipe.is_set()
                    {
                        self.ctrl_disconnect_tcp();
                        self.ctrl_connect_tcp()?;
                    }
                    continue;
                }
                other => break other?,
            }
        };

        // C (ctrl.c:1389-1398): RUDP — ACK auf die HTTP-Antwort senden. (Das C
        // setzt bei Fehler quit_reason SESSION_REQUEST_UNKNOWN, das vom
        // ctrl_thread_func jedoch sofort mit CTRL_CONNECT_FAILED überschrieben
        // wird — hier daher nur der Fehler selbst.)
        if let CtrlTransport::Holepunch(holepunch) = &self.transport {
            holepunch.rudp_send_ack_message(remote_counter).inspect_err(|_| {
                tracing::error!("CTRL - Failed to send rudp ctrl request response ack message");
            })?;
        }

        tracing::info!("Ctrl received http header as response");
        tracing::trace!(
            "Ctrl response header:\n{}",
            String::from_utf8_lossy(&buf[..header_size])
        );

        let http_response =
            response_parse(&buf[..header_size])
                .inspect_err(|_| tracing::error!("Failed to parse ctrl request response"))?;

        tracing::info!("Ctrl received ctrl request http response");

        let mut response = CtrlResponse::default();
        parse_ctrl_response(&mut response, &http_response);
        if !response.success {
            tracing::error!(
                "Ctrl http response was not successful. HTTP code was {}",
                http_response.code
            );
            return Err(ChiakiError::Unknown);
        }

        if response.server_type_valid {
            match self
                .shared
                .rpcrypt
                .decrypt_buf(self.crypt_counter_remote, &response.rp_server_type)
            {
                Ok(decrypted) => {
                    self.crypt_counter_remote += 1;
                    response.rp_server_type = decrypted.try_into().unwrap_or([0u8; 0x10]);
                }
                Err(err) => {
                    tracing::error!("Ctrl failed to decrypt RP-Server-Type: {err}");
                    response.server_type_valid = false;
                }
            }
        }

        if response.server_type_valid {
            let server_type = response.rp_server_type[0]; // 0 = PS4, 1 = PS4 Pro, 2 = PS5
            tracing::info!("Ctrl got Server Type: {}", server_type);
            // Video-Downgrade (1080p->720p, H264-Zwang) macht die Session im
            // ServerType-Callback (C: Mutation von connect_info.video_profile,
            // ctrl.c:1438-1464) — siehe Moduldoku.
            self.emit(CtrlEvent::ServerType { server_type });
        } else {
            tracing::error!("No valid Server Type in ctrl response");
        }

        if response.rp_prohibit {
            self.emit(CtrlEvent::CantDisplay { meta: 0, cant: true });
        }

        // if we already got more data than the header, put the rest in the buffer.
        self.recv_buf_size = received_size - header_size;
        if self.recv_buf_size > 0 {
            self.recv_buf[..self.recv_buf_size]
                .copy_from_slice(&buf[header_size..received_size]);
        }

        Ok(())
    }

    /// `ctrl_thread_func`-Hauptschleife (ctrl.c:425-619): Framing + Queue,
    /// Empfang je Transport (RUDP-Zweig 514-600 / TCP-Zweig 601-615).
    fn message_loop(&mut self, msg_queue_rx: &Receiver<CtrlMessage>) {
        loop {
            // ---- Framing: alle vollständigen Frames aus recv_buf parsen ----
            let mut overflow = false;
            while self.recv_buf_size >= 8 {
                let payload_size =
                    u32::from_be_bytes([self.recv_buf[0], self.recv_buf[1], self.recv_buf[2], self.recv_buf[3]])
                        as usize;
                if self.recv_buf_size < 8 + payload_size {
                    if 8 + payload_size > CTRL_RECV_BUF_SIZE {
                        tracing::error!("Ctrl buffer overflow!");
                        overflow = true;
                    }
                    break;
                }

                let msg_type = u16::from_be_bytes([self.recv_buf[4], self.recv_buf[5]]);

                // Payload kopieren (Entschlüsselung in-place, danach Frame
                // aus recv_buf schieben — C: memmove).
                let mut payload =
                    self.recv_buf[8..8 + payload_size].to_vec();
                self.message_received(msg_type, &mut payload);

                self.recv_buf_size -= 8 + payload_size;
                if self.recv_buf_size > 0 {
                    self.recv_buf
                        .copy_within(8 + payload_size..8 + payload_size + self.recv_buf_size, 0);
                }
            }

            if overflow {
                self.ctrl_failed(CtrlQuitReason::Unknown);
                break;
            }

            // ---- Queue: eingereichte Nachrichten versenden (C: Drain auf
            // dem CANCELED-Pfad VOR dem should_stop-Check, ctrl.c:473-498) ----
            while let Ok(msg) = msg_queue_rx.try_recv() {
                let _ = self.message_send(msg.msg_type, &msg.payload);
            }

            // ---- Login-PIN (C: login_pin_entered unter notif_mutex) ----
            let pin = self
                .shared
                .login_pin
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some(pin) = pin {
                tracing::info!("Ctrl received entered Login PIN, sending to console");
                let _ = self.message_send(CtrlMessageType::LoginPinRep as u16, &pin);
                continue;
            }

            // ---- Stop? ----
            if self.should_stop() {
                tracing::info!("Ctrl requested to stop");
                break;
            }

            // ---- Empfangen (RUDP-Zweig ctrl.c:514-600, TCP-Zweig
            //      ctrl.c:601-615; Poll-Adaption siehe Moduldoku) ----
            let received = if self.transport.is_rudp() {
                self.receive_rudp()
            } else {
                self.receive_tcp()
            };
            match received {
                Ok(Receive::Data) => {}
                Ok(Receive::Nothing) => continue,
                Ok(Receive::Eof) => {
                    // C: received == 0 -> sauberes EOF, kein ctrl_failed.
                    break;
                }
                Ok(Receive::Overflow) => {
                    self.ctrl_failed(CtrlQuitReason::Unknown);
                    break;
                }
                Err(_) => {
                    // ctrl_failed ist bereits im Empfangszweig gelaufen.
                    break;
                }
            }
        }
    }

    /// C (ctrl.c:601-615): roher TCP-Empfang in recv_buf.
    fn receive_tcp(&mut self) -> ChiakiResult<Receive> {
        let received = match self.sock.as_mut() {
            Some(sock) => match sock.set_read_timeout(Some(RECV_POLL)) {
                Ok(()) => match sock.read(&mut self.recv_buf[self.recv_buf_size..]) {
                    Ok(n) => Ok(n),
                    Err(e)
                        if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut =>
                    {
                        Err(ChiakiError::Timeout)
                    }
                    Err(e) => Err(map_io_error(&e)),
                },
                Err(_) => Err(ChiakiError::Network),
            },
            None => Err(ChiakiError::Disconnected),
        };
        match received {
            Ok(0) => Ok(Receive::Eof),
            Ok(n) => {
                tracing::info!("CTRL RECEIVED");
                tracing::trace!(
                    "Ctrl recv: {:02x?}",
                    &self.recv_buf[self.recv_buf_size..self.recv_buf_size + n]
                );
                self.recv_buf_size += n;
                Ok(Receive::Data)
            }
            Err(ChiakiError::Timeout) => Ok(Receive::Nothing),
            Err(err) => {
                tracing::error!("Ctrl failed to recv: {err}");
                self.ctrl_failed(CtrlQuitReason::Unknown);
                Err(err)
            }
        }
    }

    /// C (ctrl.c:514-600): RUDP-Message empfangen
    /// (`chiaki_rudp_recv_only` mit `sizeof(rudp_recv_buf) - recv_buf_size`),
    /// je Subtype ACKs/Send-Buffer-ACKs ausführen und die transportierten
    /// Ctrl-Frames in recv_buf übernehmen — inkl. der Sub-Message-Kette
    /// (ctrl.c:583-599). Die eigentliche Framing-Auswertung passiert wie im C
    /// am Schleifenanfang von [`Self::message_loop`].
    fn receive_rudp(&mut self) -> ChiakiResult<Receive> {
        let holepunch = match &self.transport {
            CtrlTransport::Holepunch(holepunch) => Arc::clone(holepunch),
            CtrlTransport::Tcp => return self.receive_tcp(),
        };

        let mut message =
            match holepunch.rudp_recv_only(CTRL_RUDP_RECV_BUF_SIZE - self.recv_buf_size) {
                Ok(message) => message,
                // Poll-Adaption (Moduldoku): Timeout = nichts empfangen
                // (das C blockiert hinter dem select und kann hier nie
                // Timeout sehen).
                Err(ChiakiError::Timeout) => return Ok(Receive::Nothing),
                Err(err) => {
                    tracing::error!("Failed to receive Rudp ctrl packet");
                    self.ctrl_failed(CtrlQuitReason::Unknown);
                    return Err(err);
                }
            };
        if message.data.len() < 4 {
            tracing::error!("Rudp ctrl message response too small");
            holepunch.rudp_print_message(&message);
            self.ctrl_failed(CtrlQuitReason::Unknown);
            return Err(ChiakiError::InvalidResponse);
        }
        let remote_counter = message.remote_counter;
        // C: ack_counter bleibt über die Sub-Message-Kette hinweg bestehen und
        // wird im default-Zweig ggf. unverändert (0) benutzt.
        let mut ack_counter = 0u16;
        loop {
            // C: "switch(message.subtype) // wrong but works ..."
            match message.subtype {
                // Fallthrough im C: erst Send-Buffer-ACK, dann der 0x02-Body.
                0x12 | 0x26 | 0x36 => {
                    if message.data.len() >= 4 {
                        ack_counter = u16::from_be_bytes([message.data[2], message.data[3]]);
                    }
                    let _ = holepunch.rudp_ack_packet(ack_counter);
                    let _ = holepunch.rudp_send_ack_message(remote_counter);
                    let offset = rudp_packet_type_data_offset(message.subtype);
                    if !self.rudp_try_append_ctrl_frame(&message.data, offset) {
                        return Ok(Receive::Overflow);
                    }
                }
                0x02 => {
                    let _ = holepunch.rudp_send_ack_message(remote_counter);
                    let offset = rudp_packet_type_data_offset(message.subtype);
                    if !self.rudp_try_append_ctrl_frame(&message.data, offset) {
                        return Ok(Receive::Overflow);
                    }
                }
                0x24 => {
                    if message.data.len() >= 4 {
                        ack_counter = u16::from_be_bytes([message.data[2], message.data[3]]);
                    }
                    let _ = holepunch.rudp_ack_packet(ack_counter);
                }
                0xC0 => {
                    tracing::info!("Received rudp finish message, stopping ctrl.");
                    self.ctrl_failed(CtrlQuitReason::Unknown);
                    // C: kein Abbruch — die Session sieht ctrl_failed und
                    // stoppt das Ctrl; der Loop läuft hier weiter.
                }
                _ => {
                    tracing::info!("Received message of unknown type: {:#04x}", message.type_);
                    let _ = holepunch.rudp_ack_packet(ack_counter);
                    let _ = holepunch.rudp_send_ack_message(remote_counter);
                    // we already checked before if data size was at least 4
                    let offset = 4;
                    if !self.rudp_try_append_ctrl_frame(&message.data, offset) {
                        return Ok(Receive::Overflow);
                    }
                }
            }
            // C (ctrl.c:583-599): zur Sub-Message weitergehen oder fertig
            // (pointers_free — hier RAII).
            match message.sub_message.take() {
                Some(sub) => message = *sub,
                None => break,
            }
        }
        Ok(Receive::Data)
    }

    /// C (ctrl.c:546-556/570-580): prüft, ob hinter `offset` ein gültiger
    /// Ctrl-Frame liegt (8-Byte-Header, payload_size passt zur Restlänge) und
    /// hängt Header+Payload an recv_buf. `false` = recv_buf-Überlauf (das C
    /// memcpy'd hier ungeprüft — mögliches UB; hier kontrolliert wie beim
    /// Framing-Overflow).
    fn rudp_try_append_ctrl_frame(&mut self, data: &[u8], offset: usize) -> bool {
        // ctrl message header is 8 bytes
        if data.len() < offset + 8 {
            // C: `break` im switch — nichts tun.
            return true;
        }
        let ctrl_payload_size =
            u32::from_be_bytes([data[offset], data[offset + 1], data[offset + 2], data[offset + 3]])
                as usize;
        let frame_len = data.len() - offset;
        // check if message is ctrl message by making sure the payload size
        // (size of message - 8 byte header) is correct
        if frame_len - 8 == ctrl_payload_size {
            if self.recv_buf_size + frame_len > CTRL_RECV_BUF_SIZE {
                tracing::error!("Ctrl buffer overflow!");
                return false;
            }
            self.recv_buf[self.recv_buf_size..self.recv_buf_size + frame_len]
                .copy_from_slice(&data[offset..]);
            self.recv_buf_size += frame_len;
        }
        true
    }

    fn should_stop(&self) -> bool {
        self.shared.stop_pipe.is_set()
    }

    /// `ctrl_message_send()` (ctrl.c:634-712): Payload verschlüsseln
    /// (crypt_counter_local++), Frame je Transport senden (TCP: send_fully —
    /// RUDP: chiaki_rudp_send_ctrl_message, ctrl.c:674-689).
    fn message_send(&mut self, msg_type: u16, payload: &[u8]) -> ChiakiResult<()> {
        tracing::trace!("Ctrl sending message type {msg_type:#x}, size {}", payload.len());
        if !payload.is_empty() {
            tracing::trace!("Ctrl send payload: {:02x?}", payload);
        }

        // C (ctrl.c:651-657): der LOGIN_PIN_REP-"Sonderfall" im RUDP-Pfad
        // (`local_counter = crypt_counter_local++;
        // encrypt(local_counter - 1, ...)`) ist zähleridentisch zum
        // allgemeinen Pfad (`encrypt(crypt_counter_local++, ...)`) —
        // gemeinsamer Codepfad.
        let frame = encrypt_message(
            &mut self.crypt_counter_local,
            &self.shared.rpcrypt,
            msg_type,
            payload,
        )
        .inspect_err(|_| tracing::error!("Ctrl failed to encrypt payload"))?;

        match &self.transport {
            // C (ctrl.c:674-689): kompletter Frame (Header + verschlüsselter
            // Payload) als RUDP-CTRL-Message. (Das C trunciert `uint8_t
            // buf_size = 8 + payload_size` — hier fährt der volle Frame raus,
            // siehe Moduldoku.)
            CtrlTransport::Holepunch(holepunch) => holepunch
                .rudp_send_ctrl_message(&frame)
                .inspect_err(|_| tracing::error!("Failed to send Ctrl Message")),
            CtrlTransport::Tcp => {
                let sock = self.sock.as_mut().ok_or(ChiakiError::Disconnected)?;
                send_fully(
                    &self.shared.stop_pipe,
                    sock,
                    &frame,
                    CTRL_EXPECT_TIMEOUT_MS,
                )
                .inspect_err(|_| tracing::error!("Failed to send Ctrl Message"))
            }
        }
    }

    /// `ctrl_message_received()` (ctrl.c:795-849): Payload entschlüsseln
    /// (crypt_counter_remote++ nur bei size > 0) und dispatchen.
    fn message_received(&mut self, msg_type: u16, payload: &mut [u8]) {
        if !payload.is_empty() {
            let counter = self.crypt_counter_remote;
            self.crypt_counter_remote += 1;
            if let Err(err) = self.shared.rpcrypt.decrypt(counter, payload) {
                tracing::error!(
                    "Failed to decrypt payload for Ctrl Message type {:#x}: {err}",
                    msg_type
                );
                return;
            }
        }

        tracing::trace!(
            "Ctrl received message of type {:#x}, size {:#x}",
            msg_type,
            payload.len()
        );
        if !payload.is_empty() {
            tracing::trace!("Ctrl recv payload: {:02x?}", payload);
        }

        match msg_type {
            t if t == CtrlMessageType::SessionId as u16 => {
                self.message_received_session_id(payload);
                self.enable_features_on_thread();
            }
            t if t == CtrlMessageType::HeartbeatReq as u16 => {
                self.message_received_heartbeat_req(payload);
            }
            t if t == CtrlMessageType::LoginPinReq as u16 => {
                self.message_received_login_pin_req(payload);
            }
            t if t == CtrlMessageType::Login as u16 => {
                self.message_received_login(payload);
            }
            t if t == CtrlMessageType::KeyboardOpen as u16 => {
                self.message_received_keyboard_open(payload);
            }
            t if t == CtrlMessageType::KeyboardTextChangeRes as u16 => {
                self.message_received_keyboard_text_change(payload);
            }
            t if t == CtrlMessageType::KeyboardCloseRemote as u16 => {
                self.message_received_keyboard_close();
            }
            t if t == CtrlMessageType::Displaya as u16 => {
                self.message_received_displaya(payload);
            }
            t if t == CtrlMessageType::Displayb as u16 => {
                self.message_received_displayb(payload);
            }
            t if t == CtrlMessageType::SwitchToStreamConnection as u16 => {
                self.message_received_switch_to_stream_connection(payload);
            }
            _ => {
                // C: hexdump auf WARNING
                tracing::warn!("Ctrl unknown message {msg_type:#x}: {:02x?}", payload);
            }
        }
    }

    /// `ctrl_enable_features()` vom Ctrl-Thread aus (nach SESSION_ID): im C
    /// werden die Nachrichten hier direkt gesendet — wire-identisch, sofort.
    /// (Für Aufrufe aus anderen Threads: `Ctrl::enable_features()` enqueued.)
    fn enable_features_on_thread(&mut self) {
        if self.shared.enable_dualsense {
            tracing::info!("Enabling DualSense features");
            const ENABLE: [u8; 3] = [0x00, 0x40, 0x00];
            let _ = self.message_send(CtrlMessageType::EnableDualsenseFeatures as u16, &ENABLE);
            const CONNECT: [u8; 0x10] = [
                0xa0, 0xab, 0x51, 0xbd, 0xd1, 0x7e, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff,
                0x00, 0x00, 0x00,
            ];
            let _ = self.message_send(0x11, &CONNECT);
        }
        if self.shared.enable_keyboard {
            tracing::info!("Enabling Keyboard");
            // TODO: Signature ?!
            const SIGNATURE: [u8; 0x10] = [
                0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x05, 0xAE, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00,
            ];
            let _ = self.message_send(CtrlMessageType::KeyboardEnable as u16, &SIGNATURE);
            let _ = self.message_send(CtrlMessageType::KeyboardEnableToggle as u16, &[1u8]);
        }
        let _ = self.message_send(
            CtrlMessageType::MicToggle as u16,
            &mic_toggle_payload(false),
        );
        let _ = self.message_send(
            CtrlMessageType::MicToggle as u16,
            &mic_toggle_payload(false),
        );
        const DISPLAY: [u8; 0x4] = [0x00, 0x00, 0x00, 0x00];
        let _ = self.message_send(CtrlMessageType::DisplayDevices as u16, &DISPLAY);
    }

    /// `ctrl_message_received_session_id()` (ctrl.c:876-939).
    fn message_received_session_id(&mut self, payload: &[u8]) {
        if self.shared.session_id_received.load(Ordering::SeqCst) {
            tracing::warn!("Received another Session Id Message");
            return;
        }

        if payload.len() < 2 {
            tracing::error!(
                "Invalid Session Id \"{}\" received",
                String::from_utf8_lossy(payload)
            );
            self.set_fallback_session_id();
            return;
        }

        if payload[0] != 0x4a {
            tracing::warn!("Received presumably invalid Session Id:");
            tracing::warn!("Session Id payload: {:02x?}", payload);
        }

        match validate_session_id_payload(payload) {
            Some(id) => {
                tracing::info!("Ctrl received valid Session Id: {}", id);
                self.shared.session_id_received.store(true, Ordering::SeqCst);
                self.emit(CtrlEvent::SessionId(id));
            }
            None => {
                // zu lang / zu kurz / ungültige Zeichen — jeweilige C-Logs
                // laufen über die Validierung; hier nur der Fallback.
                if payload.len() - 1 >= SESSION_ID_SIZE_MAX - 1 {
                    tracing::error!("Received Session Id is too long");
                } else if payload.len() - 1 < 24 {
                    tracing::error!("Received Session Id is too short");
                } else {
                    tracing::error!("Ctrl received Session Id contains invalid characters");
                }
                self.set_fallback_session_id();
            }
        }
    }

    /// `ctrl_message_set_fallback_session_id()` aus ctrl.c (Aufruf bei
    /// ungültiger Session-Id).
    fn set_fallback_session_id(&mut self) {
        if self.shared.session_id_received.load(Ordering::SeqCst) {
            tracing::warn!("Aleady received session Id don't need fallback.");
            return;
        }
        match generate_fallback_session_id() {
            Ok(id) => {
                tracing::info!("Ctrl set fallback session Id {}", id);
                self.shared.session_id_received.store(true, Ordering::SeqCst);
                self.emit(CtrlEvent::SessionId(id));
            }
            Err(err) => {
                tracing::error!(
                    "Couldn't generate fallback session Id with error: {err}."
                );
            }
        }
    }

    /// `ctrl_message_received_heartbeat_req()` (ctrl.c:941-948): sofort
    /// HEARTBEAT_REP zurückschicken.
    fn message_received_heartbeat_req(&mut self, payload: &[u8]) {
        if !payload.is_empty() {
            tracing::warn!("Ctrl received Heartbeat request with non-empty payload");
        }
        tracing::info!("Ctrl received Heartbeat, sending reply");
        let _ = self.message_send(CtrlMessageType::HeartbeatRep as u16, &[]);
    }

    /// `ctrl_message_received_login_pin_req()` (ctrl.c:962-983).
    fn message_received_login_pin_req(&mut self, payload: &[u8]) {
        if !payload.is_empty() {
            tracing::warn!("Ctrl received Login PIN request with non-empty payload");
        }
        tracing::info!("Ctrl received Login PIN request");
        self.login_pin_requested = true;

        // If receive login pin request after starting session, quit session
        // as this won't work
        if self.shared.session_id_received.load(Ordering::SeqCst) {
            self.ctrl_failed(CtrlQuitReason::Unknown);
            return;
        }
        self.emit(CtrlEvent::LoginPinRequested(false));
    }

    /// `ctrl_message_received_login()` (ctrl.c:1014-1048).
    fn message_received_login(&mut self, payload: &[u8]) {
        if payload.len() != 1 {
            tracing::warn!(
                "Ctrl received Login message with payload of size {:#x}",
                payload.len()
            );
            if payload.len() < 1 {
                return;
            }
        }

        let state = payload[0];
        match state {
            CTRL_LOGIN_STATE_SUCCESS => {
                tracing::info!("Ctrl received Login message: success");
                self.login_pin_requested = false;
                self.emit(CtrlEvent::LoginSuccess);
            }
            CTRL_LOGIN_STATE_PIN_INCORRECT => {
                tracing::info!("Ctrl received Login message: PIN incorrect");
                if self.login_pin_requested {
                    tracing::info!("Ctrl requesting PIN from Session again");
                    self.emit(CtrlEvent::LoginPinRequested(true));
                } else {
                    tracing::warn!(
                        "Ctrl Login PIN incorrect message, but PIN was not requested"
                    );
                }
            }
            _ => {
                tracing::info!("Ctrl received Login message with state: {:#x}", state);
            }
        }
    }

    /// `ctrl_message_received_displaya()` (ctrl.c:985-997). C liest
    /// payload[0] ungeprüft; hier mind. 1 Byte erzwungen (Memory-Safety).
    fn message_received_displaya(&mut self, payload: &[u8]) {
        if payload.is_empty() {
            return;
        }
        if payload[0] == 0x1 {
            self.cant_displaya = true;
        } else if payload[0] == 0x0 && !self.cant_displayb {
            self.cant_displaya = false;
            tracing::info!("Ctrl received message that the stream can now display.");
            self.emit(CtrlEvent::CantDisplay { meta: 0, cant: false });
        }
    }

    /// `ctrl_message_received_displayb()` (ctrl.c:999-1012). C liest
    /// payload[0]/[1] ungeprüft; hier mind. 2 Bytes erzwungen.
    fn message_received_displayb(&mut self, payload: &[u8]) {
        if payload.len() < 2 {
            return;
        }
        if self.cant_displaya {
            if !(payload[0] == 0x01 && payload[1] == 0xff) && !self.cant_displayb {
                self.emit(CtrlEvent::CantDisplay { meta: 0, cant: true });
                tracing::info!("Ctrl received message that the stream can't display due to displaying some content that can't be streamed.");
                self.cant_displayb = true;
            }
        }
        if self.cant_displayb && payload[0] == 0x01 && payload[1] == 0xff {
            self.cant_displayb = false;
        }
    }

    /// `ctrl_message_received_switch_to_stream_connection()` (ctrl.c:950-960).
    /// Dedupe des zweiten ACK macht die Session (stream_connection_switch_received).
    fn message_received_switch_to_stream_connection(&mut self, payload: &[u8]) {
        if !payload.is_empty() {
            tracing::warn!(
                "Ctrl received Switch to Stream Connection Ack with non-empty payload"
            );
        }
        self.emit(CtrlEvent::SwitchToStreamConnection);
    }

    /// `ctrl_message_received_keyboard_open()` (ctrl.c:1050-1076).
    /// Header: `CtrlKeyboardOpenMessage { uint8 unk[0x1C]; u32 text_length; }`.
    fn message_received_keyboard_open(&mut self, payload: &[u8]) {
        const HEADER_SIZE: usize = 0x20;
        if payload.len() < HEADER_SIZE {
            tracing::error!(
                "Ctrl received invalid message keyboard open with payload size {} while expected size is at least {}",
                payload.len(), HEADER_SIZE
            );
            return;
        }
        let text_length = u32::from_be_bytes([
            payload[0x1c], payload[0x1d], payload[0x1e], payload[0x1f],
        ]) as usize;
        // C: assert(payload_size == HEADER_SIZE + text_length); hier nicht-fatal.
        let text_length = text_length.min(payload.len() - HEADER_SIZE);
        let text = String::from_utf8_lossy(&payload[HEADER_SIZE..HEADER_SIZE + text_length])
            .into_owned();
        self.emit(CtrlEvent::KeyboardOpen(text));
    }

    /// `ctrl_message_received_keyboard_text_change()` (ctrl.c:1089-1115).
    /// Header: `CtrlKeyboardTextResponseMessage { u32 counter; u32 unk;
    /// u32 text_length1; u32 unk2; uint8 unk3[0x10]; u32 unk4; u32 text_length2; }`.
    fn message_received_keyboard_text_change(&mut self, payload: &[u8]) {
        const HEADER_SIZE: usize = 40;
        if payload.len() < HEADER_SIZE {
            tracing::error!(
                "Ctrl received invalid message keyboard text change with payload size {} while expected size is at least {}",
                payload.len(), HEADER_SIZE
            );
            return;
        }
        let text_length = u32::from_be_bytes([
            payload[8], payload[9], payload[10], payload[11],
        ]) as usize;
        // C: assert(payload_size == HEADER_SIZE + text_length1); hier nicht-fatal.
        let text_length = text_length.min(payload.len() - HEADER_SIZE);
        let text = String::from_utf8_lossy(&payload[HEADER_SIZE..HEADER_SIZE + text_length])
            .into_owned();
        self.emit(CtrlEvent::KeyboardTextChange(text));
    }

    /// `ctrl_message_received_keyboard_close()` (ctrl.c:1078-1087).
    fn message_received_keyboard_close(&mut self) {
        self.emit(CtrlEvent::KeyboardRemoteClose);
    }
}

// ---------------------------------------------------------------------------
// Socket-Helper (wie regist.rs; ctrl.c nutzt chiaki_send_fully /
// chiaki_stop_pipe_connect aus utils.c/stoppipe.c)
// ---------------------------------------------------------------------------

/// `chiaki_send_fully()` über einen TCP-Stream: sendet alles; bei WouldBlock
/// wird mit StopPipe + Deadline gewartet.
fn send_fully(
    stop_pipe: &StopPipe,
    sock: &mut TcpStream,
    mut buf: &[u8],
    timeout_ms: u64,
) -> ChiakiResult<()> {
    let deadline = if timeout_ms == u64::MAX {
        None
    } else {
        Some(Instant::now() + Duration::from_millis(timeout_ms))
    };
    while !buf.is_empty() {
        stop_pipe.check()?;
        let remaining = match deadline {
            None => Duration::MAX,
            Some(d) => d.saturating_duration_since(Instant::now()),
        };
        if remaining.is_zero() {
            return Err(ChiakiError::Timeout);
        }
        sock.set_write_timeout(Some(remaining.min(RECV_POLL)))
            .map_err(|_| ChiakiError::Network)?;
        match sock.write(buf) {
            Ok(0) => return Err(ChiakiError::Network),
            Ok(n) => buf = &buf[n..],
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                continue
            }
            Err(e) => return Err(map_io_error(&e)),
        }
    }
    Ok(())
}

/// `chiaki_stop_pipe_connect()`-Äquivalent für TCP: connect_timeout in
/// Teilstücken, dazwischen Stop-Flag prüfen (siehe stoppipe.rs-Doku).
fn stop_pipe_connect(
    addr: &SocketAddr,
    stop_pipe: &StopPipe,
    timeout_ms: u64,
) -> ChiakiResult<TcpStream> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        stop_pipe.check()?;
        let now = Instant::now();
        if now >= deadline {
            return Err(ChiakiError::Timeout);
        }
        let slice = (deadline - now).min(RECV_POLL);
        match TcpStream::connect_timeout(addr, slice) {
            Ok(sock) => return Ok(sock),
            Err(e) if e.kind() == ErrorKind::TimedOut => return Err(ChiakiError::Timeout),
            Err(e) if e.kind() == ErrorKind::Interrupted || e.kind() == ErrorKind::WouldBlock => {
                continue
            }
            Err(e) => return Err(map_io_error(&e)),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, UdpSocket};
    use std::sync::mpsc;
    use std::time::Duration;

    fn hx(s: &str) -> Vec<u8> {
        hex::decode(s).expect("valid hex")
    }

    fn k16(s: &str) -> [u8; 16] {
        let v = hx(s);
        let mut out = [0u8; 16];
        out.copy_from_slice(&v);
        out
    }

    /// Test-Rpcrypt mit den Golden-Vektoren aus rpcrypt.rs (PS4 10.0).
    fn test_rpcrypt() -> Rpcrypt {
        let nonce = k16("ae92e764882651ef89018cfa696c6938");
        let morning = k16("74a59c9693c2083ba6a84ba050fa8e5a");
        Rpcrypt::new_auth(Target::Ps4_10, &nonce, &morning).unwrap()
    }

    // ---- Framing ----

    #[test]
    fn frame_header_golden() {
        // goto_bed: keine Payload, Type 0x50
        assert_eq!(
            build_header(0, 0x50),
            [0x00, 0x00, 0x00, 0x00, 0x00, 0x50, 0x00, 0x00]
        );
        // Heartbeat-Rep: Type 0x1fe -> BE-Bytes 01 fe
        assert_eq!(
            build_header(0, CtrlMessageType::HeartbeatRep as u16),
            [0x00, 0x00, 0x00, 0x00, 0x01, 0xfe, 0x00, 0x00]
        );
        // Heartbeat-Req vom Server: Type 0x00fe
        assert_eq!(
            build_header(0, CtrlMessageType::HeartbeatReq as u16),
            [0x00, 0x00, 0x00, 0x00, 0x00, 0xfe, 0x00, 0x00]
        );
        // Mic-Toggle: 4 Bytes Payload, Type 0x36
        assert_eq!(
            build_header(4, 0x36),
            [0x00, 0x00, 0x00, 0x04, 0x00, 0x36, 0x00, 0x00]
        );
        // Session-Id: 33 Bytes Payload, Type 0x33
        assert_eq!(
            build_header(33, 0x33),
            [0x00, 0x00, 0x00, 33, 0x00, 0x33, 0x00, 0x00]
        );
    }

    #[test]
    fn message_type_discriminants_match_c() {
        // ctrl_message_type_t (Werte protokollrelevant)
        assert_eq!(CtrlMessageType::SessionId as u16, 0x33);
        assert_eq!(CtrlMessageType::HeartbeatReq as u16, 0xfe);
        assert_eq!(CtrlMessageType::HeartbeatRep as u16, 0x1fe);
        assert_eq!(CtrlMessageType::LoginPinReq as u16, 0x4);
        assert_eq!(CtrlMessageType::LoginPinRep as u16, 0x8004);
        assert_eq!(CtrlMessageType::Login as u16, 0x5);
        assert_eq!(CtrlMessageType::GotoBed as u16, 0x50);
        assert_eq!(CtrlMessageType::KeyboardEnable as u16, 0xd);
        assert_eq!(CtrlMessageType::KeyboardEnableToggle as u16, 0x20);
        assert_eq!(CtrlMessageType::KeyboardOpen as u16, 0x21);
        assert_eq!(CtrlMessageType::KeyboardCloseRemote as u16, 0x22);
        assert_eq!(CtrlMessageType::KeyboardTextChangeReq as u16, 0x23);
        assert_eq!(CtrlMessageType::KeyboardTextChangeRes as u16, 0x24);
        assert_eq!(CtrlMessageType::KeyboardCloseReq as u16, 0x25);
        assert_eq!(CtrlMessageType::EnableDualsenseFeatures as u16, 0x13);
        assert_eq!(CtrlMessageType::GoHome as u16, 0x14);
        assert_eq!(CtrlMessageType::Displaya as u16, 0x1);
        assert_eq!(CtrlMessageType::Displayb as u16, 0x16);
        assert_eq!(CtrlMessageType::MicConnect as u16, 0x30);
        assert_eq!(CtrlMessageType::MicToggle as u16, 0x36);
        assert_eq!(CtrlMessageType::DisplayDevices as u16, 0x910);
        assert_eq!(CtrlMessageType::SwitchToStreamConnection as u16, 0x34);
    }

    /// Verschlüsselte Nachricht: Header im Klartext, Payload CFB128 mit
    /// crypt_counter; Golden via Entschlüsselung mit demselben Zähler.
    #[test]
    fn encrypted_message_golden() {
        let rpcrypt = test_rpcrypt();

        let frame = build_encrypted_message(&rpcrypt, 5, 0x36, &[0, 1, 1, 89]).unwrap();
        // Header
        assert_eq!(
            &frame[..8],
            &[0x00, 0x00, 0x00, 0x04, 0x00, 0x36, 0x00, 0x00]
        );
        // Payload ist verschlüsselt (nicht Klartext)
        assert_ne!(&frame[8..], &[0, 1, 1, 89][..]);
        // Entschlüsselung mit counter=5 ergibt den Klartext
        let mut payload = frame[8..].to_vec();
        rpcrypt.decrypt(5, &mut payload).unwrap();
        assert_eq!(payload, vec![0, 1, 1, 89]);
    }

    /// crypt_counter_local-Advance: der Zähler läuft nur bei nicht-leerem
    /// Payload hoch (C: `if(payload) encrypt(local_counter++, ...)`).
    #[test]
    fn crypt_counter_advances_per_message() {
        let rpcrypt = test_rpcrypt();
        let mut counter: u64 = 0;

        let f0 = encrypt_message(&mut counter, &rpcrypt, 0x50, &[]).unwrap();
        let f1 = encrypt_message(&mut counter, &rpcrypt, 0x36, &[0, 1, 0, 89]).unwrap();
        let f2 = encrypt_message(&mut counter, &rpcrypt, 0x14, &[0u8; 16]).unwrap();

        // leerer Payload -> kein Advance (wie im C)
        assert_eq!(counter, 2);
        assert_eq!(&f0[..6], &[0, 0, 0, 0, 0, 0x50]);
        assert_eq!(f0.len(), 8);

        // Payloads lassen sich mit ihren Zählern entschlüsseln:
        let mut p1 = f1[8..].to_vec();
        rpcrypt.decrypt(0, &mut p1).unwrap();
        assert_eq!(p1, vec![0, 1, 0, 89]);
        // ... aber nicht mit dem falschen Zähler:
        let mut p1_wrong = f1[8..].to_vec();
        rpcrypt.decrypt(1, &mut p1_wrong).unwrap();
        assert_ne!(p1_wrong, vec![0, 1, 0, 89]);

        let mut p2 = f2[8..].to_vec();
        rpcrypt.decrypt(1, &mut p2).unwrap();
        assert_eq!(p2, vec![0u8; 16]);
    }

    // ---- Payload-Formatter ----

    #[test]
    fn mic_toggle_payload_golden() {
        // C: {0, 1, 1, 89}, muted -> toggle[2] = 0
        assert_eq!(mic_toggle_payload(false), [0, 1, 1, 89]);
        assert_eq!(mic_toggle_payload(true), [0, 1, 0, 89]);
    }

    #[test]
    fn keyboard_text_request_golden() {
        // CtrlKeyboardTextRequestMessage: counter(4) text_length1(4)
        // unk1(8) unk2(16) text_length2(4), dann Text — memset 0, alles BE.
        let payload = build_keyboard_text_request(1, "abc");
        assert_eq!(payload.len(), 36 + 3);
        assert_eq!(&payload[..8], &[0, 0, 0, 1, 0, 0, 0, 3]); // counter=1, len=3
        assert_eq!(&payload[8..32], &[0u8; 24][..]); // unk1+unk2
        assert_eq!(&payload[32..36], &[0, 0, 0, 3]); // text_length2
        assert_eq!(&payload[36..], b"abc");

        // counter zählt hoch (C: ++ctrl->keyboard_text_counter)
        let payload2 = build_keyboard_text_request(0x1234, "");
        assert_eq!(&payload2[..4], &[0, 0, 0x12, 0x34]);
        assert_eq!(payload2.len(), 36);
    }

    // ---- Session-Id-Validierung (ctrl.c:876-939) ----

    #[test]
    fn session_id_validation() {
        // erstes Byte wird übersprungen ("size")
        let valid = {
            let mut p = vec![0x4a];
            p.extend_from_slice(b"abcdefghijklmnopqrstuvwx"); // 24 alnum
            p
        };
        assert_eq!(
            validate_session_id_payload(&valid).as_deref(),
            Some("abcdefghijklmnopqrstuvwx")
        );

        // zu kurz (< 24 nach Skip)
        let mut too_short = vec![24u8];
        too_short.extend_from_slice(b"abcdefghijklmnopqrstuvw"); // 23
        assert_eq!(validate_session_id_payload(&too_short), None);

        // zu lang (>= SESSION_ID_SIZE_MAX - 1 nach Skip)
        let too_long = vec![0u8; SESSION_ID_SIZE_MAX]; // 80 Bytes -> 79 nach Skip
        assert_eq!(validate_session_id_payload(&too_long), None);

        // ungültige Zeichen
        let mut invalid = vec![5u8];
        invalid.extend_from_slice(b"abcdefghijklmnopqrstuvwx");
        invalid[5] = b'!';
        assert_eq!(validate_session_id_payload(&invalid), None);

        // zu kleiner Payload (< 2, ctrl.c:887)
        assert_eq!(validate_session_id_payload(&[0x4a]), None);
        assert_eq!(validate_session_id_payload(&[]), None);
    }

    #[test]
    fn fallback_session_id_shape() {
        let id = generate_fallback_session_id().unwrap();
        // dezimaler Sekundenstempel (>= 10 Ziffern heutzutage, <= 15 wie im C
        // via snprintf mit size 16) + 64 base64-Zeichen = 65..=79 Zeichen
        assert!(
            id.len() >= 65 && id.len() <= 79,
            "unexpected fallback session id length: {}",
            id.len()
        );
        let (ts, b64part) = id.split_at(id.len() - 64);
        assert!(ts.chars().all(|c| c.is_ascii_digit()));
        assert!(ts.len() <= 15);
        assert!(b64part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='));
    }

    // ---- Ctrl-Response-Parsing (ctrl.c:1117-1154) ----

    fn http_with_headers(headers: Vec<crate::http::HttpHeader>) -> HttpResponse {
        HttpResponse {
            code: 200,
            headers,
        }
    }

    #[test]
    fn parse_ctrl_response_prohibit() {
        let response = http_with_headers(vec![crate::http::HttpHeader {
            key: "RP-Prohibit".to_string(),
            value: "1".to_string(),
        }]);
        let mut parsed = CtrlResponse::default();
        parse_ctrl_response(&mut parsed, &response);
        assert!(parsed.success);
        assert!(!parsed.server_type_valid);
        assert!(parsed.rp_prohibit);

        let response0 = http_with_headers(vec![crate::http::HttpHeader {
            key: "RP-Prohibit".to_string(),
            value: "0".to_string(),
        }]);
        let mut parsed = CtrlResponse::default();
        parse_ctrl_response(&mut parsed, &response0);
        assert!(parsed.success);
        assert!(!parsed.rp_prohibit);

        // atoi-Semantik: kein int -> 0 -> false
        let responsejunk = http_with_headers(vec![crate::http::HttpHeader {
            key: "RP-Prohibit".to_string(),
            value: "junk".to_string(),
        }]);
        let mut parsed = CtrlResponse::default();
        parse_ctrl_response(&mut parsed, &responsejunk);
        assert!(!parsed.rp_prohibit);
    }

    /// RP-Server-Type: gepaddetes base64 (16 Bytes) wird inkl. des vom C
    /// mitgegebenen NUL-Terminators korrekt dekodiert ('=' terminiert das
    /// Parsen vorher); server_type_valid ist true.
    #[test]
    fn parse_ctrl_response_server_type() {
        let mut server_type = [0u8; 16];
        server_type[0] = 2;
        let response = http_with_headers(vec![crate::http::HttpHeader {
            key: "RP-Server-Type".to_string(),
            value: base64::encode(&server_type),
        }]);
        let mut parsed = CtrlResponse::default();
        parse_ctrl_response(&mut parsed, &response);
        assert!(parsed.success);
        assert!(parsed.server_type_valid);
        assert_eq!(parsed.rp_server_type, server_type);
        assert!(!parsed.rp_prohibit);
    }

    /// Ungepadetes base64 läuft im C (strlen+1) in den NUL und scheitert
    /// (NUL ist in der base64-Tabelle invalid) -> success=false. 1:1 erhalten.
    #[test]
    fn parse_ctrl_response_server_type_unpadded_fails_like_c() {
        // 3 Bytes -> 4 base64-Zeichen, kein '='
        let response = http_with_headers(vec![crate::http::HttpHeader {
            key: "RP-Server-Type".to_string(),
            value: base64::encode(&[1u8, 2, 3]),
        }]);
        let mut parsed = CtrlResponse::default();
        parse_ctrl_response(&mut parsed, &response);
        assert!(!parsed.success, "C: NUL im ungepadten base64 -> decode failure");
    }

    #[test]
    fn parse_ctrl_response_nonok_code() {
        let response = HttpResponse {
            code: 503,
            headers: vec![],
        };
        let mut parsed = CtrlResponse::default();
        parse_ctrl_response(&mut parsed, &response);
        assert!(!parsed.success);
    }

    #[test]
    fn atoi_semantics() {
        assert_eq!(atoi_i32("1"), 1);
        assert_eq!(atoi_i32("0"), 0);
        assert_eq!(atoi_i32(" 12x"), 12);
        assert_eq!(atoi_i32("junk"), 0);
        assert_eq!(atoi_i32(""), 0);
        assert_eq!(atoi_i32("-3"), -3);
    }

    #[test]
    fn rp_version_table() {
        // muss mit chiaki_rp_version_string (session.c) identisch sein
        assert_eq!(rp_version_string(Target::Ps4_8), Some("8.0"));
        assert_eq!(rp_version_string(Target::Ps4_9), Some("9.0"));
        assert_eq!(rp_version_string(Target::Ps4_10), Some("10.0"));
        assert_eq!(rp_version_string(Target::Ps5_1), Some("1.0"));
        assert_eq!(rp_version_string(Target::Ps4Unknown), None);
        assert_eq!(rp_version_string(Target::Ps5Unknown), None);
    }

    // ---- Loopback: Ctrl-Protokoll gegen einen Fake-Server ----

    fn read_frame(sock: &mut TcpStream) -> ([u8; 8], Vec<u8>) {
        let mut header = [0u8; 8];
        sock.read_exact(&mut header).unwrap();
        let size = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let mut payload = vec![0u8; size];
        if size > 0 {
            sock.read_exact(&mut payload).unwrap();
        }
        (header, payload)
    }

    /// Minimaler Fake-Server wie in ctrl.c erwartet: HTTP-Handshake, dann
    /// Framing (Heartbeat-Req/Rep, Session-Id, Enable-Features, Login-PIN,
    /// Keyboard, GotoBed, GoHome, Mic). Prüft Golden-Bytes und die
    /// crypt_counter-Advance-Logik in beiden Richtungen.
    #[test]
    fn ctrl_loopback_fake_server() {
        let nonce = k16("ae92e764882651ef89018cfa696c6938");
        let morning = k16("74a59c9693c2083ba6a84ba050fa8e5a");

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host_addr = listener.local_addr().unwrap();

        // Session-Id-Payload: [size] + 30 alnum (remote counter 1)
        let session_id = b"chiakiSessionIdTest00000000abc";
        let mut sid_plain = vec![session_id.len() as u8];
        sid_plain.extend_from_slice(session_id);

        let rpcrypt_for_server = Rpcrypt::new_auth(Target::Ps4_10, &nonce, &morning).unwrap();
        let sid_enc = rpcrypt_for_server.encrypt_buf(1, &sid_plain).unwrap();
        // RP-Server-Type: [2, 0, ...] verschlüsselt mit REMOTE counter 0
        let mut server_type_plain = [0u8; 16];
        server_type_plain[0] = 2;
        let server_type_enc = rpcrypt_for_server.encrypt_buf(0, &server_type_plain).unwrap();

        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

            // --- HTTP-Request empfangen und prüfen ---
            let mut req = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                sock.read_exact(&mut byte).unwrap();
                req.push(byte[0]);
                if req.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let req = String::from_utf8(req).unwrap();
            assert!(
                req.starts_with("GET /sie/ps4/rp/sess/ctrl HTTP/1.1\r\n"),
                "req: {req}"
            );
            assert!(req.contains("Host: 127.0.0.1:9295\r\n"));
            assert!(req.contains("User-Agent: remoteplay Windows\r\n"));
            assert!(req.contains("Connection: keep-alive\r\n"));
            assert!(req.contains("Content-Length: 0\r\n"));
            assert!(req.contains("RP-Auth: "));
            assert!(req.contains("RP-Version: 10.0\r\n"));
            assert!(req.contains("RP-Did: "));
            assert!(req.contains("RP-ControllerType: 3\r\n"));
            assert!(req.contains("RP-ClientType: 11\r\n"));
            assert!(req.contains("RP-OSType: "));
            assert!(req.contains("RP-ConPath: 1\r\n"));
            // target >= PS4_10 -> StartBitrate da (4 verschlüsselte NULs)
            assert!(req.contains("RP-StartBitrate: "));
            assert!(!req.contains("RP-StreamingType: ")); // kein PS5

            // --- HTTP-Response mit RP-Server-Type (2 = PS5-ähnlich; hier nur
            //     Prototyp — die Downgrade-Logik läuft in der Session) ---
            sock.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nRP-Server-Type: {}\r\n\r\n",
                    base64::encode(&server_type_enc)
                )
                .as_bytes(),
            )
            .unwrap();

            // --- Heartbeat-Req schicken; Golden-Rep erwarten ---
            sock.write_all(&build_header(0, CtrlMessageType::HeartbeatReq as u16))
                .unwrap();
            let (header, payload) = read_frame(&mut sock);
            assert_eq!(
                header,
                [0x00, 0x00, 0x00, 0x00, 0x01, 0xfe, 0x00, 0x00],
                "Heartbeat-Rep-Frame"
            );
            assert!(payload.is_empty());

            // --- Login-PIN-Req VOR der Session-Id (nach der Session-Id führt
            //     ein PIN-Request korrekt zu ctrl_failed, ctrl.c:971-979).
            //     PIN-Rep mit crypt_counter_local 4 erwarten. ---
            sock.write_all(&build_header(0, CtrlMessageType::LoginPinReq as u16))
                .unwrap();
            let (header, payload) = read_frame(&mut sock);
            assert_eq!(
                &header[..6],
                &build_header(4, CtrlMessageType::LoginPinRep as u16)[..6]
            );
            assert_eq!(
                rpcrypt_for_server.decrypt_buf(4, &payload).unwrap(),
                b"1234"
            );

            // --- Session-Id schicken (remote counter 1) ---
            let mut frame =
                build_header(sid_enc.len(), CtrlMessageType::SessionId as u16).to_vec();
            frame.extend_from_slice(&sid_enc);
            sock.write_all(&frame).unwrap();

            // --- Enable-Features: mic toggle x2 + display devices ---
            // crypt_counter_local: 0=auth,1=did,2=ostype,3=bitrate,4=pin-rep
            //                     -> hier 5,6,7
            let expected_features: [(u16, u64, Vec<u8>); 3] = [
                (CtrlMessageType::MicToggle as u16, 5, vec![0, 1, 1, 89]),
                (CtrlMessageType::MicToggle as u16, 6, vec![0, 1, 1, 89]),
                (CtrlMessageType::DisplayDevices as u16, 7, vec![0, 0, 0, 0]),
            ];
            for (msg_type, counter, plain) in expected_features {
                let (header, payload) = read_frame(&mut sock);
                assert_eq!(&header[..6], &build_header(plain.len(), msg_type)[..6]);
                let decrypted = rpcrypt_for_server.decrypt_buf(counter, &payload).unwrap();
                assert_eq!(decrypted, plain, "feature msg {msg_type:#x}");
            }

            // --- Session-Kommandos: accept, set_text, reject, goto_bed,
            //     go_home, toggle mic, connect mic (counter 8..14) ---
            let (header, payload) = read_frame(&mut sock);
            assert_eq!(
                &header[..6],
                &build_header(4, CtrlMessageType::KeyboardCloseReq as u16)[..6]
            );
            assert_eq!(
                rpcrypt_for_server.decrypt_buf(8, &payload).unwrap(),
                vec![0, 0, 0, 0]
            );

            let (header, payload) = read_frame(&mut sock);
            assert_eq!(
                &header[..6],
                &build_header(36 + 5, CtrlMessageType::KeyboardTextChangeReq as u16)[..6]
            );
            let text_msg = rpcrypt_for_server.decrypt_buf(9, &payload).unwrap();
            assert_eq!(&text_msg[..8], &[0, 0, 0, 1, 0, 0, 0, 5]); // kbd counter 1, len 5
            assert_eq!(&text_msg[36..], b"hello");

            let (header, payload) = read_frame(&mut sock);
            assert_eq!(
                &header[..6],
                &build_header(4, CtrlMessageType::KeyboardCloseReq as u16)[..6]
            );
            assert_eq!(
                rpcrypt_for_server.decrypt_buf(10, &payload).unwrap(),
                vec![0, 0, 0, 1]
            );

            let (header, payload) = read_frame(&mut sock);
            assert_eq!(&header[..6], &build_header(0, CtrlMessageType::GotoBed as u16)[..6]);
            assert!(payload.is_empty());

            let (header, payload) = read_frame(&mut sock);
            assert_eq!(&header[..6], &build_header(0x10, CtrlMessageType::GoHome as u16)[..6]);
            assert_eq!(
                // goto_bed (leerer Payload) hat den Zähler nicht erhöht
                rpcrypt_for_server.decrypt_buf(11, &payload).unwrap(),
                vec![0x00, 0xff, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
            );

            let (header, payload) = read_frame(&mut sock);
            assert_eq!(&header[..6], &build_header(4, CtrlMessageType::MicToggle as u16)[..6]);
            assert_eq!(
                rpcrypt_for_server.decrypt_buf(12, &payload).unwrap(),
                vec![0, 1, 0, 89]
            );

            let (header, payload) = read_frame(&mut sock);
            assert_eq!(&header[..6], &build_header(2, CtrlMessageType::MicConnect as u16)[..6]);
            assert_eq!(
                rpcrypt_for_server.decrypt_buf(13, &payload).unwrap(),
                vec![0, 0]
            );

            // Sync: ACK-Nachricht an den Ctrl -> Test-Thread wartet auf das
            // SwitchToStreamConnection-Event, bevor er stop() ruft. (Ein
            // stop() direkt nach dem Enqueue würde die Queued-Sends wie im C
            // über send_fully(CANCELED) verwerfen.)
            sock.write_all(&build_header(0, CtrlMessageType::SwitchToStreamConnection as u16))
                .unwrap();

            // Socket offen halten, bis der Test ctrl.stop() gerufen hat;
            // dann schließen -> Ctrl-Loop endet per EOF ohne Quit.
            std::thread::sleep(Duration::from_millis(600));
        });

        // --- Ctrl aufsetzen ---
        let (event_tx, event_rx) = mpsc::channel();
        let (msg_queue_tx, msg_queue_rx) = mpsc::channel();
        let mut ctrl = Ctrl::new(CtrlInit {
            rpcrypt: Rpcrypt::new_auth(Target::Ps4_10, &nonce, &morning).unwrap(),
            target: Target::Ps4_10,
            regist_key: *b"regist_key_regs\0",
            did: [0x42; RP_DID_SIZE],
            hostname: "127.0.0.1".to_string(),
            host_addr,
            sock: None,
            transport: CtrlTransport::Tcp,
            codec: Codec::H264,
            enable_dualsense: false,
            enable_keyboard: false,
            msg_queue_tx,
            msg_queue_rx,
            event_cb: Arc::new(move |ev| {
                let _ = event_tx.send(ev);
            }),
        })
        .unwrap();
        ctrl.start().unwrap();

        // ServerType-Event (aus dem HTTP-Handshake, remote counter 0)
        match event_rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            CtrlEvent::ServerType { server_type } => {
                assert_eq!(server_type, 2);
            }
            other => panic!("expected ServerType, got {other:?}"),
        }

        // PIN-Request-Event (vor der Session-Id) -> PIN setzen
        match event_rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            CtrlEvent::LoginPinRequested(pin_incorrect) => {
                assert!(!pin_incorrect);
            }
            other => panic!("expected LoginPinRequested, got {other:?}"),
        }
        ctrl.set_login_pin(b"1234");

        // Session-Id-Event
        match event_rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            CtrlEvent::SessionId(id) => {
                assert_eq!(id, "chiakiSessionIdTest00000000abc");
                assert!(ctrl.session_id_received());
            }
            other => panic!("expected SessionId, got {other:?}"),
        }

        // Session-Kommandos (der Server prüft die Frames oben)
        ctrl.keyboard_accept().unwrap();
        ctrl.keyboard_set_text("hello").unwrap();
        ctrl.keyboard_reject().unwrap();
        ctrl.goto_bed().unwrap();
        ctrl.go_home().unwrap();
        ctrl.toggle_microphone(true).unwrap();
        ctrl.connect_microphone().unwrap();

        // Auf den ACK des Servers warten, damit sicher alle Queued-Sends
        // raus sind (s. Kommentar im Server-Thread).
        match event_rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            CtrlEvent::SwitchToStreamConnection => {}
            other => panic!("expected SwitchToStreamConnection, got {other:?}"),
        }

        // sauber herunterfahren
        ctrl.stop();
        ctrl.join().unwrap();
        let _ = server.join().unwrap();

        // Nach sauberem EOF darf kein Quit-Event kommen
        match event_rx.recv_timeout(Duration::from_millis(200)) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {}
            other => panic!("unexpected event after clean shutdown: {other:?}"),
        }
    }

    /// Ctrl-Fehlerpfad: Verbindungsaufbau zu einer toten Adresse -> der
    /// Ctrl-Thread meldet Quit-Events; letzter ist ConnectFailed (das C
    /// überschreibt quit_reason in ctrl_thread_func).
    #[test]
    fn ctrl_connect_failure_reports_quit() {
        let nonce = k16("ae92e764882651ef89018cfa696c6938");
        let morning = k16("74a59c9693c2083ba6a84ba050fa8e5a");

        // Port 1 auf localhost: nichts lauscht dort -> refused
        let host_addr: SocketAddr = "127.0.0.1:1".parse().unwrap();

        let (event_tx, event_rx) = mpsc::channel();
        let (msg_queue_tx, msg_queue_rx) = mpsc::channel();
        let mut ctrl = Ctrl::new(CtrlInit {
            rpcrypt: Rpcrypt::new_auth(Target::Ps4_10, &nonce, &morning).unwrap(),
            target: Target::Ps4_10,
            regist_key: [0u8; RPCRYPT_KEY_SIZE],
            did: [0u8; RP_DID_SIZE],
            hostname: "127.0.0.1".to_string(),
            host_addr,
            sock: None,
            transport: CtrlTransport::Tcp,
            codec: Codec::H264,
            enable_dualsense: false,
            enable_keyboard: false,
            msg_queue_tx,
            msg_queue_rx,
            event_cb: Arc::new(move |ev| {
                let _ = event_tx.send(ev);
            }),
        })
        .unwrap();
        ctrl.start().unwrap();

        // Beim Connect-Fehler feuert ctrl_connect_tcp den inneren Grund und
        // danach ctrl_thread_func ConnectFailed (C-Verhalten: quit_reason
        // wird überschrieben -> letzter Event gewinnt).
        let mut last = None;
        while let Ok(ev) = event_rx.recv_timeout(Duration::from_secs(10)) {
            last = Some(ev);
            if matches!(last, Some(CtrlEvent::Quit(CtrlQuitReason::ConnectFailed))) {
                break;
            }
        }
        assert!(
            matches!(last, Some(CtrlEvent::Quit(CtrlQuitReason::ConnectFailed))),
            "expected Quit(ConnectFailed) last, got {last:?}"
        );

        ctrl.join().unwrap();
    }

    // ---- RUDP-Loopback (Muster aus chiaki-remote/src/regist_psn.rs-Tests:
    //      simulierter Konsole-Peer über 127.0.0.1-UDP mit RUDP-Handshake,
    //      dann Ctrl-Messages) ----
    //
    // chiaki-core darf chiaki-remote nicht als Dependency ziehen — das
    // minimale RUDP-Wire-Codec hier spiegelt chiaki-remote::rudp (dort
    // golden-geprüft); die Client-Trait-Impl spiegelt die Produktions-Impl
    // in regist_psn.rs.

    /// RUDP_CONSTANT (rudp.c).
    const T_RUDP_CONSTANT: u32 = 0x244F_244F;
    /// RudpPacketType-Werte (rudp.rs / rudp.h).
    const T_INIT_REQUEST: u16 = 0x8030;
    const T_INIT_RESPONSE: u16 = 0xD000;
    const T_COOKIE_REQUEST: u16 = 0x9030;
    const T_COOKIE_RESPONSE: u16 = 0xA030;
    const T_SESSION_MESSAGE: u16 = 0x2030;
    const T_ACK: u16 = 0x2430;
    const T_CTRL_MESSAGE: u16 = 0x0230;
    /// Offset8 (Subtype 0x12, ctrl.c-Dispatch).
    const T_OFFSET8: u16 = 0x1230;
    const T_FINISH: u16 = 0xC000;

    /// Vom Test als ps_ctrl_port gemeldeter Port (bewusst != 9295, um den
    /// ctrl.c:1299-Zweig zu prüfen).
    const TEST_PS_CTRL_PORT: u16 = 9300;
    /// ps_selected_addr-Äquivalent der Test-Holepunch-Session.
    const TEST_PS_SELECTED_ADDR: &str = "10.0.0.1";

    #[derive(Debug, Clone)]
    struct TMsg {
        subtype: u8,
        type_: u16,
        data: Vec<u8>,
        sub: Option<Box<TMsg>>,
        remote_counter: u16,
    }

    fn rudp_frame(type_: u16, data: Vec<u8>) -> TMsg {
        TMsg {
            subtype: (type_ >> 8) as u8,
            type_,
            data,
            sub: None,
            remote_counter: 0,
        }
    }

    /// `rudp_message_serialize` (eine Ebene + Sub-Message).
    fn t_serialize(msg: &TMsg) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + msg.data.len());
        out.extend_from_slice(&(((0xC << 12) | (8 + msg.data.len())) as u16).to_be_bytes());
        out.extend_from_slice(&T_RUDP_CONSTANT.to_be_bytes());
        out.extend_from_slice(&msg.type_.to_be_bytes());
        out.extend_from_slice(&msg.data);
        if let Some(sub) = &msg.sub {
            out.extend_from_slice(&t_serialize(sub));
        }
        out
    }

    /// `chiaki_rudp_message_parse` (inkl. Sub-Message-Kette, remote_counter
    /// = data[0..2] + 1).
    fn t_parse(buf: &[u8]) -> TMsg {
        assert!(buf.len() >= 8, "RUDP-Header < 8 Bytes");
        let size = u16::from_be_bytes([buf[0], buf[1]]);
        let type_ = u16::from_be_bytes([buf[6], buf[7]]);
        let subtype = buf[6];
        let length = (size & 0x0FFF) as usize;
        let mut remote_counter = 0;
        let mut data = Vec::new();
        let mut remaining = buf.len() as i64 - 8;
        if length > 8 {
            let data_size = (length - 8).min(remaining.max(0) as usize);
            data = buf[8..8 + data_size].to_vec();
            if data_size >= 2 {
                remote_counter = u16::from_be_bytes([data[0], data[1]]).wrapping_add(1);
            }
            remaining -= data_size as i64;
        }
        let mut sub = None;
        if remaining >= 8 {
            let off = 8 + data.len();
            sub = Some(Box::new(t_parse(&buf[off..])));
        }
        TMsg {
            subtype,
            type_,
            data,
            sub,
            remote_counter,
        }
    }

    fn to_ctrl_msg(msg: TMsg) -> crate::session::CtrlRudpMessage {
        crate::session::CtrlRudpMessage {
            subtype: msg.subtype,
            type_: msg.type_,
            remote_counter: msg.remote_counter,
            data: msg.data,
            sub_message: msg.sub.map(|s| Box::new(to_ctrl_msg(*s))),
        }
    }

    /// Client-Seite des RUDP-Transports für den Test — spiegelt die
    /// Produktions-Impl aus chiaki-remote (regist_psn.rs-Trait-Impl) über ein
    /// verbundenes UDP-Socket.
    struct TestRudp {
        sock: UdpSocket,
        counter: Mutex<u16>,
        header: u32,
        /// rudp_ack_packet-Aufrufe (Send-Buffer-ACKs) für Assertions.
        acked: Mutex<Vec<u16>>,
        /// Empfangs-Poll-Intervall (damit ctrl.stop() greift).
        recv_timeout: Duration,
    }

    impl TestRudp {
        /// Vermascht ein bereits gebundenes Socket mit der Gegenstelle
        /// (loopback_pair-Muster aus regist_psn.rs: beide Seiten verbinden
        /// sich gegenseitig).
        fn wrap(sock: UdpSocket, peer: SocketAddr) -> ChiakiResult<Self> {
            sock.connect(peer).unwrap();
            Ok(TestRudp {
                sock,
                counter: Mutex::new(0x0300),
                header: 0x1122_3344,
                acked: Mutex::new(Vec::new()),
                recv_timeout: Duration::from_millis(150),
            })
        }

        fn next_counter(&self) -> u16 {
            let mut c = self.counter.lock().unwrap();
            let v = *c;
            *c = c.wrapping_add(1);
            v
        }

        fn local_counter(&self) -> u16 {
            *self.counter.lock().unwrap()
        }

        fn send_msg(&self, msg: &TMsg) -> ChiakiResult<()> {
            self.sock
                .send(&t_serialize(msg))
                .map(|_| ())
                .map_err(|_| ChiakiError::Network)
        }

        fn recv_msg(&self, buf_size: usize) -> ChiakiResult<TMsg> {
            let mut buf = vec![0u8; buf_size];
            self.sock.set_read_timeout(Some(self.recv_timeout)).unwrap();
            let n = self.sock.recv(&mut buf).map_err(|e| {
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut
                {
                    ChiakiError::Timeout
                } else {
                    ChiakiError::Network
                }
            })?;
            if n <= 8 {
                return Err(ChiakiError::Network);
            }
            Ok(t_parse(&buf[..n]))
        }

        /// INIT/COOKIE-Handshake (ctrl.c:1165-1187).
        fn start_session(&self) -> ChiakiResult<u16> {
            let init_data = self.init_cookie_data(&[]);
            self.send_msg(&rudp_frame(T_INIT_REQUEST, init_data))?;
            let resp = self.recv_msg(1500)?;
            assert_eq!(resp.subtype, 0xD0, "INIT_RESPONSE erwartet");
            // C: init_response = message.data[8..] (ctrl.c:1175-1177).
            let init_response = resp.data[8..].to_vec();
            let cookie_data = self.init_cookie_data(&init_response);
            self.send_msg(&rudp_frame(T_COOKIE_REQUEST, cookie_data))?;
            let resp = self.recv_msg(1500)?;
            assert_eq!(resp.subtype, 0xA0, "COOKIE_RESPONSE erwartet");
            Ok(resp.remote_counter)
        }

        fn init_cookie_data(&self, tail: &[u8]) -> Vec<u8> {
            let mut data = Vec::with_capacity(14 + tail.len());
            data.extend_from_slice(&self.next_counter().to_be_bytes());
            // after_counter
            data.extend_from_slice(&[0x0B, 0x01, 0x01, 0x00, 0x01, 0x00]);
            data.extend_from_slice(&self.header.to_be_bytes());
            // after_header
            data.extend_from_slice(&[0x05, 0x82]);
            data.extend_from_slice(tail);
            data
        }

        /// `chiaki_send_recv_http_header_psn`: Request als SESSION_MESSAGE
        /// senden, Antwort-CTRL-Message erwarten; liefert (Data, remote_counter).
        fn send_recv_http(&self, request: &[u8], remote_counter: u16) -> ChiakiResult<(Vec<u8>, u16)> {
            let local = self.next_counter();
            let sub = rudp_frame(T_CTRL_MESSAGE, {
                let mut d = local.to_be_bytes().to_vec();
                d.extend_from_slice(request);
                d
            });
            let mut data = Vec::with_capacity(4);
            data.extend_from_slice(&local.to_be_bytes());
            data.extend_from_slice(&remote_counter.to_be_bytes());
            let mut msg = rudp_frame(T_SESSION_MESSAGE, data);
            msg.sub = Some(Box::new(sub));
            self.send_msg(&msg)?;

            let mut resp = self.recv_msg(1500)?;
            // assign_submessage_to_message-Schleife (erwartet CTRL-Message).
            loop {
                if (resp.subtype & 0x0F) == 0x2 || (resp.subtype & 0x0F) == 0x6 {
                    break;
                }
                match resp.sub.take() {
                    Some(s) => resp = *s,
                    None => return Err(ChiakiError::InvalidResponse),
                }
            }
            Ok((resp.data[2..].to_vec(), resp.remote_counter))
        }

        /// Header-Ende-Scan wie `send_recv_http_header_psn` (http.c).
        fn scan_header_end(data: &[u8]) -> usize {
            const TRANSITIONS_R: [usize; 4] = [1, 1, 3, 1];
            const TRANSITIONS_N: [usize; 4] = [0, 2, 0, 4];
            let mut nl_state = 0usize;
            for (i, &b) in data.iter().enumerate() {
                nl_state = match b {
                    b'\r' => TRANSITIONS_R[nl_state],
                    b'\n' => TRANSITIONS_N[nl_state],
                    _ => 0,
                };
                if nl_state == 4 {
                    return i + 1;
                }
            }
            0
        }
    }

    impl HolepunchSession for TestRudp {
        fn sock(&self, _port_type: crate::session::HolepunchPortType) -> Option<UdpSocket> {
            None
        }
        fn create_offer(&self, _port_type: crate::session::HolepunchPortType) -> ChiakiResult<()> {
            Ok(())
        }
        fn punch_hole(&self, _port_type: crate::session::HolepunchPortType) -> ChiakiResult<()> {
            Ok(())
        }
        fn ps_selected_addr(&self) -> String {
            TEST_PS_SELECTED_ADDR.to_string()
        }
        fn ps_ctrl_port(&self) -> u16 {
            TEST_PS_CTRL_PORT
        }
        fn regist_info(&self) -> ChiakiResult<crate::session::HolepunchRegistInfo> {
            Ok(crate::session::HolepunchRegistInfo::default())
        }
        fn regist(
            &self,
            _info: &crate::session::HolepunchRegistInfo,
            _target: Target,
            _psn_account_id: &[u8; crate::regist::PSN_ACCOUNT_ID_SIZE],
            _stop: &StopPipe,
        ) -> ChiakiResult<crate::regist::RegisteredHost> {
            Err(ChiakiError::Unknown)
        }
        fn rudp_start_session(&self) -> ChiakiResult<u16> {
            Err(ChiakiError::Unknown)
        }
        fn rudp_send_recv_http_header(
            &self,
            request: &[u8],
            remote_counter: u16,
            buf: &mut [u8],
        ) -> ChiakiResult<(usize, usize, u16)> {
            let (data, remote_counter) = self.send_recv_http(request, remote_counter)?;
            if data.len() > buf.len() {
                return Err(ChiakiError::BufTooSmall);
            }
            buf[..data.len()].copy_from_slice(&data);
            Ok((Self::scan_header_end(&data), data.len(), remote_counter))
        }
        fn rudp_finish(&self, _remote_counter: u16) -> ChiakiResult<()> {
            Err(ChiakiError::Unknown)
        }
        fn rudp_send_switch_to_stream_connection(&self) -> ChiakiResult<()> {
            Err(ChiakiError::Unknown)
        }
        fn rudp_ctrl_start_session(&self) -> ChiakiResult<u16> {
            self.start_session()
        }
        fn rudp_send_ctrl_message(&self, message: &[u8]) -> ChiakiResult<()> {
            let mut data = self.next_counter().to_be_bytes().to_vec();
            data.extend_from_slice(message);
            self.send_msg(&rudp_frame(T_CTRL_MESSAGE, data))
        }
        fn rudp_recv_only(&self, buf_size: usize) -> ChiakiResult<crate::session::CtrlRudpMessage> {
            Ok(to_ctrl_msg(self.recv_msg(buf_size)?))
        }
        fn rudp_ack_packet(&self, counter_to_ack: u16) -> ChiakiResult<()> {
            self.acked.lock().unwrap().push(counter_to_ack);
            Ok(())
        }
        fn rudp_send_ack_message(&self, remote_counter: u16) -> ChiakiResult<()> {
            // C: lokaler Counter wird nicht erhöht, dann remote_counter,
            // dann {0x00, 0x92}.
            let mut data = Vec::with_capacity(6);
            data.extend_from_slice(&self.local_counter().to_be_bytes());
            data.extend_from_slice(&remote_counter.to_be_bytes());
            data.extend_from_slice(&[0x00, 0x92]);
            self.send_msg(&rudp_frame(T_ACK, data))
        }
        fn rudp_print_message(&self, _message: &crate::session::CtrlRudpMessage) {}
    }

    /// "Konsole": RUDP-Gegenseite des Loopbacks (INIT/COOKIE, HTTP über die
    /// SESSION_MESSAGE, Ctrl-Messages mit RPCrypt-Ver-/Entschlüsselung).
    struct ConsolePeer {
        sock: UdpSocket,
        counter: u16,
        rpcrypt: Rpcrypt,
        last_counter: u16,
    }

    impl ConsolePeer {
        fn new(sock: UdpSocket, rpcrypt: Rpcrypt) -> Self {
            sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            ConsolePeer {
                sock,
                counter: 0x1000,
                rpcrypt,
                last_counter: 0,
            }
        }

        /// 2-Byte-Counter-Präfix für Message-Data (zählt hoch).
        fn counter_prefix(&mut self) -> Vec<u8> {
            self.last_counter = self.counter;
            self.counter = self.counter.wrapping_add(1);
            self.last_counter.to_be_bytes().to_vec()
        }

        fn send(&self, msg: &TMsg) {
            self.sock.send(&t_serialize(msg)).unwrap();
        }

        fn recv(&self) -> TMsg {
            let mut buf = [0u8; 1500];
            let n = self.sock.recv(&mut buf).unwrap();
            t_parse(&buf[..n])
        }

        /// Empfängt eine RUDP-CTRL-Message und liefert den Ctrl-Frame
        /// (8-Byte-Header + Payload, ohne die 2 Counter-Bytes).
        fn recv_ctrl_frame(&self) -> Vec<u8> {
            let msg = self.recv();
            assert_eq!(msg.subtype, 0x02, "RUDP-CTRL-Message erwartet");
            msg.data[2..].to_vec()
        }

        /// Sendet einen Ctrl-Frame als RUDP-CTRL-Message.
        fn send_ctrl_frame(&mut self, frame: &[u8]) {
            let mut data = self.counter_prefix();
            data.extend_from_slice(frame);
            self.send(&rudp_frame(T_CTRL_MESSAGE, data));
        }

        /// Empfängt einen Ctrl-Frame und prüft Type + (mit `crypt_counter`
        /// entschlüsselten) Payload — `None` = leerer Payload.
        fn expect_ctrl(&self, msg_type: u16, crypt_counter: Option<u64>, plain: &[u8]) {
            let frame = self.recv_ctrl_frame();
            let size = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
            let rtype = u16::from_be_bytes([frame[4], frame[5]]);
            assert_eq!(rtype, msg_type, "Ctrl-Message-Type");
            assert_eq!(size, plain.len(), "Payload-Größe von Type {msg_type:#x}");
            match crypt_counter {
                Some(c) => {
                    let mut buf = frame[8..].to_vec();
                    self.rpcrypt.decrypt(c, &mut buf).unwrap();
                    assert_eq!(&buf, plain, "Payload von Type {msg_type:#x}");
                }
                None => assert!(frame[8..].is_empty()),
            }
        }

        /// Empfängt eine ACK-Message und prüft data[2..4] == expected_remote.
        fn expect_ack(&self, expected_remote: u16) {
            let ack = self.recv();
            assert_eq!(ack.type_, T_ACK, "ACK-Message erwartet");
            assert_eq!(
                &ack.data[2..4],
                &expected_remote.to_be_bytes(),
                "ACK-Remote-Counter"
            );
        }
    }

    /// Konsolen-Vorspann: INIT/COOKIE-Handshake + HTTP-Request prüfen +
    /// Antwort (mit optionalem Leftover-Frame hinter dem Header) + ACK
    /// erwarten (ctrl.c:1165-1187, 1329-1398).
    fn console_handshake_head(peer: &mut ConsolePeer, response: &str, leftover: &[u8]) {
        // (1) INIT_REQUEST -> INIT_RESPONSE (data[8..] = "Cookie").
        let init = peer.recv();
        assert_eq!(init.subtype, 0x80, "INIT_REQUEST erwartet");
        let cookie = b"COOKIE123";
        let mut resp_data = peer.counter_prefix();
        resp_data.extend_from_slice(&[0u8; 6]);
        resp_data.extend_from_slice(cookie);
        peer.send(&rudp_frame(T_INIT_RESPONSE, resp_data));

        // (2) COOKIE_REQUEST (trägt die Cookie-Antwort in data[14..]).
        let cookie_req = peer.recv();
        assert_eq!(cookie_req.subtype, 0x90, "COOKIE_REQUEST erwartet");
        assert_eq!(
            &cookie_req.data[14..],
            cookie,
            "init_response muss im Cookie-Request wiederkehren"
        );
        let resp_data = peer.counter_prefix();
        peer.send(&rudp_frame(T_COOKIE_RESPONSE, resp_data));

        // (3) HTTP-Request über die SESSION_MESSAGE prüfen (Port aus
        //     ps_ctrl_port, ctrl.c:1299).
        let sess = peer.recv();
        assert_eq!(sess.type_, T_SESSION_MESSAGE, "SESSION_MESSAGE erwartet");
        let sub = sess.sub.expect("Sub-Message (CTRL)");
        assert_eq!(sub.type_, T_CTRL_MESSAGE);
        let request = String::from_utf8(sub.data[2..].to_vec()).unwrap();
        assert!(
            request.starts_with("GET /sie/ps4/rp/sess/ctrl HTTP/1.1\r\n"),
            "request: {request}"
        );
        assert!(
            request.contains(&format!(
                "Host: {TEST_PS_SELECTED_ADDR}:{TEST_PS_CTRL_PORT}\r\n"
            )),
            "ps_selected_addr:ps_ctrl_port als Host erwartet: {request}"
        );
        assert!(request.contains("User-Agent: remoteplay Windows\r\n"));
        assert!(request.contains("Connection: keep-alive\r\n"));
        assert!(request.contains("Content-Length: 0\r\n"));
        assert!(request.contains("RP-Auth: "));
        assert!(request.contains("RP-Version: 10.0\r\n"));
        assert!(request.contains("RP-Did: "));
        assert!(request.contains("RP-ControllerType: 3\r\n"));
        assert!(request.contains("RP-ClientType: 11\r\n"));
        assert!(request.contains("RP-OSType: "));
        assert!(request.contains("RP-ConPath: 1\r\n"));
        // target >= PS4_10 -> StartBitrate, kein StreamingType (kein PS5).
        assert!(request.contains("RP-StartBitrate: "));
        assert!(!request.contains("RP-StreamingType: "));

        // (4) Antwort: HTTP-Header (+ Leftover — prüft den
        //     "mehr Data als Header"-Pfad, ctrl.c:1469-1472).
        let mut resp_bytes = response.as_bytes().to_vec();
        resp_bytes.extend_from_slice(leftover);
        let mut data = peer.counter_prefix();
        let c_http_resp = peer.last_counter;
        data.extend_from_slice(&resp_bytes);
        peer.send(&rudp_frame(T_CTRL_MESSAGE, data));

        // (5) ACK auf die HTTP-Antwort (ctrl.c:1389-1398): remote_counter
        //     = Counter der Antwort + 1.
        peer.expect_ack(c_http_resp.wrapping_add(1));
    }

    /// RUDP-Loopback über den vollen Flow: Handshake, HTTP (mit RP-Server-
    /// Type), Session-Id als HTTP-Leftover, Heartbeat, 0x12-Send-Buffer-ACK
    /// mit Ctrl-Frame hinter Offset 8, Enable-Features, Session-Kommandos,
    /// Switch-to-Stream-Connection.
    #[test]
    fn ctrl_rudp_loopback_full_flow() {
        let nonce = k16("ae92e764882651ef89018cfa696c6938");
        let morning = k16("74a59c9693c2083ba6a84ba050fa8e5a");

        let (sock_a, sock_b) = rudp_loopback_pair();
        let sock_b_addr = sock_b.local_addr().unwrap();

        // Session-Id-Frame für den HTTP-Leftover (remote counter 1 —
        // counter 0 geht an RP-Server-Type).
        let rpcrypt_for_server = Rpcrypt::new_auth(Target::Ps4_10, &nonce, &morning).unwrap();
        let session_id = b"chiakiSessionIdTest00000000abc";
        let mut sid_plain = vec![session_id.len() as u8];
        sid_plain.extend_from_slice(session_id);
        let sid_enc = rpcrypt_for_server.encrypt_buf(1, &sid_plain).unwrap();
        let mut sid_frame = build_header(sid_enc.len(), CtrlMessageType::SessionId as u16).to_vec();
        sid_frame.extend_from_slice(&sid_enc);

        let mut server_type_plain = [0u8; 16];
        server_type_plain[0] = 2;
        let server_type_enc = rpcrypt_for_server.encrypt_buf(0, &server_type_plain).unwrap();
        let response =
            format!("HTTP/1.1 200 OK\r\nRP-Server-Type: {}\r\n\r\n", base64::encode(&server_type_enc));

        let console = std::thread::spawn(move || {
            let mut peer = ConsolePeer::new(sock_b, rpcrypt_for_server);
            console_handshake_head(&mut peer, &response, &sid_frame);

            // Enable-Features nach der Session-Id (lokale Counter 4-6:
            // 0-3 gehen an auth/did/ostype/bitrate).
            peer.expect_ctrl(CtrlMessageType::MicToggle as u16, Some(4), &[0, 1, 1, 89]);
            peer.expect_ctrl(CtrlMessageType::MicToggle as u16, Some(5), &[0, 1, 1, 89]);
            peer.expect_ctrl(CtrlMessageType::DisplayDevices as u16, Some(6), &[0, 0, 0, 0]);

            // Session-Kommandos (Queue des Ctrl-Threads).
            peer.expect_ctrl(CtrlMessageType::GotoBed as u16, None, &[]);
            peer.expect_ctrl(
                CtrlMessageType::GoHome as u16,
                Some(7),
                &[0x00, 0xff, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            );
            peer.expect_ctrl(CtrlMessageType::MicToggle as u16, Some(8), &[0, 1, 0, 89]);
            peer.expect_ctrl(CtrlMessageType::MicConnect as u16, Some(9), &[0, 0]);
            peer.expect_ctrl(CtrlMessageType::KeyboardCloseReq as u16, Some(10), &[0, 0, 0, 0]);

            // Heartbeat: REQ schicken — der 0x02-Handler des Clients bestätigt
            // die Message per ACK (ctrl.c:544-545), dann die Golden-REP
            // erwarten.
            peer.send_ctrl_frame(&build_header(0, CtrlMessageType::HeartbeatReq as u16));
            let c_heartbeat = peer.last_counter;
            peer.expect_ack(c_heartbeat.wrapping_add(1));
            let frame = peer.recv_ctrl_frame();
            assert_eq!(
                frame,
                build_header(0, CtrlMessageType::HeartbeatRep as u16),
                "Heartbeat-Rep-Frame"
            );

            // 0x12-Message: Send-Buffer-ACK (0x4321) + Ctrl-Frame hinter
            // Offset 8 (Type 0x99, leer — löst keinen Event aus). Der
            // Handler antwortet mit einer ACK-Message (ctrl.c:539-557).
            const OFFSET8_ACK: u16 = 0x4321;
            let mut data = peer.counter_prefix();
            let c12 = peer.last_counter;
            data.extend_from_slice(&OFFSET8_ACK.to_be_bytes());
            data.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // Füller bis offset 8
            data.extend_from_slice(&build_header(0, 0x99));
            peer.send(&rudp_frame(T_OFFSET8, data));
            peer.expect_ack(c12.wrapping_add(1));

            // Switch-to-Stream-Connection-ACK.
            peer.send_ctrl_frame(&build_header(0, CtrlMessageType::SwitchToStreamConnection as u16));

            // Socket offen halten, bis der Test gestoppt hat (länger als der
            // Empfangs-Poll des Ctrl-Loops, damit der Stop sauber greift).
            std::thread::sleep(Duration::from_millis(400));
        });

        // --- Ctrl mit Holepunch-Transport aufsetzen ---
        let test_rudp = Arc::new(TestRudp::wrap(sock_a, sock_b_addr).unwrap());
        let (event_tx, event_rx) = mpsc::channel();
        let (msg_queue_tx, msg_queue_rx) = mpsc::channel();
        let mut ctrl = Ctrl::new(CtrlInit {
            rpcrypt: Rpcrypt::new_auth(Target::Ps4_10, &nonce, &morning).unwrap(),
            target: Target::Ps4_10,
            regist_key: *b"regist_key_regs\0",
            did: [0x42; RP_DID_SIZE],
            hostname: TEST_PS_SELECTED_ADDR.to_string(),
            // Im RUDP-Pfad ungenutzt (C: Port/Adresse aus der Holepunch-Session).
            host_addr: "127.0.0.1:0".parse().unwrap(),
            sock: None,
            transport: CtrlTransport::Holepunch(test_rudp.clone()),
            codec: Codec::H264,
            enable_dualsense: false,
            enable_keyboard: false,
            msg_queue_tx,
            msg_queue_rx,
            event_cb: Arc::new(move |ev| {
                let _ = event_tx.send(ev);
            }),
        })
        .unwrap();
        ctrl.start().unwrap();

        // ServerType aus dem HTTP-Handshake (remote counter 0).
        match event_rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            CtrlEvent::ServerType { server_type } => assert_eq!(server_type, 2),
            other => panic!("expected ServerType, got {other:?}"),
        }
        // Session-Id aus dem HTTP-Leftover-Frame.
        match event_rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            CtrlEvent::SessionId(id) => {
                assert_eq!(id, "chiakiSessionIdTest00000000abc");
                assert!(ctrl.session_id_received());
            }
            other => panic!("expected SessionId, got {other:?}"),
        }

        // Session-Kommandos (die Konsole prüft die Frames oben).
        ctrl.goto_bed().unwrap();
        ctrl.go_home().unwrap();
        ctrl.toggle_microphone(true).unwrap();
        ctrl.connect_microphone().unwrap();
        ctrl.keyboard_accept().unwrap();

        // Switch-to-Stream-Connection-ACK.
        match event_rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            CtrlEvent::SwitchToStreamConnection => {}
            other => panic!("expected SwitchToStreamConnection, got {other:?}"),
        }

        // Send-Buffer-ACK aus der 0x12-Message ist bei der Holepunch-Session
        // angekommen (ctrl.c:543).
        assert_eq!(test_rudp.acked.lock().unwrap().as_slice(), &[0x4321][..]);

        // Sauber herunterfahren — danach darf kein Quit kommen.
        ctrl.stop();
        ctrl.join().unwrap();
        console.join().unwrap();
        match event_rx.recv_timeout(Duration::from_millis(200)) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {}
            other => panic!("unexpected event after clean shutdown: {other:?}"),
        }
    }

    /// Vermaschtes UDP-Loopback-Paar (Muster `loopback_pair` aus den
    /// regist_psn.rs-Tests): beide Sockets sind aufeinander verbunden.
    fn rudp_loopback_pair() -> (UdpSocket, UdpSocket) {
        let a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").unwrap();
        a.connect(b.local_addr().unwrap()).unwrap();
        b.connect(a.local_addr().unwrap()).unwrap();
        (a, b)
    }

    /// RUDP-Finish-Message (Subtype 0xC0, ctrl.c:562-565): ctrl_failed, aber
    /// der Ctrl-Loop läuft weiter bis zur Stop-Anforderung.
    #[test]
    fn ctrl_rudp_finish_message_reports_quit() {
        run_rudp_error_case(|peer| {
            // FINISH-Message nach dem Handshake — Data >= 4 Bytes (sonst
            // greift der "too small"-Zweig vor dem Subtype-Switch).
            let mut data = peer.counter_prefix();
            data.extend_from_slice(&[0x00, 0x00]);
            peer.send(&rudp_frame(T_FINISH, data));
        });
    }

    /// Zu kleine RUDP-Response (data < 4 Bytes, ctrl.c:527-533): ctrl_failed.
    #[test]
    fn ctrl_rudp_too_small_message_reports_quit() {
        run_rudp_error_case(|peer| {
            let mut data = peer.counter_prefix();
            data.push(0x00); // nur 3 Bytes Data < 4
            peer.send(&rudp_frame(T_CTRL_MESSAGE, data));
        });
    }

    /// Gemeinsamer Fahrer der Fehlerpfad-Tests: Handshake/HTTP gegen die
    /// Konsole (ohne RP-Server-Type), dann die jeweilige Fehler-Message
    /// senden und auf Quit(Unknown) warten.
    fn run_rudp_error_case(send_error: impl FnOnce(&mut ConsolePeer) + Send + 'static) {
        let nonce = k16("ae92e764882651ef89018cfa696c6938");
        let morning = k16("74a59c9693c2083ba6a84ba050fa8e5a");
        let (sock_a, sock_b) = rudp_loopback_pair();
        let sock_b_addr = sock_b.local_addr().unwrap();
        let rpcrypt_for_server = Rpcrypt::new_auth(Target::Ps4_10, &nonce, &morning).unwrap();

        let console = std::thread::spawn(move || {
            let mut peer = ConsolePeer::new(sock_b, rpcrypt_for_server);
            console_handshake_head(&mut peer, "HTTP/1.1 200 OK\r\n\r\n", &[]);
            send_error(&mut peer);
            std::thread::sleep(Duration::from_millis(400));
        });

        let test_rudp = Arc::new(TestRudp::wrap(sock_a, sock_b_addr).unwrap());
        let (event_tx, event_rx) = mpsc::channel();
        let (msg_queue_tx, msg_queue_rx) = mpsc::channel();
        let mut ctrl = Ctrl::new(CtrlInit {
            rpcrypt: Rpcrypt::new_auth(Target::Ps4_10, &nonce, &morning).unwrap(),
            target: Target::Ps4_10,
            regist_key: [0u8; RPCRYPT_KEY_SIZE],
            did: [0u8; RP_DID_SIZE],
            hostname: TEST_PS_SELECTED_ADDR.to_string(),
            host_addr: "127.0.0.1:0".parse().unwrap(),
            sock: None,
            transport: CtrlTransport::Holepunch(test_rudp),
            codec: Codec::H264,
            enable_dualsense: false,
            enable_keyboard: false,
            msg_queue_tx,
            msg_queue_rx,
            event_cb: Arc::new(move |ev| {
                let _ = event_tx.send(ev);
            }),
        })
        .unwrap();
        ctrl.start().unwrap();

        // Einziger Event: Quit(Unknown) aus dem RUDP-Fehlerpfad.
        let mut last = None;
        while let Ok(ev) = event_rx.recv_timeout(Duration::from_secs(5)) {
            last = Some(ev);
            if matches!(last, Some(CtrlEvent::Quit(CtrlQuitReason::Unknown))) {
                break;
            }
        }
        assert!(
            matches!(last, Some(CtrlEvent::Quit(CtrlQuitReason::Unknown))),
            "expected Quit(Unknown), got {last:?}"
        );

        // Der Loop läuft nach ctrl_failed weiter (wie im C) — Stop beendet ihn.
        ctrl.stop();
        ctrl.join().unwrap();
        console.join().unwrap();
    }

    /// `rudp_packet_type_data_offset()` (ctrl.c:103-114).
    #[test]
    fn rudp_packet_type_data_offset_golden() {
        assert_eq!(rudp_packet_type_data_offset(0x12), 8);
        assert_eq!(rudp_packet_type_data_offset(0x26), 6);
        assert_eq!(rudp_packet_type_data_offset(0x02), 2);
        assert_eq!(rudp_packet_type_data_offset(0x36), 2);
        assert_eq!(rudp_packet_type_data_offset(0xC0), 2);
        assert_eq!(rudp_packet_type_data_offset(0x00), 2);
    }
}

