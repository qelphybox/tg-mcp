---
name: accept
description: Acceptance on the Test account (ADR-0009). Use once a feature's own tests pass and before committing it, when a ticket names a `cargo accept` scenario, or to write or fix a scenario.
---

# Accept

Prove the feature against real Telegram on the Test account, then hand the maintainer only the Human checks. Vocabulary: GLOSSARY → Acceptance (Test account, Driver, Peer, Acceptance app, Human check).

## 1. Pick the scenario

The ticket's acceptance criteria name it (`cargo accept --list` shows them all). A new feature, or an acceptance section of `docs/acceptance.md` whose feature you touched, gets a scenario first: [scenarios.md](scenarios.md). Done when a scenario covers every criterion of the ticket that Telegram or the server can show.

## 2. Make it pass on the Fake world

`cargo test -p acceptance` runs every scenario on the fake Telegram, with no secrets, in any session. Done when it is green.

## 3. Run it on the Test account

Pick by where you run:

- **Local session**: `cargo accept <id>…`. It reads its secrets from the environment or the local file `scripts/accept-setup.sh` writes; a missing one is named in the error: point the maintainer at that script and stop there.
- **Cloud session**: Telegram can't be reached from here. Trigger the `acceptance` workflow on your branch with the scenario ids and read its job summary ([cloud.md](cloud.md)).

Done when the run ends in one of its three outcomes:

- **PASS**: go on.
- **FAIL**: a bug in the feature or the scenario. Fix it, back to step 2.
- **BLOCKED**: Telegram limited the Test account (`FLOOD_WAIT`, a freeze, a refused login, a lease another run holds past its wait). It says nothing about the code: report the reason and the wait to the maintainer, change nothing, and run again only after the wait.

## 4. Commit and open the PR

A PASS is the gate for committing and opening the PR. Put the run's report in the PR body: its acceptance table, then its `### Human checks` checkboxes exactly as printed.

## 5. Get the Human checks answered

Ask every Human check with `AskUserQuestion` (in a cloud session, send a push notification first), one question each, its expected answer as the first option. Tick a checkbox in the PR body only on the maintainer's answer; a "no" becomes a PR comment and a bug to fix. The maintainer may instead tick them on GitHub.

Done when every checkbox is ticked: that is the gate for merging.
