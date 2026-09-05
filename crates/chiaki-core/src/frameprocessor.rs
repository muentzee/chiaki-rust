// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/frameprocessor.c + lib/include/chiaki/frameprocessor.h (chiaki-ng).
//
// Sammelt Frame-Units aus AV-Packets, füllt einen Stride-basierten Frame-Buffer
// und rekonstruiert fehlende Units per FEC (crate::fec::decode — identische
// Parameter wie chiaki_fec_decode im C).

use super::error::{ChiakiError, ChiakiResult};
use super::fec;
use super::packetstats::PacketStats;
use super::takion::AVPacket;
use super::video::VIDEO_BUFFER_PADDING_SIZE;

/// `UNIT_SLOTS_MAX` (frameprocessor.c).
const UNIT_SLOTS_MAX: usize = 512;

/// Port von `ChiakiStreamStats`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StreamStats {
    pub frames: u64,
    pub bytes: u64,
}

impl StreamStats {
    /// Port von `chiaki_stream_stats_reset()`.
    pub fn reset(&mut self) {
        self.frames = 0;
        self.bytes = 0;
    }

    /// Port von `chiaki_stream_stats_frame()`.
    pub fn frame(&mut self, size: u64) {
        self.frames += 1;
        self.bytes += size;
    }

    /// Port von `chiaki_stream_stats_bitrate()`.
    pub fn bitrate(&self, framerate: u64) -> u64 {
        if self.frames == 0 {
            return 0;
        }
        (self.bytes * 8 * framerate) / self.frames
    }
}

/// Port von `ChiakiFrameUnit` (hält im C nur `data_size`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameUnit {
    pub data_size: usize,
}

/// Port von `ChiakiFrameProcessorFlushResult`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum FlushResult {
    Success = 0,
    FecSuccess = 1,
    FecFailed = 2,
    Failed = 3,
}

/// Port von `ChiakiFrameProcessor`.
pub struct FrameProcessor {
    frame_buf: Vec<u8>,
    frame_buf_size: usize,
    buf_size_per_unit: usize,
    buf_stride_per_unit: usize,
    units_source_expected: u32,
    units_fec_expected: u32,
    units_source_received: u32,
    units_fec_received: u32,
    unit_slots: Vec<FrameUnit>,
    /// C: `unit_slots_size` (allokierte Länge von `unit_slots`).
    unit_slots_size: usize,
    /// whether we have already flushed the current frame, i.e. are only
    /// interested in stats, not data.
    flushed: bool,
    stream_stats: StreamStats,
}

impl Default for FrameProcessor {
    fn default() -> Self {
        FrameProcessor::new()
    }
}

impl FrameProcessor {
    /// Port von `chiaki_frame_processor_init()`.
    pub fn new() -> Self {
        FrameProcessor {
            frame_buf: Vec::new(),
            frame_buf_size: 0,
            buf_size_per_unit: 0,
            buf_stride_per_unit: 0,
            units_source_expected: 0,
            units_fec_expected: 0,
            units_source_received: 0,
            units_fec_received: 0,
            unit_slots: Vec::new(),
            unit_slots_size: 0,
            flushed: true,
            stream_stats: StreamStats::default(),
        }
    }

    /// Port von `chiaki_stream_stats_*`-Zugriff (FrameProcessor.stream_stats).
    pub fn stream_stats(&self) -> &StreamStats {
        &self.stream_stats
    }

