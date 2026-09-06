// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
//
// Port of chiaki-ng lib/src/rpcrypt.c + lib/include/chiaki/rpcrypt.h.
//
// Semantics (1:1 from C):
// - The RPCrypt derives two 16-byte keys ("bright" = AES key, "ambassador" = HMAC input)
//   either for auth (from nonce/morning via the big sigil tables) or for regist.
// - The IV for a message is HMAC-SHA256 over ambassador || counter (big-endian),
//   keyed with a per-target HMAC key, truncated to 16 bytes.
// - Encryption/decryption is AES-128-CFB128 (encrypt-direction block cipher for both)
//   with the per-message IV, exactly chiaki_rpcrypt_crypt from the C reference.

mod rpcrypt_tables;

use crate::error::{ChiakiError, ChiakiResult, Target};
use aes::cipher::{BlockEncrypt, KeyInit};
use aes::Aes128;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use rpcrypt_tables::{
    ECHO_A, ECHO_B, HMAC_KEY_PS4, HMAC_KEY_PS4_PRE10, HMAC_KEY_PS5, KEYS_A_PS4, KEYS_A_PS5,
    KEYS_B_PS4, KEYS_B_PS5, PS4_KEYS_0, PS4_KEYS_1, PS5_KEYS_0, PS5_KEYS_1, REGIST_AES_KEY,
};

/// CHIAKI_RPCRYPT_KEY_SIZE
pub const RPCRYPT_KEY_SIZE: usize = 0x10;

const HMAC_KEY_SIZE: usize = 0x10;

type HmacSha256 = Hmac<Sha256>;

/// Returns `target < CHIAKI_TARGET_PS4_10` (C enum ordering).
fn is_pre_10(target: Target) -> bool {
    target < Target::Ps4_10
}

/// hmac key selection (rpcrypt_hmac_key).
fn rpcrypt_hmac_key(target: Target) -> &'static [u8; HMAC_KEY_SIZE] {
    match target {
        Target::Ps5_1 => &HMAC_KEY_PS5,
        Target::Ps4_8 | Target::Ps4_9 => &HMAC_KEY_PS4_PRE10,
        _ => &HMAC_KEY_PS4,
    }
}

/// chiaki_rpcrypt_bright_ambassador_ps4_pre10 (targets PS4 8/9).
fn bright_ambassador_ps4_pre10(
    nonce: &[u8; RPCRYPT_KEY_SIZE],
    morning: &[u8; RPCRYPT_KEY_SIZE],
) -> ([u8; RPCRYPT_KEY_SIZE], [u8; RPCRYPT_KEY_SIZE]) {
    let mut ambassador = [0u8; RPCRYPT_KEY_SIZE];
    for i in 0..RPCRYPT_KEY_SIZE {
        let mut v = nonce[i];
        v = v.wrapping_sub(i as u8);
        v = v.wrapping_sub(0x27);
        v ^= ECHO_A[i];
        ambassador[i] = v;
    }

    let mut bright = [0u8; RPCRYPT_KEY_SIZE];
    for i in 0..RPCRYPT_KEY_SIZE {
        let mut v = morning[i];
        v = v.wrapping_sub(i as u8);
        v = v.wrapping_add(0x34);
        v ^= ECHO_B[i];
        v ^= nonce[i];
        bright[i] = v;
    }
    (bright, ambassador)
}

