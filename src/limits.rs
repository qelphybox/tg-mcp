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
//! Writes (class [`MethodClass::Writes`], ADR-0005) also draw from the Writes
//! budget (`writes_per_10_min` in any 10 minutes), and a sent message
//! ([`AccountLimiter::run_send`]) from its Chat's Send cap
//! (`sends_per_chat_per_minute` in any minute). A write never waits: it goes
//! now or fails with the wait ([`TgError::WritesLimited`], [`TgError::RateLimited`],
//! [`TgError::FloodWait`]), so nothing is written after the caller gave up. A
//! refused write costs nothing; one Telegram was asked for counts whatever it
//! answered.
//!
//! Accounts are independent: one Account in FLOOD_WAIT doesn't slow others.

use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::time::Instant;

use crate::tg::{TgError, chats::ChatId};

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
    /// Requests that change the Account (ADR-0005): `messages.saveDraft`,
    /// `messages.sendMessage`, `messages.sendReaction`, `messages.readHistory`,
    /// `channels.readHistory`, `messages.readDiscussion` (a forum Topic read).
    /// They also spend the Writes budget and never wait inside a call; a new
    /// write method belongs here.
    Writes,
}

impl MethodClass {
    pub const ALL: [Self; 5] = [
        Self::Default,
        Self::Search,
        Self::Files,
        Self::Transcribe,
        Self::Writes,
    ];

    /// The class of a Telegram method, by its TL name (e.g. `messages.searchGlobal`).
    pub fn of(method: &str) -> Self {
        match method {
            "messages.searchGlobal" | "messages.search" | "contacts.search" => Self::Search,
            "messages.transcribeAudio" => Self::Transcribe,
            "messages.saveDraft"
            | "messages.sendMessage"
            | "messages.sendReaction"
            | "messages.readHistory"
            | "channels.readHistory"
            | "messages.readDiscussion" => Self::Writes,
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
            Self::Writes => "writes",
        }
    }

    pub(crate) fn index(self) -> usize {
        self as usize
    }
}

/// Whether a Telegram method (by TL name) sends a message, which counts against
/// the Send cap of its Chat ([`AccountLimiter::run_send`]). Reactions, marking
/// read and Drafts are writes but send nothing.
pub fn is_send(method: &str) -> bool {
    method == "messages.sendMessage"
}

/// Which of our write limits refused a write ([`TgError::WritesLimited`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteLimit {
    /// The Account's Writes budget: `writes_per_10_min` in any 10 minutes.
    Account,
    /// The Chat's Send cap: `sends_per_chat_per_minute` messages sent to one
    /// Chat in any minute.
    Chat,
}

impl WriteLimit {
    pub fn name(self) -> &'static str {
        match self {
            Self::Account => "writes_budget",
            Self::Chat => "chat_send_cap",
        }
    }
}

/// The span of the Writes budget.
pub const WRITES_WINDOW: Duration = Duration::from_secs(600);
/// The span of a Chat's Send cap.
pub const SENDS_WINDOW: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy)]
pub struct LimitsConfig {
    pub per_minute: u32,
    pub burst: u32,
    /// Longest wait (budget or FLOOD_WAIT) slept through inside one call.
    /// Never for writes, which don't wait.
    pub max_wait: Duration,
    /// Writes per Account in any [`WRITES_WINDOW`] (the Writes budget).
    pub writes_per_10_min: u32,
    /// Messages sent to one Chat per Account in any [`SENDS_WINDOW`] (the Send cap).
    pub sends_per_chat_per_minute: u32,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            per_minute: 60,
            burst: 20,
            max_wait: Duration::from_secs(10),
            writes_per_10_min: 20,
            sends_per_chat_per_minute: 5,
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
            // but an active FLOOD_WAIT (and the writes counted so far) is always
            // held by its Live client.
            accounts.retain(|_, limiter| Arc::strong_count(limiter) > 1);
        }
        accounts
            .entry(account_id)
            .or_insert_with(|| Arc::new(AccountLimiter::new(self.config)))
            .clone()
    }
}

/// When the last writes within a span happened, oldest first: "at most N in any
/// span", not per fixed span.
#[derive(Default)]
struct Window(VecDeque<Instant>);

impl Window {
    /// Forget what happened `span` or longer ago, then say how long until one
    /// more fits under `limit` (zero: now).
    fn wait(&mut self, now: Instant, limit: u32, span: Duration) -> Duration {
        self.forget(now, span);
        let limit = limit.max(1) as usize;
        if self.0.len() < limit {
            Duration::ZERO
        } else {
            (self.0[self.0.len() - limit] + span).saturating_duration_since(now)
        }
    }

