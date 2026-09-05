// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/senkusha.c + lib/include/chiaki/senkusha.h (chiaki-ng).
//
// Senkusha ist der Verbindungs-Test vor der StreamConnection: Takion-Verbindung
// auf Port 9297 aufbauen, Takion-Protokoll-Version aushandeln, BANG empfangen,
// RTT messen (Echo-Pings) und MTU in/out per Binärsuche ermitteln.
//
// Die State-Machine ist 1:1 aus senkusha.c übernommen (STATE_TAKION_CONNECT →
// STATE_EXPECT_PROTOCOL_ACK → STATE_EXPECT_BANG → RTT (STATE_EXPECT_PONG) →
// STATE_EXPECT_MTU → STATE_EXPECT_CLIENT_MTU_COMMAND → Disconnect).
//
// Abweichungen gegenüber dem C-Code:
// - Der C-Code liest Host/Log/enable_dualsense aus ChiakiSession
//   (session->connect_info.host_addrinfo_selected, Port wird auf 9297 gesetzt).
//   Damit Senkusha unabhängig von (noch nicht portiertem) session.rs nutzbar
//   und testbar ist, gibt es stattdessen `SenkushaConnectInfo`.
// - `chiaki_senkusha_stop()` existiert in diesem chiaki-ng-Stand nicht (das
//   Feld `should_stop` wird nur initialisiert); als Abbruchhaken ist hier
//   dennoch `Senkusha::stop()` vorhanden (setzt should_stop, wie es das
//   chiaki-Upstream-Design vorsieht — alle Warte-Prädikate prüfen es).
// - `mtu_cmd.num = 1` im C setzt das Feld NICHT auf die Leitung (nanopb
//   encodiert optionale Felder nur bei gesetztem `has_num`, das C setzt nur
//   `num`). Das wire-identische Verhalten wird abgebildet (`num: None`).
// - Encryption ist — wie im C (`takion_info.enable_crypt = false`) — komplett
//   deaktiviert; Senkusha nutzt keine Launchspec-Keys.

use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use prost::Message as _;

use super::error::{ChiakiError, ChiakiResult};
use super::proto::{
    decode_takion_message, senkusha_payload, takion_message, BigPayload, DisconnectPayload,
    SenkushaClientMtuCommand, SenkushaEchoCommand, SenkushaMtuCommand, SenkushaPayload,
    TakionMessage, TakionProtocolRequestPayload,
};
use super::random::random_32;
use super::takion::{
    v7_av_packet_format_header, AVPacket, DisableAudioVideo, Takion, TakionConnectInfo,
    TakionEvent, TakionMessageDataType, V7_AV_HEADER_SIZE_BASE,
};
use super::time::now_us;

/// SENKUSHA_PORT (senkusha.c)
pub const SENKUSHA_PORT: u16 = 9297;

/// EXPECT_TIMEOUT_MS
pub const EXPECT_TIMEOUT_MS: u64 = 5000;
/// CONNECT_TIMEOUT_MS
pub const CONNECT_TIMEOUT_MS: u64 = 30000;

/// SENKUSHA_PING_COUNT_DEFAULT
pub const SENKUSHA_PING_COUNT_DEFAULT: u16 = 10;
/// EXPECT_PONG_TIMEOUT_MS
pub const EXPECT_PONG_TIMEOUT_MS: u64 = 1000;

/// Assuming IPv4, sizeof(ip header) + sizeof(udp header)
pub const MTU_UDP_PACKET_ADD: u32 = 0x1c;

/// MTU_AV_PACKET_ADD = CHIAKI_TAKION_V7_AV_HEADER_SIZE_BASE
pub const MTU_AV_PACKET_ADD: u32 = V7_AV_HEADER_SIZE_BASE as u32;

/// Amount of bytes to add to AV data size for MTU pings to get the full size
/// of the ip packet for MTU
pub const MTU_PING_DATA_ADD: u32 = MTU_UDP_PACKET_ADD + MTU_AV_PACKET_ADD;

/// Port von `SenkushaState` (senkusha.c, privat).
/// (ExpectStreaminfoAck wird im C-Code ebenfalls nie gesetzt — 1:1 übernommen.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum SenkushaState {
    Idle,
    TakionConnect,
    ExpectStreaminfoAck,
    ExpectBang,
    ExpectDataAck,
    ExpectProtocolAck,
    ExpectPong,
    ExpectMtu,
    ExpectClientMtuCommand,
}

/// Der vom Recv-Thread (Takion-Callback) und vom run()-Thread geteilte
/// Zustand (C: state/state_finished/state_failed/should_stop + Zähler hinter
/// state_mutex; Änderungen an state_finished/should_stop signalisieren
/// state_cond).
struct SenkushaSharedState {
    state: SenkushaState,
    state_finished: bool,
    /// wie im C gesetzt, wird (in diesem Stand) nie gelesen
    #[allow(dead_code)]
    state_failed: bool,
    should_stop: bool,
    data_ack_seq_num_expected: u32,
    pong_time_us: u64,
    ping_test_index: u16,
    ping_index: u16,
    ping_tag: u32,
    mtu_id: u32,
}

type Shared = Arc<(Mutex<SenkushaSharedState>, Condvar)>;

fn lock_state(shared: &Shared) -> MutexGuard<'_, SenkushaSharedState> {
    shared.0.lock().unwrap_or_else(PoisonError::into_inner)
}

fn notify(shared: &Shared) {
    shared.1.notify_all();
}

/// Port von `SenkushaConnectInfo` — ersetzt die C-Abhängigkeit von
/// ChiakiSession (Host-Sockaddr + enable_dualsense).
#[derive(Debug, Clone, Copy)]
pub struct SenkushaConnectInfo {
    /// Zieladresse; der Port wird (wie im C via `set_port(htons(9297))`)
    /// auf [`SENKUSHA_PORT`] gesetzt.
    pub host: SocketAddr,
    /// C: session->connect_info.enable_dualsense
    pub enable_dualsense: bool,
}

/// Port von `ChiakiSenkusha` (senkusha.h).
pub struct Senkusha {
    shared: Shared,
    takion: Option<Takion>,
}

impl Default for Senkusha {
    fn default() -> Self {
        Self::new()
    }
}

impl Senkusha {
    /// Port von `chiaki_senkusha_init()`.
    pub fn new() -> Self {
        Senkusha {
            shared: Arc::new((
                Mutex::new(SenkushaSharedState {
                    state: SenkushaState::Idle,
                    state_finished: false,
                    state_failed: false,
                    should_stop: false,
                    data_ack_seq_num_expected: 0,
                    ping_tag: 0,
                    pong_time_us: 0,
                    ping_test_index: 0,
                    ping_index: 0,
                    mtu_id: 0,
                }),
                Condvar::new(),
            )),
            takion: None,
        }
    }

