// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/remote/rudp.c + lib/include/chiaki/remote/rudp.h (chiaki-ng).
//
// "Remote Play over Internet" uses a custom UDP-based protocol named "SCE RUDP"
// for communication between the console and the client for the portions that use
// TCP for a local connection.
//
// Framing: [u16 size (0xC<<12 | len)] [u32 RUDP_CONSTANT] [u16 type] [data...]
//          optional direkt danach: ein Sub-Message im selben Format.
//
// Abweichungen (Rust-Speicherverwaltung):
// - `RudpMessage` besitzt seine Daten (Vec/Box statt malloc/free);
//   `chiaki_rudp_message_pointers_free` entfällt (Drop).
// - Das C `RudpMessage *out`-Parametermuster wird zu Rückgabewerten.
// - `chiaki_stop_pipe_select_single` ohne std-select(): Der Socket wird in
//   kurzen Empfangs-Intervallen gepollt; ein empfangenes Datagramm wird in
//   einem 1-Slot-Stash zwischengespeichert, den select_recv/recv_only zuerst
//   entleeren — beobachtbares Verhalten identisch (select meldet "lesbar",
//   das folgende recv liefert genau dieses Datagramm).
// - Der Socket gehört der Rudp-Instanz (C: roher fd, der in chiaki_rudp_fini
//   geschlossen wird); `fini()`/Drop schließen ihn (Drop des std-Sockets).

use std::net::UdpSocket;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use chiaki_core::error::{ChiakiError, ChiakiResult};
use chiaki_core::random;
use chiaki_core::sock;
use chiaki_core::stoppipe::StopPipe;

use crate::rudpsendbuffer::RudpSendBuffer;

/// RUDP_CONSTANT (rudp.c)
pub const RUDP_CONSTANT: u32 = 0x244F_244F;
/// RUDP_SEND_BUFFER_SIZE (rudp.c). MUST be consistent with the acked seqnums
/// array size in rudp_handle_message_ack()
pub const RUDP_SEND_BUFFER_SIZE: usize = 16;
/// RUDP_EXPECT_TIMEOUT_MS (rudp.c)
pub const RUDP_EXPECT_TIMEOUT_MS: u64 = 1000;

/// Port von `RudpPacketType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum RudpPacketType {
    InitRequest = 0x8030,
    InitResponse = 0xD000,
    CookieRequest = 0x9030,
    CookieResponse = 0xA030,
    SessionMessage = 0x2030,
    StreamConnectionSwitchAck = 0x242E,
    Ack = 0x2430,
    CtrlMessage = 0x0230,
    Unknown = 0x022F,
    Offset8 = 0x1230,
    Offset10 = 0x2630,
    Finish = 0xC000,
}

impl RudpPacketType {
    pub fn value(self) -> u16 {
        self as u16
    }

    pub fn from_u16(v: u16) -> Option<RudpPacketType> {
        Some(match v {
            0x8030 => RudpPacketType::InitRequest,
            0xD000 => RudpPacketType::InitResponse,
            0x9030 => RudpPacketType::CookieRequest,
            0xA030 => RudpPacketType::CookieResponse,
            0x2030 => RudpPacketType::SessionMessage,
            0x242E => RudpPacketType::StreamConnectionSwitchAck,
            0x2430 => RudpPacketType::Ack,
            0x0230 => RudpPacketType::CtrlMessage,
            0x022F => RudpPacketType::Unknown,
            0x1230 => RudpPacketType::Offset8,
            0x2630 => RudpPacketType::Offset10,
            0xC000 => RudpPacketType::Finish,
            _ => return None,
        })
    }

    /// Port von `print_rudp_message_type()` / `GetRudpPacketType()`.
    pub fn name(self) -> &'static str {
        match self {
            RudpPacketType::InitRequest => "Init Request",
            RudpPacketType::InitResponse => "Init Response",
            RudpPacketType::CookieRequest => "Cookie Request",
            RudpPacketType::CookieResponse => "Cookie Response",
            RudpPacketType::SessionMessage => "Session Message",
            RudpPacketType::StreamConnectionSwitchAck => "Takion Switch Ack",
            RudpPacketType::Ack => "Ack",
            RudpPacketType::CtrlMessage => "Ctrl Message",
            RudpPacketType::Unknown => "Unknown",
            RudpPacketType::Offset8 | RudpPacketType::Offset10 => "Offset Message",
            RudpPacketType::Finish => "Finish",
        }
    }
}

/// Port von `RudpMessage` (Daten im Besitz der Struktur).
#[derive(Debug, Clone, Default)]
pub struct RudpMessage {
    pub subtype: u8,
    /// Roher Type-Wert aus dem Framing (ein `RudpPacketType` via [`Self::packet_type`]).
    pub type_: u16,
    pub size: u16,
    pub data: Vec<u8>,
    pub remote_counter: u16,
    pub sub_message: Option<Box<RudpMessage>>,
    pub sub_message_size: u16,
}

impl RudpMessage {
    /// Enum zum rohen Type-Feld (None bei unbekanntem Wert).
    pub fn packet_type(&self) -> Option<RudpPacketType> {
        RudpPacketType::from_u16(self.type_)
    }
}

/// Port von `RudpInstance` (Handle `ChiakiRudp`).
pub struct Rudp {
    pub(crate) shared: Arc<RudpShared>,
}

