// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/base64.c + lib/include/chiaki/base64.h (chiaki-ng).
//
// Implementierung wie im C-Code von
// https://en.wikibooks.org/wiki/Algorithm_Implementation/Miscellaneous/Base64
// übernommen: Standard-Alphabet, '='-Padding. Abweichungen im Detail:
// - Encode liefert einen `String` statt (out-Buffer + BUF_TOO_SMALL).
// - Decode liefert einen `Vec<u8>`; der BUF_TOO_SMALL-Fall des C-Codes
//   (fester Ausgabepuffer) kann damit nicht auftreten.
// - Ungültige Eingabebytes → `ChiakiError::InvalidData` (wie C).
// - Decode bricht (wie C) am ersten '=' ab und ignoriert alles danach.

use super::error::ChiakiError;

/// Port von `chiaki_base64_encode()`.
///
/// Gleiche Semantik wie C: Standard-Alphabet `A-Za-z0-9+/`, '='-Padding auf ein
/// Vielfaches von 4 Zeichen.
pub fn encode(input: &[u8]) -> String {
    const BASE64CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let pad_count = input.len() % 3;

    // increment over the length of the string, three characters at a time
    let mut x = 0;
    while x < input.len() {
        // these three 8-bit characters become one 24-bit number
        let mut n = (input[x] as u32) << 16;
        if x + 1 < input.len() {
            n += (input[x + 1] as u32) << 8;
        }
        if x + 2 < input.len() {
            n += input[x + 2] as u32;
        }

        // this 24-bit number gets separated into four 6-bit numbers
        let n0 = ((n >> 18) & 63) as usize;
        let n1 = ((n >> 12) & 63) as usize;
        let n2 = ((n >> 6) & 63) as usize;
        let n3 = (n & 63) as usize;

        // if we have one byte available, then its encoding is spread
        // out over two characters
        out.push(BASE64CHARS[n0] as char);
        out.push(BASE64CHARS[n1] as char);

        // if we have only two bytes available, then their encoding is
        // spread out over three chars
        if x + 1 < input.len() {
            out.push(BASE64CHARS[n2] as char);
        }

        // if we have all three bytes available, then their encoding is spread
        // out over four characters
        if x + 2 < input.len() {
            out.push(BASE64CHARS[n3] as char);
        }

        x += 3;
    }

    // create and add padding that is required if we did not have a multiple of 3
    // number of characters available
    for _ in 0..(3 - pad_count) % 3 {
        out.push('=');
    }
    out
}

const WHITESPACE: u8 = 64;
const EQUALS: u8 = 65;
const INVALID: u8 = 66;

// Decode-Tabelle 1:1 aus lib/src/base64.c übernommen.
// Hinweis: In der chiaki-Variante ist NUR '\n' (10) Whitespace; Leerzeichen,
// '\t' und '\r' sind INVALID.
static D: [u8; 256] = [
    66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 64, 66, 66, 66, 66, 66, //   0..15  ('\n' = 64)
    66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, //  16..31
    66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 62, 66, 66, 66, 63, //  32..47  ('+' = 62, '/' = 63)
    52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 66, 66, 66, 65, 66, 66, //  48..63  ('0'-'9', '=' = 65)
    66, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, //  64..79  ('A'..)
    15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 66, 66, 66, 66, 66, //  80..95  (..'Z')
    66, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, //  96..111 ('a'..)
    41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 66, 66, 66, 66, 66, // 112..127 (..'z')
    66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, // 128..143
    66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, // 144..159
    66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, // 160..175
    66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, // 176..191
    66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, // 192..207
    66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, // 208..223
    66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, // 224..239
    66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, 66, // 240..255
];

