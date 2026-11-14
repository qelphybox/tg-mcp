# v0.3.0 acceptance run, in one pass

One ordered run through the sections of `docs/acceptance.md` that sprint #110 added or changed:

- §42 (#54);
- §56, §57 and §58 (#72);
- §59 (#61), §60 (#63), §61 (#65);
- §62 (#68), §63 (#60), §64 (#62), §65 (#55);
- §66 (#64), §67 (#101), §68 (#102), §69 (#103), §70 (#104), §71 (#106), §72 (#107);
- the landing page checks (#77, #78 and the copy part of #72).

Production has run every ticket since its own deploy. This file only sets the order, shares the connectors and log windows between sections, and says where each check goes; the commands and expectations stay in the sections. Record the results in the table at the end and in the release notes, then close each ticket with a comment naming its section's result.

The run has three parts, because some checks need the release itself:

1. **Part 1, before the tag:** everything production and a local Docker can show.
2. **Part 2, the tag.**
3. **Part 3, after the tag:** what needs the published release images (Docker Hub, the built-in app, multi-arch tags) and a VPS with a domain.

Order matters in four places:

- **`/forget` comes last of the Telegram blocks.** §60 step 5 deletes **every** connection of the account it runs on, the regular `telegram` connector included. Everything that needs a connector of the main account runs before block G.
- **Logs.** Each section's "server's side" step greps the last 15 minutes. Do it at the end of the block it belongs to, as listed, with a wider window (`logs 40m`) when a block took longer.
- **Rate limits.** §60 step 6 spends `/authorize`'s burst for your IP; §56 step 8 spends A's Send cap for a minute. Both are at the end of their blocks.
- **Time.** §66 needs an hour and a quarter after a deploy: start block H's clock with the deploy of step 0.3.

About 3 hours of hands-on time for Part 1, plus the waiting of block H. About 11 QR scans in Part 1 and 5 in Part 3.

## What you need

- A local clone on `main` (`git checkout main && git pull`), bash, Docker with `buildx` and Compose 2.23.1 or later, `jq`, `sqlite3`, Node.js (for `tests/site/landing-check.cjs` and the MCP Inspector), and `.env` loaded (below).
- Telegram on a phone and on Telegram Desktop; a desktop browser for the logins. This is the **Consent browser**: authenticate every Claude Code connector below in it.
- A **second Telegram account** (a second phone, or a friend) that can post in A and in the forum, and that connects once in block G.
- Chats, as in the v0.2.0 run (names used throughout):
  - **A**: a chat where test messages and reactions do no harm;
  - **B**: another chat of yours that is in no Send allow-list below;
  - **the forum**: a supergroup of yours with Topics, a Topic "Releases" (id `R`) with a message in it (id `M`), and General (topic 1);
  - **G**: a group you administer, with a few messages.
- For §42, §62, §64 and §65: the chats and notes each section's "Needs" paragraph lists (unread chats, 4+ pinned chats, three polls, a channel post with 5+ comments…). Prepare them in the app before block C.
- For Part 3: a VPS with a domain whose A/AAAA record points at it, ports 80/443 open (§71, §72).

## 0. Setup and the maintainer's steps (before Part 1)

1. **Load the helpers.**

   ```bash
   bash
   set -a; . ./.env; set +a
   SSHD="ssh -i $HOME/.ssh/tg-mcp-deploy -o IdentitiesOnly=yes ${KAMAL_SSH_USER:-deploy}@$KAMAL_HOST"
   logs() { $SSHD "docker logs --since ${1:-15m} \$(docker ps -qf label=service=tg-mcp) 2>&1" | sed 's/\x1b\[[0-9;]*m//g'; }
   ```

   `logs` takes an optional window (`logs 40m`) and strips the ANSI colours (§66 needs that). Where a section defines its own `logs`, this one does the same.

2. **§61 step 1, on the server:** run `scripts/harden-server.sh <deploy public key>` twice, then check the four journald settings and `docker info --format '{{.LoggingDriver}}'` as §61 says. Then `kamal proxy reboot` from the clone (a few seconds without service). Note the date on #65.
3. **Deploy the release commit.** Merge the release PR (version 0.3.0, this file). Its Deploy run is the deploy that §61 step 2 checks, that §66's hour counts from, and whose switch §66 step 4 looks at. Note the time `T0` when it finishes.
4. **§66 step 4, right after step 3:** `kamal app logs --since 15m | sed 's/\x1b\[[0-9;]*m//g' | grep 'reason="shutdown"'` shows the old container's last `stats` line, around the switch.
5. **§61 step 2:** the app and kamal-proxy log to journald, and `kamal app logs -n 20` still works. Then **§61 steps 3–4**.
6. **The external monitor (§66 step 5's setup):** set up UptimeRobot on `https://connector.tgmcp.dev/healthz` as `docs/deploy.md` steps 1–7 say, and note the service on #64.
7. **For Part 3, any time before the tag** (the release fails, or skips Docker Hub, without them):
   - the repository secrets `TG_MCP_BUILTIN_API_ID` and `TG_MCP_BUILTIN_API_HASH`, from a **second** Telegram app on a **second** phone number, never production's (#103, ADR-0007);
   - Docker Hub: the public repository `qelphybox/tg-mcp` with its short description set by hand, the `DOCKERHUB_USERNAME` variable, the `DOCKERHUB_TOKEN` secret (Read & Write), and optionally `DOCKERHUB_DESCRIPTION_TOKEN` for the README sync (`docs/deploy.md`, Releases; #104).

# Part 1: before the tag

## A. No QR: curl and a local Docker

1. **§58 steps 1 and 5**: `/confirm/…` answers 404 (fifteen times, no 429), and the privacy policy no longer mentions confirmation.
2. **§59 step 1**: the five icon files, their types, sizes and caching, `cmp` with `site/favicon.svg`.
3. **§60 steps 2 and 8**: `/forget` explains before anything starts; the privacy policy links it. Don't press the button yet.
4. **§68 step 5**: production's issuer and `public_url_source=TG_MCP_PUBLIC_URL`.
5. **§67 steps 1 and 2 (the binary part)**, with production's image `ghcr.io/qelphybox/tg-mcp:<sha of T0's deploy>`: the smoke test's five PASS lines, `statically linked, stripped`, mimalloc linked. The multi-arch part of step 2 waits for Part 3.
6. **§68 steps 1–4** with the same image: the four platforms' issuers, an explicit URL and `TG_MCP_BIND` winning, the default, a bad `PORT`.
7. **§69 step 4**: `docker build -t tg-mcp:nosecret .` and the error naming both ways out.
8. **§71 steps 1–2**, locally, with `TG_MCP_IMAGE=ghcr.io/qelphybox/tg-mcp:<sha>` added to each command (Docker Hub has no release yet): the database survives `down`/`up`; `--profile https` without a domain fails with its message.
9. **The landing pages** (#77, #78, #72 copy, #107):

   ```bash
   node tests/site/landing-check.cjs          # the English page: widths, requests, buttons, demo, fallback, no JavaScript
   node tests/site/landing-check.cjs site /ru/ # the Russian page
   curl -s https://tgmcp.dev/ | grep -ciE 'confirm(ation)? page|confirm each'      # expect 0
   curl -s https://tgmcp.dev/ru/ | grep -ci 'страниц[аеу] подтверждения'           # expect 0
   curl -s https://tgmcp.dev/llms.txt | grep -ci 'confirmed by you'                # expect 0
   ```

   Then open `https://tgmcp.dev` and `https://tgmcp.dev/ru/` on a real phone. Check four things:
   - the EN / RU switch works both ways;
   - "Add to …" opens its steps, and Copy → Copied works;
   - the phone demo plays;
   - the self-host block shows the one-line `docker run` (§72 step 5 checks its wording after the release).

## B. Scripts (2 QR)

10. **§56 step 1 and §57 step 1 together** (their `scope-check.py` commands are the same):

    ```bash
    python3 scripts/scope-check.py - read
    python3 scripts/scope-check.py "read draft" "read draft"
    ```

    Expect every line `[PASS]`, among them "react is not listed" and "mark_read is not listed"; no `send_message` in either printed `tools/list`. **§59 step 3 rides along:** when the first run prints its login link, open it in the Consent browser and check the tab's favicon, the two `<link rel="icon">` tags loaded with 200 and the unchanged CSP header in the developer tools, then go on with the QR as the script expects.

## C. Reading, with the regular `telegram` connector (no QR)

A fresh Claude Code session (it loads the current tool and prompt lists).

11. **§62**: pinned chats beyond `limit`, main list, Archive and `unread_only`. Don't touch the pins meanwhile.
12. **§64**: the three polls and the search hit.
13. **§65**: the comment count, two pages, the discussion group as `chat_id`, comments off, an empty discussion, the refused combinations.
14. **§63, the Claude Code part**: the three misspellings, `foo`, `limit=3`. The claude.ai part goes in block D.
15. **§42 steps 1–6**, each in the fresh session its step asks for. Step 7 (claude.ai) goes in block D.
16. The "nothing logged" lines of §42, §64 and §65 (`logs 60m | grep -iF '<word>'` for each word they name): nothing.

## D. Sending, reacting, and claude.ai (3 QR)

17. **Connect `telegram-send`** as §56's "Needs" paragraph says (A and the forum, no B), and note `R` and `M`.
18. **§56 steps 2–7.** Step 3 adds the claude.ai connector (send, A only). **Keep that claude.ai connector until block G**: §60 counts it as one of the account's connections. In the same claude.ai chat:
    - **§63's claude.ai part:** "list my 3 most recent Telegram chats" and "what did I miss on Telegram?", with no `unknown parameter` error. Record it on #60.
    - **§42 step 7:** how claude.ai shows the prompts, if at all.
19. **§58 step 2** (one more QR, `telegram-send3`, developer tools open): no "without asking me" switch, no `set-cookie` on the consent POST, the new wording. Remove `telegram-send3` and end its session in Devices.
20. **§58 step 3**: Claude Code's own prompt before `send_message`; **No** sends nothing, **Yes** sends.
21. **§58 step 4** only if a send connector made under 0.2.0 is still connected (the v0.2.0 run removed its own in block F). Otherwise skip it, and write "skipped: no 0.2.0 send Grant left" in the results.
22. **Connect `telegram-react`** (A, the forum and G) and set G's reactions to only 👍, as §57's "Needs" paragraph says. Then run **§57 steps 2–9**. Step 1 was done in block B. In step 9, watch "marking General read" (topic 1).
23. **§56 step 8 (the Send cap)**, last in this block: 5 messages to A within a minute, then the 6th is refused.
24. **The server's side for the whole block** (`logs 60m`): §56 step 9, §57 step 10 and §58 step 6, each grep as its section gives it.

## E. The Inspector (1 QR)

25. **§59 step 2**: `serverInfo.icons` (PNG 128, PNG 256, SVG, in that order) and `websiteUrl` in the `initialize` response. End the Inspector's session in Devices afterwards.
26. **§59 step 4**: what claude.ai, Claude Code and ChatGPT show for the connector. Record it on #61; it is information, not a pass/fail.

## F. Clean up blocks C–E, except two connectors

27. Remove the reactions and test messages, as §56 step 10, §57 step 11 and §58 step 7 say. Then:
    - remove `telegram-send` and `telegram-react`, and end their sessions in Telegram → Settings → Devices;
    - **keep** the regular `telegram` connector and the claude.ai connector of step 18;
    - set G's reactions back.

## G. `/forget`, last (3–4 QR)

28. **Connect the second account once**, e.g. `python3 scripts/scope-check.py - read` scanned with its phone, or a connector on its own machine. Then run **§60 step 1**: all three connections answer.
29. **§60 steps 3–5**:
    - step 3: the main account's QR, "N connections" (write N down; it should be 2: `telegram` and claude.ai);
    - step 4: **Cancel** deletes nothing;
    - step 5: the main account's QR again, then **Delete**. Expect 401 for `telegram` and claude.ai, the second account untouched, and "Login expired" on a second press.
30. **§60 step 6** (403 without the cookie, then the per-IP limits), **then wait a minute** before any `/authorize` from this network.
31. **§60 step 7**: the logs, ids and counts only.
32. **§60 step 9**: connect the main account again where you use it, and end leftover "Unofficial MCP Server" sessions in Devices (the second account's too).

## H. An hour and a quarter after `T0`

33. **§66 steps 1–3**: one `stats` line an hour, the counts matching three calls made right after a line, numbers only (no ids, no titles). Step 4 was done in step 0.4.
34. **§66 step 5**: the monitor shows "Up", and the "Down"/"Up" e-mails of `docs/deploy.md` step 6 arrived.
35. **§67 step 3 (maintainer, optional before the tag)**: memory before and after #101. Compare production on `4558b7e` (main just before #101; **no migration since**, so the rollback is safe) with the current sha, using §6's three readings each.
    - `kamal rollback` needs the old container on the server. If Kamal has pruned it, the rollback refuses: record the local reference numbers that §67 gives and say so in the notes.
    - Go back to the current sha afterwards (`kamal rollback <current sha>`, or the next deploy).

# Part 2: the tag

36. Part 1 passed, or each failure has its own ticket and a decision that it doesn't block the release. Step 0.7 is done.
37. **§69 step 1, the release guard**, only if you want to see it fail first. Push the tag before adding the built-in app secrets, and the Release run stops at "Built-in Telegram app secrets are set". Then delete the tag (`git push --delete origin v0.3.0`; nothing was published), add the secrets, and tag again. Otherwise tag once with the secrets in place, and the step shows green.
38. `git tag v0.3.0 && git push origin v0.3.0`. The Release run is green, and its log shows no `qemu` / `binfmt` step.

# Part 3: after the tag

## I. The release images

39. **§70 step 1**: already seen during the sprint, with the Publish runs on `main` skipped before the Docker Hub variable existed. Record a run link from before step 0.7.
40. **§70 steps 2–5**:
    - push any commit to `main`: `<sha>` and `edge` are published for both architectures with one digest;
    - the release's six tag lines share one digest across Docker Hub and GHCR;
    - the labels and descriptions are right;
    - the write token stays in CI.
41. **§67 step 2, the multi-arch part**: `docker buildx imagetools inspect ghcr.io/qelphybox/tg-mcp:latest` lists `linux/amd64` and `linux/arm64`. On an arm64 machine, §67 step 1 passes with that tag.
42. **§69 steps 2, 3, 5 and 6** (2 QR):
    - the release image with no Telegram variables logs in through the built-in app;
    - your own app wins;
    - neither value is in `docker history`, the provenance or `image inspect`;
    - production still says `source=env`.

## J. Self-hosting from the docs (3 QR)

43. **§71 steps 3–6** on the VPS:
    - HTTPS, the issuer and one rate-limit bucket despite forged `X-Forwarded-For`;
    - only Caddy is reachable from outside, with 8080 on loopback;
    - streams aren't buffered;
    - your own app with `source=env`.
44. **§72 steps 1–5**, word for word from the README:
    - the one-line `docker run` on a machine that never ran tg-mcp;
    - localhost failing in claude.ai, as the README says;
    - compose with HTTPS;
    - your own app;
    - the landing pages' self-host block (EN and RU).
45. Clean up: remove the VPS stack if it was only for this, end the built-in app's and your own app's test sessions in Devices, and remove the local containers and volumes (`b69`, `p68`, `s71`, `tg-mcp-data`).

## Results

| Block | Sections and steps | Result | Notes |
|---|---|---|---|
| 0 | §61.1–4, §66.4 | | harden-server run on: |
| A | §58.1, 5; §59.1; §60.2, 8; §68.1–5; §67.1–2 (binary); §69.4; §71.1–2; landing EN/RU | | image size: |
| B | §56.1 + §57.1; §59.3 | | |
| C | §62, §64, §65, §63 (Claude Code), §42.1–6 | | prompt form in Claude Code: |
| D | §56.2–9, §63 (claude.ai), §42.7, §58.2–4, 6, §57.2–10 | | §58.4 run / skipped: |
| E | §59.2, 4 | | what each client shows (copied to #61): |
| G | §60.1, 3–7, 9 | | N connections: |
| H | §66.1–3, 5; §67.3 | | memory before / after: |
| 2 | Release run | | |
| I | §70.1–5, §67.2 (multi-arch), §69.2, 3, 5, 6 | | |
| J | §71.3–6, §72.1–5 | | |

Then close #54, #55, #60, #61, #62, #63, #64, #65, #68, #72, #77, #78, #101, #102, #103, #104, #106 and #107. Each closing comment gives its section's result (and the run link for the ones checked in Part 3). Tick the last lines of #110.

## Known open points to look at during the run

- **The 0.2.0 Grants with confirmation on** (§58 step 4). They send at once since #113 was deployed. If none is left to test with, only the fake-backed tests (`cargo test --test send_message`, `cargo test --lib allow_list::`) cover it.
- **Whether claude.ai shows `serverInfo.icons` for a custom connector** (§59 step 4). No client documents it; this is input for the connectors directory (#57).
- **Marking General read** (§57 step 9) and **forum Drafts** were first written for v0.2.0 against TDLib's conventions. If the v0.2.0 run didn't record them on real Telegram, record them here.
- **Docker Hub's anonymous pull limit** (10 an hour per IP) may slow block J on a shared network: log in to Docker Hub there if pulls get refused.
- **No rollback past 0.2.0**: migrations 0003–0005 still stop a 0.1.x image from starting. 0.3.0 adds no migration, so rolling back to any 0.2.x or 0.3.0 sha is safe.