    /// Setzt `should_stop` (Abort-Haken; der C-Stand exportiert dafür keine
    /// Funktion, das Feld ist aber Teil der State-Machine). Alle Warte-
    /// Prädikate brechen damit mit `ChiakiError::Canceled` ab.
    pub fn stop(&self) {
        lock_state(&self.shared).should_stop = true;
        notify(&self.shared);
    }

    /// Port von `chiaki_senkusha_run()`:
    ///
    /// ```c
    /// ChiakiErrorCode chiaki_senkusha_run(ChiakiSenkusha *senkusha,
    ///     uint32_t *mtu_in, uint32_t *mtu_out, uint64_t *rtt_us,
    ///     chiaki_socket_t *socket);
    /// ```
    ///
    /// `sock` entspricht dem C-`socket`-Parameter: wird er mitgegeben, nutzt
    /// Takion ihn (und schließt ihn nicht), sonst baut Takion einen eigenen
    /// Socket auf.
    pub fn run(
        &mut self,
        info: &SenkushaConnectInfo,
        mtu_in: &mut u32,
        mtu_out: &mut u32,
        rtt_us: &mut u64,
        sock: Option<UdpSocket>,
    ) -> ChiakiResult<()> {
        let err = self.run_internal(info, mtu_in, mtu_out, rtt_us, sock);
        // quit_takion: chiaki_takion_close(&senkusha->takion)
        if let Some(mut takion) = self.takion.take() {
            takion.close();
            tracing::info!("Senkusha closed takion");
        }
        err
    }

    fn run_internal(
        &mut self,
        info: &SenkushaConnectInfo,
        mtu_in: &mut u32,
        mtu_out: &mut u32,
        rtt_us: &mut u64,
        sock: Option<UdpSocket>,
    ) -> ChiakiResult<()> {
        {
            let mut st = lock_state(&self.shared);
            if st.should_stop {
                return Err(ChiakiError::Canceled);
            }

            // C: takion_info aus session->connect_info.host_addrinfo_selected,
            // Port wird auf SENKUSHA_PORT gesetzt; enable_crypt = false,
            // protocol_version = 7, av_reorder_timeout_us = 0.
            let mut host = info.host;
            host.set_port(SENKUSHA_PORT);

            let shared_cb = Arc::clone(&self.shared);
            let takion_info = TakionConnectInfo {
                host,
                ip_dontfrag: true,
                callback: Arc::new(move |event| senkusha_takion_cb(&shared_cb, event)),
                enable_crypt: false,
                disable_audio_video: DisableAudioVideo::NoneDisabled,
                enable_dualsense: info.enable_dualsense,
                protocol_version: 7,
                av_reorder_timeout_us: 0,
            };

            st.state = SenkushaState::TakionConnect;
            st.state_finished = false;
            st.state_failed = false;

            let takion = Takion::connect(takion_info, sock)
                .inspect_err(|_| tracing::error!("Senkusha connect failed"))?;
            self.takion = Some(takion);
        }

        // CONNECT_TIMEOUT_MS auf Connected/Disconnect warten
        let mut err = self.wait_state_finished(CONNECT_TIMEOUT_MS);
        if !self.state_finished() {
            if err == ChiakiError::Timeout {
                tracing::error!("Senkusha connect timeout");
            }
            if self.should_stop() {
                err = ChiakiError::Canceled;
            } else {
                tracing::error!("Senkusha Takion connect failed");
            }
            return Err(err);
        }

        tracing::info!("Setting takion versions");

        self.set_state(SenkushaState::ExpectProtocolAck);

        if let Err(e) = self.set_version() {
            tracing::error!("Senkusha failed to set takion version");
            return Err(e);
        }
        let mut err = self.wait_state_finished(EXPECT_TIMEOUT_MS);
        if !self.state_finished() {
            if err == ChiakiError::Timeout {
                tracing::error!("Senkusha set takion version receive timeout");
            }
            if self.should_stop() {
                err = ChiakiError::Canceled;
            } else {
                tracing::error!("Senkusha didn't receive protocol request ack");
            }
            return Err(err);
        }

        tracing::info!("Senkusha successfully set takion version");

        tracing::info!("Senkusha sending big");

        self.set_state(SenkushaState::ExpectBang);
        let buf = build_big();
        if let Err(e) = self
            .takion
            .as_ref()
            .ok_or(ChiakiError::Uninitialized)?
            .send_message_data(1, 1, &buf)
        {
            tracing::error!("Senkusha failed to send big");
            return Err(e);
        }

        let err = self.wait_state_finished(EXPECT_TIMEOUT_MS);
        if !self.state_finished() {
            if err == ChiakiError::Timeout {
                tracing::error!("Senkusha bang receive timeout");
            }
            if self.should_stop() {
                return Err(ChiakiError::Canceled);
            } else {
                tracing::error!("Senkusha didn't receive bang");
            }
            return Err(err);
        }

        tracing::info!("Senkusha successfully received bang");

        let mut err = self.run_rtt_test(0, SENKUSHA_PING_COUNT_DEFAULT, rtt_us);
        if err != ChiakiError::Success {
            tracing::error!("Senkusha Ping Test failed");
            return Err(err);
        }

        // C: mtu_timeout_ms = (*rtt_us * 5) / 1000, geklemmt auf 5..500
        // (rtt in µs -> das ist das 5-fache der RTT in ms — 1:1 übernommen).
        let mut mtu_timeout_ms = (*rtt_us * 5) / 1000;
        mtu_timeout_ms = mtu_timeout_ms.clamp(5, 500);

        err = self.run_mtu_in_test(576, 1454, 3, mtu_timeout_ms, mtu_in);
        if err != ChiakiError::Success {
            tracing::error!("Senkusha MTU in test failed");
            return Err(err);
        }

        err = self.run_mtu_out_test(*mtu_in, 576, 1454, 3, mtu_timeout_ms, mtu_out);
        if err != ChiakiError::Success {
            tracing::error!("Senkusha MTU out test failed");
            return Err(err);
        }

        // disconnect: Fehler werden ignoriert (C: kein CHECK im disconnect-Pfad)
        tracing::info!("Senkusha is disconnecting");
        let _ = self.send_disconnect();
        Ok(())
    }

    // -- State-Helfer -------------------------------------------------------

    /// C: senkusha->state = s; state_finished = false; state_failed = false
    fn set_state(&self, state: SenkushaState) {
        let mut st = lock_state(&self.shared);
        st.state = state;
        st.state_finished = false;
        st.state_failed = false;
    }

    fn should_stop(&self) -> bool {
        lock_state(&self.shared).should_stop
    }

    fn state_finished(&self) -> bool {
        lock_state(&self.shared).state_finished
    }