/// Interner, geteilter Zustand (auch für den Send-Buffer-Thread via Weak).
pub(crate) struct RudpShared {
    pub(crate) counter: Mutex<u16>,
    pub(crate) header: Mutex<u32>,
    pub(crate) sock: Mutex<Option<UdpSocket>>,
    pub(crate) stop_pipe: StopPipe,
    /// 1-Slot-Zwischenablage für stop_pipe_select_single (siehe Modul-Kopf).
    pub(crate) stash: Mutex<Option<Vec<u8>>>,
    pub(crate) send_buffer: OnceLock<Arc<RudpSendBuffer>>,
}

impl Rudp {
    /// Port von `chiaki_rudp_init()`.
    ///
    /// Der Socket geht in den Besitz der Instanz über. Startet den
    /// Re-Send-Buffer-Thread (RUDP_SEND_BUFFER_SIZE Slots).
    pub fn new(sock: UdpSocket) -> ChiakiResult<Rudp> {
        let shared = Arc::new(RudpShared {
            counter: Mutex::new(0),
            header: Mutex::new(0),
            sock: Mutex::new(Some(sock)),
            stop_pipe: StopPipe::new(),
            stash: Mutex::new(None),
            send_buffer: OnceLock::new(),
        });

        // chiaki_rudp_reset_counter_header(rudp);
        shared.reset_counter_header();

        // The send buffer size MUST be consistent with the acked seqnums array size in rudp_handle_message_ack()
        let send_buffer = Arc::new(RudpSendBuffer::new(RUDP_SEND_BUFFER_SIZE));
        send_buffer
            .attach(Arc::downgrade(&shared))
            .map_err(|e| {
                tracing::error!("Rudp failed initializing, failed creating send buffer");
                e
            })?;
        shared
            .send_buffer
            .set(send_buffer)
            .map_err(|_| ChiakiError::Unknown)?;

        Ok(Rudp { shared })
    }

    /// Port von `chiaki_rudp_reset_counter_header()`.
    ///
    /// Resets the counter and header of the rudp instance (used before init
    /// message is sent if rudp is already initialized).
    pub fn reset_counter_header(&self) {
        self.shared.reset_counter_header();
    }

    /// Port von `chiaki_rudp_send_init_message()`.
    ///
    /// Creates and sends an init rudp message for use when starting the session.
    pub fn send_init_message(&self) -> ChiakiResult<()> {
        let local_counter = self.shared.get_then_increase_counter();
        let header = self.shared.header();
        let mut data = Vec::with_capacity(14);
        data.extend_from_slice(&local_counter.to_be_bytes());
        // after_counter
        data.extend_from_slice(&[0x0B, 0x01, 0x01, 0x00, 0x01, 0x00]);
        data.extend_from_slice(&header.to_be_bytes());
        // after_header
        data.extend_from_slice(&[0x05, 0x82]);

        let message = RudpMessage {
            subtype: 0,
            type_: RudpPacketType::InitRequest.value(),
            size: (0xC << 12) | (8 + data.len() as u16),
            data,
            remote_counter: 0,
            sub_message: None,
            sub_message_size: 0,
        };
        let serialized = message_serialize(&message);
        self.shared.send_raw(&serialized)
    }

    /// Port von `chiaki_rudp_send_cookie_message()`.
    ///
    /// Creates and sends a cookie rudp message for use after starting message.
    ///
    /// @param response_buf The response from the init message
    pub fn send_cookie_message(&self, response_buf: &[u8]) -> ChiakiResult<()> {
        let local_counter = self.shared.get_then_increase_counter();
        let header = self.shared.header();
        let mut data = Vec::with_capacity(14 + response_buf.len());
        data.extend_from_slice(&local_counter.to_be_bytes());
        data.extend_from_slice(&[0x0B, 0x01, 0x01, 0x00, 0x01, 0x00]);
        data.extend_from_slice(&header.to_be_bytes());
        data.extend_from_slice(&[0x05, 0x82]);
        data.extend_from_slice(response_buf);

        let message = RudpMessage {
            subtype: 0,
            type_: RudpPacketType::CookieRequest.value(),
            size: (0xC << 12) | (8 + data.len() as u16),
            data,
            remote_counter: 0,
            sub_message: None,
            sub_message_size: 0,
        };
        let serialized = message_serialize(&message);
        self.shared.send_raw(&serialized)
    }

    /// Port von `chiaki_rudp_send_session_message()`.
    ///
    /// Creates and sends a session rudp message for use with registration
    /// message. Die eigentliche Nutzlast wird als CTRL-Sub-Message verpackt.
    pub fn send_session_message(
        &self,
        remote_counter: u16,
        session_msg: &[u8],
    ) -> ChiakiResult<()> {
        let local_counter = self.shared.get_then_increase_counter();

        let mut subdata = Vec::with_capacity(2 + session_msg.len());
        subdata.extend_from_slice(&local_counter.to_be_bytes());
        subdata.extend_from_slice(session_msg);
        let sub_message = RudpMessage {
            subtype: 0,
            type_: RudpPacketType::CtrlMessage.value(),
            size: (0xC << 12) | (8 + subdata.len() as u16),
            data: subdata,
            remote_counter: 0,
            sub_message: None,
            sub_message_size: 0,
        };

        let mut data = Vec::with_capacity(4);
        data.extend_from_slice(&local_counter.to_be_bytes());
        data.extend_from_slice(&remote_counter.to_be_bytes());

        let message = RudpMessage {
            subtype: 0,
            type_: RudpPacketType::SessionMessage.value(),
            size: (0xC << 12) | (8 + data.len() as u16),
            data,
            remote_counter: 0,
            sub_message: Some(Box::new(sub_message)),
            sub_message_size: 0,
        };
        let serialized = message_serialize(&message);
        self.shared.send_raw(&serialized)
    }

