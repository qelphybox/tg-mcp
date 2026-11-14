//! The hourly `stats` log line (#64): its counts, its reset, and that it names no
//! chat, Account or Grant.

mod common;

use std::{
    io::Write,
    sync::{Arc, Mutex},
    time::Duration,
};

use common::{account, start_with};
use rmcp::model::CallToolRequestParams;
use serde_json::json;
use tg_mcp::{
    limits::MethodClass,
    stats::{self, Gauges, Refusal, Stats, ToolError},
    tg::{
        chats::{ChatKind, ChatSummary, MessagePreview},
        fake::FakeData,
    },
};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Sink {
    /// The `stats` lines logged so far.
    fn stats_lines(&self) -> Vec<String> {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .filter(|l| l.contains("tg_mcp::stats") && l.contains(" stats "))
            .map(str::to_string)
            .collect()
    }
}

/// Logs of this test only: the runtime is current-thread, so everything it runs
/// logs through this thread's default subscriber.
fn capture() -> (Sink, tracing::subscriber::DefaultGuard) {
    let sink = Sink::default();
    let writer = sink.clone();
    let guard = tracing_subscriber::fmt()
        .with_env_filter("info,tg_mcp=trace,rmcp=info")
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .set_default();
    (sink, guard)
}

/// Let the reporter task run what is due.
async fn settle() {
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn after_an_hour_the_line_carries_the_counts_then_they_reset() {
    let (logs, _guard) = capture();
    let stats: &'static Stats = Box::leak(Box::new(Stats::new()));
    let shutdown = CancellationToken::new();
    let gauges = || async {
        Gauges {
            grants: Some(7),
            live_clients: 3,
            login_flows: 1,
            media_links: 2,
        }
    };
    let reporter = tokio::spawn(stats::report(
        stats,
        stats::PERIOD,
        gauges,
        shutdown.clone(),
    ));
    settle().await;

    stats.tool_call("list_chats");
    stats.tool_call("list_chats");
    stats.tool_call("get_messages");
    stats.tool_error(ToolError::Internal);
    stats.tool_error(ToolError::Scope);
    stats.flood_wait(MethodClass::Search, 30);
    stats.flood_wait(MethodClass::Search, 12);
    stats.flood_wait(MethodClass::Files, 4);
    stats.refused(Refusal::RequestBudget);
    stats.refused(Refusal::OAuth);
    stats.refused(Refusal::OAuth);
    stats.login();
    stats.revocation();

    tokio::time::advance(stats::PERIOD - Duration::from_secs(1)).await;
    settle().await;
    assert!(logs.stats_lines().is_empty(), "nothing before the hour");

    tokio::time::advance(Duration::from_secs(1)).await;
    settle().await;
    let lines = logs.stats_lines();
    assert_eq!(lines.len(), 1, "{lines:#?}");
    let line = &lines[0];
    for field in [
        "reason=\"hourly\"",
        "grants=7",
        "live_clients=3",
        "login_flows=1",
        "media_links=2",
        "tool_calls=get_messages:1 list_chats:2",
        "tool_errors=scope:1 internal:1",
        "flood_waits=search:2/30s files:1/4s",
        "refused=request_budget:1 oauth:2",
        "logins=1",
        "revocations=1",
    ] {
        assert!(line.contains(field), "{field} missing in {line}");
    }

    // The next hour starts from zero.
    stats.tool_call("find_chats");
    tokio::time::advance(stats::PERIOD).await;
    settle().await;
    let lines = logs.stats_lines();
    assert_eq!(lines.len(), 2, "{lines:#?}");
    for field in [
        "tool_calls=find_chats:1",
        "tool_errors=-",
        "flood_waits=-",
        "refused=-",
        "logins=0",
        "revocations=0",
    ] {
        assert!(lines[1].contains(field), "{field} missing in {}", lines[1]);
    }

    // Shutdown logs what the hour had so far, then the reporter ends.
    stats.revocation();
    shutdown.cancel();
    reporter.await.unwrap();
    let lines = logs.stats_lines();
    assert_eq!(lines.len(), 3, "{lines:#?}");
    assert!(lines[2].contains("reason=\"shutdown\""), "{}", lines[2]);
    assert!(lines[2].contains("revocations=1"), "{}", lines[2]);
}

const TITLE: &str = "Zebrafinch Supper Club";
const OWNER: &str = "Quillon Marsh";
const ACCOUNT_ID: i64 = 918_273_645;

fn chat() -> ChatSummary {
    ChatSummary {
        id: "user:5551234".into(),
        title: TITLE.into(),
        kind: ChatKind::Private,
        username: Some("zebrafinch_supper".into()),
        is_self: false,
        unread_count: 1,
        pinned: false,
        muted: false,
        marked_unread: false,
        last_message: Some(MessagePreview {
            date: "2026-11-06T12:00:00Z".into(),
            text: "meet at the zebrafinch".into(),
            media: None,
        }),
    }
}

#[tokio::test]
async fn the_line_counts_tool_calls_by_name_and_names_no_chat_account_or_grant() {
    let (logs, _guard) = capture();
    let server = start_with(vec![
        FakeData::new(account(ACCOUNT_ID, OWNER)).with_chats(vec![chat()]),
    ])
    .await;
    let (client_id, tokens) = server.login().await;
    let access = tokens["access_token"].as_str().unwrap().to_string();
    let client = server.mcp(&access).await;

    for _ in 0..2 {
        let listed = client
            .call_tool(
                CallToolRequestParams::new("list_chats")
                    .with_arguments(json!({}).as_object().unwrap().clone()),
            )
            .await
            .unwrap();
        assert!(
            serde_json::to_string(&listed.structured_content)
                .unwrap()
                .contains(TITLE)
        );
    }
    let unknown = client
        .call_tool(CallToolRequestParams::new("zebrafinch_secret_tool"))
        .await;
    assert!(unknown.is_err());

    // Revoked by its client: one revocation.
    let revoked = server
        .post_form(
            "/revoke",
            &[
                ("token", tokens["refresh_token"].as_str().unwrap()),
                ("token_type_hint", "refresh_token"),
                ("client_id", &client_id),
            ],
            None,
        )
        .await;
    assert!(revoked.status().is_success(), "{}", revoked.status());

    // This binary's other test counts into its own Stats, so the global ones
    // hold this test's calls only.
    stats::log(&stats::global().take(), &Gauges::default(), "hourly");
    let lines = logs.stats_lines();
    assert_eq!(lines.len(), 1, "{lines:#?}");
    let line = &lines[0];
    assert!(line.contains("tool_calls=list_chats:2"), "{line}");
    assert!(line.contains("unknown_tool:1"), "{line}");
    assert!(line.contains("logins=1"), "{line}");
    assert!(line.contains("revocations=1"), "{line}");
    let lower = line.to_lowercase();
    for needle in [
        "zebrafinch",
        "supper",
        "quillon",
        "marsh",
        "5551234",
        &ACCOUNT_ID.to_string(),
        &access,
    ] {
        assert!(
            !lower.contains(&needle.to_lowercase()),
            "{needle} in {line}"
        );
    }
    // Nor a Grant id: the server logged one when the Grant was created.
    let all = String::from_utf8_lossy(&logs.0.lock().unwrap()).into_owned();
    let grant_id = all
        .split("grant_id=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .expect("a grant id in the logs")
        .to_string();
    assert!(!line.contains(&grant_id), "{grant_id} in {line}");
}
