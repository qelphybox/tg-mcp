//! The Confirmation cookie (#45): set by the Consent screen that grants
//! `send`, in that browser only, and the one thing that lets a pending send
//! be sent (or cancelled) on the confirmation page. The model's own shell has
//! the page's URL, never this cookie.
//!
//! The cookie is `tgm_confirm_<handle>=<secret>`: a random 256-bit secret
//! under a random name, so the Grants of one browser each keep their own
//! (`HttpOnly; SameSite=Strict; Path=/confirm`, `Secure` on an https server,
//! `Max-Age` the refresh Token lifetime). The server keeps only a verifier,
//! the SHA-256 of the secret, and only sealed with the Grant's Send
//! allow-list ([`crate::allow_list`]): a database dump holds no cookie, and a
//! database writer can't swap in one of their own. Neither the name nor the
//! value is ever logged.

use std::time::Duration;

use axum::http::{HeaderMap, HeaderValue, header};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Every Confirmation cookie's name starts with this.
pub const PREFIX: &str = "tgm_confirm_";
/// The path every Confirmation cookie is sent to, and nowhere else.
pub const PATH: &str = "/confirm";
/// At most this many Confirmation cookies of one request are checked.
const MAX_CHECKED: usize = 32;
/// Domain separation of the verifier.
const VERIFIER_CONTEXT: &[u8] = b"tg-mcp/confirm-cookie/v1:";

/// A new Confirmation cookie: the header that sets it, and the verifier the
/// Grant keeps.
pub struct Issued {
    pub set_cookie: HeaderValue,
    pub verifier: [u8; 32],
}

/// A Confirmation cookie a request carried that matched a verifier.
#[derive(Clone)]
pub struct Matched {
    name: String,
    value: String,
}

/// A fresh cookie living `max_age`; `Secure` when `secure` (an https server).
pub fn issue(max_age: Duration, secure: bool) -> Issued {
    let name = format!(
        "{PREFIX}{}",
        URL_SAFE_NO_PAD.encode(crate::sessions::random_bytes::<9>())
    );
    let value = URL_SAFE_NO_PAD.encode(crate::sessions::random_bytes::<32>());
    Issued {
        set_cookie: set_cookie(&name, &value, max_age, secure),
        verifier: verifier(&value),
    }
}

/// The same cookie again, living `max_age` from now: a browser that uses
/// its cookie keeps it as long as it keeps using it.
pub fn renew(matched: &Matched, max_age: Duration, secure: bool) -> HeaderValue {
    set_cookie(&matched.name, &matched.value, max_age, secure)
}

fn set_cookie(name: &str, value: &str, max_age: Duration, secure: bool) -> HeaderValue {
    let secure = if secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{name}={value}; Path={PATH}; HttpOnly; SameSite=Strict; Max-Age={}{secure}",
        max_age.as_secs()
    ))
    .expect("base64url cookie")
}

/// What the Grant keeps of a cookie whose value is `value`.
pub fn verifier(value: &str) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(VERIFIER_CONTEXT);
    hash.update(value.as_bytes());
    hash.finalize().into()
}

/// The Confirmation cookie in `headers` that `expected` verifies, if any.
/// Compared in constant time; at most [`MAX_CHECKED`] cookies are looked at.
pub fn find(headers: &HeaderMap, expected: &[u8; 32]) -> Option<Matched> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .filter(|(name, _)| name.starts_with(PREFIX))
        .take(MAX_CHECKED)
        .find(|(_, value)| bool::from(verifier(value).ct_eq(expected)))
        .map(|(name, value)| Matched {
            name: name.to_string(),
            value: value.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie_pair(issued: &Issued) -> String {
        issued
            .set_cookie
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string()
    }

    fn with_cookies(cookies: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for cookie in cookies {
            headers.append(header::COOKIE, HeaderValue::from_str(cookie).unwrap());
        }
        headers
    }

    #[test]
    fn a_cookie_is_strict_http_only_scoped_to_confirm_and_lives_as_long_as_refresh() {
        let issued = issue(Duration::from_secs(2_592_000), true);
        let header = issued.set_cookie.to_str().unwrap();
        for part in [
            "Path=/confirm",
            "HttpOnly",
            "SameSite=Strict",
            "Max-Age=2592000",
            "Secure",
        ] {
            assert!(header.contains(&format!("; {part}")), "{part}: {header}");
        }
        let (name, value) = cookie_pair(&issued)
            .split_once('=')
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .unwrap();
        assert!(name.starts_with(PREFIX), "{name}");
        assert_eq!(URL_SAFE_NO_PAD.decode(&value).unwrap().len(), 32);
        assert_eq!(issued.verifier, verifier(&value));
        assert_ne!(
            issued.verifier,
            Sha256::digest(value.as_bytes()).as_slice(),
            "domain separated"
        );
        // Plain http (a local server): no Secure, or the browser drops it.
        let local = issue(Duration::from_secs(60), false);
        assert!(!local.set_cookie.to_str().unwrap().contains("Secure"));

        let other = issue(Duration::from_secs(60), true);
        assert_ne!(cookie_pair(&issued), cookie_pair(&other));
    }

    #[test]
    fn only_the_cookie_the_verifier_checks_matches() {
        let mine = issue(Duration::from_secs(60), true);
        let other = issue(Duration::from_secs(60), true);
        let both = format!(
            "tgm_login=x; {}; {}",
            cookie_pair(&other),
            cookie_pair(&mine)
        );
        let found = find(&with_cookies(&[&both]), &mine.verifier).unwrap();
        assert_eq!(
            format!("{}={}", found.name, found.value),
            cookie_pair(&mine)
        );
        assert!(find(&with_cookies(&[&cookie_pair(&other)]), &mine.verifier).is_none());
        assert!(find(&HeaderMap::new(), &mine.verifier).is_none());

        // The right secret under a name that isn't ours doesn't count.
        let value = cookie_pair(&mine).split_once('=').unwrap().1.to_string();
        assert!(
            find(
                &with_cookies(&[&format!("session={value}")]),
                &mine.verifier
            )
            .is_none()
        );

        // Renewing keeps name and value.
        let renewed = renew(&found, Duration::from_secs(99), false);
        assert!(
            renewed
                .to_str()
                .unwrap()
                .starts_with(&format!("{};", cookie_pair(&mine))),
            "{renewed:?}"
        );
        assert!(renewed.to_str().unwrap().contains("Max-Age=99"));
    }
}
