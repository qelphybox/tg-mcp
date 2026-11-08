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

    /// Those of `scopes` that some tool needs, and `read`: what a Grant with
    /// `scopes` can do on this server. A Scope granted before any tool uses it
    /// (`send` until a tool sends) adds nothing yet.
    pub fn usable(&self, scopes: Scopes) -> Scopes {
        scopes
            .iter()
            .filter(|scope| self.required.values().any(|needed| needed == scope))
            .collect()
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

    fn tools() -> ScopedTools<TgMcp> {
        TgMcp::scoped_tools()
    }

    fn drafts() -> Scopes {
        [Scope::Draft].into_iter().collect()
    }

    #[test]
    fn every_tool_needs_read_but_save_draft() {
        let tools = TgMcp::scoped_tools();
        let all = tools.router().list_all();
        assert!(all.iter().any(|t| t.name == "whoami"));
        assert!(all.iter().any(|t| t.name == "get_drafts"));
        for tool in &all {
            let needed = if tool.name == "save_draft" {
                Scope::Draft
            } else {
                Scope::Read
            };
            assert_eq!(tools.required[tool.name.as_ref()], needed, "{}", tool.name);
        }
        assert_eq!(tools.listed(Scopes::READ_ONLY).len(), all.len() - 1);
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
        assert!(read.contains(&"get_drafts".to_string()));
        assert!(!read.contains(&"save_draft".to_string()));
        assert_eq!(draft.len(), read.len() + 1);
        assert!(draft.contains(&"save_draft".to_string()));

        assert!(tools.check("save_draft", Scopes::READ_ONLY).is_err());
        assert!(tools.check("save_draft", drafts()).is_ok());
        assert!(tools.check("whoami", Scopes::READ_ONLY).is_ok());
        assert!(tools.check("get_drafts", Scopes::READ_ONLY).is_ok());
        assert!(tools.check("no_such_tool", Scopes::READ_ONLY).is_ok());
    }

    #[test]
    #[should_panic(expected = "declared under two Scopes")]
    fn one_tool_has_one_scope() {
        let _ = TgMcp::scoped_tools().with(Scope::Draft, TgMcp::scoped_tools().router().clone());
    }

    /// `/mcp` as rmcp serves it, for a caller with `scopes`.
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
            send_allow_list: None,
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
        assert!(!read.iter().any(|t| t.name == "save_draft"));
        assert!(draft.iter().any(|t| t.name == "save_draft"));
        assert_eq!(draft.len(), read.len() + 1);

        // Into Saved Messages, which every Account has.
        let save = || {
            let saved_messages =
                crate::tg::fake::fake_issued_id(crate::tg::chats::ChatId::User(42));
            CallToolRequestParams::new("save_draft").with_arguments(
                serde_json::json!({ "chat_id": saved_messages, "text": "hi" })
                    .as_object()
                    .unwrap()
                    .clone(),
            )
        };
        let refused = reader
            .call_tool(save())
            .await
            .expect_err("a read Grant may not call a draft tool");
        assert!(
            refused.to_string().contains("needs the `draft` Scope"),
            "{refused}"
        );
        let done = drafter.call_tool(save()).await.unwrap();
        assert_eq!(done.is_error, Some(false));
        assert_eq!(done.structured_content.unwrap()["status"], "saved");

        // Read tools work for both.
        for client in [&reader, &drafter] {
            let whoami = client
                .call_tool(CallToolRequestParams::new("whoami"))
                .await
                .unwrap();
            assert_eq!(whoami.structured_content.unwrap()["id"], 42);
        }
    }

    #[test]
    fn only_scopes_with_tools_are_usable() {
        let tools = tools();
        let all: Scopes = Scope::ALL.into_iter().collect();
        // No tool sends or organizes yet: such a Grant can read and write Drafts.
        assert_eq!(tools.usable(all), drafts());
        assert_eq!(tools.usable(drafts()), drafts());
        assert_eq!(tools.usable(Scopes::READ_ONLY), Scopes::READ_ONLY);
        let sending: Scopes = [Scope::Send].into_iter().collect();
        assert_eq!(tools.usable(sending), Scopes::READ_ONLY);
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

        // Granted `send` while no tool sends: told only what it can do.
        let (sender, _guard3) = serve(drafts().with(Scope::Send)).await;
        assert_eq!(said(&sender), crate::mcp::instructions(drafts()));
        assert!(said(&sender).contains("you cannot send, edit or delete messages"));
    }
}