/// Static bright_ambassador() for targets >= PS4 10 (table lookups).
fn bright_ambassador_table(
    target: Target,
    nonce: &[u8; RPCRYPT_KEY_SIZE],
    morning: &[u8; RPCRYPT_KEY_SIZE],
) -> ChiakiResult<([u8; RPCRYPT_KEY_SIZE], [u8; RPCRYPT_KEY_SIZE])> {
    if is_pre_10(target) {
        return Err(ChiakiError::InvalidData);
    }

    let keys_a: &[u8] = if target.is_ps5() { &KEYS_A_PS5 } else { &KEYS_A_PS4 };
    let keys_b: &[u8] = if target.is_ps5() { &KEYS_B_PS5 } else { &KEYS_B_PS4 };

    let mut ambassador = [0u8; RPCRYPT_KEY_SIZE];
    let key = &keys_a[((nonce[0] >> 3) as usize) * 0x70..][..RPCRYPT_KEY_SIZE];
    for i in 0..RPCRYPT_KEY_SIZE {
        let mut v = nonce[i];
        if target.is_ps5() {
            v = v.wrapping_sub(0x2d);
            v = v.wrapping_sub(i as u8);
        } else {
            v = v.wrapping_add(0x36);
            v = v.wrapping_add(i as u8);
        }
        v ^= key[i];
        ambassador[i] = v;
    }

    let mut bright = [0u8; RPCRYPT_KEY_SIZE];
    let key = &keys_b[((nonce[7] >> 3) as usize) * 0x70..][..RPCRYPT_KEY_SIZE];
    if target.is_ps5() {
        for i in 0..RPCRYPT_KEY_SIZE {
            let mut v = morning[i];
            v = v.wrapping_add(0x18);
            v = v.wrapping_add(i as u8);
            v ^= nonce[i];
            v ^= key[i];
            bright[i] = v;
        }
    } else {
        for i in 0..RPCRYPT_KEY_SIZE {
            let mut v = key[i] ^ morning[i];
            v = v.wrapping_add(0x21);
            v = v.wrapping_add(i as u8);
            v ^= nonce[i];
            bright[i] = v;
        }
    }
    Ok((bright, ambassador))
}

/// chiaki_rpcrypt_bright_ambassador
///
/// Returns `(bright, ambassador)`. `Err` for targets below PS4 10 other than
/// PS4 8/9 (the C code asserts in that case).
pub fn bright_ambassador(
    target: Target,
    nonce: &[u8; RPCRYPT_KEY_SIZE],
    morning: &[u8; RPCRYPT_KEY_SIZE],
) -> ChiakiResult<([u8; RPCRYPT_KEY_SIZE], [u8; RPCRYPT_KEY_SIZE])> {
    match target {
        Target::Ps4_8 | Target::Ps4_9 => Ok(bright_ambassador_ps4_pre10(nonce, morning)),
        _ => bright_ambassador_table(target, nonce, morning),
    }
}

/// chiaki_rpcrypt_aeropause_ps4_pre10
pub fn aeropause_ps4_pre10(ambassador: &[u8; RPCRYPT_KEY_SIZE]) -> [u8; RPCRYPT_KEY_SIZE] {
    let mut aeropause = [0u8; RPCRYPT_KEY_SIZE];
    for i in 0..RPCRYPT_KEY_SIZE {
        let mut v = ambassador[i];
        v = v.wrapping_sub(i as u8);
        v = v.wrapping_sub(0x29);
        v ^= ECHO_B[i];
        aeropause[i] = v;
    }
    aeropause
}

/// chiaki_rpcrypt_aeropause
///
/// `key_1_off` must be < 0x20 (the C code would read out of bounds otherwise;
/// here it is rejected with `InvalidData`).
pub fn aeropause(
    target: Target,
    key_1_off: usize,
    ambassador: &[u8; RPCRYPT_KEY_SIZE],
) -> ChiakiResult<[u8; RPCRYPT_KEY_SIZE]> {
    if is_pre_10(target) {
        return Err(ChiakiError::InvalidData);
    }
    if key_1_off >= 0x20 {
        return Err(ChiakiError::InvalidData);
    }
    let keys_1: &[u8] = if target.is_ps5() { &PS5_KEYS_1 } else { &PS4_KEYS_1 };
    // uint8_t wurzelbert = is_ps5 ? -0x2d : 0x29
    let wurzelbert: u8 = if target.is_ps5() { 0xd3 } else { 0x29 };

    let mut aeropause = [0u8; RPCRYPT_KEY_SIZE];
    for i in 0..RPCRYPT_KEY_SIZE {
        let k = keys_1[i * 0x20 + key_1_off];
        aeropause[i] = (ambassador[i] ^ k)
            .wrapping_add(wurzelbert)
            .wrapping_add(i as u8);
    }
    Ok(aeropause)
}

