# Write access: Scopes, Send allow-list, human confirmation

v0.2.0 lets the AI act on the Account, and every message it reads is attacker-controlled input: one crafted message in any group can try to make the model send or leak as the user. So writing is split into Scopes the user picks on the Consent screen (`read` always, `draft`, `send`, `organize`; changing them means a new Grant), tools outside the granted Scopes are not even listed, and `send` only works inside a Send allow-list of existing dialogs chosen at Consent and stored sealed under the Session key. Each send is confirmed by the human on our own confirmation page, which only works in the browser that gave Consent (a cookie the model's shell doesn't have) and is offered through URL elicitation where the client supports it; elsewhere the page can turn the message into a Draft, which the user sends from Telegram. Drafts are the first write feature because the human always presses send.

## Considered Options

- Rely on the MCP client's approval prompts: no mechanism works across clients (`destructiveHint` has no documented effect in Claude Code, claude.ai connector defaults are undocumented, Research mode calls tools without approval).
- Elicitation only: the model never sees the link, but `send` would then work in one client.
- One `write` Scope: simpler, but "may write Drafts" would imply "may send messages".

## Consequences

- A pending send lives in memory only, for 10 minutes, holding a reference to the Grant's Live client (like an authorization code or a Media link, ADR-0003); a restart drops it. It is decided once, and keeps the `random_id` it will be sent with, so a retry after Telegram didn't answer can't send it twice.
- Confirmation is possible only in the Consent browser; other devices fall back to "make it a Draft". That browser is recognised by a Confirmation cookie set by the Consent that grants `send` (HttpOnly, SameSite=Strict, path `/confirm`, as long-lived as a refresh Token and renewed when used); the Grant keeps only its SHA-256 verifier, sealed with the Send allow-list, so it survives Token rotation, ends with the Grant, and a database writer can't substitute a cookie. A link followed from another site arrives without the SameSite=Strict cookie: the page reloads itself once from our own origin, which carries it.
- The page sends with a CSRF token under a CSP that allows no script, no framing and forms only to itself; it and its decisions are rate-limited per client IP.
- URL elicitation is used when the client declared it and its protocol lets a server send it requests (before 2026-07-28); the tool call then waits for the decision, up to the 10 minutes. Declining the elicitation cancels the send. Any other client gets the link (`pending_confirmation`), and the model checks the outcome later with `get_messages` / `get_drafts`.
- A confirmed send goes through the Writes budget and the Chat's Send cap when it goes to Telegram: refused there, it is decided as not sent, and both the page and the tool say when to retry. A send that got no answer from Telegram stays pending for another press.
- A Grant may opt out of confirmation for its Send allow-list at Consent (off by default); reactions and mark-as-read never need it.
- No forwarding, no messages to people without an existing dialog (also protects the Account from Telegram's anti-spam limits), a separate writes budget and a per-Chat send cap.
- Write actions are logged with ids only; no write log is stored in the database.
- Never in scope: group administration, contacts, profile and privacy, joining or leaving Chats, bot inline buttons, payments, secret chats.
