//! Every scenario and the harness's own guarantees, on the Fake world: the real
//! server, in-process, on a fake Telegram.

use acceptance::world::Who;
use acceptance::{
    fake::{FakeWorld, TEST_ACCOUNT},
    forms::Form,
    login::LoginFlow,
    run::{self, Outcome, Scenario},
    scenarios,
};
use serde_json::json;
use tg_mcp::tg::{Account, fake::FakeData};

#[tokio::test]
async fn every_scenario_passes_on_the_fake_world() {
    for scenario in scenarios::ALL {
        let world = FakeWorld::start().await.unwrap();
        let report = run::run(world.world.clone(), &[scenario]).await;
        assert_eq!(report.outcome(), Outcome::Pass, "{report}");
    }
}

#[tokio::test]
async fn the_login_waits_for_the_driver() {
    let world = FakeWorld::builder().idle_driver().start().await.unwrap();
    let report = run::run(world.world.clone(), &[scenarios::find("login").unwrap()]).await;
    let Outcome::Fail(why) = report.outcome() else {
        panic!("a login nobody accepted passed: {report}");
    };
    assert!(why.contains("still waiting"), "{why}");
}

/// Logs in with `draft` and writes a Draft: what the interlock must stop.
const WRITES: Scenario = Scenario {
    id: "writes",
    title: "writes a Draft",
    run: |run| {
        Box::pin(async move {
            let session = run.login(&["draft"]).await?;
            session
                .call(
                    "save_draft",
                    json!({ "chat_id": format!("user:{TEST_ACCOUNT}"), "text": "x" }),
                )
                .await?;
            Ok(())
        })
    },
};

#[tokio::test]
async fn another_account_is_stopped_before_any_write() {
    let stranger = Account {
        id: 42,
        first_name: "Not".into(),
        last_name: None,
        username: Some("not_the_test_account".into()),
    };
    let world = FakeWorld::builder()
        .login_as(FakeData::new(stranger))
        .start()
        .await
        .unwrap();
    let report = run::run(world.world.clone(), &[&WRITES]).await;
    let Outcome::Fail(why) = report.outcome() else {
        panic!("a stranger's Grant passed: {report}");
    };
    assert!(why.contains("interlock"), "{why}");
    assert!(why.contains("42"), "{why}");
    assert!(world.connector.drafts().of(42).is_empty());
    assert!(world.connector.drafts().of(TEST_ACCOUNT).is_empty());
}

#[tokio::test]
async fn the_test_account_writes_once_the_interlock_passed() {
    let world = FakeWorld::start().await.unwrap();
    let report = run::run(world.world.clone(), &[&WRITES]).await;
    assert_eq!(report.outcome(), Outcome::Pass, "{report}");
    assert_eq!(world.connector.drafts().of(TEST_ACCOUNT).len(), 1);
}

#[tokio::test]
async fn the_consent_form_is_read_from_the_servers_page() {
    let world = FakeWorld::start().await.unwrap();
    let mut flow = LoginFlow::start(&world.world, &["draft"]).await.unwrap();
    flow.wait_logged_in(&world.world, Who::TestAccount)
        .await
        .unwrap();
    let html = flow.consent_page().await.unwrap();
    let consent = Form::find(&html, &format!("{}/consent", flow.login_path)).unwrap();
    assert_eq!(consent.method, "post");
    let boxes = consent.checkboxes("scope");
    assert!(
        boxes.contains(&("draft".to_string(), true)),
        "draft was asked for, so it opens ticked: {boxes:?}"
    );
    assert!(
        boxes.contains(&("send".to_string(), false)),
        "send never opens ticked: {boxes:?}"
    );
    let chats = consent
        .controls
        .iter()
        .find(|c| c.formaction.is_some())
        .and_then(|c| c.formaction.clone());
    assert_eq!(
        chats,
        Some(format!("{}/consent/chats", flow.login_path)),
        "the Send allow-list pickers post to their own action"
    );
    // Unticking what was asked for grants read only.
    let code = flow.allow(&html, &[]).await.unwrap();
    assert_eq!(flow.token(&code).await.unwrap().scope, "read");
}

#[tokio::test]
async fn a_telegram_limit_blocks_the_run_and_nothing_after_it_runs() {
    let world = FakeWorld::builder().password_flood().start().await.unwrap();
    let report = run::run(
        world.world.clone(),
        &[
            scenarios::find("login").unwrap(),
            scenarios::find("login-peer").unwrap(),
        ],
    )
    .await;
    let Outcome::Blocked(why) = report.outcome() else {
        panic!("a FLOOD_WAIT didn't block: {report}");
    };
    assert!(why.contains("30 seconds"), "{why}");
    assert_eq!(report.scenarios.len(), 1, "{report}");
    assert_eq!(report.not_run, vec!["login-peer"]);
    let shown = report.to_string();
    assert!(shown.contains("BLOCKED"), "{shown}");
    assert!(shown.contains("not a bug in the code"), "{shown}");
}

