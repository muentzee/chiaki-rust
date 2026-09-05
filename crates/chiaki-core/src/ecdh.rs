// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
//
// Port of chiaki-ng lib/src/ecdh.c + lib/include/chiaki/ecdh.h.
//
// The C reference uses OpenSSL with NID_secp256k1; here the same curve is
// provided by the k256 crate:
// - chiaki_ecdh_init generates a random secp256k1 key pair.
// - chiaki_ecdh_set_local_key imports a persisted private key (32 bytes,
//   big-endian) plus its public key (SEC1 octet string); the public key is
//   validated against the private one (the C code trusts it blindly).
// - chiaki_ecdh_get_local_pub_key exports the public point uncompressed
//   (65 bytes: 0x04 || X || Y) and returns
//   HMAC-SHA256(handshake_key, pub_key) as signature (32 bytes).
// - chiaki_ecdh_derive_secret computes the raw ECDH shared secret, which is
//   the x coordinate of the shared point, left-aligned/big-endian in 32 bytes
//   (OpenSSL ECDH_compute_key with NULL KDF). Like the C implementation, the
//   handshake_key and remote_sig parameters are not used inside this function
//   (signature verification happens in the caller, takion.c).

use crate::error::{ChiakiError, ChiakiResult};
use crate::gkcrypt::HANDSHAKE_KEY_SIZE;
use hmac::{Hmac, Mac};
use k256::elliptic_curve::ecdh::diffie_hellman;
use k256::elliptic_curve::sec1::ToEncodedPoint;
use k256::{PublicKey, SecretKey};
use rand::rngs::OsRng;
use sha2::Sha256;

/// CHIAKI_ECDH_SECRET_SIZE
pub const ECDH_SECRET_SIZE: usize = 32;

/// Length of an uncompressed SEC1 point for secp256k1 (0x04 || X || Y).
pub const ECDH_PUB_KEY_SIZE: usize = 65;

type HmacSha256 = Hmac<Sha256>;

/// ChiakiECDH (chiaki_ecdh_t) on NID_secp256k1.
pub struct Ecdh {
    secret: SecretKey,
}

impl Ecdh {
    /// chiaki_ecdh_init: generate a fresh random key pair.
    pub fn new() -> ChiakiResult<Self> {
        let secret = SecretKey::random(&mut OsRng);
        Ok(Ecdh { secret })
    }

    /// chiaki_ecdh_set_local_key: import a private key (32 bytes big-endian)
    /// with its public key (SEC1 uncompressed/compressed octet string).
    ///
    /// Used to restore the key persisted by the regist session. The public
    /// key is checked against the private key (the C code would silently use
    /// a mismatching pair; a mismatch is a data error).
    pub fn set_local_key(&mut self, private_key: &[u8], public_key: &[u8]) -> ChiakiResult<()> {
        let secret = SecretKey::from_slice(private_key).map_err(|_| ChiakiError::InvalidData)?;
        let provided = PublicKey::from_sec1_bytes(public_key).map_err(|_| ChiakiError::InvalidData)?;
        if provided.as_affine() != secret.public_key().as_affine() {
            return Err(ChiakiError::InvalidData);
        }
        self.secret = secret;
        Ok(())
    }

    /// chiaki_ecdh_get_local_pub_key: returns `(public_key, signature)`.
    ///
    /// The public key is the uncompressed SEC1 encoding (65 bytes), the
    /// signature is HMAC-SHA256 keyed with `handshake_key` over the public
    /// key bytes.
    pub fn get_local_pub_key(
        &self,
        handshake_key: &[u8; HANDSHAKE_KEY_SIZE],
    ) -> ChiakiResult<([u8; ECDH_PUB_KEY_SIZE], [u8; ECDH_SECRET_SIZE])> {
        let public_key = self.secret.public_key();
        let encoded = public_key.as_affine().to_encoded_point(false);

        let mut key_out = [0u8; ECDH_PUB_KEY_SIZE];
        let bytes = encoded.as_bytes();
        if bytes.len() != ECDH_PUB_KEY_SIZE {
            return Err(ChiakiError::Unknown);
        }
        key_out.copy_from_slice(bytes);

        let mut mac = HmacSha256::new_from_slice(&handshake_key[..HANDSHAKE_KEY_SIZE])
            .map_err(|_| ChiakiError::Unknown)?;
        mac.update(&key_out);
        let sig_full = mac.finalize().into_bytes();

        let mut sig_out = [0u8; ECDH_SECRET_SIZE];
        sig_out.copy_from_slice(&sig_full);
        Ok((key_out, sig_out))
    }

