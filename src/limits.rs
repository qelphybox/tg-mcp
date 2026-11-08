//! The `limits` module: per-Account request budget and FLOOD_WAIT handling.
//!
//! Every request a Live client makes to Telegram goes through the Account's
//! [`AccountLimiter`], tagged with its [`MethodClass`]:
//! - a token bucket (`burst` requests at once, refilled at `per_minute`), one per
//!   Account and shared by all classes;
//! - a remembered `FLOOD_WAIT` per class: once Telegram says "wait N s" to a
//!   request, every call of that class for that Account waits or fails fast until
//!   the window has passed, without touching Telegram again (honoured, never
//!   retried around); the other classes go on;
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

/// Which `FLOOD_WAIT` window a Telegram request falls in. Telegram issues
/// `FLOOD_WAIT` per method, and search and file downloads get them more often and
/// for longer than reading, so a flood in one class must not block the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodClass {
    /// Everything that isn't search or files: reading chats, history, details.
    Default,
    /// `messages.searchGlobal`, `messages.search`, `contacts.search`.
    Search,
    /// `upload.*`: file downloads (`upload.getFile`, `upload.getCdnFile`, ...).
    Files,
    /// `messages.transcribeAudio`. Without Premium, Telegram answers a used-up
    /// weekly trial with a `FLOOD_WAIT` lasting until the trial resets (days,
    /// as TDLib reads it), which must not hold back anything else.
    Transcribe,
}

impl MethodClass {
    pub const ALL: [Self; 4] = [Self::Default, Self::Search, Self::Files, Self::Transcribe];

