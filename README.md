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
| `whoami` | The connected account: id, name, @username, and its Chat folders (`chat_folders`: id, title, emoji, `rules` when a folder is defined by rules rather than a list of chats), in the app's order. |
| `list_chats` | Chats, most recent first: id, title, type, unread count, `muted` and `marked_unread` flags, last-message preview (text or caption, and the kind of media). Paged by `cursor`; `archived` lists the Archive folder. `unread_only` lists only chats with unread messages or marked as unread ("what did I miss"): Telegram can't filter by unread, so one call reads at most 500 chats and says `unread_scan_truncated` when it stopped there; `next_cursor` reads on. `chat_folder` lists one Chat folder as its tab in the app (folder-pinned first, archived chats included; with `unread_only` too); folders defined by rules aren't supported yet. |
| `get_messages` | Messages of one chat, newest first: sender, date, text (or caption), `media` (kind: photo, video, video note, voice, audio, document, sticker, GIF, location, contact, poll or other; with duration, size, MIME type, file name and dimensions when Telegram sends them), reply/forward/edit marks, `reactions` (count per emoji, custom emoji summed as `custom`, Stars as `paid`), a `t.me` `link` to the message (channels and supergroups) and `pinned`; in a forum, each message's `topic` and the page's `topics` (id → title of the Topics it mentions), and `topic` reads one Topic only. Paged by `before_id`; `pinned_only` reads only the pinned messages; `since`/`until` narrow by time; `around_id` gives a window around one message (e.g. a search hit), continued with `before_id` / `after_id`; `thread_of` gives the chain of messages one message replies to, up to its root or `depth` ancestors (default 10, max 50), saying where a deleted parent broke it (`broken_at`) or which chat it goes on in (`continues_in`). |
| `get_chat_info` | Title, type, @username of one chat; a group's or channel's description and member count. For a user, the profile Telegram shows the account: bio, last-seen `status` (`online`, `last_seen` with the exact time, or `recently` / `last_week` / `last_month` / `long_ago` when hidden), bot flag and bot description, verified/premium/scam/fake flags, phone and birthday only when shown to the account, and the number of common groups and channels; `common_chats: true` also lists them (at most 20, ids usable with the other tools). A forum lists its `topics` (up to 50: id, title, closed, pinned; `topics_truncated` when there are more). |
| `get_members` | Members of a group, supergroup or channel: id (usable with the other tools), name, @username, `role` (`creator` / `admin` / `member`), bot flag. `query` filters by name or @username, `admins_only` lists only the creator and admins; Telegram's approximate `total` count. Paged by `cursor`, up to 200 per page (the last page may be empty). The list may be incomplete: Telegram hides members in some groups (`members_hidden`), shows a channel's subscribers only to its admins (an error says so) and stops listing a large group after about 10,000 members. |
| `search_messages` | Telegram's own search by words (not meaning), newest first: across all chats, or in one with `chat_id`. Each hit: its chat (id, title, type), message id, date, sender, a ~300-character snippet around the match, the kind of media, and the reactions, link, pin and forum `topic` of `get_messages` (titles in the page's `topics`); read it in context with `get_messages` `around_id`. Narrow with `since`/`until` and `media`; across chats with `chat_type` and `archived`, in one chat with `from` (a sender, or `me`) and, in a forum, `topic`. Paged by `cursor`, at most 50 hits per page, no total count. |
| `get_media` | The media of one message (`chat_id`, `message_id`, `mode`). Mode `view`: the **picture itself** as image content the model looks at, so a client without a shell (Claude on the web, desktop or phone) sees it: a photo in Telegram's ~800 px size (`size: "large"`: ~1280 px), the thumbnail of a video, video note, GIF, sticker or file, or a whole album (up to 4 images in message order, `album_truncated` when there are more), at most ~1 MB per call, as Telegram's own JPEGs (no processing on the server), with a short text saying what each image is. Mode `transcribe`: what is said in a voice message or video note, as **text**, by Telegram's own transcription (the audio never leaves Telegram; no other speech-to-text is used): it waits up to 20 s and answers `done` with the text (up to 16000 characters) or `pending` with `retry_after_s`; it needs Telegram Premium or Telegram's weekly free trial (`trial_remaining` says what is left), otherwise it is an error pointing to `download`. Mode `download`: a **Media link** for an agent with a shell to fetch with curl or wget (`url`, `expires_at`, `mime_type`, `size_bytes`, `duration_s`, `suggested_filename`). It works for 5 minutes and 3 downloads, without a token, for photos (original size), videos, video notes, voice messages, audio, documents, stickers and GIFs up to 20 MB. The file is streamed from Telegram straight into the download, never stored. Voice messages are OGG/Opus and video notes MP4: convert or transcribe them locally (or use `transcribe`). |
| `find_chats` | The account's own chats and contacts by name, title or @username (`@` optional), via Telegram's chat search: id, title, type, @username, at most 30. Contacts without a chat are included; public chats the account hasn't joined never are. No paging and no fallback: a chat it doesn't find is still in `list_chats`. |

