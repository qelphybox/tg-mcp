---
title: Privacy Policy
permalink: /privacy/
---

# Privacy Policy

**Effective date:** 12 November 2026

This policy covers the hosted tg-mcp service at `https://tgmcp.qelphybox.space` (the "Service"). tg-mcp is an unofficial, open-source [MCP](https://modelcontextprotocol.io) server that lets AI clients you connect (such as Claude or Cursor) read your Telegram account and, only if you allow it, write Drafts on your behalf. It is not affiliated with or endorsed by Telegram.

The Service is operated by [TODO: operator name] ("we", "us"). Contact: [TODO: contact email].

If you run tg-mcp on your own server, that instance is yours: this policy doesn't apply to it, and whoever runs it is responsible for the data on it.

## In short

- **We don't store your messages.** Chats, messages, media and Drafts pass through the server's memory on their way to your AI client and are never written to disk.
- **We can't open your Telegram session.** It is encrypted with a key that only the tokens held by your AI client can unlock. Without one of those tokens, nobody can decrypt it, including us.
- **No analytics, no ads, no selling data, no AI training.** The Service has no trackers, and the only cookie it sets is the one that keeps a login together.
- **You can cut access at any time** in Telegram → Settings → Devices.

## What we process and why

### 1. Your Telegram session

When you log in by scanning a QR code, Telegram issues a session to the Service. We store it so that you don't have to log in again after every server restart. The session holds Telegram's authorization keys and your home data center, and it is stored **only in encrypted form** (XChaCha20-Poly1305), with a key that is never stored on the server. Each OAuth token we issue to your AI client carries the secret that unlocks that key, and we store only a verifier and a wrapped copy of the key per token. The technical design is described in [ADR-0003](https://github.com/qelphybox/tg-mcp/blob/main/docs/adr/0003-session-key-lives-only-in-tokens.md).

### 2. Your Telegram account id

We store your numeric Telegram user id alongside each connection. It is not encrypted. We use it to tie a connection to an account, to apply per-account rate limits that protect your account from Telegram's anti-spam limits, and to find your connections if you ask us to delete them.

### 3. Your connection (OAuth grant)

For each connection we store:

- the permissions you granted on the consent screen: `read`, plus `draft` or `send` if you ticked them;
- the token records described above (id, verifier, wrapped key, expiry);
- creation and update times;
- the id of the AI client the connection was issued to.

If you allow **Send messages**, the list of chats it may send to and your auto-send choice are encrypted with the same key as your Telegram session. We can't read which chats you chose.

AI clients register themselves with the Service automatically. For each one we store the name it declares (shown on the consent screen) and its redirect URLs. These describe the client app, not you.

### 4. Content from your Telegram account

When your AI client calls a tool, the Service asks Telegram for exactly what that call needs and returns it to the client. Depending on the tool, that can be your chat list, messages and their media, contacts, profile information, chat folders, Drafts, members of a group, or search results. This content:

- is processed **in memory only** and is not written to disk or to our database;
- is never logged (logs carry ids, counts and sizes only, see below);
- includes messages and profile data of the **other people** in your chats, which we handle the same way.

**Downloads:** if your AI client requests a file, the Service gives it a single-use download link. The link lives only in memory, works for 5 minutes and at most 3 downloads, and stops working once you revoke the connection or the server restarts. The file streams from Telegram to your client without being saved on the server.

**Transcriptions:** voice messages are transcribed by Telegram itself, which requires Telegram Premium or Telegram's trial quota. The hosted Service does not run its own speech recognition and does not send audio to any third party.

### 5. Writes

The Service writes to your Telegram account only within the permissions you granted:

- **Drafts:** text placed in a chat's message field, which only you can send.
- **Messages** (once a sending tool ships; none does yet): sent only to the chats you chose. Each one is confirmed by you, unless you turned confirmation off.

The text is passed to Telegram and not stored by us.

### 6. Login

The QR login and, if your account has one, your Telegram cloud password are handled on the Service's own pages. Your AI client never sees them. The password is used only to complete the login with Telegram and is never stored or logged. A login in progress, and the one-time authorization code it produces, live in memory for at most 10 minutes and 60 seconds respectively.

### 7. Cookies

We set one cookie, `tgm_login`, scoped to the login pages. It ties the steps of a login to the browser that started it and expires after 10 minutes. It is strictly necessary, and we use no analytics, advertising or third-party cookies.

### 8. Technical logs

To keep the Service running and secure, we log events with identifiers and counts only: Telegram account ids, connection ids, AI client ids, numbers of items, sizes, durations and error codes. We never log message or Draft text, chat titles, search terms, tokens, session keys or Telegram sessions.

What else ends up in logs:

- The IP addresses of requests to the login and OAuth endpoints are used, in memory, for rate limiting, and are logged when a login is refused.
- The reverse proxy in front of the Service keeps an access log with IP address, time, request path and status code.

Logs stay on the server, are rotated by size (at most 5 files of 20 MB per service), with the oldest overwritten first, and are not archived or sent anywhere.

## Who receives data

- **Telegram.** This is necessary to provide the Service: every tool call is a request to Telegram on your behalf, made through the Service's registered Telegram app. See [Telegram's Privacy Policy](https://telegram.org/privacy).
- **Your AI client and its provider.** Whatever a tool returns goes to the AI client you connected, for example Anthropic for Claude or OpenAI for ChatGPT. From there, that provider's terms and privacy policy apply. Choose and configure your AI client accordingly.
- **Our hosting provider** ([TODO: hosting provider], servers in [TODO: country]). They run the server that stores the encrypted data above and the logs. They don't get decryption keys.

We don't sell, rent or share personal data with anyone else, and we don't use it to train AI models. We will disclose data only where the law requires it. Even then, we can't decrypt Telegram sessions or produce message content, because we don't have either.

## How long we keep it

| Data | Kept until |
|---|---|
| Access token | 1 hour |
| Refresh token | 30 days after it was issued; every refresh issues a new one |
| Encrypted Telegram session, Send allow-list, account id, connection record | Deleted once the connection has no valid token left (at most 30 days after your AI client last used it), when you revoke it, or when Telegram reports the session as ended |
| AI client registrations | Kept as long as the Service runs; they contain no personal data |
| Login in progress, authorization codes, download links | Memory only: 10 minutes, 60 seconds, and 5 minutes respectively, and gone on restart |
| Message content | Not stored |
| Logs | Rotated by size, at most 5 × 20 MB per service |

Backups: [TODO: describe backups of the database, or state that there are none].

## Your choices and rights

- **Disconnect:** end the **Unofficial MCP Server** session in Telegram → Settings → Devices. This logs the Service out at once, and your AI client gets an error on its next call. Removing the server in your AI client alone does **not** do this, because most clients just forget their tokens. The encrypted session then sits unusable until it expires within 30 days.
- **Delete your data now:** write to [TODO: contact email] with your Telegram user id or @username. We will delete every connection of that account. We may ask you to confirm by sending us a message from that account.
- **Access and other rights:** depending on where you live (for example under the GDPR or UK GDPR), you may have the right to access, correct, delete, restrict or object to processing of your personal data, and to complain to your data protection authority. Write to us to exercise them. Because of the design above, what we can show you is limited to your account id, your connections and their timestamps.

**Legal basis (GDPR):** we process your data to provide the Service you asked for (Art. 6(1)(b)), and process logs and rate-limiting data for our legitimate interest in keeping the Service secure and available (Art. 6(1)(f)).

## Security

The Service uses HTTPS only and OAuth 2.1 with PKCE. Tokens rotate, and a reused refresh token revokes the whole connection. Telegram sessions and Send allow-lists are encrypted at rest with keys we don't hold, and the server is hardened. No system is perfectly secure. If you find a vulnerability, please report it as described on the [Support](../support/) page.

## Children

The Service is not intended for anyone under 16 or under the minimum age to use Telegram in your country.

## Changes

We will post changes to this policy on this page and update the effective date. Material changes will also be noted in the project's [changelog](https://github.com/qelphybox/tg-mcp/blob/main/CHANGELOG.md). The full history of this page is in the project's Git repository.

## Contact

[TODO: operator name], [TODO: contact email]
