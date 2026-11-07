# tg-mcp

**Unofficial** remote [MCP](https://modelcontextprotocol.io) server that gives AI clients (Claude Code, Claude, Cursor, …) **read-only** access to your personal Telegram account. You add a URL, log in by scanning a QR code, and you're done.

> Not affiliated with or endorsed by Telegram. It uses the Telegram API through its own registered app ("Unofficial MCP Server"). It can only read: nothing is ever sent, edited or deleted.

## Quick start

Public instance: `https://tgmcp.qelphybox.space/mcp`

**Claude Code**

```sh
claude mcp add --transport http telegram https://tgmcp.qelphybox.space/mcp
```

Then run `/mcp` in Claude Code → `telegram` → **Authenticate**. A browser opens:

1. Scan the QR code with Telegram on your phone: **Settings → Devices → Link Desktop Device**.
2. If your account has a cloud password (two-step verification), type it on the same page. It goes only to this server's login page, never through the AI client or the chat.
3. **Allow** read-only access on the consent screen.

**Claude (web/desktop):** Settings → Connectors → Add custom connector → the same URL. **Other MCP clients:** add it as a streamable-HTTP server; they discover OAuth on their own.

## Tools

| Tool | What it returns |
| --- | --- |
| `whoami` | The connected account: id, name, @username. |
| `list_chats` | Chats, most recent first: id, title, type, unread count, last-message preview. Paged by `cursor`; `archived` lists the Archive folder. |
| `get_messages` | Messages of one chat, newest first: sender, date, text, reply/forward/edit marks. Paged by `before_id`; `since`/`until` narrow by time; `around_id` gives a window around one message (e.g. a search hit), continued with `before_id` / `after_id`. |
| `get_chat_info` | Title, type, @username, bio/description and member count of one chat. |
| `search_messages` | Telegram's own search by words (not meaning), newest first: across all chats, or in one with `chat_id`. Each hit: its chat (id, title, type), message id, date, sender, a ~300-character snippet around the match and a media marker; read it in context with `get_messages` `around_id`. Narrow with `since`/`until` and `media`; across chats with `chat_type` and `archived`, in one chat with `from` (a sender, or `me`). Paged by `cursor`, at most 50 hits per page, no total count. |

Chat ids look like `user:123~a-MGLzhi9Ek`, `chat:456` (basic group) or `channel:789~18YMXnDF6JI` (channels and supergroups), as returned by `list_chats` and in message senders; the part after `~` is the access hash Telegram needs, so an id works without the server looking it up. Ids without it (`user:123`) are still accepted. Every tool is marked read-only and has bounded output: at most 100 items per call, long texts cut, media shown as `[photo]`, `[voice]`, ….

## Disconnecting

End the **Unofficial MCP Server** session in Telegram → **Settings → Devices**. That logs the server out for real; the AI client gets a 401 on its next call.

Removing the server in your AI client alone is **not** enough: e.g. Disconnect in claude.ai's connector settings only forgets the tokens on its side, and without a token the server can't decrypt (or log out) your session — by design, see below. Clients that call the OAuth revocation endpoint (`/revoke`, RFC 7009) do log the session out.

## Security model

- **A database dump is useless.** Your Telegram session is encrypted (XChaCha20-Poly1305) with a per-login key. That key is never stored: each OAuth token your client holds carries the secret needed to unwrap it, and the server keeps only a verifier and a wrapped copy per token. Without one of your tokens, nobody, including the operator, can decrypt the session. ([ADR-0003](docs/adr/0003-session-key-lives-only-in-tokens.md))
- **Restarts don't need a re-login**, because the encrypted session survives in the database and your client's token unlocks it again.
- **No messages on disk.** Chats and messages pass through memory only; the session at rest is just the auth keys and home datacenter. Logs carry ids and sizes, never message content or tokens.
- **Your password never reaches the AI.** QR login and the cloud password happen on this server's own pages.
- **OAuth 2.1** with PKCE (S256), dynamic client registration, single-use authorization codes, rotating refresh tokens bound to their client, and a consent screen that leads with where the login is sent, names the account, and shows the self-declared client name as unverified.
- **Telegram limits are respected.** Requests per account are rate-limited, and Telegram's `FLOOD_WAIT` blocks that account's calls of the same kind (reading, search or file downloads) until it passes instead of being retried around.
- **Anonymous callers can't make it flood Telegram.** The OAuth endpoints are rate-limited per client IP, and only a few logins (each one a Telegram connection) can be in progress at once, server-wide and per IP.

## Self-hosting

You need a server with HTTPS on a public hostname (OAuth requires it) and your own Telegram app:

1. Register an app at [my.telegram.org/apps](https://my.telegram.org/apps) → note `api_id` and `api_hash`. Please keep "Unofficial" in its title.
2. Run the image behind a TLS-terminating reverse proxy that forwards the `Host` header:

```sh
docker run -d --name tg-mcp --restart unless-stopped \
  -p 127.0.0.1:8080:8080 \
  -v tg-mcp-data:/data \
  -e TG_MCP_PUBLIC_URL=https://tg.example.com \
  -e TG_MCP_DB_PATH=/data/tg-mcp.db \
  -e TG_MCP_API_ID=123456 \
  -e TG_MCP_API_HASH=your_api_hash \
  ghcr.io/qelphybox/tg-mcp:latest
```

If the proxy sets or appends `X-Forwarded-For` from the address it accepted the connection from (Caddy and kamal-proxy do; nginx with `proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for`), also pass `-e TG_MCP_TRUST_FORWARDED_FOR=1`, so the per-IP rate limits of the OAuth endpoints see real client addresses instead of the proxy's.

Images are published for `linux/amd64` and `linux/arm64` on every release tag. All settings are environment variables, documented in [`.env.example`](.env.example).

This repository also deploys itself with [Kamal](https://kamal-deploy.org) to a single VPS (zero-downtime rollouts, automatic Let's Encrypt): see [docs/deploy.md](docs/deploy.md). `scripts/setup-deploy.sh` walks through the one-time setup, including server hardening.

## Development

```sh
cargo test                                   # unit + HTTP-level tests, no network needed
TG_MCP_FAKE_TELEGRAM=1 cargo run             # local server with a fake Telegram account
cargo test -- --ignored live_                # MTProto handshake against real Telegram DCs
```

Rust, tokio, axum, [rmcp](https://github.com/modelcontextprotocol/rust-sdk), [grammers](https://github.com/Lonami/grammers), SQLite. Domain vocabulary is in [GLOSSARY.md](GLOSSARY.md), decisions in [docs/adr/](docs/adr/), contributor and agent conventions in [CLAUDE.md](CLAUDE.md).

## License

MIT, see [LICENSE](LICENSE).