    /// Port von `chiaki_frame_processor_alloc_frame()`.
    pub fn alloc_frame(&mut self, packet: &AVPacket) -> ChiakiResult<()> {
        if packet.units_in_frame_total < packet.units_in_frame_fec {
            tracing::error!("Packet has units_in_frame_total < units_in_frame_fec");
            return Err(ChiakiError::InvalidData);
        }

        self.flushed = false;
        self.units_source_expected = (packet.units_in_frame_total - packet.units_in_frame_fec) as u32;
        self.units_fec_expected = packet.units_in_frame_fec as u32;
        if self.units_fec_expected < 1 {
            self.units_fec_expected = 1;
        }

        self.buf_size_per_unit = packet.data.len();
        if packet.is_video && (packet.unit_index as u32) < self.units_source_expected {
            if packet.data.len() < 2 {
                tracing::error!("Packet too small to read buf size extension");
                return Err(ChiakiError::BufTooSmall);
            }
            // ntohs(((uint16_t *)packet->data)[0]) — big-endian size extension
            let ext = u16::from_be_bytes([packet.data[0], packet.data[1]]);
            self.buf_size_per_unit += ext as usize;
        }
        self.buf_stride_per_unit = self.buf_size_per_unit.div_ceil(0x10) * 0x10;

        if self.buf_size_per_unit == 0 {
            tracing::error!("Frame Processor doesn't handle empty units");
            return Err(ChiakiError::BufTooSmall);
        }

        self.units_source_received = 0;
        self.units_fec_received = 0;

        let unit_slots_size_required = self.units_source_expected as usize + self.units_fec_expected as usize;
        if unit_slots_size_required > UNIT_SLOTS_MAX {
            tracing::error!("Packet suggests more than {} unit slots", UNIT_SLOTS_MAX);
            return Err(ChiakiError::InvalidData);
        }
        if unit_slots_size_required != self.unit_slots_size {
            // C: realloc/free-Logik — in Rust übernimmt Vec::resize beides.
            self.unit_slots.resize(unit_slots_size_required, FrameUnit::default());
            self.unit_slots_size = unit_slots_size_required;
        }
        for slot in &mut self.unit_slots {
            *slot = FrameUnit::default();
        }

        // C: SIZE_MAX-Overflow-Check entfällt (Vec prüft selbst); die
        // Allokation umfasst das FFMPEG-Padding wie im C.
        let frame_buf_size_required = self.unit_slots_size * self.buf_stride_per_unit;
        if self.frame_buf_size < frame_buf_size_required {
            self.frame_buf = vec![0u8; frame_buf_size_required + VIDEO_BUFFER_PADDING_SIZE];
            self.frame_buf_size = frame_buf_size_required;
        }
        let zero_len = (frame_buf_size_required + VIDEO_BUFFER_PADDING_SIZE).min(self.frame_buf.len());
        self.frame_buf[..zero_len].fill(0);

        Ok(())
    }

    /// Port von `chiaki_frame_processor_put_unit()`.
    pub fn put_unit(&mut self, packet: &AVPacket) -> ChiakiResult<()> {
        if packet.unit_index >= packet.units_in_frame_total {
            tracing::error!("Packet's unit index is outside frame unit count");
            return Err(ChiakiError::InvalidData);
        }

        if packet.unit_index as usize >= self.unit_slots_size {
            tracing::error!("Packet's unit index is too high");
            return Err(ChiakiError::InvalidData);
        }

        if packet.data.is_empty() {
            tracing::warn!("Unit is empty");
            return Err(ChiakiError::InvalidData);
        }

        if packet.data.len() > self.buf_size_per_unit {
            tracing::warn!("Unit is bigger than pre-calculated size!");
            return Err(ChiakiError::InvalidData);
        }

        let unit = &mut self.unit_slots[packet.unit_index as usize];
        if unit.data_size != 0 {
            tracing::warn!("Received duplicate unit");
            return Err(ChiakiError::InvalidData);
        }

        unit.data_size = packet.data.len();
        if !self.flushed {
            let offset = packet.unit_index as usize * self.buf_stride_per_unit;
            self.frame_buf[offset..offset + packet.data.len()].copy_from_slice(&packet.data);
        }

        if (packet.unit_index as u32) < self.units_source_expected {
            self.units_source_received += 1;
        } else {
            self.units_fec_received += 1;
        }

        Ok(())
    }

    /// Port von `chiaki_frame_processor_report_packet_stats()`.
    pub fn report_packet_stats(&self, packet_stats: &PacketStats) {
        let received = self.units_source_received as u64 + self.units_fec_received as u64;
        let expected = self.units_source_expected as u64 + self.units_fec_expected as u64;
        packet_stats.push_generation(received, expected - received);
    }

    /// Flush möglich, sobald alle Source-Units angekommen sind
    /// (`chiaki_frame_processor_flush_possible`).
    pub fn flush_possible(&self) -> bool {
        self.units_source_received + self.units_fec_received >= self.units_source_expected
    }

