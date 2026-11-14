//! A run: scenarios in one world, their steps, Human checks and outcome.

use std::{
    fmt,
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use reqwest::header;
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, ClientConfig},
    service::RunningService,
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::Value;

use crate::{
    blocked,
    login::{self, Grant},
    world::{Who, World},
};

/// A scenario's body.
pub type ScenarioFn = for<'a> fn(&'a mut Run) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>>;

/// One acceptance scenario, by the id of the section it replaces.
pub struct Scenario {
    pub id: &'static str,
    pub title: &'static str,
    pub run: ScenarioFn,
}

/// How a scenario (or a whole run) ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Pass,
    /// What failed, as the report says it.
    Fail(String),
    /// Telegram limited an account: the run stopped there. Not a code bug.
    Blocked(String),
}

/// The Grant's `whoami` is not the account it must be: nothing may be written.
#[derive(Debug)]
pub struct Interlock {
    pub expected: i64,
    pub got: Value,
}

impl fmt::Display for Interlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "interlock: whoami is account {}, not the expected account {}; nothing was written",
            self.got, self.expected
        )
    }
}

impl std::error::Error for Interlock {}

/// A step only a person can judge (GLOSSARY: Human check).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HumanCheck {
    pub scenario: &'static str,
    pub question: String,
    pub expected: String,
    /// Must be answered at the moment it comes up, not at the end of the run.
    pub live: bool,
}

/// One scenario as it ran.
#[derive(Debug, Clone)]
pub struct ScenarioReport {
    pub id: &'static str,
    pub title: &'static str,
    pub outcome: Outcome,
    /// What it proved, in order.
    pub steps: Vec<String>,
    pub human: Vec<HumanCheck>,
    /// What cleaning up after it couldn't do (never changes the outcome).
    pub teardown: Vec<String>,
}

/// A whole run.
#[derive(Debug, Clone)]
pub struct Report {
    /// Which world it ran in: `fake` or `real`.
    pub world: String,
    pub scenarios: Vec<ScenarioReport>,
    /// Scenarios that never ran because the run was blocked or aborted.
    pub not_run: Vec<&'static str>,
    /// Why the run stopped before its first scenario, if it did.
    pub aborted: Option<Outcome>,
    /// Checks of the run as a whole (#138's invariants), with their outcome.
    pub checks: Vec<(String, Outcome)>,
}

impl Report {
    /// Blocked if anything was blocked; else the first failure; else pass.
    pub fn outcome(&self) -> Outcome {
        let all = self
            .aborted
            .iter()
            .chain(self.scenarios.iter().map(|s| &s.outcome))
            .chain(self.checks.iter().map(|(_, o)| o));
        let mut fail = None;
        for o in all {
            match o {
                Outcome::Blocked(_) => return o.clone(),
                Outcome::Fail(_) if fail.is_none() => fail = Some(o.clone()),
                _ => {}
            }
        }
        fail.unwrap_or(Outcome::Pass)
    }

    /// Every Human check of the run, in order.
    pub fn human_checks(&self) -> impl Iterator<Item = &HumanCheck> {
        self.scenarios.iter().flat_map(|s| s.human.iter())
    }
}

