// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/bitstream.c + lib/include/chiaki/bitstream.h (chiaki-ng)
// inkl. der zugrundeliegenden Mesa-Primitiven vl_vlc/vl_rbsp (lib/src/vl_rbsp.h).
//
// Die C-Datei nutzt den MSB-first-Bitstrom-Leser `vl_vlc` (64-Bit-Schieberegister,
// Nachladen in Dwords) und die RBSP-Schicht `vl_rbsp` (Emulation-Prevention-Bytes
// 00 00 03 entfernen, NAL-Grenzen über Startcodes finden). Beides wird hier
// 1:1 als [`BitReader`] (vl_vlc) und [`Rbsp`] (vl_rbsp) abgebildet — die
// Bit-Ordnung ist byteidentisch zum C-Code.
//
// Ergänzt um [`BitWriter`] (MSB-first, spiegelbildlich zum BitReader), wie in
// der Portierungsvorgabe gefordert; er kommt im C-Code nicht vor.
//
// Bekannte, bewusst übernommene Eigenheiten des C-Codes:
// - `vl_vlc_bits_left()` doppelt die Eingabelänge (bytes_left wird in
//   vl_vlc_init zusätzlich zu end-data gesetzt). Das ist für die
//   Parser-Ergebnisse relevant (skip_startcode frisst so über das Eingabeende
//   hinaus, statt Phantom-Startcodes in Stale-Bits zu erkennen) und wird
//   exakt nachgebaut.
// - `slice_set_reference_frame_h265` schreibt das used_by_curr_pic_s0_flag
//   direkt in die rohen NAL-Bytes (Fenster data[pos-8..pos], Bitposition
//   31-invalid_bits vom LSB). In Rust wird derselbe raw-Fenster-/Shift-Mechanismus
//   verwendet; ein C-Überlauf (pos < 8) wird abgefangen und liefert false.
// - Stellen, an denen C UB hätte (Shifts > 63, Endlosschleife in ue() bei
//   truncierten Streams, Out-of-bounds-Fenster), terminieren in Rust
//   deterministisch bzw. liefern false. Auf wohlgeformten Streams ist das
//   Verhalten identisch.

use super::error::Codec;

/// Port von `vl_vlc` (lib/src/vl_rbsp.h) — MSB-first-Bitstrom-Leser.
#[derive(Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    /// vl_vlc.data — Zeiger (als Index) auf die noch nicht geladenen Bytes.
    pos: usize,
    /// vl_vlc.end — Lese-Limit, per `limit()` auf das NAL-Ende gesetzt.
    end: usize,
    /// vl_vlc.bytes_left — im C-Code Rest-Eingabe über Puffergrenzen hinweg;
    /// hier 1:1 nachgebaut (inkl. des initiale Doppelzählens mit end-data).
    bytes_left: usize,
    /// vl_vlc.buffer — 64-Bit-Schieberegister, MSB-first.
    buffer: u64,
    /// vl_vlc.invalid_bits — Anzahl zu füllender Bits unten im Register
    /// (negativ: überfüllt). valid_bits = 32 - invalid_bits.
    invalid_bits: i32,
}