    /// Port von `chiaki_cond_timedwait_pred(..., state_finished_cond_check, ...)`:
    /// SUCCESS, sobald `state_finished || should_stop`; TIMEOUT sonst.
    fn wait_state_finished(&self, timeout_ms: u64) -> ChiakiError {
        let (mutex, cond) = &*self.shared;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut guard = mutex.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if guard.state_finished || guard.should_stop {
                return ChiakiError::Success;
            }
            let now = Instant::now();
            if now >= deadline {
                return ChiakiError::Timeout;
            }
            let (next, _) = cond
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(PoisonError::into_inner);
            guard = next;
        }
    }

    /// Port von `senkusha_send_data_wait_for_ack()`: sendet auf Kanal 8,
    /// merkt sich die Seq-Num und wartet auf das DataAck.
    fn send_data_wait_for_ack(&self, buf: &[u8]) -> ChiakiResult<()> {
        self.set_state(SenkushaState::ExpectDataAck);
        let seq_num = self
            .takion
            .as_ref()
            .ok_or(ChiakiError::Uninitialized)?
            .send_message_data(1, 8, buf)
            .inspect_err(|_| tracing::error!("Senkusha failed to send echo command"))?;
        lock_state(&self.shared).data_ack_seq_num_expected = seq_num;

        let mut err = self.wait_state_finished(EXPECT_TIMEOUT_MS);
        if !self.state_finished() {
            if err == ChiakiError::Timeout {
                tracing::error!("Senkusha data ack for echo command receive timeout");
            }
            if self.should_stop() {
                err = ChiakiError::Canceled;
            } else {
                tracing::error!("Senkusha failed to receive data ack for echo command");
            }
            return Err(err);
        }
        Ok(())
    }

    // -- Tests (RTT / MTU) --------------------------------------------------

    /// Port von `senkusha_run_rtt_test()`.
    fn run_rtt_test(&mut self, ping_test_index: u16, ping_count: u16, rtt_us: &mut u64) -> ChiakiError {
        tracing::info!("Senkusha Ping Test with count {ping_count} starting");

        if let Err(e) = self.send_echo_command(true) {
            tracing::error!("Senkusha Ping Test failed because sending echo command (true) failed");
            return e;
        }
        tracing::info!("Senkusha enabled echo");

        let mut rtt_us_acc = 0u64;
        let mut pings_successful = 0u64;
        for ping_index in 0..ping_count {
            tracing::info!("Senkusha sending Ping {ping_index} of test index {ping_test_index}");

            let av_packet = AVPacket {
                codec: 0xff,
                is_video: false,
                frame_index: ping_test_index,
                unit_index: ping_index,
                units_in_frame_total: 0x800, // or 0
                ..Default::default()
            };

            let mut data = [0u8; 0x224];
            let tag = match format_ping_packet(&mut data, &av_packet) {
                Ok(tag) => tag,
                Err(e) => {
                    tracing::error!("Senkusha failed to format AV Header");
                    return e;
                }
            };

            {
                let mut st = lock_state(&self.shared);
                st.state = SenkushaState::ExpectPong;
                st.state_finished = false;
                st.state_failed = false;
                st.ping_test_index = ping_test_index;
                st.ping_index = ping_index;
                st.ping_tag = tag;
            }

            let time_start_us = now_us();

            let send_res = self
                .takion
                .as_ref()
                .ok_or(ChiakiError::Uninitialized)
                .and_then(|t| t.send_raw(&data));
            if let Err(e) = send_res {
                tracing::error!("Senkusha failed to send ping");
                return e;
            }

            let err = self.wait_state_finished(EXPECT_PONG_TIMEOUT_MS);

            if !self.state_finished() {
                if err == ChiakiError::Timeout {
                    tracing::error!("Senkusha pong receive timeout");
                }
                if self.should_stop() {
                    return ChiakiError::Canceled;
                } else {
                    tracing::error!("Senkusha failed to receive pong");
                }
                continue;
            }

            let delta_us = lock_state(&self.shared).pong_time_us - time_start_us;
            rtt_us_acc += delta_us;
            pings_successful += 1;
            tracing::info!("Senkusha received Pong, RTT = {:.3} ms", delta_us as f32 * 0.001);
        }

        if let Err(e) = self.send_echo_command(false) {
            tracing::error!("Senkusha Ping Test failed because sending echo command (false) failed");
            return e;
        }
        tracing::info!("Senkusha disabled echo");

        if pings_successful < 1 {
            tracing::error!("Senkusha Ping test did not receive a single Pong");
            return ChiakiError::Unknown;
        }

        *rtt_us = rtt_us_acc / pings_successful;
        tracing::info!("Senkusha determined average RTT = {:.3} ms", *rtt_us as f32 * 0.001);

        ChiakiError::Success
    }

    /// Port von `senkusha_run_mtu_in_test()`.
    fn run_mtu_in_test(
        &mut self,
        mut min: u32,
        mut max: u32,
        retries: u32,
        timeout_ms: u64,
        mtu: &mut u32,
    ) -> ChiakiError {
        tracing::info!(
            "Senkusha starting MTU in test with min {min}, max {max}, retries {retries}, timeout {timeout_ms} ms"
        );

        let mut cur = max;
        let mut request_id = 0u32;
        while (max - min) > 1 {
            let mut success = false;
            for attempt in 0..retries {
                {
                    let mut st = lock_state(&self.shared);
                    st.state = SenkushaState::ExpectMtu;
                    st.state_finished = false;
                    st.state_failed = false;
                    request_id += 1;
                    st.mtu_id = request_id;
                }

                let mtu_cmd = SenkushaMtuCommand {
                    id: request_id,
                    mtu_req: cur,
                    // C: `mtu_cmd.num = 1` setzt kein has_num -> nie auf der
                    // Leitung; hier wire-identisch (siehe Modul-Doku).
                    ..Default::default()
                };
                if let Err(e) = self.send_mtu_command(&mtu_cmd) {
                    tracing::error!("Senkusha failed to send MTU command");
                    return e;
                }

                tracing::info!(
                    "Senkusha MTU request {cur} (min {min}, max {max}), id {request_id}, attempt {attempt}"
                );

                let err = self.wait_state_finished(timeout_ms);

                if !self.state_finished() {
                    if err == ChiakiError::Timeout {
                        tracing::info!("Senkusha MTU {cur} timeout");
                        continue;
                    }
                    if self.should_stop() {
                        return ChiakiError::Canceled;
                    } else {
                        tracing::error!("Senkusha failed to receive MTU response");
                    }
                }

                tracing::info!("Senkusha MTU {cur} success");
                success = true;
                break;
            }

            if success {
                min = cur;
            } else {
                max = cur;
            }
            cur = min + (max - min) / 2;
        }

        tracing::info!("Senkusha determined inbound MTU {min}");
        *mtu = min;

        ChiakiError::Success
    }

    /// Port von `senkusha_run_mtu_out_test()`.
    fn run_mtu_out_test(
        &mut self,
        mtu_in: u32,
        mut min: u32,
        mut max: u32,
        retries: u32,
        timeout_ms: u64,
        mtu: &mut u32,
    ) -> ChiakiError {
        if min < 8 + MTU_PING_DATA_ADD || max < min || mtu_in < min || mtu_in > max {
            return ChiakiError::InvalidData;
        }

        tracing::info!(
            "Senkusha starting MTU out test with min {min}, max {max}, retries {retries}, timeout {timeout_ms} ms"
        );

        {
            let mut st = lock_state(&self.shared);
            st.state = SenkushaState::ExpectClientMtuCommand;
            st.state_finished = false;
            st.state_failed = false;
            st.mtu_id = 1;
        }

        let client_mtu_cmd = SenkushaClientMtuCommand {
            id: lock_state(&self.shared).mtu_id,
            state: true,
            mtu_req: mtu_in,
            mtu_down: Some(mtu_in),
        };
        if let Err(e) = self.send_client_mtu_command(&client_mtu_cmd, false) {
            tracing::error!("Senkusha failed to send client MTU command");
            return e;
        }

        tracing::info!("Senkusha sent initial client MTU command");

        let err = self.wait_state_finished(EXPECT_TIMEOUT_MS);

        if !self.state_finished() {
            if err == ChiakiError::Timeout {
                tracing::error!("Senkusha Client MTU Command from server receive timeout");
                return err;
            }
            if self.should_stop() {
                return ChiakiError::Canceled;
            } else {
                tracing::error!("Senkusha failed to receive Client MTU command");
            }
            return ChiakiError::Unknown;
        }

        let packet_buf_size = (max - MTU_UDP_PACKET_ADD) as usize;
        let mut packet_buf = vec![0u8; packet_buf_size];
        // memset(packet_buf, 0, MTU_AV_PACKET_ADD + 8) steckt in vec![0];
        // Padding "CHIAKI" ab MTU_AV_PACKET_ADD + 8:
        const PADDING: &[u8; 6] = b"CHIAKI";
        for i in 0..packet_buf_size - (MTU_AV_PACKET_ADD + 8) as usize {
            packet_buf[i + (MTU_AV_PACKET_ADD + 8) as usize] = PADDING[i % PADDING.len()];
        }

        let mut err = ChiakiError::Success;

        let mut cur = mtu_in;
        while (max - min) > 1 {
            let mut success = false;
            'attempts: for attempt in 0..retries {
                let tag = random_32();

                {
                    let mut st = lock_state(&self.shared);
                    st.state = SenkushaState::ExpectPong;
                    st.state_finished = false;
                    st.state_failed = false;
                    st.ping_tag = tag;
                    st.ping_test_index = 0;
                    st.ping_index = attempt as u16;
                }

                let av_packet = AVPacket {
                    codec: 0xff,
                    is_video: false,
                    frame_index: 0,
                    unit_index: attempt as u16,
                    units_in_frame_total: 0x800,
                    ..Default::default()
                };

                let header_size = match v7_av_packet_format_header(&mut packet_buf, &av_packet) {
                    Ok(h) => h,
                    Err(e) => {
                        tracing::error!("Senkusha failed to format AV Header");
                        return e;
                    }
                };
                debug_assert_eq!(header_size as u32, MTU_AV_PACKET_ADD);

                packet_buf[MTU_AV_PACKET_ADD as usize..MTU_AV_PACKET_ADD as usize + 4].fill(0);
                packet_buf[MTU_AV_PACKET_ADD as usize + 4..MTU_AV_PACKET_ADD as usize + 8]
                    .copy_from_slice(&tag.to_be_bytes());

                tracing::info!("Senkusha MTU {cur} out ping attempt {attempt}");

                let send_size = (cur - MTU_UDP_PACKET_ADD) as usize;
                let send_res = self
                    .takion
                    .as_ref()
                    .ok_or(ChiakiError::Uninitialized)
                    .and_then(|t| t.send_raw(&packet_buf[..send_size]));

                let err = match send_res {
                    // C: Sendefehler -> err = TIMEOUT (continue wie Timeout)
                    Err(_) => {
                        tracing::error!("Senkusha failed to send ping");
                        ChiakiError::Timeout
                    }
                    Ok(()) => self.wait_state_finished(timeout_ms),
                };

                if !self.state_finished() {
                    if err == ChiakiError::Timeout {
                        tracing::info!("Senkusha MTU pong {cur} timeout");
                        continue 'attempts;
                    }
                    if self.should_stop() {
                        return ChiakiError::Canceled;
                    } else {
                        tracing::error!("Senkusha failed to receive MTU pong");
                    }
                }

                tracing::info!("Senkusha MTU ping {cur} success");
                success = true;
                break;
            }

            if success {
                min = cur;
            } else {
                max = cur;
            }
            cur = min + (max - min) / 2;
        }

        tracing::info!("Senkusha determined outbound MTU {min}");
        *mtu = min;

        tracing::info!("Senkusha sending final Client MTU Command");
        let client_mtu_cmd = SenkushaClientMtuCommand {
            id: 2,
            state: false,
            mtu_req: max,
            mtu_down: Some(mtu_in),
        };
        if let Err(e) = self.send_client_mtu_command(&client_mtu_cmd, true) {
            tracing::error!("Senkusha failed to send client MTU command");
            err = e;
        }

        err
    }

    // -- Sendefunktionen ----------------------------------------------------

    /// Port von `senkusha_set_version()`: TakionProtocolRequest mit Version 9
    /// auf Kanal 1.
    fn set_version(&self) -> ChiakiResult<()> {
        let buf = build_takion_protocol_request();
        self.takion
            .as_ref()
            .ok_or(ChiakiError::Uninitialized)?
            .send_message_data(1, 1, &buf)
            .map(|_| ())
    }

    /// Port von `senkusha_send_disconnect()` (der disconnect-Pfad im run
    /// ignoriert den Fehler wie im C).
    fn send_disconnect(&self) -> ChiakiResult<()> {
        let buf = build_disconnect();
        self.takion
            .as_ref()
            .ok_or(ChiakiError::Uninitialized)?
            .send_message_data(1, 1, &buf)
            .map(|_| ())
    }

    /// Port von `senkusha_send_echo_command()` (wartet auf DataAck).
    fn send_echo_command(&self, enable: bool) -> ChiakiResult<()> {
        let buf = build_echo_command(enable);
        self.send_data_wait_for_ack(&buf)
    }

    /// Port von `senkusha_send_mtu_command()` (Kanal 8, ohne ACK-Warte).
    fn send_mtu_command(&self, command: &SenkushaMtuCommand) -> ChiakiResult<()> {
        let buf = build_mtu_command(command);
        self.takion
            .as_ref()
            .ok_or(ChiakiError::Uninitialized)?
            .send_message_data(1, 8, &buf)
            .map(|_| ())
    }

    /// Port von `senkusha_send_client_mtu_command()`.
    fn send_client_mtu_command(
        &self,
        command: &SenkushaClientMtuCommand,
        wait_for_ack: bool,
    ) -> ChiakiResult<()> {
        let buf = build_client_mtu_command(command);
        if !wait_for_ack {
            return self
                .takion
                .as_ref()
                .ok_or(ChiakiError::Uninitialized)?
                .send_message_data(1, 8, &buf)
                .map(|_| ());
        }
        self.send_data_wait_for_ack(&buf)
    }
}

