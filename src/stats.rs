//! The hourly `stats` log line: how the service is doing, without a metrics stack.
//!
//! Once an hour, and once on graceful shutdown, one `info` event `stats` (target
//! `tg_mcp::stats`) carries numbers only:
//!
//! - now: Grants in the database (`grants`, left out if the database couldn't be
//!   read), Live clients in the pool
//!   (`live_clients`), pending Login flows (`login_flows`), live Media links
//!   (`media_links`);
//! - over the hour since the last line: tool calls by tool name (`tool_calls`), tool
//!   errors by kind (`tool_errors`), Telegram's `FLOOD_WAIT`s per method class with
//!   the longest wait (`flood_waits`, `class:count/max_s`), our own limits' refusals
//!   by limit (`refused`), Logins completed (`logins`) and Grants revoked
//!   (`revocations`).
//!
//! The counters live in memory, reset with each line and are lost on restart: the
//! first line after a deploy covers less than an hour. No user, Account, Grant, chat
//! or message identifiers and no titles: tool names, error kinds, method classes and
//! limits are fixed names of ours, and an unknown tool counts as `unknown`.

use std::{collections::BTreeMap, fmt::Write as _, future::Future, sync::Mutex, time::Duration};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::limits::MethodClass;

/// How often the line is logged.
pub const PERIOD: Duration = Duration::from_secs(60 * 60);

/// What a failed tool call is counted as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ToolError {
    /// The tool exists but the Grant's Scopes don't allow it.
    Scope,
    /// Arguments the tool refused (JSON-RPC `invalid_params`).
    InvalidParams,
    /// Telegram, our limits or the server failed it (JSON-RPC `internal_error`).
    Internal,
    /// No tool of that name.
    UnknownTool,
    /// Any other JSON-RPC error.
    Other,
}

impl ToolError {
    pub fn name(self) -> &'static str {
        match self {
            ToolError::Scope => "scope",
            ToolError::InvalidParams => "invalid_params",
            ToolError::Internal => "internal",
            ToolError::UnknownTool => "unknown_tool",
            ToolError::Other => "other",
        }
    }
}

/// Which of our own limits refused a request (ADR-0005 and the OAuth endpoints').
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Refusal {
    /// The Account's request budget.
    Account,
    /// The Account's Writes budget.
    Writes,
    /// A Chat's Send cap.
    ChatSends,
    /// The per-client limit of an OAuth endpoint (`429`).
    OAuth,
}

impl Refusal {
    pub fn name(self) -> &'static str {
        match self {
            Refusal::Account => "account",
            Refusal::Writes => "writes",
            Refusal::ChatSends => "chat_sends",
            Refusal::OAuth => "oauth",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Floods {
    count: u64,
    max_seconds: u32,
}

/// The counters of one hour.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Hour {
    tool_calls: BTreeMap<String, u64>,
    tool_errors: BTreeMap<ToolError, u64>,
    flood_waits: [Floods; MethodClass::ALL.len()],
    refused: BTreeMap<Refusal, u64>,
    logins: u64,
    revocations: u64,
}

impl Hour {
    const fn new() -> Self {
        Self {
            tool_calls: BTreeMap::new(),
            tool_errors: BTreeMap::new(),
            flood_waits: [Floods {
                count: 0,
                max_seconds: 0,
            }; MethodClass::ALL.len()],
            refused: BTreeMap::new(),
            logins: 0,
            revocations: 0,
        }
    }

    fn tool_calls(&self) -> String {
        pairs(
            self.tool_calls
                .iter()
                .map(|(k, v)| (k.as_str(), v.to_string())),
        )
    }

    fn tool_errors(&self) -> String {
        pairs(
            self.tool_errors
                .iter()
                .map(|(k, v)| (k.name(), v.to_string())),
        )
    }

    fn flood_waits(&self) -> String {
        pairs(
            MethodClass::ALL
                .iter()
                .zip(&self.flood_waits)
                .filter(|(_, f)| f.count > 0)
                .map(|(class, f)| (class.name(), format!("{}/{}s", f.count, f.max_seconds))),
        )
    }

    fn refused(&self) -> String {
        pairs(self.refused.iter().map(|(k, v)| (k.name(), v.to_string())))
    }
}

/// `a:1 b:2`, or `-` when there is nothing, so the field is never empty.
fn pairs<'a>(items: impl Iterator<Item = (&'a str, String)>) -> String {
    let mut out = String::new();
    for (key, value) in items {
        if !out.is_empty() {
            out.push(' ');
        }
        let _ = write!(out, "{key}:{value}");
    }
    if out.is_empty() {
        out.push('-');
    }
    out
}

/// The current hour's counters. The process has one ([`global`]); tests make their own.
pub struct Stats {
    hour: Mutex<Hour>,
}

impl Default for Stats {
    fn default() -> Self {
        Self::new()
    }
}

impl Stats {
    pub const fn new() -> Self {
        Self {
            hour: Mutex::new(Hour::new()),
        }
    }