    /// Port von `chiaki_rudp_send_ack_message()`.
    ///
    /// Creates and sends an ack rudp message for use in acking received rudp
    /// messages. (Der lokale Counter wird — wie im C — nicht erhöht.)
    pub fn send_ack_message(&self, remote_counter: u16) -> ChiakiResult<()> {
        let counter = self.shared.local_counter();
        let mut data = Vec::with_capacity(6);
        data.extend_from_slice(&counter.to_be_bytes());
        data.extend_from_slice(&remote_counter.to_be_bytes());
        // after_counters
        data.extend_from_slice(&[0x00, 0x92]);

        let message = RudpMessage {
            subtype: 0,
            type_: RudpPacketType::Ack.value(),
            size: (0xC << 12) | (8 + data.len() as u16),
            data,
            remote_counter: 0,
            sub_message: None,
            sub_message_size: 0,
        };
        let serialized = message_serialize(&message);
        self.shared.send_raw(&serialized)
    }

    /// Port von `chiaki_rudp_send_ctrl_message()`.
    ///
    /// Creates and sends a ctrl rudp message for use with ctrl. Das Paket wird
    /// zusätzlich in den Send-Buffer gestellt, bis der remote Counter+1
    /// geackt wurde.
    pub fn send_ctrl_message(&self, ctrl_message: &[u8]) -> ChiakiResult<()> {
        let counter = self.shared.get_then_increase_counter();
        let counter_ack = self.shared.local_counter();

        let mut data = Vec::with_capacity(2 + ctrl_message.len());
        data.extend_from_slice(&counter.to_be_bytes());
        data.extend_from_slice(ctrl_message);

        let message = RudpMessage {
            subtype: 0,
            type_: RudpPacketType::CtrlMessage.value(),
            size: (0xC << 12) | (8 + data.len() as u16),
            data,
            remote_counter: 0,
            sub_message: None,
            sub_message_size: 0,
        };
        let serialized = message_serialize(&message);
        self.shared.send_raw(&serialized)?;
        self.send_buffer_push(counter_ack, serialized)
    }