impl<'a> BitReader<'a> {
    /// Port von `vl_vlc_init`. (Das Pointer-Alignment von vl_vlc_align_data_ptr
    /// ist in Rust gegenstandslos — die Reihenfolge der ins Register geladenen
    /// Bytes und damit das Ergebnis sind identisch.)
    pub fn new(data: &'a [u8]) -> BitReader<'a> {
        let mut vlc = BitReader {
            data,
            pos: 0,
            end: data.len(),
            bytes_left: data.len(),
            buffer: 0,
            invalid_bits: 32,
        };
        vlc.fillbits();
        vlc
    }

    /// Port von `vl_vlc_fillbits`: füllt das Register so, dass mindestens
    /// 32 Bits gültig sind (solange Eingabe übrig ist).
    pub fn fillbits(&mut self) {
        while self.invalid_bits > 0 {
            let bytes_left = self.end - self.pos;

            // if this input is depleted
            if bytes_left == 0 {
                return;
            }

            if bytes_left >= 4 {
                // enough bytes left, read in a whole dword
                let value = u32::from_be_bytes([
                    self.data[self.pos],
                    self.data[self.pos + 1],
                    self.data[self.pos + 2],
                    self.data[self.pos + 3],
                ]) as u64;

                self.buffer |= value << self.invalid_bits;
                self.pos += 4;
                self.invalid_bits -= 32;

                // buffer is now definitely filled up, avoid the loop test
                break;
            } else {
                // not enough bytes left in buffer, read single bytes
                while self.pos < self.end {
                    self.buffer |= (self.data[self.pos] as u64) << (24 + self.invalid_bits);
                    self.pos += 1;
                    self.invalid_bits -= 8;
                }
            }
        }
    }

    /// Port von `vl_vlc_valid_bits` (als i32; Werte > 32 bzw. < 0 treten am
    /// Eingabeende auf — die Parser-Schicht ahmt das u32-Wrap von C nach).
    fn valid_bits_i(&self) -> i32 {
        32 - self.invalid_bits
    }

    fn valid_bits_u32(&self) -> u32 {
        // C: `return 32 - vlc->invalid_bits;` mit unsigned — Wrap bewusst erhalten.
        (32 - self.invalid_bits) as u32
    }

    /// Port von `vl_vlc_bits_left` (inkl. des C-Doppelzählens von bytes_left).
    pub fn bits_left(&self) -> i64 {
        let bytes_left = (self.end - self.pos) as i64 + self.bytes_left as i64;
        bytes_left * 8 + self.valid_bits_i() as i64
    }

    /// Reale (nicht doppelt gezählte) Anzahl verbleibender Bits — nur für
    /// Abbruchschranken gegen truncierte Streams (C hätte hier UB).
    fn real_bits_left(&self) -> i64 {
        (self.end - self.pos) as i64 * 8 + self.valid_bits_i() as i64
    }

    /// Port von `vl_vlc_peekbits`: die nächsten `num_bits` lesen, ohne zu
    /// konsumieren. Gibt die vollen 64 Bits zurück (C schneidet auf unsigned
    /// ab — die Nutzer maskieren selbst, das Verhalten ist identisch).
    pub fn peek_bits(&self, num_bits: u32) -> u64 {
        if num_bits == 0 || num_bits > 64 {
            return 0; // C: UB (Shift >= 64) — deterministisch abfangen
        }
        self.buffer >> (64 - num_bits)
    }

    /// Port von `vl_vlc_eatbits`.
    pub fn eat_bits(&mut self, num_bits: u32) {
        self.buffer <<= num_bits;
        self.invalid_bits += num_bits as i32;
    }

    /// Port von `vl_vlc_get_uimsbf`: `num_bits` (MSB-first, unsigned) lesen
    /// und konsumieren.
    pub fn get_bits(&mut self, num_bits: u32) -> u32 {
        if num_bits == 0 {
            return 0;
        }
        let value = self.peek_bits(num_bits) as u32;
        self.eat_bits(num_bits);
        value
    }

    /// Port von `vl_vlc_get_simsbf`: `num_bits` als vorzeichenbehafteter Wert.
    pub fn get_bits_signed(&mut self, num_bits: u32) -> i32 {
        if num_bits == 0 || num_bits > 32 {
            return 0;
        }
        let value = ((self.buffer as i64) >> (64 - num_bits)) as i32;
        self.eat_bits(num_bits);
        value
    }

    /// Port von `vl_vlc_search_byte`: byteweise nach `value` suchen
    /// (erst im Bitregister, dann direkt in den Bytes). `num_bits == u32::MAX`
    /// bedeutet unbegrenzt (so wird es aus vl_rbsp_init genutzt).
    pub fn search_byte(&mut self, mut num_bits: u32, value: u8) -> bool {
        // deplete the bit buffer
        while self.valid_bits_i() > 0 {
            if self.peek_bits(8) as u8 == value {
                self.fillbits();
                return true;
            }
            self.eat_bits(8);

            if num_bits != u32::MAX {
                num_bits -= 8;
                if num_bits == 0 {
                    return false;
                }
            }
        }

        // deplete the byte buffers
        loop {
            // if this input is depleted
            if self.pos == self.end {
                return false;
            }

            if self.data[self.pos] == value {
                // vl_vlc_align_data_ptr ist in Rust ein No-op (kein
                // Pointer-Alignment nötig)
                self.fillbits();
                return true;
            }

            self.pos += 1;
            if num_bits != u32::MAX {
                num_bits -= 8;
                if num_bits == 0 {
                    return false;
                }
            }
        }
    }

    /// Port von `vl_vlc_removebits`: `num_bits` Bits ab Position `pos` (von
    /// MSB gezählt) aus dem Register entfernen (für die Emulation-Prevention).
    pub fn remove_bits(&mut self, pos: u32, num_bits: u32) {
        let below = if pos + num_bits >= 64 {
            0 // C: UB (Shift >= 64); korrektes Ergebnis hier ist 0
        } else {
            (self.buffer & (u64::MAX >> (pos + num_bits))) << num_bits
        };
        let above = if pos == 0 {
            0 // C: UB (Shift >= 64)
        } else {
            self.buffer & (u64::MAX << (64 - pos))
        };
        self.buffer = below | above;
        self.invalid_bits += num_bits as i32;
    }

    /// Port von `vl_vlc_limit`: die verbleibende Lese-Länge auf `bits` Bits
    /// begrenzen (NAL-Ende).
    fn limit(&mut self, bits: i64) {
        self.fillbits();
        let valid = self.valid_bits_i() as i64;
        if bits < valid {
            if bits <= 0 {
                self.buffer = 0; // C: UB (Shift >= 64)
            } else {
                self.invalid_bits = (32 - bits) as i32;
                self.buffer &= u64::MAX << (self.invalid_bits + 32);
            }
            self.end = self.pos;
            self.bytes_left = 0;
        } else {
            let bytes_after = ((bits - valid) / 8) as usize;
            let remaining = self.end - self.pos;
            if bytes_after < remaining {
                self.end = self.pos + bytes_after;
                self.bytes_left = 0;
            } else {
                self.bytes_left -= remaining;
            }
        }
    }

    /// Rohposition des nächsten zu lesenden Bits (wie im C-Code von
    /// slice_set_reference_frame_h265 benutzt: `8*data - 32 + invalid_bits`).
    fn raw_window(&self) -> Option<(usize, u32)> {
        // Fenster der letzten 8 geladenen Bytes und die Bitposition des
        // nächsten zu lesenden Bits darin (31 - invalid_bits vom LSB).
        if self.pos < 8 {
            return None; // C liest hier Out-of-bounds (UB)
        }
        let shift_lsb = (31 - self.invalid_bits) as u32;
        if shift_lsb >= 64 {
            return None; // kann bei regulärem Ablauf nicht passieren (inv <= 24)
        }
        Some((self.pos - 8, shift_lsb))
    }
}

/// Port von `vl_rbsp` (lib/src/vl_rbsp.h) — RBSP-Schicht über dem NAL-Bitstrom:
/// entfernt Emulation-Prevention-Bytes (00 00 03 -> 00 00) on-the-fly.
pub struct Rbsp<'a> {
    nal: BitReader<'a>,
    escaped: u32,
    /// Anzahl entfernter Bits (wie C rbsp->removed; nur aus Port-Treue erhalten).
    #[allow(dead_code)]
    removed: u32,
}