    fn with(&self, f: impl FnOnce(&mut Hour)) {
        // Counting must never take a request down: a poisoned lock still counts.
        let mut hour = self.hour.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut hour);
    }

    /// One call of tool `tool`: only a name from the server's own tool list, never
    /// an unchecked caller's string (an unknown tool is a [`ToolError::UnknownTool`]).
    pub fn tool_call(&self, tool: &str) {
        self.with(|h| match h.tool_calls.get_mut(tool) {
            Some(n) => *n += 1,
            None => {
                h.tool_calls.insert(tool.to_string(), 1);
            }
        });
    }

    pub fn tool_error(&self, kind: ToolError) {
        self.with(|h| *h.tool_errors.entry(kind).or_default() += 1);
    }

    /// Telegram answered a request of `class` with `FLOOD_WAIT` of `seconds`.
    pub fn flood_wait(&self, class: MethodClass, seconds: u32) {
        self.with(|h| {
            let f = &mut h.flood_waits[class as usize];
            f.count += 1;
            f.max_seconds = f.max_seconds.max(seconds);
        });
    }

    pub fn refused(&self, by: Refusal) {
        self.with(|h| *h.refused.entry(by).or_default() += 1);
    }

    /// A Login flow ended in a new Grant.
    pub fn login(&self) {
        self.with(|h| h.logins += 1);
    }

    /// A Grant was revoked (by its client, or for a reused refresh Token).
    pub fn revocation(&self) {
        self.with(|h| h.revocations += 1);
    }

    /// The hour's counters, leaving them at zero.
    pub fn take(&self) -> Hour {
        let mut hour = self.hour.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *hour)
    }
}

static GLOBAL: Stats = Stats::new();

/// The process's counters, which the server's code counts into.
pub fn global() -> &'static Stats {
    &GLOBAL
}

/// What exists right now, read when the line is logged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Gauges {
    /// `None` when the database couldn't be read.
    pub grants: Option<u64>,
    pub live_clients: usize,
    pub login_flows: usize,
    pub media_links: usize,
}

/// Log one `stats` line for `hour` and `gauges`. `reason` is `hourly` or `shutdown`.
pub fn log(hour: &Hour, gauges: &Gauges, reason: &'static str) {
    tracing::info!(
        target: "tg_mcp::stats",
        reason,
        grants = gauges.grants,
        live_clients = gauges.live_clients,
        login_flows = gauges.login_flows,
        media_links = gauges.media_links,
        tool_calls = %hour.tool_calls(),
        tool_errors = %hour.tool_errors(),
        flood_waits = %hour.flood_waits(),
        refused = %hour.refused(),
        logins = hour.logins,
        revocations = hour.revocations,
        "stats"
    );
}

/// Every `period`, log the line for `stats` and reset it; once more when `shutdown`
/// is cancelled, then return.
pub async fn report<F, Fut>(stats: &Stats, period: Duration, gauges: F, shutdown: CancellationToken)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Gauges>,
{
    let mut every = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = every.tick() => log(&stats.take(), &gauges().await, "hourly"),
            () = shutdown.cancelled() => {
                log(&stats.take(), &gauges().await, "shutdown");
                return;
            }
        }
    }
}

static REPORTER: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

/// Run [`report`] on the [`global`] counters in the background, until `shutdown`.
/// [`finish`] waits for its last line.
pub fn spawn<F, Fut>(gauges: F, shutdown: CancellationToken)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = Gauges> + Send,
{
    let handle = tokio::spawn(async move { report(global(), PERIOD, gauges, shutdown).await });
    let previous = REPORTER
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace(handle);
    if let Some(previous) = previous {
        previous.abort();
    }
}

/// After the server's `shutdown` token was cancelled: wait, at most `timeout`, for
/// the last `stats` line, so it is logged before the process exits.
pub async fn finish(timeout: Duration) {
    let handle = REPORTER.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(handle) = handle
        && tokio::time::timeout(timeout, handle).await.is_err()
    {
        tracing::warn!("the last stats line didn't make it before shutdown");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_resets() {
        let stats = Stats::new();
        stats.tool_call("list_chats");
        stats.tool_call("list_chats");
        stats.tool_call("get_messages");
        stats.tool_error(ToolError::Internal);
        stats.flood_wait(MethodClass::Search, 30);
        stats.flood_wait(MethodClass::Search, 7);
        stats.flood_wait(MethodClass::Default, 2);
        stats.refused(Refusal::OAuth);
        stats.login();
        stats.revocation();
        stats.revocation();

        let hour = stats.take();
        assert_eq!(hour.tool_calls(), "get_messages:1 list_chats:2");
        assert_eq!(hour.tool_errors(), "internal:1");
        assert_eq!(hour.flood_waits(), "default:1/2s search:2/30s");
        assert_eq!(hour.refused(), "oauth:1");
        assert_eq!((hour.logins, hour.revocations), (1, 2));

        let next = stats.take();
        assert_eq!(next, Hour::default());
        assert_eq!(next.tool_calls(), "-");
        assert_eq!(next.flood_waits(), "-");
    }

    #[test]
    fn every_method_class_has_its_own_slot() {
        let stats = Stats::new();
        for (i, class) in MethodClass::ALL.iter().enumerate() {
            stats.flood_wait(*class, i as u32 + 1);
        }
        let hour = stats.take();
        for (i, class) in MethodClass::ALL.iter().enumerate() {
            assert_eq!(hour.flood_waits[*class as usize].max_seconds, i as u32 + 1);
        }
    }
}
