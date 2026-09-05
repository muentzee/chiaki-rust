// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/videoreceiver.c + lib/include/chiaki/videoreceiver.h (chiaki-ng).
//
// Video-Frame-Zusammenbau über den FrameProcessor (alloc_frame/put_unit/flush,
// inkl. FEC), Bitstream-Analyse (IDR-Warten, P-Frame-Referenz-Reparatur) und
// Auslieferung kompletter Frames an den Besitzer (C: session->video_sample_cb).
//
// Querverweise, die im C über `session`/`stream_connection` laufen, sind hier
// Callbacks im Konstruktor:
// - video_sample      <- session->video_sample_cb (Rückgabe = Decode-Erfolg;
//                        false ⇒ frame_index_prev_complete wird nicht
//                        weitergeschoben ⇒ nächster Frame meldet corrupt)
// - send_corrupt_frame <- stream_connection_send_corrupt_frame(start, end)
// - send_idr_request   <- stream_connection_send_idr_request()
// - video_fec_failure  <- ChiakiEvent CHIAKI_EVENT_VIDEO_FEC_FAILURE
//
// Das IDR-Request-Verhalten selbst bleibt (wie im C) hier: bei FEC-Failure mit
// enable_idr_on_fec_failure wird genau ein IDR-Request geschickt und auf den
// nächsten I-Frame gewartet; P-Frames werden solange übersprungen.
//
// Abweichungen gegenüber C: der gesamte Receiver-Zustand hängt an einer Mutex
// (im C ungeschützt, läuft aber faktisch alles im Takion-Recv-Thread);
// `waiting_for_idr` und `frames_lost(_total)` behalten wie im C je eigene
// Mutexe (Cross-Thread-Zugriff aus GUI/StreamConnection). `fini` entfällt.
// Der FrameProcessor liefert den gerollten Frame nur als `&[u8]` — die
// P-Frame-Referenz-Reparatur (im C in-place) arbeitet deshalb einmalig auf
// einer Kopie, die dann dem Decoder übergeben wird.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::bitstream::{Bitstream, Slice, SliceType};
use crate::error::{ChiakiError, ChiakiResult, Codec};
use crate::frameprocessor::{FlushResult, FrameProcessor};
use crate::packetstats::PacketStats;
use crate::seqnum::{seq_num_16_gt, seq_num_16_lt};
use crate::takion::AVPacket;
use crate::video::VideoProfile;

/// C: `#define CHIAKI_VIDEO_PROFILES_MAX 8`
pub const VIDEO_PROFILES_MAX: usize = 8;

/// Port von `ChiakiVideoReceiverCallbacks`-Äquivalenten (im C: Session-Felder
/// bzw. StreamConnection-Aufrufe). Alle optional.
#[derive(Clone, Default)]
pub struct VideoReceiverCallbacks {
    /// C: `session->video_sample_cb(frame, frame_size, frames_lost, recovered, user) -> bool`.
    /// Die Rückgabe meldet, ob der Decoder den Frame übernommen hat.
    pub video_sample: Option<Arc<dyn Fn(&[u8], i32, bool) -> bool + Send + Sync>>,
    /// C: `stream_connection_send_corrupt_frame(sc, start, end)`.
    pub send_corrupt_frame: Option<Arc<dyn Fn(u16, u16) -> ChiakiResult<()> + Send + Sync>>,
    /// C: `stream_connection_send_idr_request(sc)`.
    pub send_idr_request: Option<Arc<dyn Fn() -> ChiakiResult<()> + Send + Sync>>,
    /// C: `ChiakiEvent` `CHIAKI_EVENT_VIDEO_FEC_FAILURE` → (frame_index, idr_request_sent).
    pub video_fec_failure: Option<Arc<dyn Fn(i32, bool) + Send + Sync>>,
}

/// C: `frames_lost` + `frames_lost_total` (gemeinsam über eine Mutex geschützt).
#[derive(Default)]
struct FramesLost {
    frames_lost: i32,
    frames_lost_total: i32,
}

/// Mutabler Zustand (C: Felder von ChiakiVideoReceiver).
struct VideoReceiverState {
    profiles: Vec<VideoProfile>,
    /// < 0 if no profile selected yet, else index in profiles (C-Kommentar)
    profile_cur: i32,

