//! Token wire format: `tgm_<id>.<secret>`, both base64url without padding.
//!
//! The `tgm_` prefix makes leaked Tokens greppable by secret scanners. The id is a
//! lookup key and may be logged; the secret never leaves memory except to the client.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use zeroize::Zeroizing;

const PREFIX: &str = "tgm_";
pub(crate) const ID_LEN: usize = 16;
pub(crate) const SECRET_LEN: usize = 32;

pub(crate) struct ParsedToken {
    /// base64url of the 16 id bytes; the primary key in `tokens`.
    pub id: String,
    pub secret: Zeroizing<[u8; SECRET_LEN]>,
}

pub(crate) fn format(id: &str, secret: &[u8; SECRET_LEN]) -> Zeroizing<String> {
    let secret = Zeroizing::new(URL_SAFE_NO_PAD.encode(secret));
    Zeroizing::new(format!("{PREFIX}{id}.{}", secret.as_str()))
}

pub(crate) fn new_id() -> String {
    URL_SAFE_NO_PAD.encode(super::crypto::random::<ID_LEN>())
}

/// `None` for anything that isn't exactly a well-formed Token.
pub(crate) fn parse(token: &str) -> Option<ParsedToken> {
    let (id, secret) = token.strip_prefix(PREFIX)?.split_once('.')?;
    let id_bytes = URL_SAFE_NO_PAD.decode(id).ok()?;
    if id_bytes.len() != ID_LEN {
        return None;
    }
    let decoded = Zeroizing::new(URL_SAFE_NO_PAD.decode(secret).ok()?);
    let secret: [u8; SECRET_LEN] = decoded.as_slice().try_into().ok()?;
    Some(ParsedToken {
        id: id.to_string(),
        secret: Zeroizing::new(secret),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let id = new_id();
        let secret = super::super::crypto::random::<SECRET_LEN>();
        let token = format(&id, &secret);
        assert!(token.starts_with("tgm_"));
        let parsed = parse(&token).unwrap();
        assert_eq!(parsed.id, id);
        assert_eq!(*parsed.secret, secret);
    }

    #[test]
    fn rejects_malformed() {
        let good = format(&new_id(), &[7; SECRET_LEN]);
        for bad in [
            "",
            "tgm_",
            "tgm_.",
            &good[4..],                         // no prefix
            &good[..good.len() - 1],            // truncated secret
            &good.replace('.', ""),             // no separator
            &format!("{}A", good.as_str()),     // secret too long
            &good.replacen("tgm_", "tgm_A", 1), // id too long
        ] {
            assert!(parse(bad).is_none(), "accepted {bad:?}");
        }
    }
}
