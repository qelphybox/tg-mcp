//! `login`: the Login flow end to end, the Driver accepting the QR, and
//! `whoami` answering as the Test account (acceptance §1, without the stopwatch).

use anyhow::{Result, ensure};
use serde_json::json;

use crate::run::{Run, Scenario};

pub const SCENARIO: Scenario = Scenario {
    id: "login",
    title: "Log in through the QR and Consent, whoami is the Test account",
    run: |run| Box::pin(login(run)),
};

async fn login(run: &mut Run) -> Result<()> {
    let session = run.login(&[]).await?;
    ensure!(
        session.grant.scope == "read",
        "a read-only Consent granted `{}`",
        session.grant.scope
    );
    let tools = session.tools().await?;
    ensure!(
        tools.iter().any(|t| t == "whoami") && !tools.iter().any(|t| t == "save_draft"),
        "a read-only Grant lists {tools:?}"
    );
    run.passed("a read-only Grant lists whoami and no write tool");
    let chats = session.call("list_chats", json!({ "limit": 3 })).await?;
    ensure!(
        chats["chats"].is_array(),
        "list_chats answered without a chat list"
    );
    run.passed("list_chats answers");
    Ok(())
}
