//! Which Scope each tool needs (ADR-0005). A tool's Scope is the router it is
//! declared in: [`super::TgMcp::scoped_tools`] puts each `#[tool_router(router = …)]`
//! block under one Scope, so a new tool gets its Scope from the block it is
//! written in and is never listed or called without it.

use std::collections::HashMap;

use rmcp::{ErrorData, handler::server::router::tool::ToolRouter, model::Tool};

use crate::scope::{Scope, Scopes};

/// Every tool, with the Scope a Grant needs to see it in `tools/list` and to call it.
pub struct ScopedTools<S> {
    router: ToolRouter<S>,
    required: HashMap<String, Scope>,
}

impl<S: Send + Sync + 'static> ScopedTools<S> {
    pub fn new() -> Self {
        Self {
            router: ToolRouter::new(),
            required: HashMap::new(),
        }
    }

    /// Add every tool of `router` under `scope`. Panics on a tool name that is
    /// already there: one tool, one Scope.
    pub fn with(mut self, scope: Scope, router: ToolRouter<S>) -> Self {
        for tool in router.list_all() {
            let name = tool.name.to_string();
            assert!(
                self.required.insert(name.clone(), scope).is_none(),
                "tool {name} is declared under two Scopes"
            );
        }
        self.router.merge(router);
        self
    }

    /// The tools a Grant with `scopes` may use, sorted by name.
    pub fn listed(&self, scopes: Scopes) -> Vec<Tool> {
        self.router
            .list_all()
            .into_iter()
            .filter(|tool| self.allows(&tool.name, scopes))
            .collect()
    }

    fn allows(&self, name: &str, scopes: Scopes) -> bool {
        self.required
            .get(name)
            .is_some_and(|scope| scopes.contains(*scope))
    }

    /// Err when a Grant with `scopes` may not call `name`. An unknown name passes:
    /// the router answers it as any unknown tool.
    pub fn check(&self, name: &str, scopes: Scopes) -> Result<(), ErrorData> {
        match self.required.get(name) {
            Some(scope) if !scopes.contains(*scope) => Err(ErrorData::invalid_params(
                format!(
                    "{name} needs the `{scope}` Scope, which this connection was not granted \
                     (it has: {scopes}). Scopes are chosen on the consent screen when connecting: \
                     the user connects again and allows it there."
                ),
                None,
            )),
            _ => Ok(()),
        }
    }

    /// The definition of tool `name`, to adjust what it lists (description,
    /// schemas) before serving. Its Scope stays as declared.
    pub fn tool_mut(&mut self, name: &str) -> Option<&mut Tool> {
        self.router.map.get_mut(name).map(|route| &mut route.attr)
    }

    /// Every tool, whatever the Scope: what the router dispatches to once
    /// [`ScopedTools::check`] passed.
    pub fn router(&self) -> &ToolRouter<S> {
        &self.router
    }
}

