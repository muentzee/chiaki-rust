// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/audiosender.c + lib/include/chiaki/audiosender.h (chiaki-ng).
//
// Mikrofon-/Opus-Senderseite: sammelt die letzten Opus-Frames und baut daraus
// das rohe Mic-AV-Packet (Header von Hand formatiert, 3 Units à 40 Bytes),
// das über Takion::send_mic_packet rausgeht.
//
// ACHTUNG, C-Original-Verhalten (1:1 übernommen, auch die Kuriositäten):
// - Nur Frames mit exakt buf_size_per_unit (40) Bytes werden gesendet ("skip
//   audio packets without encoded audio").
// - Die ersten beiden Frames werden nur gepuffert (frameb/framea); ab dem
//   dritten wird gesendet.
// - Beim Zusammenbau wird Unit 0 des Packets NACH dem Kopieren von frameb/
//   framea/opus nochmals mit dem aktuellen Opus-Frame überschrieben, und
//   frameb erhält danach denselben Inhalt wie framea (aktuelles Frame). Das
//   entspricht exakt der memcpy-Sequenz im C-Code.
//
// Abweichungen gegenüber C: Staging (framea/frameb/frame_buf) liegt mit unter
// der Mutex (im C ungeschützt, aber nur aus dem einen Audio-Callback-Thread
// benutzt); die Paket-Formatierung ist als format_mic_packet() getrennt
// testbar; `fini`/`free` entfällt (Drop).

use std::ops::Range;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::takion::Takion;

/// C: `uint8_t packet_type = 3; // TAKION_PACKET_TYPE_AUDIO`
const TAKION_PACKET_TYPE_AUDIO: u8 = 3;
/// C: `uint8_t codec = 5;`
const AUDIO_CODEC: u8 = 5;
/// C: `uint32_t units_in_frame_total = 3;`
const UNITS_IN_FRAME_TOTAL: u16 = 3;
/// C: `uint32_t units_in_frame_fec_raw = 10273;`
const UNITS_IN_FRAME_FEC_RAW: u32 = 10273;

/// Port von `ChiakiAudioSender`.
pub struct AudioSender {
    ps5: bool,
    takion: Arc<Takion>,
    inner: Mutex<AudioSenderInner>,
}

/// Mutabler Zustand (C: Felder von ChiakiAudioSender).
struct AudioSenderInner {
    buf_size_per_unit: u16,
    /// Im C berechnet (`((buf_size_per_unit + 0xf) / 0x10) * 0x10`), aber
    /// nirgendwo benutzt — 1:1 gehalten.
    #[allow(dead_code)]
    buf_stride_per_unit: u16,
    frame_index: u16,
    framea: Option<Vec<u8>>,
    frameb: Option<Vec<u8>>,
    /// 3 * buf_size_per_unit Bytes (C: frame_buf / frame_buf_size).
    frame_buf: Vec<u8>,
    /// frame_buf_size + 20 Bytes (C: filled_packet_buf).
    filled_packet_buf: Vec<u8>,
}

impl AudioSenderInner {
    /// Port der Feldinitialisierung aus `chiaki_audio_sender_init`.
    fn new() -> Self {
        let buf_size_per_unit: u16 = 40;
        let buf_stride_per_unit: u16 = buf_size_per_unit.div_ceil(0x10) * 0x10;
        let frame_buf_size = 3 * buf_size_per_unit as usize;
        AudioSenderInner {
            buf_size_per_unit,
            buf_stride_per_unit,
            frame_index: 0,
            framea: None,
            frameb: None,
            frame_buf: vec![0; frame_buf_size],
            filled_packet_buf: vec![0; frame_buf_size + 20],
        }
    }