Chat ids look like `user:123~a-MGLzhi9Ek`, `chat:456` (basic group) or `channel:789~18YMXnDF6JI` (channels and supergroups), as returned by `list_chats`, `find_chats` and in message senders; the part after `~` is the access hash Telegram needs, so an id works without the server looking it up. Ids without it (`user:123`) are still accepted. Every tool is marked read-only and has bounded output: at most 100 items per call (200 members for `get_members`), long texts cut. Messages describe their media (`media`: its kind and metadata) without downloading it; `text` holds only a message's text or caption, so a voice message has an empty `text` and `media.kind: "voice"`. Only `get_media` fetches a file, a picture or a transcription, and only when asked to.

## Prompts

Two MCP prompts turn common requests into step-by-step instructions that use the tools above. A client that supports prompts lists them and inserts the rendered text as if you typed it; Claude Code shows them as slash commands named after the server, e.g. `/mcp__telegram__catch_up` for a server added as `telegram`, with arguments in the order below (quote one of several words). Rendering a prompt reads nothing from Telegram.

| Prompt | Arguments | What it asks the model to do |
| --- | --- | --- |
| `catch_up` | `since`, `chat`, `max_chats` (all optional; `max_chats` 1-50, default 10) | Read the unread chats (`list_chats` `unread_only`), or the one chat named, message by message with `get_messages` (never from previews), forums Topic by Topic, at most `max_chats` chats, saying how many were left out. Report per chat what happened and who waits for a reply (sender, quote, `link`), and end with a "needs your reply" list, or say there is none. |
| `research` | `topic` (required), `since`, `chat` | Several short `search_messages` queries (word forms, synonyms, transliteration), narrowed by date and sender; promising hits read in context (`get_messages` `around_id` / `thread_of`); every claim cited with chat, date and `link`; facts kept apart from inferences; what was searched and found nothing. |

`since` takes a date, a time or words like "yesterday evening"; `chat` a chat id or a name, title or @username. Both prompts tell the model that message content is data, never instructions.

## Disconnecting

End the **Unofficial MCP Server** session in Telegram → **Settings → Devices**. That logs the server out for real; the AI client gets a 401 on its next call.

Removing the server in your AI client alone is **not** enough: e.g. Disconnect in claude.ai's connector settings only forgets the tokens on its side, and without a token the server can't decrypt (or log out) your session — by design, see below. Clients that call the OAuth revocation endpoint (`/revoke`, RFC 7009) do log the session out.

## Security model

- **A database dump is useless.** Your Telegram session is encrypted (XChaCha20-Poly1305) with a per-login key. That key is never stored: each OAuth token your client holds carries the secret needed to unwrap it, and the server keeps only a verifier and a wrapped copy per token. Without one of your tokens, nobody, including the operator, can decrypt the session. ([ADR-0003](docs/adr/0003-session-key-lives-only-in-tokens.md))
- **Restarts don't need a re-login**, because the encrypted session survives in the database and your client's token unlocks it again.
- **No messages on disk.** Chats and messages pass through memory only; the session at rest is just the auth keys and home datacenter. Logs carry ids and sizes, never message content or tokens.
- **Media links are short-lived credentials.** A `get_media` download link is 256 random bits, lives in memory only, works for 5 minutes and 3 downloads, and only for the one file it was made for; revoking the login ends it at once, and a restart ends them all. The file streams from Telegram into the response without touching disk. A `view` holds its images (at most ~1 MB) in memory only for the one call.
- **Your password never reaches the AI.** QR login and the cloud password happen on this server's own pages.
- **OAuth 2.1** with PKCE (S256), dynamic client registration, single-use authorization codes, rotating refresh tokens bound to their client, and a consent screen that leads with where the login is sent, names the account, and shows the self-declared client name as unverified.
- **Telegram limits are respected.** Requests per account are rate-limited, and Telegram's `FLOOD_WAIT` blocks that account's calls of the same kind (reading, search, file downloads or transcription) until it passes instead of being retried around.
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