#[tokio::test]
async fn a_driver_on_another_account_stops_the_run_before_anything() {
    let world = FakeWorld::builder().driver_is(42).start().await.unwrap();
    let report = run::run(world.world.clone(), &[&WRITES]).await;
    let Outcome::Fail(why) = report.outcome() else {
        panic!("a Driver on another account passed: {report}");
    };
    assert!(why.contains("interlock: the Driver"), "{why}");
    assert!(report.scenarios.is_empty());
    assert_eq!(report.not_run, vec!["writes"]);
    assert!(world.connector.drafts().of(42).is_empty());
    assert!(world.connector.drafts().of(TEST_ACCOUNT).is_empty());
}

#[tokio::test]
async fn every_grant_is_revoked_when_its_scenario_ends() {
    let world = FakeWorld::start().await.unwrap();
    let report = run::run(world.world.clone(), &[scenarios::find("login").unwrap()]).await;
    assert_eq!(report.outcome(), Outcome::Pass, "{report}");
    assert!(
        world.connector.logged_out().contains(&TEST_ACCOUNT),
        "the Grant's Telegram session was logged out by /revoke: {:?}",
        world.connector.logged_out()
    );
}

#[tokio::test]
async fn another_runs_live_lease_keeps_a_run_from_starting() {
    let world = FakeWorld::start().await.unwrap();
    let until = acceptance::lease::unix_now() + 600;
    world
        .world
        .driver
        .set_saved_draft(&format!("tgmcp-accept lease other1 until {until}"))
        .await
        .unwrap();
    let report = run::run(world.world.clone(), &[scenarios::find("login").unwrap()]).await;
    let Outcome::Fail(why) = report.outcome() else {
        panic!("a run started under another run's lease: {report}");
    };
    assert!(why.contains("lease held by run other1"), "{why}");
    assert!(why.contains(&until.to_string()), "{why}");
    assert!(report.scenarios.is_empty());
    assert_eq!(
        world.world.driver.saved_draft().await.unwrap(),
        Some(format!("tgmcp-accept lease other1 until {until}")),
        "another run's lease is left alone"
    );
}

#[tokio::test]
async fn an_expired_lease_is_taken_over_and_released_at_the_end() {
    let world = FakeWorld::start().await.unwrap();
    let past = acceptance::lease::unix_now() - 1;
    world
        .world
        .driver
        .set_saved_draft(&format!("tgmcp-accept lease crashed until {past}"))
        .await
        .unwrap();
    let report = run::run(world.world.clone(), &[scenarios::find("login").unwrap()]).await;
    assert_eq!(report.outcome(), Outcome::Pass, "{report}");
    assert_eq!(
        world.world.driver.saved_draft().await.unwrap(),
        None,
        "the run's own lease is cleared when it ends"
    );
}

#[tokio::test]
async fn a_failed_run_releases_its_lease_too() {
    let world = FakeWorld::builder().idle_driver().start().await.unwrap();
    let report = run::run(world.world.clone(), &[scenarios::find("login").unwrap()]).await;
    assert!(matches!(report.outcome(), Outcome::Fail(_)), "{report}");
    assert_eq!(world.world.driver.saved_draft().await.unwrap(), None);
}

/// Writes one marked and one foreign Draft, then finds only its own.
const MARKS: Scenario = Scenario {
    id: "marks",
    title: "marks what it writes",
    run: |run| {
        Box::pin(async move {
            let session = run.login(&["draft"]).await?;
            let mine = run.mark("hello");
            anyhow::ensure!(mine.starts_with("hello [tgmcp-accept "), "{mine}");
            let chat = format!("user:{TEST_ACCOUNT}");
            session
                .call(
                    "save_draft",
                    json!({ "chat_id": chat, "text": mine, "format": "plain" }),
                )
                .await?;
            let foreign = "left over [tgmcp-accept someoneelse]";
            let texts = [mine.as_str(), foreign, "typed by hand"];
            anyhow::ensure!(run.ours(texts) == vec![mine.as_str()], "{texts:?}");
            Ok(())
        })
    },
};