// ---------------------------------------------------------------------------
// Payload-Aufbau (protobuf)
// ---------------------------------------------------------------------------

/// Payload von `senkusha_set_version()`: TakionMessage{type=
/// TAKIONPROTOCOLREQUEST, takion_protocol_request={supported_versions=[9]}}
/// (C: chiaki_pb_encode_list mit einer Version).
fn build_takion_protocol_request() -> Vec<u8> {
    let msg = TakionMessage {
        r#type: takion_message::PayloadType::Takionprotocolrequest.into(),
        takion_protocol_request: Some(TakionProtocolRequestPayload {
            supported_takion_versions: vec![9],
        }),
        ..Default::default()
    };
    msg.encode_to_vec()
}

/// Payload von `senkusha_send_big()`: BIG mit client_version 9 und leeren
/// session_key/launch_spec/encrypted_key (C: leere Strings via Callbacks;
/// required-Felder werden — wie in nanopb — immer geschrieben).
fn build_big() -> Vec<u8> {
    let msg = TakionMessage {
        r#type: takion_message::PayloadType::Big.into(),
        big_payload: Some(BigPayload {
            client_version: 9,
            session_key: String::new(),
            launch_spec: String::new(),
            encrypted_key: Vec::new(),
            ecdh_pub_key: None,
            ecdh_sig: None,
        }),
        ..Default::default()
    };
    msg.encode_to_vec()
}

