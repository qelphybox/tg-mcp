//! What a client sees when Telegram (or our budget) says "wait".
//! The limiter's timing rules are unit-tested in src/limits.rs.

mod common;

use common::{account, start_with};
use rmcp::model::CallToolRequestParams;
use tg_mcp::tg::fake::FakeData;

#[tokio::test]
async fn flood_wait_reaches_the_model_with_the_seconds_to_wait() {
    let mut flooded = FakeData::new(account(42, "Alice"));
    flooded.flood_wait = Some(120);
    let server = start_with(vec![flooded, FakeData::new(account(7, "Bob"))]).await;
    let (_, alice) = server.login().await;
    let (_, bob) = server.login().await;

    let client = server.mcp(alice["access_token"].as_str().unwrap()).await;
    let err = client
        .call_tool(CallToolRequestParams::new("list_chats"))
        .await
        .expect_err("a flooded account can't list chats");
    let message = err.to_string();
    assert!(
        message.contains("wait 120 s") && message.contains("FLOOD_WAIT"),
        "{message}"
    );

    // Another account on the same server is unaffected.
    let client = server.mcp(bob["access_token"].as_str().unwrap()).await;
    client
        .call_tool(CallToolRequestParams::new("list_chats"))
        .await
        .unwrap();
}