    /// Port von `chiaki_rudp_send_switch_to_stream_connection_message()`.
    ///
    /// Creates and sends a switch to stream connection rudp message for use
    /// when switching from Senkusha to Stream Connection.
    pub fn send_switch_to_stream_connection_message(&self) -> ChiakiResult<()> {
        let counter = self.shared.get_then_increase_counter();
        let counter_ack = self.shared.local_counter();

        let mut data = Vec::with_capacity(26);
        data.extend_from_slice(&counter.to_be_bytes());
        // before_buf
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x10, 0x00, 0x0D, 0x00, 0x00]);
        let mut buf = [0u8; 16];
        random::random_bytes_crypt(&mut buf)?;
        data.extend_from_slice(&buf);

        let message = RudpMessage {
            subtype: 0,
            type_: RudpPacketType::CtrlMessage.value(),
            size: (0xC << 12) | (8 + data.len() as u16),
            data,
            remote_counter: 0,
            sub_message: None,
            sub_message_size: 0,
        };
        let serialized = message_serialize(&message);
        self.shared.send_raw(&serialized)?;
        self.send_buffer_push(counter_ack, serialized)
    }

    /// Port von `chiaki_rudp_send_raw()`.
    ///
    /// Sends a raw byte array over the Rudp socket.
    pub fn send_raw(&self, buf: &[u8]) -> ChiakiResult<()> {
        self.shared.send_raw(buf)
    }

    /// Port von `chiaki_rudp_select_recv()`.
    ///
    /// Selects an incoming message from the queue and receives the rudp message
    /// (wartet bis zu RUDP_EXPECT_TIMEOUT_MS; `Err(Timeout)`/`Err(Canceled)`).
    pub fn select_recv(&self, buf_size: usize) -> ChiakiResult<RudpMessage> {
        // Stash zuerst entleeren (von stop_pipe_select_single)
        if let Some(data) = self.shared.take_stash() {
            return parse_message(&data);
        }
        let deadline = Instant::now() + Duration::from_millis(RUDP_EXPECT_TIMEOUT_MS);
        loop {
            self.shared.stop_pipe.check()?;
            let mut buf = vec![0u8; buf_size];
            {
                let guard = self.shared.lock_sock();
                let Some(s) = guard.as_ref() else {
                    return Err(ChiakiError::Disconnected);
                };
                match sock::recv_from_timeout(s, &mut buf, Duration::from_millis(100)) {
                    Ok((n, _)) => {
                        if n <= 8 {
                            return Err(ChiakiError::Network);
                        }
                        buf.truncate(n);
                        return parse_message(&buf);
                    }
                    Err(ChiakiError::Timeout) => {}
                    Err(e) => {
                        tracing::error!("Rudp select failed: {e}");
                        return Err(e);
                    }
                }
            }
            if Instant::now() >= deadline {
                return Err(ChiakiError::Timeout);
            }
        }
    }

    /// Port von `chiaki_rudp_recv_only()`.
    ///
    /// Receives the rudp message. Must use select separately from this function.
    /// (Der Socket wird mit RUDP_EXPECT_TIMEOUT_MS gelesen, damit Library-Pfade
    /// nicht endlos blockieren; ein vorheriges select_recv/stop_pipe_select_single
    /// liefert über den Stash dasselbe Datagramm aus.)
    pub fn recv_only(&self, buf_size: usize) -> ChiakiResult<RudpMessage> {
        if let Some(data) = self.shared.take_stash() {
            return parse_message(&data);
        }
        let mut buf = vec![0u8; buf_size];
        let n = {
            let guard = self.shared.lock_sock();
            let Some(s) = guard.as_ref() else {
                return Err(ChiakiError::Disconnected);
            };
            match sock::recv_from_timeout(s, &mut buf, Duration::from_millis(RUDP_EXPECT_TIMEOUT_MS))
            {
                Ok((n, _)) => n,
                Err(e) => {
                    tracing::error!("Rudp recv failed: {e}");
                    return Err(e);
                }
            }
        };
        if n <= 8 {
            tracing::error!("Rudp recv returned less than the required 8 byte RUDP header");
            return Err(ChiakiError::Network);
        }
        parse_message(&buf[..n])
    }

    /// Port von `chiaki_rudp_stop_pipe_select_single()`.
    ///
    /// Selects a rudp message using the given stop pipe and timeout. Ohne
    /// std-select() wird hier gepollt; das erste empfangene Datagramm wird in
    /// den Stash gelegt und beim folgenden recv_only/select_recv ausgeliefert.
    pub fn stop_pipe_select_single(
        &self,
        stop_pipe: &StopPipe,
        timeout: Duration,
    ) -> ChiakiResult<()> {
        let deadline = Instant::now() + timeout;
        loop {
            stop_pipe.check()?;
            {
                let guard = self.shared.lock_sock();
                let Some(s) = guard.as_ref() else {
                    return Err(ChiakiError::Disconnected);
                };
                let mut buf = vec![0u8; 1500];
                match sock::recv_from_timeout(s, &mut buf, Duration::from_millis(50)) {
                    Ok((n, _)) => {
                        buf.truncate(n);
                        *self.shared.stash_lock() = Some(buf);
                        return Ok(());
                    }
                    Err(ChiakiError::Timeout) => {}
                    Err(e) => {
                        tracing::error!("Rudp select failed: {e}");
                        return Err(e);
                    }
                }
            }
            if Instant::now() >= deadline {
                return Err(ChiakiError::Timeout);
            }
        }
    }

    /// Port von `chiaki_rudp_send_recv()`.
    ///
    /// Sends a rudp message of a given type and checks for a given type to be
    /// returned. Tries a given number of times before failing. Used for initial
    /// rudp sequences.
    ///
    /// @param buf The buf to send as part of the data of the rudp message or
    ///            empty if not used.
    #[allow(clippy::too_many_arguments)]
    pub fn send_recv(
        &self,
        buf: &[u8],
        remote_counter: u16,
        send_type: RudpPacketType,
        recv_type: RudpPacketType,
        min_data_size: usize,
        tries: usize,
    ) -> ChiakiResult<RudpMessage> {
        let mut success = false;
        let mut message = RudpMessage::default();
        for _ in 0..tries {
            match send_type {
                RudpPacketType::InitRequest => {
                    let _ = self.send_init_message();
                }
                RudpPacketType::CookieRequest => {
                    let _ = self.send_cookie_message(buf);
                }
                RudpPacketType::Ack => {
                    let _ = self.send_ack_message(remote_counter);
                }
                RudpPacketType::SessionMessage => {
                    let _ = self.send_session_message(remote_counter, buf);
                }
                _ => {
                    tracing::error!(
                        "Selected RudpPacketType {:#04x} to send that is not supported by rudp send receive.",
                        send_type.value()
                    );
                    return Err(ChiakiError::InvalidData);
                }
            }
            match self.select_recv(1500) {
                Err(ChiakiError::Timeout) => continue,
                Err(e) => return Err(e),
                Ok(msg) => message = msg,
            }
            let mut found = true;
            loop {
                let expected_subtype_ok = match recv_type {
                    RudpPacketType::InitResponse => message.subtype == 0xD0,
                    RudpPacketType::CookieResponse => message.subtype == 0xA0,
                    RudpPacketType::CtrlMessage => {
                        (message.subtype & 0x0F) == 0x2 || (message.subtype & 0x0F) == 0x6
                    }
                    RudpPacketType::Finish => message.subtype == 0xC0,
                    _ => {
                        tracing::error!(
                            "Selected RudpPacketType {:#04x} to receive that is not supported by rudp send receive.",
                            recv_type.value()
                        );
                        return Err(ChiakiError::InvalidData);
                    }
                };
                if expected_subtype_ok {
                    break;
                }
                if assign_submessage_to_message(&mut message) {
                    continue;
                }
                tracing::error!(
                    "Expected {} with subtype {:#04x}.\nReceived unexpected RUDP message ... retrying",
                    recv_type.name(),
                    message.subtype
                );
                self.print_message(&message);
                found = false;
                break;
            }
            if !found {
                continue;
            }
            if message.data.len() < min_data_size {
                tracing::error!("Received message with too small of data size");
                continue;
            }
            success = true;
            break;
        }
        if success {
            Ok(message)
        } else {
            tracing::error!("Could not receive correct RUDP message after {} tries", tries);
            tracing::info!("Message Type: {}", recv_type.name());
            Err(ChiakiError::InvalidResponse)
        }
    }

    /// Port von `chiaki_rudp_ack_packet()`.
    ///
    /// Acknowledge received ack for rudp packet.
    pub fn ack_packet(&self, counter_to_ack: u16) -> ChiakiResult<()> {
        let sb = self
            .shared
            .send_buffer
            .get()
            .ok_or(ChiakiError::Uninitialized)?;
        sb.ack(counter_to_ack)?;
        Ok(())
    }

    /// Port von `chiaki_rudp_print_message()`.
    pub fn print_message(&self, message: &RudpMessage) {
        print_message_impl(message);
    }

    /// Port von `chiaki_rudp_get_local_counter()`.
    pub fn local_counter(&self) -> u16 {
        self.shared.local_counter()
    }

    /// Intern: Push in den Send-Buffer (von send_ctrl_message /
    /// send_switch_to_stream_connection_message genutzt).
    pub(crate) fn send_buffer_push(&self, seq_num: u16, buf: Vec<u8>) -> ChiakiResult<()> {
        let sb = self
            .shared
            .send_buffer
            .get()
            .ok_or(ChiakiError::Uninitialized)?;
        sb.push(seq_num, buf)
    }

    /// Port von `chiaki_rudp_fini()`: stoppt den Send-Buffer-Thread und
    /// schließt den Socket. Wird auch vom Drop ausgeführt.
    pub fn fini(mut self) -> ChiakiResult<()> {
        self.fini_impl();
        Ok(())
    }

    fn fini_impl(&mut self) {
        if let Some(sb) = self.shared.send_buffer.get() {
            sb.fini();
        }
        // Socket schließen (C: CHIAKI_SOCKET_CLOSE)
        *self.shared.lock_sock() = None;
    }
}

