//! The `limits` module: per-Account request budget and FLOOD_WAIT handling.
//!
//! Every request a Live client makes to Telegram goes through the Account's
//! [`AccountLimiter`]:
//! - a token bucket (`burst` requests at once, refilled at `per_minute`);
//! - a remembered `FLOOD_WAIT`: once Telegram says "wait N s", every call for
//!   that Account waits or fails fast until the window has passed, without
//!   touching Telegram again (honoured, never retried around);
//! - waits up to `max_wait` are slept through transparently; longer ones
//!   become an error that says how long to wait.
//!
//! Accounts are independent: one Account in FLOOD_WAIT doesn't slow others.

use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::time::Instant;

use crate::tg::TgError;

#[derive(Debug, Clone, Copy)]
pub struct LimitsConfig {
    pub per_minute: u32,
    pub burst: u32,
    /// Longest wait (budget or FLOOD_WAIT) slept through inside one call.
    pub max_wait: Duration,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            per_minute: 60,
            burst: 20,
            max_wait: Duration::from_secs(10),
        }
    }
}

/// One limiter per Account, shared by all of its Grants.
pub struct Limits {
    config: LimitsConfig,
    accounts: Mutex<HashMap<i64, Arc<AccountLimiter>>>,
}

impl Limits {
    pub fn new(config: LimitsConfig) -> Self {
        Self {
            config,
            accounts: Mutex::new(HashMap::new()),
        }
    }

    pub fn for_account(&self, account_id: i64) -> Arc<AccountLimiter> {
        let mut accounts = self.accounts.lock().expect("limits lock");
        if accounts.len() > 10_000 {
            // Forget limiters nobody holds; a fresh one starts with a full bucket,
            // but an active FLOOD_WAIT is always held by its Live client.
            accounts.retain(|_, limiter| Arc::strong_count(limiter) > 1);
        }
        accounts
            .entry(account_id)
            .or_insert_with(|| Arc::new(AccountLimiter::new(self.config)))
            .clone()
    }
}

struct State {
    /// May go negative: requests reserve tokens and sleep until they exist.
    tokens: f64,
    refilled_at: Instant,
    blocked_until: Option<Instant>,
}

pub struct AccountLimiter {
    config: LimitsConfig,
    state: Mutex<State>,
}

impl AccountLimiter {
    pub fn new(config: LimitsConfig) -> Self {
        Self {
            config,
            state: Mutex::new(State {
                tokens: f64::from(config.burst.max(1)),
                refilled_at: Instant::now(),
                blocked_until: None,
            }),
        }
    }

    /// Run one Telegram request under the budget. A `FLOOD_WAIT` from it blocks the
    /// Account; a short one is waited out and the request retried once.
    pub async fn run<T, F, Fut>(&self, mut request: F) -> Result<T, TgError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, TgError>>,
    {
        let mut retried = false;
        loop {
            self.admit().await?;
            match request().await {
                Err(TgError::FloodWait { seconds }) => {
                    self.block(seconds);
                    tracing::warn!(seconds, "telegram FLOOD_WAIT; account blocked");
                    if retried || Duration::from_secs(seconds.into()) > self.config.max_wait {
                        return Err(TgError::FloodWait { seconds });
                    }
                    retried = true;
                }
                other => return other,
            }
        }
    }

