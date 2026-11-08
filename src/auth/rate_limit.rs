//! Per-client-IP limits on the anonymous endpoints (`/register`, `/authorize`,
//! `/token`, `/revoke`, and `/media/{ticket}` for Media links): a token bucket
//! per endpoint and client, answered with `429 Too Many Requests` +
//! `Retry-After` when empty. Everything else (`/mcp`, `/healthz`,
//! `.well-known/*`, the Login flow pages) is not limited here.
//!
//! The client is the TCP peer, or, when [`OAuthLimits::trust_forwarded_for`] is on
//! (behind kamal-proxy), the right-most `X-Forwarded-For` entry: the one our own
//! proxy appended, which a client cannot forge.

use std::{
    collections::HashMap,
    fmt,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use axum::{
    Json,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde_json::json;
use tokio::time::Instant;

use super::{AuthState, pages};

/// Above this many tracked buckets, full ones (same as fresh) are forgotten.
const MAX_TRACKED: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    pub per_minute: u32,
    pub burst: u32,
}

/// Limits on what anonymous callers can make the server do.
#[derive(Debug, Clone, Copy)]
pub struct OAuthLimits {
    /// Take the client IP from the right-most `X-Forwarded-For` entry. Only safe
    /// behind a proxy that always appends (or sets) it from its own TCP peer.
    pub trust_forwarded_for: bool,
    pub authorize: Rate,
    pub register: Rate,
    /// `/token` and `/revoke`, each its own bucket.
    pub token: Rate,
    /// `/media/{ticket}`: Media link downloads (and guesses).
    pub media: Rate,
    /// Login flows pending at once, server-wide. Each holds a Telegram connection.
    pub max_pending_logins: usize,
    /// Login flows pending at once from one client.
    pub max_pending_logins_per_client: usize,
}

impl Default for OAuthLimits {
    fn default() -> Self {
        Self {
            trust_forwarded_for: false,
            authorize: Rate {
                per_minute: 10,
                burst: 5,
            },
            register: Rate {
                per_minute: 5,
                burst: 5,
            },
            token: Rate {
                per_minute: 30,
                burst: 10,
            },
            media: Rate {
                per_minute: 30,
                burst: 10,
            },
            max_pending_logins: 50,
            max_pending_logins_per_client: 3,
        }
    }
}

/// Who is calling, for limiting: an IPv4 address, or the /64 of an IPv6 address
/// (one host usually has a whole /64, so per-address limits would be no limit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientIp(IpAddr);

impl ClientIp {
    pub fn new(ip: IpAddr) -> Self {
        match ip {
            IpAddr::V4(_) => Self(ip),
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => Self(IpAddr::V4(v4)),
                None => {
                    let prefix = u128::from(v6) & !((1u128 << 64) - 1);
                    Self(IpAddr::V6(Ipv6Addr::from(prefix)))
                }
            },
        }
    }

    /// The right-most `X-Forwarded-For` entry if trusted and well-formed, else the peer.
    pub fn of(headers: &HeaderMap, peer: IpAddr, trust_forwarded_for: bool) -> Self {
        let forwarded = trust_forwarded_for
            .then(|| {
                headers
                    .get_all("x-forwarded-for")
                    .iter()
                    .next_back()?
                    .to_str()
                    .ok()?
                    .rsplit(',')
                    .next()?
                    .trim()
                    .parse::<IpAddr>()
                    .ok()
            })
            .flatten();
        Self::new(forwarded.unwrap_or(peer))
    }
}

impl fmt::Display for ClientIp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            IpAddr::V4(ip) => ip.fmt(f),
            IpAddr::V6(ip) => write!(f, "{ip}/64"),
        }
    }
}

/// The limited endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Endpoint {
    Register,
    Authorize,
    Token,
    Revoke,
    Media,
}

impl Endpoint {
    fn path(self) -> &'static str {
        match self {
            Self::Register => "/register",
            Self::Authorize => "/authorize",
            Self::Token => "/token",
            Self::Revoke => "/revoke",
            // Never the ticket: it is a credential.
            Self::Media => "/media",
        }
    }

    fn rate(self, limits: &OAuthLimits) -> Rate {
        match self {
            Self::Register => limits.register,
            Self::Authorize => limits.authorize,
            Self::Token | Self::Revoke => limits.token,
            Self::Media => limits.media,
        }
    }
}

