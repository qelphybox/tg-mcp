# tg-mcp

A public remote MCP server that gives AI clients read access to a user's personal Telegram account, and the writes its Grant's Scopes allow, onboarded with a URL and a QR login.

## Identity and access

**Account**:
The user's personal Telegram account that the server reads on their behalf.
_Avoid_: user (ambiguous with Telegram users inside chats), profile

**Grant**:
One authorization of one MCP client to one Account, created by finishing the Login flow. Owns exactly one Telegram session and one Session key. Revoking it logs the Telegram session out.
_Avoid_: connection, authorization, link

**Telegram session**:
The serialized grammers/MTProto state (auth key, DC, peer data) that lets the server act as the Account. Stored only encrypted with the Session key.
_Avoid_: session (unqualified), login

**Session key**:
The random per-Grant symmetric key that encrypts the Telegram session. Never stored in plaintext; recoverable only from a Token.
_Avoid_: master key, secret

**Token**:
An OAuth access or refresh token issued for a Grant. Its secret half unwraps the Session key; the server stores only a verifier and the wrapped key.
_Avoid_: API key, session token

**Wrapped key**:
A copy of the Session key encrypted under a key derived from one Token's secret. One per live Token, and per consumed refresh Token during its Grace window.

**Grace window**:
The 60 seconds after a refresh Token's first use in which it still works like a live one. Presenting it after that revokes its Grant (ADR-0003).
_Avoid_: leeway, reuse interval

## Onboarding

**Login flow**:
The browser part of OAuth authorization: QR login, optional 2FA password, then Consent. Ends with an authorization code.
_Avoid_: signup, auth (unqualified)

**QR login**:
Telegram login by scanning a `tg://login` QR code (`auth.exportLoginToken` / `updateLoginToken`) from an already-logged-in Telegram app.

**Cloud password**:
The Account's Telegram two-step verification password. Asked for on the login page after QR login when the Account has one, checked once via SRP (`account.getPassword` + `auth.checkPassword`), never stored or logged.
_Avoid_: 2FA code, PIN

**Consent**:
The screen where the user chooses the Grant's Scopes (and, with `send`, its Send allow-list) before a Grant is created.

## Writing on the Account's behalf

**Scope**:
A kind of access a Grant has: `read` (always), `draft`, `send`, `organize`. Asked for by the client (`scope` on `/authorize`, pre-checking those boxes), chosen by the user on the Consent screen and stored with the Grant; changing Scopes means a new Grant. A tool needs exactly one Scope and is neither listed nor callable without it. Grants from before Scopes have `read` only.
_Avoid_: permission, role, right

**Send allow-list**:
The Chats a Grant with the `send` Scope may send to, react in or mark as read, chosen on the Consent screen. Nothing outside it can be written to.
_Avoid_: whitelist, allowed chats

**Writes budget**:
How many writes (Drafts, sent messages, reactions, marking as read) an Account may make in any 10 minutes, 20 by default, on top of the request budget every call to Telegram shares. A write over it fails at once with the wait; it never waits or queues.
_Avoid_: write quota, rate limit (unqualified)

**Send cap**:
How many messages the Account may send to one Chat in any minute, 5 by default. Only sent messages count, not reactions, marking as read or Drafts. Counted when the message goes to Telegram, so a send awaiting confirmation doesn't count yet; over the cap it fails at once with the wait and is never queued.
_Avoid_: per-chat limit, throttle

**Draft**:
Unsent text in a Chat's input field of the Account, synced to all its devices: one per Chat, or per Topic in a forum, and a new one replaces it. Only the user sends it.
_Avoid_: pending message, unsent message

## Runtime

**Live client**:
A connected grammers client for one Grant, held in memory with its decrypted Telegram session while the Grant is active; evicted after an idle TTL.
_Avoid_: connection, worker

**Client pool**:
The set of Live clients, keyed by Grant.

**Chat**:
Any Telegram dialog the Account can see: private chat, group, supergroup or channel.
_Avoid_: dialog, conversation, peer (peer is the MTProto term; use it only in code that talks MTProto)

**Chat id**:
How the tools name a Chat: `user:N~H`, `channel:N~H` or `chat:N` (basic group), where `H` is the user's or channel's access hash, so an id is resolved without a lookup. Ids without `~H`, issued before ids carried it, are still accepted and looked up in the dialog list.
_Avoid_: peer id, chat handle

**Archive**:
Telegram's single built-in place for Chats the user moved out of the main chat list. A Chat is either archived or not.
_Avoid_: archived folder, folder (unqualified)

**Chat folder**:
A user-defined, named grouping of Chats ("Work", "Family"), shown as tabs in Telegram apps. A Chat can be in several Chat folders, and in none. Unrelated to the Archive. Defined either as an explicit list (pinned and included Chats, minus excluded ones) or also by rules (kinds of Chats such as contacts or groups, or leaving out muted, read or archived ones). A shared Chat folder (chatlist) is always a list.
_Avoid_: folder (unqualified), filter, tab

**Topic**:
A separate message stream inside a forum supergroup, with its own title. Every message of a forum belongs to exactly one Topic; the tools name it by its id, which is the id of the message that created it, and General, every forum's first Topic, is 1.
_Avoid_: thread (a thread is a chain of replies), subchat

**Rich message**:
A message whose content is page blocks (paragraphs, lists, tables, collapsible sections, photos: Telegram's Instant View blocks, `message.rich_message`) rather than, or besides, its plain text. The tools read it as plain lines after the plain text.
_Avoid_: Instant View (that is a web page's preview), formatted message

**Media link**:
A short-lived URL that lets whoever holds it download the media of one message, without a Token.
_Avoid_: download token, file URL

**Message link**:
A `https://t.me/…` URL that opens one message in the Telegram apps: `t.me/<username>/<id>` in a channel or supergroup with a public @username, `t.me/c/<channel id>/<id>` (members only) in one without. Computed by the server, never fetched; private chats and basic groups have none. Carries no credential, unlike a Media link.
_Avoid_: permalink, share link

**Album**:
Messages sent together as one post (Telegram's `grouped_id`): up to 10 photos, videos or files with consecutive ids in one Chat. get_media `view` shows an album, not just the one message.
_Avoid_: media group, gallery

**Transcription**:
Telegram's own text of what is said in a voice message or video message (`messages.transcribeAudio`), read by get_media `transcribe`. The audio never leaves Telegram. Needs Telegram Premium, or the weekly free trial of an Account without it.
_Avoid_: speech-to-text, STT, recognition

**Search hit**:
One message found by a search, together with its Chat and a snippet of its text around the match.
_Avoid_: result, match

**MCP session**:
The streamable-HTTP session identified by `Mcp-Session-Id`. Unrelated to the Telegram session.