/// Payload von `senkusha_send_disconnect()`.
fn build_disconnect() -> Vec<u8> {
    let msg = TakionMessage {
        r#type: takion_message::PayloadType::Disconnect.into(),
        disconnect_payload: Some(DisconnectPayload {
            reason: "Client Disconnecting".to_owned(),
            extended_info: None,
        }),
        ..Default::default()
    };
    msg.encode_to_vec()
}

/// Payload von `senkusha_send_echo_command()`.
fn build_echo_command(enable: bool) -> Vec<u8> {
    let msg = TakionMessage {
        r#type: takion_message::PayloadType::Senkusha.into(),
        senkusha_payload: Some(SenkushaPayload {
            command: senkusha_payload::Command::EchoCommand.into(),
            echo_command: Some(SenkushaEchoCommand { state: enable }),
            ..Default::default()
        }),
        ..Default::default()
    };
    msg.encode_to_vec()
}

/// Payload von `senkusha_send_mtu_command()`.
fn build_mtu_command(command: &SenkushaMtuCommand) -> Vec<u8> {
    let msg = TakionMessage {
        r#type: takion_message::PayloadType::Senkusha.into(),
        senkusha_payload: Some(SenkushaPayload {
            command: senkusha_payload::Command::MtuCommand.into(),
            mtu_command: Some(command.clone()),
            ..Default::default()
        }),
        ..Default::default()
    };
    msg.encode_to_vec()
}

/// Payload von `senkusha_send_client_mtu_command()`.
fn build_client_mtu_command(command: &SenkushaClientMtuCommand) -> Vec<u8> {
    let msg = TakionMessage {
        r#type: takion_message::PayloadType::Senkusha.into(),
        senkusha_payload: Some(SenkushaPayload {
            command: senkusha_payload::Command::ClientMtuCommand.into(),
            client_mtu_command: Some(command.clone()),
            ..Default::default()
        }),
        ..Default::default()
    };
    msg.encode_to_vec()
}

/// Gemeinsamer Ping-Paket-Aufbau von RTT-Test und MTU-out-Test:
/// v7-AV-Header (is_video=false), dann 4 Nullbytes, dann der Tag
/// (big-endian) — exakt die Byte-Platzierung aus senkusha.c. Liefert den Tag.
///
/// RTT-Pfad (C): Puffer `data[0x224]` (Nullen); MTU-out-Pfad: Padding-Puffer;
/// die Sendelänge steuert der Aufrufer, gesendet wird via `send_raw`.
fn format_ping_packet(buf: &mut [u8], av_packet: &AVPacket) -> ChiakiResult<u32> {
    let header_size = v7_av_packet_format_header(buf, av_packet)?;
    let tag = random_32();
    buf[header_size + 4..header_size + 8].copy_from_slice(&tag.to_be_bytes());
    Ok(tag)
}

// ---------------------------------------------------------------------------
// Takion-Callback (senkusha_takion_cb + Handler)
// ---------------------------------------------------------------------------

fn senkusha_takion_cb(shared: &Shared, event: TakionEvent) {
    // C: CHIAKI_TAKION_EVENT_TYPE_CONNECTED / _DISCONNECT gemeinsam behandelt
    let connected = matches!(event, TakionEvent::Connected);
    match event {
        TakionEvent::Connected | TakionEvent::Disconnect(_) => {
            let mut st = lock_state(shared);
            if st.state == SenkushaState::TakionConnect {
                st.state_finished = connected;
                st.state_failed = !connected;
                drop(st);
                notify(shared);
            }
        }
        TakionEvent::Data { data_type, buf } => senkusha_takion_data(shared, data_type, &buf),
        TakionEvent::DataAck { seq_num } => senkusha_takion_data_ack(shared, seq_num),
        TakionEvent::Av(packet) => senkusha_takion_av(shared, &packet),
    }
}