struct Bucket {
    tokens: f64,
    refilled_at: Instant,
    /// Rejections since the last admitted request: only the first one is logged.
    rejected: u64,
}

impl Bucket {
    fn refill(&mut self, rate: Rate, now: Instant) {
        let elapsed = now.saturating_duration_since(self.refilled_at);
        self.tokens = (self.tokens + elapsed.as_secs_f64() * per_sec(rate)).min(burst(rate));
        self.refilled_at = now;
    }
}

fn per_sec(rate: Rate) -> f64 {
    f64::from(rate.per_minute.max(1)) / 60.0
}

fn burst(rate: Rate) -> f64 {
    f64::from(rate.burst.max(1))
}

pub struct RateLimiter {
    limits: OAuthLimits,
    buckets: Mutex<HashMap<(Endpoint, ClientIp), Bucket>>,
    rejected_total: AtomicU64,
}

impl RateLimiter {
    pub fn new(limits: OAuthLimits) -> Self {
        Self {
            limits,
            buckets: Mutex::default(),
            rejected_total: AtomicU64::new(0),
        }
    }

    /// Take one token, or say how long until one exists.
    pub fn check(&self, endpoint: Endpoint, client: ClientIp) -> Result<(), Duration> {
        let now = Instant::now();
        let rate = endpoint.rate(&self.limits);
        let mut buckets = self.buckets.lock().expect("rate limiter lock");
        if buckets.len() >= MAX_TRACKED {
            // A full bucket is the same as a fresh one.
            buckets.retain(|(endpoint, _), bucket| {
                let rate = endpoint.rate(&self.limits);
                bucket.refill(rate, now);
                bucket.tokens < burst(rate)
            });
        }
        let bucket = buckets.entry((endpoint, client)).or_insert(Bucket {
            tokens: burst(rate),
            refilled_at: now,
            rejected: 0,
        });
        bucket.refill(rate, now);
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            bucket.rejected = 0;
            return Ok(());
        }
        bucket.rejected += 1;
        let total = self.rejected_total.fetch_add(1, Ordering::Relaxed) + 1;
        if bucket.rejected == 1 {
            tracing::warn!(
                route = endpoint.path(),
                client_ip = %client,
                rejected_total = total,
                "rate limited"
            );
        }
        Err(Duration::from_secs_f64(
            (1.0 - bucket.tokens) / per_sec(rate),
        ))
    }
}