    /// chiaki_ecdh_derive_secret: raw ECDH shared secret with the remote
    /// public key (SEC1 octet string). Result is the 32-byte big-endian
    /// x coordinate of the shared point.
    ///
    /// `handshake_key` and `remote_sig` are accepted for API parity with the
    /// C function but unused, exactly like in ecdh.c (the caller verifies the
    /// remote signature before trusting the key).
    pub fn derive_secret(
        &self,
        remote_key: &[u8],
        _handshake_key: &[u8],
        _remote_sig: &[u8],
    ) -> ChiakiResult<[u8; ECDH_SECRET_SIZE]> {
        let remote_public_key =
            PublicKey::from_sec1_bytes(remote_key).map_err(|_| ChiakiError::InvalidData)?;

        let shared = diffie_hellman(self.secret.to_nonzero_scalar(), remote_public_key.as_affine());
        let raw = shared.raw_secret_bytes();
        let mut secret_out = [0u8; ECDH_SECRET_SIZE];
        secret_out.copy_from_slice(raw);
        Ok(secret_out)
    }

    /// The local public key as uncompressed SEC1 point (helper, used by tests
    /// and session code that wants the raw point without the HMAC signature).
    pub fn local_pub_key_bytes(&self) -> [u8; ECDH_PUB_KEY_SIZE] {
        let encoded = self.secret.public_key().as_affine().to_encoded_point(false);
        let mut key_out = [0u8; ECDH_PUB_KEY_SIZE];
        key_out.copy_from_slice(encoded.as_bytes());
        key_out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hx(s: &str) -> Vec<u8> {
        hex::decode(s).expect("valid hex")
    }

    fn k32(s: &str) -> [u8; 32] {
        let v = hex::decode(s).expect("valid hex");
        let mut out = [0u8; 32];
        out.copy_from_slice(&v);
        out
    }

    // Golden vectors from chiaki-ng test/gkcrypt.c (test_ecdh).

    #[test]
    fn test_ecdh_golden() {
        let handshake_key = hx("fc5d4ba03a353abb6a7fac791b17bb34");
        assert_eq!(handshake_key.len(), HANDSHAKE_KEY_SIZE);
        let mut handshake_key16 = [0u8; HANDSHAKE_KEY_SIZE];
        handshake_key16.copy_from_slice(&handshake_key);

        let local_private_key = hex::decode(
            "16e75dcbda9855fb6befdd8aa5f16e7f46fde1d2279703601872d84b1538d900",
        )
        .unwrap();
        let local_public_key = hex::decode(
            "04f40af135a4889436cee52b5c73a33ec5ad0be0952f57f4f0ed0c80b0beda7c\
             a643789393a5947e9faa3f6795c9aa09a96325dfe850bfc3f1db62a50abfb0ff\
             f7",
        )
        .unwrap();
        let local_public_key_sig = hex::decode(
            "99b5cbb537180bfc55da437f4476a817c937fe561b8abe0c4112ab71f5a68d29",
        )
        .unwrap();
        let remote_public_key = hex::decode(
            "04dfef08bba856f2b44b8a0e4f44203f8e493feed43ce93afe5c64677720157c\
             5910156794ae5f024aad0ccefa14150aabee080b141276ea3ec0d565f46877a3\
             ca",
        )
        .unwrap();
        let remote_public_key_sig = hex::decode(
            "13c589e23b728524a99f968003a181305968f1bbb64dc4a76ccef6794ceb2d98",
        )
        .unwrap();
        let secret_expected = hex::decode(
            "b81c6146e749738c9630ca13ff71e59b3bf94198d467a5a2bc7804928143ec1d",
        )
        .unwrap();

        let mut ecdh = Ecdh::new().unwrap();

        ecdh.set_local_key(&local_private_key, &local_public_key).unwrap();

        let (local_public_key_result, local_public_key_sig_result) =
            ecdh.get_local_pub_key(&handshake_key16).unwrap();

        assert_eq!(local_public_key_result.len(), local_public_key.len());
        assert_eq!(&local_public_key_result[..], &local_public_key[..]);

        assert_eq!(
            &local_public_key_sig_result[..],
            &local_public_key_sig[..]
        );

        let secret_result = ecdh
            .derive_secret(
                &remote_public_key,
                &handshake_key16,
                &remote_public_key_sig,
            )
            .unwrap();

        assert_eq!(ECDH_SECRET_SIZE, secret_expected.len());
        assert_eq!(&secret_result[..], &secret_expected[..]);
    }

    // Roundtrip between two freshly generated instances: both derive the same
    // shared secret regardless of direction.
    #[test]
    fn test_ecdh_roundtrip() {
        let mut handshake_key16 = [0u8; HANDSHAKE_KEY_SIZE];
        handshake_key16.copy_from_slice(&hex::decode("000102030405060708090a0b0c0d0e0f").unwrap());

        let a = Ecdh::new().unwrap();
        let b = Ecdh::new().unwrap();

        let (a_pub, a_sig) = a.get_local_pub_key(&handshake_key16).unwrap();
        let (b_pub, b_sig) = b.get_local_pub_key(&handshake_key16).unwrap();

        // format checks: uncompressed point, sig is HMAC over the point
        assert_eq!(a_pub[0], 0x04);
        assert_eq!(b_pub[0], 0x04);

        let mut mac = HmacSha256::new_from_slice(&handshake_key16).unwrap();
        mac.update(&b_pub);
        assert_eq!(&mac.finalize().into_bytes()[..], &b_sig[..]);

        // asymmetric derivation must agree in both directions
        let a_secret = a.derive_secret(&b_pub, &handshake_key16, &b_sig).unwrap();
        let b_secret = b.derive_secret(&a_pub, &handshake_key16, &a_sig).unwrap();
        assert_eq!(a_secret, b_secret);

        // and must not be the all-zero secret
        assert!(a_secret != [0u8; ECDH_SECRET_SIZE]);
    }

    // Deterministic key import: fixed private key must map to the matching
    // public key (regist session persistence path).
    #[test]
    fn test_ecdh_set_local_key_deterministic() {
        // Any fixed scalar in [1, q-1]; the matching public point is derived
        // by the implementation and checked against k256 itself first.
        let private_key = hex::decode(
            "16e75dcbda9855fb6befdd8aa5f16e7f46fde1d2279703601872d84b1538d900",
        )
        .unwrap();

        let secret = SecretKey::from_slice(&private_key).unwrap();
        let expected_pub: Vec<u8> = secret
            .public_key()
            .as_affine()
            .to_encoded_point(false)
            .as_bytes()
            .to_vec();
        assert_eq!(expected_pub.len(), 65);

        // 1) import via set_local_key, export again
        let mut ecdh = Ecdh::new().unwrap();
        ecdh.set_local_key(&private_key, &expected_pub).unwrap();
        assert_eq!(&ecdh.local_pub_key_bytes()[..], &expected_pub[..]);

        // 2) wrong public key must be rejected
        let other = Ecdh::new().unwrap();
        let other_pub = other.local_pub_key_bytes();
        assert!(ecdh.set_local_key(&private_key, &other_pub).is_err());

        // 3) invalid formats must be rejected
        assert!(ecdh.set_local_key(&[0u8; 31], &expected_pub).is_err());
        assert!(ecdh.set_local_key(&[0u8; 32], &expected_pub).is_err()); // zero scalar
        assert!(ecdh.set_local_key(&private_key, &expected_pub[..64]).is_err());
        assert!(ecdh.set_local_key(&private_key, &[0x02u8; 65]).is_err());

        // 4) import via public key compressed form must also work
        // compressed encoding: 0x02/0x03 || X (parity in the prefix)
        let mut compressed = vec![if expected_pub[64] & 1 == 0 { 0x02 } else { 0x03 }];
        compressed.extend_from_slice(&expected_pub[1..33]);
        ecdh.set_local_key(&private_key, &compressed).unwrap();
        assert_eq!(&ecdh.local_pub_key_bytes()[..], &expected_pub[..]);
    }

    // Freshly generated keys must be valid and unique across instances.
    #[test]
    fn test_ecdh_new_random() {
        let a = Ecdh::new().unwrap();
        let b = Ecdh::new().unwrap();
        assert_ne!(a.local_pub_key_bytes(), b.local_pub_key_bytes());
        assert_eq!(a.local_pub_key_bytes()[0], 0x04);
    }
}