impl<'a> Rbsp<'a> {
    /// Port von `vl_rbsp_init`: `nal` ist der nach dem NAL-Header positionierte
    /// BitReader; er wird (wie in C) bis zum nächsten Startcode weitergespult,
    /// während die Kopie in `Rbsp` auf das NAL-Ende limitiert wird.
    pub fn init(nal: &mut BitReader<'a>) -> Rbsp<'a> {
        let mut rbsp_nal = nal.clone();
        let bits_at_start = nal.bits_left();

        // search for the end of the NAL unit
        while nal.search_byte(u32::MAX, 0x00) {
            if nal.peek_bits(24) == 0x000001 || nal.peek_bits(32) == 0x00000001 {
                rbsp_nal.limit(bits_at_start - nal.bits_left());
                break;
            }
            nal.eat_bits(8);
        }

        // search for the emulation prevention three byte
        let valid = rbsp_nal.valid_bits_u32();
        let mut i: u32 = 24;
        while i <= valid {
            if rbsp_nal.peek_bits(i) & 0xffffff == 0x3 {
                rbsp_nal.remove_bits(i - 8, 8);
                i += 8;
            }
            i += 8;
        }

        let valid = rbsp_nal.valid_bits_u32();
        let escaped = if valid >= 16 {
            16
        } else if valid >= 8 {
            8
        } else {
            0
        };

        Rbsp {
            nal: rbsp_nal,
            escaped,
            removed: 0,
        }
    }

    /// Port von `vl_rbsp_fillbits`: sorgt für >= 32 gültige Bits und entfernt
    /// dabei Emulation-Prevention-Bytes (inkrementell, escaped/removed wie C).
    fn fillbits(&mut self) {
        let valid_pre = self.nal.valid_bits_u32();

        // abort if we still have enough bits
        if valid_pre >= 32 {
            return;
        }

        self.nal.fillbits();

        // abort if we have less than 24 bits left in this nal
        // (C nutzt hier die doppelt gezählte bits_left — Verhalten identisch)
        if self.nal.bits_left() < 24 {
            return;
        }

        // handle the already escaped bits (u32-Wrap von C bewusst erhalten: bei
        // valid_pre < escaped wird der Startpunkt der Suche identisch zu C
        // "modulo 2^32" berechnet und landet damit wieder im gültigen Bereich)
        let valid = valid_pre.wrapping_sub(self.escaped);

        // search for the emulation prevention three byte
        self.escaped = 16;
        let mut bits = self.nal.valid_bits_u32();
        let mut i = valid.wrapping_add(24);
        while i <= bits {
            if self.nal.peek_bits(i) & 0xffffff == 0x3 {
                self.nal.remove_bits(i - 8, 8);
                self.escaped = bits - i;
                bits -= 8;
                self.removed += 8;
                i += 8;
            }
            i += 8;
        }
    }

    /// Port von `vl_rbsp_u`: unsigned Integer aus den ersten n Bits.
    pub fn u(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        self.fillbits();
        if n > 16 {
            self.fillbits();
        }
        // n > 32 hätte in C UB; der Parser nutzt maximal 32.
        self.nal.get_bits(n.min(32))
    }

    /// Port von `vl_rbsp_ue`: unsigned exponential-Golomb-kodierter Integer.
    pub fn ue(&mut self) -> u32 {
        let mut bits: u32 = 0;

        self.fillbits();
        while self.nal.get_bits(1) == 0 {
            bits += 1;
            if bits >= 32 || self.nal.real_bits_left() <= 0 {
                // truncierter Stream: C würde hier endlos lesen/UB haben.
                return u32::MAX;
            }
        }

        (1u32 << bits).wrapping_sub(1).wrapping_add(self.u(bits))
    }

    /// Port von `vl_rbsp_se`: signed exponential-Golomb-kodierter Integer.
    pub fn se(&mut self) -> i32 {
        let code_num = self.ue();
        if code_num & 1 == 1 {
            ((code_num + 1) >> 1) as i32
        } else {
            -((code_num >> 1) as i32)
        }
    }

    /// Position (Fensterstart, Bitshift vom LSB) des nächsten zu lesenden
    /// rohen Bits — für slice_set_reference_frame_h265.
    fn raw_window(&self) -> Option<(usize, u32)> {
        self.nal.raw_window()
    }
}

/// Port von `ChiakiBitstreamSliceType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceType {
    Unknown = 0,
    I,
    P,
}

/// Port von `ChiakiBitstreamSlice`.
#[derive(Debug, Clone, Copy)]
pub struct Slice {
    pub slice_type: SliceType,
    pub reference_frame: u32,
}

impl Default for Slice {
    fn default() -> Self {
        Slice {
            slice_type: SliceType::Unknown,
            reference_frame: u32::MAX,
        }
    }
}

/// Port von `ChiakiBitstream` (lib/include/chiaki/bitstream.h).
///
/// Die C-Union (h264/h265-SPS-Werte) wird als zwei Felder abgebildet; wie in C
/// werden beide bei jedem `header()`-Aufruf genullt (memset der Union).
pub struct Bitstream {
    codec: Codec,
    h264_sps_log2_max_frame_num_minus4: u32,
    h265_sps_log2_max_pic_order_cnt_lsb_minus4: u32,
}

impl Bitstream {
    /// Port von `chiaki_bitstream_init`.
    pub fn new(codec: Codec) -> Bitstream {
        Bitstream {
            codec,
            h264_sps_log2_max_frame_num_minus4: 0,
            h265_sps_log2_max_pic_order_cnt_lsb_minus4: 0,
        }
    }

    /// Port von `chiaki_bitstream_header`: SPS parsen und den passenden
    /// log2-Wert setzen. false = Stream unverwertbar.
    pub fn header(&mut self, data: &[u8]) -> bool {
        match self.codec {
            Codec::H264 => {
                self.h264_sps_log2_max_frame_num_minus4 = 0;
                self.header_h264(data)
            }
            _ => {
                self.h265_sps_log2_max_pic_order_cnt_lsb_minus4 = 0;
                self.header_h265(data)
            }
        }
    }

    /// Port von `chiaki_bitstream_slice`.
    pub fn slice(&self, data: &[u8], slice: &mut Slice) -> bool {
        match self.codec {
            Codec::H264 => self.slice_h264(data, slice),
            _ => self.slice_h265(data, slice),
        }
    }

    /// Port von `chiaki_bitstream_slice_set_reference_frame`.
    ///
    /// Ändert (wie das C-Original) `data` in-place: das
    /// used_by_curr_pic_s0_flag des gewünschten Referenzframes wird gesetzt,
    /// die der niedrigeren Indizes gelöscht. Nur für H265 (H264 liefert false).
    pub fn slice_set_reference_frame(&self, data: &mut [u8], reference_frame: u32) -> bool {
        match self.codec {
            Codec::H264 => false,
            _ => self.slice_set_reference_frame_h265(data, reference_frame),
        }
    }

    /// Port von `skip_startcode` (lib/src/bitstream.c).
    fn skip_startcode(vlc: &mut BitReader) -> bool {
        vlc.fillbits();
        for _ in 0..64 {
            if vlc.bits_left() < 32 {
                break;
            }
            if vlc.peek_bits(32) == 1 {
                break;
            }
            vlc.eat_bits(8);
            vlc.fillbits();
        }
        if vlc.peek_bits(32) != 1 {
            return false;
        }
        vlc.eat_bits(32);
        vlc.fillbits();
        true
    }