/// chiaki_rpcrypt_aeropause_psn
pub fn aeropause_psn(
    target: Target,
    key_1_off: usize,
    ambassador: &[u8; RPCRYPT_KEY_SIZE],
) -> ChiakiResult<[u8; RPCRYPT_KEY_SIZE]> {
    if is_pre_10(target) {
        return Err(ChiakiError::InvalidData);
    }
    if key_1_off >= 0x20 {
        return Err(ChiakiError::InvalidData);
    }
    let keys_1: &[u8] = if target.is_ps5() { &PS5_KEYS_1 } else { &PS4_KEYS_1 };
    // uint8_t wurzelbert = is_ps5 ? 0x2B : -0x29
    let wurzelbert: u8 = if target.is_ps5() { 0x2b } else { 0xd7 };

    let mut aeropause = [0u8; RPCRYPT_KEY_SIZE];
    for i in 0..RPCRYPT_KEY_SIZE {
        let k = keys_1[i * 0x20 + key_1_off];
        aeropause[i] = ambassador[i]
            .wrapping_sub(i as u8)
            .wrapping_add(wurzelbert)
            ^ k;
    }
    Ok(aeropause)
}

/// chiaki_rpcrypt_ambassador_from_aeropause
pub fn ambassador_from_aeropause(
    target: Target,
    key_1_off: usize,
    aeropause: &[u8; RPCRYPT_KEY_SIZE],
) -> ChiakiResult<[u8; RPCRYPT_KEY_SIZE]> {
    if is_pre_10(target) {
        return Err(ChiakiError::InvalidData);
    }
    if key_1_off >= 0x20 {
        return Err(ChiakiError::InvalidData);
    }
    let keys_1: &[u8] = if target.is_ps5() { &PS5_KEYS_1 } else { &PS4_KEYS_1 };
    // uint8_t wurzelbert = is_ps5 ? 0x2B : -0x29
    let wurzelbert: u8 = if target.is_ps5() { 0x2b } else { 0xd7 };

    let mut ambassador = [0u8; RPCRYPT_KEY_SIZE];
    for i in 0..RPCRYPT_KEY_SIZE {
        let k = keys_1[i * 0x20 + key_1_off];
        ambassador[i] = (aeropause[i] ^ k)
            .wrapping_add(i as u8)
            .wrapping_sub(wurzelbert);
    }
    Ok(ambassador)
}

/// ChiakiRPCrypt (chiaki_rpcrypt_t).
///
/// Clone: das C teilt denselben `chiaki_rpcrypt_t`-Zeiger zwischen Session,
/// Ctrl und StreamConnection; in Rust klonen sich alle Beteiligten denselben
/// Zustand (nur unveränderliche Keys + Target).
#[derive(Clone)]
pub struct Rpcrypt {
    pub target: Target,
    /// AES-128 key ("bright").
    pub bright: [u8; RPCRYPT_KEY_SIZE],
    /// HMAC input for IV generation ("ambassador").
    pub ambassador: [u8; RPCRYPT_KEY_SIZE],
}

impl Rpcrypt {
    /// chiaki_rpcrypt_init_auth
    pub fn new_auth(
        target: Target,
        nonce: &[u8; RPCRYPT_KEY_SIZE],
        morning: &[u8; RPCRYPT_KEY_SIZE],
    ) -> ChiakiResult<Self> {
        let (bright, ambassador) = bright_ambassador(target, nonce, morning)?;
        Ok(Rpcrypt {
            target,
            bright,
            ambassador,
        })
    }

    /// chiaki_rpcrypt_init_regist_ps4_pre10
    pub fn new_regist_ps4_pre10(ambassador: &[u8; RPCRYPT_KEY_SIZE], pin: u32) -> Self {
        // representative target, might not be the actual version
        let mut bright = REGIST_AES_KEY;
        bright[0] ^= ((pin >> 0x18) & 0xff) as u8;
        bright[1] ^= ((pin >> 0x10) & 0xff) as u8;
        bright[2] ^= ((pin >> 0x08) & 0xff) as u8;
        bright[3] ^= ((pin >> 0x00) & 0xff) as u8;
        Rpcrypt {
            target: Target::Ps4_9,
            bright,
            ambassador: *ambassador,
        }
    }

    /// chiaki_rpcrypt_init_regist
    pub fn new_regist(
        target: Target,
        ambassador: &[u8; RPCRYPT_KEY_SIZE],
        key_0_off: usize,
        pin: u32,
    ) -> ChiakiResult<Self> {
        if is_pre_10(target) {
            return Err(ChiakiError::InvalidData);
        }
        let keys_0: &[u8] = if target.is_ps5() { &PS5_KEYS_0 } else { &PS4_KEYS_0 };
        if key_0_off >= 0x20 {
            return Err(ChiakiError::InvalidData);
        }

        let mut bright = [0u8; RPCRYPT_KEY_SIZE];
        for i in 0..RPCRYPT_KEY_SIZE {
            bright[i] = keys_0[i * 0x20 + key_0_off];
        }
        bright[0xc] ^= ((pin >> 0x18) & 0xff) as u8;
        bright[0xd] ^= ((pin >> 0x10) & 0xff) as u8;
        bright[0xe] ^= ((pin >> 0x08) & 0xff) as u8;
        bright[0xf] ^= ((pin >> 0x00) & 0xff) as u8;

        Ok(Rpcrypt {
            target,
            bright,
            ambassador: *ambassador,
        })
    }

