//! Every scenario and the harness's own guarantees, on the Fake world: the real
//! server, in-process, on a fake Telegram.

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
        let report = run::run(world.world.clone(), "fake", &[scenario]).await;
        assert_eq!(report.outcome(), Outcome::Pass, "{report}");
    }
}

#[tokio::test]
async fn the_login_waits_for_the_driver() {
    let world = FakeWorld::builder().idle_driver().start().await.unwrap();
    let report = run::run(
        world.world.clone(),
        "fake",
        &[scenarios::find("login").unwrap()],
    )
    .await;
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
    let report = run::run(world.world.clone(), "fake", &[&WRITES]).await;
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
    let report = run::run(world.world.clone(), "fake", &[&WRITES]).await;
    assert_eq!(report.outcome(), Outcome::Pass, "{report}");
    assert_eq!(world.connector.drafts().of(TEST_ACCOUNT).len(), 1);
}

#[tokio::test]
async fn the_consent_form_is_read_from_the_servers_page() {
    let world = FakeWorld::start().await.unwrap();
    let mut flow = LoginFlow::start(&world.world, &["draft"]).await.unwrap();
    flow.wait_logged_in(&world.world).await.unwrap();
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
