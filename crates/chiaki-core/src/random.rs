// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/random.c + lib/include/chiaki/random.h (chiaki-ng).
//
// C nutzt OpenSSL RAND_bytes (bzw. mbedtls ctr_drbg). In Rust übernimmt das
// die `rand`-Crate (ThreadRNG, ChaCha12-basiert, OS-geseedet).
// Ein `chiaki_random_bytes_init` existiert im C-Code nicht (das Seeding von
// `srand` macht chiaki_lib_init) — auch hier ist kein Init nötig.

use rand::RngCore;

use super::error::ChiakiResult;

/// Port von `chiaki_random_bytes_crypt()` — kryptografisch passende Zufallsbytes.
///
/// Die `rand`-ThreadRNG-`fill_bytes` schlägt praktisch nie fehl; deshalb wird
/// hier (abweichend vom ChiakiErrorCode-Original) bei Bedarf
/// `ChiakiError::Unknown` gemeldet und ansonsten `Success` zurückgegeben.
pub fn random_bytes_crypt(buf: &mut [u8]) -> ChiakiResult<()> {
    rand::thread_rng().fill_bytes(buf);
    Ok(())
}

/// Wie `random_bytes_crypt`, aber ohne Result: Die ThreadRNG kann hier nicht
/// versagen (kein Fehlerpfad im C-Sinne). Für FFI-kompatible Aufrufer siehe
/// [`random_bytes_crypt`].
pub fn random_bytes(buf: &mut [u8]) {
    rand::thread_rng().fill_bytes(buf);
}

/// Port von `chiaki_random_32()`.
pub fn random_32() -> u32 {
    rand::thread_rng().next_u32()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_bytes_fills_buffer() {
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        random_bytes(&mut a);
        random_bytes_crypt(&mut b).unwrap();
        // nicht alle Null (praktisch ausgeschlossen: 2^-256)
        assert!(a.iter().any(|&x| x != 0));
        assert!(b.iter().any(|&x| x != 0));
        // zwei Aufrufe liefern unterschiedliche Bytes
        assert_ne!(a, b);
    }

    #[test]
    fn random_bytes_empty_ok() {
        let mut empty: [u8; 0] = [];
        random_bytes(&mut empty);
        assert!(random_bytes_crypt(&mut empty).is_ok());
    }

    #[test]
    fn random_32_varies() {
        let a = random_32();
        let b = random_32();
        let c = random_32();
        // 3 gleiche Werte hintereinander: p = 2^-64
        assert!(!(a == b && b == c));
    }
}
