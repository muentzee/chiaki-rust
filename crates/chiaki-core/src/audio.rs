// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/audio.c + lib/include/chiaki/audio.h (chiaki-ng).

use super::error::{ChiakiError, ChiakiResult};

/// `CHIAKI_AUDIO_HEADER_SIZE`.
pub const AUDIO_HEADER_SIZE: usize = 0xe;

/// Port von `ChiakiAudioHeader`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioHeader {
    pub channels: u8,
    pub bits: u8,
    pub rate: u32,
    pub frame_size: u32,
    pub unknown: u32,
}

impl Default for AudioHeader {
    fn default() -> Self {
        AudioHeader {
            channels: 0,
            bits: 0,
            rate: 0,
            frame_size: 0,
            unknown: 0,
        }
    }
}

impl AudioHeader {
    /// Port von `chiaki_audio_header_set()` (`unknown` wird wie im C auf 1 gesetzt).
    pub fn set(channels: u8, bits: u8, rate: u32, frame_size: u32) -> Self {
        AudioHeader {
            channels,
            bits,
            rate,
            frame_size,
            unknown: 1,
        }
    }

    /// Port von `chiaki_audio_header_load()`: liest `AUDIO_HEADER_SIZE` Bytes
    /// im Netzwerk-Byteorder-Layout.
    ///
    /// Abweichung zum C (das hier ungeprüft out-of-bounds liest): ein zu
    /// kurzer Buffer liefert `ChiakiError::BufTooSmall`.
    pub fn load(buf: &[u8]) -> ChiakiResult<Self> {
        if buf.len() < AUDIO_HEADER_SIZE {
            return Err(ChiakiError::BufTooSmall);
        }
        Ok(AudioHeader {
            channels: buf[0],
            bits: buf[1],
            rate: u32::from_be_bytes([buf[2], buf[3], buf[4], buf[5]]),
            frame_size: u32::from_be_bytes([buf[6], buf[7], buf[8], buf[9]]),
            unknown: u32::from_be_bytes([buf[0xa], buf[0xb], buf[0xc], buf[0xd]]),
        })
    }

    /// Port von `chiaki_audio_header_save()`.
    ///
    /// ACHTUNG (1:1 aus audio.c übernommen): `save` schreibt — anders als
    /// `load` — zuerst `bits` und dann `channels`; save/load sind im
    /// C-Original also keine Inversen voneinander. Dieses Verhalten wird
    /// bytekompatibel beibehalten.
    pub fn save(&self, buf: &mut [u8]) -> ChiakiResult<()> {
        if buf.len() < AUDIO_HEADER_SIZE {
            return Err(ChiakiError::BufTooSmall);
        }
        buf[0] = self.bits;
        buf[1] = self.channels;
        buf[2..6].copy_from_slice(&self.rate.to_be_bytes());
        buf[6..10].copy_from_slice(&self.frame_size.to_be_bytes());
        buf[0xa..0xe].copy_from_slice(&self.unknown.to_be_bytes());
        Ok(())
    }

    /// Port von `chiaki_audio_header_frame_buf_size()`:
    /// `frame_size * channels * sizeof(int16_t)`.
    pub fn frame_buf_size(&self) -> usize {
        self.frame_size as usize * self.channels as usize * std::mem::size_of::<i16>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_matches_c() {
        let h = AudioHeader::set(2, 16, 48000, 480);
        assert_eq!(h.channels, 2);
        assert_eq!(h.bits, 16);
        assert_eq!(h.rate, 48000);
        assert_eq!(h.frame_size, 480);
        assert_eq!(h.unknown, 1);
    }

    #[test]
    fn load_parses_network_layout() {
        // wie streamconnection.c: {0x02, 0x10, rate, frame_size, unknown} BE
        let mut buf = [0u8; AUDIO_HEADER_SIZE];
        buf[0] = 2; // channels
        buf[1] = 16; // bits
        buf[2..6].copy_from_slice(&48000u32.to_be_bytes());
        buf[6..10].copy_from_slice(&960u32.to_be_bytes());
        buf[0xa..0xe].copy_from_slice(&1u32.to_be_bytes());
        let h = AudioHeader::load(&buf).unwrap();
        assert_eq!(h.channels, 2);
        assert_eq!(h.bits, 16);
        assert_eq!(h.rate, 48000);
        assert_eq!(h.frame_size, 960);
        assert_eq!(h.unknown, 1);
    }

    #[test]
    fn save_writes_swapped_like_c() {
        // C-Quirk: save schreibt bits zuerst, dann channels (load erwartet
        // umgekehrt) — wird 1:1 nachgebaut.
        let h = AudioHeader::set(2, 16, 48000, 480);
        let mut buf = [0u8; AUDIO_HEADER_SIZE];
        h.save(&mut buf).unwrap();
        assert_eq!(buf[0], 16, "C: buf[0] = bits");
        assert_eq!(buf[1], 2, "C: buf[1] = channels");
        assert_eq!(&buf[2..6], &48000u32.to_be_bytes());
        assert_eq!(&buf[6..10], &480u32.to_be_bytes());
        assert_eq!(&buf[0xa..0xe], &1u32.to_be_bytes());
    }

    #[test]
    fn load_rejects_short_buffer() {
        let buf = [0u8; AUDIO_HEADER_SIZE - 1];
        assert_eq!(AudioHeader::load(&buf), Err(ChiakiError::BufTooSmall));
        let mut out = [0u8; AUDIO_HEADER_SIZE - 1];
        let h = AudioHeader::set(2, 16, 48000, 480);
        assert_eq!(h.save(&mut out), Err(ChiakiError::BufTooSmall));
    }

    #[test]
    fn frame_buf_size_matches_c_formula() {
        let h = AudioHeader::set(2, 16, 48000, 480);
        assert_eq!(h.frame_buf_size(), 480 * 2 * 2);
        let h = AudioHeader::set(1, 16, 48000, 480);
        assert_eq!(h.frame_buf_size(), 480 * 1 * 2);
        // PS5-Standard: 16 bit stereo, frame_size 480 -> 1920 Bytes (opus 480 Samples)
        let h = AudioHeader::set(2, 16, 48000, 810);
        assert_eq!(h.frame_buf_size(), 810 * 2 * 2);
    }

    #[test]
    fn header_size_constant() {
        assert_eq!(AUDIO_HEADER_SIZE, 0xe);
    }
}
