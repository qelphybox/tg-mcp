//! Drives the server through its real HTTP surface with rmcp's own client.

use std::sync::Arc;

use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, ClientConfig},
    transport::StreamableHttpClientTransport,
};
use serde_json::json;
use tg_mcp::{
    config::Config,
    tg::{Account, FakeTelegram},
};
use tokio_util::sync::CancellationToken;

struct TestServer {
    base: String,
    shutdown: CancellationToken,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

async fn start(account: Account) -> TestServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let config = Config::from_lookup(|key| (key == "TG_MCP_PUBLIC_URL").then(|| base.clone()))
        .expect("config");
    let shutdown = CancellationToken::new();
    let app = tg_mcp::app(
        &config,
        Arc::new(FakeTelegram::new(account)),
        shutdown.clone(),
    );
    let stop = shutdown.clone();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(stop.cancelled_owned())
            .await
            .unwrap();
    });
    TestServer { base, shutdown }
}

fn alice() -> Account {
    Account {
        id: 42,
        first_name: "Alice".into(),
        last_name: None,
        username: Some("alice".into()),
    }
}

#[tokio::test]
async fn healthz_is_ok() {
    let server = start(alice()).await;
    let res = reqwest::get(format!("{}/healthz", server.base))
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
}

#[tokio::test]
async fn whoami_over_streamable_http() {
    let server = start(alice()).await;
    let transport = StreamableHttpClientTransport::from_uri(format!("{}/mcp", server.base));
    let client = ClientConfig::default()
        .serve(transport)
        .await
        .expect("initialize");

    let info = client.peer_info().expect("server info");
    assert_eq!(
        info.server_info.as_ref().map(|s| s.name.as_str()),
        Some("tg-mcp")
    );

    let tools = client.list_all_tools().await.expect("tools/list");
    let names: Vec<_> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(names, ["whoami"]);
    let annotations = tools[0].annotations.as_ref().expect("annotations");
    assert_eq!(annotations.read_only_hint, Some(true));
    assert!(
        tools[0].output_schema.is_some(),
        "whoami declares its output"
    );

    let result = client
        .call_tool(CallToolRequestParams::new("whoami"))
        .await
        .expect("tools/call");
    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        result.structured_content,
        Some(json!({"id": 42, "first_name": "Alice", "username": "alice"}))
    );

    client.cancel().await.unwrap();
}