    /// Port von `header_h264`.
    fn header_h264(&mut self, data: &[u8]) -> bool {
        let mut vlc = BitReader::new(data);
        if !Self::skip_startcode(&mut vlc) {
            tracing::warn!("parse_sps_h264: No startcode found");
            return false;
        }

        vlc.eat_bits(1); // forbidden_zero_bit
        vlc.eat_bits(2); // nal_ref_idc
        let nal_unit_type = vlc.get_bits(5);

        if nal_unit_type != 7 {
            tracing::warn!("parse_sps_h264: Unexpected NAL unit type {nal_unit_type}");
            return false;
        }

        let mut rbsp = Rbsp::init(&mut vlc);

        let profile_idc = rbsp.u(8);

        rbsp.u(6); // constraint_set_flags
        rbsp.u(2); // reserved_zero_2bits
        rbsp.u(8); // level_idc

        rbsp.ue(); // seq_parameter_set_id

        if matches!(
            profile_idc,
            100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
        ) {
            if rbsp.ue() == 3 {
                // chroma_format_idc
                rbsp.u(1); // separate_colour_plane_flag
            }

            rbsp.ue(); // bit_depth_luma_minus8
            rbsp.ue(); // bit_depth_chroma_minus8
            rbsp.u(1); // qpprime_y_zero_transform_bypass_flag

            if rbsp.u(1) == 1 {
                // seq_scaling_matrix_present_flag
                return false;
            }
        }

        let log2_max_frame_num_minus4 = rbsp.ue();
        if log2_max_frame_num_minus4 > 12 {
            tracing::warn!(
                "parse_sps_h264: Unexpected log2_max_frame_num_minus4 value {log2_max_frame_num_minus4}"
            );
            return false;
        }

        self.h264_sps_log2_max_frame_num_minus4 = log2_max_frame_num_minus4;
        true
    }

    /// Port von `header_h265` (das `goto sps_start` bei VPS wird zur Schleife).
    fn header_h265(&mut self, data: &[u8]) -> bool {
        let mut vlc = BitReader::new(data);
        // sps_start:
        loop {
            if !Self::skip_startcode(&mut vlc) {
                tracing::warn!("parse_sps_h265: No startcode found");
                return false;
            }

            vlc.eat_bits(1); // forbidden_zero_bit
            let nal_unit_type = vlc.get_bits(6);
            vlc.eat_bits(6); // nuh_layer_id
            vlc.eat_bits(3); // nuh_temporal_id_plus1

            if nal_unit_type == 32 {
                // VPS -> weiter zum nächsten NAL (goto sps_start)
                continue;
            }

            if nal_unit_type != 33 {
                tracing::warn!("parse_sps_h265: Unexpected NAL unit type {nal_unit_type}");
                return false;
            }
            break;
        }

        let mut rbsp = Rbsp::init(&mut vlc);

        rbsp.u(4); // sps_video_parameter_set_id
        rbsp.u(3); // sps_max_sub_layers_minus1
        rbsp.u(1); // sps_temporal_id_nesting_flag

        rbsp.u(2); // general_profile_space
        rbsp.u(1); // general_tier_flag
        rbsp.u(5); // general_profile_idc
        rbsp.u(32); // general_profile_compatibility_flag[0-31]
        rbsp.u(1); // general_progressive_source_flag
        rbsp.u(1); // general_interlaced_source_flag
        rbsp.u(1); // general_non_packed_constraint_flag
        rbsp.u(1); // general_frame_only_constraint_flag
        rbsp.u(32);
        rbsp.u(11); // general_reserved_zero_43bits
        rbsp.u(1); // general_inbld_flag / general_reserved_zero_bit
        rbsp.u(8); // general_level_idc

        rbsp.ue(); // sps_seq_parameter_set_id
        if rbsp.ue() == 3 {
            // chroma_format_idc
            rbsp.u(1); // separate_colour_plane_flag
        }

        rbsp.ue(); // pic_width_in_luma_samples
        rbsp.ue(); // pic_height_in_luma_samples

        if rbsp.u(1) == 1 {
            // conformance_window_flag
            rbsp.ue(); // conf_win_left_offset
            rbsp.ue(); // conf_win_right_offset
            rbsp.ue(); // conf_win_top_offset
            rbsp.ue(); // conf_win_bottom_offset
        }

        rbsp.ue(); // bit_depth_luma_minus8
        rbsp.ue(); // bit_depth_chroma_minus8

        let log2_max_pic_order_cnt_lsb_minus4 = rbsp.ue();
        if log2_max_pic_order_cnt_lsb_minus4 > 12 {
            tracing::warn!(
                "parse_sps_h265: Unexpected log2_max_pic_order_cnt_lsb_minus4 value {log2_max_pic_order_cnt_lsb_minus4}"
            );
            return false;
        }

        self.h265_sps_log2_max_pic_order_cnt_lsb_minus4 = log2_max_pic_order_cnt_lsb_minus4;
        true
    }

    /// Port von `slice_h264`.
    fn slice_h264(&self, data: &[u8], slice: &mut Slice) -> bool {
        let mut vlc = BitReader::new(data);
        if !Self::skip_startcode(&mut vlc) {
            tracing::warn!("parse_slice_h264: No startcode found");
            return false;
        }

        vlc.eat_bits(1); // forbidden_zero_bit
        vlc.eat_bits(2); // nal_ref_idc
        let nal_unit_type = vlc.get_bits(5);

        if nal_unit_type != 1 && nal_unit_type != 5 {
            tracing::warn!("parse_slice_h264: Unexpected NAL unit type {nal_unit_type}");
            return false;
        }

        let mut rbsp = Rbsp::init(&mut vlc);
        rbsp.ue(); // first_mb_in_slice

        slice.slice_type = match rbsp.ue() {
            0 | 5 => SliceType::P,
            2 | 7 => SliceType::I,
            _ => SliceType::Unknown,
        };

        if nal_unit_type == 1 {
            slice.reference_frame = 0;
            rbsp.ue(); // pic_parameter_set_id
            rbsp.u(self.h264_sps_log2_max_frame_num_minus4 + 4); // frame_num
            if rbsp.u(1) == 1 {
                // num_ref_idx_active_override_flag
                if rbsp.u(1) == 1 {
                    // num_ref_idx_active_override_flag
                    rbsp.ue(); // num_ref_idx_l0_active_minus1
                }
            }
            if rbsp.u(1) == 1 {
                // ref_pic_list_modification_flag_l0
                let mut i: u32 = 0;
                let mut modification_of_pic_nums_idc = rbsp.ue();
                // C: while(i++ < 3)
                while {
                    let old = i;
                    i += 1;
                    old < 3
                } {
                    if modification_of_pic_nums_idc == 0 {
                        slice.reference_frame = rbsp.ue(); // abs_diff_pic_num_minus1
                    } else if modification_of_pic_nums_idc < 3 {
                        rbsp.ue(); // abs_diff_pic_num_minus1 or long_term_pic_num
                    } else if modification_of_pic_nums_idc == 3 {
                        return true;
                    } else {
                        break;
                    }
                    modification_of_pic_nums_idc = rbsp.ue();
                }
                tracing::warn!("parse_slice_h264: Failed to parse ref_pic_list_modification");
                return false;
            }
        }

        true
    }

