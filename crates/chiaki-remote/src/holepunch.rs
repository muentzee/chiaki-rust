// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/remote/holepunch.c + lib/include/chiaki/remote/holepunch.h
// (chiaki-ng).
//
// "Remote Play over Internet" uses a custom UDP-based protocol for
// communication between the console and the client (see `rudp` for details on
// that). The protocol is designed to work even if both the console and the
// client are behind NATs, by using UDP hole punching via an intermediate
// server. The end result of the hole punching process is a pair of sockets,
// one for control messages (using the custom protocol wrapper) and one for
// data messages (using the same protocol as a local connection).
//
// Funktionsreihenfolge (aus holepunch.h):
// 1.  list_devices()           Geräte des PSN-Accounts listen
// 2.  HolepunchSession::new()  Session mit gültigem OAuth2-Token initialisieren
// 3.  create()                 Remote-Play-Session auf dem PSN-Server anlegen
// 4.  create_offer()           Angebot (Netzinfo Control-Socket) erzeugen
// 5.  start()                  Session für ein Gerät starten
// 6.  punch_hole(Ctrl)         Control-Socket vorbereiten
// 7.  create_offer()           Angebot für den Data-Socket erzeugen
// 8.  punch_hole(Data)         Data-Socket vorbereiten
// 9.  fini()                   nach Ende des Streaming
//
// Abweichungen gegenüber C (Speicherverwaltung/Threading, Semantik identisch):
// - libcurl → ureq (siehe `psn`), json-c → serde_json, miniupnpc →
//   eingebauter Mini-UPnP-IGD-Client (Modul `upnp`), libcurl-WebSocket →
//   eingebaunter RFC-6455-Client über rustls (Modul `ws`), libevent-poll →
//   nonblocking-Poll-Loop (std).
// - Die Notification-Queue ist eine VecDeque unter Mutex+Condvar;
//   `wait_for_notification` entnimmt die gefundene Notification direkt
//   (das C-Pointer/`clear_notification`-Muster wird dadurch ersetzt).
// - `get_client_addr_local` (Windows: GetAdaptersInfo, Ethernet bevorzugt)
//   wird über den UDP-Default-Route-Trick (connect + getsockname) gelöst —
//   kein unsafe/iphlpapi.
// - `notif_pipe` (im C angelegt, aber nie benutzt) entfällt.
// - random_uuidv4 nutzt den ThreadRNG statt srand(time(NULL))+rand().
// - Cancels brechen Warteschleifen sofort ab (im C wird teilweise erst nach
//   dem Wake-up geprüft).
// - send_offer(): Der C-Code ignoriert den Rückgabewert — wird 1:1 so
//   übernommen (der nachfolgende ACK-Wait läuft dann in den Timeout).

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use chiaki_core::base64;
use chiaki_core::error::{ChiakiError, ChiakiResult};
use chiaki_core::random;
use chiaki_core::sock;
use chiaki_core::stoppipe::StopPipe;

use crate::psn::{self, PsnClient};
use crate::stun::{self, StunServer};

mod holepunch_device {
    // Öffentliche Typen nahe an holepunch.h.

    /// Port von `ChiakiHolepunchRegistInfo` — Info für Remote Registration.
    #[derive(Debug, Clone, Default)]
    pub struct RegistInfo {
        pub data1: [u8; 16],
        pub data2: [u8; 16],
        pub custom_data1: [u8; 16],
        pub regist_local_ip: String,
    }

    /// Port von `chiaki_holepunch_device_info_t` — Info über ein für Remote
    /// Play nutzbares Gerät.
    #[derive(Debug, Clone)]
    pub struct DeviceInfo {
        pub type_: super::ConsoleType,
        pub device_name: String,
        pub device_uid: [u8; 32],
        pub remoteplay_enabled: bool,
    }
}

pub use holepunch_device::{DeviceInfo, RegistInfo};

/// `DUID_PREFIX` (holepunch.h)
pub const DUID_PREFIX: &str = "0000000700410080";
/// `CHIAKI_DUID_STR_SIZE` (16 Präfix-Zeichen + 32 Hex-Zeichen + NUL)
pub const CHIAKI_DUID_STR_SIZE: usize = 49;

/// Port von `chiaki_holepunch_console_type_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ConsoleType {
    Ps4 = 0,
    Ps5 = 1,
}

/// Port von `chiaki_holepunch_port_type_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PortType {
    Ctrl = 0,
    Data = 1,
}

// Konstanten (holepunch.c)
const UUIDV4_STR_LEN: usize = 37;
const WEBSOCKET_PING_INTERVAL_SEC: u64 = 5;
// Maximum WebSocket frame size currently supported by libcurl
const WEBSOCKET_MAX_FRAME_SIZE: usize = 64 * 1024;
const SESSION_CREATION_TIMEOUT_SEC: u64 = 30;
const SESSION_START_TIMEOUT_SEC: u64 = 30;
const SESSION_DELETION_TIMEOUT_SEC: u64 = 3;
const SELECT_CANDIDATE_TIMEOUT_SEC: f32 = 0.5;
const SELECT_CANDIDATE_TRIES: u32 = 20;
const SELECT_CANDIDATE_CONNECTION_SEC: u64 = 5;
const RANDOM_ALLOCATION_GUESSES_NUMBER: i32 = 75;
const RANDOM_ALLOCATION_SOCKS_NUMBER: i32 = 250;
const CHECK_CANDIDATES_REQUEST_NUMBER: usize = 1;
const WAIT_RESPONSE_TIMEOUT_SEC: u64 = 1;
const MSG_TYPE_REQ: u32 = 0x06000000;
const MSG_TYPE_RESP: u32 = 0x07000000;
const EXTRA_CANDIDATE_ADDRESSES: usize = 3;
const ENABLE_IPV6: bool = false;
const UPNP_DISCOVER_TIMEOUT_MS: u64 = 7000;
const CUSTOMDATA1_EXTRA_BYTES_MAX: usize = 4;

// Port von `NotificationType` (Bitmaske).
pub const NOTIFICATION_TYPE_UNKNOWN: u16 = 0;
/// psn:sessionManager:sys:remotePlaySession:created
pub const NOTIFICATION_TYPE_SESSION_CREATED: u16 = 1 << 0;
/// psn:sessionManager:sys:rps:members:created
pub const NOTIFICATION_TYPE_MEMBER_CREATED: u16 = 1 << 1;
/// psn:sessionManager:sys:rps:members:deleted
pub const NOTIFICATION_TYPE_MEMBER_DELETED: u16 = 1 << 2;
/// psn:sessionManager:sys:rps:customData1:updated
pub const NOTIFICATION_TYPE_CUSTOM_DATA1_UPDATED: u16 = 1 << 3;
/// psn:sessionManager:sys:rps:sessionMessage:created
pub const NOTIFICATION_TYPE_SESSION_MESSAGE_CREATED: u16 = 1 << 4;
/// psn:sessionManager:sys:remotePlaySession:deleted
pub const NOTIFICATION_TYPE_SESSION_DELETED: u16 = 1 << 5;

// Port von `SessionState` (Bitmaske).
pub const SESSION_STATE_INIT: u32 = 1 << 0;
pub const SESSION_STATE_WS_OPEN: u32 = 1 << 1;
pub const SESSION_STATE_CREATED: u32 = 1 << 2;
pub const SESSION_STATE_STARTED: u32 = 1 << 3;
pub const SESSION_STATE_CLIENT_JOINED: u32 = 1 << 4;
pub const SESSION_STATE_DATA_SENT: u32 = 1 << 5;
pub const SESSION_STATE_CONSOLE_JOINED: u32 = 1 << 6;
pub const SESSION_STATE_CUSTOMDATA1_RECEIVED: u32 = 1 << 7;
pub const SESSION_STATE_CTRL_OFFER_RECEIVED: u32 = 1 << 8;
pub const SESSION_STATE_CTRL_OFFER_SENT: u32 = 1 << 9;
pub const SESSION_STATE_CTRL_CONSOLE_ACCEPTED: u32 = 1 << 10;
pub const SESSION_STATE_CTRL_CLIENT_ACCEPTED: u32 = 1 << 11;
pub const SESSION_STATE_CTRL_ESTABLISHED: u32 = 1 << 12;
pub const SESSION_STATE_DATA_OFFER_RECEIVED: u32 = 1 << 13;
pub const SESSION_STATE_DATA_OFFER_SENT: u32 = 1 << 14;
pub const SESSION_STATE_DATA_CONSOLE_ACCEPTED: u32 = 1 << 15;
pub const SESSION_STATE_DATA_CLIENT_ACCEPTED: u32 = 1 << 16;
pub const SESSION_STATE_DATA_ESTABLISHED: u32 = 1 << 17;
pub const SESSION_STATE_DELETED: u32 = 1 << 18;

// Port von `SessionMessageAction` (Bitmaske, Werte wie im C).
pub const SESSION_MESSAGE_ACTION_UNKNOWN: u8 = 0;
pub const SESSION_MESSAGE_ACTION_OFFER: u8 = 1;
pub const SESSION_MESSAGE_ACTION_RESULT: u8 = 1 << 2;
pub const SESSION_MESSAGE_ACTION_ACCEPT: u8 = 1 << 3;
pub const SESSION_MESSAGE_ACTION_TERMINATE: u8 = 1 << 4;

/// Port von `candidate_type_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum CandidateType {
    #[default]
    Static = 0,
    Local = 1,
    Stun = 2,
    Derived = 3,
}

impl CandidateType {
    fn as_str(self) -> &'static str {
        match self {
            CandidateType::Static => "STATIC",
            CandidateType::Local => "LOCAL",
            CandidateType::Stun => "STUN",
            CandidateType::Derived => "DERIVED",
        }
    }

    fn from_str(s: &str) -> CandidateType {
        match s {
            "LOCAL" => CandidateType::Local,
            "STUN" => CandidateType::Stun,
            "DERIVED" => CandidateType::Derived,
            _ => CandidateType::Static,
        }
    }
}

/// Port von `candidate_t`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Candidate {
    pub type_: CandidateType,
    pub addr: String,
    pub addr_mapped: String,
    pub port: u16,
    pub port_mapped: u16,
}

/// Port von `connection_request_t`.
#[derive(Debug, Clone, Default)]
pub struct ConnectionRequest {
    pub sid: u32,
    pub peer_sid: u32,
    pub skey: [u8; 16],
    pub nat_type: u8,
    pub candidates: Vec<Candidate>,
    pub default_route_mac_addr: [u8; 6],
    pub local_hashed_id: [u8; 20],
}

/// Port von `session_message_t`.
#[derive(Debug, Clone, Default)]
pub struct SessionMessage {
    pub action: u8,
    pub req_id: u16,
    pub error: u16,
    pub conn_request: Option<ConnectionRequest>,
}

/// Port von `notification_t` (JSON im Besitz der Struktur).
#[derive(Debug, Clone)]
pub(crate) struct Notification {
    pub type_: u16,
    pub json: serde_json::Value,
}

#[derive(Debug, Default)]
struct StopFlags {
    main_should_stop: bool,
    ws_thread_should_stop: bool,
}

#[derive(Default)]
struct NotifQueue {
    queue: VecDeque<Notification>,
}