    /// C: `chiaki_frame_processor_fec()` (privat).
    fn fec(&mut self) -> ChiakiResult<()> {
        tracing::info!(
            "Frame Processor received {}+{} / {}+{} units, attempting FEC",
            self.units_source_received,
            self.units_fec_received,
            self.units_source_expected,
            self.units_fec_expected
        );

        let total = self.units_source_expected as usize + self.units_fec_expected as usize;
        let erasures_count =
            total - (self.units_source_received as usize + self.units_fec_received as usize);
        let mut erasures = Vec::with_capacity(erasures_count);

        for (i, slot) in self.unit_slots[..total].iter().enumerate() {
            if slot.data_size == 0 {
                if erasures.len() >= erasures_count {
                    // should never happen by design, but too scary not to check
                    return Err(ChiakiError::Unknown);
                }
                erasures.push(i as u32);
            }
        }

        match fec::decode(
            &mut self.frame_buf,
            self.buf_size_per_unit,
            self.buf_stride_per_unit,
            self.units_source_expected,
            self.units_fec_expected,
            &erasures,
        ) {
            Ok(()) => {
                tracing::info!("FEC successful");

                // restore unit sizes
                for i in 0..self.units_source_expected as usize {
                    let buf_off = self.buf_stride_per_unit * i;
                    let padding = u16::from_be_bytes([self.frame_buf[buf_off], self.frame_buf[buf_off + 1]]);
                    if padding as usize >= self.buf_size_per_unit {
                        tracing::error!(
                            "Padding in unit ({:#x}) is larger or equals to the whole unit size ({:#x})",
                            padding,
                            self.buf_size_per_unit
                        );
                        continue;
                    }
                    self.unit_slots[i].data_size = self.buf_size_per_unit - padding as usize;
                }
                Ok(())
            }
            Err(e) => {
                let _ = e;
                tracing::error!("FEC failed");
                Err(ChiakiError::FecFailed)
            }
        }
    }