    /// Port von `slice_h265`.
    fn slice_h265(&self, data: &[u8], slice: &mut Slice) -> bool {
        let mut vlc = BitReader::new(data);

        if !Self::skip_startcode(&mut vlc) {
            tracing::warn!("parse_slice_h265: No startcode found");
            return false;
        }

        vlc.eat_bits(1); // forbidden_zero_bit
        let nal_unit_type = vlc.get_bits(6);
        vlc.eat_bits(6); // nuh_layer_id
        vlc.eat_bits(3); // nuh_temporal_id_plus1

        if nal_unit_type != 1 && nal_unit_type != 20 {
            tracing::warn!("parse_slice_h265: Unexpected NAL unit type {nal_unit_type}");
            return false;
        }

        let mut rbsp = Rbsp::init(&mut vlc);
        let first_slice_segment_in_pic_flag = rbsp.u(1);
        if nal_unit_type == 20 {
            rbsp.u(1); // no_output_of_prior_pics_flag
        }

        rbsp.ue(); // slice_pic_parameter_set_id
        if first_slice_segment_in_pic_flag == 0 {
            rbsp.ue(); // slice_segment_address
        }

        slice.slice_type = match rbsp.ue() {
            1 => SliceType::P,
            2 => SliceType::I,
            _ => SliceType::Unknown,
        };

        if nal_unit_type == 1 {
            slice.reference_frame = 0xff;
            rbsp.u(self.h265_sps_log2_max_pic_order_cnt_lsb_minus4 + 4); // slice_pic_order_cnt_lsb
            if rbsp.u(1) == 0 {
                // short_term_ref_pic_set_sps_flag
                let num_negative_pics = rbsp.ue();
                if num_negative_pics > 16 {
                    tracing::warn!("parse_slice_h265: Unexpected num_negative_pics {num_negative_pics}");
                    return false;
                }
                rbsp.ue(); // num_positive_pics
                for i in 0..num_negative_pics {
                    rbsp.ue(); // delta_poc_s0_minus1[i]
                    if rbsp.u(1) == 1 {
                        // used_by_curr_pic_s0_flag[i]
                        slice.reference_frame = i;
                        break;
                    }
                }
            }
            if slice.reference_frame == 0xff {
                tracing::trace!("parse_slice_h265: No ref frame found");
            }
        }

        true
    }

    /// Port von `slice_set_reference_frame_h265`.
    ///
    /// Der C-Code schreibt das used_by_curr_pic_s0_flag-Bit direkt in die rohen
    /// NAL-Bytes zurück (Fenster data[pos-8..pos], Shift 31-invalid_bits vom
    /// LSB). Da der Leser die Bits schon im Register hat, wird hier zunächst mit
    /// einer unveränderlichen Sicht geparst und die Positionen gesammelt; danach
    /// werden die Bits mit exakt demselben Fenster-/Shift-Mechanismus in `data`
    /// zurückgeschrieben.
    fn slice_set_reference_frame_h265(&self, data: &mut [u8], reference_frame: u32) -> bool {
        // (Fensterstart, Shift vom LSB) je used_by_curr_pic_s0_flag[i]
        let mut flag_windows: Vec<(usize, u32)> = Vec::new();

        let found = {
            let mut vlc = BitReader::new(data);
            if !Self::skip_startcode(&mut vlc) {
                tracing::warn!("slice_set_reference_frame_h265: No startcode found");
                return false;
            }

            vlc.eat_bits(1); // forbidden_zero_bit
            let nal_unit_type = vlc.get_bits(6);
            vlc.eat_bits(6); // nuh_layer_id
            vlc.eat_bits(3); // nuh_temporal_id_plus1

            if nal_unit_type != 1 {
                tracing::warn!("slice_set_reference_frame_h265: Unexpected NAL unit type {nal_unit_type}");
                return false;
            }

            let mut rbsp = Rbsp::init(&mut vlc);
            let first_slice_segment_in_pic_flag = rbsp.u(1);

            rbsp.ue(); // slice_pic_parameter_set_id
            if first_slice_segment_in_pic_flag == 0 {
                rbsp.ue(); // slice_segment_address
            }

            if rbsp.ue() != 1 {
                tracing::warn!("slice_set_reference_frame_h265: Not P slice");
                return false;
            }

            rbsp.u(self.h265_sps_log2_max_pic_order_cnt_lsb_minus4 + 4); // slice_pic_order_cnt_lsb
            if rbsp.u(1) != 0 {
                // short_term_ref_pic_set_sps_flag
                return false;
            }

            let num_negative_pics = rbsp.ue();
            if num_negative_pics > 16 {
                tracing::warn!(
                    "slice_set_reference_frame_h265: Unexpected num_negative_pics {num_negative_pics}"
                );
                return false;
            }
            rbsp.ue(); // num_positive_pics

            let mut found = false;
            for i in 0..num_negative_pics {
                rbsp.ue(); // delta_poc_s0_minus1[i]

                match rbsp.raw_window() {
                    Some(w) => flag_windows.push(w),
                    None => {
                        // C würde hier Out-of-bounds lesen/schreiben (UB)
                        tracing::warn!("slice_set_reference_frame_h265: NAL too short to rewrite");
                        return false;
                    }
                }

                if i == reference_frame {
                    found = true;
                    break;
                }
                rbsp.u(1); // used_by_curr_pic_s0_flag[i]
            }
            found
        };

        // Zurückschreiben in die rohen Bytes wie im C-Code: alle Flags der
        // durchlaufenen Iterationen löschen, an der Zielposition setzen.
        for (i, &(window_start, shift_lsb)) in flag_windows.iter().enumerate() {
            let set = found && i as u32 == reference_frame;
            write_raw_bit(data, window_start, shift_lsb, set);
        }

        found
    }
}

