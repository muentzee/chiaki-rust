// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/audioreceiver.c + lib/include/chiaki/audioreceiver.h (chiaki-ng).
//
// WICHTIG (aus dem C-Code gelesen, keine Annahmen): der chiaki-ng
// AudioReceiver benutzt KEINEN FrameProcessor und KEINE ReorderQueue.
// Stattdessen werden die einzelnen Units eines Audio-AV-Packets über
// abgeleitete 16-bit-Frame-Indizes (SeqNum-Serial-Arithmetic) in einen
// 8-Slot-Jitter-Buffer einsortiert:
//
//   Source-Unit i  -> frame_index = packet.frame_index + i
//   FEC-Unit j     -> frame_index = packet.frame_index - units_in_frame_fec + j
//
// Eine FEC-Unit belegt damit genau den Jitter-Slot des (evtl. verlorenen)
// Frames mit demselben Index — die FEC-"Rekonstruktion" läuft hier nicht über
// chiaki_fec_decode, sondern über diese Index-Verschiebung. Duplikate
// (gleicher Frame-Index) werden verworfen. Vor Start (Prefill 3) und bei
// Lücken mit Lookahead wird Concealment geliefert (C: frame_cb(NULL, 0)).
//
// Abweichungen gegenüber C (nur Speicherverwaltung/Threading, verhaltenidentisch):
// - Statt `session->audio_sink` / `session->haptics_sink` werden die Callbacks
//   als Arc<dyn Fn> im Konstruktor übergeben (Callback mit `user` entfällt).
// - Concealment ruft frame_cb mit leerem Slice `&[]` statt (NULL, 0).
// - `fini`/`free` entfällt (Drop); C-Mutex-Fehlerpfade entfallen (std-Mutex).

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::audio::AudioHeader;
use crate::packetstats::PacketStats;
use crate::seqnum::{seq_num_16_gt, seq_num_16_lt};
use crate::takion::AVPacket;

/// C: `#define CHIAKI_AUDIO_JITTER_PREFILL 3`
const AUDIO_JITTER_PREFILL: usize = 3;
/// C: `#define CHIAKI_AUDIO_JITTER_BUFFER_SIZE 8`
const AUDIO_JITTER_BUFFER_SIZE: usize = 8;

/// Port von `ChiakiAudioSink` (audioreceiver.h): Sink für Opus-Audio.
#[derive(Clone, Default)]
pub struct AudioSink {
    /// C: `ChiakiAudioSinkHeader header_cb` — wird mit dem Stream-Info-Header gerufen.
    pub header_cb: Option<Arc<dyn Fn(&AudioHeader) + Send + Sync>>,
    /// C: `ChiakiAudioSinkFrame frame_cb` — leerer Slice bedeutet Concealment
    /// (C: `frame_cb(NULL, 0)` → Packet-Loss-Concealment im Decoder).
    pub frame_cb: Option<Arc<dyn Fn(&[u8]) + Send + Sync>>,
}

/// Haptics-Anteil von C `session->haptics_sink` (ChiakiAudioSink, nur frame_cb genutzt).
#[derive(Clone, Default)]
pub struct HapticsSink {
    pub frame_cb: Option<Arc<dyn Fn(&[u8]) + Send + Sync>>,
}

/// Jitter-Buffer-Slot (C: anonymer Struct in ChiakiAudioReceiver).
///
/// C führt `buf` + `buf_size` getrennt; `Option<Vec<u8>>` deckt beides ab
/// (`None` entspricht `buf == NULL`, `buf_size == 0`).
#[derive(Default)]
struct JitterSlot {
    occupied: bool,
    frame_index: u16,
    buf: Option<Vec<u8>>,
}

/// Mutabler Zustand (C: Felder der Struct, geschützt durch `mutex`).
struct AudioReceiverState {
    frame_index_prev: u16,
    next_frame_index: u16,
    next_frame_index_valid: bool,
    playback_started: bool,
    /// whether frame_index_prev has definitely not wrapped yet (C-Kommentar)
    frame_index_startup: bool,
    jitter_buffer: [JitterSlot; AUDIO_JITTER_BUFFER_SIZE],
    jitter_buffer_count: usize,
}

