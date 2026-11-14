# Writes are approved by the client: no confirmation page

Supersedes the confirmation part of ADR-0005: the confirmation page, the Confirmation cookie, URL elicitation and the per-Grant Auto-send opt-out. tg-mcp is an MCP server: once the user grants a write Scope at Consent, `send_message` sends at once, and whether a human approves each call is the MCP client's business (its permission prompts, auto-approve settings, classifiers), not ours. The page sent the user to a browser for every message, worked only in the browser that gave Consent, and on a phone could only turn the message into a Draft: it got in the way more than it protected. Authority is granted once, as rules at Consent, and those rules stay enforced on our server: the Scopes, the Send allow-list (only Chats the Account already has a dialog with), the writes budget, the per-Chat Send cap, `FLOOD_WAIT` honoured and never forwarding (ADR-0005).

## Considered Options

- Keep the page, opt-in per Grant (as Auto-send was, the other way round): keeps all the page's machinery and its costs for a protection most users switch off.
- Keep it only where the client supports URL elicitation: `send` would then behave differently per client, and the same residual risk remains everywhere else.

## Consequences

- **Residual risk, accepted:** every message the agent reads is attacker-controlled input, and one crafted message can try to make it send as the user. Only the client now stands between that and a sent message, and some clients call tools without asking (claude.ai's Research mode, auto-approve settings). What bounds the damage is ours: a message goes only into the Send allow-list the user picked, at most 5 per Chat a minute and 20 writes per 10 minutes for the Account, never the same text twice a minute, text only, never forwarded, never to someone without an existing dialog.
- Tool annotations ask clients to check with the user: `send_message` is `destructiveHint: true` (a sent message can't be taken back by us) and `openWorldHint: true` (other people read it); `react` and `mark_read` are `openWorldHint: true` (others see them) and not destructive; `save_draft` stays closed-world, since only the user sees a Draft. Nothing on the server relies on any of this: a client that ignores the hints gets exactly the same limits.
- `send_message` answers `status: "sent"` with the message id, or an error; there is no `pending_confirmation`, `confirm_url`, `drafted`, `cancelled` or `expired` any more. `/confirm/...` is gone (404), with its rate limit (`TG_MCP_CONFIRM_*` is ignored).
- Grants made before this keep working and send at once, whether or not they had Auto-send: their sealed Send allow-list still opens, and its Auto-send flag and Confirmation cookie verifier are ignored. Lists sealed from now on carry only their Chats (format version 3). A `tgm_confirm_…` cookie left in a browser is no longer read and expires on its own.
- Nothing about a send is held in memory between calls except the duplicate guard's keyed hashes, now one guard per server shared by every MCP session.
- The broader model (per-Scope Chat allow-lists, kinds of action, a journal: the access-rules map #81) is a later ADR that will supersede the rest of ADR-0005; this one records only the removal of the confirmation.