    /// frame that is currently being filled (-1 = keins)
    frame_index_cur: i32,
    /// last frame that has been at least partially decoded
    frame_index_prev: i32,
    /// last frame that has been completely decoded
    frame_index_prev_complete: i32,

    frame_processor: FrameProcessor,

    reference_frames: [i32; 16],
    bitstream: Bitstream,
}

/// Port von `ChiakiVideoReceiver`.
pub struct VideoReceiver {
    callbacks: VideoReceiverCallbacks,
    /// C: `session->connect_info.enable_idr_on_fec_failure`
    enable_idr_on_fec_failure: bool,
    packet_stats: Option<Arc<PacketStats>>,

    waiting_for_idr: Mutex<bool>,
    frames_lost_mutex: Mutex<FramesLost>,
    state: Mutex<VideoReceiverState>,
}

fn lock(mutex: &Mutex<VideoReceiverState>) -> MutexGuard<'_, VideoReceiverState> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl VideoReceiver {
    /// Port von `chiaki_video_receiver_init`. Im C kommen `codec` aus
    /// `connect_info.video_profile.codec` und `enable_idr_on_fec_failure` aus
    /// `connect_info`; `session`-Abhängigkeiten sind die `callbacks`.
    pub fn new(
        codec: Codec,
        enable_idr_on_fec_failure: bool,
        packet_stats: Option<Arc<PacketStats>>,
        callbacks: VideoReceiverCallbacks,
    ) -> Self {
        VideoReceiver {
            callbacks,
            enable_idr_on_fec_failure,
            packet_stats,
            waiting_for_idr: Mutex::new(false),
            frames_lost_mutex: Mutex::new(FramesLost::default()),
            state: Mutex::new(VideoReceiverState {
                profiles: Vec::new(),
                profile_cur: -1,
                frame_index_cur: -1,
                frame_index_prev: -1,
                frame_index_prev_complete: 0,
                frame_processor: FrameProcessor::new(),
                reference_frames: [-1; 16],
                bitstream: Bitstream::new(codec),
            }),
        }
    }

    /// Port von `chiaki_video_receiver_set_waiting_for_idr`.
    pub fn set_waiting_for_idr(&self, waiting_for_idr: bool) {
        *self.waiting_for_idr.lock().unwrap_or_else(PoisonError::into_inner) = waiting_for_idr;
    }

    /// Port von `chiaki_video_receiver_get_waiting_for_idr`.
    pub fn get_waiting_for_idr(&self) -> bool {
        *self.waiting_for_idr.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Port von `chiaki_video_receiver_get_frames_lost_total`.
    pub fn get_frames_lost_total(&self) -> i32 {
        self.frames_lost_mutex
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .frames_lost_total
    }

    /// Port von `chiaki_video_receiver_stream_info`: übernimmt die Profile
    /// (Ownership der Header, wie im C).
    ///
    /// Muss vor den ersten AV-Packets gerufen werden; ein zweiter Aufruf wird
    /// wie im C mit Fehler-Log ignoriert.
    pub fn stream_info(&self, profiles: Vec<VideoProfile>) {
        let mut st = lock(&self.state);

        if !st.profiles.is_empty() {
            tracing::error!("Video Receiver profiles already set");
            return;
        }

        // C verlangt profiles_count <= CHIAKI_VIDEO_PROFILES_MAX (memcpy wäre
        // sonst UB); hier defensiv beschneiden statt UB.
        let mut profiles = profiles;
        if profiles.len() > VIDEO_PROFILES_MAX {
            tracing::warn!(
                "Video Receiver got {} profiles, truncating to {}",
                profiles.len(),
                VIDEO_PROFILES_MAX
            );
            profiles.truncate(VIDEO_PROFILES_MAX);
        }

        tracing::info!("Video Profiles:");
        for (i, profile) in profiles.iter().enumerate() {
            tracing::info!("  {}: {}x{}", i, profile.width, profile.height);
        }

        st.profiles = profiles;
    }

    /// Port von `chiaki_video_receiver_av_packet`.
    pub fn av_packet(&self, packet: &AVPacket) {
        let mut st = lock(&self.state);

        // old frame?
        let frame_index = packet.frame_index;
        if st.frame_index_cur >= 0 && seq_num_16_lt(frame_index, st.frame_index_cur as u16) {
            tracing::warn!("Video Receiver received old frame packet");
            return;
        }

        // check adaptive stream index
        if st.profile_cur < 0 || st.profile_cur != packet.adaptive_stream_index as i32 {
            if packet.adaptive_stream_index as usize >= st.profiles.len() {
                tracing::error!(
                    "Packet has invalid adaptive stream index {} >= {}",
                    packet.adaptive_stream_index,
                    st.profiles.len()
                );
                return;
            }
            st.profile_cur = packet.adaptive_stream_index as i32;

            let profile = &st.profiles[st.profile_cur as usize];
            tracing::info!(
                "Switched to profile {}, resolution: {}x{}",
                st.profile_cur,
                profile.width,
                profile.height
            );
            // C: video_sample_cb mit dem Profil-Header (SPS/PPS/VPS), Rückgabewert
            // wird ignoriert, frames_lost = 0, recovered = false.
            // Header kopiert, damit die profiles-Borrow endet bevor der
            // (mutable) Bitstream-State angefasst wird.
            let profile_header = profile.header.clone();
            if let Some(cb) = self.callbacks.video_sample.clone() {
                cb(&profile_header, 0, false);
            }
            if !st.bitstream.header(&profile_header) {
                tracing::warn!("Failed to parse video header");
            }
        }

        // next frame?
        if st.frame_index_cur < 0 || seq_num_16_gt(frame_index, st.frame_index_cur as u16) {
            if let Some(packet_stats) = &self.packet_stats {
                st.frame_processor.report_packet_stats(packet_stats);
            }

            // last frame not flushed yet?
            if st.frame_index_cur >= 0 && st.frame_index_prev != st.frame_index_cur
                && self.flush_frame(&mut st).is_err()
            {
                tracing::warn!("Video receiver could not flush frame.");
            }

            let next_frame_expected = (st.frame_index_prev_complete + 1) as u16;
            if seq_num_16_gt(frame_index, next_frame_expected)
                && !(frame_index == 1 && st.frame_index_cur < 0) // ok for frame 1 (C-Kommentar)
            {
                tracing::warn!(
                    "Detected missing or corrupt frame(s) from {} to {}",
                    next_frame_expected,
                    frame_index
                );
                let err = self
                    .callbacks
                    .send_corrupt_frame
                    .as_ref()
                    .map(|send| send(next_frame_expected, frame_index.wrapping_sub(1)))
                    .unwrap_or(Ok(()));
                if err.is_err() {
                    tracing::warn!("Error sending corrupt frame.");
                }
            }

            st.frame_index_cur = frame_index as i32;
            if st.frame_processor.alloc_frame(packet).is_err() {
                tracing::warn!("Video receiver could not allocate frame for packet.");
            }
        }

        if st.frame_processor.put_unit(packet).is_err() {
            tracing::warn!("Video receiver could not put unit.");
        }

        // if we are currently building up a frame (C-Kommentar); flush,
        // wenn genug für den ganzen Frame da ist (C-Kommentar)
        if st.frame_index_cur != st.frame_index_prev
            && (st.frame_processor.flush_possible()
                || packet.unit_index == packet.units_in_frame_total.wrapping_sub(1))
        {
            if self.flush_frame(&mut st).is_err() {
                tracing::warn!("Video receiver could not flush frame.");
            }
        }
    }

    /// Port von `chiaki_video_receiver_flush_frame` (static).
    ///
    /// Wird (wie das gesamte av_packet) unter der State-Mutex ausgeführt; die
    /// Callbacks rufen im C dieselbe Stelle ohne zusätzliche Locks auf.
    fn flush_frame(&self, st: &mut VideoReceiverState) -> ChiakiResult<()> {
        let (flush_result, frame) = st.frame_processor.flush();

        if matches!(flush_result, FlushResult::Failed | FlushResult::FecFailed) {
            if flush_result == FlushResult::FecFailed {
                let next_frame_expected = (st.frame_index_prev_complete + 1) as u16;
                // C ignoriert den Fehler dieses Aufrufs.
                if let Some(send) = &self.callbacks.send_corrupt_frame {
                    let _ = send(next_frame_expected, st.frame_index_cur as u16);
                }
                if self.enable_idr_on_fec_failure {
                    let waiting_for_idr = self.get_waiting_for_idr();
                    // C: bool idr_request_sent = waiting_for_idr;
                    let mut idr_request_sent = waiting_for_idr;
                    if !waiting_for_idr {
                        match self.callbacks.send_idr_request.as_ref().map(|send| send()) {
                            Some(Ok(())) => {
                                // C: idr_request_sent = err == CHIAKI_ERR_SUCCESS;
                                idr_request_sent = true;
                                self.set_waiting_for_idr(true);
                                tracing::info!("FEC failed, waiting for IDR frame");
                            }
                            Some(Err(err)) => {
                                idr_request_sent = false;
                                tracing::warn!("FEC failed and IDR request could not be sent: {}", err);
                            }
                            // Kein Callback registriert: wie Fehlerfall, ohne Log.
                            None => idr_request_sent = false,
                        }
                    } else {
                        tracing::warn!("Video FEC failure, already waiting for requested IDR");
                    }
                    if let Some(cb) = &self.callbacks.video_fec_failure {
                        cb(st.frame_index_cur, idr_request_sent);
                    }
                }
                // C: int32_t lost = frame_index_cur - (u16)next_frame_expected + 1;
                let lost = st.frame_index_cur - next_frame_expected as i32 + 1;
                {
                    let mut fl = self.frames_lost_mutex.lock().unwrap_or_else(PoisonError::into_inner);
                    fl.frames_lost += lost;
                    fl.frames_lost_total += lost;
                }
                st.frame_index_prev = st.frame_index_cur;
            }
            tracing::warn!("Failed to complete frame {}", st.frame_index_cur);
            return Err(ChiakiError::Unknown);
        }

        // C: `succ = flush_result != FEC_FAILED` — nach dem Early-Return oben
        // immer true (SUCCESS oder FEC_SUCCESS).
        let mut succ = true;
        let mut recovered = false;
        // Nur gesetzt, wenn die Referenz-Reparatur den Frame verändert hat
        // (siehe Kommentar unten; im C wird in-place in den Frame-Buffer
        // geschrieben).
        let mut delivered_frame: Option<Vec<u8>> = None;

        let mut slice = Slice::default();
        let slice_parsed = st.bitstream.slice(frame, &mut slice);

        if slice_parsed {
            if self.get_waiting_for_idr() {
                if slice.slice_type == SliceType::I {
                    self.set_waiting_for_idr(false);
                    tracing::info!("Received IDR frame, resuming decode");
                } else {
                    tracing::trace!("Skipping P-frame {} while waiting for IDR", st.frame_index_cur);
                    st.frame_index_prev = st.frame_index_cur;
                    return Ok(());
                }
            }

            if slice.slice_type == SliceType::P {
                // C: ChiakiSeqNum16 ref_frame_index = frame_index_cur - reference_frame - 1;
                // (u32-Arithmetik mit Wrap, danach u16-Truncation)
                let ref_frame_index = (st.frame_index_cur as u32)
                    .wrapping_sub(slice.reference_frame)
                    .wrapping_sub(1) as u16;
                if slice.reference_frame != 0xff && !have_ref_frame(&st.reference_frames, ref_frame_index as i32) {
                    // Abweichung zum C: der FrameProcessor liefert den Frame nur
                    // als &[u8] (nicht &mut). Die Referenz-Reparatur schreibt im
                    // C in-place in den Frame-Buffer — hier wird dafür einmalig
                    // eine Kopie angelegt, die (wie im C) der Decoder erhält.
                    let mut recovered_frame: Vec<u8> = Vec::new();
                    // C: for(unsigned i = reference_frame + 1; i < 16; i++)
                    for i in slice.reference_frame.wrapping_add(1)..16 {
                        let ref_frame_index_new = (st.frame_index_cur as u32)
                            .wrapping_sub(i)
                            .wrapping_sub(1) as u16;
                        if have_ref_frame(&st.reference_frames, ref_frame_index_new as i32) {
                            if recovered_frame.is_empty() {
                                recovered_frame = frame.to_vec();
                            }
                            if st.bitstream.slice_set_reference_frame(&mut recovered_frame, i) {
                                recovered = true;
                                tracing::warn!(
                                    "Missing reference frame {} for decoding frame {} -> changed to {}",
                                    ref_frame_index,
                                    st.frame_index_cur,
                                    ref_frame_index_new
                                );
                            }
                            break;
                        }
                    }
                    if !recovered {
                        succ = false;
                        {
                            let mut fl = self.frames_lost_mutex.lock().unwrap_or_else(PoisonError::into_inner);
                            fl.frames_lost += 1;
                            fl.frames_lost_total += 1;
                        }
                        tracing::warn!(
                            "Missing reference frame {} for decoding frame {}",
                            ref_frame_index,
                            st.frame_index_cur
                        );
                    } else {
                        delivered_frame = Some(recovered_frame);
                    }
                }
            }
        }

        if succ {
            if let Some(cb) = self.callbacks.video_sample.clone() {
                let frames_lost = self
                    .frames_lost_mutex
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .frames_lost;
                let frame_bytes: &[u8] = delivered_frame
                    .as_deref()
                    .unwrap_or(frame);
                let cb_succ = cb(frame_bytes, frames_lost, recovered);
                {
                    let mut fl = self.frames_lost_mutex.lock().unwrap_or_else(PoisonError::into_inner);
                    fl.frames_lost = 0;
                }
                if !cb_succ {
                    succ = false;
                    tracing::warn!("Video callback did not process frame successfully.");
                } else {
                    add_ref_frame(&mut st.reference_frames, st.frame_index_cur);
                    tracing::trace!(
                        "Added reference {} frame {}",
                        if slice.slice_type == SliceType::I { 'I' } else { 'P' },
                        st.frame_index_cur
                    );
                }
            }
        }

        st.frame_index_prev = st.frame_index_cur;

        if succ {
            st.frame_index_prev_complete = st.frame_index_cur;
        }

        Ok(())
    }
}