    /// Port von `chiaki_frame_processor_flush()`.
    ///
    /// Liefert `(FlushResult, Frame-Daten)`; die Daten liegen im internen
    /// Buffer und sind nur bis zum nächsten `alloc_frame`/`put_unit` gültig
    /// (wie im C dokumentiert). Bei `FlushResult::Failed` ein leerer Slice.
    pub fn flush(&mut self) -> (FlushResult, &[u8]) {
        if self.units_source_expected == 0 || self.flushed {
            return (FlushResult::Failed, &[]);
        }

        let mut result = FlushResult::Success;
        if self.units_source_received < self.units_source_expected {
            result = if self.fec().is_ok() {
                FlushResult::FecSuccess
            } else {
                FlushResult::FecFailed
            };
        }

        let mut cur = 0usize;
        for i in 0..self.units_source_expected as usize {
            let data_size = self.unit_slots[i].data_size;
            if data_size == 0 {
                tracing::warn!("Missing unit {:#x}", i);
                continue;
            }
            if data_size < 2 {
                tracing::error!("Saved unit has size < 2");
                continue;
            }
            let part_size = data_size - 2;
            let buf_off = i * self.buf_stride_per_unit;
            // memmove(frame_buf + cur, buf_ptr + 2, part_size)
            self.frame_buf.copy_within(buf_off + 2..buf_off + 2 + part_size, cur);
            cur += part_size;
        }

        self.stream_stats.frame(cur as u64);

        (result, &self.frame_buf[..cur])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Codec;

    /// AVPacket für Tests bauen (Felder laut CONTRACT-TAKION.md).
    fn av_packet(unit_index: u16, total: u16, fec: u16, is_video: bool, data: Vec<u8>) -> AVPacket {
        AVPacket {
            packet_index: 0,
            frame_index: 0,
            uses_nalu_info_structs: false,
            is_video,
            is_haptics: false,
            unit_index,
            units_in_frame_total: total,
            units_in_frame_fec: fec,
            codec: Codec::H264 as u8,
            word_at_0x18: 0,
            adaptive_stream_index: 0,
            byte_at_0x2c: 0,
            key_pos: 0,
            data,
        }
    }

    /// Stride == unit_size (16), 3 Source-Units + 1 FEC-Unit.
    /// Jede Unit: [ext_hi, ext_lo, payload...] mit ext = 16 - data_size.
    /// Alle Source-Units sind bereits `put`.
    fn setup_frame() -> (FrameProcessor, Vec<Vec<u8>>) {
        let payloads: [&[u8]; 3] = [b"AAAAAAAA", b"BBBBBBBBBB", b"CCCCCC"];
        let units: Vec<Vec<u8>> = payloads
            .iter()
            .map(|p| {
                let ds = 2 + p.len() as u16;
                let mut v = vec![((16 - ds) >> 8) as u8, (16 - ds) as u8];
                v.extend_from_slice(p);
                v
            })
            .collect();

        // C: units_in_frame_total = Source + FEC -> 3 Source + 1 FEC = total 4
        let mut fp = FrameProcessor::new();
        let p0 = av_packet(0, 4, 1, true, units[0].clone());
        fp.alloc_frame(&p0).unwrap();
        assert_eq!(fp.buf_size_per_unit, 16);
        assert_eq!(fp.buf_stride_per_unit, 16);
        assert_eq!(fp.unit_slots_size, 4);

        for (i, u) in units.iter().enumerate() {
            fp.put_unit(&av_packet(i as u16, 4, 1, true, u.clone())).unwrap();
        }
        assert_eq!(fp.units_source_received, 3);

        // FEC-Unit über die Source-Units berechnen (wie es die Gegenseite tut):
        let mut scratch = vec![0u8; 64];
        for (i, u) in units.iter().enumerate() {
            scratch[i * 16..i * 16 + u.len()].copy_from_slice(u);
        }
        fec::encode(&mut scratch, 16, 16, 3, 1).unwrap();
        let mut all_units = units;
        all_units.push(scratch[48..64].to_vec());

        (fp, all_units)
    }

    #[test]
    fn stream_stats_bitrate() {
        let mut stats = StreamStats::default();
        assert_eq!(stats.bitrate(60), 0);
        stats.frame(1000);
        stats.frame(3000);
        assert_eq!(stats.frames, 2);
        assert_eq!(stats.bytes, 4000);
        // (bytes * 8 * framerate) / frames
        assert_eq!(stats.bitrate(60), 4000 * 8 * 60 / 2);
        stats.reset();
        assert_eq!(stats.frames, 0);
        assert_eq!(stats.bytes, 0);
    }

    #[test]
    fn alloc_frame_size_extension_and_errors() {
        let mut fp = FrameProcessor::new();

        // Zu kleines Video-Unit für die size extension
        let p = av_packet(0, 2, 0, true, vec![0x00]);
        assert_eq!(fp.alloc_frame(&p), Err(ChiakiError::BufTooSmall));

        // unit_size 6 + ext 0x14 (20) = 26 -> stride auf 32 gerundet
        let p = av_packet(0, 2, 0, true, vec![0x00, 0x14, 1, 2, 3, 4]);
        fp.alloc_frame(&p).unwrap();
        assert_eq!(fp.buf_size_per_unit, 26);
        assert_eq!(fp.buf_stride_per_unit, 32);

        // units_in_frame_total < units_in_frame_fec
        let p = av_packet(0, 1, 2, true, vec![0, 0, 1, 2, 3, 4]);
        assert_eq!(fp.alloc_frame(&p), Err(ChiakiError::InvalidData));

        // Mehr als UNIT_SLOTS_MAX
        let mut data = vec![0x00, 0x0c]; // ext 12 -> size 14 -> stride 16
        data.extend_from_slice(&[0u8; 12]);
        let p = av_packet(0, 600, 0, true, data);
        assert_eq!(fp.alloc_frame(&p), Err(ChiakiError::InvalidData));

        // buf_size_per_unit == 0
        let p = av_packet(0, 1, 0, false, Vec::new());
        assert_eq!(fp.alloc_frame(&p), Err(ChiakiError::BufTooSmall));

        // Audio-Packets bekommen keine size extension
        let p = av_packet(0, 2, 0, false, vec![0xaa; 10]);
        fp.alloc_frame(&p).unwrap();
        assert_eq!(fp.buf_size_per_unit, 10, "Audio: keine ntohs-Erweiterung");
        assert_eq!(fp.buf_stride_per_unit, 16);
    }

    #[test]
    fn put_unit_validation() {
        let mut fp = FrameProcessor::new();
        let p0 = av_packet(0, 4, 1, true, vec![0x00, 0x0c, 0xaa, 0xbb]);
        fp.alloc_frame(&p0).unwrap(); // size = 4 + 12 = 16, slots = 4

        // unit_index >= units_in_frame_total
        let p = av_packet(4, 4, 1, true, vec![0x00, 0x0c, 0xaa, 0xbb]);
        assert_eq!(fp.put_unit(&p), Err(ChiakiError::InvalidData));

        // FEC-Unit (Index 3 == source_expected) ist gültig
        let p = av_packet(3, 4, 1, true, vec![0x11; 4]);
        fp.put_unit(&p).unwrap();
        assert_eq!(fp.units_fec_received, 1);
        assert_eq!(fp.units_source_received, 0);

        // leere Unit
        let p = av_packet(1, 4, 1, true, Vec::new());
        assert_eq!(fp.put_unit(&p), Err(ChiakiError::InvalidData));

        // Unit größer als buf_size_per_unit
        let p = av_packet(1, 4, 1, true, vec![0xaa; 17]);
        assert_eq!(fp.put_unit(&p), Err(ChiakiError::InvalidData));

        // Duplikat
        let p = av_packet(3, 4, 1, true, vec![0x11; 4]);
        assert_eq!(fp.put_unit(&p), Err(ChiakiError::InvalidData));
    }

    #[test]
    fn flush_complete_frame_without_fec() {
        let (mut fp, units) = setup_frame();
        // FEC-Unit auch setzen -> alles da, kein FEC nötig
        let pf = av_packet(3, 4, 1, true, units[3].clone());
        fp.put_unit(&pf).unwrap();
        assert!(fp.flush_possible());

        let (result, frame) = fp.flush();
        assert_eq!(result, FlushResult::Success);
        assert_eq!(frame, b"AAAAAAAABBBBBBBBBBCCCCCC");
        assert_eq!(fp.stream_stats().frames, 1);
        assert_eq!(fp.stream_stats().bytes, 24);
    }

    #[test]
    fn flush_reconstructs_missing_unit_via_fec() {
        let (_fp, units) = setup_frame();
        // Unit 0 fehlt: neuen Prozessor bauen und nur Units 1..3 setzen.
        let mut fp = FrameProcessor::new();
        let p0 = av_packet(0, 4, 1, true, units[0].clone());
        fp.alloc_frame(&p0).unwrap();
        fp.put_unit(&av_packet(1, 4, 1, true, units[1].clone())).unwrap();
        fp.put_unit(&av_packet(2, 4, 1, true, units[2].clone())).unwrap();
        fp.put_unit(&av_packet(3, 4, 1, true, units[3].clone())).unwrap();
        assert!(fp.flush_possible(), "2 Source + 1 FEC >= 3 Source");

        let (result, frame) = fp.flush();
        assert_eq!(result, FlushResult::FecSuccess);
        // Rekonstruierte Unit A: padding = 6 -> data_size 10 -> payload 8
        assert_eq!(frame, b"AAAAAAAABBBBBBBBBBCCCCCC");
    }

    #[test]
    fn flush_fails_when_too_many_units_missing() {
        let mut fp = FrameProcessor::new();
        let p0 = av_packet(0, 4, 1, true, vec![0x00, 0x0c, 0xaa, 0xbb]);
        fp.alloc_frame(&p0).unwrap();
        fp.put_unit(&p0).unwrap();
        assert!(!fp.flush_possible());

        // 3 Source-Units fehlen (2 komplett + 1 gar nicht gesetzt),
        // aber nur 1 FEC-Unit -> decode unmöglich
        let (result, frame) = fp.flush();
        assert_eq!(result, FlushResult::FecFailed);
        // Vorhandene Units werden trotzdem zusammengesetzt:
        // Unit 0: data_size 4 -> payload [0xaa, 0xbb]; Units 1/2 fehlen.
        assert_eq!(frame, b"\xaa\xbb");
    }

    #[test]
    fn flush_without_alloc_fails() {
        let mut fp = FrameProcessor::new();
        let (result, frame) = fp.flush();
        assert_eq!(result, FlushResult::Failed);
        assert!(frame.is_empty());
    }

    #[test]
    fn report_packet_stats_counts_generation() {
        let (mut fp, units) = setup_frame();
        let stats = PacketStats::new();
        // 3 von 4 Units da -> received 3, lost 1
        fp.report_packet_stats(&stats);
        let pf = av_packet(3, 4, 1, true, units[3].clone());
        fp.put_unit(&pf).unwrap();
        fp.report_packet_stats(&stats);
        // 3 + 4 = 7 received, expected 4+4=8 -> lost 1
        assert_eq!(stats.get(true), (7, 1));
    }
}
