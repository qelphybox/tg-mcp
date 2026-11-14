# v0.2.0 acceptance run: write tools, in one pass

One ordered run through acceptance §50–§57 (`docs/acceptance.md`) against production, after every v0.2.0 ticket is deployed (#16, #41–#47). This file only sets the order, shares the connectors and database dumps between sections and says where each check goes; the commands and expectations stay in the sections. Record the results in the table at the end and in the release notes.

Order matters in three places:

- **Logs.** Each section's "server's side" step greps the last 15 minutes. Do it at the end of the block it belongs to, as listed below, not at the very end.
- **Limits last.** The Writes budget (20 writes per account in any 10 minutes, §52 step 4) and the Send cap (5 messages per chat in any minute, §52 step 5, §55 step 11) are spent on purpose. They are block F; after the Writes budget run, the account can't write through tg-mcp for 10 minutes.
- **Time.** Block G needs at least an hour of normal use after the logins (refresh Tokens rotate hourly).

About 2 hours of hands-on time plus the hour of block G. 9 QR scans.

## What you need

- A local clone on `main` (`git checkout main && git pull`), bash, and `.env` loaded (below).
- Telegram on a phone and on Telegram Desktop; a desktop browser for Claude Code's logins (the **Consent browser**: all Claude Code connectors below are authenticated in it).
- A second Telegram account, or a friend, who can post in A and in the forum.
- Chats (names used throughout):
  - **A**: a chat where test messages, Drafts and reactions do no harm (a friend who agreed, or a test group).
  - **B**: another chat of yours that is in no Send allow-list below.
  - **the forum**: a supergroup of yours with Topics, a Topic "Releases" (id `R`) with a message in it (id `M`), and General (topic 1).
  - **G**: a group you administer, with a few messages, reactions set to only 👍 (G → Edit → Reactions → "Some reactions").
  - **Work**: a Chat folder that is a plain list of chats (§54), **Old**: a chat outside the 100 most recent, **Carol**: a contact you never wrote to, and the name of a public channel you are not in.

## 0. Setup

```bash
bash
set -a; . ./.env; set +a
SSHD="ssh -i $HOME/.ssh/tg-mcp-deploy -o IdentitiesOnly=yes ${KAMAL_SSH_USER:-deploy}@$KAMAL_HOST"
logs() { $SSHD "docker logs --since ${1:-15m} \$(docker ps -qf label=service=tg-mcp) 2>&1"; }
dump() { rm -rf /tmp/tgmcp-dump && mkdir -p /tmp/tgmcp-dump && $SSHD 'docker run --rm -v tg-mcp-data:/data:ro busybox tar -C /data -cf - .' | tar -xf - -C /tmp/tgmcp-dump; }
```

The `deploy` user only takes the deploy key that `scripts/setup-deploy.sh` made (`~/.ssh/tg-mcp-deploy`), not your personal key; where a section writes `ssh … deploy@$KAMAL_HOST '…'`, `$SSHD '…'` is the same. `logs` takes an optional window (`logs 40m`): steps with several QR logins easily take longer than the sections' 15 minutes, so widen it rather than miss lines. `dump` replaces the `mkdir … | tar` lines of the sections; their Python snippets then read `/tmp/tgmcp-dump/tg-mcp.db`.

## A. Before any new login (no QR)

1. **§51 step 1**, the Grant from before: `whoami` through your regular connector, then `dump` and the snippet. The migration version is now **5** (§51 says 4: that was before #44), and every Grant is still `read` or what you granted since #41.
2. **§54 step 1**: the same dump, its snippet (no Grant has a Send allow-list yet).
3. **§51 steps 2–3**: metadata and `invalid_scope` (curl only).
4. **§52 steps 1–3**: defaults in the container, reading is unaffected, its logs.
5. **§55 steps 1–2**: an unknown `/confirm` link, the page's rate limit. **Wait a minute** after step 2 before anything else from this network.

## B. Scripts (3 QR)

6. **§50** (`scripts/refresh-reuse-check.py`, 2 QR) passed on 2026-11-08 before the rest of v0.2.0 was deployed. Run it again only if you want the whole release covered in one pass; its log greps right after.
7. **§51 steps 4–6** (`scripts/scope-check.py`, 3 QR). Step 4 is also **§54 step 2**: on that Consent, "Send messages as you" is unticked and opens no chat list. The script also checks that `send_message`, `react` and `mark_read` are listed only with `send`.
8. **§51 step 8**: logs.

## C. Drafts (1 QR)

9. **§51 step 7 and §53 step 1 together.** Connect `telegram-drafts` as §53 step 1 says. Before touching the boxes, **record whether "Write Drafts" opened ticked**: that is §51 step 7's question (did Claude Code ask for every Scope). Then keep it ticked and Allow (§51 step 7 unticks it; here the Grant is needed with `draft`, so its log line says `scopes=read draft`).
10. **§53 steps 2–8**: Markdown on both devices, replace and clear, `reply_to`, the forum rule (**watch step 5**: a Draft in Releases and in General is encoded the TDLib way, not yet seen on real Telegram), length, `get_drafts`, logs.
11. Leave `telegram-drafts` connected for block F.

## D. Send allow-list, confirmation page, send_message, react and mark_read (2 QR)

12. **§54 step 3 + §55 step 3 together: connect `telegram-send`.** Open the browser's developer tools (Network, "Preserve log") before Authenticate (§55 step 3). On Consent, tick "Send messages" and use all three pickers as §54 step 3 says; the list you Allow must contain **A, the forum and G**, and not B (a folder adds all its chats: pick one without B, or skip it here and use it in step 13 only). Leave "without asking me" unticked.
13. **§54 step 4**: before Allow, untick everything once and press Allow: refused (422). Then choose again as in step 12 and Allow. Check the cookie as §55 step 3 says.
14. **§54 step 6**: connect `telegram-send2`, "Send messages" with **A only**, "without asking me" **ticked**.
15. **One dump for §54 step 5 and §55 step 4**: `dump`, then both snippets (no chat id, no cookie in the files; the list blobs are opaque).
16. **§54 step 7**: logs.
17. **§55 steps 5–10** with `telegram-send` (elicitation in Claude Code, forum reply, Cancel and declining, the phone that can only make a Draft, a link from another site, expiry). **Skip step 11 here** (block F). **Step 12** with `telegram-send2`. Watch step 5 closely: whether Claude Code opens the URL by elicitation, and that a call waiting on the page isn't cut off by Claude Code's own tool timeout.
18. **§55 step 13**: logs.
19. **§56 steps 1–8** (connectors as above; step 2 points back to §55 steps 5, 7 and 8, already done). Step 3 adds claude.ai as a client: authenticate it in the same desktop browser. Step 9: logs.
20. **§57 with `telegram-send` instead of a new `telegram-react`**: its list already has A, the forum and G, which is all §57 needs. Steps 1–9 (**watch step 9**: marking General read goes through `readDiscussion` with topic 1, not yet seen on real Telegram), then step 10, logs.

## E. Clean up what blocks C–D left, except the connectors

21. Remove your reactions (§57 step 11), delete the test messages in A, the forum and G, and clear the test Drafts (`save_draft` with `""`, or in the app). Keep `telegram-drafts`, `telegram-send` and `telegram-send2` for block F.

## F. Limits, last

22. **The Send cap: §55 step 11** (confirm 5 messages with different texts to A within a minute, the 6th is refused on the page with "retry in N s"). **§52 step 5** is the same cap through the tool result; one run covers both if you note what the page and the tool each say.
23. **Wait 10 minutes with no writes**, then **the Writes budget: §52 step 4** with `telegram-drafts` (21 `save_draft` calls, the 21st refused with N a little under 600, `list_chats` still answers). After it, nothing can write through tg-mcp for 10 minutes.
24. Logs for steps 22–23 (`logs 30m`): the `send cap` / `writes budget` refusals with ids and seconds only, no text, `grep -c tgm_` is 0.
25. Clean up as §55 step 14, §56 step 10 and §57 step 11 say: remove `telegram-drafts`, `telegram-send`, `telegram-send2` (and claude.ai's test connector from §56 step 3), end their sessions in Telegram → Settings → Devices, delete the remaining test messages and Drafts. The Devices list should end with only your own apps and the regular connector.

## G. After an hour or more

26. **§50, last paragraphs**: `dump` and its snippet (no used refresh Token keeps a Wrapped key 5 minutes after its window), and two Claude Code sessions on your regular connector across an hourly refresh: no `refresh token misused` line for your Grant (`logs 2h`).
27. **§51 step 1, last line**: the regular connector's `whoami` still answers without a browser after its refresh.

## Results

| Block | Sections and steps | Result | Notes |
|---|---|---|---|
| A | §51.1–3, §52.1–3, §54.1, §55.1–2 | | migration version: |
| B | §50 (optional), §51.4–6 + §54.2, §51.8 | | |
| C | §51.7 + §53.1–8 | | Drafts opened ticked in Claude Code: yes / no |
| D | §54.3–7, §55.3–10, 12–13, §56.1–9, §57.1–10 | | elicitation in Claude Code: yes / no; tool timeout seen: |
| F | §55.11 + §52.5, §52.4 | | |
| G | §50 (dump, two sessions), §51.1 refresh | | |

Known open points to look at during the run:

- **Forum Drafts** (§53 step 5) and **marking General read** (§57 step 9) follow TDLib's conventions and were never tried on real Telegram before this run.
- **Claude Code and URL elicitation** (§55 step 5): whether it elicits, and whether a 10-minute wait survives its tool-call timeout.
- **Does Claude Code ask for every Scope** (block C, step 9). If it does, §11 (outside §50–§69) expects "read-only access" in the Consent heading and no longer holds; the option on the table is a `scope="read"` hint in the 401 challenge.
- **No rollback past this release**: migrations 0003–0005 stop an older image from starting. A failure found here is fixed forward.
