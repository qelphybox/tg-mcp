//! Primitives for ADR-0003: XChaCha20-Poly1305 sealing and HKDF-SHA256 derivation.

use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroizing;

pub(crate) const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;

/// Bytes from the OS CSPRNG. Without one nothing here is safe, so failing is a panic.
pub(crate) fn random<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).expect("OS random number generator unavailable");
    buf
}

/// Encrypt and authenticate `plaintext`, bound to `aad`. Output: `nonce || ciphertext+tag`.
pub(crate) fn seal(key: &[u8; KEY_LEN], plaintext: &[u8], aad: &[u8]) -> Vec<u8> {
    let cipher = XChaCha20Poly1305::new_from_slice(key).expect("key is 32 bytes");
    let nonce_bytes = random::<NONCE_LEN>();
    let nonce = XNonce::try_from(&nonce_bytes[..]).expect("nonce is 24 bytes");
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .expect("XChaCha20-Poly1305 encryption is infallible for in-memory buffers");
    let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    out
}

/// Reverse of [`seal`]. `None` on a wrong key, wrong `aad` or any tampering.
pub(crate) fn open(key: &[u8; KEY_LEN], sealed: &[u8], aad: &[u8]) -> Option<Zeroizing<Vec<u8>>> {
    if sealed.len() < NONCE_LEN {
        return None;
    }
    let (nonce_bytes, ciphertext) = sealed.split_at(NONCE_LEN);
    let cipher = XChaCha20Poly1305::new_from_slice(key).expect("key is 32 bytes");
    let nonce = XNonce::try_from(nonce_bytes).ok()?;
    cipher
        .decrypt(
            &nonce,
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .ok()
        .map(Zeroizing::new)
}

/// HKDF-SHA256 of a high-entropy `secret` (a Token secret: 256 random bits, so no stretching).
pub(crate) fn derive(secret: &[u8], salt: &[u8], info: &[u8]) -> Zeroizing<[u8; KEY_LEN]> {
    let mut out = Zeroizing::new([0u8; KEY_LEN]);
    Hkdf::<Sha256>::new(Some(salt), secret)
        .expand(info, out.as_mut())
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_round_trip() {
        let key = random::<KEY_LEN>();
        let sealed = seal(&key, b"hello", b"aad");
        assert_eq!(
            open(&key, &sealed, b"aad").as_deref().map(Vec::as_slice),
            Some(&b"hello"[..])
        );
    }

    #[test]
    fn open_rejects_wrong_key_aad_and_tampering() {
        let key = random::<KEY_LEN>();
        let sealed = seal(&key, b"hello", b"aad");
        assert!(open(&random::<KEY_LEN>(), &sealed, b"aad").is_none());
        assert!(open(&key, &sealed, b"other").is_none());
        let mut tampered = sealed.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(open(&key, &tampered, b"aad").is_none());
        assert!(open(&key, &sealed[..10], b"aad").is_none());
    }

    #[test]
    fn derive_separates_by_info_and_salt() {
        let secret = random::<32>();
        let a = derive(&secret, b"salt", b"one");
        assert_ne!(*a, *derive(&secret, b"salt", b"two"));
        assert_ne!(*a, *derive(&secret, b"pepper", b"one"));
        assert_eq!(*a, *derive(&secret, b"salt", b"one"));
    }
}
