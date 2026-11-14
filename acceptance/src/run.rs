//! A run: scenarios in one world, their steps, Human checks and outcome.

use std::{fmt, future::Future, pin::Pin, sync::Arc};

use anyhow::{Context, Result};
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
    login::{self, Grant},
    world::World,
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
}

/// The Grant's `whoami` is not the Test account: nothing may be written.
#[derive(Debug)]
pub struct Interlock {
    pub expected: i64,
    pub got: Value,
}

impl fmt::Display for Interlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "interlock: whoami is account {}, not the Test account {}; nothing was written",
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
}

/// A whole run.
#[derive(Debug, Clone)]
pub struct Report {
    /// Which world it ran in, e.g. "fake".
    pub world: String,
    pub scenarios: Vec<ScenarioReport>,
}

impl Report {
    /// Pass if every scenario passed, else the first failure.
    pub fn outcome(&self) -> Outcome {
        self.scenarios
            .iter()
            .map(|s| s.outcome.clone())
            .find(|o| *o != Outcome::Pass)
            .unwrap_or(Outcome::Pass)
    }

    /// Every Human check of the run, in order.
    pub fn human_checks(&self) -> impl Iterator<Item = &HumanCheck> {
        self.scenarios.iter().flat_map(|s| s.human.iter())
    }
}

impl fmt::Display for Report {
    /// The acceptance table and the Human checks as PR checkboxes.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let verdict = match self.outcome() {
            Outcome::Pass => "PASS".to_string(),
            Outcome::Fail(_) => "FAIL".to_string(),
        };
        writeln!(f, "## Acceptance: {verdict} ({} world)", self.world)?;
        writeln!(f)?;
        writeln!(f, "| Scenario | Automated | Human checks |")?;
        writeln!(f, "|---|---|---|")?;
        for s in &self.scenarios {
            let auto = match &s.outcome {
                Outcome::Pass => "✅".to_string(),
                Outcome::Fail(why) => format!("❌ {}", one_line(why)),
            };
            let human = if s.human.is_empty() {
                "—".to_string()
            } else {
                format!("⏳ {}", s.human.len())
            };
            writeln!(f, "| `{}` {} | {auto} | {human} |", s.id, s.title)?;
        }
        for s in &self.scenarios {
            if s.steps.is_empty() {
                continue;
            }
            writeln!(f)?;
            writeln!(f, "<details><summary><code>{}</code> steps</summary>", s.id)?;
            writeln!(f)?;
            for step in &s.steps {
                writeln!(f, "- {step}")?;
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

    /// Logs in through the Login flow with `scopes` ticked, opens an MCP
    /// session and checks it is the Test account before anything else.
    pub async fn login(&mut self, scopes: &[&str]) -> Result<Session> {
        let grant = login::login(&self.world, scopes).await?;
        self.passed(format!(
            "Login flow: the Driver accepted the QR, Consent granted `{}`",
            grant.scope
        ));
        let session = Session::open(&self.world, grant).await?;
        let me = session
            .call("whoami", Value::Object(Default::default()))
            .await?;
        if me["id"].as_i64() != Some(self.world.test_account) {
            return Err(Interlock {
                expected: self.world.test_account,
                got: me["id"].clone(),
            }
            .into());
        }
        self.passed("whoami is the Test account");
        Ok(session)
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
    /// tool without one). A tool error is an error.
    pub async fn call(&self, tool: &str, arguments: Value) -> Result<Value> {
        let Value::Object(arguments) = arguments else {
            anyhow::bail!("{tool}: arguments must be an object");
        };
        let result = self
            .client
            .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(arguments))
            .await
            .with_context(|| tool.to_string())?;
        let text = || {
            result
                .content
                .iter()
                .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                .collect::<Vec<_>>()
                .join("\n")
        };
        if result.is_error == Some(true) {
            anyhow::bail!("{tool} failed: {}", text());
        }
        Ok(result
            .structured_content
            .clone()
            .unwrap_or_else(|| Value::String(text())))
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

/// Runs `scenarios` in `world`, one after another, each to its end.
pub async fn run(world: Arc<World>, world_name: &str, scenarios: &[&Scenario]) -> Report {
    let mut reports = Vec::new();
    for scenario in scenarios {
        let mut run = Run {
            world: world.clone(),
            scenario: scenario.id,
            steps: Vec::new(),
            human: Vec::new(),
        };
        let outcome = match (scenario.run)(&mut run).await {
            Ok(()) => Outcome::Pass,
            Err(e) => Outcome::Fail(format!("{e:#}")),
        };
        reports.push(ScenarioReport {
            id: scenario.id,
            title: scenario.title,
            outcome,
            steps: run.steps,
            human: run.human,
        });
    }
    Report {
        world: world_name.to_string(),
        scenarios: reports,
    }
}