impl fmt::Display for Report {
    /// The acceptance table and the Human checks as PR checkboxes.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.outcome() {
            Outcome::Pass => writeln!(f, "## Acceptance: PASS ({} world)", self.world)?,
            Outcome::Fail(_) => writeln!(f, "## Acceptance: FAIL ({} world)", self.world)?,
            Outcome::Blocked(why) => {
                writeln!(f, "## Acceptance: BLOCKED ({} world)", self.world)?;
                writeln!(f)?;
                writeln!(
                    f,
                    "**BLOCKED:** {} — not a bug in the code; don't change code or retry around it.",
                    one_line(&why)
                )?;
            }
        }
        if let Some(Outcome::Fail(why) | Outcome::Blocked(why)) = &self.aborted {
            writeln!(f)?;
            writeln!(f, "The run stopped before any scenario: {}", one_line(why))?;
        }
        writeln!(f)?;
        writeln!(f, "| Scenario | Automated | Human checks |")?;
        writeln!(f, "|---|---|---|")?;
        for s in &self.scenarios {
            let auto = match &s.outcome {
                Outcome::Pass => "✅".to_string(),
                Outcome::Fail(why) => format!("❌ {}", one_line(why)),
                Outcome::Blocked(why) => format!("⛔ {}", one_line(why)),
            };
            let human = if s.human.is_empty() {
                "—".to_string()
            } else {
                format!("⏳ {}", s.human.len())
            };
            writeln!(f, "| `{}` {} | {auto} | {human} |", s.id, s.title)?;
        }
        for id in &self.not_run {
            writeln!(f, "| `{id}` | not run | — |")?;
        }
        for (check, outcome) in &self.checks {
            let result = match outcome {
                Outcome::Pass => "✅".to_string(),
                Outcome::Fail(why) => format!("❌ {}", one_line(why)),
                Outcome::Blocked(why) => format!("skipped: {}", one_line(why)),
            };
            writeln!(f, "| {check} | {result} | — |")?;
        }
        for s in &self.scenarios {
            if s.steps.is_empty() && s.teardown.is_empty() {
                continue;
            }
            writeln!(f)?;
            writeln!(f, "<details><summary><code>{}</code> steps</summary>", s.id)?;
            writeln!(f)?;
            for step in &s.steps {
                writeln!(f, "- {step}")?;
            }
            for note in &s.teardown {
                writeln!(f, "- teardown: {note}")?;
            }
            writeln!(f)?;
            writeln!(f, "</details>")?;
        }
        let human: Vec<_> = self.human_checks().collect();
        if !human.is_empty() {
            writeln!(f)?;
            writeln!(f, "### Human checks")?;
            writeln!(f)?;
            for h in human {
                let live = if h.live { " (live)" } else { "" };
                writeln!(
                    f,
                    "- [ ] {}{live}: {} — expected: {}",
                    h.scenario, h.question, h.expected
                )?;
            }
        }
        Ok(())
    }
}

fn one_line(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "\\|")
}

/// What a scenario runs with.
pub struct Run {
    pub world: Arc<World>,
    scenario: &'static str,
    steps: Vec<String>,
    human: Vec<HumanCheck>,
    /// Every Grant made, revoked when the scenario ends.
    grants: Vec<Grant>,
}

impl Run {
    /// Records a step the scenario proved.
    pub fn passed(&mut self, step: impl Into<String>) {
        self.steps.push(step.into());
    }

    /// Records a Human check, asked at the end of the run.
    pub fn human(&mut self, question: impl Into<String>, expected: impl Into<String>) {
        self.human.push(HumanCheck {
            scenario: self.scenario,
            question: question.into(),
            expected: expected.into(),
            live: false,
        });
    }

    /// Logs the Test account in with `scopes` ticked, opens an MCP session and
    /// checks its `whoami` before anything else.
    pub async fn login(&mut self, scopes: &[&str]) -> Result<Session> {
        self.login_as(Who::TestAccount, scopes).await
    }

    /// [`Run::login`] for `who`.
    pub async fn login_as(&mut self, who: Who, scopes: &[&str]) -> Result<Session> {
        let grant = login::login(&self.world, who, scopes).await?;
        self.grants.push(grant.clone());
        let by = match who {
            Who::TestAccount => "the Driver",
            Who::Peer => "the Peer",
        };
        self.passed(format!(
            "Login flow: {by} accepted the QR, Consent granted `{}`",
            grant.scope
        ));
        let session = Session::open(&self.world, grant).await?;
        let me = session
            .call("whoami", Value::Object(Default::default()))
            .await?;
        let expected = self.world.account_of(who);
        if me["id"].as_i64() != Some(expected) {
            return Err(Interlock {
                expected,
                got: me["id"].clone(),
            }
            .into());
        }
        self.passed(match who {
            Who::TestAccount => "whoami is the Test account",
            Who::Peer => "whoami is the Peer",
        });
        Ok(session)
    }

    /// Revokes every Grant the scenario made; returns what didn't work.
    async fn teardown(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        let http = reqwest::Client::new();
        for grant in self.grants.drain(..) {
            let token = grant
                .refresh_token
                .clone()
                .unwrap_or_else(|| grant.access_token.clone());
            let body = url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs([("token", token.as_str()), ("client_id", &grant.client_id)])
                .finish();
            let res = http
                .post(self.world.server.url("/revoke"))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(body)
                .send()
                .await;
            match res {
                Ok(r) if r.status().is_success() => {}
                Ok(r) => notes.push(format!("/revoke answered {}", r.status())),
                Err(e) => notes.push(format!("/revoke failed: {e}")),
            }
        }
        notes
    }
}

