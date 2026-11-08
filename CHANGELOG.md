# Changelog

## Unreleased

### New

- **Scopes.** Each connection (Grant) has Scopes, chosen on the consent screen: `read`, always, and `draft` (writing Drafts; no tool uses it yet). `send` and `organize` are listed in `scopes_supported` of both OAuth metadata documents but not offered yet: asking for them grants nothing more. `/authorize` takes `scope` (space-separated; an unknown one is `invalid_scope`), and the boxes it asks for start ticked; the token response's `scope` says what was granted, and a refresh keeps it whatever it asks for. Other Scopes mean connecting again.
- `tools/list` lists only the tools a connection's Scopes allow; calling another one is an error that names the missing Scope. The server's `instructions` and the consent screen say "read-only" only while no write Scope is granted.
- Connections made before Scopes become `read` and keep working: no re-login.
- MCP clients that ask for every advertised Scope (the MCP spec's default when the 401 names none) open the consent screen with "Write Drafts" ticked; untick it to stay read-only.

### Security

- **A reused refresh Token revokes its Grant.** A refresh Token keeps working for 60 seconds after its first use (two client processes refreshing at once, or a retry after a lost response, both get a working pair). Presented after that, or by a client it wasn't issued to, it revokes the whole Grant: every Token stops working (`/mcp` → 401) and, if the server holds a Live client for it, its Telegram session is logged out. The client sees the usual `invalid_grant`; the server logs `refresh_reuse` or `foreign_client` with the Grant id, never a Token. A wrong secret for a used Token revokes nothing.
- A used refresh Token's Wrapped key is deleted from the database within a few minutes; SQLite's `secure_delete` is on, so deleted keys leave the file.

### Operations

- Migration `0003_consumed_refresh_tokens` rebuilds the `tokens` table. An image built before it does not start on a database that ran it, so `kamal rollback` past this release fails: fix forward instead.
- Migration `0004_grant_scopes` adds `grants.scopes` (plaintext, `read` for every existing Grant). Same rollback rule.
- New settings for the write tools to come (no tool writes yet): `TG_MCP_ACCOUNT_WRITES_PER_10_MIN` (default 20), the Writes budget, writes per account in any 10 minutes on top of the request budget; `TG_MCP_CHAT_SENDS_PER_MINUTE` (default 5), the Send cap, messages sent to one chat in any minute. A write over either, or in a `FLOOD_WAIT` of writes, fails at once with "retry in N s": writes never wait inside a call, are never retried and never queued. Telegram's `PEER_FLOOD` (the account is limited for spam) is reported as such, never retried. Reading is unaffected.

## 0.1.3

### Breaking: message media is a field of its own

- `get_messages` messages carry `media` as an object: `{ kind, duration_s?, size_bytes?, mime_type?, file_name?, width?, height? }`. `list_chats` previews and `search_messages` hits carry `{ kind }`.
- `text` is only a message's text or caption, possibly empty. The old placeholders (`[photo]`, `[voice]`, `[file]`, …) are gone.
- **Reconnect the connector after upgrading.** MCP clients (claude.ai, Claude Code, others) keep the tool list they fetched when the connector was added. Until it is reconnected (claude.ai: Settings → Connectors; Claude Code: a new session), a client checks answers against the old schemas: `search_messages` fails output validation on hits with media, and new tools, modes and parameters (`get_media` `view` / `transcribe`, `get_members`, `pinned_only`, `topic`) are unknown to the model.

### New

- **`get_media`**, one tool for a message's media, in three modes:
  - `download`: a Media link, a short-lived URL (5 minutes, 3 downloads, in memory only) that an agent with a shell fetches with curl. Files up to 20 MB, streamed from Telegram, never written to disk.
  - `view`: the picture itself as image content, for clients without a shell (claude.ai web, desktop and mobile). Photos, thumbnails of videos, video messages, GIFs and stickers; whole albums up to 4 images; about 1 MB per call.
  - `transcribe`: what a voice message or video message says, transcribed by Telegram itself (`messages.transcribeAudio`); the audio never leaves Telegram. Needs Telegram Premium or Telegram's free weekly transcriptions.
- **`get_members`**: a group's members and admins, with roles; says when Telegram hides them.
- **`get_chat_info` on a user**: bio, phone and birthday as Telegram shows them to the account, last-seen status, and common chats on request.
- **Forum Topics**: `get_chat_info` lists a forum's Topics, each message says its Topic, and `get_messages` / `search_messages` read one Topic.
- **Reactions, links and pins** on messages: `reactions` counts, a `t.me` `link` for channel and supergroup messages, `pinned`; `get_messages` `pinned_only` reads only the pinned messages.

### Fixed

- A sender id at the newest edge of a chat comes with its access hash, so it works as a chat id at once.
- The account's own messages in Saved Messages are outgoing, not from a sender; forwards there show the original author.
- A window read at a chat's newest message reads the top again instead of coming back short.

### Operations

- kamal-proxy no longer buffers responses (`proxy.buffering.responses: false`), so media files are passed through, not spooled to its disk.
- New settings: `TG_MCP_MEDIA_PER_MINUTE`, `TG_MCP_MEDIA_BURST` (per-IP rate limit of `/media/…`).
