//! PKCE (RFC 7636), S256 only.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// A code_challenge must be base64url(SHA-256(...)): exactly 43 characters.
pub fn challenge_is_well_formed(challenge: &str) -> bool {
    challenge.len() == 43
        && challenge
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Does `verifier` hash to `challenge`? The verifier must be 43..=128 unreserved characters.
pub fn verify(verifier: &str, challenge: &str) -> bool {
    let well_formed = (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'));
    if !well_formed {
        return false;
    }
    let computed = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    computed.as_bytes().ct_eq(challenge.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc7636_appendix_b_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert!(challenge_is_well_formed(challenge));
        assert!(verify(verifier, challenge));
        assert!(!verify(
            "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXK",
            challenge
        ));
        assert!(!verify("short", challenge));
        assert!(!challenge_is_well_formed("plain-text-challenge"));
    }
}