    /// chiaki_rpcrypt_init_regist_psn
    ///
    /// Encrypts `custom_data1` using `data1` as key and `data2` to build the
    /// HMAC input, then mixes the result into the bright key.
    pub fn new_regist_psn(
        target: Target,
        ambassador: &[u8; RPCRYPT_KEY_SIZE],
        key_0_off: usize,
        custom_data1: &[u8; RPCRYPT_KEY_SIZE],
        data1: &[u8; RPCRYPT_KEY_SIZE],
        data2: &[u8; RPCRYPT_KEY_SIZE],
    ) -> ChiakiResult<Self> {
        let custom_data_crypt = Rpcrypt {
            target,
            ambassador: *data2,
            bright: *data1,
        };
        let encrypted_custom_data1 = custom_data_crypt.encrypt_buf(0, custom_data1)?;

        if is_pre_10(target) {
            return Err(ChiakiError::InvalidData);
        }
        let keys_0: &[u8] = if target.is_ps5() { &PS5_KEYS_0 } else { &PS4_KEYS_0 };
        if key_0_off >= 0x20 {
            return Err(ChiakiError::InvalidData);
        }

        let mut bright = [0u8; RPCRYPT_KEY_SIZE];
        for i in 0..RPCRYPT_KEY_SIZE {
            bright[i] = keys_0[i * 0x20 + key_0_off] ^ encrypted_custom_data1[i];
        }

        Ok(Rpcrypt {
            target,
            bright,
            ambassador: *ambassador,
        })
    }

    /// chiaki_rpcrypt_generate_iv: HMAC-SHA256(ambassador || counter_be64)[:0x10]
    pub fn generate_iv(&self, counter: u64) -> ChiakiResult<[u8; RPCRYPT_KEY_SIZE]> {
        let hmac_key = rpcrypt_hmac_key(self.target);

        let mut buf = [0u8; RPCRYPT_KEY_SIZE + 8];
        buf[..RPCRYPT_KEY_SIZE].copy_from_slice(&self.ambassador);
        buf[RPCRYPT_KEY_SIZE..].copy_from_slice(&counter.to_be_bytes());

        let mut mac =
            <HmacSha256 as Mac>::new_from_slice(hmac_key).map_err(|_| ChiakiError::Unknown)?;
        mac.update(&buf);
        let out = mac.finalize().into_bytes();
        let mut iv = [0u8; RPCRYPT_KEY_SIZE];
        iv.copy_from_slice(&out[..RPCRYPT_KEY_SIZE]);
        Ok(iv)
    }

    /// chiaki_rpcrypt_crypt (AES-128-CFB128, in-place like the common C usage).
    pub fn crypt_in_place(&self, counter: u64, data: &mut [u8], encrypt: bool) -> ChiakiResult<()> {
        let iv = self.generate_iv(counter)?;
        let cipher = Aes128::new_from_slice(&self.bright).map_err(|_| ChiakiError::Unknown)?;
        cfb128_crypt(&cipher, iv, data, encrypt);
        Ok(())
    }

    /// chiaki_rpcrypt_encrypt (in-place)
    pub fn encrypt(&self, counter: u64, data: &mut [u8]) -> ChiakiResult<()> {
        self.crypt_in_place(counter, data, true)
    }

    /// chiaki_rpcrypt_decrypt (in-place)
    pub fn decrypt(&self, counter: u64, data: &mut [u8]) -> ChiakiResult<()> {
        self.crypt_in_place(counter, data, false)
    }

    /// chiaki_rpcrypt_encrypt into a separate output buffer.
    pub fn encrypt_buf(&self, counter: u64, input: &[u8]) -> ChiakiResult<Vec<u8>> {
        let mut out = input.to_vec();
        self.crypt_in_place(counter, &mut out, true)?;
        Ok(out)
    }

