# Write access: Scopes, Send allow-list, human confirmation

v0.2.0 lets the AI act on the Account, and every message it reads is attacker-controlled input: one crafted message in any group can try to make the model send or leak as the user. So writing is split into Scopes the user picks on the Consent screen (`read` always, `draft`, `send`, `organize`; changing them means a new Grant), tools outside the granted Scopes are not even listed, and `send` only works inside a Send allow-list of existing dialogs chosen at Consent and stored sealed under the Session key. Each send is confirmed by the human on our own confirmation page, which only works in the browser that gave Consent (a cookie the model's shell doesn't have) and is offered through URL elicitation where the client supports it; elsewhere the page can turn the message into a Draft, which the user sends from Telegram. Drafts are the first write feature because the human always presses send.

## Considered Options

- Rely on the MCP client's approval prompts: no mechanism works across clients (`destructiveHint` has no documented effect in Claude Code, claude.ai connector defaults are undocumented, Research mode calls tools without approval).
- Elicitation only: the model never sees the link, but `send` would then work in one client.
- One `write` Scope: simpler, but "may write Drafts" would imply "may send messages".

## Consequences

- A pending send lives in memory only, for 10 minutes, holding a reference to the Grant's Live client (like an authorization code or a Media link, ADR-0003); a restart drops it.
- Confirmation is possible only in the Consent browser; other devices fall back to "make it a Draft".
- A Grant may opt out of confirmation for its Send allow-list at Consent (off by default); reactions and mark-as-read never need it.
- No forwarding, no messages to people without an existing dialog (also protects the Account from Telegram's anti-spam limits), a separate writes budget and a per-Chat send cap.
- Write actions are logged with ids only; no write log is stored in the database.
- Never in scope: group administration, contacts, profile and privacy, joining or leaving Chats, bot inline buttons, payments, secret chats.