    /// Forget what happened `span` or longer ago; whether anything is left.
    fn forget(&mut self, now: Instant, span: Duration) -> bool {
        while self.0.front().is_some_and(|at| *at + span <= now) {
            self.0.pop_front();
        }
        !self.0.is_empty()
    }

    fn record(&mut self, now: Instant) {
        self.0.push_back(now);
    }
}

struct State {
    /// May go negative: requests reserve tokens and sleep until they exist.
    tokens: f64,
    refilled_at: Instant,
    /// The remembered FLOOD_WAIT of each [`MethodClass`], by [`MethodClass::index`].
    blocked_until: [Option<Instant>; MethodClass::ALL.len()],
    /// The Writes budget.
    writes: Window,
    /// The Send cap of each Chat sent to in the last [`SENDS_WINDOW`].
    sends: HashMap<ChatId, Window>,
}

impl State {
    /// Refill the shared bucket up to `now`; how long until it holds a whole token.
    fn refill(&mut self, config: &LimitsConfig, now: Instant) -> Duration {
        let per_sec = f64::from(config.per_minute.max(1)) / 60.0;
        let elapsed = now
            .saturating_duration_since(self.refilled_at)
            .as_secs_f64();
        self.tokens = (self.tokens + elapsed * per_sec).min(f64::from(config.burst.max(1)));
        self.refilled_at = now;
        if self.tokens >= 1.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64((1.0 - self.tokens) / per_sec)
        }
    }

    /// What is left of `class`'s FLOOD_WAIT, forgetting one that has passed.
    fn flood_left(&mut self, class: MethodClass, now: Instant) -> Duration {
        let blocked_until = &mut self.blocked_until[class.index()];
        let left =
            blocked_until.map_or(Duration::ZERO, |until| until.saturating_duration_since(now));
        if left.is_zero() {
            *blocked_until = None;
        }
        left
    }
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
                writes: Window::default(),
                sends: HashMap::new(),
            }),
        }
    }

    /// Run one Telegram request of `class` under the budget. A `FLOOD_WAIT` from it
    /// blocks that class for the Account; a short one is waited out and the request
    /// retried once.
    ///
    /// A request of class [`MethodClass::Writes`] (a Draft, a reaction, marking
    /// read) also spends the Writes budget, and never waits or retries: it goes
    /// now or fails with the wait. A sent message goes through [`Self::run_send`].
    pub async fn run<T, F, Fut>(&self, class: MethodClass, request: F) -> Result<T, TgError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, TgError>>,
    {
        self.run_named(class, "unnamed", request).await
    }

    /// [`Self::run`] for TL method `method` (e.g. `messages.getPeerDialogs`),
    /// which its `FLOOD_WAIT` warning names: a method name is no content.
    pub async fn run_named<T, F, Fut>(
        &self,
        class: MethodClass,
        method: &'static str,
        mut request: F,
    ) -> Result<T, TgError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, TgError>>,
    {
        if class == MethodClass::Writes {
            return self.write(None, method, request).await;
        }
        let mut retried = false;
        loop {
            self.admit(class).await?;
            match request().await {
                Err(TgError::FloodWait { seconds, .. }) => {
                    self.block(class, seconds);
                    tracing::warn!(
                        seconds,
                        class = class.name(),
                        method,
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

    /// Run one request that sends a message to `chat` (`messages.sendMessage`):
    /// a write ([`MethodClass::Writes`]) that also counts against `chat`'s Send
    /// cap. Charged here, at the call to Telegram, so only what really goes
    /// out counts. Never waits, never retried: over any limit it fails at once
    /// with the wait.
    pub async fn run_send<T, F, Fut>(&self, chat: ChatId, request: F) -> Result<T, TgError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, TgError>>,
    {
        self.run_send_named(chat, "unnamed", request).await
    }

    /// [`Self::run_send`] for TL method `method` (`messages.sendMessage`), which
    /// its `FLOOD_WAIT` warning names, as [`Self::run_named`]'s do.
    pub async fn run_send_named<T, F, Fut>(
        &self,
        chat: ChatId,
        method: &'static str,
        request: F,
    ) -> Result<T, TgError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, TgError>>,
    {
        self.write(Some(chat), method, request).await
    }

    async fn write<T, F, Fut>(
        &self,
        chat: Option<ChatId>,
        method: &'static str,
        request: F,
    ) -> Result<T, TgError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, TgError>>,
    {
        let class = MethodClass::Writes;
        self.admit_write(chat)?;
        match request().await {
            Err(TgError::FloodWait { seconds, .. }) => {
                self.block(class, seconds);
                tracing::warn!(
                    seconds,
                    class = class.name(),
                    method,
                    "telegram FLOOD_WAIT; class blocked for the account"
                );
                Err(TgError::FloodWait { seconds, class })
            }
            other => other,
        }
    }

    /// Wait for permission to send one request, or fail if that would take too long.
    async fn admit(&self, class: MethodClass) -> Result<(), TgError> {
        let wait = {
            let mut state = self.state.lock().expect("limiter lock");
            let now = Instant::now();
            let flood_left = state.flood_left(class, now);
            if flood_left > self.config.max_wait {
                return Err(TgError::FloodWait {
                    seconds: ceil_secs(flood_left),
                    class,
                });
            }

            // Refill, then reserve one token.
            let budget_wait = state.refill(&self.config, now);
            let wait = budget_wait.max(flood_left);
            if wait > self.config.max_wait {
                crate::stats::global().refused(crate::stats::Refusal::RequestBudget);
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

    /// Admit one write now, or say how long to wait: the Writes FLOOD_WAIT, then
    /// whichever of the Send cap, the Writes budget and the shared bucket would
    /// hold it longest. Nothing is spent unless every limit lets it through.
    fn admit_write(&self, chat: Option<ChatId>) -> Result<(), TgError> {
        let config = &self.config;
        let mut state = self.state.lock().expect("limiter lock");
        let now = Instant::now();
        let flood_left = state.flood_left(MethodClass::Writes, now);
        if !flood_left.is_zero() {
            return Err(TgError::FloodWait {
                seconds: ceil_secs(flood_left),
                class: MethodClass::Writes,
            });
        }

        let per_chat = config.sends_per_chat_per_minute;
        // Chats not sent to for a minute have nothing to remember.
        state
            .sends
            .retain(|_, sends| sends.forget(now, SENDS_WINDOW));
        let chat_wait = chat.map_or(Duration::ZERO, |chat| {
            state
                .sends
                .entry(chat)
                .or_default()
                .wait(now, per_chat, SENDS_WINDOW)
        });
        let account_wait = state
            .writes
            .wait(now, config.writes_per_10_min, WRITES_WINDOW);
        let bucket_wait = state.refill(config, now);

        let longest = chat_wait.max(account_wait).max(bucket_wait);
        if !longest.is_zero() {
            let seconds = ceil_secs(longest);
            let refused = if longest == chat_wait {
                TgError::WritesLimited {
                    seconds,
                    limit: WriteLimit::Chat,
                }
            } else if longest == account_wait {
                TgError::WritesLimited {
                    seconds,
                    limit: WriteLimit::Account,
                }
            } else {
                TgError::RateLimited { seconds }
            };
            crate::stats::global().refused(match &refused {
                TgError::WritesLimited {
                    limit: WriteLimit::Chat,
                    ..
                } => crate::stats::Refusal::SendCap,
                TgError::WritesLimited { .. } => crate::stats::Refusal::WritesBudget,
                _ => crate::stats::Refusal::RequestBudget,
            });
            return Err(refused);
        }

        state.tokens -= 1.0;
        state.writes.record(now);
        if let Some(chat) = chat {
            state.sends.entry(chat).or_default().record(now);
        }
        Ok(())
    }

    fn block(&self, class: MethodClass, seconds: u32) {
        crate::stats::global().flood_wait(class, seconds);
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

    thread_local! {
        static LOGS: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    /// Writes into the calling thread's [`LOGS`].
    struct ThreadLogs;

    impl std::io::Write for ThreadLogs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            LOGS.with(|logs| logs.borrow_mut().extend_from_slice(buf));
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Starts capturing this thread's log lines (see [`captured_logs`]).
    ///
    /// One global subscriber for the whole test binary, not a per-test
    /// `set_default`: with a single scoped subscriber, tracing caches each
    /// callsite's interest from whichever thread hits it first, so a test
    /// without one running in parallel could switch the warning off for good.
    fn capture_logs() {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            tracing::subscriber::set_global_default(
                tracing_subscriber::fmt()
                    .with_ansi(false)
                    .with_writer(|| ThreadLogs)
                    .finish(),
            )
            .expect("no other global subscriber in the unit tests");
        });
        LOGS.with(|logs| logs.borrow_mut().clear());
    }

    /// What this thread logged since [`capture_logs`].
    fn captured_logs() -> String {
        LOGS.with(|logs| String::from_utf8(logs.borrow().clone()).unwrap())
    }

    fn config(per_minute: u32, burst: u32) -> LimitsConfig {
        LimitsConfig {
            per_minute,
            burst,
            max_wait: Duration::from_secs(10),
            ..LimitsConfig::default()
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

    #[tokio::test]
    async fn a_flood_wait_warning_names_the_method_telegram_throttled() {
        capture_logs();
        let limiter = AccountLimiter::new(config(600, 20));
        let calls = AtomicU32::new(0);
        let flooded = limiter
            .run_named(MethodClass::Default, "messages.getPeerDialogs", || async {
                flood(&calls, 300, MethodClass::Default)
            })
            .await;
        assert!(matches!(
            flooded,
            Err(TgError::FloodWait { seconds: 300, .. })
        ));
        let logs = captured_logs();
        assert!(
            logs.contains("seconds=300")
                && logs.contains("class=\"default\"")
                && logs.contains("method=\"messages.getPeerDialogs\""),
            "{logs}"
        );
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
    async fn a_writes_flood_blocks_only_writes() {
        only_the_flooded_class_is_blocked(MethodClass::Writes).await;
    }

    #[tokio::test(start_paused = true)]
    async fn the_21st_write_in_10_minutes_fails_fast_and_reads_go_on() {
        let limiter = AccountLimiter::new(LimitsConfig::default());
        let calls = AtomicU32::new(0);
        // 20 writes, one every 15 s: t = 0, 15, ..., 285.
        for _ in 0..20 {
            limiter
                .run(MethodClass::Writes, || ok(&calls))
                .await
                .unwrap();
            tokio::time::advance(Duration::from_secs(15)).await;
        }
        // t = 300: the first write leaves the 10 minutes at t = 600.
        let start = Instant::now();
        match limiter.run(MethodClass::Writes, || ok(&calls)).await {
            Err(TgError::WritesLimited {
                seconds: 300,
                limit: WriteLimit::Account,
            }) => {}
            other => panic!("expected the writes budget to refuse for 300 s, got {other:?}"),
        }
        assert_eq!(start.elapsed(), Duration::ZERO, "refused at once");
        assert_eq!(calls.load(Ordering::SeqCst), 20, "Telegram not asked");

        // Reading is not held back by the writes budget.
        for class in [
            MethodClass::Default,
            MethodClass::Search,
            MethodClass::Files,
        ] {
            limiter.run(class, || ok(&calls)).await.unwrap();
        }
        assert_eq!(start.elapsed(), Duration::ZERO, "reads don't wait");

        // Any 10 minutes, not fixed ones: at t = 600 one write fits again, and
        // the next waits for the t = 15 one to leave.
        tokio::time::advance(Duration::from_secs(300)).await;
        limiter
            .run(MethodClass::Writes, || ok(&calls))
            .await
            .unwrap();
        assert!(matches!(
            limiter.run(MethodClass::Writes, || ok(&calls)).await,
            Err(TgError::WritesLimited {
                seconds: 15,
                limit: WriteLimit::Account
            })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn the_6th_send_to_a_chat_in_a_minute_fails_while_another_chat_works() {
        let limiter = AccountLimiter::new(LimitsConfig::default());
        let calls = AtomicU32::new(0);
        let (a, b) = (ChatId::User(1), ChatId::Channel(1));
        for _ in 0..5 {
            limiter.run_send(a, || ok(&calls)).await.unwrap();
            tokio::time::advance(Duration::from_secs(2)).await;
        }
        // t = 10: the first send to `a` leaves the minute at t = 60.
        let start = Instant::now();
        match limiter.run_send(a, || ok(&calls)).await {
            Err(TgError::WritesLimited {
                seconds: 50,
                limit: WriteLimit::Chat,
            }) => {}
            other => panic!("expected the per-Chat cap to refuse for 50 s, got {other:?}"),
        }
        assert_eq!(start.elapsed(), Duration::ZERO, "never queued");
        assert_eq!(calls.load(Ordering::SeqCst), 5);

        limiter.run_send(b, || ok(&calls)).await.unwrap();
        // A reaction or marking read in `a` is a write, not a sent message.
        limiter
            .run(MethodClass::Writes, || ok(&calls))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 7);

        tokio::time::advance(Duration::from_secs(50)).await;
        limiter.run_send(a, || ok(&calls)).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_write_costs_nothing() {
        let limiter = AccountLimiter::new(LimitsConfig {
            writes_per_10_min: 6,
            ..LimitsConfig::default()
        });
        let calls = AtomicU32::new(0);
        let (a, b) = (ChatId::User(1), ChatId::User(2));
        for _ in 0..5 {
            limiter.run_send(a, || ok(&calls)).await.unwrap();
        }
        for _ in 0..3 {
            assert!(matches!(
                limiter.run_send(a, || ok(&calls)).await,
                Err(TgError::WritesLimited {
                    limit: WriteLimit::Chat,
                    ..
                })
            ));
        }
        // The refused sends to `a` didn't spend the writes budget: one is left.
        limiter.run_send(b, || ok(&calls)).await.unwrap();
        assert!(matches!(
            limiter.run_send(b, || ok(&calls)).await,
            Err(TgError::WritesLimited {
                limit: WriteLimit::Account,
                ..
            })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 6);
    }

    #[tokio::test(start_paused = true)]
    async fn writes_never_wait_for_the_shared_bucket() {
        let limiter = AccountLimiter::new(config(60, 1)); // one token per second
        let calls = AtomicU32::new(0);
        limiter
            .run(MethodClass::Default, || ok(&calls))
            .await
            .unwrap();
        let start = Instant::now();
        match limiter.run_send(ChatId::User(1), || ok(&calls)).await {
            Err(TgError::RateLimited { seconds: 1 }) => {}
            other => panic!("expected a 1 s wait, refused at once, got {other:?}"),
        }
        assert!(matches!(
            limiter.run(MethodClass::Writes, || ok(&calls)).await,
            Err(TgError::RateLimited { seconds: 1 })
        ));
        assert_eq!(start.elapsed(), Duration::ZERO, "a write never sleeps");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // A read in the same spot is paced, as before.
        limiter
            .run(MethodClass::Default, || ok(&calls))
            .await
            .unwrap();
        assert_eq!(start.elapsed(), Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn a_write_flood_is_never_retried_or_slept_through() {
        let limiter = AccountLimiter::new(LimitsConfig::default());
        let calls = AtomicU32::new(0);
        let start = Instant::now();
        // Short enough that a read would sleep through it and retry.
        match limiter
            .run_send(ChatId::User(1), || async {
                flood(&calls, 3, MethodClass::Writes)
            })
            .await
        {
            Err(TgError::FloodWait {
                seconds: 3,
                class: MethodClass::Writes,
            }) => {}
            other => panic!("expected FLOOD_WAIT 3 s of writes, got {other:?}"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "not retried");
        match limiter.run(MethodClass::Writes, || ok(&calls)).await {
            Err(TgError::FloodWait {
                seconds: 3,
                class: MethodClass::Writes,
            }) => {}
            other => panic!("expected the window to refuse at once, got {other:?}"),
        }
        assert_eq!(start.elapsed(), Duration::ZERO, "no write slept");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "Telegram not asked again");
        limiter
            .run(MethodClass::Default, || ok(&calls))
            .await
            .unwrap();

        tokio::time::advance(Duration::from_secs(3)).await;
        limiter
            .run(MethodClass::Writes, || ok(&calls))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_write_flood_wait_warning_names_the_method_too() {
        capture_logs();
        let limiter = AccountLimiter::new(LimitsConfig::default());
        let calls = AtomicU32::new(0);
        let drafted = limiter
            .run_named(MethodClass::Writes, "messages.saveDraft", || async {
                flood(&calls, 30, MethodClass::Writes)
            })
            .await;
        assert!(matches!(
            drafted,
            Err(TgError::FloodWait { seconds: 30, .. })
        ));
        // Another Account's limiter, so the Writes window above doesn't refuse it.
        let limiter = AccountLimiter::new(LimitsConfig::default());
        let sent = limiter
            .run_send_named(ChatId::User(1), "messages.sendMessage", || async {
                flood(&calls, 40, MethodClass::Writes)
            })
            .await;
        assert!(matches!(sent, Err(TgError::FloodWait { seconds: 40, .. })));
        let logs = captured_logs();
        assert!(
            logs.contains("seconds=30")
                && logs.contains("method=\"messages.saveDraft\"")
                && logs.contains("seconds=40")
                && logs.contains("method=\"messages.sendMessage\"")
                && logs.contains("class=\"writes\""),
            "{logs}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn peer_flood_is_returned_once_as_is() {
        let limiter = AccountLimiter::new(LimitsConfig::default());
        let calls = AtomicU32::new(0);
        let result = limiter
            .run_send(ChatId::User(1), || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(TgError::PeerFlood)
            })
            .await;
        assert!(matches!(result, Err(TgError::PeerFlood)), "{result:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "not retried");
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
        for write in [
            "messages.saveDraft",
            "messages.sendMessage",
            "messages.sendReaction",
            "messages.readHistory",
            "channels.readHistory",
            "messages.readDiscussion",
        ] {
            assert_eq!(MethodClass::of(write), MethodClass::Writes, "{write}");
            assert_eq!(is_send(write), write == "messages.sendMessage", "{write}");
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