    /// The class of a Telegram method, by its TL name (e.g. `messages.searchGlobal`).
    pub fn of(method: &str) -> Self {
        match method {
            "messages.searchGlobal" | "messages.search" | "contacts.search" => Self::Search,
            "messages.transcribeAudio" => Self::Transcribe,
            _ if method.starts_with("upload.") => Self::Files,
            _ => Self::Default,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Search => "search",
            Self::Files => "files",
            Self::Transcribe => "transcribe",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

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
    /// The remembered FLOOD_WAIT of each [`MethodClass`], by [`MethodClass::index`].
    blocked_until: [Option<Instant>; MethodClass::ALL.len()],
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
                blocked_until: [None; MethodClass::ALL.len()],
            }),
        }
    }

    /// Run one Telegram request of `class` under the budget. A `FLOOD_WAIT` from it
    /// blocks that class for the Account; a short one is waited out and the request
    /// retried once.
    pub async fn run<T, F, Fut>(&self, class: MethodClass, mut request: F) -> Result<T, TgError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, TgError>>,
    {
        let mut retried = false;
        loop {
            self.admit(class).await?;
            match request().await {
                Err(TgError::FloodWait { seconds, .. }) => {
                    self.block(class, seconds);
                    tracing::warn!(
                        seconds,
                        class = class.name(),
                        "telegram FLOOD_WAIT; class blocked for the account"
                    );
                    if retried || Duration::from_secs(seconds.into()) > self.config.max_wait {
                        return Err(TgError::FloodWait { seconds, class });
                    }
                    retried = true;
                }
                other => return other,
            }
        }
    }

    /// Wait for permission to send one request, or fail if that would take too long.
    async fn admit(&self, class: MethodClass) -> Result<(), TgError> {
        let wait = {
            let mut state = self.state.lock().expect("limiter lock");
            let now = Instant::now();
            let blocked_until = &mut state.blocked_until[class.index()];
            let flood_left =
                blocked_until.map_or(Duration::ZERO, |until| until.saturating_duration_since(now));
            if flood_left.is_zero() {
                *blocked_until = None;
            } else if flood_left > self.config.max_wait {
                return Err(TgError::FloodWait {
                    seconds: ceil_secs(flood_left),
                    class,
                });
            }

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

    fn block(&self, class: MethodClass, seconds: u32) {
        let until = Instant::now() + Duration::from_secs(seconds.into());
        let mut state = self.state.lock().expect("limiter lock");
        let blocked_until = &mut state.blocked_until[class.index()];
        *blocked_until = Some(blocked_until.map_or(until, |current| current.max(until)));
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
            limiter
                .run(MethodClass::Default, || ok(&calls))
                .await
                .unwrap();
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
            limiter.run(MethodClass::Default, || ok(&calls)),
            limiter.run(MethodClass::Default, || ok(&calls)),
            limiter.run(MethodClass::Default, || ok(&calls)),
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
            limiter
                .run(MethodClass::Default, || ok(&calls))
                .await
                .unwrap();
        }
        assert_eq!(start.elapsed(), Duration::from_secs(20));
    }

    #[tokio::test(start_paused = true)]
    async fn short_flood_wait_is_slept_through_and_retried_once() {
        let limiter = AccountLimiter::new(config(600, 20));
        let calls = AtomicU32::new(0);
        let start = Instant::now();
        let result = limiter
            .run(MethodClass::Default, || async {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(TgError::FloodWait {
                        seconds: 5,
                        class: MethodClass::Default,
                    })
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
            Err::<(), _>(TgError::FloodWait {
                seconds: 120,
                class: MethodClass::Default,
            })
        };
        assert!(matches!(
            limiter.run(MethodClass::Default, flood).await,
            Err(TgError::FloodWait { seconds: 120, .. })
        ));

        tokio::time::advance(Duration::from_secs(30)).await;
        match limiter.run(MethodClass::Default, || ok(&calls)).await {
            Err(TgError::FloodWait { seconds: 90, .. }) => {}
            other => panic!("expected the remaining 90 s, got {other:?}"),
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "Telegram must not be asked again while blocked"
        );

        // Within max_wait of the end: waited out, then sent.
        tokio::time::advance(Duration::from_secs(85)).await;
        limiter
            .run(MethodClass::Default, || ok(&calls))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_flood_wait_that_repeats_after_the_retry_is_returned() {
        let limiter = AccountLimiter::new(config(600, 20));
        let calls = AtomicU32::new(0);
        let result = limiter
            .run(MethodClass::Default, || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(TgError::FloodWait {
                    seconds: 3,
                    class: MethodClass::Default,
                })
            })
            .await;
        assert!(matches!(result, Err(TgError::FloodWait { seconds: 3, .. })));
        assert_eq!(calls.load(Ordering::SeqCst), 2, "one retry, not a loop");
    }

    #[tokio::test(start_paused = true)]
    async fn accounts_are_independent() {
        let limits = Limits::new(config(600, 20));
        let blocked = limits.for_account(1);
        let _ = blocked
            .run(MethodClass::Default, || async {
                Err::<(), _>(TgError::FloodWait {
                    seconds: 300,
                    class: MethodClass::Default,
                })
            })
            .await;
        assert!(matches!(
            limits
                .for_account(1)
                .run(MethodClass::Default, || async { Ok(()) })
                .await,
            Err(TgError::FloodWait { .. })
        ));
        assert!(
            limits
                .for_account(2)
                .run(MethodClass::Default, || async { Ok(()) })
                .await
                .is_ok()
        );
    }

    /// A flood the way the Live client reports one: `FLOOD_WAIT_X` from Telegram,
    /// classified by the request that got it.
    fn flood(calls: &AtomicU32, seconds: u32, class: MethodClass) -> Result<(), TgError> {
        calls.fetch_add(1, Ordering::SeqCst);
        Err(TgError::FloodWait { seconds, class })
    }

    /// A long flood in `flooded` fails that class fast, untouched by Telegram, and
    /// leaves every other class of the same Account alone.
    async fn only_the_flooded_class_is_blocked(flooded: MethodClass) {
        let limiter = AccountLimiter::new(config(600, 20));
        let calls = AtomicU32::new(0);
        match limiter
            .run(flooded, || async { flood(&calls, 300, flooded) })
            .await
        {
            Err(TgError::FloodWait {
                seconds: 300,
                class,
            }) => assert_eq!(class, flooded),
            other => panic!("expected FLOOD_WAIT 300 s, got {other:?}"),
        }

        tokio::time::advance(Duration::from_secs(100)).await;
        match limiter.run(flooded, || ok(&calls)).await {
            Err(TgError::FloodWait {
                seconds: 200,
                class,
            }) => assert_eq!(class, flooded),
            other => panic!("expected {flooded:?} to fail fast with 200 s, got {other:?}"),
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the flooded class must not reach Telegram"
        );

        let start = Instant::now();
        for other in MethodClass::ALL.into_iter().filter(|c| *c != flooded) {
            limiter
                .run(other, || ok(&calls))
                .await
                .unwrap_or_else(|err| panic!("{other:?} blocked by a {flooded:?} flood: {err:?}"));
        }
        assert_eq!(start.elapsed(), Duration::ZERO, "other classes don't wait");
        assert_eq!(calls.load(Ordering::SeqCst), MethodClass::ALL.len() as u32);
    }

    #[tokio::test(start_paused = true)]
    async fn a_search_flood_blocks_only_search() {
        only_the_flooded_class_is_blocked(MethodClass::Search).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_files_flood_blocks_only_files() {
        only_the_flooded_class_is_blocked(MethodClass::Files).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_transcribe_flood_blocks_only_transcribe() {
        only_the_flooded_class_is_blocked(MethodClass::Transcribe).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_default_flood_blocks_only_default() {
        only_the_flooded_class_is_blocked(MethodClass::Default).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_short_search_flood_being_slept_through_doesnt_delay_reading() {
        let limiter = AccountLimiter::new(config(600, 20));
        let calls = AtomicU32::new(0);
        let start = Instant::now();
        let (search, read_at) = tokio::join!(
            limiter.run(MethodClass::Search, || async {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(TgError::FloodWait {
                        seconds: 5,
                        class: MethodClass::Search,
                    })
                } else {
                    Ok(())
                }
            }),
            async {
                limiter
                    .run(MethodClass::Default, || async { Ok(()) })
                    .await
                    .unwrap();
                start.elapsed()
            },
        );
        search.unwrap();
        assert_eq!(read_at, Duration::ZERO);
        assert!(
            start.elapsed() >= Duration::from_secs(5),
            "the search waited"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn all_classes_draw_from_one_bucket() {
        let limiter = AccountLimiter::new(config(60, 2)); // one token per second
        let calls = AtomicU32::new(0);
        let start = Instant::now();
        limiter
            .run(MethodClass::Search, || ok(&calls))
            .await
            .unwrap();
        limiter
            .run(MethodClass::Files, || ok(&calls))
            .await
            .unwrap();
        assert_eq!(start.elapsed(), Duration::ZERO);
        limiter
            .run(MethodClass::Default, || ok(&calls))
            .await
            .unwrap();
        assert_eq!(
            start.elapsed(),
            Duration::from_secs(1),
            "the bucket was empty"
        );
    }

    #[test]
    fn methods_fall_in_their_class() {
        for search in [
            "messages.searchGlobal",
            "messages.search",
            "contacts.search",
        ] {
            assert_eq!(MethodClass::of(search), MethodClass::Search, "{search}");
        }
        for files in [
            "upload.getFile",
            "upload.getCdnFile",
            "upload.getCdnFileHashes",
            "upload.getFileHashes",
            "upload.getWebFile",
            "upload.reuploadCdnFile",
        ] {
            assert_eq!(MethodClass::of(files), MethodClass::Files, "{files}");
        }
        // Without Premium, a used-up weekly trial is FLOOD_WAIT_X for days:
        // it must not hold back reading.
        assert_eq!(
            MethodClass::of("messages.transcribeAudio"),
            MethodClass::Transcribe
        );
        for default in [
            "messages.getHistory",
            "messages.getDialogs",
            "users.getFullUser",
            "channels.getFullChannel",
            "messages.searchSentMedia",
            "uploads.getFile",
            "",
        ] {
            assert_eq!(MethodClass::of(default), MethodClass::Default, "{default}");
        }
    }
}