    /// chiaki_rpcrypt_decrypt into a separate output buffer.
    pub fn decrypt_buf(&self, counter: u64, input: &[u8]) -> ChiakiResult<Vec<u8>> {
        let mut out = input.to_vec();
        self.crypt_in_place(counter, &mut out, false)?;
        Ok(out)
    }
}

/// AES-128-CFB128 over arbitrary-length data (equivalent to
/// mbedtls_aes_crypt_cfb128 with iv_off=0 / EVP_aes_128_cfb128 one-shot).
/// The block cipher always runs in encrypt direction; on encrypt the IV is
/// advanced with the ciphertext output, on decrypt with the ciphertext input.
fn cfb128_crypt(cipher: &Aes128, mut iv: [u8; RPCRYPT_KEY_SIZE], data: &mut [u8], encrypt: bool) {
    let mut pos = 0;
    while pos < data.len() {
        let mut keystream = aes::Block::from(iv);
        cipher.encrypt_block(&mut keystream);
        let n = (data.len() - pos).min(RPCRYPT_KEY_SIZE);
        for j in 0..n {
            let input = data[pos + j];
            let output = input ^ keystream[j];
            data[pos + j] = output;
            iv[j] = if encrypt { output } else { input };
        }
        pos += n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hx(s: &str) -> Vec<u8> {
        hex::decode(s).expect("valid hex")
    }

    fn k16(s: &str) -> [u8; 16] {
        let v = hx(s);
        assert_eq!(v.len(), 16);
        let mut out = [0u8; 16];
        out.copy_from_slice(&v);
        out
    }

    // Golden vectors from chiaki-ng test/rpcrypt.c.

    #[test]
    fn test_bright_ambassador_ps4_pre10() {
        let nonce = k16("430967ae364b1c452662377abf3fe939");
        let morning = k16("d2789f5185a799a24452779c2b83cf07");
        let bright_expected = k16("a44e2a165e20d30faa118bc77ca7dc11");
        let ambassador_expected = k16("1da8b91f6e26642ebc088b004f015b52");

        let (bright, ambassador) = bright_ambassador(Target::Ps4_9, &nonce, &morning).unwrap();
        assert_eq!(bright, bright_expected);
        assert_eq!(ambassador, ambassador_expected);
    }

    #[test]
    fn test_bright_ambassador() {
        let nonce_local = k16("ae92e764882651ef89018cfa696c6938");
        let morning_local = k16("74a59c9693c2083ba6a84ba050fa8e5a");
        let ambassador_expected = k16("92bee219b9d5b1abc6494577a421e9bd");
        let bright_expected = k16("67408c5e65665ad291a832ebe2d90abb");

        let (bright, ambassador) =
            bright_ambassador(Target::Ps4_10, &nonce_local, &morning_local).unwrap();
        assert_eq!(ambassador, ambassador_expected);
        assert_eq!(bright, bright_expected);
    }

    #[test]
    fn test_iv_ps4_pre10() {
        let nonce = k16("430967ae364b1c452662377abf3fe939");
        let morning = k16("d2789f5185a799a24452779c2b83cf07");
        let iv_a_expected = k16("0629be04e9911c48b45c026db7b78846");
        let iv_b_expected = k16("3fd0830ac730fc56752dbeb82c68a704");

        let rpcrypt = Rpcrypt::new_auth(Target::Ps4_9, &nonce, &morning).unwrap();

        let iv = rpcrypt.generate_iv(0).unwrap();
        assert_eq!(iv, iv_a_expected);

        let iv = rpcrypt.generate_iv(0).unwrap();
        assert_eq!(iv, iv_a_expected);

        let iv = rpcrypt.generate_iv(0x0102030405060708).unwrap();
        assert_eq!(iv, iv_b_expected);

        let iv = rpcrypt.generate_iv(0x0102030405060708).unwrap();
        assert_eq!(iv, iv_b_expected);
    }

    #[test]
    fn test_iv_regist_ps4() {
        let ambassador = k16("3e7e7a825973adab2f694346bd44dab5");
        let iv_expected = k16("ac489977f92ac55bb9093c33b6113c46");

        let rpcrypt = Rpcrypt::new_regist(Target::Ps4_10, &ambassador, 0, 0).unwrap();
        let iv = rpcrypt.generate_iv(0).unwrap();
        assert_eq!(iv, iv_expected);
    }

    #[test]
    fn test_iv_regist_ps5() {
        let ambassador = k16("3e7e7a825973adab2f694346bd44dab5");
        let iv_expected = k16("9044408273f8044dca767b5a16394d64");

        let rpcrypt = Rpcrypt::new_regist(Target::Ps5_1, &ambassador, 0, 0).unwrap();
        let iv = rpcrypt.generate_iv(0).unwrap();
        assert_eq!(iv, iv_expected);
    }

    #[test]
    fn test_bright_regist_ps4() {
        let ambassador = k16("dca1c84dfe50d65722da09654231e7c2");
        let bright_expected = k16("edfc1dc5a2fe2d7f09198575336c1316");
        let key_0_off = 0x1e;

        let rpcrypt = Rpcrypt::new_regist(Target::Ps4_10, &ambassador, key_0_off, 78703893).unwrap();
        assert_eq!(rpcrypt.bright, bright_expected);
    }

    #[test]
    fn test_bright_regist_ps5() {
        let ambassador = k16("dca1c84dfe50d65722da09654231e7c2");
        let bright_expected = k16("e29d644c141b9d617431a56d34cfc17f");
        let key_0_off = 0x1e;

        let rpcrypt = Rpcrypt::new_regist(Target::Ps5_1, &ambassador, key_0_off, 78703893).unwrap();
        assert_eq!(rpcrypt.bright, bright_expected);
    }

    #[test]
    fn test_encrypt_ps4_pre10() {
        let nonce = k16("430967ae364b1c452662377abf3fe939");
        let morning = k16("d2789f5185a799a24452779c2b83cf07");
        let rpcrypt = Rpcrypt::new_auth(Target::Ps4_9, &nonce, &morning).unwrap();

        // less than block size
        let mut buf_a = hx("1337c0ffee");
        let cipher_expected_a = hx("404863ebb4");
        rpcrypt.encrypt(0x0102030405060708, &mut buf_a).unwrap();
        assert_eq!(buf_a, cipher_expected_a);

        // 1x block size
        let mut buf_b = hx("dfa8a3d26ad7f34c4f874ceb8c57fb3f");
        let cipher_expected_b = hx("8cd700c630253a181520eb26f7edab15");
        rpcrypt.encrypt(0x0102030405060708, &mut buf_b).unwrap();
        assert_eq!(buf_b, cipher_expected_b);

        // more than block size, but not dividable by block size
        let mut buf_c = hx("080d80c7b22b9ff3e2d7c8a5b092d5008de6d774");
        let cipher_expected_c = hx("5b7223d3e8d956a7b8706f68cb28852a06a5d2e0");
        rpcrypt.encrypt(0x0102030405060708, &mut buf_c).unwrap();
        assert_eq!(buf_c, cipher_expected_c);

        // 2x block size
        let mut buf_d = hx("a106911b4974e07303e84742084d834ef326147bde2df67d47968c3b66957e5d");
        let cipher_expected_d = hx("f279320f13862927594fe08f73f7d3649d807e90d1f5d818e7be1629fb482bf8");
        rpcrypt.encrypt(0x0102030405060708, &mut buf_d).unwrap();
        assert_eq!(buf_d, cipher_expected_d);
    }

    #[test]
    fn test_decrypt_ps4_pre10() {
        let nonce = k16("430967ae364b1c452662377abf3fe939");
        let morning = k16("d2789f5185a799a24452779c2b83cf07");
        let rpcrypt = Rpcrypt::new_auth(Target::Ps4_9, &nonce, &morning).unwrap();

        // less than block size
        let mut buf_a = hx("8dd21dfb");
        let expected_a = hx("deadbeef");
        rpcrypt.decrypt(0x0102030405060708, &mut buf_a).unwrap();
        assert_eq!(buf_a, expected_a);

        // 1x block size
        let mut buf_b = hx("eb224eb773946a316edde58729dcd56b");
        let expected_b = hx("b85deda32966a365347a424a52668541");
        rpcrypt.decrypt(0x0102030405060708, &mut buf_b).unwrap();
        assert_eq!(buf_b, expected_b);

        // more than block size, but not dividable by block size
        let mut buf_c = hx("2bd81d86390e2dd7de2db5bcba5ee978eec93b18");
        let expected_c = hx("78a7be9263fce483848a1271c1e4b952f2bbcd39");
        rpcrypt.decrypt(0x0102030405060708, &mut buf_c).unwrap();
        assert_eq!(buf_c, expected_c);

        // 2x block size
        let mut buf_d = hx("2b80daa05e8d2e1de19516b1edd2c38ab8cc6c42578dc57df15c044297253a91");
        let expected_d = hx("78ff79b4047fe749bb32b17c966893a086a65638792aa803679bf2822d1a0829");
        rpcrypt.decrypt(0x0102030405060708, &mut buf_d).unwrap();
        assert_eq!(buf_d, expected_d);
    }

    // Additional deterministic self-test: the PS5-PSN regist path derives its
    // bright key from keys_0 ^ AES-CFB128(custom_data1); checked against
    // hardcoded bytes, plus encrypt/decrypt roundtrip.
    #[test]
    fn test_regist_psn_custom_data_and_roundtrip() {
        let target = Target::Ps5_1;
        let ambassador = k16("000102030405060708090a0b0c0d0e0f");
        let custom_data1 = k16("deadbeefdeadbeefdeadbeefdeadbeef");
        let data1 = k16("00112233445566778899aabbccddeeff");
        let data2 = k16("8899aabbccddeeff0011223344556677");

        let rpcrypt =
            Rpcrypt::new_regist_psn(target, &ambassador, 0x13, &custom_data1, &data1, &data2)
                .unwrap();
        let bright_expected = k16("4ee1b8cf3f2429bfd9d104dffe42f7a8");
        assert_eq!(rpcrypt.bright, bright_expected);

        // Roundtrip with a fixed counter must reproduce the plaintext.
        let plain = hx(
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f",
        );
        let mut data = plain.clone();
        rpcrypt.encrypt(0x1122334455667788, &mut data).unwrap();
        assert_ne!(data, plain);
        rpcrypt.decrypt(0x1122334455667788, &mut data).unwrap();
        assert_eq!(data, plain);

        // encrypt_buf equivalence with the in-place variant.
        let encrypted = rpcrypt.encrypt_buf(0x1122334455667788, &plain).unwrap();
        let mut inplace = plain.clone();
        rpcrypt.encrypt(0x1122334455667788, &mut inplace).unwrap();
        assert_eq!(encrypted, inplace);
    }

    #[test]
    fn test_aeropause_roundtrip() {
        let ambassador = k16("dca1c84dfe50d65722da09654231e7c2");
        let key_1_off = 0x7;

        // ambassador_from_aeropause is the inverse of aeropause_psn
        // (C formulas: psn aer = (amb - i + w) ^ k ; back = (aer ^ k) + i - w)
        let target = Target::Ps4_10;
        let aeropause_psn = super::aeropause_psn(target, key_1_off, &ambassador).unwrap();
        let back = ambassador_from_aeropause(target, key_1_off, &aeropause_psn).unwrap();
        assert_eq!(back, ambassador);
        let plain_aeropause = super::aeropause(target, key_1_off, &ambassador).unwrap();
        assert_ne!(plain_aeropause, aeropause_psn);

        // PS5 roundtrip
        let target = Target::Ps5_1;
        let aeropause_psn = super::aeropause_psn(target, key_1_off, &ambassador).unwrap();
        let back = ambassador_from_aeropause(target, key_1_off, &aeropause_psn).unwrap();
        assert_eq!(back, ambassador);
    }

    #[test]
    fn test_pre10_targets_rejected() {
        let ambassador = [0u8; 16];
        assert!(Rpcrypt::new_regist(Target::Ps4Unknown, &ambassador, 0, 0).is_err());
        assert!(Rpcrypt::new_regist(Target::Ps4_8, &ambassador, 0, 0).is_err());
        assert!(Rpcrypt::new_regist(Target::Ps4_9, &ambassador, 0, 0).is_err());
        assert!(super::aeropause(Target::Ps4_8, 0, &ambassador).is_err());
        assert!(super::aeropause_psn(Target::Ps4_9, 0, &ambassador).is_err());
        assert!(ambassador_from_aeropause(Target::Ps4_8, 0, &ambassador).is_err());
        // out-of-range key offsets
        assert!(Rpcrypt::new_regist(Target::Ps4_10, &ambassador, 0x20, 0).is_err());
        assert!(super::aeropause(Target::Ps4_10, 0x20, &ambassador).is_err());
    }
}
