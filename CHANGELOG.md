# Changelog

## Unreleased

### New

- **Drafts, the first write.** `save_draft` (Scope `draft`) puts text into a chat's message field on all the account's devices, replacing the Draft there; `""` clears it, and nothing is ever sent: the user sends it from Telegram. Text is a small Markdown subset by default (bold, italic, inline code, code blocks, links; anything else stays as typed) or `format: "plain"`, at most 4096 characters once formatted (longer is an error, never split). `reply_to` replies to a message of the chat; in a forum, `topic` (or a `reply_to` in a Topic) names the Topic, and neither is an error. Any chat of the account's, no allow-list. Each call spends the writes budget; the text is never logged, only ids and sizes. Annotated not read-only, not destructive, idempotent.
- `get_drafts` (Scope `read`): the account's Drafts, newest first, with chat, forum Topic, `reply_to`, date and text (sanitised like message text); at most 100 and ~40,000 characters per call.
- MCP clients keep the tool list they fetched when the connector was added: reconnect it (claude.ai: Settings → Connectors; Claude Code: a new session) to see `get_drafts`, and connect again with "Write Drafts" ticked to get `save_draft`.
- **Scopes.** Each connection (Grant) has Scopes, chosen on the consent screen: `read`, always, and `draft` (writing Drafts with `save_draft`). `send` and `organize` are listed in `scopes_supported` of both OAuth metadata documents but not offered yet: asking for them grants nothing more. `/authorize` takes `scope` (space-separated; an unknown one is `invalid_scope`), and the boxes it asks for start ticked; the token response's `scope` says what was granted, and a refresh keeps it whatever it asks for. Other Scopes mean connecting again.
- `tools/list` lists only the tools a connection's Scopes allow; calling another one is an error that names the missing Scope. The server's `instructions` and the consent screen say "read-only" only while no write Scope is granted.
- Connections made before Scopes become `read` and keep working: no re-login.
- MCP clients that ask for every advertised Scope (the MCP spec's default when the 401 names none) open the consent screen with "Write Drafts" ticked; untick it to stay read-only.
- **`send` is offered on the consent screen, with its Send allow-list.** Ticking "Send messages" and pressing "Choose chats" lists the account's Chat folders that are explicit lists (rule-based ones are left out), its 100 most recent chats, and a search by name that shows only the account's own chats with an existing dialog (never a contact it never wrote to, nor a public chat it isn't in). A ticked folder is read into its chats when you allow. Allow with "Send messages" ticked and no chat chosen is refused and the page says why. "Send to these chats without asking me each time" is off unless ticked. "Send messages" never starts ticked, even when the client asks for `send`. Changing the chats means connecting again.
- No tool sends yet (`send_message`, `react` and `mark_read` come next): a `send` Grant lists the same tools as one without, and the server's `instructions` describe only what the listed tools can do, so it reads as read-only (or Drafts) until then.

### Security

- **The Send allow-list is sealed with the Session key.** A `send` Grant's chats and its auto-send choice are one blob encrypted like its Telegram session (its own AAD, `tg-mcp/send-allow-list/v1:` + Grant id; plaintext padded to 512 bytes): a database dump without a Token shows neither the chats nor, beyond a rough size, how many; a database writer can't turn confirmation off or move a list to another Grant. Logs carry counts (`send_chats`, `auto_send`), never chat ids, titles or search terms.
- The consent screen's chat pickers reach Telegram before any Token exists: they need the login's cookie and a finished Telegram login, go through the account's own request budget, and are rate-limited per client IP (new settings below) and to 30 searches per login.
- **A reused refresh Token revokes its Grant.** A refresh Token keeps working for 60 seconds after its first use (two client processes refreshing at once, or a retry after a lost response, both get a working pair). Presented after that, or by a client it wasn't issued to, it revokes the whole Grant: every Token stops working (`/mcp` → 401) and, if the server holds a Live client for it, its Telegram session is logged out. The client sees the usual `invalid_grant`; the server logs `refresh_reuse` or `foreign_client` with the Grant id, never a Token. A wrong secret for a used Token revokes nothing.
- A used refresh Token's Wrapped key is deleted from the database within a few minutes; SQLite's `secure_delete` is on, so deleted keys leave the file.

### Operations

- Migration `0003_consumed_refresh_tokens` rebuilds the `tokens` table. An image built before it does not start on a database that ran it, so `kamal rollback` past this release fails: fix forward instead.
- Migration `0004_grant_scopes` adds `grants.scopes` (plaintext, `read` for every existing Grant). Same rollback rule.
- Migration `0005_send_allow_list` adds `grants.send_allow_list` (a sealed blob, NULL for every existing Grant and for Grants without `send`). Same rollback rule.
- New settings: `TG_MCP_CONSENT_CHATS_PER_MINUTE` (default 20), `TG_MCP_CONSENT_CHATS_BURST` (default 10), the per-IP rate limit of the consent screen's chat pickers (`/login/{id}/consent/chats`).
- New settings for the write tools: `TG_MCP_ACCOUNT_WRITES_PER_10_MIN` (default 20), the Writes budget, writes per account in any 10 minutes on top of the request budget (`save_draft` is the first write to spend it); `TG_MCP_CHAT_SENDS_PER_MINUTE` (default 5), the Send cap, messages sent to one chat in any minute. A write over either, or in a `FLOOD_WAIT` of writes, fails at once with "retry in N s": writes never wait inside a call, are never retried and never queued. Telegram's `PEER_FLOOD` (the account is limited for spam) is reported as such, never retried. Reading is unaffected.

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
