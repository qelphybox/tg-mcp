//! `smoke`: after a deploy, the hosted server end to end on the Test account
//! (`cargo accept --target https://connector.tgmcp.dev smoke`). No Human
//! checks; the logs and database checks are skipped, production's being out
//! of reach.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde_json::json;

use crate::{
    lease::unix_now,
    run::{Run, Scenario},
    world::Who,
};

pub const SCENARIO: Scenario = Scenario {
    id: "smoke",
    title: "Login, read, a Draft and a message in A, revoke: the deployed server works",
    run: |run| Box::pin(smoke(run)),
};

async fn smoke(run: &mut Run) -> Result<()> {
    let since = unix_now() - 60;
    let a_prefix = format!("user:{}", run.world.peer_account);
    let session = run
        .login_sending(Who::TestAccount, &["draft", "send"], &[&a_prefix])
        .await?;
    ensure!(
        session.grant.scope == "read draft send",
        "Consent granted `{}`",
        session.grant.scope
    );

    let chats = session.call("list_chats", json!({ "limit": 50 })).await?;
    let a = chats["chats"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["id"].as_str())
        .find(|id| id.starts_with(&format!("{a_prefix}~")) || *id == a_prefix)
        .context("A (the chat with the Peer) is not among the 50 latest chats: run `cargo accept fixtures`")?
        .to_string();
    run.passed("list_chats answers, with A among the chats");

    let draft = run.mark("smoke draft");
    session
        .call(
            "save_draft",
            json!({ "chat_id": a, "text": draft, "format": "plain" }),
        )
        .await?;
    let held = run.world.driver.draft(&a, None).await?;
    ensure!(
        held.as_ref().map(|d| d.text.as_str()) == Some(draft.as_str()),
        "Telegram holds {} in A",
        held.as_ref()
            .map(|d| run.describe(&d.text))
            .unwrap_or_else(|| "no Draft".into())
    );
    session
        .call("save_draft", json!({ "chat_id": a, "text": "" }))
        .await?;
    run.passed("save_draft: Telegram holds the Draft in A; cleared again");

    let text = run.mark("smoke message");
    let sent = session
        .call(
            "send_message",
            json!({ "chat_id": a, "text": text, "format": "plain" }),
        )
        .await?;
    ensure!(sent["status"] == "sent", "send_message answered {sent}");
    let id = sent["message_id"]
        .as_i64()
        .context("send_message gave no message_id")?;
    let mut seen = false;
    for _ in 0..10 {
        if run
            .world
            .peer
            .received_from_test_account(run.world.test_account, &text)
            .await?
        {
            seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    run.world.driver.delete_message(&a, id).await?;
    ensure!(seen, "the Peer never got message {id}");
    run.passed("send_message: the Peer got the message (deleted again)");

    run.revoke_all().await?;
    let mut left = usize::MAX;
    for _ in 0..20 {
        left = run.world.driver.server_sessions_since(since).await?;
        if left == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    ensure!(
        left == 0,
        "{left} \"Unofficial MCP Server\" session(s) of this run are still in Telegram after /revoke"
    );
    run.passed("/revoke: the Test account's session of the run is gone from Telegram");
    Ok(())
}