    /// Port von `chiaki_audio_sender_opus_data` (ohne Senden).
    ///
    /// Baut das Paket in `filled_packet_buf` und liefert den zu sendenden
    /// Byte-Range; `None` = übersprungen (falsche Größe) oder nur Staging
    /// (erste beiden Frames).
    fn opus_data(&mut self, opus_data: &[u8], ps5: bool) -> Option<Range<usize>> {
        // skip audio packets without encoded audio
        // if no audio the packet will have only 3 encoded units because there is
        // no entropy in the packet, otherwise should be max of 40 (C-Kommentar)
        if opus_data.len() != self.buf_size_per_unit as usize {
            return None;
        }

        // Staging der ersten beiden Frames (C: frameb, dann framea).
        if self.frameb.is_none() {
            self.frameb = Some(opus_data.to_vec());
            return None;
        }
        if self.framea.is_none() {
            self.framea = Some(opus_data.to_vec());
            return None;
        }

        let bps = self.buf_size_per_unit as usize;
        {
            let (Some(frameb), Some(framea)) = (&self.frameb, &self.framea) else {
                return None; // durch das Staging oben unerreichbar
            };
            self.frame_buf[0..bps].copy_from_slice(&frameb[..bps]);
            self.frame_buf[bps..2 * bps].copy_from_slice(&framea[..bps]);
            self.frame_buf[2 * bps..3 * bps].copy_from_slice(&opus_data[..bps]);
            // C: Unit 0 wird nochmals mit dem aktuellen Frame überschrieben
            // (exakte memcpy-Sequenz des Originals).
            self.frame_buf[0..bps].copy_from_slice(&opus_data[..bps]);
        }
        // C: framea = opus; frameb = framea (= aktuelles Frame).
        self.framea = Some(opus_data.to_vec());
        self.frameb = self.framea.clone();

        let filled_packet_size =
            format_mic_packet(&mut self.filled_packet_buf, self.frame_index, &self.frame_buf, ps5);
        Some(0..filled_packet_size)
    }
}

/// Port der Header-/Paketformatierung aus `chiaki_audio_sender_opus_data`.
///
/// Layout (C-Offsets, `htons`/`htonl` = Big-Endian):
/// ```text
/// [0]      packet_type = 3 (TAKION_PACKET_TYPE_AUDIO)
/// [1..3]   packet_index  = frame_index
/// [3..5]   frame_index   = frame_index + 1
/// [5..9]   units_number  = htonl((fec_raw & 0xffff) | ((total-1 & 0xff) << 16) | (unit_index << 24))
/// [9]      codec = 5
/// [10..14] gmac   = 0
/// [14..18] key_pos = 0
/// [18]     zero_byte
/// [19]     zero_byte (nur PS5)
/// [19+ps5] frame_buf (3 * 40 Bytes Units)
/// ```
/// Liefert `filled_packet_size` (= frame_buf_size + 19 + ps5_packet).
pub fn format_mic_packet(out: &mut [u8], frame_index: u16, frame_buf: &[u8], ps5: bool) -> usize {
    let ps5_packet: usize = if ps5 { 1 } else { 0 };
    let filled_packet_size = frame_buf.len() + 19 + ps5_packet;
    debug_assert!(out.len() >= filled_packet_size);

    out[0] = TAKION_PACKET_TYPE_AUDIO;
    out[1..3].copy_from_slice(&frame_index.to_be_bytes());
    out[3..5].copy_from_slice(&frame_index.wrapping_add(1).to_be_bytes());
    // C: (fec_raw & 0xffff) | ((total-1 & 0xff) << 0x10) | ((unit_index & 0xff) << 0x18)
    // mit unit_index = 0 (lettischer Term konstant 0).
    let units_number: u32 = (UNITS_IN_FRAME_FEC_RAW & 0xffff)
        | (((UNITS_IN_FRAME_TOTAL - 1) as u32 & 0xff) << 0x10);
    out[5..9].copy_from_slice(&units_number.to_be_bytes());
    out[9] = AUDIO_CODEC;
    out[10..14].copy_from_slice(&0u32.to_be_bytes()); // gmac
    out[14..18].copy_from_slice(&0u32.to_be_bytes()); // key_pos
    out[18] = 0; // zero_byte
    if ps5 {
        out[19] = 0; // zero_byte (PS5)
    }
    out[19 + ps5_packet..19 + ps5_packet + frame_buf.len()].copy_from_slice(frame_buf);

    filled_packet_size
}