    /// Wait for permission to send one request, or fail if that would take too long.
    async fn admit(&self) -> Result<(), TgError> {
        let wait = {
            let mut state = self.state.lock().expect("limiter lock");
            let now = Instant::now();
            if let Some(until) = state.blocked_until {
                let left = until.saturating_duration_since(now);
                if left.is_zero() {
                    state.blocked_until = None;
                } else if left > self.config.max_wait {
                    return Err(TgError::FloodWait {
                        seconds: ceil_secs(left),
                    });
                }
            }
            let flood_left = state
                .blocked_until
                .map_or(Duration::ZERO, |until| until.saturating_duration_since(now));

            // Refill, then reserve one token.
            let per_sec = f64::from(self.config.per_minute.max(1)) / 60.0;
            let elapsed = now
                .saturating_duration_since(state.refilled_at)
                .as_secs_f64();
            state.tokens =
                (state.tokens + elapsed * per_sec).min(f64::from(self.config.burst.max(1)));
            state.refilled_at = now;
            let budget_wait = if state.tokens >= 1.0 {
                Duration::ZERO
            } else {
                Duration::from_secs_f64((1.0 - state.tokens) / per_sec)
            };
            let wait = budget_wait.max(flood_left);
            if wait > self.config.max_wait {
                return Err(TgError::RateLimited {
                    seconds: ceil_secs(budget_wait),
                });
            }
            state.tokens -= 1.0;
            wait
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        Ok(())
    }

    fn block(&self, seconds: u32) {
        let until = Instant::now() + Duration::from_secs(seconds.into());
        let mut state = self.state.lock().expect("limiter lock");
        state.blocked_until = Some(
            state
                .blocked_until
                .map_or(until, |current| current.max(until)),
        );
    }
}

fn ceil_secs(d: Duration) -> u32 {
    u32::try_from(d.as_secs() + u64::from(d.subsec_nanos() > 0)).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    fn config(per_minute: u32, burst: u32) -> LimitsConfig {
        LimitsConfig {
            per_minute,
            burst,
            max_wait: Duration::from_secs(10),
        }
    }

    async fn ok(calls: &AtomicU32) -> Result<(), TgError> {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn burst_goes_through_then_the_rate_paces_requests() {
        let limiter = AccountLimiter::new(config(60, 2));
        let calls = AtomicU32::new(0);
        let start = Instant::now();
        for _ in 0..4 {
            limiter.run(|| ok(&calls)).await.unwrap();
        }
        // 2 at once, then one per second.
        assert_eq!(start.elapsed(), Duration::from_secs(2));
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn a_queue_longer_than_max_wait_is_rate_limited() {
        let limiter = AccountLimiter::new(config(6, 1)); // one token per 10 s
        let calls = AtomicU32::new(0);
        // Three at once: the first goes now, the second queues 10 s, the third would
        // need 20 s, more than max_wait.
        let (a, b, c) = tokio::join!(
            limiter.run(|| ok(&calls)),
            limiter.run(|| ok(&calls)),
            limiter.run(|| ok(&calls)),
        );
        assert!(a.is_ok() && b.is_ok());
        match c {
            Err(TgError::RateLimited { seconds: 20 }) => {}
            other => panic!("expected RateLimited 20 s, got {other:?}"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn sequential_callers_are_paced_not_refused() {
        let limiter = AccountLimiter::new(config(6, 1));
        let calls = AtomicU32::new(0);
        let start = Instant::now();
        for _ in 0..3 {
            limiter.run(|| ok(&calls)).await.unwrap();
        }
        assert_eq!(start.elapsed(), Duration::from_secs(20));
    }

    #[tokio::test(start_paused = true)]
    async fn short_flood_wait_is_slept_through_and_retried_once() {
        let limiter = AccountLimiter::new(config(600, 20));
        let calls = AtomicU32::new(0);
        let start = Instant::now();
        let result = limiter
            .run(|| async {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(TgError::FloodWait { seconds: 5 })
                } else {
                    Ok("done")
                }
            })
            .await;
        assert_eq!(result.unwrap(), "done");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(start.elapsed() >= Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn long_flood_wait_blocks_the_account_without_calling_telegram() {
        let limiter = AccountLimiter::new(config(600, 20));
        let calls = AtomicU32::new(0);
        let flood = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(TgError::FloodWait { seconds: 120 })
        };
        assert!(matches!(
            limiter.run(flood).await,
            Err(TgError::FloodWait { seconds: 120 })
        ));

        tokio::time::advance(Duration::from_secs(30)).await;
        match limiter.run(|| ok(&calls)).await {
            Err(TgError::FloodWait { seconds: 90 }) => {}
            other => panic!("expected the remaining 90 s, got {other:?}"),
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "Telegram must not be asked again while blocked"
        );

        // Within max_wait of the end: waited out, then sent.
        tokio::time::advance(Duration::from_secs(85)).await;
        limiter.run(|| ok(&calls)).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_flood_wait_that_repeats_after_the_retry_is_returned() {
        let limiter = AccountLimiter::new(config(600, 20));
        let calls = AtomicU32::new(0);
        let result = limiter
            .run(|| async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(TgError::FloodWait { seconds: 3 })
            })
            .await;
        assert!(matches!(result, Err(TgError::FloodWait { seconds: 3 })));
        assert_eq!(calls.load(Ordering::SeqCst), 2, "one retry, not a loop");
    }

    #[tokio::test(start_paused = true)]
    async fn accounts_are_independent() {
        let limits = Limits::new(config(600, 20));
        let blocked = limits.for_account(1);
        let _ = blocked
            .run(|| async { Err::<(), _>(TgError::FloodWait { seconds: 300 }) })
            .await;
        assert!(matches!(
            limits.for_account(1).run(|| async { Ok(()) }).await,
            Err(TgError::FloodWait { .. })
        ));
        assert!(limits.for_account(2).run(|| async { Ok(()) }).await.is_ok());
    }
}