impl AudioReceiverState {
    fn new() -> Self {
        AudioReceiverState {
            frame_index_prev: 0,
            next_frame_index: 0,
            next_frame_index_valid: false,
            playback_started: false,
            frame_index_startup: true,
            jitter_buffer: std::array::from_fn(|_| JitterSlot::default()),
            jitter_buffer_count: 0,
        }
    }
}

/// Port von `ChiakiAudioReceiver`.
///
/// Sink that receives Audio encoded as Opus (C-Kommentar).
pub struct AudioReceiver {
    mutex: Mutex<AudioReceiverState>,
    sink: AudioSink,
    haptics_sink: HapticsSink,
    packet_stats: Option<Arc<PacketStats>>,
}

fn lock(mutex: &Mutex<AudioReceiverState>) -> MutexGuard<'_, AudioReceiverState> {
    // C loggt hier einen Mutex-Fehler und kehrt zurück; das Rust-Pendant
    // (poisoned Mutex) wird ignoriert und wie im C-Normalfall fortgefahren.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl AudioReceiver {
    /// Port von `chiaki_audio_receiver_init`. Statt `ChiakiSession*` werden die
    /// Sinks (Audio/Haptics) direkt übergeben; `packet_stats` entspricht dem
    /// C-Zeiger (`None` = deaktiviert).
    pub fn new(sink: AudioSink, haptics_sink: HapticsSink, packet_stats: Option<Arc<PacketStats>>) -> Self {
        AudioReceiver {
            mutex: Mutex::new(AudioReceiverState::new()),
            sink,
            haptics_sink,
            packet_stats,
        }
    }

    /// Port von `chiaki_audio_receiver_stream_info`: übergibt den Audio-Header
    /// (channels/bits/rate/frame_size/unknown) und setzt den Jitter-Zustand zurück.
    ///
    /// Wie im C wird der Header-Callback während gehaltener Mutex aufgerufen.
    pub fn stream_info(&self, audio_header: &AudioHeader) {
        let mut st = lock(&self.mutex);

        tracing::info!("Audio Header:");
        tracing::info!("  channels = {}", audio_header.channels);
        tracing::info!("  bits = {}", audio_header.bits);
        tracing::info!("  rate = {}", audio_header.rate);
        tracing::info!("  frame size = {}", audio_header.frame_size);
        tracing::info!("  unknown = {}", audio_header.unknown);

        st.frame_index_prev = 0;
        st.next_frame_index = 0;
        st.next_frame_index_valid = false;
        st.playback_started = false;
        st.frame_index_startup = true;
        clear_jitter_buffer(&mut st);

        if let Some(header_cb) = self.sink.header_cb.clone() {
            header_cb(audio_header);
        }
        // Unlock wie im C (Guard-Drop).
    }

    /// Port von `chiaki_audio_receiver_av_packet`.
    pub fn av_packet(&self, packet: &AVPacket) {
        if packet.codec != 5 {
            tracing::error!("Received Audio Packet with unknown Codec");
            return;
        }

        let source_units_count = packet.audio_source_units_count();
        let fec_units_count = packet.audio_fec_units_count();
        let unit_size = packet.audio_unit_size();

        if packet.data.is_empty() {
            tracing::error!("Audio AV Packet is empty");
            return;
        }

        if fec_units_count as u16 + source_units_count as u16 != packet.units_in_frame_total {
            tracing::error!("Source Units + FEC Units != Total Units in Audio AV Packet");
            return;
        }

        if packet.data.len() != unit_size as usize * packet.units_in_frame_total as usize {
            tracing::error!(
                "Audio AV Packet size mismatch {:#x} vs {:#x}",
                packet.data.len(),
                unit_size as usize * packet.units_in_frame_total as usize
            );
            return;
        }

        // C: `if(packet->frame_index > (1 << 15)) frame_index_startup = false;`
        // Der Wert ändert sich während der Unit-Schleife nicht (nur stream_info
        // setzt ihn zurück) — wir erfassen ihn einmal unter der Mutex.
        let frame_index_startup = {
            let mut st = lock(&self.mutex);
            if packet.frame_index > (1 << 15) {
                st.frame_index_startup = false;
            }
            st.frame_index_startup
        };

        for i in 0..source_units_count as usize + fec_units_count as usize {
            let frame_index = if i < source_units_count as usize {
                packet.frame_index.wrapping_add(i as u16)
            } else {
                let fec_index = i - source_units_count as usize;
                // C: `packet->frame_index + fec_index < fec_units_count + 1`
                // (int/size_t-Arithmetik, kein u16-Wrap im Vergleich).
                if frame_index_startup
                    && packet.frame_index as usize + fec_index < fec_units_count as usize + 1
                {
                    continue;
                }
                packet
                    .frame_index
                    .wrapping_sub(fec_units_count as u16)
                    .wrapping_add(fec_index as u16)
            };

            let unit_size = unit_size as usize;
            self.frame(
                frame_index,
                packet.is_haptics,
                &packet.data[unit_size * i..unit_size * (i + 1)],
            );
        }

        if let Some(packet_stats) = &self.packet_stats {
            packet_stats.push_seq(packet.frame_index);
        }
    }

    /// Port von `chiaki_audio_receiver_frame` (static).
    ///
    /// Die While-True-Schleife des C wird nachgebildet: nach jeder Auslieferung
    /// wird erneut versucht, den nächsten erwarteten Frame zu liefern, bis
    /// nichts mehr lieferbar ist (C: `if(!frame_cb) return`).
    fn frame(&self, frame_index: u16, is_haptics: bool, buf: &[u8]) {
        // Haptics-Zweig des C: sofortige monotone Auslieferung, danach return.
        // (Im C steht der Zweig in der Schleife, kehrt dort aber immer zurück.)
        if is_haptics {
            let deliver: Option<(Arc<dyn Fn(&[u8]) + Send + Sync>, &[u8])> = {
                let mut st = lock(&self.mutex);
                if seq_num_16_gt(frame_index, st.frame_index_prev) {
                    st.frame_index_prev = frame_index;
                    self.haptics_sink.frame_cb.clone().map(|cb| (cb, buf))
                } else {
                    None
                }
            };
            if let Some((cb, buf)) = deliver {
                cb(buf);
            }
            return;
        }

        // C setzt `buf` nach dem Einsortieren auf NULL; hier als Schleifen-Zustand
        // (Copy-tauglich, da nur eine Referenz gehalten wird).
        let mut input: Option<&[u8]> = Some(buf);

        enum Deliver {
            Nothing,
            /// Frame aus dem Jitter-Buffer (C: deliver_owned_buf).
            Owned(Option<Arc<dyn Fn(&[u8]) + Send + Sync>>, Vec<u8>),
            /// Concealment: C ruft frame_cb(NULL, 0) — hier leerer Slice.
            Conceal(Option<Arc<dyn Fn(&[u8]) + Send + Sync>>),
        }

        loop {
            let deliver;
            {
                let mut st = lock(&self.mutex);

                // C: `if(buf) { ... store ... }` mit goto unlock_and_exit bei
                // zu alten / nicht speicherbaren Frames.
                let proceed = match input {
                    Some(buf) => {
                        if st.next_frame_index_valid && seq_num_16_lt(frame_index, st.next_frame_index) {
                            false // C: goto unlock_and_exit
                        } else if store_audio_frame_locked(&mut st, frame_index, buf) {
                            input = None; // C: buf = NULL
                            true
                        } else {
                            false // C: goto unlock_and_exit
                        }
                    }
                    None => true,
                };

                if !proceed {
                    deliver = Deliver::Nothing;
                } else {
                    // Prefill: Playback erst starten, wenn genug gepuffert ist.
                    if !st.playback_started && st.jitter_buffer_count >= AUDIO_JITTER_PREFILL {
                        if let Some(oldest) = find_oldest_audio_slot(&st.jitter_buffer) {
                            st.next_frame_index = st.jitter_buffer[oldest].frame_index;
                            st.next_frame_index_valid = true;
                            st.playback_started = true;
                        }
                    }

                    if st.playback_started && st.next_frame_index_valid {
                        if let Some(slot) = find_audio_slot(&st.jitter_buffer, st.next_frame_index) {
                            // Frame ausliefern: Buffer aus dem Slot nehmen.
                            let out_buf = st.jitter_buffer[slot].buf.take().unwrap_or_default();
                            st.jitter_buffer[slot].occupied = false;
                            st.jitter_buffer_count -= 1;
                            st.frame_index_prev = st.next_frame_index;
                            st.next_frame_index = st.next_frame_index.wrapping_add(1);
                            deliver = Deliver::Owned(self.sink.frame_cb.clone(), out_buf);
                        } else if st.jitter_buffer_count > 0 {
                            let oldest = find_oldest_audio_slot(&st.jitter_buffer);
                            let newest = find_newest_audio_slot(&st.jitter_buffer);
                            let newer_audio_buffered = match oldest {
                                Some(oldest) => {
                                    seq_num_16_gt(st.jitter_buffer[oldest].frame_index, st.next_frame_index)
                                }
                                None => false,
                            };
                            let mut can_conceal_loss = false;
                            if newer_audio_buffered {
                                if let Some(newest) = newest {
                                    if st.jitter_buffer_count >= AUDIO_JITTER_PREFILL {
                                        // C: u16-Arithmetik mit Wrap.
                                        let required_lookahead =
                                            st.next_frame_index.wrapping_add(AUDIO_JITTER_PREFILL as u16);
                                        can_conceal_loss = seq_num_16_gt(
                                            st.jitter_buffer[newest].frame_index,
                                            required_lookahead.wrapping_sub(1),
                                        );
                                    } else {
                                        can_conceal_loss = true;
                                    }
                                }
                            }
                            if can_conceal_loss {
                                // Zustandsfortschritt wie im C unabhängig davon,
                                // ob ein Callback existiert.
                                st.frame_index_prev = st.next_frame_index;
                                st.next_frame_index = st.next_frame_index.wrapping_add(1);
                                deliver = Deliver::Conceal(self.sink.frame_cb.clone());
                            } else {
                                deliver = Deliver::Nothing;
                            }
                        } else {
                            deliver = Deliver::Nothing;
                        }
                    } else {
                        deliver = Deliver::Nothing;
                    }
                }
            }

            // C: Callback außerhalb der Mutex; ohne Callback wird die Schleife
            // verlassen (`if(!frame_cb) return;`).
            match deliver {
                Deliver::Nothing => return,
                Deliver::Owned(Some(cb), out_buf) => cb(&out_buf),
                Deliver::Owned(None, _) => return,
                Deliver::Conceal(Some(cb)) => cb(&[]),
                Deliver::Conceal(None) => return,
            }
        }
    }
}