fn lock(mutex: &Mutex<AudioSenderInner>) -> MutexGuard<'_, AudioSenderInner> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl AudioSender {
    /// Port von `chiaki_audio_sender_init`. Im C werden `ps5` aus
    /// `session->connect_info` und `takion` aus dem StreamConnection gezogen;
    /// in Rust werden beide direkt übergeben.
    pub fn new(ps5: bool, takion: Arc<Takion>) -> Self {
        AudioSender {
            ps5,
            takion,
            inner: Mutex::new(AudioSenderInner::new()),
        }
    }

    /// Port von `chiaki_audio_sender_opus_data`: nimmt ein Opus-Frame entgegen
    /// (genau `buf_size_per_unit` = 40 Bytes) und sendet es als Mic-Packet.
    pub fn opus_data(&self, opus_data: &[u8]) {
        let mut inner = lock(&self.inner);
        let Some(range) = inner.opus_data(opus_data, self.ps5) else {
            return;
        };

        // Port von chiaki_audio_sender_frame: Senden unter der Mutex, danach
        // frame_index inkrementieren (Wrap bei UINT16_MAX).
        if let Err(err) = self.takion.send_mic_packet(&inner.filled_packet_buf[range], self.ps5) {
            tracing::error!("Failed to send mic audio packet: {}", err);
        }
        inner.frame_index = if inner.frame_index == u16::MAX {
            0
        } else {
            inner.frame_index + 1
        };
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(tag: u8) -> Vec<u8> {
        (0..40).map(|i| tag.wrapping_add(i as u8)).collect()
    }

    #[test]
    fn golden_packet_layout_ps4() {
        let mut inner = AudioSenderInner::new();

        // Erste beiden Frames: nur Staging.
        assert!(inner.opus_data(&frame(0x10), false).is_none());
        assert!(inner.opus_data(&frame(0x20), false).is_none());

        // Drittes Frame: erstes Paket — frame_index ist noch 0 (C: der
        // Inkrement passiert erst nach dem Senden in chiaki_audio_sender_frame,
        // den nur AudioSender::opus_data nachbildet).
        let range = inner.opus_data(&frame(0x30), false).expect("packet expected");
        assert_eq!(range, 0..139); // frame_buf_size(120) + 19

        let p = &inner.filled_packet_buf[..range.len()];
        assert_eq!(p[0], 3); // TAKION_PACKET_TYPE_AUDIO
        assert_eq!(&p[1..3], &0u16.to_be_bytes()); // packet_index
        assert_eq!(&p[3..5], &1u16.to_be_bytes()); // frame_index + 1
        assert_eq!(&p[5..9], &[0x00, 0x02, 0x28, 0x21]); // units_number (htonl(0x00022821))
        assert_eq!(p[9], 5); // codec
        assert_eq!(&p[10..14], &[0; 4]); // gmac
        assert_eq!(&p[14..18], &[0; 4]); // key_pos
        assert_eq!(p[18], 0); // zero_byte

        // Payload: C-Quirk — Unit 0 ist das AKTUELLE Frame (Überschreiben),
        // Unit 1 = framea (vorheriges Frame), Unit 2 = aktuelles Frame.
        assert_eq!(&p[19..59], &frame(0x30)[..]);
        assert_eq!(&p[59..99], &frame(0x20)[..]);
        assert_eq!(&p[99..139], &frame(0x30)[..]);
    }

    #[test]
    fn golden_packet_layout_ps5() {
        let mut inner = AudioSenderInner::new();
        inner.opus_data(&frame(0x10), true);
        inner.opus_data(&frame(0x20), true);

        let range = inner.opus_data(&frame(0x30), true).expect("packet expected");
        assert_eq!(range, 0..140); // +19 Header +1 ps5_packet

        let p = &inner.filled_packet_buf[..range.len()];
        assert_eq!(p[18], 0);
        assert_eq!(p[19], 0); // zero_byte (PS5)
        assert_eq!(&p[20..60], &frame(0x30)[..]); // Payload beginnt bei 19+1
    }

    #[test]
    fn staging_sequence_and_quirk() {
        let mut inner = AudioSenderInner::new();

        // Staging: 0x10 → frameb, 0x20 → framea; 0x30 erzeugt das erste Paket
        // und setzt framea=0x30, frameb=framea (= 0x30).
        assert!(inner.opus_data(&frame(0x10), false).is_none());
        assert!(inner.opus_data(&frame(0x20), false).is_none());
        assert!(inner.opus_data(&frame(0x30), false).is_some());

        // Vierter Aufruf: frameb wurde im C mit framea überschrieben
        // (= aktuelles Frame des dritten Aufrufs).
        let range = inner.opus_data(&frame(0x40), false).expect("packet expected");
        let p = &inner.filled_packet_buf[..range.len()];
        assert_eq!(&p[19..59], &frame(0x40)[..]); // Unit 0: aktuelles Frame
        assert_eq!(&p[59..99], &frame(0x30)[..]); // Unit 1: framea (0x30)
        assert_eq!(&p[99..139], &frame(0x40)[..]); // Unit 2: aktuelles Frame

        // Der frame_index-Inkrement passiert erst in AudioSender::opus_data
        // (nach dem Senden) — direkt auf Inner bleibt er 0.
        assert_eq!(inner.frame_index, 0);
    }

    #[test]
    fn wrong_size_is_skipped() {
        let mut inner = AudioSenderInner::new();

        assert!(inner.opus_data(&[0u8; 39], false).is_none());
        assert!(inner.opus_data(&[0u8; 41], false).is_none());
        assert!(inner.opus_data(&[], false).is_none());

        // Kein Staging passiert: auch nach 40-Byte-Frames wird erst beim
        // dritten gesendet.
        assert!(inner.opus_data(&frame(1), false).is_none());
        assert!(inner.opus_data(&frame(2), false).is_none());
        assert!(inner.opus_data(&frame(3), false).is_some());
    }

    #[test]
    fn frame_index_wraps_at_uint16_max() {
        let mut inner = AudioSenderInner::new();
        inner.opus_data(&frame(1), false);
        inner.opus_data(&frame(2), false);
        inner.frame_index = u16::MAX;

        let range = inner.opus_data(&frame(3), false).expect("packet expected");
        let p = &inner.filled_packet_buf[..range.len()];
        assert_eq!(&p[1..3], &u16::MAX.to_be_bytes()); // packet_index = 65535
        assert_eq!(&p[3..5], &0u16.to_be_bytes()); // frame_index + 1 (Wrap)

        // Der Inkrement passiert in AudioSender::opus_data (nach dem Senden);
        // hier der C-Logik nachempfunden:
        inner.frame_index = if inner.frame_index == u16::MAX { 0 } else { inner.frame_index + 1 };
        assert_eq!(inner.frame_index, 0);
    }

    #[test]
    fn format_mic_packet_matches_c_offsets() {
        // frame_buf im C sind frame_buf_size = 3 * buf_size_per_unit = 120 Bytes
        let frame_buf: Vec<u8> = [frame(0x50), frame(0x51), frame(0x52)].concat();
        assert_eq!(frame_buf.len(), 120);

        let mut out = vec![0xaau8; 141];
        let n = format_mic_packet(&mut out, 0x1234, &frame_buf, true);
        assert_eq!(n, 140); // 120 + 19 + 1 (ps5)
        assert_eq!(out[0], 3);
        assert_eq!(&out[1..3], &0x1234u16.to_be_bytes());
        assert_eq!(&out[3..5], &0x1235u16.to_be_bytes());
        assert_eq!(&out[5..9], &[0x00, 0x02, 0x28, 0x21]); // htonl(0x00022821)
        assert_eq!(out[9], 5);
        // Restliche Header-Bytes genullt:
        assert!(out[10..20].iter().all(|&b| b == 0));
        // Payload bei 19+1 (PS5), dahinter bleibt die 0xaa-Füllung:
        assert_eq!(&out[20..140], &frame_buf[..]);
        assert_eq!(out[140], 0xaa);

        let mut out2 = vec![0xaau8; 140];
        let n2 = format_mic_packet(&mut out2, 0, &frame_buf, false);
        assert_eq!(n2, 139); // 120 + 19
        assert_eq!(out2[19], frame_buf[0]); // Payload direkt bei 19 (PS4)
        assert_eq!(&out2[19..139], &frame_buf[..]);
        assert_eq!(out2[139], 0xaa);
    }
}