/// Setzt/löscht ein einzelnes Bit in den rohen NAL-Bytes — Äquivalent zum
/// `d[-2]/d[-1]`-Fensterrück schreib-Code in slice_set_reference_frame_h265.
fn write_raw_bit(data: &mut [u8], window_start: usize, shift_lsb: u32, set: bool) {
    let mut window = u64::from_be_bytes(
        data[window_start..window_start + 8]
            .try_into()
            .expect("Fenster hat exakt 8 Bytes (obvious invariant)"),
    );
    let mask = 1u64 << shift_lsb;
    if set {
        window |= mask;
    } else {
        window &= !mask;
    }
    data[window_start..window_start + 8].copy_from_slice(&window.to_be_bytes());
}

/// MSB-first-Bit-Schreiber (Ergänzung zum BitReader, Bit-Ordnung identisch:
/// das MSB eines jeden Bytes wird zuerst geschrieben).
#[derive(Default)]
pub struct BitWriter {
    out: Vec<u8>,
    bit_len: usize,
}

impl BitWriter {
    pub fn new() -> BitWriter {
        BitWriter {
            out: Vec::new(),
            bit_len: 0,
        }
    }

    /// Schreibt die unteren `n` Bits von `value` MSB-first (wie der C-Code der
    /// H.264/H.265-Streams sie liest: `u(n)`).
    pub fn write_bits(&mut self, value: u32, n: u32) {
        debug_assert!(n <= 32);
        for i in (0..n).rev() {
            let bit_pos = self.bit_len % 8;
            if bit_pos == 0 {
                self.out.push(0);
            }
            if (value >> i) & 1 == 1 {
                let last = self.out.len() - 1; // invariant: bit_pos != 0 => out nicht leer
                self.out[last] |= 0x80 >> bit_pos;
            }
            self.bit_len += 1;
        }
    }

    /// Bisher geschriebene Bitlänge.
    pub fn bit_len(&self) -> usize {
        self.bit_len
    }

