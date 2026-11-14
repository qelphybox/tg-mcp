//! `cargo accept`: run acceptance scenarios (ADR-0009).

use std::process::ExitCode;

use acceptance::{
    fake::FakeWorld,
    run::{self, Outcome},
    scenarios,
};

const USAGE: &str = "\
usage: cargo accept --list
       cargo accept --fake <scenario>...   run on the Fake world (no Telegram)

Scenario ids match the acceptance sections they replace (docs/acceptance.md).";

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--list") {
        for s in scenarios::ALL {
            println!("{:<10} {}", s.id, s.title);
        }
        return ExitCode::SUCCESS;
    }
    let fake = args.iter().any(|a| a == "--fake");
    let ids: Vec<&str> = args
        .iter()
        .filter(|a| !a.starts_with("--"))
        .map(String::as_str)
        .collect();
    let mut chosen = Vec::new();
    for id in &ids {
        match scenarios::find(id) {
            Some(s) => chosen.push(s),
            None => {
                eprintln!("no scenario `{id}` (cargo accept --list)");
                return ExitCode::from(2);
            }
        }
    }
    if !fake {
        eprintln!("only --fake runs exist so far");
        return ExitCode::from(2);
    }
    let world = match FakeWorld::start().await {
        Ok(w) => w,
        Err(e) => {
            eprintln!("the Fake world didn't start: {e:#}");
            return ExitCode::from(2);
        }
    };
    let report = run::run(world.world.clone(), "fake", &chosen).await;
    println!("{report}");
    match report.outcome() {
        Outcome::Pass => ExitCode::SUCCESS,
        Outcome::Fail(_) => ExitCode::FAILURE,
    }
}
