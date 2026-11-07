# tg-mcp

A public remote MCP server that gives AI clients read-only access to a user's personal Telegram account, onboarded with a URL and a QR login.

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
A copy of the Session key encrypted under a key derived from one Token's secret. One per live Token.

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
A kind of access a Grant has: `read` (always), `draft`, `send`, `organize`. Chosen by the user on the Consent screen; changing Scopes means a new Grant.
_Avoid_: permission, role, right

**Send allow-list**:
The Chats a Grant with the `send` Scope may send to, react in or mark as read, chosen on the Consent screen. Nothing outside it can be written to.
_Avoid_: whitelist, allowed chats

**Draft**:
Unsent text in a Chat's input field of the Account, synced to all its devices. Only the user sends it.
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
A user-defined, named grouping of Chats ("Work", "Family"), shown as tabs in Telegram apps. A Chat can be in several Chat folders, and in none. Unrelated to the Archive.
_Avoid_: folder (unqualified), filter, tab

**Topic**:
A separate message stream inside a forum supergroup, with its own title. Every message of a forum belongs to exactly one Topic.
_Avoid_: thread (a thread is a chain of replies), subchat

**Media link**:
A short-lived URL that lets whoever holds it download the media of one message, without a Token.
_Avoid_: download token, file URL

**Search hit**:
One message found by a search, together with its Chat and a snippet of its text around the match.
_Avoid_: result, match

**MCP session**:
The streamable-HTTP session identified by `Mcp-Session-Id`. Unrelated to the Telegram session.