/// An MCP session of one Grant.
pub struct Session {
    pub grant: Grant,
    client: RunningService<RoleClient, ClientConfig>,
}

impl Session {
    async fn open(world: &World, grant: Grant) -> Result<Self> {
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(world.server.url("/mcp"))
                .auth_header(grant.access_token.clone()),
        );
        let client = ClientConfig::default()
            .serve(transport)
            .await
            .context("MCP initialize")?;
        Ok(Self { grant, client })
    }

    /// Calls `tool` and returns its structured output (or its text, for a
    /// tool without one). A tool error is an error, and blocks the run when
    /// it says Telegram limited the account.
    pub async fn call(&self, tool: &str, arguments: Value) -> Result<Value> {
        self.try_call(tool, arguments).await?.map_err(|message| {
            match blocked::server_message(&message) {
                Some(b) => anyhow::Error::new(b).context(tool.to_string()),
                None => anyhow::anyhow!("{tool} failed: {message}"),
            }
        })
    }

    /// Calls `tool`; an error the tool answered (as opposed to a broken
    /// connection) comes back as `Err(message)` for the scenario to check.
    pub async fn try_call(
        &self,
        tool: &str,
        arguments: Value,
    ) -> Result<std::result::Result<Value, String>> {
        let Value::Object(arguments) = arguments else {
            anyhow::bail!("{tool}: arguments must be an object");
        };
        let result = self
            .client
            .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(arguments))
            .await;
        let result = match result {
            Ok(result) => result,
            Err(rmcp::ServiceError::McpError(e)) => return Ok(Err(e.message.to_string())),
            Err(e) => return Err(anyhow::Error::new(e).context(tool.to_string())),
        };
        let text = result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        if result.is_error == Some(true) {
            return Ok(Err(text));
        }
        Ok(Ok(result
            .structured_content
            .clone()
            .unwrap_or(Value::String(text))))
    }

    /// The tools this Grant lists.
    pub async fn tools(&self) -> Result<Vec<String>> {
        Ok(self
            .client
            .list_all_tools()
            .await
            .context("tools/list")?
            .into_iter()
            .map(|t| t.name.to_string())
            .collect())
    }
}

fn outcome_of(err: &anyhow::Error) -> Outcome {
    match blocked::find(err) {
        Some(b) => Outcome::Blocked(b.reason.clone()),
        None => Outcome::Fail(format!("{err:#}")),
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Runs `scenarios` in `world`, one after another. Checks the Driver and the
/// Peer first; stops at the first blocked scenario. Each scenario's Grants are
/// revoked when it ends, whatever its outcome, and sessions it left behind are
/// ended through the Driver at the end.
pub async fn run(world: Arc<World>, scenarios: &[&Scenario]) -> Report {
    let started = unix_now();
    let mut report = Report {
        world: world.name.to_string(),
        scenarios: Vec::new(),
        not_run: Vec::new(),
        aborted: None,
        checks: Vec::new(),
    };
    if let Err(e) = world.check_parties().await {
        report.aborted = Some(outcome_of(&e));
        report.not_run = scenarios.iter().map(|s| s.id).collect();
        return report;
    }
    let mut stopped = false;
    for scenario in scenarios {
        if stopped {
            report.not_run.push(scenario.id);
            continue;
        }
        let mut run = Run {
            world: world.clone(),
            scenario: scenario.id,
            steps: Vec::new(),
            human: Vec::new(),
            grants: Vec::new(),
        };
        let outcome = match (scenario.run)(&mut run).await {
            Ok(()) => Outcome::Pass,
            Err(e) => outcome_of(&e),
        };
        stopped = matches!(outcome, Outcome::Blocked(_));
        let teardown = run.teardown().await;
        report.scenarios.push(ScenarioReport {
            id: scenario.id,
            title: scenario.title,
            outcome,
            steps: run.steps,
            human: run.human,
            teardown,
        });
    }
    if let Err(e) = world.driver.end_server_sessions(started - 60).await
        && let Some(last) = report.scenarios.last_mut()
    {
        last.teardown.push(format!(
            "ending leftover sessions through the Driver: {e:#}"
        ));
    }
    report
}