/// Port von `chiaki_base64_decode()`.
///
/// Gleiche Semantik wie C: Whitespace ('\n') wird übersprungen, '=' beendet das
/// Parsen, jedes andere ungültige Byte liefert `ChiakiError::InvalidData`.
/// Anders als C gibt es keinen BUF_TOO_SMALL-Fall (dynamische Ausgabe).
pub fn decode(input: &[u8]) -> Result<Vec<u8>, ChiakiError> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut iter = 0usize;
    let mut buf = 0u32;

    for &b in input {
        let c = D[b as usize];
        match c {
            WHITESPACE => continue,      // skip whitespace
            INVALID => {                 // invalid input
                tracing::debug!("base64_decode: invalid input byte 0x{b:02x}");
                return Err(ChiakiError::InvalidData);
            }
            EQUALS => break,             // pad character, end of data
            _ => {
                buf = buf << 6 | c as u32;
                iter += 1;
                // If the buffer is full, split it into bytes
                if iter == 4 {
                    out.push(((buf >> 16) & 0xff) as u8);
                    out.push(((buf >> 8) & 0xff) as u8);
                    out.push((buf & 0xff) as u8);
                    buf = 0;
                    iter = 0;
                }
            }
        }
    }

    match iter {
        3 => {
            out.push(((buf >> 10) & 0xff) as u8);
            out.push(((buf >> 2) & 0xff) as u8);
        }
        2 => {
            out.push(((buf >> 4) & 0xff) as u8);
        }
        _ => {} // iter == 0 (vollständige Gruppen) oder iter == 1 (wie C: stiller Erfolg, 0 Bytes)
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Die Decodetabelle muss exakt der C-Tabelle entsprechen:
    /// Alphabet-Werte, '\n' als Whitespace, '=' als Pad, alles andere INVALID.
    #[test]
    fn decode_table_matches_c() {
        let mut expected = [INVALID; 256];
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        for (i, &c) in alphabet.iter().enumerate() {
            expected[c as usize] = i as u8;
        }
        expected[b'\n' as usize] = WHITESPACE;
        expected[b'=' as usize] = EQUALS;
        // explizit dokumentierte Abweichungen der chiaki-Variante:
        assert_eq!(D[b' ' as usize], INVALID);
        assert_eq!(D[b'\t' as usize], INVALID);
        assert_eq!(D[b'\r' as usize], INVALID);
        assert_eq!(D[b'\n' as usize], WHITESPACE);

        assert_eq!(D, expected, "D-Tabelle weicht von der C-Tabelle ab");
    }

    // RFC 4648 / bekannte Testvektoren
    #[test]
    fn encode_rfc4648_vectors() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg==");
        assert_eq!(encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn decode_rfc4648_vectors() {
        assert_eq!(decode(b""), Ok(Vec::<u8>::new()));
        assert_eq!(decode(b"Zg==").unwrap(), b"f");
        assert_eq!(decode(b"Zm8=").unwrap(), b"fo");
        assert_eq!(decode(b"Zm9v").unwrap(), b"foo");
        assert_eq!(decode(b"Zm9vYg==").unwrap(), b"foob");
        assert_eq!(decode(b"Zm9vYmE=").unwrap(), b"fooba");
        assert_eq!(decode(b"Zm9vYmFy").unwrap(), b"foobar");
    }

    #[test]
    fn roundtrip_all_lengths() {
        for len in 0..64usize {
            let data: Vec<u8> = (0..len as u8).map(|i| i.wrapping_mul(37).wrapping_add(11)).collect();
            let enc = encode(&data);
            assert_eq!(enc.len(), (len + 2) / 3 * 4);
            let dec = decode(enc.as_bytes()).unwrap();
            assert_eq!(dec, data, "roundtrip failed bei Länge {len}");
        }
    }

    #[test]
    fn decode_stops_at_first_equals() {
        // wie C: alles nach dem ersten '=' wird ignoriert
        assert_eq!(decode(b"Zm9v###").unwrap_err(), ChiakiError::InvalidData);
        assert_eq!(decode(b"Zm9v=Yg==").unwrap(), b"foo");
    }

    #[test]
    fn decode_skips_newlines() {
        assert_eq!(decode(b"Zm9v\nYmFy").unwrap(), b"foobar");
        assert_eq!(decode(b"\nZg==\n").unwrap(), b"f");
    }

    #[test]
    fn decode_invalid_input() {
        // ungültige Zeichen → InvalidData
        assert_eq!(decode(b"Zm9!").unwrap_err(), ChiakiError::InvalidData);
        assert_eq!(decode(b" ").unwrap_err(), ChiakiError::InvalidData); // Space ist in chiaki INVALID
        assert_eq!(decode(b"\t").unwrap_err(), ChiakiError::InvalidData);
        assert_eq!(decode(b"\r").unwrap_err(), ChiakiError::InvalidData);
        // Bytes >= 0x80 sind INVALID (in C werden sie über die Tabelle gemappt)
        assert_eq!(decode(&[b'A', b'A', b'A', 0xff]).unwrap_err(), ChiakiError::InvalidData);
    }

    #[test]
    fn decode_trailing_single_char_ignored_like_c() {
        // "A" lässt iter==1 — der C-Code liefert dann Success mit 0 Bytes
        assert_eq!(decode(b"A").unwrap(), Vec::<u8>::new());
        // iter==3-Zweig: "ZgA=" -> 2 Bytes inkl. des Null-Bytes (wie C)
        assert_eq!(decode(b"ZgA=").unwrap(), vec![0x66, 0x00]);
    }
}