/// WebSocket-Thread-Zustand.
#[derive(Default)]
struct WsState {
    fqdn: Option<String>,
    open: bool,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Port von `UPNPGatewayStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum GatewayStatus {
    #[default]
    Unknown = 1,
    Found = 2,
    NotFound = 4,
}

#[derive(Default)]
struct UpnpState {
    gw: Option<upnp::GatewayInfo>,
    status: GatewayStatus,
    thread_running: bool,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Daten, die der PS-Handshake (check_candidates / send_response) braucht —
/// aus [`MainState`] geklont, damit keine Mutex über Wartezeiten gehalten wird.
#[derive(Debug, Clone)]
struct PsHandshakeCtx {
    hashed_id_local: [u8; 20],
    hashed_id_console: [u8; 20],
    sid_local: u16,
    sid_console: u16,
}

impl PsHandshakeCtx {
    fn from_main(main: &MainState) -> PsHandshakeCtx {
        PsHandshakeCtx {
            hashed_id_local: main.hashed_id_local,
            hashed_id_console: main.hashed_id_console,
            sid_local: main.sid_local,
            sid_console: main.sid_console,
        }
    }
}

/// Hauptzustand (im C direkt im `Session`-Struct, nur vom Main-Thread
/// verändert — hier gebündelt hinter einem Mutex).
struct MainState {
    console_uid: [u8; 32],
    console_type: ConsoleType,
    ipv4_sock: Option<UdpSocket>,
    ipv6_sock: Option<UdpSocket>,
    ctrl_sock: Option<UdpSocket>,
    data_sock: Option<UdpSocket>,
    local_candidates: Vec<Candidate>,
    our_offer_msg: Option<SessionMessage>,
    account_id: i64,
    online_id: Option<String>,
    session_id: String, // 36 Zeichen UUIDv4, initial leer
    pushctx_id: String, // 36 Zeichen UUIDv4
    sid_local: u16,
    sid_console: u16,
    hashed_id_local: [u8; 20],
    hashed_id_console: [u8; 20],
    local_req_id: usize,
    #[allow(dead_code)]
    local_mac_addr: [u8; 6], // im C ungenutzt
    local_port_ctrl: u16,
    local_port_data: u16,
    stun_allocation_increment: i32,
    stun_random_allocation: bool,
    force_port_guessing: bool,
    port_guessing_count: i32,
    port_guessing_socks: i32,
    stun_server_list: Vec<StunServer>,
    stun_server_list_ipv6: Vec<StunServer>,
    data1: [u8; 16],
    data2: [u8; 16],
    custom_data1: [u8; 16],
    ps_ip: String,
    ctrl_port: u16,
    client_local_ip: String,
}

impl MainState {
    fn new() -> Self {
        // chiaki_random_32() als u16 (C-Konvertierung)
        let sid_local = random::random_32() as u16;
        let mut hashed_id_local = [0u8; 20];
        let mut data1 = [0u8; 16];
        let mut data2 = [0u8; 16];
        let _ = random::random_bytes_crypt(&mut hashed_id_local);
        let _ = random::random_bytes_crypt(&mut data1);
        let _ = random::random_bytes_crypt(&mut data2);
        MainState {
            console_uid: [0; 32],
            console_type: ConsoleType::Ps5,
            ipv4_sock: None,
            ipv6_sock: None,
            ctrl_sock: None,
            data_sock: None,
            local_candidates: Vec::new(),
            our_offer_msg: None,
            account_id: 0,
            online_id: None,
            session_id: String::new(),
            pushctx_id: random_uuidv4(),
            sid_local,
            sid_console: 0,
            hashed_id_local,
            hashed_id_console: [0; 20],
            local_req_id: 1,
            local_mac_addr: [0; 6],
            local_port_ctrl: 0,
            local_port_data: 0,
            // C: stun_allocation_increment = -1 → "STUN-Test noch nicht gelaufen"
            stun_allocation_increment: -1,
            stun_random_allocation: false,
            force_port_guessing: false,
            port_guessing_count: RANDOM_ALLOCATION_GUESSES_NUMBER,
            port_guessing_socks: RANDOM_ALLOCATION_SOCKS_NUMBER,
            stun_server_list: Vec::new(),
            stun_server_list_ipv6: Vec::new(),
            data1,
            data2,
            custom_data1: [0; 16],
            ps_ip: String::new(),
            ctrl_port: 0,
            client_local_ip: String::new(),
        }
    }

    #[cfg(test)]
    fn new_for_test() -> Self {
        Self::new()
    }
}

struct Inner {
    psn: PsnClient,
    state: Mutex<u32>,
    state_cond: Condvar,
    stop: Mutex<StopFlags>,
    notif: Mutex<NotifQueue>,
    notif_cond: Condvar,
    select_pipe: StopPipe,
    main: Mutex<MainState>,
    ws: Mutex<WsState>,
    upnp: Mutex<UpnpState>,
}

/// Handle zu einer Holepunch-Session (`ChiakiHolepunchSession`).
///
/// Klonen teilt dieselbe Session (C: Pointer). Für `ConnectInfo`:
/// `Option<HolepunchSession>` (None = direkte Verbindung).
#[derive(Clone)]
pub struct HolepunchSession {
    inner: Arc<Inner>,
}

impl HolepunchSession {
    /// Port von `chiaki_holepunch_session_init()`.
    ///
    /// **IMPORTANT**: The OAuth2 token must fulfill the following requirements:
    /// - It must be a valid PSN OAuth2 token, ideally refreshed before calling
    ///   this function.
    /// - It must be authorized for the scopes `psn:clientapp`,
    ///   `referenceDataService:countryConfig.read`,
    ///   `pushNotification:webSocket.desktop.connect`,
    ///   `sessionManager:remotePlaySession.system.update`.
    /// - It must have been initially created with a `duid` parameter set to a
    ///   unique identifier for the client device (see
    ///   [`generate_client_device_uid`](Self::generate_client_device_uid)).
    pub fn new(psn_oauth2_token: &str) -> ChiakiResult<HolepunchSession> {
        Ok(HolepunchSession {
            inner: Arc::new(Inner {
                psn: PsnClient::new(psn_oauth2_token),
                state: Mutex::new(SESSION_STATE_INIT),
                state_cond: Condvar::new(),
                stop: Mutex::new(StopFlags::default()),
                notif: Mutex::new(NotifQueue::default()),
                notif_cond: Condvar::new(),
                select_pipe: StopPipe::new(),
                main: Mutex::new(MainState::new()),
                ws: Mutex::new(WsState::default()),
                upnp: Mutex::new(UpnpState::default()),
            }),
        })
    }

    fn state_lock(&self) -> std::sync::MutexGuard<'_, u32> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn main_lock(&self) -> std::sync::MutexGuard<'_, MainState> {
        self.inner.main.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn stop_lock(&self) -> std::sync::MutexGuard<'_, StopFlags> {
        self.inner.stop.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn notif_lock(&self) -> std::sync::MutexGuard<'_, NotifQueue> {
        self.inner.notif.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// C-Muster: main_should_stop prüfen und dabei zurücksetzen.
    fn check_cancel(&self) -> bool {
        let mut flags = self.stop_lock();
        if flags.main_should_stop {
            flags.main_should_stop = false;
            return true;
        }
        false
    }

    // -------------------------------------------------------------------
    // Statische/freie Funktionen aus holepunch.h
    // -------------------------------------------------------------------

    /// Port von `chiaki_holepunch_list_devices()`.
    ///
    /// List devices associated with a PSN account that can be used for remote
    /// play. (Nur PS5, wie im C.)
    pub fn list_devices(
        psn_oauth2_token: &str,
        console_type: ConsoleType,
    ) -> ChiakiResult<Vec<DeviceInfo>> {
        PsnClient::new(psn_oauth2_token).list_devices(console_type)
    }

    /// Port von `chiaki_holepunch_generate_client_device_uid()`.
    ///
    /// Generate a unique device identifier for the client.
    pub fn generate_client_device_uid() -> ChiakiResult<String> {
        let mut random_bytes = [0u8; 16];
        random::random_bytes_crypt(&mut random_bytes)?;
        Ok(format!("{}{}", DUID_PREFIX, psn::bytes_to_hex(&random_bytes)))
    }

    /// Port von `chiaki_get_regist_info()`.
    ///
    /// This function should be called after the first `punch_hole(Ctrl)`
    /// punching the control hole used for regist.
    pub fn regist_info(&self) -> RegistInfo {
        let main = self.main_lock();
        RegistInfo {
            data1: main.data1,
            data2: main.data2,
            custom_data1: main.custom_data1,
            regist_local_ip: main.client_local_ip.clone(),
        }
    }

    /// Port von `chiaki_get_ps_selected_addr()`.
    pub fn ps_selected_addr(&self) -> String {
        self.main_lock().ps_ip.clone()
    }

    /// Port von `chiaki_get_ps_ctrl_port()`.
    pub fn ps_ctrl_port(&self) -> u16 {
        self.main_lock().ctrl_port
    }

    /// Port von `chiaki_get_holepunch_sock()`.
    ///
    /// This function should be called after `punch_hole` for the given sock.
    /// Liefert einen Klon des `std::net::UdpSocket` (für ConnectInfo.rudp_sock).
    pub fn holepunch_sock(&self, type_: PortType) -> Option<UdpSocket> {
        let main = self.main_lock();
        let sock_ref = match type_ {
            PortType::Ctrl => main.ctrl_sock.as_ref(),
            PortType::Data => main.data_sock.as_ref(),
        };
        let s = sock_ref?;
        s.try_clone().ok()
    }

    /// Port von `chiaki_holepunch_session_get_stun_allocation()`.
    ///
    /// Get the STUN port allocation results that were computed while creating
    /// the hole punch session. `None` vor dem ersten STUN-Test (C: increment
    /// == -1); sonst `(allocation_increment, random_allocation)`.
    pub fn stun_allocation(&self) -> Option<(i32, bool)> {
        let main = self.main_lock();
        if main.stun_allocation_increment < 0 {
            return None;
        }
        Some((main.stun_allocation_increment, main.stun_random_allocation))
    }

    /// Port von `chiaki_holepunch_session_force_port_guessing()`.
    ///
    /// This forces port guessing to be used when port increment is 0.
    pub fn force_port_guessing(&self, enabled: bool) {
        self.main_lock().force_port_guessing = enabled;
    }

    /// Port von `chiaki_holepunch_session_set_port_guessing_ports()`.
    ///
    /// Sets the number of ports to use for port guessing NAT traversal
    /// (0 keeps the default of 75).
    pub fn set_port_guessing_ports(&self, count: i32) {
        if count > 0 {
            self.main_lock().port_guessing_count = count;
        }
    }

    /// Port von `chiaki_holepunch_session_set_port_guessing_socks()`.
    ///
    /// Set the number of sockets to open for port guessing NAT traversal
    /// (0 keeps the default of 250).
    pub fn set_port_guessing_socks(&self, count: i32) {
        if count > 0 {
            self.main_lock().port_guessing_socks = count;
        }
    }

    // -------------------------------------------------------------------
    // Session-Lifecycle
    // -------------------------------------------------------------------

    /// Port von `chiaki_holepunch_upnp_discover()`.
    ///
    /// Discovers UPnP if available. Läuft mit Timeout in einem Thread; bei
    /// Zeitüberschreitung wird weiter geladen (Status dann NotFound), wie im C.
    pub fn upnp_discover(&self) -> ChiakiResult<()> {
        {
            let mut upnp_state = self.inner.upnp.lock().unwrap_or_else(|e| e.into_inner());
            if upnp_state.thread.is_some() {
                return Ok(()); // läuft/beendet bereits
            }
            upnp_state.thread_running = true;
        }
        let inner = Arc::clone(&self.inner);
        let handle = std::thread::Builder::new()
            .name("Chiaki Holepunch UPnP".to_owned())
            .spawn(move || {
                let gw = upnp::discover(Duration::from_millis(2000));
                let mut upnp_state = inner.upnp.lock().unwrap_or_else(|e| e.into_inner());
                match gw {
                    Ok(gw) => {
                        upnp_state.gw = Some(gw);
                        upnp_state.status = GatewayStatus::Found;
                    }
                    Err(_) => {
                        upnp_state.gw = None;
                        upnp_state.status = GatewayStatus::NotFound;
                    }
                }
                upnp_state.thread_running = false;
                inner.state_cond.notify_all();
            })
            .map_err(|_| ChiakiError::Thread)?;

        let mut upnp_state = self.inner.upnp.lock().unwrap_or_else(|e| e.into_inner());
        while upnp_state.thread_running {
            let (guard, wait) = self
                .inner
                .state_cond
                .wait_timeout(upnp_state, Duration::from_millis(UPNP_DISCOVER_TIMEOUT_MS))
                .unwrap_or_else(|e| e.into_inner());
            upnp_state = guard;
            if wait.timed_out() {
                break;
            }
        }

        if upnp_state.thread_running {
            // Thread läuft weiter (Ergebnis kommt später), wir warten nicht länger
            upnp_state.thread = Some(handle);
            drop(upnp_state);
            tracing::warn!(
                "UPnP discovery timed out after {} ms, skipping",
                UPNP_DISCOVER_TIMEOUT_MS
            );
            self.inner
                .upnp
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .status = GatewayStatus::NotFound;
            return Ok(());
        }
        drop(upnp_state);
        if let Some(handle) = self
            .inner
            .upnp
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .thread
            .take()
        {
            let _ = handle.join();
        }
        Ok(())
    }

    /// Port von `chiaki_holepunch_session_create()`.
    ///
    /// Create a remote play session on the PSN server.
    /// This function must be called after [`new`](Self::new).
    pub fn create(&self) -> ChiakiResult<()> {
        // WebSocket-FQDN holen
        let fqdn = self.inner.psn.get_websocket_fqdn()?;
        {
            let mut ws = self.inner.ws.lock().unwrap_or_else(|e| e.into_inner());
            ws.fqdn = Some(fqdn);
        }

        if self.check_cancel() {
            tracing::info!("chiaki_holepunch_session_create: canceled");
            return Err(ChiakiError::Canceled);
        }

        // WebSocket-Thread starten
        {
            let mut ws = self.inner.ws.lock().unwrap_or_else(|e| e.into_inner());
            if ws.thread.is_none() {
                let inner = Arc::clone(&self.inner);
                ws.thread = Some(
                    std::thread::Builder::new()
                        .name("Chiaki Holepunch WS".to_owned())
                        .spawn(move || websocket_thread_func(inner))
                        .map_err(|_| ChiakiError::Thread)?,
                );
                tracing::trace!("chiaki_holepunch_session_create: Created websocket thread");
            }
        }

        // Auf WebSocket-Open warten (Cancel-Check hier: Verbesserung ggü. C)
        {
            let mut state = self.state_lock();
            while *state & SESSION_STATE_WS_OPEN == 0 {
                tracing::trace!(
                    "chiaki_holepunch_session_create: Waiting for websocket to open..."
                );
                if self.check_cancel() {
                    tracing::info!("chiaki_holepunch_session_create: canceled");
                    return Err(ChiakiError::Canceled);
                }
                let (guard, _) = self
                    .inner
                    .state_cond
                    .wait_timeout(state, Duration::from_millis(500))
                    .unwrap_or_else(|e| e.into_inner());
                state = guard;
            }
        }

        if self.check_cancel() {
            tracing::info!("chiaki_holepunch_session_create: canceled");
            return Err(ChiakiError::Canceled);
        }

        let pushctx_id = self.main_lock().pushctx_id.clone();
        let (session_id, account_id) = self.inner.psn.create_session(&pushctx_id)?;
        {
            let mut main = self.main_lock();
            main.session_id = session_id;
            main.account_id = account_id;
        }
        tracing::trace!(
            "chiaki_holepunch_session_create: Sent holepunch session creation request"
        );

        if self.check_cancel() {
            tracing::info!("chiaki_holepunch_session_create: canceled");
            return Err(ChiakiError::Canceled);
        }

        // FIXME (C): Kein gemeinsamer Timeout für beide Notifications.
        let notif_query = NOTIFICATION_TYPE_SESSION_CREATED | NOTIFICATION_TYPE_MEMBER_CREATED;
        let mut finished = false;
        while !finished {
            let notif = match self.wait_for_notification(
                notif_query,
                Duration::from_secs(SESSION_CREATION_TIMEOUT_SEC),
            ) {
                Err(ChiakiError::Timeout) => {
                    tracing::error!("chiaki_holepunch_session_create: Timed out waiting for holepunch session creation notifications.");
                    return Err(ChiakiError::Timeout);
                }
                Err(ChiakiError::Canceled) => {
                    tracing::info!("chiaki_holepunch_session_create: canceled");
                    return Err(ChiakiError::Canceled);
                }
                Err(e) => {
                    tracing::error!("chiaki_holepunch_session_create: Failed to wait for holepunch session creation notifications.");
                    return Err(e);
                }
                Ok(notif) => notif,
            };

            {
                let mut state = self.state_lock();
                match notif.type_ {
                    NOTIFICATION_TYPE_SESSION_CREATED => {
                        *state |= SESSION_STATE_CREATED;
                        tracing::trace!(
                            "chiaki_holepunch_session_create: Holepunch session created."
                        );
                        // Get the user's online id
                        let online_id = notif
                            .json
                            .pointer("/to/onlineId")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_owned());
                        let Some(online_id) = online_id else {
                            tracing::error!("chiaki_holepunch_session_create: JSON does not contain member with online Id of user");
                            tracing::trace!(
                                "chiaki_holepunch_session_create: JSON was:\n{}",
                                notif.json
                            );
                            return Err(ChiakiError::Unknown);
                        };
                        self.main_lock().online_id = Some(online_id);
                    }
                    NOTIFICATION_TYPE_MEMBER_CREATED => {
                        *state |= SESSION_STATE_CLIENT_JOINED;
                        tracing::trace!("chiaki_holepunch_session_create: Client joined.");
                    }
                    _ => {
                        tracing::error!(
                            "chiaki_holepunch_session_create: Got unexpected notification of type {}",
                            notif.type_
                        );
                        return Err(ChiakiError::Unknown);
                    }
                }
            }

            if self.check_cancel() {
                tracing::info!("chiaki_holepunch_session_create: canceled");
                return Err(ChiakiError::Canceled);
            }
            let session_id = self.main_lock().session_id.clone();
            let _ = self.inner.psn.check_session(&session_id, true);

            let state = *self.state_lock();
            log_session_state(state);
            finished = (state & SESSION_STATE_CREATED) != 0
                && (state & SESSION_STATE_CLIENT_JOINED) != 0;
        }
        Ok(())
    }

    /// Port von `chiaki_holepunch_session_start()`.
    ///
    /// Start a remote play session for a specific device.
    /// This function must be called after [`create`](Self::create).
    pub fn start(&self, console_uid: &[u8; 32], console_type: ConsoleType) -> ChiakiResult<()> {
        {
            let state = *self.state_lock();
            if state & SESSION_STATE_CREATED == 0 {
                tracing::error!("chiaki_holepunch_session_start: Holepunch session not created yet");
                return Err(ChiakiError::Uninitialized);
            }
            if state & SESSION_STATE_STARTED != 0 {
                tracing::error!("chiaki_holepunch_session_start: Holepunch session already started");
                return Err(ChiakiError::Unknown);
            }
        }

        {
            let mut main = self.main_lock();
            main.console_type = console_type;
        }

        if console_type == ConsoleType::Ps4 {
            // Wakes up and connects to the main PS4 console connected to a PSN
            // account (only main console can be used for remote connection via
            // PSN due to a limitation imposed by Sony)
            let (session_id, online_id, data1, data2) = {
                let main = self.main_lock();
                (
                    main.session_id.clone(),
                    main.online_id.clone(),
                    main.data1,
                    main.data2,
                )
            };
            tracing::trace!(
                "chiaki_holepunch_session_start: Starting holepunch session {} for the Main PS4 console registered to your PlayStation account",
                session_id
            );
            let Some(online_id) = online_id else {
                return Err(ChiakiError::Unknown);
            };
            if let Err(e) = self
                .inner
                .psn
                .ps4_session_wakeup(&online_id, &session_id, &data1, &data2)
            {
                tracing::error!(
                    "chiaki_holepunch_session_start: Starting holepunch session for PS4 failed with error {}",
                    e.code()
                );
                return Err(e);
            }
        } else {
            let duid_str = psn::bytes_to_hex(console_uid);
            let session_id = self.main_lock().session_id.clone();
            tracing::trace!(
                "chiaki_holepunch_session_start: Starting holepunch session {} for the PS5 console with duid {}",
                session_id,
                duid_str
            );
            let (account_id, data1, data2) = {
                let mut main = self.main_lock();
                main.console_uid.copy_from_slice(console_uid);
                (main.account_id, main.data1, main.data2)
            };
            if let Err(e) = self.inner.psn.start_session(
                console_uid,
                console_type,
                account_id,
                &session_id,
                &data1,
                &data2,
            ) {
                tracing::error!(
                    "chiaki_holepunch_session_start: Starting holepunch session for PS5 failed with error {}",
                    e.code()
                );
                return Err(e);
            }
        }

        {
            let mut state = self.state_lock();
            *state |= SESSION_STATE_DATA_SENT;
            log_session_state(*state);
        }

        if self.check_cancel() {
            tracing::info!("chiaki_holepunch_session_start: canceled");
            return Err(ChiakiError::Canceled);
        }

        // FIXME (C): Kein gemeinsamer Timeout für beide Notifications.
        let notif_query =
            NOTIFICATION_TYPE_MEMBER_CREATED | NOTIFICATION_TYPE_CUSTOM_DATA1_UPDATED;
        let mut finished = false;
        let mut last_err: ChiakiResult<()> = Ok(());
        while !finished {
            let notif = match self.wait_for_notification(
                notif_query,
                Duration::from_secs(SESSION_START_TIMEOUT_SEC),
            ) {
                Err(ChiakiError::Timeout) => {
                    tracing::error!("chiaki_holepunch_session_start: Timed out waiting for holepunch session start notifications.");
                    return Err(ChiakiError::HostDown);
                }
                Err(ChiakiError::Canceled) => {
                    tracing::info!("chiaki_holepunch_session_start: canceled");
                    return Err(ChiakiError::Canceled);
                }
                Err(_) => {
                    tracing::error!("chiaki_holepunch_session_start: Failed to wait for holepunch session start notifications.");
                    return Err(ChiakiError::Unknown);
                }
                Ok(notif) => notif,
            };

            {
                let mut state = self.state_lock();
                match notif.type_ {
                    NOTIFICATION_TYPE_MEMBER_CREATED => {
                        // Check if the session now contains the console we requested
                        let member_duid = notif
                            .json
                            .pointer("/body/data/members/0/deviceUniqueId")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_owned());
                        let Some(member_duid) = member_duid else {
                            tracing::error!("chiaki_holepunch_session_start: JSON does not contain member with a deviceUniqueId string field!");
                            tracing::trace!(
                                "chiaki_holepunch_session_start: JSON was:\n{}",
                                notif.json
                            );
                            last_err = Err(ChiakiError::Unknown);
                            break;
                        };
                        if member_duid.len() != 64 {
                            tracing::error!(
                                "chiaki_holepunch_session_start: \"deviceUniqueId\" has unexpected length, got {}, expected 64",
                                member_duid.len()
                            );
                            last_err = Err(ChiakiError::Unknown);
                            break;
                        }
                        let mut duid_bytes = [0u8; 32];
                        if psn::hex_to_bytes(&member_duid, &mut duid_bytes).is_err() {
                            tracing::error!("chiaki_holepunch_session_start: Could not convert member duid to bytes");
                            last_err = Err(ChiakiError::Unknown);
                            break;
                        }
                        // We don't have duid beforehand for PS4
                        let mut main = self.main_lock();
                        if console_type == ConsoleType::Ps4 {
                            main.console_uid = duid_bytes;
                        } else if duid_bytes != main.console_uid {
                            tracing::error!("chiaki_holepunch_session_start: holepunch session does not contain console");
                            drop(main);
                            last_err = Err(ChiakiError::Unknown);
                            break;
                        }
                        *state |= SESSION_STATE_CONSOLE_JOINED;
                    }
                    NOTIFICATION_TYPE_CUSTOM_DATA1_UPDATED => {
                        let custom_data1 = notif
                            .json
                            .pointer("/body/data/customData1")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_owned());
                        let Some(custom_data1) = custom_data1 else {
                            tracing::error!("chiaki_holepunch_session_start: JSON does not contain \"customData1\" string field");
                            tracing::trace!(
                                "chiaki_holepunch_session_start: JSON was:\n{}",
                                notif.json
                            );
                            last_err = Err(ChiakiError::Unknown);
                            break;
                        };
                        if custom_data1.len() != 32 {
                            tracing::error!(
                                "chiaki_holepunch_session_start: \"customData1\" has unexpected length, got {}, expected 32",
                                custom_data1.len()
                            );
                            last_err = Err(ChiakiError::Unknown);
                            break;
                        }
                        let mut out = [0u8; 16];
                        if let Err(e) = decode_customdata1(&custom_data1, &mut out) {
                            tracing::error!(
                                "chiaki_holepunch_session_start: Failed to decode \"customData1\": '{}' with error {}",
                                custom_data1,
                                e.code()
                            );
                            last_err = Err(e);
                            break;
                        }
                        self.main_lock().custom_data1 = out;
                        *state |= SESSION_STATE_CUSTOMDATA1_RECEIVED;
                    }
                    _ => {
                        tracing::error!(
                            "chiaki_holepunch_session_start: Got unexpected notification of type {}",
                            notif.type_
                        );
                        last_err = Err(ChiakiError::Unknown);
                        break;
                    }
                }
            }

            if self.check_cancel() {
                tracing::info!("chiaki_holepunch_session_start: canceled");
                return Err(ChiakiError::Canceled);
            }
            let session_id = self.main_lock().session_id.clone();
            let _ = self.inner.psn.check_session(&session_id, false);

            let state = *self.state_lock();
            finished = (state & SESSION_STATE_CONSOLE_JOINED) != 0
                && (state & SESSION_STATE_CUSTOMDATA1_RECEIVED) != 0;
            log_session_state(state);
        }
        last_err
    }

    /// Port von `holepunch_session_create_offer()`.
    ///
    /// Creates an OFFER session message to send via PSN. Muss vor `punch_hole`
    /// gerufen werden (zuerst für den Control-, dann für den Data-Socket).
    pub fn create_offer(&self) -> ChiakiResult<()> {
        let result = self.create_offer_impl();
        if result.is_err() {
            // cleanup_socket (C): Sockets schließen
            let mut main = self.main_lock();
            main.ipv4_sock = None;
            main.ipv6_sock = None;
        }
        result
    }

    fn create_offer_impl(&self) -> ChiakiResult<()> {
        let mut main = self.main_lock();

        if main.our_offer_msg.is_some() {
            tracing::warn!("Overwriting previously unsent offer message. Make sure you're punching the control hole before the data hole!");
            main.our_offer_msg = None;
        }
        if !main.local_candidates.is_empty() {
            tracing::warn!("Overwriting previously unused message. Make sure you're punching the control hole before the data hole!");
            main.local_candidates.clear();
        }

        // Create socket with available local port for connection
        main.ipv4_sock = Some(sock::create_udp_socket(
            "0.0.0.0:0".parse().unwrap(),
            &sock::UdpSocketOptions {
                reuse_address: true,
                ..Default::default()
            },
        )
        .map_err(|e| {
            tracing::error!("holepunch_session_create_offer: Creating ipv4 socket failed ({e:?})");
            ChiakiError::Unknown
        })?);
        let local_port = main.ipv4_sock.as_ref().expect("just set").local_addr().map_err(|e| {
            tracing::error!(
                "holepunch_session_create_offer: Getting ipv4 socket name failed with error {e}"
            );
            ChiakiError::Unknown
        })?
        .port();

        // IPv6-Socket auf demselben Port (C: Fehler → Abbruch)
        main.ipv6_sock = Some(sock::create_udp_socket(
            format!("[::]:{}", local_port).parse().unwrap(),
            &sock::UdpSocketOptions {
                reuse_address: true,
                ..Default::default()
            },
        )
        .map_err(|e| {
            tracing::error!(
                "holepunch_session_create_offer: Creating/binding ipv6 socket failed with error {e}"
            );
            ChiakiError::Unknown
        })?);

        let our_offer_msg_req_id = main.local_req_id;
        main.local_req_id += 1;

        if main.local_port_ctrl == 0 {
            main.local_port_ctrl = local_port;
        } else {
            main.local_port_data = local_port;
        }

        let mut candidates = vec![Candidate::default(); 3];
        let mut num_candidates = 2usize;
        // candidate_local = &candidates[2], candidate_remote = &candidates[1]
        let mut candidate_local_idx = 2usize;
        let mut candidate_remote_idx = 1usize;
        candidates[candidate_local_idx].type_ = CandidateType::Local;
        candidates[candidate_local_idx].addr_mapped = "0.0.0.0".to_owned();
        candidates[candidate_local_idx].port = local_port;
        candidates[candidate_local_idx].port_mapped = 0;
        candidates[candidate_remote_idx].type_ = CandidateType::Static;

        let mut have_addr = false;
        let gw = {
            let upnp_state = self.inner.upnp.lock().unwrap_or_else(|e| e.into_inner());
            match upnp_state.status {
                GatewayStatus::Found => upnp_state.gw.clone(),
                _ => None,
            }
        };
        match gw {
            None => {
                // GATEWAY_STATUS_UNKNOWN / NOT_FOUND
                let mut addr = String::new();
                if upnp::get_client_addr_local(&mut addr).is_err() {
                    return Err(ChiakiError::Network);
                }
                candidates[candidate_local_idx].addr = addr;
            }
            Some(gw) => {
                candidates[candidate_local_idx].addr = gw.lan_ip.clone();
                match upnp::add_port_mapping(&self.inner.psn.agent(), &gw, local_port, local_port) {
                    Ok(()) => {
                        tracing::info!(
                            "holepunch_session_create_offer: Added local UPNP port mapping to port {}",
                            local_port
                        );
                        match upnp::get_external_address(&self.inner.psn.agent(), &gw) {
                            Ok(ext) => {
                                candidates[candidate_remote_idx].addr = ext;
                                have_addr = true;
                            }
                            Err(_) => {
                                tracing::error!("UPNP error getting external IP Address");
                            }
                        }
                    }
                    Err(_) => {
                        tracing::error!("holepunch_session_create_offer: Adding upnp port mapping failed");
                    }
                }
            }
        }

        main.client_local_ip = candidates[candidate_local_idx].addr.clone();

        if !have_addr {
            // Move current candidates behind STUN candidates so when the console
            // reaches out to our STUN candidate it will be using the correct port
            // if behind symmetric NAT
            let candidate_stun_idx = 0usize;
            candidates[candidate_stun_idx].type_ = CandidateType::Stun;
            candidates[candidate_stun_idx].addr_mapped = "0.0.0.0".to_owned();
            candidates[candidate_stun_idx].port_mapped = 0;

            let mut stun_addr = String::new();
            let mut stun_port = 0u16;
            // Der Socket wird als Option herausgenommen (STUN kann ihn bei
            // Sendefehlern schließen, wie im C)
            let mut stun_sock_slot = main.ipv4_sock.take();
            have_addr = self.run_stun(
                &mut main,
                &mut stun_sock_slot,
                &mut stun_addr,
                &mut stun_port,
                true,
            );
            main.ipv4_sock = stun_sock_slot;
            if have_addr {
                candidates[candidate_stun_idx].addr = stun_addr.clone();
                candidates[candidate_stun_idx].port = stun_port;
                candidates[candidate_remote_idx].addr = stun_addr.clone();
                // Local port is used externally so don't make duplicate STUN
                // candidate since STATIC candidate will have same ip and port number
                if main.stun_allocation_increment != 0 {
                    let original_candidates = candidates[..3].to_vec();
                    if !main.stun_random_allocation {
                        candidates.resize(11, Candidate::default());
                        // Setup extra stun candidate in case there was an
                        // allocation in between the stun request and our allocation
                        let mut port_check = i32::from(original_candidates[0].port);
                        for i in 0..8 {
                            let tmp = port_check;
                            // skip well known ports 0-1024 unless current
                            // allocation is within that range since most
                            // routers don't use these (it it is in range
                            // implies router uses those ports)
                            port_check += main.stun_allocation_increment;
                            port_check = wrap_increment_port(port_check, tmp);
                            let c = &mut candidates[i];
                            c.type_ = CandidateType::Stun;
                            c.addr_mapped = "0.0.0.0".to_owned();
                            c.port_mapped = 0;
                            c.addr = original_candidates[0].addr.clone();
                            c.port = port_check as u16;
                        }
                        candidates[8] = original_candidates[1].clone();
                        candidates[9] = original_candidates[2].clone();
                        candidate_remote_idx = 8;
                        candidate_local_idx = 9;
                        num_candidates = 10;
                    } else {
                        let guess_count = main.port_guessing_count;
                        // Setup session->port_guessing_count STUN candidates
                        // because we have a random allocation and usually 64
                        // port blocks are minimum
                        tracing::info!(
                            "Initiating random allocation guesses with {} guesses",
                            guess_count
                        );
                        candidates.resize((guess_count + 3) as usize, Candidate::default());
                        let base_port = i32::from(original_candidates[0].port);
                        for i in 0..guess_count {
                            let port = guess_port(base_port, i);
                            let c = &mut candidates[i as usize];
                            c.type_ = CandidateType::Stun;
                            c.addr_mapped = "0.0.0.0".to_owned();
                            c.port_mapped = 0;
                            c.port = port as u16;
                            c.addr = original_candidates[0].addr.clone();
                        }
                        candidates[guess_count as usize] = original_candidates[1].clone();
                        candidates[(guess_count + 1) as usize] = original_candidates[2].clone();
                        candidate_remote_idx = guess_count as usize;
                        candidate_local_idx = (guess_count + 1) as usize;
                        num_candidates = (guess_count + 2) as usize;
                    }
                } else if i32::from(local_port) == i32::from(stun_port) {
                    candidates[0] = candidates[1].clone();
                    candidates[1] = candidates[2].clone();
                    candidate_remote_idx = 0;
                    candidate_local_idx = 1;
                } else if main.force_port_guessing {
                    // Possible double NAT or port-rewriting cone NAT:
                    // local_port != stun_port but increment is 0 (EIM on outermost NAT).
                    // Since we can't predict the actual external port mapping, force
                    // the random allocation path: send sequential port guesses as STUN
                    // candidates for the console to try, and open many sockets in
                    // check_candidates so the NAT assigns us many external ports,
                    // maximizing the chance of overlap with the console's connection attempts.
                    let guess_count = main.port_guessing_count;
                    tracing::info!(
                        "holepunch_session_create_offer: Port rewriting NAT detected (local_port {} != stun_port {} with increment 0), forcing random allocation with {} guesses",
                        local_port,
                        stun_port,
                        guess_count
                    );
                    main.stun_random_allocation = true;
                    main.stun_allocation_increment = 1;
                    let original_candidates = candidates[..3].to_vec();
                    candidates.resize((guess_count + 3) as usize, Candidate::default());
                    let base_port = i32::from(original_candidates[0].port);
                    for i in 0..guess_count {
                        let port = guess_port(base_port, i);
                        let c = &mut candidates[i as usize];
                        c.type_ = CandidateType::Stun;
                        c.addr_mapped = "0.0.0.0".to_owned();
                        c.port_mapped = 0;
                        c.port = port as u16;
                        c.addr = original_candidates[0].addr.clone();
                    }
                    candidates[guess_count as usize] = original_candidates[1].clone();
                    candidates[(guess_count + 1) as usize] = original_candidates[2].clone();
                    candidate_remote_idx = guess_count as usize;
                    candidate_local_idx = (guess_count + 1) as usize;
                    num_candidates = (guess_count + 2) as usize;
                } else {
                    num_candidates = 3;
                    candidate_remote_idx = 1;
                    candidate_local_idx = 2;
                }
            } else {
                tracing::error!("holepunch_session_create_offer: Could not get remote address from STUN");
            }
            if main.ipv4_sock.is_none() {
                // STUN hat den Socket wegen Fehlers geschlossen
                tracing::error!("holepunch_session_create_offer: STUN caused socket to close due to error");
                return Err(ChiakiError::Unknown);
            }
            // Only PS5 supports ipv6 — im C per ENABLE_IPV6=false deaktiviert
            if ENABLE_IPV6 {
                tracing::info!("holepunch_session_create_offer: IPV6 NOT supported by your PlayStation console. Skipping IPV6 connection");
            } else {
                tracing::info!("holepunch_session_create_offer: IPV6 NOT supported by your PlayStation console. Skipping IPV6 connection");
            }
        } else {
            // If no STUN address the static and local candidates are our first candidates
            candidates[0] = candidates[1].clone();
            candidates[1] = candidates[2].clone();
            candidate_remote_idx = 0;
            candidate_local_idx = 1;
        }

        if !have_addr {
            tracing::error!("holepunch_session_create_offer: Could not get remote address");
            return Err(ChiakiError::Unknown);
        }

        {
            let Some(ipv4_sock) = main.ipv4_sock.as_ref() else {
                return Err(ChiakiError::Unknown);
            };
            if let Err(e) = sock::set_nonblock(ipv4_sock, true) {
                tracing::error!(
                    "holepunch_session_create_offer: Failed to set ipv4 socket to non-blocking: {e:?}"
                );
                return Err(ChiakiError::Unknown);
            }
        }

        candidates[candidate_remote_idx].addr_mapped = "0.0.0.0".to_owned();
        candidates[candidate_remote_idx].port = local_port;
        candidates[candidate_remote_idx].port_mapped = 0;

        main.local_candidates = vec![
            candidates[candidate_local_idx].clone(),
            // either STUN candidate if it exists, else STATIC candidate
            candidates[0].clone(),
        ];

        let mut msg = SessionMessage {
            action: SESSION_MESSAGE_ACTION_OFFER,
            req_id: our_offer_msg_req_id as u16,
            error: 0,
            conn_request: Some(ConnectionRequest {
                sid: u32::from(main.sid_local),
                peer_sid: u32::from(main.sid_console),
                nat_type: 2,
                skey: [0; 16],
                default_route_mac_addr: [0; 6],
                local_hashed_id: main.hashed_id_local,
                candidates,
            }),
        };
        // C: num_candidates — hier implizit über conn_request.candidates
        if let Some(cr) = msg.conn_request.as_mut() {
            cr.candidates.truncate(num_candidates);
        }
        // IPv6-Pfad ist deaktiviert (ENABLE_IPV6 == false): C-Socket schließen
        main.ipv6_sock = None;
        main.our_offer_msg = Some(msg);
        Ok(())
    }

    /// Port von `chiaki_holepunch_session_punch_hole()`.
    ///
    /// Punch a hole in the NAT for the control or data socket. This function
    /// must be called twice, once for the control socket and once for the data
    /// socket, precisely in that order.
    pub fn punch_hole(&self, port_type: PortType) -> ChiakiResult<()> {
        let result = self.punch_hole_impl(port_type);
        // cleanup (C): bei Fehler ipv4/ipv6-Sockets schließen;
        // offer_cleanup: our_offer_msg + local_candidates immer freigeben
        {
            let mut main = self.main_lock();
            if result.is_err() {
                main.ipv4_sock = None;
                main.ipv6_sock = None;
            }
            main.our_offer_msg = None;
            main.local_candidates.clear();
        }
        result
    }

    fn punch_hole_impl(&self, port_type: PortType) -> ChiakiResult<()> {
        {
            let state = *self.state_lock();
            if port_type == PortType::Ctrl && state & SESSION_STATE_CUSTOMDATA1_RECEIVED == 0 {
                tracing::error!("chiaki_holepunch_session_punch_holes: customData1 not received yet.");
                return Err(ChiakiError::Unknown);
            } else if port_type == PortType::Data && state & SESSION_STATE_CTRL_ESTABLISHED == 0 {
                tracing::error!("chiaki_holepunch_session_punch_holes: Control port not open yet.");
                return Err(ChiakiError::Unknown);
            }
        }

        // NOTE: Needs to be kept around until the end, we're using the
        // candidates in the message later on
        let console_offer_msg = match self.wait_for_session_message(
            SESSION_MESSAGE_ACTION_OFFER,
            Duration::from_secs(SESSION_START_TIMEOUT_SEC),
        ) {
            Err(ChiakiError::Timeout) => {
                tracing::error!("chiaki_holepunch_session_punch_holes: Timed out waiting for OFFER holepunch session message.");
                return Err(ChiakiError::Timeout);
            }
            Err(ChiakiError::Canceled) => {
                tracing::info!("chiaki_holepunch_session_punch_holes: canceled");
                return Err(ChiakiError::Canceled);
            }
            Err(e) => {
                tracing::error!("chiaki_holepunch_session_punch_holes: Failed to wait for OFFER holepunch session message.");
                return Err(e);
            }
            Ok(msg) => msg,
        };

        let console_req = console_offer_msg
            .conn_request
            .clone()
            .ok_or(ChiakiError::Unknown)?;
        {
            let mut main = self.main_lock();
            main.hashed_id_console = console_req.local_hashed_id;
            main.sid_console = console_req.sid as u16;
        }

        {
            let mut state = self.state_lock();
            *state |= match port_type {
                PortType::Ctrl => SESSION_STATE_CTRL_OFFER_RECEIVED,
                PortType::Data => SESSION_STATE_DATA_OFFER_RECEIVED,
            };
        }

        // ACK the message
        let ack_msg = SessionMessage {
            action: SESSION_MESSAGE_ACTION_RESULT,
            req_id: console_offer_msg.req_id,
            error: 0,
            conn_request: None,
        };
        if let Err(e) = self.send_session_message_short(&ack_msg) {
            tracing::error!("chiaki_holepunch_session_punch_holes: Couldn't send holepunch session message");
            return Err(e);
        }

        if self.check_cancel() {
            tracing::info!("chiaki_holepunch_session_punch_holes: canceled");
            return Err(ChiakiError::Canceled);
        }

        // Send our own OFFER
        let our_offer_req_id = {
            let mut main = self.main_lock();
            let req_id = main.local_req_id;
            main.local_req_id += 1;
            if let Some(offer) = main.our_offer_msg.as_mut() {
                offer.req_id = req_id as u16;
            }
            req_id
        };
        // C ignoriert den Rückgabewert von send_offer()
        let _ = self.send_offer();

        // Wait for ACK of OFFER, ignore other OFFERs, simply ACK them
        if let Err(e) = self.wait_for_session_message_ack(
            our_offer_req_id as u16,
            Duration::from_secs(SESSION_START_TIMEOUT_SEC),
        ) {
            match e {
                ChiakiError::Timeout => {
                    tracing::error!("chiaki_holepunch_session_punch_holes: Timed out waiting for ACK of our connection offer.");
                }
                ChiakiError::Canceled => {
                    tracing::info!("chiaki_holepunch_session_punch_holes: canceled");
                }
                _ => {
                    tracing::error!("chiaki_holepunch_session_punch_holes: Failed to wait for ACK of our connection offer.");
                }
            }
            return Err(e);
        }
        let session_id = self.main_lock().session_id.clone();
        let _ = self.inner.psn.check_session(&session_id, true);

        // Find candidate that we can use to connect to the console
        for candidate in &console_req.candidates {
            print_candidate(candidate);
        }
        let (selected_sock, selected_candidate) = {
            let mut main = self.main_lock();
            match self.check_candidates(&mut main, console_req.candidates.clone()) {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(
                        "chiaki_holepunch_session_punch_holes: Failed to find reachable candidate for {} connection.",
                        if port_type == PortType::Ctrl { "control" } else { "data" }
                    );
                    return Err(e);
                }
            }
        };

        if self.check_cancel() {
            tracing::info!("chiaki_holepunch_session_punch_holes: canceled");
            return Err(ChiakiError::Canceled);
        }

        if let Err(e) = self.send_accept(our_offer_req_id as u16, &selected_candidate) {
            tracing::error!("chiaki_holepunch_session_punch_holes: Failed to send ACCEPT message.");
            return Err(e);
        }

        if self.check_cancel() {
            tracing::info!("chiaki_holepunch_session_punch_holes: canceled");
            return Err(ChiakiError::Canceled);
        }
        {
            let mut main = self.main_lock();
            main.local_req_id += 1;
        }

        let msg = match self.wait_for_session_message(
            SESSION_MESSAGE_ACTION_ACCEPT,
            Duration::from_secs(SESSION_START_TIMEOUT_SEC),
        ) {
            Err(ChiakiError::Timeout) => {
                tracing::error!("chiaki_holepunch_session_punch_holes: Timed out waiting for ACCEPT holepunch session message.");
                return Err(ChiakiError::Timeout);
            }
            Err(ChiakiError::Canceled) => {
                tracing::info!("chiaki_holepunch_session_punch_holes: canceled");
                return Err(ChiakiError::Canceled);
            }
            Err(e) => {
                tracing::error!("chiaki_holepunch_session_punch_holes: Failed to wait for ACCEPT or OFFER holepunch session message.");
                return Err(e);
            }
            Ok(msg) => msg,
        };

        // ACK the accept message response
        let accept_ack_msg = SessionMessage {
            action: SESSION_MESSAGE_ACTION_RESULT,
            req_id: msg.req_id,
            error: 0,
            conn_request: None,
        };
        if let Err(e) = self.send_session_message_short(&accept_ack_msg) {
            tracing::error!("chiaki_holepunch_session_punch_holes: Couldn't send holepunch session message");
            return Err(e);
        }

        if self.check_cancel() {
            tracing::info!("chiaki_holepunch_session_punch_holes: canceled");
            return Err(ChiakiError::Canceled);
        }

        {
            let mut main = self.main_lock();
            main.ps_ip = selected_candidate.addr.clone();
        }

        {
            let mut state = self.state_lock();
            match port_type {
                PortType::Ctrl => {
                    *state |= SESSION_STATE_CTRL_ESTABLISHED;
                    let mut main = self.main_lock();
                    main.ctrl_sock = Some(selected_sock.try_clone().map_err(|_| ChiakiError::Unknown)?);
                    main.ctrl_port = selected_candidate.port;
                    tracing::trace!("chiaki_holepunch_session_punch_holes: Control connection established.");
                }
                PortType::Data => {
                    *state |= SESSION_STATE_DATA_ESTABLISHED;
                    self.main_lock().data_sock =
                        Some(selected_sock.try_clone().map_err(|_| ChiakiError::Unknown)?);
                    tracing::trace!("chiaki_holepunch_session_punch_holes: Data connection established.");
                }
            }
        }

        if self.check_cancel() {
            tracing::info!("chiaki_holepunch_session_punch_holes: canceled");
            return Err(ChiakiError::Canceled);
        }

        let ctx = {
            let main = self.main_lock();
            PsHandshakeCtx::from_main(&main)
        };
        let mut err = Ok(());
        match receive_request_send_response_ps(
            &self.inner.select_pipe,
            &ctx,
            &selected_sock,
            &selected_candidate,
            Duration::from_secs(WAIT_RESPONSE_TIMEOUT_SEC),
        ) {
            Err(pserr @ ChiakiError::Timeout) => {
                // C: pserr == TIMEOUT ist kein Fehler
                let _ = pserr;
            }
            Err(pserr) => {
                tracing::error!("Sending extra request to ps failed");
                err = Err(pserr);
            }
            Ok(()) => {}
        }
        log_session_state(*self.state_lock());
        err
    }

    /// Port von `chiaki_holepunch_main_thread_cancel()`.
    ///
    /// Cancel initial psn connection steps (i.e., session create, session start
    /// and session punch hole).
    pub fn main_thread_cancel(&self, stop_thread: bool) {
        {
            let mut flags = self.stop_lock();
            if stop_thread {
                flags.ws_thread_should_stop = true;
                self.inner.select_pipe.stop();
            } else {
                tracing::info!("Canceling establishing connection over PSN");
            }
            flags.main_should_stop = true;
        }
        self.inner.notif_cond.notify_all();
        self.inner.state_cond.notify_all();
    }

    /// Port von `chiaki_holepunch_session_fini()`.
    ///
    /// **IMPORTANT**: This function should be called after the **streaming**
    /// session has terminated, not after all sockets have been obtained.
    ///
    /// Will delete the session on the PSN server and free all resources
    /// associated with the session.
    pub fn fini(&self) {
        let ws_open = self.inner.ws.lock().unwrap_or_else(|e| e.into_inner()).open;
        if ws_open {
            let session_id = self.main_lock().session_id.clone();
            if self.inner.psn.delete_session(&session_id).is_err() {
                tracing::error!("Couldn't remove our holepunch session gracefully from PlayStation servers.");
            }
            let notif_query = NOTIFICATION_TYPE_MEMBER_DELETED | NOTIFICATION_TYPE_SESSION_DELETED;
            loop {
                match self.wait_for_notification(
                    notif_query,
                    Duration::from_secs(SESSION_DELETION_TIMEOUT_SEC),
                ) {
                    Err(ChiakiError::Timeout) => {
                        tracing::error!("chiaki_holepunch_session_fini: Timed out waiting for holepunch session deletion notifications.");
                        break;
                    }
                    Err(_) => {
                        tracing::error!("chiaki_holepunch_session_fini: Failed to wait for holepunch session deletion notifications.");
                        break;
                    }
                    Ok(notif) => {
                        if notif.type_
                            & (NOTIFICATION_TYPE_MEMBER_DELETED | NOTIFICATION_TYPE_SESSION_DELETED)
                            != 0
                        {
                            let mut state = self.state_lock();
                            *state |= SESSION_STATE_DELETED;
                            log_session_state(*state);
                            drop(state);
                            tracing::info!("chiaki_holepunch_session_fini: Holepunch session deleted.");
                            break;
                        }
                        tracing::error!(
                            "chiaki_holepunch_session_fini: Got unexpected notification of type {}",
                            notif.type_
                        );
                        break;
                    }
                }
            }
            {
                let mut flags = self.stop_lock();
                flags.ws_thread_should_stop = true;
            }
            self.inner.select_pipe.stop();
            let handle = self
                .inner
                .ws
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .thread
                .take();
            if let Some(handle) = handle {
                let _ = handle.join();
            }
        }

        {
            let handle = self
                .inner
                .upnp
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .thread
                .take();
            if let Some(handle) = handle {
                tracing::info!("Waiting for UPnP discovery thread to finish...");
                let _ = handle.join();
                self.inner
                    .upnp
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .thread_running = false;
            }
        }

        let (gw, local_port_ctrl, local_port_data) = {
            let main = self.main_lock();
            let gw = self
                .inner
                .upnp
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .gw
                .clone();
            (gw, main.local_port_ctrl, main.local_port_data)
        };
        if let Some(gw) = gw {
            if local_port_ctrl != 0 {
                match upnp::delete_port_mapping(&self.inner.psn.agent(), &gw, local_port_ctrl) {
                    Ok(()) => tracing::info!("Deleted UPNP local port ctrl mapping"),
                    Err(_) => tracing::error!("Couldn't delete UPNP local port ctrl mapping"),
                }
            }
            if local_port_data != 0 {
                match upnp::delete_port_mapping(&self.inner.psn.agent(), &gw, local_port_data) {
                    Ok(()) => tracing::info!("Deleted UPNP local port data mapping"),
                    Err(_) => tracing::error!("Couldn't delete UPNP local port data mapping"),
                }
            }
        }
    }
}

impl Drop for HolepunchSession {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 1 {
            // Letzte Referenz: WS-Thread anhalten (PSN-Delete passiert nur in
            // fini(), wie im C).
            self.inner
                .stop
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .ws_thread_should_stop = true;
            self.inner.select_pipe.stop();
            self.inner.notif_cond.notify_all();
            self.inner.state_cond.notify_all();
        }
    }
}

// ---------------------------------------------------------------------------
// Interne Helfer (Main-Flow)
// ---------------------------------------------------------------------------

impl HolepunchSession {
    /// Port von `get_client_addr_remote_stun()`.
    ///
    /// @param sock Der (geklonte) Socket-Slot; STUN kann ihn bei Sendefehlern
    ///             schließen (Option wird None), wie im C.
    fn run_stun(
        &self,
        main: &mut MainState,
        sock_slot: &mut Option<UdpSocket>,
        address: &mut String,
        port: &mut u16,
        ipv4: bool,
    ) -> bool {
        // run STUN test if it hasn't been run yet
        if main.stun_allocation_increment == -1 {
            match self.inner.psn.fetch_stun_servers() {
                Ok(servers) => main.stun_server_list = servers,
                Err(e) => tracing::warn!("Getting stun servers returned error {}", e.code()),
            }
            let mut allocation_increment = main.stun_allocation_increment;
            let mut random_allocation = main.stun_random_allocation;
            let ok = stun::stun_port_allocation_test(
                address,
                port,
                &mut allocation_increment,
                &mut random_allocation,
                &main.stun_server_list,
                sock_slot,
            );
            main.stun_allocation_increment = allocation_increment;
            main.stun_random_allocation = random_allocation;
            if !ok {
                tracing::error!("get_client_addr_remote_stun: Failed to get external address");
                return false;
            }
            return true;
        }
        let servers: &[StunServer] = if ipv4 {
            &main.stun_server_list
        } else {
            &main.stun_server_list_ipv6
        };
        if !stun::stun_get_external_address(address, port, servers, sock_slot, ipv4) {
            tracing::error!("get_client_addr_remote_stun: Failed to get external address");
            return false;
        }
        true
    }

    /// Port von `send_offer()`.
    fn send_offer(&self) -> ChiakiResult<()> {
        let (msg, account_id, console_uid, console_type, session_id) = {
            let main = self.main_lock();
            let Some(msg) = main.our_offer_msg.clone() else {
                return Err(ChiakiError::Uninitialized);
            };
            print_session_request(msg.conn_request.as_ref());
            (
                msg,
                main.account_id,
                main.console_uid,
                main.console_type,
                main.session_id.clone(),
            )
        };
        let payload = self.session_message_serialize(&msg)?;
        self.inner
            .psn
            .send_session_message(&session_id, &console_uid, console_type, account_id, &payload)
    }

    /// Port von `send_accept()`.
    fn send_accept(&self, req_id: u16, selected_candidate: &Candidate) -> ChiakiResult<()> {
        let msg = {
            let main = self.main_lock();
            SessionMessage {
                action: SESSION_MESSAGE_ACTION_ACCEPT,
                req_id,
                error: 0,
                conn_request: Some(ConnectionRequest {
                    sid: u32::from(main.sid_local),
                    peer_sid: u32::from(main.sid_console),
                    nat_type: 0,
                    skey: [0; 16],
                    default_route_mac_addr: [0; 6],
                    local_hashed_id: [0; 20],
                    candidates: vec![selected_candidate.clone()],
                }),
            }
        };
        let payload = self.session_message_serialize(&msg)?;
        let (account_id, console_uid, console_type, session_id) = {
            let main = self.main_lock();
            (
                main.account_id,
                main.console_uid,
                main.console_type,
                main.session_id.clone(),
            )
        };
        self.inner
            .psn
            .send_session_message(&session_id, &console_uid, console_type, account_id, &payload)
    }

    /// Port von `http_send_session_message()` mit `short_msg == true`
    /// (ACKs mit leerem connRequest "{}").
    fn send_session_message_short(&self, message: &SessionMessage) -> ChiakiResult<()> {
        let payload = psn::session_message_json(
            psn::action_str(message.action),
            message.req_id,
            message.error,
            "{}",
        );
        let (account_id, console_uid, console_type, session_id) = {
            let main = self.main_lock();
            (
                main.account_id,
                main.console_uid,
                main.console_type,
                main.session_id.clone(),
            )
        };
        self.inner
            .psn
            .send_session_message(&session_id, &console_uid, console_type, account_id, &payload)
    }

    /// Port von `session_message_serialize()` (auf psn-Buildern).
    fn session_message_serialize(&self, message: &SessionMessage) -> ChiakiResult<String> {
        let Some(conn_request) = message.conn_request.as_ref() else {
            // short path
            return Ok(psn::session_message_json(
                psn::action_str(message.action),
                message.req_id,
                message.error,
                "{}",
            ));
        };
        // Since the official remote play app doesn't send valid JSON half the time,
        // we can't use a proper JSON library to serialize the message. Instead, we
        // build the JSON string manually (see psn builders).
        let account_id = self.main_lock().account_id;
        let localpeeraddr_json = psn::session_localpeeraddr_json(account_id, "REMOTE_PLAY");

        let mut candidate_strs = Vec::with_capacity(conn_request.candidates.len());
        for candidate in &conn_request.candidates {
            candidate_strs.push(psn::session_connrequest_candidate_json(
                candidate.type_.as_str(),
                &candidate.addr,
                &candidate.addr_mapped,
                candidate.port,
                candidate.port_mapped,
            ));
        }
        let candidates_json = format!("[{}]", candidate_strs.join(","));

        let localhashedid_str = if conn_request.local_hashed_id == [0u8; 20] {
            String::new()
        } else {
            base64::encode(&conn_request.local_hashed_id)
        };
        let skey_str = base64::encode(&conn_request.skey);

        let connreq_json = psn::session_connrequest_json(
            conn_request.sid as u16,
            conn_request.peer_sid as u16,
            &skey_str,
            conn_request.nat_type,
            &candidates_json,
            &localpeeraddr_json,
            &localhashedid_str,
        );

        Ok(psn::session_message_json(
            psn::action_str(message.action),
            message.req_id,
            message.error,
            &connreq_json,
        ))
    }

    /// Port von `wait_for_notification()`.
    ///
    /// @param types OR-verknüpfte Notification-Typen
    /// @return Ok(notif) bei Fund, Err(Timeout/Canceled/…) sonst
    pub(crate) fn wait_for_notification(
        &self,
        types: u16,
        timeout: Duration,
    ) -> ChiakiResult<Notification> {
        let deadline = Instant::now() + timeout;
        let mut guard = self.notif_lock();
        loop {
            if let Some(idx) = guard.queue.iter().position(|n| n.type_ & types != 0) {
                tracing::trace!(
                    "wait_for_notification: Found notification of type {}",
                    guard.queue[idx].type_
                );
                let notif = guard.queue.remove(idx).expect("index valid");
                return Ok(notif);
            }
            let now = Instant::now();
            if now >= deadline {
                tracing::error!(
                    "wait_for_notification: Timed out waiting for holepunch session messages"
                );
                return Err(ChiakiError::Timeout);
            }
            let (g, _) = self
                .inner
                .notif_cond
                .wait_timeout(guard, (deadline - now).min(Duration::from_millis(500)))
                .unwrap_or_else(|e| e.into_inner());
            guard = g;

            if self.check_cancel() {
                return Err(ChiakiError::Canceled);
            }
        }
    }

    /// Port von `wait_for_session_message()`.
    fn wait_for_session_message(
        &self,
        types: u8,
        timeout: Duration,
    ) -> ChiakiResult<SessionMessage> {
        loop {
            let notif = match self.wait_for_notification(
                NOTIFICATION_TYPE_SESSION_MESSAGE_CREATED,
                Duration::from_secs(SESSION_START_TIMEOUT_SEC).min(timeout),
            ) {
                Err(ChiakiError::Timeout) => {
                    tracing::error!("Timed out waiting for holepunch session message notification.");
                    return Err(ChiakiError::Timeout);
                }
                Err(e) => {
                    tracing::error!("Failed to wait for holepunch session message notification.");
                    return Err(e);
                }
                Ok(notif) => notif,
            };
            if self.check_cancel() {
                return Err(ChiakiError::Canceled);
            }
            let payload = session_message_get_payload(&notif.json)?;
            let msg = session_message_parse(&payload)?;
            if msg.action & SESSION_MESSAGE_ACTION_TERMINATE != 0 {
                tracing::warn!(
                    "Holepunch session received Terminate message, terminating {}",
                    msg.action
                );
                return Err(ChiakiError::Canceled);
            }
            if msg.action & types == 0 {
                tracing::trace!("Ignoring holepunch session message with action {}", msg.action);
                continue;
            }
            return Ok(msg);
        }
    }

    /// Port von `wait_for_session_message_ack()`.
    fn wait_for_session_message_ack(&self, req_id: u16, timeout: Duration) -> ChiakiResult<()> {
        loop {
            let msg = self.wait_for_session_message(SESSION_MESSAGE_ACTION_RESULT, timeout)?;
            if self.check_cancel() {
                return Err(ChiakiError::Canceled);
            }
            if msg.req_id != req_id {
                tracing::error!(
                    "wait_for_session_message_ack: Got ACK for unexpected request ID {}",
                    msg.req_id
                );
                continue;
            }
            return Ok(());
        }
    }

    /// Port von `check_candidates()`.
    ///
    /// Linking to a responsive PlayStation candidate from the available console
    /// candidates. Liefert den verbundenen (connectierten) Socket und den
    /// gewählten Kandidaten.
    fn check_candidates(
        &self,
        main: &mut MainState,
        candidates_received: Vec<Candidate>,
    ) -> ChiakiResult<(UdpSocket, Candidate)> {
        let ctx = PsHandshakeCtx::from_main(main);
        let num_candidates = candidates_received.len();

        // Set up request buffer
        let mut request_buf = [[0u8; 88]; CHECK_CANDIDATES_REQUEST_NUMBER];
        let mut request_id = [[0u8; 5]; CHECK_CANDIDATES_REQUEST_NUMBER];

        // send CHECK_CANDIDATES_REQUEST_NUMBER requests for connection pairing with ps
        for i in 0..CHECK_CANDIDATES_REQUEST_NUMBER {
            random::random_bytes_crypt(&mut request_id[i])?;
            request_buf[i][0x00..0x04].copy_from_slice(&MSG_TYPE_REQ.to_be_bytes());
            request_buf[i][0x04..0x18].copy_from_slice(&ctx.hashed_id_local);
            request_buf[i][0x24..0x38].copy_from_slice(&ctx.hashed_id_console);
            request_buf[i][0x44..0x46].copy_from_slice(&ctx.sid_local.to_be_bytes());
            request_buf[i][0x46..0x48].copy_from_slice(&ctx.sid_console.to_be_bytes());
            request_buf[i][0x4b..0x50].copy_from_slice(&request_id[i]);
        }

        let local_candidate = main.local_candidates.first().cloned().unwrap_or_default();
        let remote_candidate = main.local_candidates.get(1).cloned().unwrap_or_default();

        let mut extra_addresses_used = 0usize;
        // Set up addresses for each candidate + extras
        let mut candidates = candidates_received;
        candidates.resize(num_candidates + EXTRA_CANDIDATE_ADDRESSES, Candidate::default());
        let mut addrs: Vec<Option<SocketAddr>> =
            vec![None; num_candidates + EXTRA_CANDIDATE_ADDRESSES];
        let mut responses_received = vec![0i32; num_candidates + EXTRA_CANDIDATE_ADDRESSES];

        // NAT-Probing-Sockets (Port-Guessing)
        let mut socks: Vec<Option<UdpSocket>> = Vec::new();
        if main.stun_random_allocation {
            let socks_capacity = main.port_guessing_socks.max(0) as usize;
            for i in 0..socks_capacity {
                let s = match sock::create_udp_socket(
                    "0.0.0.0:0".parse().unwrap(),
                    &sock::UdpSocketOptions {
                        reuse_address: true,
                        ..Default::default()
                    },
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!(
                            "check_candidates: Creating ipv4 socket {} failed, stopping (opened {} sockets) ({e:?})",
                            i,
                            socks.len()
                        );
                        break;
                    }
                };
                // set low ttl so packets just punch hole in NAT
                if let Err(e) = socket2::SockRef::from(&s).set_ttl_v4(2) {
                    tracing::error!("check_candidates: setsockopt(IP_TTL) failed with error {e}");
                    drop(s);
                    continue;
                }
                if let Err(e) = sock::set_nonblock(&s, true) {
                    tracing::error!(
                        "check_candidates: Failed to set ipv4 socket {} to non-blocking: {e:?}",
                        i
                    );
                    drop(s);
                    continue;
                }
                socks.push(Some(s));
            }
            tracing::info!("check_candidates: Opened {} NAT probing sockets", socks.len());
        }

        // Adressen auflösen und Requests senden
        let mut failed = true;
        for (i, candidate) in candidates.iter().enumerate().take(num_candidates) {
            let Some(mut resolved) = resolve_candidate_addr(candidate) else {
                tracing::error!(
                    "check_candidates: getaddrinfo failed for {}:{}",
                    candidate.addr,
                    candidate.port
                );
                continue;
            };
            resolved.set_port(candidate.port);
            addrs[i] = Some(resolved);
            match resolved {
                SocketAddr::V4(_) => {
                    if let Some(v4) = main.ipv4_sock.as_ref() {
                        if sock::send_to(v4, &request_buf[0], resolved).is_err() {
                            tracing::warn!(
                                "check_candidates: Sending request failed for {}:{} (type {:?})",
                                candidate.addr,
                                candidate.port,
                                candidate.type_
                            );
                            continue;
                        }
                    }
                    // C: (type == STATIC && !sent) || type == STUN — `sent` ist an
                    // dieser Stelle im C immer false, daher äquivalent:
                    if main.stun_random_allocation
                        && matches!(
                            candidate.type_,
                            CandidateType::Static | CandidateType::Stun
                        )
                    {
                        for s in socks.iter().flatten() {
                            if sock::send_to(s, &request_buf[0], resolved).is_err() {
                                tracing::warn!(
                                    "check_candidates: Sending request failed for {}:{} with error, closing socket",
                                    candidate.addr,
                                    candidate.port
                                );
                                continue;
                            }
                        }
                    }
                }
                SocketAddr::V6(_) => {
                    if let Some(v6) = main.ipv6_sock.as_ref() {
                        if sock::send_to(v6, &request_buf[0], resolved).is_err() {
                            tracing::warn!(
                                "check_candidates: Sending request failed for {}:{} (type {:?})",
                                candidate.addr,
                                candidate.port,
                                candidate.type_
                            );
                            continue;
                        }
                    }
                }
            }
            failed = false;
        }
        if failed {
            return Err(ChiakiError::Network);
        }

        // Wait for responses
        let poll = PollSockets {
            has_ipv4: main.ipv4_sock.is_some(),
            has_ipv6: main.ipv6_sock.is_some(),
        };


        // Wait-Loop: liefert (sock_id, candidate_idx, responded);
        // sock_id: 0 = ipv4, 1 = ipv6, 2+j = socks[j]
        let result: ChiakiResult<(usize, usize, bool)> = 'outer: {
            let mut selected_sock_id: Option<usize> = None;
            let mut selected_candidate_idx: Option<usize> = None;
            let mut received_response = false;
            let mut responded = false;
            let mut connecting = false;
            let mut retry_counter = 0u32;

            'wait: while selected_candidate_idx.is_none() {
                let timed_out;
                let mut ready: Option<(usize, Vec<u8>, SocketAddr)> = None;

                let interval = if connecting {
                    Duration::from_secs(SELECT_CANDIDATE_CONNECTION_SEC)
                } else {
                    Duration::from_secs_f32(SELECT_CANDIDATE_TIMEOUT_SEC)
                };
                let deadline = Instant::now() + interval;
                while Instant::now() < deadline {
                    self.inner.select_pipe.check()?;
                    if let Some(r) = poll.recv_once(main, &socks)? {
                        ready = Some(r);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                timed_out = ready.is_none();

                if timed_out {
                    if selected_sock_id.is_none() {
                        if retry_counter < SELECT_CANDIDATE_TRIES && !received_response {
                            retry_counter += 1;
                            tracing::info!(
                                "check_candidates: Resending requests to all candidates TRY {}... waiting for 1st response",
                                retry_counter
                            );
                            for (i, addr) in addrs
                                .iter()
                                .enumerate()
                                .take(num_candidates + extra_addresses_used)
                            {
                                let Some(addr) = addr else { continue };
                                let sock_ref: Option<&UdpSocket> = match addr {
                                    SocketAddr::V4(_) => main.ipv4_sock.as_ref(),
                                    SocketAddr::V6(_) => main.ipv6_sock.as_ref(),
                                };
                                let Some(sock_ref) = sock_ref else { continue };
                                if sock::send_to(sock_ref, &request_buf[0], *addr).is_err() {
                                    tracing::error!(
                                        "check_candidates: Sending request failed for {}:{}",
                                        candidates[i].addr,
                                        candidates[i].port
                                    );
                                    continue;
                                }
                            }
                            continue 'wait;
                        } else if received_response && !connecting {
                            connecting = true;
                            continue 'wait;
                        }
                        tracing::error!("check_candidates: Waiting for candidate responses timed out");
                        break 'outer Err(ChiakiError::HostUnreach);
                    }
                    // selected_sock gesetzt: C verlässt hier die Schleife
                    break 'wait;
                }

                let Some((sock_id, data, from)) = ready else {
                    continue 'wait;
                };

                // Probing-Sockets: TTL wieder hochsetzen
                if sock_id >= 2 && main.stun_random_allocation {
                    if let Some(s) = socks.get(sock_id - 2).and_then(|s| s.as_ref()) {
                        if let Err(e) = socket2::SockRef::from(s).set_ttl_v4(64) {
                            tracing::error!("setsockopt(IP_TTL) failed with error {e}");
                            break 'outer Err(ChiakiError::Unknown);
                        }
                    }
                }

                let recv_address_string = from.ip().to_string();
                let recv_address_port = from.port();

                // passenden Kandidaten finden oder als Derived aufnehmen
                let mut found_idx: Option<usize> = None;
                for (i, candidate) in candidates
                    .iter()
                    .enumerate()
                    .take(num_candidates + extra_addresses_used)
                {
                    if candidate.addr == recv_address_string
                        && candidate.port == recv_address_port
                    {
                        found_idx = Some(i);
                        break;
                    }
                }
                let candidate_idx = match found_idx {
                    Some(i) => i,
                    None => {
                        if extra_addresses_used >= EXTRA_CANDIDATE_ADDRESSES {
                            tracing::info!(
                                "check_candidates: Received more than {} extra candidates skipping this one",
                                EXTRA_CANDIDATE_ADDRESSES
                            );
                            continue 'wait;
                        }
                        let i = num_candidates + extra_addresses_used;
                        let candidate = &mut candidates[i];
                        responses_received[i] = 0;
                        candidate.addr = recv_address_string.clone();
                        candidate.port_mapped = 0;
                        candidate.type_ = CandidateType::Derived;
                        candidate.port = recv_address_port;
                        candidate.addr_mapped = if from.is_ipv4() {
                            "0.0.0.0".to_owned()
                        } else {
                            "0:0:0:0:0:0:0:0".to_owned()
                        };
                        addrs[i] = Some(from);
                        extra_addresses_used += 1;
                        tracing::info!(
                            "check_candidates: Received new candidate at {}:{}",
                            candidate.addr,
                            candidate.port
                        );
                        i
                    }
                };
                let candidate = candidates[candidate_idx].clone();
                tracing::trace!(
                    "check_candidates: Received data from {}:{}",
                    candidate.addr,
                    candidate.port
                );

                if data.len() != 88 {
                    if candidate.type_ == CandidateType::Derived {
                        continue 'wait;
                    }
                    tracing::error!(
                        "check_candidates: Received response of unexpected size {} from {}:{}",
                        data.len(),
                        candidate.addr,
                        candidate.port
                    );
                    break 'outer Err(ChiakiError::Network);
                }
                let msg_type = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
                if msg_type == MSG_TYPE_REQ {
                    tracing::info!("Responding to request");
                    responded = true;
                    let send_sock: &UdpSocket = match sock_id {
                        0 => main.ipv4_sock.as_ref().expect("ipv4 sock"),
                        1 => main.ipv6_sock.as_ref().expect("ipv6 sock"),
                        j => socks[j - 2].as_ref().expect("probing sock"),
                    };
                    if let Err(e) = send_responseto_ps(
                        &ctx,
                        send_sock,
                        &data,
                        &candidate,
                        addrs[candidate_idx].unwrap_or(from),
                    ) {
                        if candidate.type_ != CandidateType::Derived {
                            break 'outer Err(e);
                        }
                    }
                    if (main.stun_random_allocation || candidate.type_ == CandidateType::Derived)
                        && responses_received[candidate_idx] == 0
                    {
                        let send_sock: &UdpSocket = match sock_id {
                            0 => main.ipv4_sock.as_ref().expect("ipv4 sock"),
                            1 => main.ipv6_sock.as_ref().expect("ipv6 sock"),
                            j => socks[j - 2].as_ref().expect("probing sock"),
                        };
                        if sock::send_to(
                            send_sock,
                            &request_buf[0],
                            addrs[candidate_idx].unwrap_or(from),
                        )
                        .is_err()
                        {
                            tracing::error!(
                                "check_candidates: Sending request failed for {}:{}",
                                candidate.addr,
                                candidate.port
                            );
                            break 'outer Err(ChiakiError::Network);
                        }
                    }
                    continue 'wait;
                }
                if msg_type != MSG_TYPE_RESP {
                    tracing::error!(
                        "check_candidates: Received response of unexpected type {} from {}:{}",
                        msg_type,
                        candidate.addr,
                        candidate.port
                    );
                    tracing::error!(
                        "check_candidates: Received data:\n{}",
                        crate::hex_dump(&data)
                    );
                    if candidate.type_ == CandidateType::Derived {
                        continue 'wait;
                    }
                    break 'outer Err(ChiakiError::Unknown);
                }
                // TODO: More validation of localHashedIds, sids and the weird data at 0x4b?
                // (CHECK_CANDIDATES_REQUEST_NUMBER == 1 → Index immer 0, wie im C)
                if data[0x4b..0x50] != request_id[0] {
                    tracing::error!(
                        "check_candidates: Received response with unexpected request ID from {}:{}",
                        candidate.addr,
                        candidate.port
                    );
                    tracing::error!(
                        "check_candidates: Request ID expected:\n{}",
                        crate::hex_dump(&request_id[0])
                    );
                    tracing::error!(
                        "check_candidates: Request ID received:\n{}",
                        crate::hex_dump(&data[0x4b..0x50])
                    );
                    tracing::error!(
                        "check_candidates: Full response received:\n{}",
                        crate::hex_dump(&data)
                    );
                    continue 'wait;
                }
                received_response = true;
                responses_received[candidate_idx] += 1;
                let responses = responses_received[candidate_idx];
                tracing::trace!("Received response {}", responses);
                if responses > (CHECK_CANDIDATES_REQUEST_NUMBER as i32 - 1) {
                    selected_sock_id = Some(sock_id);
                    selected_candidate_idx = Some(candidate_idx);
                    // connect (Default-Peer setzen, wie im C)
                    let addr = addrs[candidate_idx].unwrap_or(from);
                    let connect_result = match sock_id {
                        0 => main
                            .ipv4_sock
                            .as_ref()
                            .expect("ipv4 sock")
                            .connect(addr),
                        1 => main
                            .ipv6_sock
                            .as_ref()
                            .expect("ipv6 sock")
                            .connect(addr),
                        j => socks[j - 2].as_ref().expect("probing sock").connect(addr),
                    };
                    if let Err(e) = connect_result {
                        tracing::error!(
                            "check_candidates: Connecting socket failed for {}:{} with error {e}",
                            candidate.addr,
                            candidate.port
                        );
                        break 'outer Err(ChiakiError::Network);
                    }
                    tracing::trace!("Selected Candidate");
                    print_candidate(&candidate);
                    break 'wait;
                } else {
                    let send_sock: &UdpSocket = match sock_id {
                        0 => main.ipv4_sock.as_ref().expect("ipv4 sock"),
                        1 => main.ipv6_sock.as_ref().expect("ipv6 sock"),
                        j => socks[j - 2].as_ref().expect("probing sock"),
                    };
                    // (C: request_buf[responses] — bei N==1 unerreichbar, hier geklemmt)
                    let idx = (responses as usize).min(CHECK_CANDIDATES_REQUEST_NUMBER - 1);
                    if sock::send_to(send_sock, &request_buf[idx], addrs[candidate_idx].unwrap_or(from))
                        .is_err()
                    {
                        tracing::error!(
                            "check_candidates: Sending request failed for {}:{}",
                            candidate.addr,
                            candidate.port
                        );
                        break 'outer Err(ChiakiError::Network);
                    }
                }
            }

            match (selected_sock_id, selected_candidate_idx) {
                (Some(s), Some(c)) => Ok((s, c, responded)),
                (Some(s), None) => Ok((s, 0, responded)),
                _ => Err(ChiakiError::HostUnreach),
            }
        };

        // Nicht gewählte Sockets schließen; den gewählten herausnehmen
        let (sock_id, candidate_idx, responded) = match result {
            Ok(v) => v,
            Err(e) => {
                // cleanup_sockets
                main.ipv4_sock = None;
                main.ipv6_sock = None;
                return Err(e);
            }
        };
        let mut selected_sock = if sock_id == 0 {
            main.ipv4_sock.take()
        } else {
            main.ipv4_sock = None;
            None
        };
        if selected_sock.is_none() {
            selected_sock = if sock_id == 1 {
                main.ipv6_sock.take()
            } else {
                main.ipv6_sock = None;
                None
            };
        }
        if selected_sock.is_none() && sock_id >= 2 {
            selected_sock = socks[sock_id - 2].take();
        }
        for s in socks.iter_mut() {
            s.take(); // Restliche schließen (Drop)
        }
        let Some(selected_sock) = selected_sock else {
            return Err(ChiakiError::Unknown);
        };
        let mut selected_candidate = candidates[candidate_idx].clone();

        match receive_request_send_response_ps(
            &self.inner.select_pipe,
            &ctx,
            &selected_sock,
            &selected_candidate,
            Duration::from_secs(WAIT_RESPONSE_TIMEOUT_SEC),
        ) {
            // C: TIMEOUT ist kein Fehler, wenn wir selbst bereits geantwortet haben
            Err(ChiakiError::Timeout) => {
                if !responded {
                    return Err(ChiakiError::Timeout);
                }
            }
            Err(e) => return Err(e),
            Ok(()) => {}
        }

        selected_candidate.addr_mapped = String::new();
        let mut local = false;
        if selected_candidate.type_ == CandidateType::Derived {
            if selected_candidate.addr.contains('.') {
                if selected_candidate.addr.starts_with("10.")
                    || selected_candidate.addr.starts_with("192.168.")
                {
                    local = true;
                } else {
                    for j in 16..32 {
                        if selected_candidate.addr.starts_with(&format!("172.{j}.")) {
                            local = true;
                            break;
                        }
                    }
                }
            } else {
                let lower = selected_candidate.addr.to_lowercase();
                if lower.starts_with("fc") || lower.starts_with("fd") {
                    local = true;
                }
            }
        }
        if selected_candidate.type_ == CandidateType::Local || local {
            selected_candidate.addr_mapped = local_candidate.addr.clone();
            selected_candidate.port_mapped = local_candidate.port;
        } else {
            selected_candidate.addr_mapped = remote_candidate.addr.clone();
            selected_candidate.port_mapped = remote_candidate.port;
        }

        Ok((selected_sock, selected_candidate))
    }
}

/// Kleiner Poll-Helper über die aktiven Sockets (libevent-Ersatz).
struct PollSockets {
    has_ipv4: bool,
    has_ipv6: bool,
}

impl PollSockets {
    /// Empfängt von genau einem bereiten Socket (nonblocking, Reihenfolge:
    /// ipv4, ipv6, probing socks).
    fn recv_once(
        &self,
        main: &MainState,
        socks: &[Option<UdpSocket>],
    ) -> ChiakiResult<Option<(usize, Vec<u8>, SocketAddr)>> {
        let mut buf = vec![0u8; 2048];
        if self.has_ipv4 {
            if let Some(s) = main.ipv4_sock.as_ref() {
                if let Ok((n, from)) = s.recv_from(&mut buf) {
                    return Ok(Some((0, buf[..n].to_vec(), from)));
                }
            }
        }
        if self.has_ipv6 {
            if let Some(s) = main.ipv6_sock.as_ref() {
                if let Ok((n, from)) = s.recv_from(&mut buf) {
                    return Ok(Some((1, buf[..n].to_vec(), from)));
                }
            }
        }
        for (j, s) in socks.iter().enumerate() {
            let Some(s) = s.as_ref() else { continue };
            if let Ok((n, from)) = s.recv_from(&mut buf) {
                return Ok(Some((2 + j, buf[..n].to_vec(), from)));
            }
        }
        Ok(None)
    }
}

fn resolve_candidate_addr(candidate: &Candidate) -> Option<SocketAddr> {
    use std::net::ToSocketAddrs;
    (candidate.addr.as_str(), candidate.port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut it| it.next())
}

/// Port von `send_response_ps()` (über verbundenen Socket).
fn send_response_ps(
    ctx: &PsHandshakeCtx,
    sock_: &UdpSocket,
    req: &[u8],
    candidate: &Candidate,
) -> ChiakiResult<()> {
    let confirm_buf = build_confirm_buf(ctx, req, candidate)?;
    sock_.send(&confirm_buf).map_err(|e| {
        tracing::error!(
            "check_candidates: Sending confirmation failed for {}:{} with error: {e}",
            candidate.addr,
            candidate.port
        );
        ChiakiError::Network
    })?;
    tracing::info!("Sent response to {}:{}", candidate.addr, candidate.port);
    Ok(())
}

/// Port von `send_responseto_ps()` (sendto an explizite Adresse).
fn send_responseto_ps(
    ctx: &PsHandshakeCtx,
    sock_: &UdpSocket,
    req: &[u8],
    candidate: &Candidate,
    addr: SocketAddr,
) -> ChiakiResult<()> {
    let confirm_buf = build_confirm_buf(ctx, req, candidate)?;
    sock::send_to(sock_, &confirm_buf, addr).map_err(|e| {
        tracing::error!(
            "check_candidates: Sending confirmation failed for {}:{} with error: {e:?}",
            candidate.addr,
            candidate.port
        );
        e
    })?;
    tracing::info!("Sent response to {}:{}", candidate.addr, candidate.port);
    Ok(())
}

/// Gemeinsamer Aufbau des 88-Byte-Confirm-Buffers (aus send_response_ps /
/// send_responseto_ps).
fn build_confirm_buf(
    ctx: &PsHandshakeCtx,
    req: &[u8],
    candidate: &Candidate,
) -> ChiakiResult<[u8; 88]> {
    let mut confirm_buf = [0u8; 88];
    confirm_buf[0x00..0x04].copy_from_slice(&MSG_TYPE_RESP.to_be_bytes());
    confirm_buf[0x04..0x18].copy_from_slice(&ctx.hashed_id_local);
    confirm_buf[0x24..0x38].copy_from_slice(&ctx.hashed_id_console);
    confirm_buf[0x44..0x46].copy_from_slice(&ctx.sid_local.to_be_bytes());
    confirm_buf[0x46..0x48].copy_from_slice(&ctx.sid_console.to_be_bytes());
    confirm_buf[0x4b..0x50].copy_from_slice(&req[0x4b..0x50]);

    let ip: IpAddr = candidate.addr.parse().map_err(|_| {
        tracing::error!("{}: inet_pton failed", candidate.addr);
        ChiakiError::InvalidData
    })?;
    let mut console_addr = [0u8; 16];
    match ip {
        IpAddr::V4(v4) => {
            console_addr[..4].copy_from_slice(&v4.octets());
        }
        IpAddr::V6(v6) => {
            console_addr.copy_from_slice(&v6.octets());
        }
    }
    let console_port = candidate.port.to_be_bytes();
    confirm_buf[0x50..0x52].copy_from_slice(&ctx.sid_local.to_be_bytes());
    confirm_buf[0x52..0x54].copy_from_slice(&ctx.sid_console.to_be_bytes());
    confirm_buf[0x54..0x56].copy_from_slice(&ctx.sid_local.to_be_bytes());
    // xor_bytes(&confirm_buf[0x50], console_addr, 4) — im C immer 4 Bytes
    for i in 0..4 {
        confirm_buf[0x50 + i] ^= console_addr[i];
    }
    for i in 0..2 {
        confirm_buf[0x54 + i] ^= console_port[i];
    }
    Ok(confirm_buf)
}

/// Port von `receive_request_send_response_ps()`.
///
/// Receives a request and sends a response buf for that request.
fn receive_request_send_response_ps(
    select_pipe: &StopPipe,
    ctx: &PsHandshakeCtx,
    sock_: &UdpSocket,
    candidate: &Candidate,
    timeout: Duration,
) -> ChiakiResult<()> {
    let mut received = false;
    // Wait for followup request from responsive candidate
    loop {
        select_pipe.check()?;
        let mut req = [0u8; 88];
        let len = match sock::recv_from_timeout(sock_, &mut req, timeout) {
            Ok((n, _)) => n,
            Err(ChiakiError::Timeout) => {
                if received {
                    return Ok(());
                }
                return Err(ChiakiError::Timeout);
            }
            Err(e) => return Err(e),
        };
        if len != 88 {
            tracing::error!(
                "check_candidates: Received request of unexpected size {} from {}:{}",
                len,
                candidate.addr,
                candidate.port
            );
            return Err(ChiakiError::Network);
        }
        let msg_type = u32::from_be_bytes([req[0], req[1], req[2], req[3]]);
        if msg_type == MSG_TYPE_RESP {
            tracing::info!("Received an extra response, ignoring....");
            continue;
        } else if msg_type != MSG_TYPE_REQ {
            tracing::error!(
                "check_candidates: Received response of unexpected type {} from {}:{}",
                msg_type,
                candidate.addr,
                candidate.port
            );
            tracing::error!("check_candidates: Received data:\n{}", crate::hex_dump(&req));
            return Err(ChiakiError::Unknown);
        }
        received = true;
        send_response_ps(ctx, sock_, &req, candidate)?;
    }
}

/// Port von `log_session_state()`.
fn log_session_state(state: u32) {
    let mut state_str = String::from("[");
    let flags: [(u32, &str); 19] = [
        (SESSION_STATE_INIT, " INIT"),
        (SESSION_STATE_WS_OPEN, " WS_OPEN"),
        (SESSION_STATE_DELETED, " DELETED"),
        (SESSION_STATE_CREATED, " CREATED"),
        (SESSION_STATE_STARTED, " STARTED"),
        (SESSION_STATE_CLIENT_JOINED, " CLIENT_JOINED"),
        (SESSION_STATE_DATA_SENT, " DATA_SENT"),
        (SESSION_STATE_CONSOLE_JOINED, " CONSOLE_JOINED"),
        (SESSION_STATE_CUSTOMDATA1_RECEIVED, " CUSTOMDATA1_RECEIVED"),
        (SESSION_STATE_CTRL_OFFER_RECEIVED, " CTRL_OFFER_RECEIVED"),
        (SESSION_STATE_CTRL_OFFER_SENT, " CTRL_OFFER_SENT"),
        (SESSION_STATE_CTRL_CONSOLE_ACCEPTED, " CTRL_CONSOLE_ACCEPTED"),
        (SESSION_STATE_CTRL_CLIENT_ACCEPTED, " CTRL_CLIENT_ACCEPTED"),
        (SESSION_STATE_CTRL_ESTABLISHED, " CTRL_ESTABLISHED"),
        (SESSION_STATE_DATA_OFFER_RECEIVED, " DATA_OFFER_RECEIVED"),
        (SESSION_STATE_DATA_OFFER_SENT, " DATA_OFFER_SENT"),
        (SESSION_STATE_DATA_CONSOLE_ACCEPTED, " DATA_CONSOLE_ACCEPTED"),
        (SESSION_STATE_DATA_CLIENT_ACCEPTED, " DATA_CLIENT_ACCEPTED"),
        (SESSION_STATE_DATA_ESTABLISHED, " DATA_ESTABLISHED"),
    ];
    for (bit, name) in flags {
        if state & bit != 0 {
            state_str.push_str(name);
        }
    }
    state_str.push_str(" ]");
    tracing::trace!("Holepunch session state: {} = {}", state, state_str);
}

/// Port von `decode_customdata1()`.
pub(crate) fn decode_customdata1(customdata1: &str, out: &mut [u8]) -> ChiakiResult<()> {
    let round1 = base64::decode(customdata1.as_bytes())?;
    let round2 = base64::decode(&round1)?;
    let out_len = out.len();
    if round2.len() < out_len {
        return Err(ChiakiError::Unknown);
    }
    if round2.len() > out_len + CUSTOMDATA1_EXTRA_BYTES_MAX {
        tracing::trace!(
            "decode_customdata1: customData1 decoded to {} bytes (max {})",
            round2.len(),
            out_len + CUSTOMDATA1_EXTRA_BYTES_MAX
        );
        return Err(ChiakiError::Unknown);
    }
    if round2.len() > out_len {
        tracing::info!(
            "decode_customdata1: customData1 contains {} extra byte(s); ignoring extras",
            round2.len() - out_len
        );
    }
    out.copy_from_slice(&round2[..out_len]);
    Ok(())
}

/// Port von `parse_notification_type()`.
pub(crate) fn parse_notification_type(json: &serde_json::Value) -> u16 {
    let Some(datatype) = json.get("dataType").and_then(|v| v.as_str()) else {
        tracing::error!("parse_notification_type: JSON does not contain \"datatype\" string field\n");
        return NOTIFICATION_TYPE_UNKNOWN;
    };
    match datatype {
        "psn:sessionManager:sys:remotePlaySession:created" => NOTIFICATION_TYPE_SESSION_CREATED,
        "psn:sessionManager:sys:rps:members:created" => NOTIFICATION_TYPE_MEMBER_CREATED,
        "psn:sessionManager:sys:rps:customData1:updated" => NOTIFICATION_TYPE_CUSTOM_DATA1_UPDATED,
        "psn:sessionManager:sys:rps:sessionMessage:created" => {
            NOTIFICATION_TYPE_SESSION_MESSAGE_CREATED
        }
        "psn:sessionManager:sys:rps:members:deleted" => NOTIFICATION_TYPE_MEMBER_DELETED,
        "psn:sessionManager:sys:remotePlaySession:deleted" => NOTIFICATION_TYPE_SESSION_DELETED,
        _ => {
            tracing::warn!("parse_notification_type: Unknown notification type \"{}\"", datatype);
            tracing::trace!("parse_notification_type: JSON was:\n{}", json);
            NOTIFICATION_TYPE_UNKNOWN
        }
    }
}

/// Port von `session_message_get_payload()`.
///
/// Get the SessionMessage json from the payload field of the message that
/// arrived over the websocket.
pub(crate) fn session_message_get_payload(
    session_message: &serde_json::Value,
) -> ChiakiResult<serde_json::Value> {
    let payload_str = session_message
        .pointer("/body/data/sessionMessage/payload")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            tracing::error!("session_message_get_payload: Failed to get payload string");
            tracing::trace!("{}", session_message);
            ChiakiError::Unknown
        })?;

    let Some(body_pos) = payload_str.find("body=") else {
        tracing::error!("session_message_get_payload: Failed to find body of payload");
        tracing::trace!("{}", payload_str);
        return Err(ChiakiError::Unknown);
    };
    let json = &payload_str[body_pos + 5..];

    // The JSON for a session message is kind of peculiar, as it's sometimes
    // invalid JSON. This happens when there is no value for the
    // `localPeerAddr` field. Instead of the value being `undefined` or the
    // empty object, the field simply doesn't have a value, i.e. the colon is
    // immediately followed by a comma. This obviously breaks our parser, so we
    // fix the JSON if the field value is missing.
    let peeraddr_key = "\"localPeerAddr\":";
    let fixed_json = match json.find(peeraddr_key) {
        None => json.to_owned(),
        Some(start) => {
            let peeraddr_end = start + peeraddr_key.len();
            let next = json[peeraddr_end..].chars().next();
            if next == Some('{') {
                // Valid JSON, we can parse without modifications
                json.to_owned()
            } else {
                // Insert empty object as value for the localPeerAddr key
                format!("{}{{}}{}", &json[..peeraddr_end], &json[peeraddr_end..])
            }
        }
    };

    serde_json::from_str(&fixed_json).map_err(|_| {
        tracing::error!("Couldn't parse the following json: {}", fixed_json);
        ChiakiError::Unknown
    })
}

/// Port von `session_message_parse()`.
pub(crate) fn session_message_parse(
    message_json: &serde_json::Value,
) -> ChiakiResult<SessionMessage> {
    let invalid_schema = || -> ChiakiError {
        tracing::error!("session_message_parse: Unexpected JSON schema for holepunch session message.");
        tracing::trace!("{}", message_json);
        ChiakiError::Unknown
    };

    let action_str = message_json
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or_else(invalid_schema)?;
    let action = match action_str {
        "OFFER" => SESSION_MESSAGE_ACTION_OFFER,
        "ACCEPT" => SESSION_MESSAGE_ACTION_ACCEPT,
        "TERMINATE" => SESSION_MESSAGE_ACTION_TERMINATE,
        "RESULT" => SESSION_MESSAGE_ACTION_RESULT,
        _ => SESSION_MESSAGE_ACTION_UNKNOWN,
    };

    let req_id = message_json
        .get("reqId")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| {
            tracing::error!("Coudln't parse reqid field from message json.");
            invalid_schema()
        })? as u16;

    let error = message_json
        .get("error")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| {
            tracing::error!("Coudln't parse error field from message json.");
            invalid_schema()
        })? as u16;

    let conn_request_json = message_json.get("connRequest").ok_or_else(|| {
        tracing::error!("Coudln't parse connRequest field from message json.");
        invalid_schema()
    })?;
    if !conn_request_json.is_object() {
        tracing::error!("Coudln't parse connRequest field from message json.");
        return Err(invalid_schema());
    }

    let mut msg = SessionMessage {
        action,
        req_id,
        error,
        conn_request: None,
    };

    if conn_request_json.as_object().map(|o| o.len()).unwrap_or(0) > 0 {
        let get = |key: &str| conn_request_json.get(key);
        let mut conn_request = ConnectionRequest::default();

        conn_request.sid = get("sid").and_then(|v| v.as_i64()).ok_or_else(|| {
            tracing::error!("Coudln't parse sid field from connection request json.");
            invalid_schema()
        })? as u32;

        conn_request.peer_sid = get("peerSid")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| {
                tracing::error!("Coudln't parse peer sid field from connection request json.");
                invalid_schema()
            })? as u32;

        let skey_str = get("skey").and_then(|v| v.as_str()).ok_or_else(|| {
            tracing::error!("Coudln't parse skey field from connection request json.");
            invalid_schema()
        })?;
        let skey = base64::decode(skey_str.as_bytes()).map_err(|e| {
            tracing::error!("session_message_parse: Failed to decode skey: '{}'", skey_str);
            e
        })?;
        if skey.len() > 16 {
            return Err(invalid_schema());
        }
        conn_request.skey[..skey.len()].copy_from_slice(&skey);

        conn_request.nat_type = get("natType")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| {
                tracing::error!("Coudln't parse natType field from connection request json.");
                invalid_schema()
            })? as u8;

        let mac_str = get("defaultRouteMacAddr")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                tracing::error!("Coudln't parse defaultRouteMacAddr field from connection request json.");
                invalid_schema()
            })?;
        if mac_str.len() == 17 {
            // Parse MAC address
            let parts: Vec<&str> = mac_str.split(':').collect();
            if parts.len() != 6 {
                return Err(invalid_schema());
            }
            for (i, part) in parts.iter().enumerate() {
                conn_request.default_route_mac_addr[i] =
                    u8::from_str_radix(part, 16).map_err(|_| invalid_schema())?;
            }
        }

        let local_hashed_id_str = get("localHashedId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                tracing::error!("Coudln't parse localHashedId field from connection request json.");
                invalid_schema()
            })?;
        let hashed = base64::decode(local_hashed_id_str.as_bytes()).map_err(|e| {
            tracing::error!(
                "session_message_parse: Failed to decode localHashedId: '{}'",
                local_hashed_id_str
            );
            e
        })?;
        if hashed.len() > 20 {
            return Err(invalid_schema());
        }
        conn_request.local_hashed_id[..hashed.len()].copy_from_slice(&hashed);

        let candidates_json = get("candidate")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                tracing::error!("Coudln't parse candidate field from connection request json.");
                invalid_schema()
            })?;
        for candidate_json in candidates_json {
            let type_str = candidate_json
                .get("type")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    tracing::error!("Coudln't parse type field from candidate json.");
                    invalid_schema()
                })?;
            let addr = candidate_json
                .get("addr")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    tracing::error!("Coudln't parse addr field from candidate json.");
                    invalid_schema()
                })?;
            let mapped_addr = candidate_json
                .get("mappedAddr")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    tracing::error!("Coudln't parse mappedAddr field from candidate json.");
                    invalid_schema()
                })?;
            let port = candidate_json
                .get("port")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| {
                    tracing::error!("Coudln't parse port field from candidate json.");
                    invalid_schema()
                })? as u16;
            let mapped_port = candidate_json
                .get("mappedPort")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| {
                    tracing::error!("Coudln't parse mapped port field from candidate json.");
                    invalid_schema()
                })? as u16;
            conn_request.candidates.push(Candidate {
                type_: CandidateType::from_str(type_str),
                addr: addr.to_owned(),
                addr_mapped: mapped_addr.to_owned(),
                port,
                port_mapped: mapped_port,
            });
        }

        msg.conn_request = Some(conn_request);
    }

    Ok(msg)
}

/// Port von `print_session_request()`.
fn print_session_request(req: Option<&ConnectionRequest>) {
    let Some(req) = req else { return };
    tracing::trace!("-----------------CONNECTION REQUEST---------------------");
    tracing::trace!("sid: {}", req.sid);
    tracing::trace!("peer_sid: {}", req.peer_sid);
    tracing::trace!("skey: {}", base64::encode(&req.skey));
    tracing::trace!("nat type {}", req.nat_type);
    if req.default_route_mac_addr != [0u8; 6] {
        let mac = &req.default_route_mac_addr;
        tracing::trace!(
            "default_route_mac_addr: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac[0],
            mac[1],
            mac[2],
            mac[3],
            mac[4],
            mac[5]
        );
    }
    if req.local_hashed_id != [0u8; 20] {
        tracing::trace!("local hashed id {}", base64::encode(&req.local_hashed_id));
    }
}

/// Port von `print_candidate()`.
fn print_candidate(candidate: &Candidate) {
    match candidate.type_ {
        CandidateType::Local => tracing::trace!("--------------LOCAL CANDIDATE---------------------"),
        CandidateType::Static => tracing::trace!("--------------REMOTE CANDIDATE--------------------"),
        CandidateType::Derived => tracing::trace!("--------------DERIVED CANDIDATE--------------------"),
        CandidateType::Stun => tracing::trace!("--------------STUN CANDIDATE--------------------"),
    }
    tracing::trace!("Address: {}", candidate.addr);
    tracing::trace!("Mapped Address: {}", candidate.addr_mapped);
    tracing::trace!("Port: {}", candidate.port);
    tracing::trace!("Mapped Port: {}", candidate.port_mapped);
}

/// Port von `random_uuidv4()`.
pub(crate) fn random_uuidv4() -> String {
    use rand::Rng;
    const HEX: &[u8] = b"0123456789abcdef";
    let mut rng = rand::thread_rng();
    let mut out = String::with_capacity(UUIDV4_STR_LEN);
    for i in 0..36 {
        let c = if i == 8 || i == 13 || i == 18 || i == 23 {
            '-'
        } else if i == 14 {
            '4'
        } else if i == 19 {
            HEX[rng.gen_range(0..4) + 8] as char
        } else {
            HEX[rng.gen_range(0..16)] as char
        };
        out.push(c);
    }
    out
}

/// Port der Port-Wraparound-Logik für sequenzielle Allocation-Increments
/// (create_offer, !stun_random_allocation-Zweig).
fn wrap_increment_port(port_check: i32, tmp: i32) -> i32 {
    const UINT16_MAX: i32 = 65535;
    if port_check < 1024 && tmp > 1024 {
        UINT16_MAX - (1024 - port_check)
    } else if port_check < 1 {
        port_check + UINT16_MAX
    } else if port_check > UINT16_MAX {
        port_check - UINT16_MAX + 1024
    } else {
        port_check
    }
}

/// Port der Delta-/Wraparound-Logik für Random-Allocation-Guesses
/// (create_offer, guess-Schleifen).
fn guess_port(base_port: i32, i: i32) -> i32 {
    let delta: i32 = if i == 0 {
        0
    } else if i % 2 == 1 {
        (i + 1) / 2
    } else {
        -(i / 2)
    };
    let port = base_port + delta;
    const UINT16_MAX: i32 = 65535;
    if port > UINT16_MAX {
        49152 + (port - UINT16_MAX - 1)
    } else if port < 1024 {
        UINT16_MAX - (1024 - port)
    } else {
        port
    }
}

// ---------------------------------------------------------------------------
// WebSocket-Thread (Port von websocket_thread_func)
// ---------------------------------------------------------------------------

fn websocket_thread_func(inner: Arc<Inner>) {
    let (fqdn, oauth_token) = {
        let ws_state = inner.ws.lock().unwrap_or_else(|e| e.into_inner());
        let fqdn = ws_state.fqdn.clone().unwrap_or_default();
        (fqdn, inner.psn.oauth_token().to_owned())
    };
    let path = "/np/pushNotification";

    let headers: Vec<(&str, String)> = vec![
        ("Sec-WebSocket-Protocol", "np-pushpacket".to_owned()),
        ("User-Agent", "WebSocket++/0.8.2".to_owned()),
        ("X-PSN-APP-TYPE", "REMOTE_PLAY".to_owned()),
        ("X-PSN-APP-VER", "RemotePlay/1.0".to_owned()),
        ("X-PSN-KEEP-ALIVE-STATUS-TYPE", "3".to_owned()),
        ("X-PSN-OS-VER", "Windows/10.0".to_owned()),
        ("X-PSN-PROTOCOL-VERSION", "2.1".to_owned()),
        ("X-PSN-RECONNECTION", "false".to_owned()),
        ("Authorization", format!("Bearer {}", oauth_token)),
    ];

    let mut client = match ws::WsClient::connect(&fqdn, path, &headers, Duration::from_millis(250))
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(
                "websocket_thread_func: Connecting to push notification WebSocket wss://{}{} failed: {e:?}",
                fqdn,
                path
            );
            let mut ws_state = inner.ws.lock().unwrap_or_else(|e| e.into_inner());
            ws_state.open = false;
            return;
        }
    };
    {
        let mut ws_state = inner.ws.lock().unwrap_or_else(|e| e.into_inner());
        ws_state.open = true;
    }
    tracing::trace!(
        "websocket_thread_func: Connected to push notification WebSocket wss://{}{}",
        fqdn,
        path
    );
    {
        let mut state = inner.state.lock().unwrap_or_else(|e| e.into_inner());
        *state |= SESSION_STATE_WS_OPEN;
        inner.state_cond.notify_all();
    }

    // Need to send a ping every 5secs
    let ping_interval = Duration::from_secs(WEBSOCKET_PING_INTERVAL_SEC);
    let mut last_ping_sent = Instant::now();
    let mut expecting_pong = false;

    loop {
        {
            let flags = inner.stop.lock().unwrap_or_else(|e| e.into_inner());
            if flags.ws_thread_should_stop {
                break;
            }
        }

        let now = Instant::now();
        if expecting_pong && now - last_ping_sent > ping_interval {
            tracing::error!("websocket_thread_func: Did not receive PONG in time.");
            break;
        }
        if now - last_ping_sent > ping_interval {
            if let Err(e) = client.send_ping() {
                tracing::error!("websocket_thread_func: Sending WebSocket PING failed: {e:?}");
                break;
            }
            tracing::trace!("websocket_thread_func: PING.");
            last_ping_sent = now;
            expecting_pong = true;
        }

        match client.recv() {
            Err(e) => {
                tracing::error!("websocket_thread_func: Receiving WebSocket frame failed: {e:?}");
                break;
            }
            Ok(None) => {
                continue;
            }
            Ok(Some(ws::Frame::Pong(_))) => {
                tracing::trace!("websocket_thread_func: Received PONG.");
                expecting_pong = false;
            }
            Ok(Some(ws::Frame::Ping(data))) => {
                tracing::trace!("websocket_thread_func: Received PING.");
                if let Err(e) = client.send_pong(&data) {
                    tracing::error!("websocket_thread_func: Sending WebSocket PONG failed: {e:?}");
                    break;
                }
                tracing::trace!("websocket_thread_func: Sent PONG.");
            }
            Ok(Some(ws::Frame::Close(_))) => {
                tracing::error!("websocket_thread_func: WebSocket closed");
                break;
            }
            Ok(Some(ws::Frame::Text(payload)))
            | Ok(Some(ws::Frame::Binary(payload))) => {
                tracing::trace!(
                    "websocket_thread_func: Received WebSocket frame with {} bytes of payload.",
                    payload.len()
                );
                let Ok(json) = serde_json::from_slice::<serde_json::Value>(&payload) else {
                    tracing::error!("websocket_thread_func: Parsing JSON from payload failed");
                    tracing::trace!(
                        "websocket_thread_func: Payload was:\n{}",
                        String::from_utf8_lossy(&payload)
                    );
                    continue;
                };
                tracing::trace!("websocket_thread_func: JSON:\n{}", json);

                let type_ = parse_notification_type(&json);

                // Automatically ACK OFFER session messages if we're not currently
                // explicitly waiting on offers
                let should_ack_offers = {
                    let state = *inner.state.lock().unwrap_or_else(|e| e.into_inner());
                    // We're not expecting any offers after receiving one for the control port and before it's established, afterwards we expect
                    // one for the data port, so we don't auto-ACK in between
                    (state & SESSION_STATE_CTRL_OFFER_RECEIVED != 0
                        && state & SESSION_STATE_CTRL_ESTABLISHED == 0)
                        // At this point all offers were received and we don't care for new ones anymore
                        || state & SESSION_STATE_DATA_OFFER_RECEIVED != 0
                };
                if should_ack_offers && type_ == NOTIFICATION_TYPE_SESSION_MESSAGE_CREATED {
                    let ack_result = (|| -> ChiakiResult<()> {
                        let payload = session_message_get_payload(&json)?;
                        let msg = session_message_parse(&payload)?;
                        if msg.action == SESSION_MESSAGE_ACTION_OFFER {
                            let ack_msg = SessionMessage {
                                action: SESSION_MESSAGE_ACTION_RESULT,
                                req_id: msg.req_id,
                                error: 0,
                                conn_request: None,
                            };
                            send_session_message_from_thread(&inner, &ack_msg)?;
                        }
                        Ok(())
                    })();
                    if let Err(e) = ack_result {
                        tracing::error!(
                            "websocket_thread_func: Failed to parse session message for ACKing: {e:?}"
                        );
                        continue;
                    }
                }

                {
                    let mut queue = inner.notif.lock().unwrap_or_else(|e| e.into_inner());
                    queue.queue.push_back(Notification { type_, json });
                    inner.notif_cond.notify_all();
                }
                if type_ == NOTIFICATION_TYPE_SESSION_DELETED {
                    tracing::info!(
                        "websocket_thread_func: Holepunch session was deleted on PSN server, exiting...."
                    );
                    break;
                }
            }
        }
    }

    let mut ws_state = inner.ws.lock().unwrap_or_else(|e| e.into_inner());
    ws_state.open = false;
}

/// http_send_session_message aus dem WS-Thread heraus (kurze ACK-Messages).
fn send_session_message_from_thread(
    inner: &Arc<Inner>,
    message: &SessionMessage,
) -> ChiakiResult<()> {
    let payload = psn::session_message_json(
        psn::action_str(message.action),
        message.req_id,
        message.error,
        "{}",
    );
    let (account_id, console_uid, console_type, session_id) = {
        let main = inner.main.lock().unwrap_or_else(|e| e.into_inner());
        (
            main.account_id,
            main.console_uid,
            main.console_type,
            main.session_id.clone(),
        )
    };
    inner
        .psn
        .send_session_message(&session_id, &console_uid, console_type, account_id, &payload)
}

// ---------------------------------------------------------------------------
// Mini-UPnP-IGD-Client (Ersatz für miniupnpc)
// ---------------------------------------------------------------------------

mod upnp {
    //! Minimaler UPnP-IGD-Client (SSDP-Discovery + SOAP/WANIPConnection),
    //! 1:1 an den miniupnpc-Aufrufstellen in holepunch.c:
    //! upnpDiscover → UPNP_GetValidIGD, UPNP_GetExternalIPAddress,
    //! UPNP_AddPortMapping, UPNP_DeletePortMapping.

    use std::net::UdpSocket;
    use std::time::{Duration, Instant};

    use chiaki_core::error::{ChiakiError, ChiakiResult};

    /// Port von `UPNPGatewayInfo` (lan_ip + URLs/Daten zu einem IGD-Handle).
    #[derive(Debug, Clone, Default)]
    pub struct GatewayInfo {
        pub lan_ip: String,
        pub control_url: String,
        pub service_type: String,
    }

    /// Retrieves the IP address on the local network of the client.
    ///
    /// C (Windows): GetAdaptersInfo, Ethernet bevorzugt vor WLAN. Hier ohne
    /// unsafe: UDP-Socket zur Default-Route "verbinden" und lokale Adresse
    /// lesen (kein Paket wird gesendet).
    pub fn get_client_addr_local(out: &mut String) -> ChiakiResult<()> {
        let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| {
            tracing::error!("get_client_addr_local: bind failed: {e}");
            ChiakiError::Network
        })?;
        // 8.8.8.8:53 — connect sendet nichts, wählt nur die Route
        sock.connect("8.8.8.8:53").map_err(|_| ChiakiError::Network)?;
        let addr = sock.local_addr().map_err(|_| ChiakiError::Network)?;
        if addr.ip().is_loopback() || !addr.ip().is_ipv4() {
            tracing::error!("Couldn't find a valid local address!");
            return Err(ChiakiError::Network);
        }
        *out = addr.ip().to_string();
        Ok(())
    }

    /// SSDP M-SEARCH (upnpDiscover-Ersatz) + IGD-Beschreibung auslesen
    /// (UPNP_GetValidIGD-Ersatz).
    pub fn discover(timeout: Duration) -> ChiakiResult<GatewayInfo> {
        let sock = UdpSocket::bind("0.0.0.0:0").map_err(|_| ChiakiError::Network)?;
        let msearch = "M-SEARCH * HTTP/1.1\r\n\
                       HOST: 239.255.255.250:1900\r\n\
                       MAN: \"ssdp:discover\"\r\n\
                       MX: 2\r\n\
                       ST: upnp:rootdevice\r\n\r\n";
        let multicast: std::net::SocketAddr = "239.255.255.250:1900"
            .parse()
            .expect("multicast addr");
        sock.send_to(msearch.as_bytes(), multicast)
            .map_err(|_| ChiakiError::Network)?;

        sock.set_read_timeout(Some(Duration::from_millis(200)))
            .map_err(|_| ChiakiError::Network)?;
        let mut locations: Vec<String> = Vec::new();
        let deadline = Instant::now() + timeout;
        let mut buf = [0u8; 2048];
        while Instant::now() < deadline {
            match chiaki_core::sock::recv_from(&sock, &mut buf) {
                Ok((n, _)) => {
                    let resp = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                    if let Some(pos) = resp.find("location:") {
                        let rest = resp[pos + "location:".len()..].trim();
                        let end = rest
                            .find(|c: char| c == '\r' || c == '\n')
                            .unwrap_or(rest.len());
                        let loc = rest[..end].to_owned();
                        if !loc.is_empty() && !locations.contains(&loc) {
                            locations.push(loc);
                        }
                    }
                }
                Err(ChiakiError::Timeout) => continue,
                Err(_) => break,
            }
        }

        if locations.is_empty() {
            tracing::info!("Failed to find UPnP-capable devices on network");
            return Err(ChiakiError::Network);
        }

        // IGD suchen: erstes Gerät mit WANIPConnection/WANPPPConnection-Service
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(3))
            .build();
        for location in &locations {
            let Ok(xml) = agent
                .get(location)
                .call()
                .map(|r| r.into_string().unwrap_or_default())
            else {
                continue;
            };
            if let Some(gw) = parse_device_description(&xml, location) {
                return Ok(gw);
            }
        }
        tracing::info!("Failed to discover internet gateway via UPnP");
        Err(ChiakiError::Network)
    }

    /// Sucht `<tag>...</tag>` case-insensitiv, schneidet aber im Original
    /// (Byte-Offsets bleiben dank ASCII-Lowercase stabil).
    fn find_tag_ci(hay: &str, tag: &str) -> Option<String> {
        let lower = hay.to_ascii_lowercase();
        let open = format!("<{}>", tag);
        let close = format!("</{}>", tag);
        let start = lower.find(&open)? + open.len();
        let end = lower[start..].find(&close)? + start;
        Some(hay[start..end].trim().to_owned())
    }

    /// Sehr kleiner XML-Scanner: findet den Service mit
    /// WANIPConnection/WANPPPConnection und dessen controlURL.
    fn parse_device_description(xml: &str, base_url: &str) -> Option<GatewayInfo> {
        let mut lan_ip = String::new();
        get_client_addr_local(&mut lan_ip).ok()?;

        for service in xml.split("<service>") {
            let Some(service_type) = find_tag_ci(service, "servicetype") else {
                continue;
            };
            let st_lower = service_type.to_ascii_lowercase();
            if !(st_lower.contains("wanipconnection") || st_lower.contains("wanpppconnection")) {
                continue;
            }
            let control_url = find_tag_ci(service, "controlurl")?;
            // relative URL auflösen
            let control_url = if control_url.starts_with("http://")
                || control_url.starts_with("https://")
            {
                control_url
            } else if control_url.starts_with('/') {
                let p = base_url.find("://")?;
                let after = &base_url[p + 3..];
                let host_end = after
                    .find('/')
                    .map(|i| p + 3 + i)
                    .unwrap_or(base_url.len());
                format!(
                    "{}/{}",
                    &base_url[..host_end],
                    control_url.trim_start_matches('/')
                )
            } else {
                // relativ zum Verzeichnis der Beschreibungs-URL
                let dir = base_url.rsplit_once('/').map(|(d, _)| d).unwrap_or(base_url);
                format!("{}/{}", dir, control_url)
            };
            return Some(GatewayInfo {
                lan_ip,
                control_url,
                service_type,
            });
        }
        None
    }

    fn soap_request(
        agent: &ureq::Agent,
        gw: &GatewayInfo,
        action: &str,
        args: &str,
    ) -> ChiakiResult<String> {
        let body = format!(
            "<?xml version=\"1.0\"?>\
             <SOAP-ENV:Envelope xmlns:SOAP-ENV=\"http://schemas.xmlsoap.org/soap/envelope/\" \
             SOAP-ENV:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
             <SOAP-ENV:Body><m:{action} xmlns:m=\"{service}\">{args}</m:{action}>\
             </SOAP-ENV:Body></SOAP-ENV:Envelope>",
            action = action,
            service = gw.service_type,
            args = args
        );
        let resp = agent
            .post(&gw.control_url)
            .set("SOAPACTION", format!("\"{}#{}\"", gw.service_type, action).as_str())
            .set("Content-Type", "text/xml; charset=\"utf-8\"")
            .send_string(&body);
        match resp {
            Ok(r) => {
                let code = r.status();
                let text = r.into_string().unwrap_or_default();
                if code != 200 {
                    tracing::error!("UPNP error {}: HTTP {} — {}", action, code, text);
                    return Err(ChiakiError::HttpNonok);
                }
                Ok(text)
            }
            Err(ureq::Error::Status(code, r)) => {
                let text = r.into_string().unwrap_or_default();
                tracing::error!("UPNP error {}: HTTP {} — {}", action, code, text);
                Err(ChiakiError::HttpNonok)
            }
            Err(_) => Err(ChiakiError::Network),
        }
    }

    /// Port von `get_client_addr_remote_upnp()` / UPNP_GetExternalIPAddress.
    pub fn get_external_address(agent: &ureq::Agent, gw: &GatewayInfo) -> ChiakiResult<String> {
        let resp = soap_request(agent, gw, "GetExternalIPAddress", "")?;
        find_tag_ci(&resp, "newexternalipaddress").ok_or(ChiakiError::Unknown)
    }

    /// Port von `upnp_add_udp_port_mapping()` / UPNP_AddPortMapping.
    pub fn add_port_mapping(
        agent: &ureq::Agent,
        gw: &GatewayInfo,
        port_internal: u16,
        port_external: u16,
    ) -> ChiakiResult<()> {
        let args = format!(
            "<NewRemoteHost></NewRemoteHost>\
             <NewExternalPort>{}</NewExternalPort>\
             <NewProtocol>UDP</NewProtocol>\
             <NewInternalPort>{}</NewInternalPort>\
             <NewInternalClient>{}</NewInternalClient>\
             <NewEnabled>1</NewEnabled>\
             <NewPortMappingDescription>Chiaki Streaming</NewPortMappingDescription>\
             <NewLeaseDuration>0</NewLeaseDuration>",
            port_external, port_internal, gw.lan_ip
        );
        soap_request(agent, gw, "AddPortMapping", &args)?;
        Ok(())
    }

    /// Port von `upnp_delete_udp_port_mapping()` / UPNP_DeletePortMapping.
    pub fn delete_port_mapping(
        agent: &ureq::Agent,
        gw: &GatewayInfo,
        port_external: u16,
    ) -> ChiakiResult<()> {
        let args = format!(
            "<NewRemoteHost></NewRemoteHost>\
             <NewExternalPort>{}</NewExternalPort>\
             <NewProtocol>UDP</NewProtocol>",
            port_external
        );
        soap_request(agent, gw, "DeletePortMapping", &args)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Minimaler RFC-6455-WebSocket-Client über rustls (Ersatz für libcurl-WS)
// ---------------------------------------------------------------------------

mod ws {
    //! Minimaler blocking-WebSocket-Client (RFC 6455) über rustls — genau so
    //! viel Protokoll, wie websocket_thread_func() braucht: Handshake,
    //! PING/PONG, TEXT/BINARY-Empfang, Maskierung für Client-Frames.

    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::Arc;
    use std::time::Duration;

    use super::WEBSOCKET_MAX_FRAME_SIZE;
    use chiaki_core::base64;
    use chiaki_core::error::{ChiakiError, ChiakiResult};
    use sha1::{Digest, Sha1};

    const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    const OP_CONT: u8 = 0x0;
    const OP_TEXT: u8 = 0x1;
    const OP_BINARY: u8 = 0x2;
    const OP_CLOSE: u8 = 0x8;
    const OP_PING: u8 = 0x9;
    const OP_PONG: u8 = 0xA;

    /// Empfangener WebSocket-Frame (Server → Client, unmaskiert).
    #[derive(Debug, Clone)]
    #[allow(dead_code)] // Pong/Close-Payloads werden nur protokolliert gebraucht
    pub enum Frame {
        Text(Vec<u8>),
        Binary(Vec<u8>),
        Ping(Vec<u8>),
        Pong(Vec<u8>),
        Close(Vec<u8>),
    }

    pub struct WsClient {
        tls: rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
        buf: Vec<u8>, // bereits gelesene, noch nicht verarbeitete Bytes
        fragments: Vec<u8>,
        fragment_op: u8,
    }

    enum Fill {
        Data,
        WouldBlock,
        Eof,
        Err,
    }

    impl WsClient {
        /// Verbindung + Upgrade-Handshake.
        ///
        /// @param fqdn Host (ohne Port/Schema)
        /// @param path Request-Pfad (z. B. "/np/pushNotification")
        pub fn connect(
            fqdn: &str,
            path: &str,
            extra_headers: &[(&str, String)],
            poll_interval: Duration,
        ) -> ChiakiResult<WsClient> {
            let tcp = TcpStream::connect((fqdn, 443)).map_err(|e| {
                tracing::error!("ws: TCP connect to {} failed: {e}", fqdn);
                ChiakiError::Network
            })?;
            tcp.set_read_timeout(Some(poll_interval))
                .map_err(|_| ChiakiError::Network)?;
            tcp.set_write_timeout(Some(Duration::from_secs(10)))
                .map_err(|_| ChiakiError::Network)?;

            let roots = rustls::RootCertStore::from_iter(
                webpki_roots::TLS_SERVER_ROOTS.iter().cloned(),
            );
            let config = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let server_name = rustls::pki_types::ServerName::try_from(fqdn.to_owned())
                .map_err(|_| ChiakiError::ParseAddr)?;
            let conn = rustls::ClientConnection::new(Arc::new(config), server_name)
                .map_err(|_| ChiakiError::Network)?;
            let mut tls = rustls::StreamOwned::new(conn, tcp);

            // Handshake-Request (curl_ws-Äquivalent mit denselben Headern)
            let mut key_bytes = [0u8; 16];
            chiaki_core::random::random_bytes(&mut key_bytes);
            let key = base64::encode(&key_bytes);
            let mut req = format!(
                "GET {path} HTTP/1.1\r\n\
                 Host: {fqdn}\r\n\
                 Upgrade: websocket\r\n\
                 Connection: Upgrade\r\n\
                 Sec-WebSocket-Key: {key}\r\n\
                 Sec-WebSocket-Version: 13\r\n"
            );
            for (k, v) in extra_headers {
                req.push_str(&format!("{}: {}\r\n", k, v));
            }
            req.push_str("\r\n");
            tls.write_all(req.as_bytes())
                .map_err(|_| ChiakiError::Network)?;
            tls.flush().map_err(|_| ChiakiError::Network)?;

            // Antwort bis \r\n\r\n lesen
            let mut resp = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let n = tls.read(&mut chunk).map_err(|e| {
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut
                    {
                        ChiakiError::Timeout
                    } else {
                        ChiakiError::Network
                    }
                })?;
                if n == 0 {
                    tracing::error!("ws: connection closed during handshake");
                    return Err(ChiakiError::Disconnected);
                }
                resp.extend_from_slice(&chunk[..n]);
                if resp.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if resp.len() > 16 * 1024 {
                    tracing::error!("ws: handshake response too large");
                    return Err(ChiakiError::InvalidResponse);
                }
            }

            let text = String::from_utf8_lossy(&resp);
            let head = text.split("\r\n\r\n").next().unwrap_or("");
            let mut lines = head.lines();
            let status = lines.next().unwrap_or("");
            if !status.contains(" 101 ") {
                tracing::error!("ws: websocket upgrade failed: {}", status);
                return Err(ChiakiError::InvalidResponse);
            }
            // Sec-WebSocket-Accept prüfen (RFC 6455)
            let mut hasher = Sha1::new();
            hasher.update(key.as_bytes());
            hasher.update(WS_GUID.as_bytes());
            let expected = base64::encode(&hasher.finalize());
            let mut accept_ok = false;
            for line in lines {
                if let Some((k, v)) = line.split_once(':') {
                    if k.trim().eq_ignore_ascii_case("sec-websocket-accept")
                        && v.trim() == expected
                    {
                        accept_ok = true;
                    }
                }
            }
            if !accept_ok {
                tracing::error!("ws: Sec-WebSocket-Accept mismatch");
                return Err(ChiakiError::InvalidResponse);
            }

            // bereits mitgelesene Frame-Bytes (nach dem Head) behalten
            let head_len = text.find("\r\n\r\n").map(|p| p + 4).unwrap_or(resp.len());
            let buf = resp[head_len.min(resp.len())..].to_vec();

            Ok(WsClient {
                tls,
                buf,
                fragments: Vec::new(),
                fragment_op: 0,
            })
        }

        /// Sendet einen maskierten Client-Frame.
        pub fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> ChiakiResult<()> {
            let mut frame = Vec::with_capacity(payload.len() + 14);
            frame.push(0x80 | opcode); // FIN
            let len = payload.len();
            use rand::RngCore;
            if len < 126 {
                frame.push(0x80 | len as u8);
            } else if len <= u16::MAX as usize {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(len as u16).to_be_bytes());
            } else {
                frame.push(0x80 | 127);
                frame.extend_from_slice(&(len as u64).to_be_bytes());
            }
            let mut mask = [0u8; 4];
            rand::thread_rng().fill_bytes(&mut mask);
            frame.extend_from_slice(&mask);
            for (i, b) in payload.iter().enumerate() {
                frame.push(b ^ mask[i % 4]);
            }
            self.tls.write_all(&frame).map_err(|_| ChiakiError::Network)?;
            self.tls.flush().map_err(|_| ChiakiError::Network)?;
            Ok(())
        }

        pub fn send_ping(&mut self) -> ChiakiResult<()> {
            self.send_frame(OP_PING, &[])
        }

        pub fn send_pong(&mut self, payload: &[u8]) -> ChiakiResult<()> {
            self.send_frame(OP_PONG, payload)
        }

        /// Liest TLS-Daten in den lokalen Puffer.
        fn fill(&mut self) -> Fill {
            let mut chunk = [0u8; 4096];
            match self.tls.read(&mut chunk) {
                Ok(0) => Fill::Eof,
                Ok(n) => {
                    self.buf.extend_from_slice(&chunk[..n]);
                    Fill::Data
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    Fill::WouldBlock
                }
                Err(_) => Fill::Err,
            }
        }

        fn parse_one_frame(&mut self) -> Option<Frame> {
            if self.buf.len() < 2 {
                return None;
            }
            let b0 = self.buf[0];
            let fin = b0 & 0x80 != 0;
            let opcode = b0 & 0x0F;
            let masked = self.buf[1] & 0x80 != 0;
            let len7 = (self.buf[1] & 0x7F) as usize;
            let mut pos = 2usize;
            let payload_len = match len7 {
                126 => {
                    if self.buf.len() < pos + 2 {
                        return None;
                    }
                    let l = u16::from_be_bytes([self.buf[pos], self.buf[pos + 1]]) as usize;
                    pos += 2;
                    l
                }
                127 => {
                    if self.buf.len() < pos + 8 {
                        return None;
                    }
                    let mut l = [0u8; 8];
                    l.copy_from_slice(&self.buf[pos..pos + 8]);
                    pos += 8;
                    u64::from_be_bytes(l) as usize
                }
                l => l,
            };
            let mask_len = if masked { 4 } else { 0 };
            if self.buf.len() < pos + mask_len + payload_len {
                // Schutz vor unsinnig großen Frames (C: WEBSOCKET_MAX_FRAME_SIZE)
                if payload_len > WEBSOCKET_MAX_FRAME_SIZE * 4 {
                    tracing::error!("ws: frame too large: {}", payload_len);
                    return Some(Frame::Close(Vec::new()));
                }
                return None;
            }
            let mask: [u8; 4] = if masked {
                let m = [
                    self.buf[pos],
                    self.buf[pos + 1],
                    self.buf[pos + 2],
                    self.buf[pos + 3],
                ];
                pos += 4;
                m
            } else {
                [0; 4]
            };
            let mut payload = self.buf[pos..pos + payload_len].to_vec();
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= mask[i % 4];
            }
            self.buf.drain(..pos + payload_len);

            if !fin {
                // Fragmentierung: einsammeln
                if self.fragment_op == 0 {
                    self.fragment_op = opcode;
                }
                self.fragments.extend_from_slice(&payload);
                return None; // noch nicht komplett
            }
            let opcode = if !self.fragments.is_empty() || self.fragment_op != 0 {
                let op = if self.fragment_op != 0 {
                    self.fragment_op
                } else {
                    opcode
                };
                self.fragments.extend_from_slice(&payload);
                payload = std::mem::take(&mut self.fragments);
                self.fragment_op = 0;
                op
            } else {
                opcode
            };
            Some(match opcode {
                OP_TEXT => Frame::Text(payload),
                OP_BINARY => Frame::Binary(payload),
                OP_PING => Frame::Ping(payload),
                OP_PONG => Frame::Pong(payload),
                OP_CLOSE => Frame::Close(payload),
                OP_CONT => Frame::Text(payload), // sollte nicht eintreten
                _ => Frame::Close(Vec::new()),
            })
        }

        /// Empfangt den nächsten Frame oder `Ok(None)` beim Poll-Timeout.
        pub fn recv(&mut self) -> ChiakiResult<Option<Frame>> {
            loop {
                if let Some(frame) = self.parse_one_frame() {
                    return Ok(Some(frame));
                }
                match self.fill() {
                    Fill::Data => continue,
                    Fill::WouldBlock => return Ok(None),
                    Fill::Eof => {
                        tracing::error!("ws: connection closed by remote");
                        return Err(ChiakiError::Disconnected);
                    }
                    Fill::Err => {
                        tracing::error!("ws: connection error while reading");
                        return Err(ChiakiError::Network);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_c() {
        assert_eq!(SESSION_MESSAGE_ACTION_OFFER, 1);
        assert_eq!(SESSION_MESSAGE_ACTION_RESULT, 4);
        assert_eq!(SESSION_MESSAGE_ACTION_ACCEPT, 8);
        assert_eq!(SESSION_MESSAGE_ACTION_TERMINATE, 16);
        assert_eq!(MSG_TYPE_REQ, 0x06000000);
        assert_eq!(MSG_TYPE_RESP, 0x07000000);
        assert_eq!(RANDOM_ALLOCATION_GUESSES_NUMBER, 75);
        assert_eq!(RANDOM_ALLOCATION_SOCKS_NUMBER, 250);
        assert_eq!(CHECK_CANDIDATES_REQUEST_NUMBER, 1);
        assert_eq!(DUID_PREFIX, "0000000700410080");
        assert_eq!(CHIAKI_DUID_STR_SIZE, 49);
        assert!(!ENABLE_IPV6);
    }

    #[test]
    fn notification_types_match_c() {
        assert_eq!(NOTIFICATION_TYPE_SESSION_CREATED, 1);
        assert_eq!(NOTIFICATION_TYPE_MEMBER_CREATED, 2);
        assert_eq!(NOTIFICATION_TYPE_MEMBER_DELETED, 4);
        assert_eq!(NOTIFICATION_TYPE_CUSTOM_DATA1_UPDATED, 8);
        assert_eq!(NOTIFICATION_TYPE_SESSION_MESSAGE_CREATED, 16);
        assert_eq!(NOTIFICATION_TYPE_SESSION_DELETED, 32);
    }

    #[test]
    fn session_state_bits_match_c() {
        assert_eq!(SESSION_STATE_INIT, 1 << 0);
        assert_eq!(SESSION_STATE_WS_OPEN, 1 << 1);
        assert_eq!(SESSION_STATE_DELETED, 1 << 18);
        assert_eq!(SESSION_STATE_DATA_ESTABLISHED, 1 << 17);
    }

    #[test]
    fn notification_type_parsing() {
        assert_eq!(
            parse_notification_type(&serde_json::json!({
                "dataType": "psn:sessionManager:sys:remotePlaySession:created"
            })),
            NOTIFICATION_TYPE_SESSION_CREATED
        );
        assert_eq!(
            parse_notification_type(&serde_json::json!({
                "dataType": "psn:sessionManager:sys:rps:sessionMessage:created"
            })),
            NOTIFICATION_TYPE_SESSION_MESSAGE_CREATED
        );
        assert_eq!(
            parse_notification_type(&serde_json::json!({"dataType": "whatever"})),
            NOTIFICATION_TYPE_UNKNOWN
        );
        assert_eq!(
            parse_notification_type(&serde_json::json!({})),
            NOTIFICATION_TYPE_UNKNOWN
        );
    }

    fn base64_encode16() -> String {
        chiaki_core::base64::encode(&[1u8; 16])
    }

    fn base64_encode20() -> String {
        chiaki_core::base64::encode(&[2u8; 20])
    }

    #[test]
    fn payload_parsing_valid_localpeeraddr() {
        let conn = serde_json::json!({
            "action": "OFFER",
            "reqId": 5,
            "error": 0,
            "connRequest": {
                "sid": 4000,
                "peerSid": 5000,
                "skey": base64_encode16(),
                "natType": 2,
                "defaultRouteMacAddr": "aa:bb:cc:dd:ee:ff",
                "localHashedId": base64_encode20(),
                "candidate": [
                    {"type": "STUN", "addr": "1.2.3.4", "mappedAddr": "0.0.0.0", "port": 9295, "mappedPort": 0},
                    {"type": "LOCAL", "addr": "192.168.1.2", "mappedAddr": "0.0.0.0", "port": 1000, "mappedPort": 0}
                ]
            }
        })
        .to_string();
        // Als Payload-String verpacken (wie er über die WebSocket kommt)
        let payload = format!("ver=1.0, type=text, body={}", conn);
        let notif = serde_json::json!({
            "body": {"data": {"sessionMessage": {"payload": payload}}}
        });
        let parsed = session_message_get_payload(&notif).expect("payload");
        let msg = session_message_parse(&parsed).expect("message");
        assert_eq!(msg.action, SESSION_MESSAGE_ACTION_OFFER);
        assert_eq!(msg.req_id, 5);
        assert_eq!(msg.error, 0);
        let cr = msg.conn_request.expect("conn request");
        assert_eq!(cr.sid, 4000);
        assert_eq!(cr.peer_sid, 5000);
        assert_eq!(cr.nat_type, 2);
        assert_eq!(
            cr.default_route_mac_addr,
            [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]
        );
        assert_eq!(cr.local_hashed_id, [2u8; 20]);
        assert_eq!(cr.skey, [1u8; 16]);
        assert_eq!(cr.candidates.len(), 2);
        assert_eq!(cr.candidates[0].type_, CandidateType::Stun);
        assert_eq!(cr.candidates[0].addr, "1.2.3.4");
        assert_eq!(cr.candidates[0].port, 9295);
        assert_eq!(cr.candidates[1].type_, CandidateType::Local);
        assert_eq!(cr.candidates[1].addr, "192.168.1.2");
    }

    #[test]
    fn payload_parsing_broken_localpeeraddr_fixed() {
        // Die offizielle App sendet "localPeerAddr":, (ohne Wert) — kaputtes
        // JSON, das der Parser flicken muss.
        let broken = r#"{"action":"OFFER","reqId":1,"error":0,"connRequest":{"sid":1,"peerSid":2,"skey":"AQEBAQEBAQEBAQEBAQEBAQ==","natType":2,"candidate":[],"defaultRouteMacAddr":"","localPeerAddr":,"localHashedId":""}}"#;
        let payload = format!("ver=1.0, type=text, body={}", broken);
        let notif = serde_json::json!({
            "body": {"data": {"sessionMessage": {"payload": payload}}}
        });
        let parsed = session_message_get_payload(&notif).expect("fixed payload");
        let msg = session_message_parse(&parsed).expect("message");
        assert_eq!(msg.action, SESSION_MESSAGE_ACTION_OFFER);
        assert_eq!(msg.req_id, 1);
        let cr = msg.conn_request.expect("conn request");
        assert_eq!(cr.sid, 1);
        assert_eq!(cr.candidates.len(), 0);
    }

    #[test]
    fn serialize_offer_message_structure() {
        let session = HolepunchSession::new("token").expect("session");
        let msg = SessionMessage {
            action: SESSION_MESSAGE_ACTION_OFFER,
            req_id: 2,
            error: 0,
            conn_request: Some(ConnectionRequest {
                sid: 4000,
                peer_sid: 5000,
                skey: [3; 16],
                nat_type: 2,
                candidates: vec![Candidate {
                    type_: CandidateType::Stun,
                    addr: "85.1.2.3".to_owned(),
                    addr_mapped: "0.0.0.0".to_owned(),
                    port: 9295,
                    port_mapped: 0,
                }],
                default_route_mac_addr: [0; 6],
                local_hashed_id: [4; 20],
            }),
        };
        let s = session.session_message_serialize(&msg).expect("serialize");
        // Struktur-Check: alle erwarteten escaped Felder vorhanden
        assert!(s.starts_with("{\\\"action\\\":\\\"OFFER\\\""), "{}", s);
        assert!(s.contains("\\\"reqId\\\":2"));
        assert!(s.contains("\\\"error\\\":0"));
        assert!(s.contains("\\\"sid\\\":4000"));
        assert!(s.contains("\\\"peerSid\\\":5000"));
        assert!(s.contains("\\\"natType\\\":2"));
        assert!(s.contains("\\\"type\\\":\\\"STUN\\\""));
        assert!(s.contains("\\\"addr\\\":\\\"85.1.2.3\\\""));
        assert!(s.contains("\\\"port\\\":9295"));
        assert!(s.contains("\\\"defaultRouteMacAddr\\\":\\\"\\\""));
        assert!(s.contains(
            "\\\"localPeerAddr\\\":{\\\"accountId\\\":\\\"0\\\",\\\"platform\\\":\\\"REMOTE_PLAY\\\"}"
        ));

        // Roundtrip: das gesamte escaped JSON de-escapen und parsen
        let value: serde_json::Value =
            serde_json::from_str(&unescape_json_string(&s)).expect("roundtrip json");
        let msg2 = session_message_parse(&value).expect("parse");
        assert_eq!(msg2.req_id, 2);
        assert_eq!(msg2.action, SESSION_MESSAGE_ACTION_OFFER);
        let cr = msg2.conn_request.unwrap();
        assert_eq!(cr.sid, 4000);
        assert_eq!(cr.candidates.len(), 1);
        assert_eq!(cr.candidates[0].port, 9295);
        assert_eq!(cr.skey, [3; 16]);
        assert_eq!(cr.local_hashed_id, [4; 20]);
    }

    /// Wandelt den escaped JSON-String zurück in gültiges JSON
    /// (Test-Hilfe: ersetzt `\"` → `"`).
    fn unescape_json_string(s: &str) -> String {
        s.replace("\\\"", "\"")
    }

    #[test]
    fn short_ack_serialization() {
        let msg = SessionMessage {
            action: SESSION_MESSAGE_ACTION_RESULT,
            req_id: 9,
            error: 0,
            conn_request: None,
        };
        let s =
            psn::session_message_json(psn::action_str(msg.action), msg.req_id, msg.error, "{}");
        assert_eq!(
            s,
            "{\\\"action\\\":\\\"RESULT\\\",\\\"reqId\\\":9,\\\"error\\\":0,\\\"connRequest\\\":{}}"
        );
    }

    #[test]
    fn customdata1_decoding() {
        // customData1 = base64(base64(16 Bytes))
        let inner = [0xABu8; 16];
        let round1 = base64::encode(&inner);
        let round2 = base64::encode(round1.as_bytes());
        let mut out = [0u8; 16];
        assert_eq!(decode_customdata1(&round2, &mut out), Ok(()));
        assert_eq!(out, inner);

        // zu kurz
        let mut out2 = [0u8; 16];
        assert_eq!(
            decode_customdata1(&base64::encode(b"short"), &mut out2),
            Err(ChiakiError::Unknown)
        );
    }

    #[test]
    fn uuidv4_format() {
        for _ in 0..16 {
            let u = random_uuidv4();
            assert_eq!(u.len(), 36);
            let bytes = u.as_bytes();
            for &pos in &[8usize, 13, 18, 23] {
                assert_eq!(bytes[pos], b'-', "{}", u);
            }
            assert_eq!(bytes[14], b'4');
            assert!(
                bytes[19] == b'8'
                    || bytes[19] == b'9'
                    || bytes[19] == b'a'
                    || bytes[19] == b'b'
            );
        }
    }

    #[test]
    fn client_device_uid_format() {
        let uid = HolepunchSession::generate_client_device_uid().expect("uid");
        assert_eq!(uid.len(), CHIAKI_DUID_STR_SIZE - 1, "{}", uid);
        assert!(uid.starts_with(DUID_PREFIX));
        assert!(uid[16..]
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    }

    #[test]
    fn guess_port_matches_c_math() {
        // Delta-Sequenz: 0, +1, -1, +2, -2, +3, -3 ...
        let base = 50000;
        assert_eq!(guess_port(base, 0), 50000);
        assert_eq!(guess_port(base, 1), 50001);
        assert_eq!(guess_port(base, 2), 49999);
        assert_eq!(guess_port(base, 3), 50002);
        assert_eq!(guess_port(base, 4), 49998);
        // Wrap oben: > 65535 → 49152 + (port - 65535 - 1)
        assert_eq!(guess_port(65535, 3), 49153);
        // Wrap unten: < 1024 → 65535 - (1024 - port)
        assert_eq!(guess_port(1023, 2), 65535 - (1024 - 1022));
    }

    #[test]
    fn wrap_increment_port_matches_c_math() {
        // skip well known ports 0-1024 unless current allocation is within range
        assert_eq!(wrap_increment_port(1000, 2000), 65535 - (1024 - 1000));
        assert_eq!(wrap_increment_port(0, 100), 65535);
        assert_eq!(wrap_increment_port(65536, 0), 1025);
        assert_eq!(wrap_increment_port(2000, 3000), 2000);
    }

    #[test]
    fn candidate_type_strings() {
        assert_eq!(CandidateType::Static.as_str(), "STATIC");
        assert_eq!(CandidateType::Local.as_str(), "LOCAL");
        assert_eq!(CandidateType::Stun.as_str(), "STUN");
        assert_eq!(CandidateType::Derived.as_str(), "DERIVED");
        assert_eq!(CandidateType::from_str("LOCAL"), CandidateType::Local);
        assert_eq!(CandidateType::from_str("STUN"), CandidateType::Stun);
        assert_eq!(CandidateType::from_str("DERIVED"), CandidateType::Derived);
        assert_eq!(CandidateType::from_str("STATIC"), CandidateType::Static);
        assert_eq!(CandidateType::from_str("whatever"), CandidateType::Static);
    }

    #[test]
    fn confirm_buf_golden() {
        let main = MainState::new_for_test();
        let ctx = PsHandshakeCtx::from_main(&main);
        let req = [0u8; 88];
        let candidate = Candidate {
            type_: CandidateType::Static,
            addr: "192.168.35.100".to_owned(),
            addr_mapped: String::new(),
            port: 9295,
            port_mapped: 0,
        };
        let buf = build_confirm_buf(&ctx, &req, &candidate).expect("buf");
        assert_eq!(buf.len(), 88);
        assert_eq!(&buf[0..4], &MSG_TYPE_RESP.to_be_bytes());
        assert_eq!(&buf[0x04..0x18], &ctx.hashed_id_local);
        assert_eq!(&buf[0x24..0x38], &ctx.hashed_id_console);
        assert_eq!(&buf[0x44..0x46], &ctx.sid_local.to_be_bytes());
        assert_eq!(&buf[0x46..0x48], &ctx.sid_console.to_be_bytes());
        // 0x50: sid_local ^ addr[0..4]  (192=0xC0, 168=0xA8, 35=0x23, 100=0x64)
        let sid_be = ctx.sid_local.to_be_bytes();
        assert_eq!(buf[0x50], sid_be[0] ^ 0xC0);
        assert_eq!(buf[0x51], sid_be[1] ^ 0xA8);
        // Achtung (C-Verhalten): das 4-Byte-Addr-XOR ab 0x50 überlappt die
        // sid_console-Felder bei 0x52/0x53 und überschreibt deren obere Bytes
        assert_eq!(buf[0x52], ctx.sid_console.to_be_bytes()[0] ^ 0x23);
        assert_eq!(buf[0x53], ctx.sid_console.to_be_bytes()[1] ^ 0x64);
        let port_be = 9295u16.to_be_bytes();
        assert_eq!(buf[0x54], sid_be[0] ^ port_be[0]);
        assert_eq!(buf[0x55], sid_be[1] ^ port_be[1]);
    }

    #[test]
    fn request_buf_layout() {
        // Struktur des 88-Byte-Request aus check_candidates
        let mut main = MainState::new_for_test();
        main.hashed_id_local = [0x11; 20];
        main.hashed_id_console = [0x22; 20];
        main.sid_local = 0x1234;
        main.sid_console = 0x5678;
        let ctx = PsHandshakeCtx::from_main(&main);

        let mut request_buf = [0u8; 88];
        let request_id = [0xAAu8; 5];
        request_buf[0x00..0x04].copy_from_slice(&MSG_TYPE_REQ.to_be_bytes());
        request_buf[0x04..0x18].copy_from_slice(&ctx.hashed_id_local);
        request_buf[0x24..0x38].copy_from_slice(&ctx.hashed_id_console);
        request_buf[0x44..0x46].copy_from_slice(&ctx.sid_local.to_be_bytes());
        request_buf[0x46..0x48].copy_from_slice(&ctx.sid_console.to_be_bytes());
        request_buf[0x4b..0x50].copy_from_slice(&request_id);

        assert_eq!(&request_buf[0..4], &[0x06, 0x00, 0x00, 0x00]);
        assert_eq!(&request_buf[0x04..0x18], &[0x11; 20]);
        assert_eq!(&request_buf[0x18..0x24], &[0; 12]);
        assert_eq!(&request_buf[0x24..0x38], &[0x22; 20]);
        assert_eq!(&request_buf[0x44..0x46], &[0x12, 0x34]);
        assert_eq!(&request_buf[0x46..0x48], &[0x56, 0x78]);
        assert_eq!(&request_buf[0x48..0x4b], &[0; 3]);
        assert_eq!(&request_buf[0x4b..0x50], &[0xAA; 5]);
        assert_eq!(&request_buf[0x50..], &[0; 8]);
    }

    #[test]
    fn regist_info_and_getters() {
        let session = HolepunchSession::new("token").expect("session");
        let info = session.regist_info();
        assert_eq!(info.data1.len(), 16);
        assert_eq!(info.data2.len(), 16);
        assert_eq!(info.custom_data1, [0u8; 16]); // noch nicht empfangen
        assert_eq!(session.ps_ctrl_port(), 0);
        assert_eq!(session.stun_allocation(), None); // STUN-Test noch nicht gelaufen
        session.force_port_guessing(true);
        session.set_port_guessing_ports(10);
        session.set_port_guessing_socks(20);
        {
            let main = session.main_lock();
            assert!(main.force_port_guessing);
            assert_eq!(main.port_guessing_count, 10);
            assert_eq!(main.port_guessing_socks, 20);
        }
    }
}