/// Port von `chiaki_audio_receiver_clear_jitter_buffer` (static).
fn clear_jitter_buffer(st: &mut AudioReceiverState) {
    for slot in st.jitter_buffer.iter_mut() {
        slot.buf = None;
        slot.occupied = false;
    }
    st.jitter_buffer_count = 0;
}

/// Port von `chiaki_audio_receiver_find_audio_slot` (static).
fn find_audio_slot(jitter_buffer: &[JitterSlot; AUDIO_JITTER_BUFFER_SIZE], frame_index: u16) -> Option<usize> {
    for (i, slot) in jitter_buffer.iter().enumerate() {
        if slot.occupied && slot.frame_index == frame_index {
            return Some(i);
        }
    }
    None
}

/// Port von `chiaki_audio_receiver_find_oldest_audio_slot` (static).
fn find_oldest_audio_slot(jitter_buffer: &[JitterSlot; AUDIO_JITTER_BUFFER_SIZE]) -> Option<usize> {
    let mut oldest: Option<usize> = None;
    for (i, slot) in jitter_buffer.iter().enumerate() {
        if !slot.occupied {
            continue;
        }
        let older = match oldest {
            None => true,
            Some(cur) => seq_num_16_lt(slot.frame_index, jitter_buffer[cur].frame_index),
        };
        if older {
            oldest = Some(i);
        }
    }
    oldest
}