/// Port von `senkusha_takion_data()`.
fn senkusha_takion_data(shared: &Shared, data_type: TakionMessageDataType, buf: &[u8]) {
    if data_type != TakionMessageDataType::Protobuf {
        return;
    }

    let msg = match decode_takion_message(buf) {
        Ok(msg) => msg,
        Err(_) => {
            tracing::error!("Senkusha failed to decode data protobuf");
            return;
        }
    };

    let mut st = lock_state(shared);
    let payload_type = takion_message::PayloadType::try_from(msg.r#type);
    match st.state {
        SenkushaState::ExpectBang => {
            if payload_type != Ok(takion_message::PayloadType::Bang) || msg.bang_payload.is_none() {
                tracing::error!("Senkusha expected bang payload but received something else");
            } else {
                st.state_finished = true;
                drop(st);
                notify(shared);
            }
        }
        SenkushaState::ExpectProtocolAck => {
            if payload_type != Ok(takion_message::PayloadType::Takionprotocolrequestack)
                || msg.takion_protocol_request_ack.is_none()
            {
                tracing::error!("Senkusha expected protocol request ack but received something else");
            } else {
                st.state_finished = true;
                drop(st);
                notify(shared);
            }
        }
        SenkushaState::ExpectClientMtuCommand => {
            let senkusha_ok = payload_type == Ok(takion_message::PayloadType::Senkusha)
                && msg.senkusha_payload.as_ref().is_some_and(|p| {
                    p.command == senkusha_payload::Command::ClientMtuCommand as i32
                        && p.client_mtu_command.is_some()
                        && p.client_mtu_command.as_ref().unwrap().id == st.mtu_id
                });
            if !senkusha_ok {
                // There might be another MTU_COMMAND from the server, which we
                // ignore, but this is not an error.
                let is_mtu_command = payload_type == Ok(takion_message::PayloadType::Senkusha)
                    && msg
                        .senkusha_payload
                        .as_ref()
                        .is_some_and(|p| p.command == senkusha_payload::Command::MtuCommand as i32);
                if !is_mtu_command {
                    tracing::error!(
                        "Senkusha expected Client MTU Command with matching id but received something else"
                    );
                }
            } else {
                tracing::info!("Senkusha received expected Client MTU Command");
                st.state_finished = true;
                drop(st);
                notify(shared);
            }
        }
        _ => {}
    }
}

/// Port von `senkusha_takion_data_ack()`.
fn senkusha_takion_data_ack(shared: &Shared, seq_num: u32) {
    let mut st = lock_state(shared);
    if st.state == SenkushaState::ExpectDataAck && st.data_ack_seq_num_expected == seq_num {
        st.state_finished = true;
        drop(st);
        notify(shared);
    }
}

/// Port von `senkusha_takion_av()`.
fn senkusha_takion_av(shared: &Shared, packet: &AVPacket) {
    let time_us = now_us();

    let mut st = lock_state(shared);
    match st.state {
        SenkushaState::ExpectPong => {
            if packet.is_video
                || packet.frame_index != st.ping_test_index
                || packet.unit_index != st.ping_index
                || packet.data.len() < 8
            {
                tracing::warn!(
                    "Senkusha received invalid Pong {}/{}, size: {:#x}",
                    packet.frame_index,
                    packet.unit_index,
                    packet.data.len()
                );
                return;
            }

            let tag = u32::from_be_bytes([
                packet.data[4],
                packet.data[5],
                packet.data[6],
                packet.data[7],
            ]);
            if tag != st.ping_tag {
                tracing::warn!("Senkusha received Pong with invalid tag");
                return;
            }

            st.pong_time_us = time_us;
            st.state_finished = true;
            drop(st);
            notify(shared);
        }
        SenkushaState::ExpectMtu => {
            if !packet.is_video || u32::from(packet.frame_index) != st.mtu_id {
                tracing::warn!(
                    "Senkusha received invalid MTU response {}, size: {:#x}, is video: {}",
                    packet.frame_index,
                    packet.data.len(),
                    packet.is_video as u8
                );
                return;
            }

            st.state_finished = true;
            drop(st);
            notify(shared);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_varint_field(out: &mut Vec<u8>, field_no: u32, value: u64) {
        push_varint(out, u64::from(field_no) << 3);
        push_varint(out, value);
    }

    fn push_len_delimited(out: &mut Vec<u8>, field_no: u32, payload: &[u8]) {
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

    /// senkusha_set_version(): type=TAKIONPROTOCOLREQUEST(31), Payload Feld 31
    /// mit einem ungepackten repeated varint 9.
    #[test]
    fn takion_protocol_request_wire_format() {
        let mut manual = Vec::new();
        push_varint_field(&mut manual, 1, 31);
        let mut inner = Vec::new();
        push_varint_field(&mut inner, 1, 9);
        push_len_delimited(&mut manual, 31, &inner);

        let built = build_takion_protocol_request();
        assert_eq!(built, manual);

        // Dekodierbar und inhaltlich korrekt
        let decoded = decode_takion_message(&built).unwrap();
        assert_eq!(
            decoded.r#type,
            i32::from(takion_message::PayloadType::Takionprotocolrequest)
        );
        assert_eq!(
            decoded.takion_protocol_request.unwrap().supported_takion_versions,
            vec![9]
        );
    }

    /// senkusha_send_big(): BIG mit client_version 9 und leeren required-
    /// Strings/Bytes — nanopb schreibt required immer (tag + Länge 0).
    #[test]
    fn big_wire_format() {
        let mut manual = Vec::new();
        push_varint_field(&mut manual, 1, 0); // BIG = 0 (required, immer geschrieben)
        let mut inner = Vec::new();
        push_varint_field(&mut inner, 1, 9); // client_version
        push_len_delimited(&mut inner, 2, b""); // session_key
        push_len_delimited(&mut inner, 3, b""); // launch_spec
        push_len_delimited(&mut inner, 4, b""); // encrypted_key
        push_len_delimited(&mut manual, 2, &inner);

        let built = build_big();
        assert_eq!(built, manual, "prost muss required-Standardwerte mitschreiben");
    }

    /// senkusha_send_disconnect(): DISCONNECT mit reason "Client Disconnecting".
    #[test]
    fn disconnect_wire_format() {
        let mut manual = Vec::new();
        push_varint_field(&mut manual, 1, 8); // DISCONNECT
        let mut inner = Vec::new();
        push_len_delimited(&mut inner, 1, b"Client Disconnecting");
        push_len_delimited(&mut manual, 10, &inner);

        assert_eq!(build_disconnect(), manual);
    }

    /// senkusha_send_echo_command(): SENKUSHA/ECHO_COMMAND mit required bool
    /// (echo_command ist eine eingebettete Message: Feld 2 mit Tag+Länge).
    #[test]
    fn echo_command_wire_format() {
        // state = true
        let mut manual_on = Vec::new();
        push_varint_field(&mut manual_on, 1, 12); // SENKUSHA
        let mut inner = Vec::new();
        push_varint_field(&mut inner, 1, 0); // ECHO_COMMAND
        let mut echo = Vec::new();
        push_varint_field(&mut echo, 1, 1); // echo_command{state=true}
        push_len_delimited(&mut inner, 2, &echo);
        push_len_delimited(&mut manual_on, 14, &inner);
        assert_eq!(build_echo_command(true), manual_on);

        // state = false — required bool wird trotzdem geschrieben
        let mut manual_off = Vec::new();
        push_varint_field(&mut manual_off, 1, 12);
        let mut inner = Vec::new();
        push_varint_field(&mut inner, 1, 0);
        let mut echo = Vec::new();
        push_varint_field(&mut echo, 1, 0); // state=false
        push_len_delimited(&mut inner, 2, &echo);
        push_len_delimited(&mut manual_off, 14, &inner);
        assert_eq!(build_echo_command(false), manual_off);
    }

    /// senkusha_send_mtu_command(): nur id/mtu_req auf der Leitung (C setzt
    /// `num` ohne `has_num` — siehe Modul-Doku).
    #[test]
    fn mtu_command_wire_format() {
        let cmd = SenkushaMtuCommand {
            id: 5,
            mtu_req: 1454,
            ..Default::default()
        };
        let mut manual = Vec::new();
        push_varint_field(&mut manual, 1, 12);
        let mut inner = Vec::new();
        push_varint_field(&mut inner, 1, 1); // MTU_COMMAND
        let mut cmd_bytes = Vec::new();
        push_varint_field(&mut cmd_bytes, 1, 5); // id
        push_varint_field(&mut cmd_bytes, 2, 1454); // mtu_req
        push_len_delimited(&mut inner, 3, &cmd_bytes); // mtu_command
        push_len_delimited(&mut manual, 14, &inner);
        assert_eq!(build_mtu_command(&cmd), manual);
    }

    /// senkusha_send_client_mtu_command(): id/state/mtu_req/mtu_down.
    #[test]
    fn client_mtu_command_wire_format() {
        let cmd = SenkushaClientMtuCommand {
            id: 1,
            state: true,
            mtu_req: 1200,
            mtu_down: Some(1200),
        };
        let mut manual = Vec::new();
        push_varint_field(&mut manual, 1, 12);
        let mut inner = Vec::new();
        push_varint_field(&mut inner, 1, 4); // CLIENT_MTU_COMMAND
        let mut cmd_bytes = Vec::new();
        push_varint_field(&mut cmd_bytes, 1, 1); // id
        push_varint_field(&mut cmd_bytes, 2, 1200); // mtu_req
        push_varint_field(&mut cmd_bytes, 3, 1); // state
        push_varint_field(&mut cmd_bytes, 4, 1200); // mtu_down
        push_len_delimited(&mut inner, 5, &cmd_bytes); // client_mtu_command
        push_len_delimited(&mut manual, 14, &inner);
        assert_eq!(build_client_mtu_command(&cmd), manual);
    }

    /// Ping-Paket-Format: v7-AV-Header + 4 Nullbytes + Tag (big-endian).
    #[test]
    fn ping_packet_format() {
        let av_packet = AVPacket {
            codec: 0xff,
            is_video: false,
            frame_index: 3,
            unit_index: 7,
            units_in_frame_total: 0x800,
            ..Default::default()
        };
        let mut data = [0u8; 0x224];
        let tag = format_ping_packet(&mut data, &av_packet).expect("format ping");

        // v7-Header: Basisgröße 0x12, is_video=false -> Basis-Typ Audio (3)
        assert_eq!(data[0] & 0x0f, 3);
        assert_eq!(&data[3..5], &3u16.to_be_bytes()); // frame_index
        // unit_index/units_in_frame stecken gepackt in dword_2:
        let dword_2 = u32::from_be_bytes([data[5], data[6], data[7], data[8]]);
        assert_eq!((dword_2 >> 0x15) & 0x7ff, 7); // unit_index
        assert_eq!((dword_2 >> 0xa) & 0x7ff, 0x800 - 1); // units_in_frame_total - 1
        assert_eq!(data[9], 0xff); // codec
        // 4 Nullbytes nach dem Header, dann der Tag
        assert_eq!(&data[0x12..0x16], &[0; 4]);
        assert_eq!(&data[0x16..0x1a], &tag.to_be_bytes());
        // Rest bleibt 0 (RTT-Ping-Puffer ist Nullen)
        assert!(data[0x1a..].iter().all(|&b| b == 0));

        // Tag zufällig, aber reproduzierbar eingelesen: ntohl an data+4
        let decoded_tag = u32::from_be_bytes([data[0x16], data[0x17], data[0x18], data[0x19]]);
        assert_eq!(decoded_tag, tag);
    }

    /// State-Machine-Handler mit simulierten Takion-Events (ohne Netz).
    #[test]
    fn state_machine_bang_and_protocol_ack() {
        let senkusha = Senkusha::new();
        let shared = &senkusha.shared;

        // EXPECT_BANG + BANG -> finished
        senkusha.set_state(SenkushaState::ExpectBang);
        let bang = TakionMessage {
            r#type: takion_message::PayloadType::Bang.into(),
            bang_payload: Some(super::super::proto::BangPayload {
                server_version: 9,
                token: 0,
                encrypted_key_accepted: false,
                version_accepted: true,
                session_key: String::new(),
                extended_info: None,
                server_version_string: None,
                ecdh_pub_key: None,
                ecdh_sig: None,
            }),
            ..Default::default()
        };
        senkusha_takion_data(
            shared,
            TakionMessageDataType::Protobuf,
            &bang.encode_to_vec(),
        );
        assert!(senkusha.state_finished());

        // EXPECT_PROTOCOL_ACK + Ack -> finished
        senkusha.set_state(SenkushaState::ExpectProtocolAck);
        let ack = TakionMessage {
            r#type: takion_message::PayloadType::Takionprotocolrequestack.into(),
            takion_protocol_request_ack: Some(super::super::proto::TakionProtocolRequestAckPayload {
                takion_protocol_version: Some(9),
            }),
            ..Default::default()
        };
        senkusha_takion_data(
            shared,
            TakionMessageDataType::Protobuf,
            &ack.encode_to_vec(),
        );
        assert!(senkusha.state_finished());

        // Falsche Payload -> nicht finished
        senkusha.set_state(SenkushaState::ExpectBang);
        senkusha_takion_data(shared, TakionMessageDataType::Protobuf, &ack.encode_to_vec());
        assert!(!senkusha.state_finished());
    }

    /// Client-MTU-Command-Handler: id muss mtu_id matching; MTU_COMMAND vom
    /// Server wird ignoriert (kein Fehler).
    #[test]
    fn state_machine_client_mtu_command() {
        let senkusha = Senkusha::new();
        let shared = &senkusha.shared;
        lock_state(shared).mtu_id = 3;
        senkusha.set_state(SenkushaState::ExpectClientMtuCommand);

        // passendes Client-MTU-Command (id = 3)
        let msg = TakionMessage {
            r#type: takion_message::PayloadType::Senkusha.into(),
            senkusha_payload: Some(SenkushaPayload {
                command: senkusha_payload::Command::ClientMtuCommand.into(),
                client_mtu_command: Some(SenkushaClientMtuCommand {
                    id: 3,
                    state: false,
                    mtu_req: 1000,
                    mtu_down: None,
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        senkusha_takion_data(shared, TakionMessageDataType::Protobuf, &msg.encode_to_vec());
        assert!(senkusha.state_finished());

        // falsche id -> nicht finished, aber MTU_COMMAND wird still ignoriert
        senkusha.set_state(SenkushaState::ExpectClientMtuCommand);
        let wrong_id = TakionMessage {
            r#type: takion_message::PayloadType::Senkusha.into(),
            senkusha_payload: Some(SenkushaPayload {
                command: senkusha_payload::Command::ClientMtuCommand.into(),
                client_mtu_command: Some(SenkushaClientMtuCommand {
                    id: 99,
                    state: false,
                    mtu_req: 1000,
                    mtu_down: None,
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        senkusha_takion_data(shared, TakionMessageDataType::Protobuf, &wrong_id.encode_to_vec());
        assert!(!senkusha.state_finished());

        let mtu_cmd = TakionMessage {
            r#type: takion_message::PayloadType::Senkusha.into(),
            senkusha_payload: Some(SenkushaPayload {
                command: senkusha_payload::Command::MtuCommand.into(),
                mtu_command: Some(SenkushaMtuCommand {
                    id: 1,
                    mtu_req: 1000,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        senkusha_takion_data(shared, TakionMessageDataType::Protobuf, &mtu_cmd.encode_to_vec());
        assert!(!senkusha.state_finished());

        // völlig andere Payload -> Fehlerlog, nicht finished
        senkusha_takion_data(
            shared,
            TakionMessageDataType::Protobuf,
            &build_disconnect(),
        );
        assert!(!senkusha.state_finished());
    }

    /// Pong-Handler: Tag/Index-Matching, pong_time_us wird gesetzt.
    #[test]
    fn state_machine_pong() {
        let senkusha = Senkusha::new();
        let shared = &senkusha.shared;
        {
            let mut st = lock_state(shared);
            st.state = SenkushaState::ExpectPong;
            st.ping_test_index = 0;
            st.ping_index = 2;
            st.ping_tag = 0xdead_beef;
        }

        // falscher unit_index -> nicht finished
        let mut pkt = AVPacket {
            is_video: false,
            frame_index: 0,
            unit_index: 1,
            data: vec![0; 8],
            ..Default::default()
        };
        pkt.data[4..8].copy_from_slice(&0xdead_beefu32.to_be_bytes());
        senkusha_takion_av(shared, &pkt);
        assert!(!senkusha.state_finished());

        // falscher Tag -> nicht finished
        let mut pkt = AVPacket {
            is_video: false,
            frame_index: 0,
            unit_index: 2,
            data: vec![0; 8],
            ..Default::default()
        };
        pkt.data[4..8].copy_from_slice(&0x1234_5678u32.to_be_bytes());
        senkusha_takion_av(shared, &pkt);
        assert!(!senkusha.state_finished());

        // zu kurz -> nicht finished
        let pkt = AVPacket {
            is_video: false,
            frame_index: 0,
            unit_index: 2,
            data: vec![0; 4],
            ..Default::default()
        };
        senkusha_takion_av(shared, &pkt);
        assert!(!senkusha.state_finished());

        // passend -> finished + pong_time_us
        let before = now_us();
        let mut pkt = AVPacket {
            is_video: false,
            frame_index: 0,
            unit_index: 2,
            data: vec![0; 8],
            ..Default::default()
        };
        pkt.data[4..8].copy_from_slice(&0xdead_beefu32.to_be_bytes());
        senkusha_takion_av(shared, &pkt);
        assert!(senkusha.state_finished());
        let st = lock_state(shared);
        assert!(st.pong_time_us >= before);
    }

    /// MTU-Response-Handler: is_video + frame_index == mtu_id.
    #[test]
    fn state_machine_mtu_response() {
        let senkusha = Senkusha::new();
        let shared = &senkusha.shared;
        lock_state(shared).mtu_id = 4;
        senkusha.set_state(SenkushaState::ExpectMtu);

        // nicht video -> nicht finished
        let pkt = AVPacket {
            is_video: false,
            frame_index: 4,
            data: vec![0; 8],
            ..Default::default()
        };
        senkusha_takion_av(shared, &pkt);
        assert!(!senkusha.state_finished());

        // video, falsche frame_index -> nicht finished
        let pkt = AVPacket {
            is_video: true,
            frame_index: 9,
            data: vec![0; 8],
            ..Default::default()
        };
        senkusha_takion_av(shared, &pkt);
        assert!(!senkusha.state_finished());

        // video mit mtu_id -> finished
        let pkt = AVPacket {
            is_video: true,
            frame_index: 4,
            data: vec![0; 8],
            ..Default::default()
        };
        senkusha_takion_av(shared, &pkt);
        assert!(senkusha.state_finished());
    }

    /// DataAck-Handler: nur mit passender Seq-Num.
    #[test]
    fn state_machine_data_ack() {
        let senkusha = Senkusha::new();
        let shared = &senkusha.shared;
        senkusha.set_state(SenkushaState::ExpectDataAck);
        lock_state(shared).data_ack_seq_num_expected = 42;

        senkusha_takion_data_ack(shared, 41);
        assert!(!senkusha.state_finished());
        senkusha_takion_data_ack(shared, 42);
        assert!(senkusha.state_finished());
    }

    /// wait_state_finished bricht mit stop() ab (C: state_finished_cond_check).
    #[test]
    fn wait_broken_by_stop() {
        let senkusha = Senkusha::new();

        let s2 = Arc::clone(&senkusha.shared);
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            lock_state(&s2).should_stop = true;
            s2.1.notify_all();
        });

        let t = Instant::now();
        // C: err == SUCCESS, state_finished false, should_stop -> Canceled
        let err = senkusha.wait_state_finished(10_000);
        assert_eq!(err, ChiakiError::Success);
        assert!(!senkusha.state_finished());
        assert!(senkusha.should_stop());
        assert!(t.elapsed() < Duration::from_secs(1));
        handle.join().unwrap();
    }

    /// wait_state_finished: Timeout nach Ablauf.
    #[test]
    fn wait_times_out() {
        let senkusha = Senkusha::new();
        let t = Instant::now();
        let err = senkusha.wait_state_finished(30);
        assert_eq!(err, ChiakiError::Timeout);
        assert!(t.elapsed() >= Duration::from_millis(25));
    }

    /// Connected-Event im STATE_TAKION_CONNECT -> state_finished.
    #[test]
    fn takion_connect_event_finishes_connect_state() {
        let senkusha = Senkusha::new();
        let shared = Arc::clone(&senkusha.shared);
        senkusha.set_state(SenkushaState::TakionConnect);

        senkusha_takion_cb(&shared, TakionEvent::Connected);
        assert!(senkusha.state_finished());

        // Disconnect wird nur im TakionConnect-State wirksam
        senkusha.set_state(SenkushaState::ExpectBang);
        senkusha_takion_cb(&shared, TakionEvent::Disconnect(ChiakiError::Disconnected));
        assert!(!lock_state(&senkusha.shared).state_failed);
        senkusha.set_state(SenkushaState::TakionConnect);
        senkusha_takion_cb(&shared, TakionEvent::Disconnect(ChiakiError::Disconnected));
        let st = lock_state(&senkusha.shared);
        assert!(!st.state_finished);
        assert!(st.state_failed);
    }

    /// Senkusha::stop() weckt wait_state_finished und der Flow bricht ab.
    #[test]
    fn stop_flag_is_set_via_stop() {
        let senkusha = Senkusha::new();
        assert!(!senkusha.should_stop());
        senkusha.stop();
        assert!(senkusha.should_stop());
        // wait kehrt sofort mit SUCCESS zurück (Prädikat), Flow mappt auf Canceled
        assert_eq!(senkusha.wait_state_finished(10_000), ChiakiError::Success);
    }
}