/// Middleware in front of one limited endpoint. Also hands the [`ClientIp`] on to
/// the handler as a request extension.
pub async fn limit(
    State((auth, endpoint)): State<(Arc<AuthState>, Endpoint)>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(ConnectInfo(peer)) = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .copied()
    else {
        tracing::error!("no peer address: the server must be started with ConnectInfo");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let limiter = &auth.rate_limiter;
    let client = ClientIp::of(
        request.headers(),
        peer.ip(),
        limiter.limits.trust_forwarded_for,
    );
    match limiter.check(endpoint, client) {
        Ok(()) => {
            request.extensions_mut().insert(client);
            next.run(request).await
        }
        Err(wait) => too_many_requests(endpoint, wait),
    }
}

fn too_many_requests(endpoint: Endpoint, wait: Duration) -> Response {
    let mut response = match endpoint {
        // A browser lands here.
        Endpoint::Authorize => pages::error(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many requests",
            "Too many sign-in attempts from your network. Wait a minute and start again from your app.",
        ),
        _ => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({
                "error": "too_many_requests",
                "error_description": "rate limit exceeded, retry later",
            })),
        )
            .into_response(),
    };
    let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(seconds.max(1)));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(xff: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in xff {
            headers.append("x-forwarded-for", HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    const PEER: &str = "172.18.0.2";

    #[test]
    fn forwarded_for_is_ignored_unless_trusted() {
        let client = ClientIp::of(&headers(&["203.0.113.7"]), ip(PEER), false);
        assert_eq!(client, ClientIp::new(ip(PEER)));
    }

    #[test]
    fn the_right_most_forwarded_entry_is_the_client() {
        // A client may send its own XFF; the proxy appends the address it saw.
        for xff in [
            &["198.51.100.1, 203.0.113.7"][..],
            &["198.51.100.1,203.0.113.7 "],
            &["198.51.100.1", "203.0.113.7"],
            &["203.0.113.7"],
        ] {
            assert_eq!(
                ClientIp::of(&headers(xff), ip(PEER), true),
                ClientIp::new(ip("203.0.113.7")),
                "{xff:?}"
            );
        }
    }

    #[test]
    fn a_missing_or_malformed_forwarded_entry_falls_back_to_the_peer() {
        for xff in [
            &[][..],
            &["unknown"],
            &["203.0.113.7, "],
            &["203.0.113.7:443"],
        ] {
            assert_eq!(
                ClientIp::of(&headers(xff), ip(PEER), true),
                ClientIp::new(ip(PEER)),
                "{xff:?}"
            );
        }
    }

    #[test]
    fn ipv6_clients_are_grouped_by_64_and_mapped_v4_is_v4() {
        assert_eq!(
            ClientIp::new(ip("2001:db8:1:2:aaaa::1")),
            ClientIp::new(ip("2001:db8:1:2:bbbb::2"))
        );
        assert_ne!(
            ClientIp::new(ip("2001:db8:1:2::1")),
            ClientIp::new(ip("2001:db8:1:3::1"))
        );
        assert_eq!(
            ClientIp::new(ip("::ffff:203.0.113.7")),
            ClientIp::new(ip("203.0.113.7"))
        );
        assert_eq!(
            ClientIp::new(ip("2001:db8:1:2::1")).to_string(),
            "2001:db8:1:2::/64"
        );
    }

    fn limiter(per_minute: u32, burst: u32) -> RateLimiter {
        let rate = Rate { per_minute, burst };
        RateLimiter::new(OAuthLimits {
            authorize: rate,
            register: rate,
            token: rate,
            ..OAuthLimits::default()
        })
    }

    #[tokio::test(start_paused = true)]
    async fn burst_then_refusal_with_the_wait_until_the_next_token() {
        let limiter = limiter(10, 2); // one token per 6 s
        let a = ClientIp::new(ip("203.0.113.7"));
        assert!(limiter.check(Endpoint::Authorize, a).is_ok());
        assert!(limiter.check(Endpoint::Authorize, a).is_ok());
        assert_eq!(
            limiter.check(Endpoint::Authorize, a),
            Err(Duration::from_secs(6))
        );

        tokio::time::advance(Duration::from_secs(4)).await;
        let wait = limiter.check(Endpoint::Authorize, a).unwrap_err();
        assert!((wait.as_secs_f64() - 2.0).abs() < 0.01, "{wait:?}");

        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(limiter.check(Endpoint::Authorize, a).is_ok());
        assert!(limiter.check(Endpoint::Authorize, a).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn clients_and_endpoints_have_their_own_buckets() {
        let limiter = limiter(1, 1);
        let a = ClientIp::new(ip("203.0.113.7"));
        let b = ClientIp::new(ip("203.0.113.8"));
        assert!(limiter.check(Endpoint::Token, a).is_ok());
        assert!(limiter.check(Endpoint::Token, a).is_err());
        assert!(limiter.check(Endpoint::Token, b).is_ok());
        assert!(limiter.check(Endpoint::Revoke, a).is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn full_buckets_are_forgotten_when_many_are_tracked() {
        let limiter = limiter(60, 1); // one token per second
        for i in 0..MAX_TRACKED as u32 - 1 {
            let client = ClientIp::new(IpAddr::V4(i.into()));
            limiter.check(Endpoint::Register, client).unwrap();
        }
        tokio::time::advance(Duration::from_millis(500)).await;
        let recent = ClientIp::new(ip("203.0.113.7"));
        limiter.check(Endpoint::Register, recent).unwrap();

        // The first ones are full again, `recent` is not: only it survives the sweep.
        tokio::time::advance(Duration::from_millis(600)).await;
        limiter
            .check(Endpoint::Register, ClientIp::new(ip("203.0.113.8")))
            .unwrap();
        assert_eq!(limiter.buckets.lock().unwrap().len(), 2);
        assert!(limiter.check(Endpoint::Register, recent).is_err());
    }
}