/// Port von `chiaki_audio_receiver_find_newest_audio_slot` (static).
fn find_newest_audio_slot(jitter_buffer: &[JitterSlot; AUDIO_JITTER_BUFFER_SIZE]) -> Option<usize> {
    let mut newest: Option<usize> = None;
    for (i, slot) in jitter_buffer.iter().enumerate() {
        if !slot.occupied {
            continue;
        }
        let newer = match newest {
            None => true,
            Some(cur) => seq_num_16_gt(slot.frame_index, jitter_buffer[cur].frame_index),
        };
        if newer {
            newest = Some(i);
        }
    }
    newest
}

/// Port von `chiaki_audio_receiver_store_audio_frame_locked` (static).
fn store_audio_frame_locked(st: &mut AudioReceiverState, frame_index: u16, buf: &[u8]) -> bool {
    if find_audio_slot(&st.jitter_buffer, frame_index).is_some() {
        return false;
    }

    let mut free_slot: Option<usize> = None;
    for (i, slot) in st.jitter_buffer.iter().enumerate() {
        if !slot.occupied {
            free_slot = Some(i);
            break;
        }
    }

    let free_slot = match free_slot {
        Some(slot) => slot,
        None => {
            // Buffer voll: nur überschreiben, wenn der neue Frame jünger als der
            // jüngste gepufferte ist.
            let newest = match find_newest_audio_slot(&st.jitter_buffer) {
                Some(newest) => newest,
                None => return false,
            };
            if !seq_num_16_lt(frame_index, st.jitter_buffer[newest].frame_index) {
                return false;
            }
            st.jitter_buffer[newest].buf = None;
            st.jitter_buffer[newest].occupied = false;
            st.jitter_buffer_count -= 1;
            newest
        }
    };

    st.jitter_buffer[free_slot] = JitterSlot {
        occupied: true,
        frame_index,
        // C: malloc nur für buf_size > 0 (sonst NULL).
        buf: if buf.is_empty() { None } else { Some(buf.to_vec()) },
    };
    st.jitter_buffer_count += 1;
    true
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// AVPacket mit Audio-Unit-Layout bauen.
    ///
    /// Codierung des `units_in_frame_fec`-Feldes (takion.h):
    /// `unit_size << 8 | fec_units << 4 | source_units`.
    fn make_packet(frame_index: u16, is_haptics: bool, unit_size: u8, source: u8, fec: u8, units: Vec<Vec<u8>>) -> AVPacket {
        let mut data = Vec::new();
        for unit in &units {
            data.extend_from_slice(unit);
        }
        AVPacket {
            packet_index: frame_index,
            frame_index,
            uses_nalu_info_structs: false,
            is_video: false,
            is_haptics,
            unit_index: 0,
            units_in_frame_total: units.len() as u16,
            units_in_frame_fec: (unit_size as u16) << 8 | (fec as u16) << 4 | source as u16,
            codec: 5,
            word_at_0x18: 0,
            adaptive_stream_index: 0,
            byte_at_0x2c: 0,
            key_pos: 0,
            data,
        }
    }

    fn unit(tag: u8) -> Vec<u8> {
        vec![tag; 4]
    }

    /// Empfänger + Sammler der ausgelieferten Frames.
    fn make_receiver(stats: Option<Arc<PacketStats>>) -> (AudioReceiver, Arc<StdMutex<Vec<Vec<u8>>>>) {
        let delivered: Arc<StdMutex<Vec<Vec<u8>>>> = Arc::new(StdMutex::new(Vec::new()));
        let sink = {
            let delivered = delivered.clone();
            AudioSink {
                header_cb: None,
                frame_cb: Some(Arc::new(move |buf: &[u8]| {
                    delivered.lock().unwrap().push(buf.to_vec());
                })),
            }
        };
        (AudioReceiver::new(sink, HapticsSink::default(), stats), delivered)
    }

    #[test]
    fn rejects_wrong_codec_and_bad_layouts() {
        let (receiver, delivered) = make_receiver(None);

        // falscher Codec
        let mut packet = make_packet(0, false, 4, 2, 2, vec![unit(0); 4]);
        packet.codec = 6;
        receiver.av_packet(&packet);
        assert!(delivered.lock().unwrap().is_empty());

        // source + fec != total
        let mut packet = make_packet(0, false, 4, 2, 2, vec![unit(0); 4]);
        packet.units_in_frame_total = 3;
        receiver.av_packet(&packet);
        assert!(delivered.lock().unwrap().is_empty());

        // Datenlänge passt nicht zu unit_size * units_total
        let mut packet = make_packet(0, false, 4, 2, 2, vec![unit(0); 4]);
        packet.data.pop();
        receiver.av_packet(&packet);
        assert!(delivered.lock().unwrap().is_empty());

        // leeres Paket
        let packet = make_packet(0, false, 4, 2, 2, vec![]);
        receiver.av_packet(&packet);
        assert!(delivered.lock().unwrap().is_empty());
    }

    #[test]
    fn dropped_source_unit_filled_by_fec_unit_in_order() {
        // Packet frame_index=10: 1 Source-Unit (f10) + 2 FEC-Einheiten
        // (Ersatz für f8, f9). Die verlorenen Frames f8/f9 werden durch die
        // FEC-Einheiten ersetzt, Auslieferung in aufsteigender Reihenfolge.
        let (receiver, delivered) = make_receiver(None);

        receiver.av_packet(&make_packet(10, false, 4, 1, 2, vec![unit(0xa0), unit(0xe0), unit(0xe1)]));

        // Prefill 3 → Playback startet beim ältesten Frame (f8):
        // f8 (FEC), f9 (FEC), f10 (Source).
        let got = delivered.lock().unwrap().clone();
        assert_eq!(got, vec![unit(0xe0), unit(0xe1), unit(0xa0)]);
    }

    #[test]
    fn swapped_packet_order_still_in_order() {
        // Zwei Pakete in vertauschter Reihenfolge → Callbacks trotzdem in-order.
        let (receiver, delivered) = make_receiver(None);

        // Packet B (frame_index=12): Source f12, f13 + FEC-Ersatz f10, f11.
        let packet_b = make_packet(12, false, 4, 2, 2, vec![unit(0xc0), unit(0xc1), unit(0xa0), unit(0xa1)]);
        // Packet A (frame_index=10): Source f10, f11 + FEC-Ersatz f8, f9.
        let packet_a = make_packet(10, false, 4, 2, 2, vec![unit(0xa0), unit(0xa1), unit(0x80), unit(0x81)]);

        receiver.av_packet(&packet_b);
        receiver.av_packet(&packet_a);

        let got = delivered.lock().unwrap().clone();
        // packet_b: nach dem 3. Store (f10=a0, FEC-Ersatz) ist Prefill erreicht
        // und der Drain läuft SOFORT (mitten in der Unit-Schleife, C-Verhalten):
        // f10=a0 wird ausgeliefert; f11 ist noch nicht gespeichert (kommt erst
        // in i=3) und Buffer < Prefill → Concealment (leerer Frame); dann f12,
        // f13. a1 (i=3) ist danach zu alt (11 < 14) und wird verworfen;
        // packet_a ist komplett zu alt.
        assert_eq!(got, vec![unit(0xa0), Vec::new(), unit(0xc0), unit(0xc1)]);
    }

    #[test]
    fn concealment_delivers_empty_frames_on_gap() {
        // Permanente Lücke mit Lookahead: f0 geht verloren (Startup, FEC-Ersatz
        // wird noch übersprungen), späteres Paket erzwingt Concealment.
        let (receiver, delivered) = make_receiver(None);

        // Paket A: frame_index=1, Source f1 + FEC-Ersatz f0 (wird wegen
        // frame_index_startup übersprungen: 1 + 0 < 1 + 1).
        receiver.av_packet(&make_packet(1, false, 4, 1, 1, vec![unit(1), unit(0xdd)]));
        // Belegt: f1.

        // Paket B: frame_index=3, Source f3 + FEC-Ersatz f2 (3 + 0 >= 2 → wird
        // gespeichert). Prefill erreicht → Auslieferung f1, f2, f3.
        receiver.av_packet(&make_packet(3, false, 4, 1, 1, vec![unit(3), unit(2)]));
        {
            let got = delivered.lock().unwrap().clone();
            assert_eq!(got, vec![unit(1), unit(2), unit(3)]);
            delivered.lock().unwrap().clear();
        }

        // Paket C: frame_index=9, Source f9 + FEC-Ersatz f8. next_frame_index=4:
        // f4..f7 fehlen dauerhaft → Concealment (4 leere Frames), dann f8, f9.
        receiver.av_packet(&make_packet(9, false, 4, 1, 1, vec![unit(9), unit(8)]));

        let got = delivered.lock().unwrap().clone();
        // i=0 speichert f9 und der Drain läuft sofort: f4..f7 werden verdeckt
        // (Buffer < Prefill → can_conceal_loss immer true) — und auch f8, das
        // erst in i=1 gespeichert WÜRDE; als i=1 f8 endlich speichert, ist
        // next schon 10 → f8 ist zu alt. Zum Schluss wird f9 ausgeliefert.
        assert_eq!(got, vec![Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), unit(9)]);
    }

    #[test]
    fn startup_skips_fec_units() {
        // frame_index_startup: bei frame_index=0 würden die FEC-Ersatz-Indizes
        // (0 - 2 + j = 65534/65535) vor dem ersten Wrap liegen → übersprungen.
        let (receiver, delivered) = make_receiver(None);

        let packet = make_packet(0, false, 4, 2, 2, vec![unit(0), unit(1), unit(0xe0), unit(0xe1)]);
        receiver.av_packet(&packet);

        // Nur f0 + f1 gespeichert (2 < Prefill 3) → keine Auslieferung.
        assert!(delivered.lock().unwrap().is_empty());

        // Duplikat-Packet: f0/f1 doppelt → verworfen, FEC-Ersatz weiter übersprungen.
        let packet = make_packet(0, false, 4, 2, 2, vec![unit(0), unit(1), unit(0xe0), unit(0xe1)]);
        receiver.av_packet(&packet);
        assert!(delivered.lock().unwrap().is_empty());

        // frame_index > 1 << 15 beendet frame_index_startup → FEC-Ersatz
        // f39998/f39999 wird einsortiert. Bereits nach dem 1. Store (f40000)
        // ist der Prefill erreicht: ältester Eintrag in Serial-16 ist f40000
        // (f0 liegt mehr als einen halben Ring "dahinter") → Playback startet
        // bei f40000. f40001 ist zu dem Zeitpunkt noch nicht gespeichert und
        // der Buffer hat < Prefill → Concealment-Kette bis "0" über den Wrap
        // erreicht ist (25535 leere Frames), dann f0/f1 aus dem ersten Packet.
        let packet = make_packet(40000, false, 4, 2, 2, vec![unit(0x40), unit(0x41), unit(0x70), unit(0x71)]);
        receiver.av_packet(&packet);
        let got = delivered.lock().unwrap().clone();
        assert_eq!(got.first(), Some(&unit(0x40)));
        // f39998/f39999 (0x70/0x71) sind in Serial-16 "jünger" als alles andere
        // hier und werden nie erreicht → nicht ausgeliefert.
        assert_eq!(got.iter().filter(|v| !v.is_empty()).count(), 3);
        assert_eq!(&got[got.len() - 2..], &[unit(0), unit(1)]);
        assert_eq!(got.len(), 1 + (65536 - 40001) + 2); // 0x40 + Conceal f40001..f65535 + f0/f1
    }

    #[test]
    fn haptics_frames_monotonic() {
        let delivered: Arc<StdMutex<Vec<Vec<u8>>>> = Arc::new(StdMutex::new(Vec::new()));
        let haptics_sink = {
            let delivered = delivered.clone();
            HapticsSink {
                frame_cb: Some(Arc::new(move |buf: &[u8]| {
                    delivered.lock().unwrap().push(buf.to_vec());
                })),
            }
        };
        let receiver = AudioReceiver::new(AudioSink::default(), haptics_sink, None);

        // Jede Unit ist ein eigener Haptics-Frame: f7, f8, f9, f10.
        receiver.av_packet(&make_packet(7, true, 4, 4, 0, vec![unit(1), unit(2), unit(3), unit(4)]));

        // frame_index 5..8: alles <= prev (10) → verworfen.
        receiver.av_packet(&make_packet(5, true, 4, 4, 0, vec![unit(9); 4]));

        // f10 == prev → verworfen; f11, f12, f13 → ausgeliefert.
        receiver.av_packet(&make_packet(10, true, 4, 4, 0, vec![unit(8); 4]));

        let got = delivered.lock().unwrap().clone();
        assert_eq!(got, vec![unit(1), unit(2), unit(3), unit(4), unit(8), unit(8), unit(8)]);
    }

    #[test]
    fn duplicate_units_are_dropped() {
        let (receiver, delivered) = make_receiver(None);

        let packet = make_packet(10, false, 4, 1, 2, vec![unit(0xa0), unit(0xe0), unit(0xe1)]);
        receiver.av_packet(&packet);
        let first = delivered.lock().unwrap().clone();
        assert_eq!(first.len(), 3);

        // Gleiches Packet nochmal: alle Slots bereits geliefert/leer → wird
        // wieder einsortiert, aber next_frame_index (11) ist schon vorbei →
        // keine zweite Auslieferung derselben Frames.
        receiver.av_packet(&packet);
        let got = delivered.lock().unwrap().clone();
        assert_eq!(got, first);
    }

    #[test]
    fn stream_info_resets_state_and_calls_header_cb() {
        let header_seen: Arc<StdMutex<Option<(u8, u8, u32, u32)>>> = Arc::new(StdMutex::new(None));
        let delivered: Arc<StdMutex<Vec<Vec<u8>>>> = Arc::new(StdMutex::new(Vec::new()));

        let sink = {
            let delivered = delivered.clone();
            let header_seen = header_seen.clone();
            AudioSink {
                header_cb: Some(Arc::new(move |header: &AudioHeader| {
                    *header_seen.lock().unwrap() = Some((header.channels, header.bits, header.rate, header.frame_size));
                })),
                frame_cb: Some(Arc::new(move |buf: &[u8]| {
                    delivered.lock().unwrap().push(buf.to_vec());
                })),
            }
        };
        let receiver = AudioReceiver::new(sink, HapticsSink::default(), None);

        // Playback in Gang setzen.
        receiver.av_packet(&make_packet(10, false, 4, 1, 2, vec![unit(0xa0), unit(0xe0), unit(0xe1)]));
        assert_eq!(delivered.lock().unwrap().len(), 3);

        // stream_info: Header-Callback + kompletter Zustands-Reset.
        // (Der Test-Sammler wird geleert — er ist vom Empfänger-State unabhängig.)
        delivered.lock().unwrap().clear();
        let header = AudioHeader {
            channels: 2,
            bits: 16,
            rate: 48000,
            frame_size: 480,
            unknown: 0,
        };
        receiver.stream_info(&header);
        assert_eq!(*header_seen.lock().unwrap(), Some((2, 16, 48000, 480)));
        assert!(delivered.lock().unwrap().is_empty());

        // Nach dem Reset läuft alles wie am Anfang (Prefill erneut nötig):
        receiver.av_packet(&make_packet(10, false, 4, 1, 2, vec![unit(0xa0), unit(0xe0), unit(0xe1)]));
        let got = delivered.lock().unwrap().clone();
        assert_eq!(got, vec![unit(0xe0), unit(0xe1), unit(0xa0)]);
    }

    #[test]
    fn packet_stats_push_seq() {
        let stats = Arc::new(PacketStats::new());
        let (receiver, _delivered) = make_receiver(Some(stats.clone()));

        receiver.av_packet(&make_packet(100, false, 4, 1, 2, vec![unit(0); 3]));
        receiver.av_packet(&make_packet(102, false, 4, 1, 2, vec![unit(0); 3]));

        let (received, _lost) = stats.get(false);
        assert_eq!(received, 2); // push_seq einmal pro av_packet
    }
}
