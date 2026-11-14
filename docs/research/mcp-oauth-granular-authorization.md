# How MCP clients and OAuth handle granular and changing authorization

Research for [qelphybox/tg-mcp#84](https://github.com/qelphybox/tg-mcp/issues/84). Sources read on 2026-11-14.

## Question

What do the MCP authorization specification, OAuth 2.1 and the main MCP clients (Claude Code, claude.ai custom connectors, ChatGPT, Cursor, and others where documented) support for authorization that is finer than a few Scopes and that changes after the first connect? In particular:

- how `scope`, `insufficient_scope` and step-up authorization are handled;
- whether a client runs authorization again when the server asks for more, or only when the connector is removed and added again;
- whether Rich Authorization Requests (RFC 9396) or anything similar is supported;
- what a client does when a refresh Token is revoked or an access Token is rejected;
- whether MCP elicitation (form or URL mode) is a usable place to show or change rules.

The goal behind it: let users edit their access rules (Scopes, Send allow-list, Auto-send) without reconnecting, if clients allow it, with reconnecting as the fallback.

Labels used below: **Documented** = stated in the owner's spec or docs. **Observed** = seen in source code, a changelog or an official issue tracker / forum. **Unknown** = no primary source found.

## Short answer

- **The MCP spec (current revision 2026-07-28) defines scope step-up and nothing finer.** A server answers `403` with `WWW-Authenticate: Bearer error="insufficient_scope", scope="..."`. A client acting for a user SHOULD run authorization again with the union of its earlier scopes and the challenged ones, then retry. Nothing in the spec covers RAR (RFC 9396), RFC 9470 or any other fine-grained mechanism.
- **Clients differ on step-up:**
  - **claude.ai** documents the full loop: an inline prompt, re-authorization, then a retry of the same call.
  - **ChatGPT** documents re-authorization for more scopes. It is triggered by a tool *result* that carries `_meta["mcp/www_authenticate"]`, not by an HTTP 403.
  - **Claude Code** documents that the call fails with "needs additional permissions" and the user re-authenticates from `/mcp`. Since v2.1.288 it also shows a prompt.
  - **Cursor** has no documentation; a forum report says it offers no step-up, only a full log-out and re-consent.
- **In every client, step-up runs the server's whole authorization flow again.** For tg-mcp that means a new Grant and a new QR login. No client uses RAR.
- **The rules don't need to live in the Token.** tg-mcp is its own authorization server, its Tokens are opaque, and every request already looks up the Grant (`src/auth/mod.rs`). Rules stored with the Grant can be changed on the server with no OAuth round-trip, provided the user has a trusted place to change them. The client only has to notice that the tool list changed.
- **Of the in-client surfaces, only URL-mode elicitation suits *widening* rules**, because it can open tg-mcp's own page in the Consent browser. Form-mode answers can be auto-filled by client hooks, and MCP Apps run in a sandboxed iframe without tg-mcp's cookie.

## 1. MCP specification

The current revision is **2026-07-28** ([versioning](https://modelcontextprotocol.io/specification/versioning)). The previous revision was 2025-11-25. Everything below quotes [Authorization, 2026-07-28](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization/index) unless another link is given.

### Scope selection (documented)

- Servers SHOULD put `scope` in the `WWW-Authenticate` header of the `401`.
- On first authorization, clients SHOULD use, in order of preference:
  1. that `scope`;
  2. otherwise all of `scopes_supported` from the protected resource metadata;
  3. otherwise no `scope` at all.
- `scopes_supported` "is intended to represent the minimal set of scopes necessary for basic functionality", with more scopes "requested incrementally through the step-up authorization flow".
- The challenged scopes "MAY match `scopes_supported`, be a subset or superset of it, or an alternative collection". Clients "MUST treat the scopes provided in the challenge as authoritative for the current operation".
- Servers "are not required to surface every dynamically issued scope through `scopes_supported`". The spec therefore allows scope strings minted per Grant, such as one per chat, but says nothing more about them.

### Runtime `insufficient_scope` and step-up (documented)

- **Server side.** On a Token with too little scope, a server SHOULD answer `403 Forbidden` with:
  - `WWW-Authenticate: Bearer error="insufficient_scope"`;
  - `scope="..."` naming the scopes needed for this operation;
  - `resource_metadata`;
  - optionally `error_description`.

  It SHOULD put every scope needed for the operation into a single challenge, and need not repeat the scopes the client already holds.
- **Client side.** "Clients acting on behalf of a user **SHOULD** attempt the step-up authorization flow." The steps are:
  1. parse the challenge;
  2. compute the union of the previously requested scopes and the challenged ones (new wording in 2026-07-28; in [2025-11-25](https://modelcontextprotocol.io/specification/2025-11-25/basic/authorization) this step only pointed back to Scope Selection Strategy);
  3. authorize again;
  4. retry "no more than a few times".
- "Scope accumulation across operations is a client-side responsibility."
- The spec has no mechanism for *narrowing* a Grant, for a server-initiated change, or for consent without an authorization request.

### Rejected Tokens and refresh (documented)

- Invalid or expired Tokens MUST get a `401`, following OAuth 2.1 §5.3 ([spec](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization/index#token-handling)).
- The spec does not say what a client does after a `401` or a failed refresh. That is client-defined.
- Clients MAY ask for `offline_access` when the authorization server lists it. Servers SHOULD NOT put `offline_access` in a `WWW-Authenticate` scope or in `scopes_supported`.

### Elicitation (documented)

Source: [Elicitation, 2026-07-28](https://modelcontextprotocol.io/specification/2026-07-28/client/elicitation).

- **Two modes.** `form` collects structured data through the client. `url` sends the user to a URL "for sensitive interactions that must *not* pass through the MCP client".
- **Form mode limits.** Servers "MUST NOT use form mode elicitation to request sensitive information such as passwords, API keys, access tokens". Clients MUST let users review and change form answers.
- **URL mode limits.**
  - It "is *not* for authorizing the MCP client's access to the MCP server … The MCP client's bearer token remains unchanged."
  - "MCP servers MUST NOT rely on URL mode elicitation to authorize users for themselves."
  - Its intended uses are third-party OAuth, payments and secrets.
  - Clients MUST show the target host and ask for consent before opening it.
  - `accept` means only that the user agreed to open the URL; the outcome reaches the server out of band.
- **Delivery changed in 2026-07-28.** Elicitation is no longer a server-initiated request. It travels inside an `InputRequiredResult` ("Multi Round-Trip Requests"), and the client answers on a retry of the original request. The `notifications/elicitation/complete` notification and `elicitationId` were removed ([changelog](https://modelcontextprotocol.io/specification/2026-07-28/changelog), items 7 and 11).
- **Tool-list caching.** In 2026-07-28, `tools/list` results carry `ttlMs` and `cacheScope` (`public` / `private`) ([changelog](https://modelcontextprotocol.io/specification/2026-07-28/changelog), minor change 5). That matters when the tool list differs per Grant.

### Authorization extensions (documented)

The official [authorization extensions](https://modelcontextprotocol.io/extensions/auth/overview) are OAuth Client Credentials and Enterprise-Managed Authorization. Neither adds fine-grained or per-resource authorization. No MCP spec page or SEP index entry mentions RFC 9396 or RFC 9470 (searched the downloaded 2026-07-28 authorization, security, elicitation and changelog pages and the [llms.txt index](https://modelcontextprotocol.io/llms.txt)).

## 2. OAuth standards

- **RFC 6750 §3.1** ([rfc6750](https://www.rfc-editor.org/rfc/rfc6750#section-3.1)): `insufficient_scope` means "the request requires higher privileges than provided by the access token". The resource server "SHOULD respond with the HTTP 403" and "MAY include the `scope` attribute". `invalid_token` goes with `401`.
- **RFC 6749 §3.3** ([rfc6749](https://www.rfc-editor.org/rfc/rfc6749#section-3.3)): the authorization server "MAY fully or partially ignore the scope requested … based on … the resource owner's instructions". If the granted scope differs from the requested one, it MUST return `scope` in the token response. Scope chosen on a Consent screen is therefore standard.
- **RFC 6749 §6** ([rfc6749](https://www.rfc-editor.org/rfc/rfc6749#section-6)): a refresh request's scope "MUST NOT include any scope not originally granted". **A refresh can narrow access but never widen it.** Widening always takes a new authorization request.
- **OAuth 2.1:** the MCP spec cites [draft-ietf-oauth-v2-1-13](https://datatracker.ietf.org/doc/html/draft-ietf-oauth-v2-1-13). The datatracker shows revision **-17** (2026-10-31) as the latest ([datatracker API](https://datatracker.ietf.org/api/v1/doc/document/draft-ietf-oauth-v2-1/?format=json)). Nothing on scopes or step-up was compared across those revisions.
- **RFC 9470** ([rfc9470](https://www.rfc-editor.org/rfc/rfc9470#section-3)) is *authentication* step-up, not scope step-up. Its error is `insufficient_user_authentication`, with `acr_values` / `max_age`, and it asks for a stronger or more recent login. Neither the MCP spec nor any client doc in this survey mentions it.
- **RFC 9396, Rich Authorization Requests** ([rfc9396](https://www.rfc-editor.org/rfc/rfc9396)): adds an `authorization_details` JSON parameter for fine-grained requests such as "read access to directory A and write access to file X" (§1).
  - "The user may also grant a subset of the requested authorization details" (§2).
  - Authorization servers may let deployments "modify requested authorization details in the user consent process" and support "storage of consented authorization details as part of a grant" (§11.2).
  - The authorization server can return enriched details in the token response (§7.1).

  This matches what tg-mcp's Consent already does with the Send allow-list. **No MCP client in this survey documents `authorization_details`** (searched each client's auth docs listed below).

## 3. Clients

### Claude Code

Sources: [MCP docs](https://code.claude.com/docs/en/mcp), [errors reference](https://code.claude.com/docs/en/errors), [CHANGELOG](https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md).

**Scope on first connect (documented).**
- Without configuration, Claude Code lets the server decide the scope. `oauth.scopes` in the config pins the scopes instead, and takes precedence over discovery ([Restrict OAuth scopes](https://code.claude.com/docs/en/mcp#restrict-oauth-scopes)).
- It appends `offline_access` when the authorization server lists it.

**`insufficient_scope` (documented).** "If the server later returns a 403 `insufficient_scope` for a tool call, the call fails with a `needs additional permissions` message that names the scope the server asks for. The server shows as needing authentication in `/mcp`." ([mcp docs](https://code.claude.com/docs/en/mcp#restrict-oauth-scopes)). The message is:

```
MCP server "<name>" needs additional permissions (scope: "<scope>") — run /mcp to re-authenticate
```

- The user runs `/mcp` and authenticates again. Claude Code then "requests the scope the server named", unless `oauth.scopes` or `authServerMetadataUrl` is set ([errors](https://code.claude.com/docs/en/errors#mcp-server-needs-you-to-sign-in-again)).
- The docs describe requesting the named scope, not a union. The challenge should therefore name every scope the Grant needs.

**Version history (observed).**
- v2.1.85: "Fixed MCP step-up authorization failing when a refresh token exists".
- v2.1.274: a 403 `insufficient_scope` is no longer reported as an expired sign-in.
- v2.1.288: "Added a re-authenticate prompt when an MCP server asks for more OAuth scope during a tool call" ([CHANGELOG](https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md)).
- The docs page still says the call fails. Whether the v2.1.288 prompt also retries the call automatically is **unknown**.
- Earlier, [anthropics/claude-code#44652](https://github.com/anthropics/claude-code/issues/44652) (Claude Code 2.1.92, April 2026) reported that a 403 challenge made Claude Code re-fetch only the resource metadata, never calling `/authorize`. The issue was closed as stale ("not planned") in June 2026, with no maintainer reply. It was read through a web summary because github.com and the GitHub API were blocked from this session.

**SDK underneath (observed in source).**
- Claude Code runs on the MCP TypeScript SDK 1.x (v1 runtime) or 2.0 (v2 runtime, which adds 2026-07-28) ([client runtimes](https://code.claude.com/docs/en/mcp#mcp-client-runtimes)).
- The SDK's Streamable HTTP transport has `onInsufficientScope: 'reauthorize' | 'throw'`, default `'reauthorize'`. That path:
  - computes the union of the previously requested and the challenged scopes;
  - skips the refresh grant when the union exceeds the granted scope ("refresh cannot widen scope per RFC 6749 §6");
  - retries once, with a retry cap ([streamableHttp.ts](https://github.com/modelcontextprotocol/typescript-sdk/blob/main/packages/client/src/client/streamableHttp.ts), [auth.ts](https://github.com/modelcontextprotocol/typescript-sdk/blob/main/packages/client/src/client/auth.ts)).
- To tell whether the union exceeds the granted scope, the SDK reads the `scope` of the token response. tg-mcp already returns it (`src/auth/token.rs`).
- Claude Code's documented behavior (fail, then `/mcp`) looks like the `'throw'` path. That is an inference, not documented.

**Rejected Tokens (documented)** ([authenticate](https://code.claude.com/docs/en/mcp#authenticate-with-remote-mcp-servers)):
- On a `401`, Claude Code refreshes the Token, reconnects and retries once. The server is flagged only if that retry fails too.
- When the refresh Token itself is rejected, Claude Code "immediately shows a notice pointing at `/mcp`", and the user selects **Re-authenticate**.
- No browser opens on its own. In `claude -p` and Agent SDK runs no OAuth flow can run at all.
- Re-authenticate replaces the stored credentials. A v2.1.216 fix stopped it "revoking working credentials before the new sign-in succeeds" ([CHANGELOG](https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md)). That implies the old Token is revoked once the new sign-in succeeds; this is observed, not documented.
- `claude mcp remove` does not revoke Tokens; `claude mcp logout` does.

**Tool list changes (documented).**
- On `notifications/tools/list_changed`, an interactive session fetches the new list "so you don't need to reconnect it" ([dynamic tool updates](https://code.claude.com/docs/en/mcp#dynamic-tool-updates)).
- On the v2 runtime, servers on 2026-07-28 send these notifications over a stream the client holds open.

**Elicitation (documented)** ([elicitation](https://code.claude.com/docs/en/mcp#respond-to-mcp-elicitation-requests)):
- Claude Code supports form and URL mode.
- It declares `elicitation: {form: {}, url: {}}` on 2026-07-28 connections, and on older revisions to servers in the user's own config (v2.1.287+). On claude.ai connectors with older revisions it declares bare `elicitation: {}`, which means form mode only.
- In cloud sessions, URL mode shows a card: **Open link**, then **I'm done, continue**.
- Form answers can be filled in automatically by an `Elicitation` hook "without showing a dialog". A non-interactive session cancels.

**Cloud sessions with claude.ai connectors (documented).** Claude Code runs no sign-in; the user reconnects at claude.ai ([authenticate](https://code.claude.com/docs/en/mcp#authenticate-with-remote-mcp-servers)).

### claude.ai custom connectors (web, Desktop, mobile)

Sources: [Authentication for connectors](https://claude.com/docs/connectors/building/authentication), [Lazy authentication](https://claude.com/docs/connectors/building/lazy-authentication), [Get started with connectors](https://claude.com/docs/connectors/getting-started), [Add an unlisted connector](https://claude.com/docs/connectors/custom/add-unlisted). The authentication page says the same OAuth infrastructure "backs claude.ai, Claude Desktop, Claude mobile, Claude Code, and Cowork".

**Scope on first connect (documented).**
- Claude requests the `scope` from the `401`'s `WWW-Authenticate`, or else `scopes_supported` from the protected resource metadata.
- It appends `offline_access` when the authorization server lists it.
- Discovery metadata is cached globally per URL for about five minutes ([lazy auth, discovery caching](https://claude.com/docs/connectors/building/lazy-authentication#allow-for-discovery-caching)).

**Step-up (documented)** ([Ask for more scope with 403](https://claude.com/docs/connectors/building/lazy-authentication#ask-for-more-scope-with-403)): "Claude prompts the user to re-authorize and, on consent, retries the same tool call with the new token."
- The requested scopes are the union of the `403` challenge and the discovery scope (the first `401` scope, or `scopes_supported`).
- Scopes "picked up in an earlier step-up aren't reliably carried forward into the next one", so the `403` should "list every scope the user still needs".
- Only a `403` with `error="insufficient_scope"` triggers this. Any other `403` is a terminal error, and a `200` with `isError: true` never triggers sign-in.

**Rejected Tokens (documented).**
- Claude refreshes reactively on `401` and proactively up to five minutes before expiry. A dead refresh Token must get `invalid_grant` ([token refresh](https://claude.com/docs/connectors/building/authentication#token-refresh)).
- A transport-level `401` during a call makes Claude "pause the call, run the OAuth flow, and retry", with an inline **Connect** card ([lazy auth](https://claude.com/docs/connectors/building/lazy-authentication)).
- When access is lost, "the connector's row shows **Reconnect**" ([manage](https://claude.com/docs/connectors/getting-started#manage-or-disconnect-a-connector)).
- Whether a revoked refresh Token leads to the inline card or only to the Reconnect row is **unknown**.

**Re-consent without removing (documented).**
- **Disconnect** signs Claude out, and the connector stays listed with **Connect** ([manage](https://claude.com/docs/connectors/getting-started#manage-or-disconnect-a-connector)). A new Consent therefore needs no remove-and-add.
- What can't change after adding is the "authentication settings" (OAuth client credentials, headers). Changing those does need remove-and-add ([add-unlisted](https://claude.com/docs/connectors/custom/add-unlisted)).
- tg-mcp's own acceptance checklist (`docs/acceptance.md` §5 and the #58 prototype note) relies on Disconnect revoking the Grant, which logs the Telegram session out.

**Tool list changes.**
- Documented for developers: none found on claude.com.
- tg-mcp's acceptance checklist (`docs/acceptance.md`, #58 prototype note) tells the tester to disconnect and connect again if claude.ai "still has the old tool list" after a deploy. That suggests claude.ai does not reliably pick up a changed list. Whether it honours `tools/list_changed` is **unknown**.

**Elicitation: unknown.**
- The builder docs list unsupported features (resource subscriptions, sampling, "advanced or draft capabilities") and don't mention elicitation ([build](https://claude.com/docs/connectors/building#decide-what-the-server-exposes)).
- Claude Code's docs show claude.ai connectors on older revisions getting form mode only, but that is Claude Code acting as the client.
- **MCP Apps** are supported: sandboxed UI in the conversation, side panel, embedded websites ([MCP Apps](https://claude.com/docs/connectors/building/mcp-apps/getting-started)). Embedded websites need a Verified listing for directory connectors.

### ChatGPT (apps / plugins, developer mode)

Sources: [Authentication](https://developers.openai.com/plugins/build/auth), [Build an MCP server](https://developers.openai.com/plugins/build/mcp-server), [Connect and test](https://developers.openai.com/plugins/deploy/connect-chatgpt), [reference](https://developers.openai.com/plugins/reference).

**Scope (documented).**
- Each tool declares `securitySchemes`, either `noauth` or `oauth2` with `scopes`, "so the consent screen is accurate".
- `scopes_supported` "helps ChatGPT explain the permissions".
- ChatGPT by default requests the OIDC scopes the authorization server advertises.

**Step-up (documented).**
- The trigger is a tool *error result* that carries `_meta["mcp/www_authenticate"]` with an RFC 7235 challenge, e.g. `error="insufficient_scope"`. The value must include `error` and `error_description`.
- "Without both halves [`securitySchemes` and the resource metadata] ChatGPT will not show the linking UI" ([Triggering authentication UI](https://developers.openai.com/plugins/build/auth#triggering-authentication-ui)).
- "When ChatGPT reauthorizes an existing link, including to request additional OAuth scopes, it may include the prior OIDC ID token … as `id_token_hint`", so the user need not log in from scratch ([reauthorization](https://developers.openai.com/plugins/build/auth#preserve-login-context-during-reauthorization)).
- Whether a plain HTTP `403 insufficient_scope` also works is **unknown**.

**Rejected Tokens (observed).**
- OpenAI support wrote on the [community forum](https://community.openai.com/t/chatgpt-does-not-re-trigger-oauth-on-401-www-authenticate-for-mcp-tool-calls/1374168) (March 2026): "I would not rely on a bare HTTP 401 WWW-Authenticate response alone to trigger the re-auth UI during a tool call". The documented trigger is the `_meta` error result.
- The docs ask servers to "plan for token revocation, refresh, and scope changes" and to treat stale Tokens as unauthenticated.
- Another [forum thread](https://community.openai.com/t/oauth-missing-scopes-after-reconnect/1366784) (Nov–Dec 2025) reports that a Disconnect + Connect sent `/authorize` with no `scope` at all. OpenAI support blamed cached discovery metadata.

**Tool list changes (documented).** After changing tools, schemas or auth, the user opens the connection in ChatGPT and selects **Refresh**, then starts a new conversation ([refresh metadata](https://developers.openai.com/plugins/deploy/connect-chatgpt#refresh-metadata)). A list that changes per Grant at runtime is not a documented scenario.

**Elicitation (documented, thin).**
- "Use MCP elicitation when the server needs structured information … Do not use it to collect secrets or bypass normal authentication" ([mcp-server](https://developers.openai.com/plugins/build/mcp-server#tool-annotations-and-elicitation)).
- Which modes are supported is **unknown**.

The OpenAI Help Center article on connectors ([12584461](https://help.openai.com/en/articles/12584461-connectors-in-chatgpt)) returned HTTP 403 to this session and was not read.

### Cursor

Source: [Cursor MCP docs](https://cursor.com/docs/mcp).

**Scope (documented).**
- Optional `auth.scopes` in `mcp.json`. Without it Cursor uses `scopes_supported` from `/.well-known/oauth-authorization-server`, which is the *authorization server* metadata, not the protected resource metadata.
- OAuth runs with PKCE and RFC 9728 / RFC 8707.
- Cursor renews with the refresh Token. Without one, "you sign in again when the token expires".

**Step-up: not documented.**
- A [forum bug report](https://forum.cursor.com/t/mcp-step-up-consent-not-exposed-for-insufficient-scope/152707) (Cursor 2.4.21, February 2026) says: "No in-session re-consent path is presented; user must fully log out and redo consent".
- Staff replied they would pass it on; no fix is recorded.
- A [separate report](https://forum.cursor.com/t/cursor-cloud-agents-bug-in-oauth-scope-handling/160396) found that Cloud Agents sent `scope: null`. Staff said this was fixed in September 2026.

**Rejected refresh Token: unknown.** Sign-out is per surface: desktop and Cloud Agents keep separate sign-ins.

**Elicitation and MCP Apps (documented).** Both are listed as "Supported". The modes are not stated.

### Others with documented behavior

**MCP Inspector (documented)** ([Inspector authorization](https://modelcontextprotocol.io/docs/2026-07-28/tools/inspector/authorization)):
- Handles a mid-session `401` (re-run the flow, then retry) and a `403 insufficient_scope` ("re-authorizes for the union of the held and required scopes") without dropping the connection.
- The web client shows an "Additional permissions required" dialog; the CLI asks `Proceed with step-up authorization? [y/N]`.
- It is the reference client for the spec's behavior.

**VS Code (Copilot): unknown.** Its [MCP page](https://code.visualstudio.com/docs/agent-customization/mcp-servers) says nothing on OAuth scope, step-up or elicitation, and no other primary source was checked.

## 4. Per-client summary

| | Step-up on `insufficient_scope` | Re-consent without removing the connector | Rejected access Token | Revoked refresh Token | Tool list changes at runtime | Elicitation | RAR |
|---|---|---|---|---|---|---|---|
| **MCP spec 2026-07-28** | SHOULD re-authorize with union of scopes, retry a few times | n/a | MUST be `401` | not specified | `list_changed`; `ttlMs` / `cacheScope` | form + URL (via `InputRequiredResult`) | not referenced |
| **Claude Code** | Documented: call fails, "needs additional permissions", user runs `/mcp`; observed: prompt since v2.1.288; requests the named scope, no documented union | Yes: `/mcp` → Re-authenticate, or `claude mcp login` | Documented: refresh, reconnect, retry once | Documented: notice pointing at `/mcp`; no automatic browser | Documented: refetches on `list_changed` (interactive) | Documented: form + URL (URL on 2026-07-28 and user-config servers); hooks can auto-answer | Unknown / none documented |
| **claude.ai** | Documented: prompt, re-authorize (union of 403 + discovery scope), retry same call | Documented: Disconnect → Connect; remove only for client-credential changes | Documented: inline Connect card, then retry | Documented: **Reconnect** on the row; inline behavior unknown | Unknown; project's checklist expects reconnect | Unknown; MCP Apps documented | None documented |
| **ChatGPT** | Documented: via `_meta["mcp/www_authenticate"]` in a tool error result; HTTP 403 unknown | Documented: "reauthorizes an existing link, including to request additional OAuth scopes" | Observed: a bare HTTP 401 is not a reliable trigger | Unknown | Documented: manual **Refresh** | Documented in passing; modes unknown | None documented |
| **Cursor** | Not documented; observed: none, full log-out needed | Observed: log out, then log in | Unknown | Unknown | Unknown | Documented: supported (modes unknown); MCP Apps supported | None documented |
| **MCP Inspector** | Documented: dialog / CLI prompt, union, retry | Yes | Documented: re-run flow, retry | Documented as part of the 401 path | n/a | n/a | None documented |

## 5. Elicitation and other in-client surfaces for showing or changing rules

- **Form mode.**
  - Can *show* the current rules and collect a choice inside the client.
  - Weak for *widening* access. The answer comes from the client, and Claude Code documents an `Elicitation` hook that answers "without showing a dialog". That is the same reason ADR-0005 rejected client approval prompts as the confirmation mechanism.
  - Form mode is also not documented for claude.ai or ChatGPT, and Claude Code gives claude.ai connectors (older revisions) form mode only.
- **URL mode.**
  - Sends the user to tg-mcp's own page. That page can require the Confirmation cookie that only the Consent browser holds, the same mechanism `/confirm` already uses.
  - The spec says URL mode is not for authorizing the client's access to the server and that servers MUST NOT rely on it "to authorize users for themselves". Whether editing an existing Grant's rules on our own page falls under that sentence is a judgement call. The Token does not change, so the client-to-server authorization is untouched.
  - The only client documented with URL mode for a remote server is Claude Code.
  - On 2026-07-28 the client learns nothing about the outcome until it retries the original request.
- **A link in a tool result**, as `pending_confirmation` does today, works in every client and needs no capability.
- **MCP Apps** (claude.ai, Cursor) render in a sandboxed iframe that "can't … steal cookies" ([MCP Apps overview](https://modelcontextprotocol.io/extensions/apps/overview)). An app could *show* rules, but it can't prove the user is in the Consent browser. Its tool calls are also available to the host, so nothing it does can be told apart from the model.

## 6. What this means for tg-mcp

These are implications only; decisions belong in an ADR.

1. **Where tg-mcp stands today (code).**
   - The `401` challenge carries `resource_metadata` and `error` but no `scope` (`src/auth/mod.rs`, `unauthorized`). Clients that follow the spec therefore request all of `scopes_supported` (`read draft send organize`), and Consent pre-checks what was asked.
   - No path returns `403 insufficient_scope`.
   - Tools outside the Grant's Scopes are not listed, so the model never calls one and nothing would ever trigger a step-up.
   - `/token` ignores `scope` on refresh and returns the Grant's Scopes (`src/auth/token.rs`). That matches RFC 6749 §6.
2. **Step-up is a reconnect in disguise.**
   - In every client, step-up runs `/authorize` again: in tg-mcp terms, the whole Login flow, a new Grant and a QR login. The old Grant is revoked only if the client revokes it.
   - It works inside the conversation on claude.ai, behind `/mcp` in Claude Code, through `_meta` in ChatGPT, and not at all in Cursor.
   - It only *adds* scopes. Removing a chat from the Send allow-list, or switching off Auto-send, has no OAuth signal at all.
3. **Rules can change without OAuth.**
   - Tokens are opaque handles to a Grant, and the server reads the Grant on every request. Scopes, the Send allow-list and Auto-send can therefore change server-side, and the Token stays valid.
   - The constraint is ADR-0003: the allow-list is sealed with the Session key, which only a Token unwraps. An edit page therefore has to be opened from a Token-bearing request, the way a Pending send is.
   - The user would then edit in the Consent browser (Confirmation cookie), reached by URL elicitation or by a link.
   - The client only has to see the new tool list. Claude Code refetches on `list_changed`; claude.ai and ChatGPT may need a reconnect or a manual Refresh.
   - Narrowing the rules is safe in any client. Widening has to go through tg-mcp's own browser page, never a form answer or the model.
4. **Reconnect remains the fallback in every client.**
   - In claude.ai it is Disconnect → Connect, without removing the connector.
   - In Claude Code it is `/mcp` → Re-authenticate.
   - In ChatGPT it is a reconnect, plus Refresh for tools.
   - In Cursor it is log out, then log in.
5. **If a step-up path is added anyway.** The challenge should name *every* scope the Grant needs, not only the missing one: Claude Code requests the named scope, and claude.ai warns that earlier step-ups are not reliably carried forward. ChatGPT needs the `_meta["mcp/www_authenticate"]` form inside a tool result.

## Open uncertainties

- Whether Claude Code's v2.1.288 "re-authenticate prompt" retries the failed call on its own, and whether it requests the union of scopes. The docs and changelog disagree on the details.
- Whether claude.ai honours `notifications/tools/list_changed` or a 2026-07-28 `ttlMs`. The project's checklist suggests it doesn't refresh without a reconnect, but no claude.com page says so.
- Whether claude.ai (web/Desktop) supports elicitation at all, and in which modes.
- Whether ChatGPT reacts to a transport-level `403 insufficient_scope`, which elicitation modes it supports, and what it does on `invalid_grant`. The Help Center article returned 403 and was not read.
- Cursor's behavior on a revoked refresh Token, on `list_changed`, and which elicitation modes it supports.
- Whether any client revokes the old Grant when step-up or re-authentication issues a new one. Claude Code's v2.1.216 changelog implies it does; nothing was found for the others.
- VS Code and other clients were not covered beyond a docs page with no relevant content.
- The current state of [anthropics/claude-code#44652](https://github.com/anthropics/claude-code/issues/44652) and related issues was read through a web summary; github.com and the GitHub API were not reachable directly from this session.
- Whether "MCP servers MUST NOT rely on URL mode elicitation to authorize users for themselves" is meant to cover changing an existing Grant's rules on the server's own page. The spec text doesn't address it.
- Whether the OAuth 2.1 draft changed anything relevant between -13 (cited by MCP) and -17 (latest). Not compared.