impl<S: Send + Sync + 'static> Default for ScopedTools<S> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, sync::Arc};

    use axum::{extract::Request, middleware::Next};
    use rmcp::{
        RoleClient, ServiceExt,
        model::{CallToolRequestParams, ClientConfig},
        service::RunningService,
        tool, tool_router,
        transport::{
            StreamableHttpClientTransport,
            streamable_http_server::{
                StreamableHttpServerConfig, StreamableHttpService,
                session::local::LocalSessionManager,
            },
        },
    };

    use super::*;
    use crate::{
        auth::Caller,
        links::MediaLinks,
        mcp::TgMcp,
        sessions::{TokenTtl, Vault},
        tg::{Account, Connector, FakeConnector, fake::FakeData},
    };

    /// A stand-in for a `draft` tool until a real one exists.
    #[tool_router(router = test_draft_tools)]
    impl TgMcp {
        #[tool(description = "A test tool under the draft Scope.")]
        async fn test_save_draft(&self) -> String {
            "drafted".into()
        }
    }

    fn tools() -> ScopedTools<TgMcp> {
        TgMcp::scoped_tools().with(Scope::Draft, TgMcp::test_draft_tools())
    }

    fn drafts() -> Scopes {
        [Scope::Draft].into_iter().collect()
    }

    #[test]
    fn every_tool_today_needs_read() {
        let tools = TgMcp::scoped_tools();
        let all = tools.router().list_all();
        assert!(all.iter().any(|t| t.name == "whoami"));
        for tool in &all {
            assert_eq!(
                tools.required[tool.name.as_ref()],
                Scope::Read,
                "{}",
                tool.name
            );
        }
        assert_eq!(tools.listed(Scopes::READ_ONLY).len(), all.len());
    }

    #[test]
    fn a_tool_is_listed_and_callable_only_with_its_scope() {
        let tools = tools();
        let names = |scopes| {
            tools
                .listed(scopes)
                .into_iter()
                .map(|t| t.name.to_string())
                .collect::<Vec<_>>()
        };
        let read = names(Scopes::READ_ONLY);
        let draft = names(drafts());
        assert!(read.contains(&"whoami".to_string()));
        assert!(!read.contains(&"test_save_draft".to_string()));
        assert_eq!(draft.len(), read.len() + 1);
        assert!(draft.contains(&"test_save_draft".to_string()));

        assert!(tools.check("test_save_draft", Scopes::READ_ONLY).is_err());
        assert!(tools.check("test_save_draft", drafts()).is_ok());
        assert!(tools.check("whoami", Scopes::READ_ONLY).is_ok());
        assert!(tools.check("no_such_tool", Scopes::READ_ONLY).is_ok());
    }

    #[test]
    #[should_panic(expected = "declared under two Scopes")]
    fn one_tool_has_one_scope() {
        let _ = TgMcp::scoped_tools().with(Scope::Draft, TgMcp::scoped_tools().router().clone());
    }

    /// `/mcp` as rmcp serves it, with a `draft` tool, for a caller with `scopes`.
    type Client = RunningService<RoleClient, ClientConfig>;

    /// Stops the server and removes its database when dropped.
    type Running = (tokio_util::sync::DropGuard, tempfile::TempDir);

    async fn serve(scopes: Scopes) -> (Client, Running) {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open_path(&dir.path().join("t.db"), TokenTtl::default())
            .await
            .unwrap();
        let session = serde_json::to_vec(&FakeData::new(Account {
            id: 42,
            first_name: "Alice".into(),
            last_name: None,
            username: None,
        }))
        .unwrap();
        let issued = vault
            .create_grant_with_scopes("client", 42, &session, scopes)
            .await
            .unwrap();
        let grant = vault.open(issued.access.expose()).await.unwrap();
        let caller = Caller {
            grant_id: grant.grant_id.clone(),
            scopes: grant.scopes,
            telegram: FakeConnector::default().connect(grant).await.unwrap(),
        };

        let links = Arc::new(MediaLinks::new("http://127.0.0.1"));
        let shutdown = tokio_util::sync::CancellationToken::new();
        let mcp = StreamableHttpService::new(
            move || Ok(TgMcp::with_tools(links.clone(), tools(), false)),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default()
                .with_allowed_hosts(["127.0.0.1"])
                .with_cancellation_token(shutdown.clone()),
        );
        let app = axum::Router::new()
            .nest_service("/mcp", mcp)
            .layer(axum::middleware::from_fn(
                move |mut request: Request, next: Next| {
                    request.extensions_mut().insert(caller.clone());
                    next.run(request)
                },
            ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let stop = shutdown.clone();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(stop.cancelled_owned())
                .await
                .unwrap();
        });
        let transport = StreamableHttpClientTransport::from_uri(format!("http://{addr}/mcp"));
        let client = ClientConfig::default()
            .serve(transport)
            .await
            .expect("initialize");
        (client, (shutdown.drop_guard(), dir))
    }

    #[tokio::test]
    async fn tools_list_and_calls_follow_the_callers_scopes() {
        let (reader, _guard) = serve(Scopes::READ_ONLY).await;
        let (drafter, _guard2) = serve(drafts()).await;

        let read: Vec<_> = reader.list_all_tools().await.unwrap();
        let draft: Vec<_> = drafter.list_all_tools().await.unwrap();
        assert!(!read.iter().any(|t| t.name == "test_save_draft"));
        assert!(draft.iter().any(|t| t.name == "test_save_draft"));
        assert_eq!(draft.len(), read.len() + 1);

        let refused = reader
            .call_tool(CallToolRequestParams::new("test_save_draft"))
            .await
            .expect_err("a read Grant may not call a draft tool");
        assert!(
            refused.to_string().contains("needs the `draft` Scope"),
            "{refused}"
        );
        let done = drafter
            .call_tool(CallToolRequestParams::new("test_save_draft"))
            .await
            .unwrap();
        assert_eq!(done.is_error, Some(false));

        // Read tools work for both.
        for client in [&reader, &drafter] {
            let whoami = client
                .call_tool(CallToolRequestParams::new("whoami"))
                .await
                .unwrap();
            assert_eq!(whoami.structured_content.unwrap()["id"], 42);
        }
    }

    #[tokio::test]
    async fn instructions_follow_the_callers_scopes() {
        let (reader, _guard) = serve(Scopes::READ_ONLY).await;
        let (drafter, _guard2) = serve(drafts()).await;
        let said = |client: &Client| {
            client
                .peer_info()
                .and_then(|info| info.instructions.clone())
                .unwrap()
        };
        assert_eq!(said(&reader), crate::mcp::instructions(Scopes::READ_ONLY));
        assert_eq!(said(&drafter), crate::mcp::instructions(drafts()));
        assert_ne!(said(&reader), said(&drafter));
    }
}
