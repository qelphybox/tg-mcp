# tg-mcp

Remote MCP server (streamable HTTP) giving AI clients read-only access to a personal Telegram account. Onboarding: `claude mcp add --transport http <name> <url>` → OAuth in the browser → QR login → done. Vocabulary is in `GLOSSARY.md`; decisions in `docs/adr/`.

## Stack

Rust (latest stable), tokio, axum, rmcp (MCP), grammers (Telegram, version pinned exactly), sqlx + SQLite. One binary, one Docker image.

## Commands

- `cargo fmt --all` / `cargo clippy --all-targets -- -D warnings` / `cargo test`: all three must pass before a commit.
- `cargo run`: starts the server; config comes from env vars (see `.env.example`).

## Rules that are not visible in the code

- Never log message content, Tokens, Session keys or Telegram sessions. Log ids and sizes only.
- Every MCP tool is read-only (`readOnlyHint: true`), has bounded input/output schemas and a capped response size.
- Telegram's `FLOOD_WAIT` is honoured, never retried around.
- The Session key is never persisted in plaintext (ADR-0003). Anything that would need it without a Token is a design bug.
- Repo artifacts (code, comments, issues, docs) are in English.

## Agent skills

Matt Pocock's skills are vendored in `.claude/skills/` (engineering + productivity, MIT). Start a change with `/grill-with-docs`, then `/to-spec` → `/to-tickets` → `/implement` (which drives `/tdd` and `/code-review`).

### Issue tracker

GitHub Issues in `qelphybox/tg-mcp` (`gh` locally, GitHub MCP tools in cloud sessions). See `docs/agents/issue-tracker.md`.

### Triage labels

Default vocabulary: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: `GLOSSARY.md` + `docs/adr/` at the repo root. See `docs/agents/domain.md`.