impl Drop for Rudp {
    fn drop(&mut self) {
        self.fini_impl();
    }
}

impl RudpShared {
    fn lock_sock(&self) -> MutexGuard<'_, Option<UdpSocket>> {
        self.sock.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn stash_lock(&self) -> MutexGuard<'_, Option<Vec<u8>>> {
        self.stash.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn take_stash(&self) -> Option<Vec<u8>> {
        self.stash_lock().take()
    }

    /// Port von `chiaki_rudp_reset_counter_header()`:
    /// counter = chiaki_random_32() % 0x5E00 + 0x1FF; header = chiaki_random_32() + 0x8000.
    pub(crate) fn reset_counter_header(&self) {
        {
            let mut counter = self.counter.lock().unwrap_or_else(|e| e.into_inner());
            *counter = (random::random_32() % 0x5E00) as u16 + 0x1FF;
        }
        let mut header = self.header.lock().unwrap_or_else(|e| e.into_inner());
        *header = random::random_32().wrapping_add(0x8000);
    }

    fn header(&self) -> u32 {
        *self.header.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Port von `chiaki_rudp_get_local_counter()`.
    pub(crate) fn local_counter(&self) -> u16 {
        *self.counter.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Port von `get_then_increase_counter()`.
    fn get_then_increase_counter(&self) -> u16 {
        let mut counter = self.counter.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = *counter;
        if *counter == u16::MAX {
            *counter = 0;
        } else {
            *counter += 1;
        }
        tmp
    }

    /// Port von `chiaki_rudp_send_raw()`.
    pub(crate) fn send_raw(&self, buf: &[u8]) -> ChiakiResult<()> {
        let guard = self.lock_sock();
        let Some(s) = guard.as_ref() else {
            return Err(ChiakiError::Disconnected);
        };
        tracing::trace!("Sending Message:\n{}", crate::hex_dump(buf));
        match s.send(buf) {
            Ok(_) => Ok(()),
            Err(e) => {
                let code = sock::map_io_error(&e);
                tracing::error!("Rudp raw failed to send packet: {e}");
                Err(code)
            }
        }
    }
}

/// Port von `rudp_message_serialize()`.
pub fn message_serialize(message: &RudpMessage) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + message.data.len());
    out.extend_from_slice(&message.size.to_be_bytes());
    out.extend_from_slice(&RUDP_CONSTANT.to_be_bytes());
    out.extend_from_slice(&message.type_.to_be_bytes());
    out.extend_from_slice(&message.data);
    if let Some(sub) = &message.sub_message {
        out.extend_from_slice(&message_serialize(sub));
    }
    out
}

/// Port von `chiaki_rudp_message_parse()`.
pub fn parse_message(serialized_msg: &[u8]) -> ChiakiResult<RudpMessage> {
    if serialized_msg.len() < 8 {
        // C liest blind 8 Header-Bytes; Aufrufer garantieren dort > 8
        return Err(ChiakiError::InvalidData);
    }
    let size = u16::from_be_bytes([serialized_msg[0], serialized_msg[1]]);
    let type_ = u16::from_be_bytes([serialized_msg[6], serialized_msg[7]]);
    let subtype = serialized_msg[6];
    // Eliminate 0xC before length (size of header + data but not submessage)
    let length = size & 0x0FFF; // serialized_msg[0] &= 0x0F; ntohs erneut
    let mut remote_counter = 0u16;

    let mut remaining = serialized_msg.len() as i64 - 8;
    let mut data = Vec::new();
    let mut sub_message = None;
    let mut sub_message_size = 0u16;

    if length > 8 {
        let mut data_size = (length - 8) as i64;
        if remaining < data_size {
            data_size = remaining;
        }
        let data_size = data_size as usize;
        data = serialized_msg[8..8 + data_size].to_vec();
        if data_size >= 2 {
            remote_counter =
                u16::from_be_bytes([data[0], data[1]]).wrapping_add(1);
        }
        remaining -= data_size as i64;
    }

    if remaining >= 8 {
        let offset = 8 + data.len();
        let sub = parse_message(&serialized_msg[offset..offset + remaining as usize])?;
        sub_message_size = remaining as u16;
        sub_message = Some(Box::new(sub));
    }

    Ok(RudpMessage {
        subtype,
        type_,
        size,
        data,
        remote_counter,
        sub_message,
        sub_message_size,
    })
}