    /// Schließt den Byte-Strom ab (letztes Byte wird mit 0-Bits aufgefüllt).
    pub fn finish(self) -> Vec<u8> {
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Golden-Tests aus chiaki-ng test/bitstream.c (1:1 übernommen) ----

    #[test]
    fn test_bitstream_parse_h264() {
        let mut bs = Bitstream::new(Codec::H264);
        let mut slice = Slice::default();

        let header: [u8; 40] = [
            0x00, 0x00, 0x00, 0x01, 0x67, 0x4d, 0x40, 0x32, 0x91, 0x8a, 0x01, 0xe0, 0x08, 0x9f,
            0x97, 0x01, 0x6a, 0x02, 0x02, 0x02, 0x80, 0x00, 0x03, 0xe9, 0x00, 0x01, 0xd4, 0xc0,
            0x44, 0xd0, 0xf1, 0xf1, 0x50, 0x00, 0x00, 0x00, 0x01, 0x68, 0xee, 0x3c,
        ];
        bs.h264_sps_log2_max_frame_num_minus4 = u32::MAX; // C: memset(-1)
        assert!(bs.header(&header));
        assert_eq!(bs.h264_sps_log2_max_frame_num_minus4, 3);

        let slice_i: [u8; 32] = [
            0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x80, 0x82, 0x1f, 0x00, 0x49, 0xee, 0x03, 0x29,
            0xff, 0xf8, 0x7f, 0x88, 0x46, 0x44, 0x77, 0x17, 0xe7, 0x6d, 0xb3, 0xad, 0x38, 0x19,
            0x74, 0x5a, 0xf1, 0x51,
        ];
        slice.reference_frame = u32::MAX;
        assert!(bs.slice(&slice_i, &mut slice));
        assert_eq!(slice.slice_type, SliceType::I);

        let slice_p: [u8; 32] = [
            0x00, 0x00, 0x00, 0x01, 0x41, 0x9a, 0x04, 0x44, 0x3f, 0x41, 0x5b, 0xf4, 0x65, 0xb4,
            0x3e, 0x1a, 0xd3, 0xa0, 0x28, 0x1f, 0x83, 0x63, 0x0e, 0xc2, 0xfc, 0x9d, 0x7a, 0xc7,
            0xc4, 0x7d, 0xf9, 0x18,
        ];
        slice.reference_frame = u32::MAX;
        assert!(bs.slice(&slice_p, &mut slice));
        assert_eq!(slice.slice_type, SliceType::P);
        assert_eq!(slice.reference_frame, 0);

        let slice_p_ref_5: [u8; 32] = [
            0x00, 0x00, 0x00, 0x01, 0x41, 0x9b, 0xfd, 0x98, 0x89, 0xdf, 0x00, 0x03, 0x24, 0x60,
            0x47, 0x1a, 0x90, 0x10, 0xb3, 0x2c, 0x4e, 0x45, 0xfc, 0xff, 0x45, 0x24, 0x8c, 0x79,
            0xec, 0x12, 0xe5, 0x9b,
        ];
        slice.reference_frame = u32::MAX;
        assert!(bs.slice(&slice_p_ref_5, &mut slice));
        assert_eq!(slice.slice_type, SliceType::P);
        assert_eq!(slice.reference_frame, 5);
    }

    #[test]
    fn test_bitstream_parse_h265() {
        let mut bs = Bitstream::new(Codec::H265);
        let mut slice = Slice::default();

        let header: [u8; 92] = [
            0x00, 0x00, 0x00, 0x01, 0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x01, 0x60, 0x00, 0x00,
            0x03, 0x00, 0xb0, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x96, 0x0a, 0xc0, 0x90,
            0x00, 0x00, 0x00, 0x01, 0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0xb0,
            0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x96, 0xa0, 0x03, 0xc0, 0x80, 0x11, 0x07,
            0xcb, 0xc2, 0xb9, 0x24, 0x29, 0x52, 0x70, 0x16, 0xa0, 0x20, 0x20, 0x20, 0x80, 0x00,
            0x07, 0xd2, 0x00, 0x01, 0xd4, 0xc0, 0x20, 0xe5, 0xa1, 0xe3, 0xd0, 0x00, 0x00, 0x00,
            0x01, 0x44, 0x01, 0xc0, 0xf3, 0xc0, 0x4c, 0x90,
        ];
        bs.h265_sps_log2_max_pic_order_cnt_lsb_minus4 = u32::MAX; // C: memset(-1)
        assert!(bs.header(&header));
        assert_eq!(bs.h265_sps_log2_max_pic_order_cnt_lsb_minus4, 0);

        let slice_i: [u8; 32] = [
            0x00, 0x00, 0x00, 0x01, 0x28, 0x01, 0xac, 0x25, 0xcf, 0x83, 0xff, 0x23, 0x54, 0xab,
            0x5c, 0xf5, 0x7a, 0x06, 0x7c, 0x3f, 0x31, 0x9b, 0xe6, 0x10, 0x57, 0xe8, 0x0e, 0xcf,
            0xdd, 0xda, 0xdb, 0x3f,
        ];
        slice.reference_frame = u32::MAX;
        assert!(bs.slice(&slice_i, &mut slice));
        assert_eq!(slice.slice_type, SliceType::I);

        let slice_p: [u8; 32] = [
            0x00, 0x00, 0x00, 0x01, 0x02, 0x01, 0xd0, 0x97, 0x61, 0x28, 0x23, 0x2d, 0x8b, 0x80,
            0x6f, 0xfd, 0x2f, 0x2b, 0x11, 0xd4, 0x55, 0x04, 0x90, 0x18, 0x49, 0xe5, 0xbc, 0xc4,
            0x97, 0xbc, 0x3d, 0xeb,
        ];
        slice.reference_frame = u32::MAX;
        assert!(bs.slice(&slice_p, &mut slice));
        assert_eq!(slice.slice_type, SliceType::P);
        assert_eq!(slice.reference_frame, 0);

        let slice_p_ref_5: [u8; 32] = [
            0x00, 0x00, 0x00, 0x01, 0x02, 0x01, 0xd7, 0x85, 0x6a, 0xae, 0xa6, 0x11, 0x80, 0x95,
            0x80, 0x0a, 0xec, 0x5e, 0xdf, 0x39, 0x86, 0xe6, 0xd9, 0x07, 0x49, 0x17, 0xe2, 0x62,
            0x57, 0x14, 0xd7, 0x08,
        ];
        slice.reference_frame = u32::MAX;
        assert!(bs.slice(&slice_p_ref_5, &mut slice));
        assert_eq!(slice.slice_type, SliceType::P);
        assert_eq!(slice.reference_frame, 5);
    }

    #[test]
    fn test_bitstream_issue_213() {
        let mut bs = Bitstream::new(Codec::H265);
        let mut slice = Slice::default();

        let header: [u8; 92] = [
            0x00, 0x00, 0x00, 0x01, 0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x01, 0x60, 0x00, 0x00,
            0x03, 0x00, 0xb0, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x96, 0x0a, 0xc0, 0x90,
            0x00, 0x00, 0x00, 0x01, 0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0xb0,
            0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x96, 0xa0, 0x03, 0xc0, 0x80, 0x11, 0x07,
            0xcb, 0xc2, 0xb9, 0x24, 0x29, 0x52, 0x70, 0x16, 0xa0, 0x20, 0x20, 0x20, 0x80, 0x00,
            0x07, 0xd2, 0x00, 0x01, 0xd4, 0xc0, 0x20, 0xe5, 0xa1, 0xe3, 0xd0, 0x00, 0x00, 0x00,
            0x01, 0x44, 0x01, 0xc0, 0xf3, 0xc0, 0x4c, 0x90,
        ];
        assert!(bs.header(&header));

        let slice_p: [u8; 16] = [
            0x00, 0x00, 0x00, 0x01, 0x02, 0x01, 0xd2, 0x0b, 0xea, 0x60, 0x86, 0x82, 0x3d, 0x00,
            0x00, 0x03,
        ];
        slice.reference_frame = u32::MAX;
        assert!(bs.slice(&slice_p, &mut slice));
        assert_eq!(slice.slice_type, SliceType::P);
        assert_eq!(slice.reference_frame, 0);
    }

    #[test]
    fn test_bitstream_set_ref_h265() {
        let mut bs = Bitstream::new(Codec::H265);
        let mut slice = Slice::default();

        let header: [u8; 92] = [
            0x00, 0x00, 0x00, 0x01, 0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x01, 0x60, 0x00, 0x00,
            0x03, 0x00, 0xb0, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x96, 0x0a, 0xc0, 0x90,
            0x00, 0x00, 0x00, 0x01, 0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0xb0,
            0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x96, 0xa0, 0x03, 0xc0, 0x80, 0x11, 0x07,
            0xcb, 0xc2, 0xb9, 0x24, 0x29, 0x52, 0x70, 0x16, 0xa0, 0x20, 0x20, 0x20, 0x80, 0x00,
            0x07, 0xd2, 0x00, 0x01, 0xd4, 0xc0, 0x20, 0xe5, 0xa1, 0xe3, 0xd0, 0x00, 0x00, 0x00,
            0x01, 0x44, 0x01, 0xc0, 0xf3, 0xc0, 0x4c, 0x90,
        ];
        bs.h265_sps_log2_max_pic_order_cnt_lsb_minus4 = u32::MAX; // C: memset(-1)
        assert!(bs.header(&header));
        assert_eq!(bs.h265_sps_log2_max_pic_order_cnt_lsb_minus4, 0);

        let mut slice_p: [u8; 32] = [
            0x00, 0x00, 0x00, 0x01, 0x02, 0x01, 0xd2, 0x85, 0x7a, 0xaa, 0xa6, 0x08, 0x60, 0x13,
            0x55, 0x17, 0x6b, 0x71, 0x72, 0xf9, 0x6e, 0xd4, 0xf2, 0x66, 0x78, 0x0c, 0x12, 0xe7,
            0x79, 0xf0, 0xbc, 0xc9,
        ];
        slice.reference_frame = u32::MAX;
        assert!(bs.slice(&slice_p, &mut slice));
        assert_eq!(slice.slice_type, SliceType::P);
        assert_eq!(slice.reference_frame, 0);

        for i in 0..9u32 {
            assert!(bs.slice_set_reference_frame(&mut slice_p, i));
            let mut slice = Slice::default();
            slice.reference_frame = u32::MAX;
            assert!(bs.slice(&slice_p, &mut slice));
            assert_eq!(slice.slice_type, SliceType::P);
            assert_eq!(slice.reference_frame, i);
        }
        // Slice hat 9 Referenzframes
        assert!(!bs.slice_set_reference_frame(&mut slice_p, 10));
    }

    // ---- BitReader-Grundtests ----

    #[test]
    fn bitreader_msb_first_order() {
        // Bitstrom: 1011 0010 0101 0101
        //  -> 5 Bits = 10110, 5 Bits = 01001, 6 Bits = 010101
        let data = [0b1011_0010u8, 0b0101_0101u8];
        let mut r = BitReader::new(&data);
        assert_eq!(r.get_bits(5), 0b10110);
        assert_eq!(r.get_bits(5), 0b01001);
        assert_eq!(r.get_bits(6), 0b010101);
    }

    #[test]
    fn bitreader_signed_and_32bit_reads() {
        let data = [0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x01];
        let mut r = BitReader::new(&data);
        assert_eq!(r.get_bits_signed(4), -1);
        r.fillbits(); // wie in vl_vlc: Aufrufer füllt vor mehrfach-Dword-Lesezugriffen
        // Bits 4..36: alles Einsen (Bytes 0..4 sind 0xff)
        assert_eq!(r.get_bits(32), 0xffff_ffff);
        r.fillbits();
        // Bits 36..40: 4 Einsen, dann 0x00 0x00 0x00 0x01
        assert_eq!(r.get_bits(4), 0xf);
        r.fillbits();
        assert_eq!(r.get_bits(32), 0x0000_0001);
        let mut r2 = BitReader::new(&[0x80]);
        assert_eq!(r2.get_bits_signed(1), -1);
        assert_eq!(r2.get_bits_signed(2), 0);
    }

    #[test]
    fn bitreader_search_byte_finds_startcode() {
        let data = [0xaa, 0xbb, 0xcc, 0x00, 0x00, 0x00, 0x01, 0x65];
        let mut r = BitReader::new(&data);
        assert!(r.search_byte(u32::MAX, 0x00));
        assert_eq!(r.peek_bits(32), 1); // 00 00 00 01
        r.eat_bits(32);
        assert_eq!(r.get_bits(8), 0x65);
    }

    #[test]
    fn bitreader_rbsp_strips_emulation_prevention() {
        // NAL (2-Byte-H265-Header 26 00), Payload mit zwei Emulation-Prevention-
        // Bytes: 00 00 [03] aa 55 ff 00 00 [03] bb  ->  00 00 aa 55 ff 00 00 bb
        let data = [
            0x00, 0x00, 0x00, 0x01, 0x26, 0x00, 0x00, 0x00, 0x03, 0xaa, 0x55, 0xff, 0x00, 0x00,
            0x03, 0xbb,
        ];
        let mut vlc = BitReader::new(&data);
        assert!(Bitstream::skip_startcode(&mut vlc));
        vlc.eat_bits(1); // forbidden_zero_bit
        assert_eq!(vlc.get_bits(6), (0x26 >> 1) & 0x3f);
        vlc.eat_bits(6); // nuh_layer_id
        vlc.eat_bits(3); // nuh_temporal_id_plus1
        let mut rbsp = Rbsp::init(&mut vlc);
        assert_eq!(rbsp.u(8), 0x00);
        assert_eq!(rbsp.u(8), 0x00);
        assert_eq!(rbsp.u(8), 0xaa);
        assert_eq!(rbsp.u(8), 0x55);
        assert_eq!(rbsp.u(8), 0xff);
        assert_eq!(rbsp.u(8), 0x00);
        assert_eq!(rbsp.u(8), 0x00);
        assert_eq!(rbsp.u(8), 0xbb);
    }

    #[test]
    fn bitreader_ue_se_golomb() {
        // ue(v): (v+1) binär mit n Bits, davor n-1 Nullen:
        // ue(0)="1", ue(1)="010", ue(2)="011", ue(3)="00100", ue(4)="00101"
        let mut w = BitWriter::new();
        w.write_bits(0b1, 1); // ue 0
        w.write_bits(0b010, 3); // ue 1
        w.write_bits(0b011, 3); // ue 2
        w.write_bits(0b00100, 5); // ue 3
        w.write_bits(0b00101, 5); // codeNum 4 -> se(-2)
        let data = w.finish();
        let vlc = BitReader::new(&data);
        let mut rbsp = Rbsp {
            nal: vlc,
            escaped: 0,
            removed: 0,
        };
        assert_eq!(rbsp.ue(), 0);
        assert_eq!(rbsp.ue(), 1);
        assert_eq!(rbsp.ue(), 2);
        assert_eq!(rbsp.ue(), 3);
        assert_eq!(rbsp.se(), -2);
    }

    // ---- BitWriter-Grundtests ----

    #[test]
    fn bitwriter_byte_identical_msb_first() {
        let mut w = BitWriter::new();
        w.write_bits(0b101, 3);
        w.write_bits(0b11, 2);
        w.write_bits(0b0101, 4);
        w.write_bits(0b000_1011, 7);
        w.write_bits(0b11_1100_0011, 10);
        assert_eq!(w.bit_len(), 26);
        let out = w.finish();
        // 3+2+4+7+10 = 26 Bits -> 4 Bytes, letztes Byte mit 0 aufgefüllt
        assert_eq!(out.len(), 4);
        let expect: Vec<bool> = [
            1, 0, 1, // 101
            1, 1, // 11
            0, 1, 0, 1, // 0101
            0, 0, 0, 1, 0, 1, 1, // 0001011
            1, 1, 1, 1, 0, 0, 0, 0, 1, 1, // 1111000011
        ]
        .iter()
        .map(|&b| b == 1)
        .collect();
        let got = w_bits(&out);
        assert_eq!(&got[..26], &expect[..]);
        assert!(got[26..].iter().all(|&b| !b));
        // byteidentisch: 10111010 10001011 11110000 11(000000)
        assert_eq!(out, [0b1011_1010, 0b1000_1011, 0b1111_0000, 0b1100_0000]);
    }

    fn w_bits(data: &[u8]) -> Vec<bool> {
        let mut v = Vec::new();
        for &b in data {
            for i in (0..8).rev() {
                v.push((b >> i) & 1 == 1);
            }
        }
        v
    }

    #[test]
    fn bitwriter_reader_roundtrip() {
        let values: [(u32, u32); 8] = [
            (0x5, 3),
            (0x1234, 13),
            (0xdeadbeef, 32),
            (0x7, 3),
            (0x0, 1),
            (0x1, 1),
            (0x2aa, 10),
            (0xfffff, 20),
        ];
        let mut w = BitWriter::new();
        for &(v, n) in &values {
            w.write_bits(v, n);
        }
        let data = w.finish();
        let vlc = BitReader::new(&data);
        let mut rbsp = Rbsp {
            nal: vlc,
            escaped: 0,
            removed: 0,
        };
        // über Rbsp::u lesen (füllt wie in C vor dem Lesen nach)
        for &(v, n) in &values {
            assert_eq!(rbsp.u(n), v, "roundtrip fehlgeschlagen für {v:#x}/{n}");
        }
    }
}
