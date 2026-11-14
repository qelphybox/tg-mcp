//! Which Scope each tool needs (ADR-0005). A tool's Scope is the router it is
//! declared in: [`super::TgMcp::scoped_tools`] puts each `#[tool_router(router = …)]`
//! block under one Scope, so a new tool gets its Scope from the block it is
//! written in and is never listed or called without it.
//! It also holds every tool to the parameters its input schema lists
//! ([`super::arguments`]).

use std::collections::HashMap;

use rmcp::{
    ErrorData,
    handler::server::router::tool::ToolRouter,
    model::{JsonObject, Tool},
};

use super::arguments;
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

    /// Add every tool of `router` under `scope`, each input schema closed to
    /// other parameters ([`arguments::forbid_others`]). Panics on a tool name
    /// that is already there: one tool, one Scope.
    pub fn with(mut self, scope: Scope, mut router: ToolRouter<S>) -> Self {
        for route in router.map.values_mut() {
            arguments::forbid_others(&mut route.attr);
        }
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

    /// Err when `arguments` has a key tool `name`'s input schema doesn't list:
    /// a misspelt parameter would otherwise be dropped silently. An unknown
    /// tool passes: the router answers it as any unknown tool.
    pub fn check_arguments(
        &self,
        name: &str,
        arguments: Option<&JsonObject>,
    ) -> Result<(), ErrorData> {
        match self.router.map.get(name) {
            Some(route) => arguments::check(&route.attr, arguments)
                .map_err(|refusal| ErrorData::invalid_params(refusal, None)),
            None => Ok(()),
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

    fn sending() -> Scopes {
        [Scope::Send].into_iter().collect()
    }

    #[test]
    fn every_tool_needs_read_but_save_draft_and_the_send_tools() {
        let tools = TgMcp::scoped_tools();
        let all = tools.router().list_all();
        assert!(all.iter().any(|t| t.name == "whoami"));
        assert!(all.iter().any(|t| t.name == "get_drafts"));
        for tool in &all {
            let needed = match tool.name.as_ref() {
                "save_draft" => Scope::Draft,
                "send_message" | "react" | "mark_read" => Scope::Send,
                _ => Scope::Read,
            };
            assert_eq!(tools.required[tool.name.as_ref()], needed, "{}", tool.name);
        }
        assert_eq!(tools.listed(Scopes::READ_ONLY).len(), all.len() - 4);
        assert_eq!(tools.listed(sending()).len(), all.len() - 1);
        for name in ["send_message", "react", "mark_read"] {
            assert!(!tools.listed(drafts()).iter().any(|t| t.name == name));
            assert!(tools.check(name, drafts()).is_err());
            assert!(tools.check(name, sending()).is_ok());
        }
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
            account_id: grant.account_id,
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
        // No tool organizes yet: such a Grant can read, write Drafts and send.
        assert_eq!(tools.usable(all), drafts().with(Scope::Send));
        assert_eq!(tools.usable(drafts()), drafts());
        assert_eq!(tools.usable(Scopes::READ_ONLY), Scopes::READ_ONLY);
        assert_eq!(tools.usable(sending()), sending());
        let organizing: Scopes = [Scope::Organize].into_iter().collect();
        assert_eq!(tools.usable(organizing), Scopes::READ_ONLY);
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

        // Granted `send`: told it can send, and how.
        let (sender, _guard3) = serve(sending()).await;
        assert_eq!(said(&sender), crate::mcp::instructions(sending()));
        assert!(said(&sender).contains("you cannot edit or delete messages"));
        // Granted `organize` while no tool organizes: told only what it can do.
        let (organizer, _guard4) = serve(drafts().with(Scope::Organize)).await;
        assert_eq!(said(&organizer), crate::mcp::instructions(drafts()));
    }

    /// A tool's parameters as its published input schema lists them.
    fn parameters(tool: &Tool) -> Vec<String> {
        arguments::parameters(&tool.input_schema)
            .map(String::from)
            .collect()
    }

    fn every_scope() -> Scopes {
        Scope::ALL.into_iter().collect()
    }

    fn call(tool: &str, arguments: serde_json::Value) -> CallToolRequestParams {
        CallToolRequestParams::new(tool.to_string())
            .with_arguments(arguments.as_object().unwrap().clone())
    }

    async fn refusal(client: &Client, tool: &str, arguments: serde_json::Value) -> String {
        match client.call_tool(call(tool, arguments)).await {
            Err(rmcp::ServiceError::McpError(e))
                if e.code == rmcp::model::ErrorCode::INVALID_PARAMS =>
            {
                e.message.to_string()
            }
            other => panic!("{tool}: expected invalid_params, got {other:?}"),
        }
    }

    #[test]
    fn every_published_input_schema_forbids_other_parameters() {
        // As the server serves them, get_media's schema adjusted either way.
        let links = Arc::new(MediaLinks::new("http://127.0.0.1"));
        for documents in [false, true] {
            let server = TgMcp::new(links.clone(), Arc::default(), documents);
            for tool in server.tools.listed(every_scope()) {
                assert_eq!(
                    tool.input_schema.get("additionalProperties"),
                    Some(&serde_json::Value::Bool(false)),
                    "{}",
                    tool.name
                );
            }
        }
    }

    #[test]
    fn every_tool_takes_each_parameter_its_schema_lists() {
        let tools = tools();
        for tool in tools.router().list_all() {
            let all: JsonObject = parameters(&tool)
                .into_iter()
                .map(|name| (name, serde_json::Value::Null))
                .collect();
            assert!(
                tools.check_arguments(&tool.name, Some(&all)).is_ok(),
                "{}",
                tool.name
            );
            assert!(tools.check_arguments(&tool.name, None).is_ok());
        }
    }

    #[tokio::test]
    async fn every_tool_refuses_an_unknown_parameter_and_names_it() {
        let (client, _guard) = serve(every_scope()).await;
        let listed = client.list_all_tools().await.unwrap();
        assert_eq!(listed.len(), tools().router().list_all().len());
        for tool in &listed {
            let known = parameters(tool);
            let message = refusal(&client, &tool.name, serde_json::json!({ "foo": 1 })).await;
            let takes = if known.is_empty() {
                "this tool takes no parameters".to_string()
            } else {
                format!("this tool takes: {}", known.join(", "))
            };
            assert_eq!(
                message,
                format!("unknown parameter `foo`; {takes}"),
                "{}",
                tool.name
            );

            // A near miss of each parameter: written camelCase, or upper case.
            for name in &known {
                let camel = name
                    .split('_')
                    .enumerate()
                    .map(|(i, part)| {
                        let mut chars = part.chars();
                        match (i, chars.next()) {
                            (0, _) | (_, None) => part.to_string(),
                            (_, Some(first)) => first.to_uppercase().chain(chars).collect(),
                        }
                    })
                    .collect::<String>();
                let miss = if camel != *name {
                    camel
                } else {
                    name.to_uppercase()
                };
                let message =
                    refusal(&client, &tool.name, serde_json::json!({ miss.clone(): 1 })).await;
                assert_eq!(
                    message,
                    format!("unknown parameter `{miss}`; did you mean `{name}`?"),
                    "{}",
                    tool.name
                );
            }
        }
    }

    #[tokio::test]
    async fn the_misspellings_seen_in_use_get_the_right_suggestion() {
        let (client, _guard) = serve(every_scope()).await;
        for (tool, miss, meant) in [
            ("get_messages", "pinnedOnly", "pinned_only"),
            ("list_chats", "archive", "archived"),
            ("search_messages", "chat", "chat_id"),
        ] {
            let message = refusal(&client, tool, serde_json::json!({ miss: true })).await;
            assert_eq!(
                message,
                format!("unknown parameter `{miss}`; did you mean `{meant}`?")
            );
        }
        // An unknown key next to known ones is refused all the same.
        let message = refusal(
            &client,
            "list_chats",
            serde_json::json!({ "limit": 5, "archive": true }),
        )
        .await;
        assert!(
            message.starts_with("unknown parameter `archive`"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn known_parameters_still_work() {
        let (client, _guard) = serve(every_scope()).await;
        let saved_messages = crate::tg::fake::fake_issued_id(crate::tg::chats::ChatId::User(42));
        for (tool, arguments) in [
            ("whoami", serde_json::json!({})),
            (
                "list_chats",
                serde_json::json!({ "limit": 5, "archived": false }),
            ),
            ("get_drafts", serde_json::json!({})),
            ("search_messages", serde_json::json!({ "query": "hi" })),
            (
                "save_draft",
                serde_json::json!({ "chat_id": saved_messages, "text": "hi" }),
            ),
        ] {
            let done = client.call_tool(call(tool, arguments)).await;
            assert!(
                done.as_ref().is_ok_and(|r| r.is_error != Some(true)),
                "{tool}: {done:?}"
            );
        }
    }

    #[tokio::test]
    async fn an_unknown_parameter_is_shown_shortened() {
        let (client, _guard) = serve(every_scope()).await;
        let long = "x".repeat(10_000);
        let message = refusal(&client, "whoami", serde_json::json!({ long: 1 })).await;
        assert!(message.len() < 200, "{}", message.len());
        assert!(message.starts_with("unknown parameter `xxx"), "{message}");
    }
}