/// Port von `assign_submessage_to_message()`.
///
/// Ersetzt `message` durch ihr Sub-Message (falls vorhanden) — true, wenn das
/// getan wurde und der Empfänger-Check erneut laufen muss.
fn assign_submessage_to_message(message: &mut RudpMessage) -> bool {
    if let Some(sub) = message.sub_message.take() {
        *message = *sub;
        true
    } else {
        false
    }
}

/// Port von `chiaki_rudp_print_message()`.
fn print_message_impl(message: &RudpMessage) {
    tracing::info!("-------------RUDP MESSAGE------------");
    let type_name = message
        .packet_type()
        .map(|t| t.name())
        .unwrap_or("Unknown Message Type");
    tracing::info!("Message Type: {}", type_name);
    tracing::info!("Rudp Message Subtype: {:#04x}", message.subtype);
    tracing::info!("Rudp Message Size: {:02x}", message.size);
    tracing::info!("Rudp Message Data Size: {}", message.data.len());
    tracing::info!("-----Rudp Message Data ---\n{}", crate::hex_dump(&message.data));
    tracing::info!("Rudp Message Remote Counter: {}", message.remote_counter);
    if let Some(sub) = &message.sub_message {
        print_message_impl(sub);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket;

    fn send_buffer_count(rudp: &Rudp) -> usize {
        rudp.shared
            .send_buffer
            .get()
            .expect("send buffer")
            .count()
    }

    fn loopback_pair() -> (UdpSocket, UdpSocket) {
        let a = chiaki_core::sock::create_udp_socket(
            "127.0.0.1:0".parse().unwrap(),
            &Default::default(),
        )
        .unwrap();
        let b = chiaki_core::sock::create_udp_socket(
            "127.0.0.1:0".parse().unwrap(),
            &Default::default(),
        )
        .unwrap();
        a.connect(b.local_addr().unwrap()).unwrap();
        b.connect(a.local_addr().unwrap()).unwrap();
        (a, b)
    }

    fn make_message(type_: u16, data: Vec<u8>, sub: Option<Box<RudpMessage>>) -> RudpMessage {
        RudpMessage {
            subtype: 0,
            type_,
            size: (0xC << 12) | (8 + data.len() as u16),
            data,
            remote_counter: 0,
            sub_message: sub,
            sub_message_size: 0,
        }
    }

    #[test]
    fn serialize_init_message_golden() {
        // Struktur aus chiaki_rudp_send_init_message mit festen Werten:
        // counter=0x1234, header=0x89ABCDEF
        let mut data = Vec::new();
        data.extend_from_slice(&0x1234u16.to_be_bytes());
        data.extend_from_slice(&[0x0B, 0x01, 0x01, 0x00, 0x01, 0x00]);
        data.extend_from_slice(&0x89AB_CDEFu32.to_be_bytes());
        data.extend_from_slice(&[0x05, 0x82]);
        let msg = make_message(RudpPacketType::InitRequest.value(), data, None);

        let bytes = message_serialize(&msg);
        // size = (0xC << 12) | (8 + 14) = 0xC016
        assert_eq!(
            bytes,
            vec![
                0xC0, 0x16, // size
                0x24, 0x4F, 0x24, 0x4F, // RUDP_CONSTANT
                0x80, 0x30, // INIT_REQUEST
                0x12, 0x34, // counter
                0x0B, 0x01, 0x01, 0x00, 0x01, 0x00, // after_counter
                0x89, 0xAB, 0xCD, 0xEF, // header
                0x05, 0x82, // after_header
            ]
        );
    }

    #[test]
    fn parse_init_message_golden() {
        let bytes = [
            0xC0, 0x16, 0x24, 0x4F, 0x24, 0x4F, 0x80, 0x30, 0x12, 0x34, 0x0B, 0x01, 0x01, 0x00,
            0x01, 0x00, 0x89, 0xAB, 0xCD, 0xEF, 0x05, 0x82,
        ];
        let msg = parse_message(&bytes).expect("parse");
        assert_eq!(msg.size, 0xC016);
        assert_eq!(msg.type_, 0x8030);
        assert_eq!(msg.packet_type(), Some(RudpPacketType::InitRequest));
        assert_eq!(msg.subtype, 0x80);
        assert_eq!(msg.data.len(), 14);
        assert_eq!(msg.remote_counter, 0x1235, "remote_counter = counter + 1");
        assert!(msg.sub_message.is_none());
        assert_eq!(&msg.data[..2], &[0x12, 0x34], "counter");
        assert_eq!(&msg.data[2..8], &[0x0B, 0x01, 0x01, 0x00, 0x01, 0x00], "after_counter");
        assert_eq!(&msg.data[8..12], &[0x89, 0xAB, 0xCD, 0xEF], "header");
        assert_eq!(&msg.data[12..], &[0x05, 0x82], "after_header");
    }

    #[test]
    fn parse_session_message_with_submessage() {
        // SESSION_MESSAGE: outer data = [local_counter][remote_counter],
        // sub = CTRL_MESSAGE mit [local_counter][payload]
        let payload = [0xde, 0xad, 0xbe, 0xef];
        let mut subdata = Vec::new();
        subdata.extend_from_slice(&0x0042u16.to_be_bytes());
        subdata.extend_from_slice(&payload);
        let sub = make_message(
            RudpPacketType::CtrlMessage.value(),
            subdata,
            None,
        );
        let mut data = Vec::new();
        data.extend_from_slice(&0x1111u16.to_be_bytes());
        data.extend_from_slice(&0x2222u16.to_be_bytes());
        let msg = make_message(
            RudpPacketType::SessionMessage.value(),
            data,
            Some(Box::new(sub)),
        );
        let bytes = message_serialize(&msg);

        // Größe: 8 + 4 + (8 + 2 + 4)
        assert_eq!(bytes.len(), 8 + 4 + 8 + 2 + 4);
        let parsed = parse_message(&bytes).expect("parse");
        assert_eq!(parsed.type_, 0x2030);
        assert_eq!(parsed.remote_counter, 0x1112);
        assert_eq!(parsed.data, vec![0x11, 0x11, 0x22, 0x22]);
        let sub = parsed.sub_message.expect("sub message");
        assert_eq!(sub.type_, 0x0230);
        assert_eq!(sub.packet_type(), Some(RudpPacketType::CtrlMessage));
        assert_eq!(sub.remote_counter, 0x0043);
        assert_eq!(&sub.data[..2], &[0x00, 0x42]);
        assert_eq!(&sub.data[2..], &payload);
        assert_eq!(parsed.sub_message_size, (8 + 2 + 4) as u16);
    }

    #[test]
    fn parse_ack_message_golden() {
        let mut data = Vec::new();
        data.extend_from_slice(&0x005Au16.to_be_bytes());
        data.extend_from_slice(&0x1234u16.to_be_bytes());
        data.extend_from_slice(&[0x00, 0x92]);
        let msg = make_message(RudpPacketType::Ack.value(), data, None);
        let bytes = message_serialize(&msg);
        assert_eq!(
            bytes,
            vec![
                0xC0, 0x0E, // size = (0xC<<12) | 14
                0x24, 0x4F, 0x24, 0x4F,
                0x24, 0x30, // ACK
                0x00, 0x5A, 0x12, 0x34, 0x00, 0x92,
            ]
        );
        let parsed = parse_message(&bytes).unwrap();
        assert_eq!(parsed.packet_type(), Some(RudpPacketType::Ack));
        assert_eq!(parsed.remote_counter, 0x005B, "counter (0x005A) + 1");
    }

    #[test]
    fn parse_truncates_size_mask() {
        // Die Länge im size-Feld wird auf 12 Bit maskiert (0xC-Nibble fliegt raus)
        let bytes = [
            0xF0, 0x0E, // 0xF00E & 0x0FFF = 0x00E = 14 → data_size 6, aber nur ...
            0x24, 0x4F, 0x24, 0x4F, 0x24, 0x30, 0x00, 0x01, 0x02, // nur 3 Datenbytes im Puffer
        ];
        let msg = parse_message(&bytes).expect("parse");
        // data_size wird auf remaining (3) geklemmt
        assert_eq!(msg.data, vec![0x00, 0x01, 0x02]);
        assert!(msg.sub_message.is_none());
    }

    #[test]
    fn packet_type_values_match_c() {
        assert_eq!(RudpPacketType::InitRequest.value(), 0x8030);
        assert_eq!(RudpPacketType::InitResponse.value(), 0xD000);
        assert_eq!(RudpPacketType::CookieRequest.value(), 0x9030);
        assert_eq!(RudpPacketType::CookieResponse.value(), 0xA030);
        assert_eq!(RudpPacketType::SessionMessage.value(), 0x2030);
        assert_eq!(RudpPacketType::StreamConnectionSwitchAck.value(), 0x242E);
        assert_eq!(RudpPacketType::Ack.value(), 0x2430);
        assert_eq!(RudpPacketType::CtrlMessage.value(), 0x0230);
        assert_eq!(RudpPacketType::Unknown.value(), 0x022F);
        assert_eq!(RudpPacketType::Offset8.value(), 0x1230);
        assert_eq!(RudpPacketType::Offset10.value(), 0x2630);
        assert_eq!(RudpPacketType::Finish.value(), 0xC000);
    }

    /// Loopback: INIT/COOKIE-Handshake zwischen zwei Rudp-Instanzen über
    /// 127.0.0.1-UDP-Sockets. Die B-Seite spielt den Konsole-Responder
    /// (INIT_RESPONSE/COOKIE_RESPONSE werden im C von der Konsole gebaut und
    /// sind nicht Teil der Client-API — hier per send_raw nachgebaut).
    #[test]
    fn rudp_to_rudp_init_cookie_handshake_loopback() {
        let (sock_a, sock_b) = loopback_pair();
        let rudp_a = Rudp::new(sock_a).expect("rudp a");
        let rudp_b = Rudp::new(sock_b).expect("rudp b");

        // A → INIT_REQUEST
        rudp_a.send_init_message().expect("send init");

        // B empfängt und prüft das Framing
        let init = rudp_b.select_recv(1500).expect("recv init");
        assert_eq!(init.packet_type(), Some(RudpPacketType::InitRequest));
        assert_eq!(init.subtype, 0x80);
        assert_eq!(init.data.len(), 14);

        // B → INIT_RESPONSE (subtype 0xD0, mit Cookie-Daten in data)
        let mut resp = make_message(RudpPacketType::InitResponse.value(), vec![0xAA; 16], None);
        resp.subtype = 0xD0;
        let resp_bytes = message_serialize(&resp);
        rudp_b.send_raw(&resp_bytes).expect("send init response");

        // A erwartet die INIT_RESPONSE (subtype 0xD0). send_recv sendet dabei
        // ein weiteres INIT_REQUEST, das bei B in der Queue landet und dort
        // vor dem Cookie abgeholt werden muss.
        let got = rudp_a
            .send_recv(
                &[],
                0,
                RudpPacketType::InitRequest,
                RudpPacketType::InitResponse,
                16,
                3,
            )
            .expect("init response");
        assert_eq!(got.subtype, 0xD0);
        assert_eq!(got.data, vec![0xAA; 16]);

        // B: das zweite INIT_REQUEST aus der Queue lesen
        let extra_init = rudp_b.select_recv(1500).expect("extra init request");
        assert_eq!(extra_init.packet_type(), Some(RudpPacketType::InitRequest));

        // A → COOKIE_REQUEST mit der Response-Daten der "Konsole"
        rudp_a
            .send_cookie_message(&got.data)
            .expect("send cookie");

        let cookie = rudp_b.select_recv(1500).expect("recv cookie");
        assert_eq!(cookie.packet_type(), Some(RudpPacketType::CookieRequest));
        assert_eq!(cookie.subtype, 0x90);
        // data = 14 Header-Bytes + 16 übernommene Response-Bytes
        assert_eq!(cookie.data.len(), 14 + 16);
        assert_eq!(&cookie.data[14..], &[0xAA; 16][..]);

        // B → COOKIE_RESPONSE, A empfängt via send_recv
        let mut cookie_resp =
            make_message(RudpPacketType::CookieResponse.value(), vec![0x55; 8], None);
        cookie_resp.subtype = 0xA0;
        rudp_b.send_raw(&message_serialize(&cookie_resp)).unwrap();
        let got = rudp_a
            .send_recv(
                &cookie_resp.data,
                0,
                RudpPacketType::CookieRequest,
                RudpPacketType::CookieResponse,
                8,
                3,
            )
            .expect("cookie response");
        assert_eq!(got.subtype, 0xA0);
        assert_eq!(got.data, vec![0x55; 8]);

        // B: das zweite COOKIE_REQUEST (von send_recv gesendet) aus der Queue lesen
        let extra_cookie = rudp_b.select_recv(1500).expect("extra cookie request");
        assert_eq!(extra_cookie.packet_type(), Some(RudpPacketType::CookieRequest));

        // A → Ctrl-Message (landet zusätzlich im Send-Buffer)
        rudp_a.send_ctrl_message(&[0x01, 0x02, 0x03]).expect("ctrl");
        let ctrl = rudp_b.select_recv(1500).expect("recv ctrl");
        assert_eq!(ctrl.packet_type(), Some(RudpPacketType::CtrlMessage));
        assert_eq!(&ctrl.data[2..], &[0x01, 0x02, 0x03]);

        // ACK-Pfad: B ackt A's Ctrl-Paket → A's Send-Buffer wird leer.
        // (ctrl.c: ack_counter = ntohs(*(uint16_t*)(message.data + 2)))
        assert_eq!(
            send_buffer_count(&rudp_a),
            1,
            "ctrl packet liegt im Send-Buffer"
        );
        rudp_b.send_ack_message(ctrl.remote_counter).expect("ack");
        let ack = rudp_a.select_recv(1500).expect("recv ack");
        assert_eq!(ack.packet_type(), Some(RudpPacketType::Ack));
        let counter_to_ack = u16::from_be_bytes([ack.data[2], ack.data[3]]);
        assert_eq!(counter_to_ack, ctrl.remote_counter);
        rudp_a.ack_packet(counter_to_ack).expect("ack packet");
        assert_eq!(send_buffer_count(&rudp_a), 0);

        let _ = rudp_a.fini();
        let _ = rudp_b.fini();
    }

    /// Session-Message-Roundtrip über den Loopback (Sub-Message-Framing).
    #[test]
    fn session_message_loopback() {
        let (sock_a, sock_b) = loopback_pair();
        let rudp_a = Rudp::new(sock_a).unwrap();
        let rudp_b = Rudp::new(sock_b).unwrap();

        let local_before = rudp_a.local_counter();
        rudp_a
            .send_session_message(0x4711, &[0xCA, 0xFE, 0xBA, 0xBE])
            .unwrap();
        let msg = rudp_b.select_recv(1500).unwrap();
        assert_eq!(msg.packet_type(), Some(RudpPacketType::SessionMessage));
        // Outer-Counter = lokaler Counter (zufälliger Start), remote_counter = +1
        assert_eq!(msg.remote_counter, local_before.wrapping_add(1));
        let sub = msg.sub_message.expect("sub");
        assert_eq!(sub.packet_type(), Some(RudpPacketType::CtrlMessage));
        assert_eq!(&sub.data[2..], &[0xCA, 0xFE, 0xBA, 0xBE]);
    }

    #[test]
    fn send_recv_times_out_without_peer() {
        // Socket ins Leere verbunden → kein Response → InvalidResponse
        let a = chiaki_core::sock::create_udp_socket(
            "127.0.0.1:0".parse().unwrap(),
            &Default::default(),
        )
        .unwrap();
        let sink = chiaki_core::sock::create_udp_socket(
            "127.0.0.1:0".parse().unwrap(),
            &Default::default(),
        )
        .unwrap();
        a.connect(sink.local_addr().unwrap()).unwrap();
        let rudp = Rudp::new(a).unwrap();
        let r = rudp.send_recv(
            &[],
            0,
            RudpPacketType::InitRequest,
            RudpPacketType::InitResponse,
            0,
            1,
        );
        assert_eq!(r.unwrap_err(), ChiakiError::InvalidResponse);
    }

    #[test]
    fn assign_submessage_replaces_message() {
        let mut msg = make_message(
            0x1000,
            vec![1],
            Some(Box::new(make_message(0x2000, vec![2, 3], None))),
        );
        assert!(assign_submessage_to_message(&mut msg));
        assert_eq!(msg.type_, 0x2000);
        assert_eq!(msg.data, vec![2, 3]);
        assert!(!assign_submessage_to_message(&mut msg));
    }
}
