//! `cargo accept`: run acceptance scenarios (ADR-0009).

use std::process::ExitCode;

use acceptance::{
    config::AcceptConfig,
    fake::FakeWorld,
    real,
    run::{self, Outcome, Report, Scenario},
    scenarios,
    telegram::{self, Role},
};

const USAGE: &str = "\
usage: cargo accept [<scenario>...]              on the Test account, with a local server
       cargo accept --image <ref> <scenario>...  on the Test account, the server from a Docker image
                                                 (ghcr.io/qelphybox/tg-mcp:<sha>; `local` builds the working copy)
       cargo accept --target <url> <scenario>... on the Test account, against a running server
       cargo accept --fake [<scenario>...]       on the Fake world (no Telegram, no secrets)
       cargo accept --list                       the scenarios
       cargo accept sign-in driver|peer          first login of the Test account / the Peer
       cargo accept fixtures                     create or repair the Test account's test chats

Without scenarios: every scenario except `smoke`. Exit status: 0 pass,
1 fail, 3 blocked (Telegram limited an account: not a bug in the code),
2 usage or configuration. Scenario ids match docs/acceptance.md sections.";

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match go(&args).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{e:#}");
            ExitCode::from(2)
        }
    }
}

async fn go(args: &[String]) -> anyhow::Result<ExitCode> {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    if args.iter().any(|a| a == "--list") {
        for s in scenarios::ALL {
            println!("{:<10} {}", s.id, s.title);
        }
        return Ok(ExitCode::SUCCESS);
    }
    if args.first().map(String::as_str) == Some("sign-in") {
        let role = match args.get(1).map(String::as_str) {
            Some("driver") => Role::Driver,
            Some("peer") => Role::Peer,
            _ => anyhow::bail!("{USAGE}"),
        };
        telegram::sign_in(role).await?;
        return Ok(ExitCode::SUCCESS);
    }

    if args.first().map(String::as_str) == Some("fixtures") {
        let config = AcceptConfig::load()?;
        return Ok(match real::fixtures(&config).await {
            Ok(done) if done.is_empty() => {
                println!("Fixtures: all in place, nothing to do.");
                ExitCode::SUCCESS
            }
            Ok(done) => {
                for line in done {
                    println!("- {line}");
                }
                ExitCode::SUCCESS
            }
            Err(e) => match acceptance::blocked::find(&e) {
                Some(b) => {
                    eprintln!("BLOCKED: {} (not a bug in the code)", b.reason);
                    ExitCode::from(3)
                }
                None => {
                    eprintln!("{e:#}");
                    ExitCode::FAILURE
                }
            },
        });
    }

    let mut fake = false;
    let mut target = None;
    let mut image = None;
    let mut ids = Vec::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--fake" => fake = true,
            "--target" => {
                target = Some(
                    rest.next()
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("--target needs a URL"))?,
                )
            }
            "--image" => {
                image = Some(
                    rest.next()
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("--image needs an image (or `local`)"))?,
                )
            }
            flag if flag.starts_with("--") => anyhow::bail!("unknown option {flag}\n\n{USAGE}"),
            id => ids.push(id.to_string()),
        }
    }
    let chosen = choose(&ids)?;

    let report = if fake {
        let world = FakeWorld::start().await?;
        run::run(world.world.clone(), &chosen).await
    } else {
        let config = AcceptConfig::load()?;
        let sut = match (target, image) {
            (Some(_), Some(_)) => anyhow::bail!("--target and --image exclude each other"),
            (Some(url), None) => real::ServerUnderTest::Target(url),
            (None, Some(image)) => real::ServerUnderTest::Image(image),
            (None, None) => real::ServerUnderTest::CargoRun,
        };
        let world = real::start(&config, &sut).await?;
        run::run(world, &chosen).await
    };
    Ok(finish(&report))
}

/// The scenarios named, or every one but `smoke`.
fn choose(ids: &[String]) -> anyhow::Result<Vec<&'static Scenario>> {
    if ids.is_empty() {
        return Ok(scenarios::ALL.iter().filter(|s| s.id != "smoke").collect());
    }
    ids.iter()
        .map(|id| {
            scenarios::find(id)
                .ok_or_else(|| anyhow::anyhow!("no scenario `{id}` (cargo accept --list)"))
        })
        .collect()
}

fn finish(report: &Report) -> ExitCode {
    println!("{report}");
    match report.outcome() {
        Outcome::Pass => ExitCode::SUCCESS,
        Outcome::Fail(_) => ExitCode::FAILURE,
        Outcome::Blocked(_) => ExitCode::from(3),
    }
}
