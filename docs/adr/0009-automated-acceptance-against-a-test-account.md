# Automated acceptance against a Test account

Acceptance was a long hand-run checklist (`docs/acceptance.md`), with a QR scan for every login, so an agent that built a feature couldn't check it against real Telegram. Acceptance now runs as code against real Telegram on a Test account that holds nothing of value. A Driver, the harness's own login to that account, does what the phone did: it accepts QR logins with `auth.acceptLoginToken` and reads what really happened in Telegram. A Peer account plays the other side of the Test account's chats. The Login flow, Consent and the MCP calls are driven headlessly. Every page involved is plain HTML forms, and `/login/<id>/status` returns the `tg://login` URL as JSON. Only Human checks need a person: how something looks on a phone, and how a real MCP client behaves. An agent asks them, and their answers gate the merge.

## Considered Options

- **Keep the checklist, run by hand.** This is the baseline. It takes about two hours of hands-on time per release, and an agent can't run it.
- **Have an LLM client run the checklist's prompt blocks ("Через Telegram MCP…") against a live connector.** Rejected for the harness: the model can quietly fix a parameter or misreport a result. The harness makes the MCP calls itself and asserts on their JSON. Checks of how a model or client uses the tools stay Human checks.
- **Telegram's test DCs** (`99966XYYYY` numbers, fixed login codes, no SIM). Rejected for now. They are a separate world that Telegram wipes from time to time. Phones reach it only through hidden debug menus. And the behaviour we most need to see is production Telegram's. They stay a fallback if the Test account gets restricted.
- **A Driver in another stack (Telethon).** Rejected: a second MTProto implementation would behave differently from the server's. The harness uses the same pinned grammers.
- **Gherkin scenarios (cucumber-rs).** Rejected: every reader of a scenario (the agent, the maintainer) reads Rust. Scenarios are plain async functions.

## Decision

- The harness is a crate, `acceptance/`, in a Cargo workspace, run with `cargo accept <scenario>`. The server binary and its image gain no dependencies.
- It refuses to write anything unless the Driver's account and every Grant's `whoami` are the configured Test account, and the Peer is the configured Peer. Agents never use a Telegram connector attached to their own session for acceptance: that connector is a real user's Account.
- It logs in through an Acceptance app, a Telegram app registered on the Test account's own number, kept apart from production's app and the Built-in app (ADR-0007), so Telegram limiting it touches no real user.
- A run has one of three outcomes: pass, fail or **blocked**. Blocked means Telegram limited the Test account (`FLOOD_WAIT`, `FROZEN_*`, `PEER_FLOOD`, a refused login). The run stops at once, with no retries, and blocked is never read as a bug in the code.
- One Test account is shared by local runs, CI and parallel agent sessions. A run takes a lease, kept as a Draft in the Test account's Saved Messages, valid for 30 minutes and renewed while it runs, and marks everything it writes with its run id.
- A local or CI run starts its own server. After every run it checks that no text the run wrote appears in the server's logs or database, and that no `tgm_` value appears in the logs.
- Fixtures (the test Chats, a forum, a group, a Chat folder, later polls and a channel with comments) are created and repaired by the harness, idempotently, and recognised by a title prefix.
- Functional runs raise the Writes budget and the Send cap. A separate limits scenario uses the defaults and runs last.
- Where it runs:
  - Local agent sessions run it directly.
  - Cloud agent sessions can't reach Telegram, which isn't reachable through their HTTPS proxy. They trigger the `acceptance` GitHub Actions workflow explicitly (dispatch or a label on the PR, never on every push) and read its result.
  - After every deploy, a smoke scenario runs against production. It alerts on failure but never rolls back.
- Secrets: the Driver's and Peer's logins, the Acceptance app and the Test account's Cloud password are kept as follows:
  - Locally: in a file outside the repository, readable only by its owner.
  - In CI: in the GitHub environment `acceptance`.
  - Never in a cloud agent environment, whose variables are stored in plain text.
- Human checks are checkboxes in the PR description. The agent asks them in chat when the maintainer is around, and anyone can tick them on GitHub. All ticked is the merge gate. A green automated run is the gate for committing and opening the PR.
- New features come with a scenario. A section in `docs/acceptance.md` stays only for Human checks and production-only checks (deploy, memory, rate limits behind the proxy, the image). Existing sections move to scenarios when their feature is next touched.

## Consequences

- The Test account becomes an Account on production through the smoke scenario: one login and one revoke per deploy.
- Logins come from CI runners' datacenter IPs. Telegram documents nothing about how it treats them. If it restricts the Test account, runs report blocked, and the remedy is a self-hosted runner or test DCs, not a change to this design.
- `account.resetAuthorization` is refused for 24 hours after the Driver logs in (`FRESH_RESET_AUTHORISATION_FORBIDDEN`). The harness therefore cleans up its Grants through `/revoke`, and resets leftover sessions through the Driver only as a fallback.
- Setting up is a one-time human job: two SIMs, the Acceptance app, the Driver's and Peer's first logins by code, the Cloud password and the CI secrets. `scripts/accept-setup.sh` walks through it.