#[tokio::test]
async fn a_run_marks_what_it_writes_and_sees_only_its_own() {
    let world = FakeWorld::start().await.unwrap();
    let report = run::run(world.world.clone(), &[&MARKS]).await;
    assert_eq!(report.outcome(), Outcome::Pass, "{report}");
    let drafts = world.connector.drafts().of(TEST_ACCOUNT);
    assert!(
        drafts
            .iter()
            .any(|d| acceptance::run::is_from_run(&d.text.text, &report.run_id)),
        "{drafts:?}"
    );
}

/// Resolves every fixture role through the server.
const ROLES: Scenario = Scenario {
    id: "roles",
    title: "finds every fixture",
    run: |run| {
        Box::pin(async move {
            let session = run.login(&[]).await?;
            let found =
                acceptance::fixtures::Fixtures::resolve(&session, run.world.peer_account).await?;
            anyhow::ensure!(found.a.starts_with("user:7000002"), "{found:?}");
            anyhow::ensure!(found.b.starts_with("chat:9001"), "{found:?}");
            anyhow::ensure!(found.forum.starts_with("channel:9002"), "{found:?}");
            anyhow::ensure!(found.releases == 10, "{found:?}");
            anyhow::ensure!(found.g.starts_with("chat:9003"), "{found:?}");
            anyhow::ensure!(found.work == 2, "{found:?}");
            anyhow::ensure!(found.saved.starts_with("user:7000001"), "{found:?}");
            Ok(())
        })
    },
};

#[tokio::test]
async fn every_fixture_role_resolves_on_the_fake_world() {
    let world = FakeWorld::start().await.unwrap();
    let report = run::run(world.world.clone(), &[&ROLES]).await;
    assert_eq!(report.outcome(), Outcome::Pass, "{report}");
}

#[tokio::test]
async fn a_missing_fixture_says_how_to_make_it() {
    let world = FakeWorld::builder()
        .login_as(FakeData::new(acceptance::fake::test_account()))
        .start()
        .await
        .unwrap();
    let report = run::run(world.world.clone(), &[&ROLES]).await;
    let Outcome::Fail(why) = report.outcome() else {
        panic!("no fixtures, yet: {report}");
    };
    assert!(why.contains("cargo accept fixtures"), "{why}");
}

/// Writes a marked Draft, then plants its own text in the server's log.
const LOG_LEAK: Scenario = Scenario {
    id: "log-leak",
    title: "a run text in the log",
    run: |run| {
        Box::pin(async move {
            run.login(&["draft"]).await?;
            let text = run.mark("a secret line nobody should log");
            if let Some(observed) = &run.world.observed {
                observed
                    .log
                    .append(format!("INFO draft saved text={text}\n").as_bytes());
            }
            Ok(())
        })
    },
};

#[tokio::test]
async fn a_run_text_in_the_logs_fails_the_run_without_showing_it() {
    let world = FakeWorld::start().await.unwrap();
    let report = run::run(world.world.clone(), &[&LOG_LEAK]).await;
    let Outcome::Fail(why) = report.outcome() else {
        panic!("a leak in the logs passed: {report}");
    };
    assert!(why.contains("server logs: nothing the run wrote"), "{why}");
    let shown = report.to_string();
    assert!(
        !shown.contains("nobody should log"),
        "the report shows the text: {shown}"
    );
}

/// Plants its marker in the database file.
const DB_LEAK: Scenario = Scenario {
    id: "db-leak",
    title: "a run text in the database",
    run: |run| {
        Box::pin(async move {
            let text = run.mark("x");
            if let Some(observed) = &run.world.observed {
                use std::io::Write;
                let mut db = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&observed.db)?;
                db.write_all(text.as_bytes())?;
            }
            Ok(())
        })
    },
};

#[tokio::test]
async fn a_run_text_in_the_database_fails_the_run() {
    let world = FakeWorld::start().await.unwrap();
    let report = run::run(world.world.clone(), &[&DB_LEAK]).await;
    let Outcome::Fail(why) = report.outcome() else {
        panic!("a leak in the database passed: {report}");
    };
    assert!(why.contains("database: nothing the run wrote"), "{why}");
}

#[tokio::test]
async fn a_clean_run_lists_its_invariants_as_passed() {
    let world = FakeWorld::start().await.unwrap();
    let report = run::run(world.world.clone(), &[&MARKS]).await;
    assert_eq!(report.outcome(), Outcome::Pass, "{report}");
    assert_eq!(report.checks.len(), 4, "{report}");
    assert!(
        report
            .checks
            .iter()
            .all(|(_, c)| *c == acceptance::invariants::Check::Pass),
        "{report}"
    );
    let log = world.world.observed.as_ref().unwrap().log.read().unwrap();
    let log = String::from_utf8_lossy(&log);
    assert!(
        log.contains("draft saved"),
        "the server's own log lines are captured, so the check reads something"
    );
}