/// Port von `add_ref_frame` (static in videoreceiver.c).
fn add_ref_frame(reference_frames: &mut [i32; 16], frame: i32) {
    if reference_frames[0] != -1 {
        // C: memmove(&rf[1], &rf[0], sizeof(int32_t) * 15)
        reference_frames.copy_within(0..15, 1);
        reference_frames[0] = frame;
        return;
    }
    for i in (0..16).rev() {
        if reference_frames[i] == -1 {
            reference_frames[i] = frame;
            return;
        }
    }
}

/// Port von `have_ref_frame` (static in videoreceiver.c).
fn have_ref_frame(reference_frames: &[i32; 16], frame: i32) -> bool {
    reference_frames.contains(&frame)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    const UNIT_PAYLOAD_LEN: usize = 14;

    /// Video-Unit: 2-Byte-Big-Endian-"Padding/Extension"-Wort (= 0) + Payload.
    /// Der FrameProcessor liest bei is_video die ersten 2 Bytes als
    /// buf_size_per_unit-Erweiterung und schneidet sie beim Flush ab.
    fn video_unit(payload_tag: u8) -> Vec<u8> {
        let mut unit = vec![0u8, 0u8];
        unit.extend(std::iter::repeat_n(payload_tag, UNIT_PAYLOAD_LEN));
        unit
    }

    fn make_av_packet(
        frame_index: u16,
        unit_index: u16,
        units_in_frame_total: u16,
        units_in_frame_fec: u16,
        data: Vec<u8>,
    ) -> AVPacket {
        AVPacket {
            packet_index: frame_index,
            frame_index,
            uses_nalu_info_structs: false,
            is_video: true,
            is_haptics: false,
            unit_index,
            units_in_frame_total,
            units_in_frame_fec,
            codec: 0,
            word_at_0x18: 0,
            adaptive_stream_index: 0,
            byte_at_0x2c: 0,
            key_pos: 0,
            data,
        }
    }

    struct Harness {
        receiver: VideoReceiver,
        stats: Arc<PacketStats>,
        frames: Arc<StdMutex<Vec<(Vec<u8>, i32, bool)>>>,
        corrupt: Arc<StdMutex<Vec<(u16, u16)>>>,
        idr_requests: Arc<StdMutex<usize>>,
        fec_failures: Arc<StdMutex<Vec<(i32, bool)>>>,
    }

    fn make_harness(enable_idr_on_fec_failure: bool, sample_result: bool) -> Harness {
        let frames: Arc<StdMutex<Vec<(Vec<u8>, i32, bool)>>> = Arc::new(StdMutex::new(Vec::new()));
        let corrupt: Arc<StdMutex<Vec<(u16, u16)>>> = Arc::new(StdMutex::new(Vec::new()));
        let idr_requests: Arc<StdMutex<usize>> = Arc::new(StdMutex::new(0));
        let fec_failures: Arc<StdMutex<Vec<(i32, bool)>>> = Arc::new(StdMutex::new(Vec::new()));

        let callbacks = VideoReceiverCallbacks {
            video_sample: {
                let frames = frames.clone();
                Some(Arc::new(move |buf: &[u8], frames_lost: i32, recovered: bool| {
                    frames.lock().unwrap().push((buf.to_vec(), frames_lost, recovered));
                    sample_result
                }))
            },
            send_corrupt_frame: {
                let corrupt = corrupt.clone();
                Some(Arc::new(move |start: u16, end: u16| {
                    corrupt.lock().unwrap().push((start, end));
                    Ok(())
                }))
            },
            send_idr_request: {
                let idr_requests = idr_requests.clone();
                Some(Arc::new(move || {
                    *idr_requests.lock().unwrap() += 1;
                    Ok(())
                }))
            },
            video_fec_failure: {
                let fec_failures = fec_failures.clone();
                Some(Arc::new(move |frame_index: i32, idr_request_sent: bool| {
                    fec_failures.lock().unwrap().push((frame_index, idr_request_sent));
                }))
            },
        };

        let stats = Arc::new(PacketStats::new());
        let receiver = VideoReceiver::new(Codec::H264, enable_idr_on_fec_failure, Some(stats.clone()), callbacks);
        receiver.stream_info(vec![VideoProfile {
            width: 1280,
            height: 720,
            header: vec![0x67, 0x42, 0xc0, 0x1e],
        }]);

        Harness {
            receiver,
            stats,
            frames,
            corrupt,
            idr_requests,
            fec_failures,
        }
    }

    /// FEC-Unit für 3 Source-Units (à 16 Bytes, stride 16) mit k=3, m=1 erzeugen.
    fn make_fec_unit(source_units: [&Vec<u8>; 3]) -> Vec<u8> {
        let unit_size = 16usize;
        let mut frame_buf = vec![0u8; unit_size * 4];
        for (i, unit) in source_units.iter().enumerate() {
            frame_buf[i * unit_size..(i + 1) * unit_size].copy_from_slice(unit);
        }
        crate::fec::encode(&mut frame_buf, unit_size, unit_size, 3, 1).expect("fec encode");
        frame_buf[3 * unit_size..4 * unit_size].to_vec()
    }

    #[test]
    fn frame_from_4_units_with_missing_unit_recovered_by_fec() {
        let h = make_harness(false, true);

        let u0 = video_unit(0xa0);
        let u1 = video_unit(0xa1);
        let u2 = video_unit(0xa2);
        let u3 = make_fec_unit([&u0, &u1, &u2]);

        // Unit 2 (index 2) fehlt — wird über FEC rekonstruiert.
        h.receiver.av_packet(&make_av_packet(0, 0, 4, 1, u0.clone()));
        h.receiver.av_packet(&make_av_packet(0, 1, 4, 1, u1.clone()));
        // Unit 2 fehlt absichtlich.
        h.receiver.av_packet(&make_av_packet(0, 3, 4, 1, u3));

        // Flush passiert automatisch (genug Units für den Frame da):
        // frames[0] = Profil-Header (Profile-Switch), frames[1] = der Frame.
        let got = h.frames.lock().unwrap().clone();
        assert_eq!(got.len(), 2);
        let (frame, frames_lost, recovered) = &got[1];
        let expected: Vec<u8> = [vec![0xa0; 14], vec![0xa1; 14], vec![0xa2; 14]].concat();
        assert_eq!(frame, &expected); // vollständiger Frame (3 × 14 Payload-Bytes)
        assert_eq!(*frames_lost, 0);
        // FEC-rekonstruiert, aber "recovered" meint Bitstream-Referenzreparatur
        assert!(!recovered);
        assert_eq!(h.receiver.get_frames_lost_total(), 0);
        assert!(h.corrupt.lock().unwrap().is_empty());
        assert_eq!(*h.idr_requests.lock().unwrap(), 0);
    }

    #[test]
    fn packet_stats_generation_counting() {
        let h = make_harness(false, true);

        // Frame 0 komplett (4 Units, Unit 3 = FEC) → Flush bei Unit 3.
        let u0 = video_unit(0xa0);
        let u1 = video_unit(0xa1);
        let u2 = video_unit(0xa2);
        let u3 = make_fec_unit([&u0, &u1, &u2]);
        h.receiver.av_packet(&make_av_packet(0, 0, 4, 1, u0));
        h.receiver.av_packet(&make_av_packet(0, 1, 4, 1, u1));
        h.receiver.av_packet(&make_av_packet(0, 2, 4, 1, u2));
        h.receiver.av_packet(&make_av_packet(0, 3, 4, 1, u3));

        // Noch keine Generation gepusht (report passiert erst beim nächsten Frame).
        let (received, lost) = h.stats.get(false);
        assert_eq!((received, lost), (0, 0));

        // Frame 1: next-frame branch → report_packet_stats über Frame-0-Zähler:
        // alle 4 Units kamen an (3 Source + FEC-Ersatz) → received=4, lost=0.
        let f1 = video_unit(0xb0);
        h.receiver.av_packet(&make_av_packet(1, 0, 1, 0, f1));

        let (received, lost) = h.stats.get(false);
        assert_eq!((received, lost), (4, 0));

        // Frame 2: report über Frame-1-Zähler: 1 received, expected 1+1
        // (fec_expected wird auf min. 1 geklemmt) → received=1, lost=1.
        let f2 = video_unit(0xc0);
        h.receiver.av_packet(&make_av_packet(2, 0, 1, 0, f2));

        let (received, lost) = h.stats.get(true);
        assert_eq!((received, lost), (5, 1)); // kumuliert (get(true) resettet)
    }

    #[test]
    fn missing_frame_reports_corrupt_frame() {
        let h = make_harness(false, true);

        // Frame 0 komplett.
        let u0 = video_unit(0xa0);
        let u1 = video_unit(0xa1);
        let u2 = video_unit(0xa2);
        let u3 = make_fec_unit([&u0, &u1, &u2]);
        for (idx, data) in [(0u16, u0), (1, u1), (2, u2), (3, u3)] {
            h.receiver.av_packet(&make_av_packet(0, idx, 4, 1, data));
        }

        // Frame 3: Lücke 1..2 → corrupt report (1, 2).
        h.receiver.av_packet(&make_av_packet(3, 0, 1, 0, video_unit(0xd0)));

        let corrupt = h.corrupt.lock().unwrap().clone();
        assert_eq!(corrupt, vec![(1, 2)]);

        // frame_index_prev_complete bleibt 0, bis Frame 3 komplett ist:
        // danach → nächster Sprung würde (4, ...) melden.
        assert_eq!(h.receiver.get_frames_lost_total(), 0);
    }

    #[test]
    fn old_frame_packet_is_dropped() {
        let h = make_harness(false, true);

        // Frame 5 direkt (frame 1 wäre der erlaubte Sonderfall): die Lücke
        // 1..4 wird beim ersten Frame gemeldet (C: send_corrupt_frame(
        // next_frame_expected=1, frame_index-1=4)).
        h.receiver.av_packet(&make_av_packet(5, 0, 1, 0, video_unit(0xa0)));
        // frames[0] = Profil-Header, frames[1] = Frame 5.
        let frames_after_first = h.frames.lock().unwrap().len();
        assert_eq!(frames_after_first, 2);
        assert_eq!(*h.corrupt.lock().unwrap(), vec![(1, 4)]);
        h.corrupt.lock().unwrap().clear();

        let (received_before, _) = h.stats.get(false);

        // Altes Frame (frame_index < cur) → komplett ignoriert:
        // kein Callback, kein Stats-Report, kein Corrupt-Report.
        h.receiver.av_packet(&make_av_packet(1, 0, 1, 0, video_unit(0xff)));

        assert_eq!(h.frames.lock().unwrap().len(), frames_after_first);
        let (received_after, _) = h.stats.get(false);
        assert_eq!(received_after, received_before);
        assert!(h.corrupt.lock().unwrap().is_empty());
    }

    #[test]
    fn fec_failure_requests_idr_and_counts_lost() {
        let h = make_harness(true, true);

        // Frame 0 komplett (4 Units) → prev_complete = 0.
        let u0 = video_unit(0xa0);
        let u1 = video_unit(0xa1);
        let u2 = video_unit(0xa2);
        let u3 = make_fec_unit([&u0, &u1, &u2]);
        for (idx, data) in [(0u16, u0), (1, u1), (2, u2), (3, u3)] {
            h.receiver.av_packet(&make_av_packet(0, idx, 4, 1, data));
        }

        // Frame 2: 2 Source + 1 FEC, nur die FEC-Unit (index 2 = letzte Unit)
        // kommt an → FEC mit 2 Erasures → FEC_FAILED.
        h.receiver.av_packet(&make_av_packet(2, 2, 3, 1, video_unit(0xbe)));

        // Erwartungen:
        // - corrupt report doppelt: (a) Lückenerkennung in av_packet:
        //   send_corrupt_frame(next=1, frame_index-1=1) → (1, 1),
        //   (b) FEC-Failure-Pfad: (prev_complete+1=1, cur=2) → (1, 2)
        // - genau ein IDR-Request, waiting_for_idr = true
        // - FEC-Failure-Event (frame_index=2, idr_request_sent=true)
        // - frames_lost_total = 2 (Frame 1 und 2)
        assert_eq!(*h.corrupt.lock().unwrap(), vec![(1, 1), (1, 2)]);
        assert_eq!(*h.idr_requests.lock().unwrap(), 1);
        assert!(h.receiver.get_waiting_for_idr());
        assert_eq!(*h.fec_failures.lock().unwrap(), vec![(2, true)]);
        assert_eq!(h.receiver.get_frames_lost_total(), 2);

        // Der unvollständige Frame wird nicht an video_sample geliefert:
        // frames[0] = Profil-Header, frames[1] = Frame 0.
        assert_eq!(h.frames.lock().unwrap().len(), 2);
    }

    #[test]
    fn video_sample_false_blocks_prev_complete() {
        let h = make_harness(false, false); // Decoder "versagt" bei jedem Frame

        // Frame 0 komplett, aber Callback meldet Fehler.
        let u0 = video_unit(0xa0);
        let u1 = video_unit(0xa1);
        let u2 = video_unit(0xa2);
        let u3 = make_fec_unit([&u0, &u1, &u2]);
        for (idx, data) in [(0u16, u0), (1, u1), (2, u2), (3, u3)] {
            h.receiver.av_packet(&make_av_packet(0, idx, 4, 1, data));
        }

        // frame_index_prev_complete bleibt 0 → Frame 2 meldet corrupt (1, 1):
        h.receiver.av_packet(&make_av_packet(2, 0, 1, 0, video_unit(0xc0)));
        assert_eq!(*h.corrupt.lock().unwrap(), vec![(1, 1)]);
    }

    #[test]
    fn profile_switch_sends_header_and_rejects_invalid_index() {
        let h = make_harness(false, true);

        // Ungültiger adaptive_stream_index → Packet wird ignoriert.
        let mut packet = make_av_packet(0, 0, 1, 0, video_unit(0xa0));
        packet.adaptive_stream_index = 5;
        h.receiver.av_packet(&packet);
        assert!(h.frames.lock().unwrap().is_empty());

        // Gültiger Stream (Index 0) → Profil-Header wird als erstes "Sample"
        // geliefert (C: video_sample_cb(profile->header, 0, false)).
        let packet = make_av_packet(0, 0, 1, 0, video_unit(0xa0));
        h.receiver.av_packet(&packet);

        let got = h.frames.lock().unwrap().clone();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, vec![0x67, 0x42, 0xc0, 0x1e]); // Profil-Header
        assert_eq!(got[1].0, vec![0xa0; 14]); // erster Frame
    }

    #[test]
    fn stream_info_twice_is_rejected() {
        let h = make_harness(false, true);
        // Zweiter stream_info-Aufruf wird ignoriert (C: "profiles already set").
        h.receiver
            .stream_info(vec![VideoProfile { width: 1, height: 1, header: vec![1] }]);
        // Kein Crash, Zustand unverändert — funktional über av_packet prüfbar:
        let packet = make_av_packet(0, 0, 1, 0, video_unit(0xa0));
        h.receiver.av_packet(&packet);
        let got = h.frames.lock().unwrap().clone();
        // Erst Profil-Header, dann Frame (1279x720-Profil aus dem Harness).
        assert_eq!(got[0].0, vec![0x67, 0x42, 0xc0, 0x1e]);
    }
}
