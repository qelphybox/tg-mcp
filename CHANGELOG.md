# Changelog

## 0.1.3

### Breaking: message media is a field of its own

- `get_messages` messages carry `media` as an object: `{ kind, duration_s?, size_bytes?, mime_type?, file_name?, width?, height? }`. `list_chats` previews and `search_messages` hits carry `{ kind }`.
- `text` is only a message's text or caption, possibly empty. The old placeholders (`[photo]`, `[voice]`, `[file]`, …) are gone.
- **Reconnect the connector after upgrading.** MCP clients (claude.ai, Claude Code, others) keep the tool list they fetched when the connector was added. Until it is reconnected (claude.ai: Settings → Connectors; Claude Code: a new session), a client checks answers against the old schemas: `search_messages` fails output validation on hits with media, and new tools and modes (`get_media` `view` / `transcribe`, `get_members`, `pinned_only`) are unknown to the model.

### New

- **`get_media`**, one tool for a message's media, in three modes:
  - `download`: a Media link, a short-lived URL (5 minutes, 3 downloads, in memory only) that an agent with a shell fetches with curl. Files up to 20 MB, streamed from Telegram, never written to disk.
  - `view`: the picture itself as image content, for clients without a shell (claude.ai web, desktop and mobile). Photos, thumbnails of videos, video messages, GIFs and stickers; whole albums up to 4 images; about 1 MB per call.
  - `transcribe`: what a voice message or video message says, transcribed by Telegram itself (`messages.transcribeAudio`); the audio never leaves Telegram. Needs Telegram Premium or Telegram's free weekly transcriptions.
- **`get_members`**: a group's members and admins, with roles; says when Telegram hides them.
- **`get_chat_info` on a user**: bio, phone and birthday as Telegram shows them to the account, last-seen status, and common chats on request.
- **Reactions, links and pins** on messages: `reactions` counts, a `t.me` `link` for channel and supergroup messages, `pinned`; `get_messages` `pinned_only` reads only the pinned messages.

### Fixed

- A sender id at the newest edge of a chat comes with its access hash, so it works as a chat id at once.
- The account's own messages in Saved Messages are outgoing, not from a sender; forwards there show the original author.
- A window read at a chat's newest message reads the top again instead of coming back short.

### Operations

- kamal-proxy no longer buffers responses (`proxy.buffering.responses: false`), so media files are passed through, not spooled to its disk.
- New settings: `TG_MCP_MEDIA_PER_MINUTE`, `TG_MCP_MEDIA_BURST` (per-IP rate limit of `/media/…`).
