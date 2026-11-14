# Deploy

Production runs at `https://connector.tgmcp.dev` on one VPS, deployed with [Kamal 2](https://kamal-deploy.org): `config/deploy.yml`, `.kamal/secrets`, `.github/workflows/deploy.yml`.

## How a rollout works

1. GitHub Actions builds the image from `Dockerfile` and pushes it to `ghcr.io/qelphybox/tg-mcp:<git sha>`.
2. Kamal SSHes into the server, pulls the image and starts the new container next to the old one.
3. kamal-proxy polls `/healthz` on the new container; once it answers, traffic switches over. The old container drains for up to `drain_timeout` (30 s), then gets SIGTERM and has `stop_timeout` (30 s) to exit before SIGKILL. On SIGTERM it closes MCP streams, waits for in-flight requests, then closes every Live client and re-seals each Telegram session that changed while live (bounded to 10 s; logged as `client pool flushed` with counts).

Triggers: every push to `main`, or **Actions → Deploy → Run workflow** (pick `setup` for a fresh server, `deploy` otherwise). Manual runs only appear once the workflow is on `main`.

kamal-proxy terminates TLS with a Let's Encrypt certificate for the host. `text/event-stream` responses (MCP streams) pass through unbuffered; `response_timeout` only bounds the wait for response headers.

Client IPs: the app only sees kamal-proxy as its TCP peer. kamal-proxy (with `ssl: true` and `forward_headers` unset, so false) drops any `X-Forwarded-For` the client sent and sets it to the address it accepted the connection from; with `forward_headers: true` it would append that address instead. Either way the right-most entry is the real client, and `TG_MCP_TRUST_FORWARDED_FOR=1` makes the app read it for the per-IP limits on `/register`, `/authorize`, `/token`, `/revoke`, `/media/{ticket}` and on pending logins. Don't set kamal-proxy's `client_ip_header`: it would copy a client-controlled header into `X-Forwarded-For`.

Media links (`get_media`, #36): `GET /media/{ticket}` carries its credential in the path, because the agent fetching it with curl has no token. The app never logs tickets, URLs or file bytes, but kamal-proxy's access log (`kamal proxy logs`) records request paths, so a ticket can end up there. This was accepted: a ticket works for 5 minutes and 3 downloads at most, for one file, and the proxy log stays on the server for 14 days at most (see Logs below), where whoever can read it can read the app's memory too. Response buffering is off in `config/deploy.yml` (`proxy.buffering.responses: false`): by default kamal-proxy holds a response whole and writes anything over 1 MB to disk, which would put downloaded files on the server's disk. Downloads stream through it with their `Content-Length`.

## What survives a deploy

- **Survives:** everything in SQLite (Grants, Tokens, encrypted Telegram sessions) on the `tg-mcp-data` volume, mounted at `/data`.
- **Lost:** in-memory state: open MCP sessions (clients re-initialize on their own), Live clients (reconnect lazily on the next call; Telegram session changes they held, such as a DC migration, are re-sealed on a graceful shutdown, but lost on SIGKILL or a crash), authorization codes and logins in progress (the user starts the browser flow again), Media links (their downloads 404; the agent calls `get_media` again), the duplicate guard of `send_message` (a text sent in the minute before the deploy can be sent again right after it), rate-limit counters (they start full again).

## Logs: 14 days

The app and kamal-proxy log to journald on the server (`logging` and `proxy.run.options` in `config/deploy.yml`, and Docker's default in `daemon.json`). `scripts/harden-server.sh` configures journald to delete entries older than 14 days (`MaxRetentionSec=14day`, one journal file per day so the cut is within a day of 14) and to use at most 1 GB. Neither ships logs anywhere else.

What the app logs carry: ids, counts and sizes (Grant, Chat and message ids, rate-limit and stats counters), never message content, Tokens, Session keys, Telegram sessions or chat titles. kamal-proxy's access log adds request paths, which can include a Media link ticket (above).

```sh
kamal app logs -f                                   # from a clone, as before (docker logs reads journald)
ssh deploy@$KAMAL_HOST journalctl -t tg-mcp -f      # on the server: the app, across deploys
ssh deploy@$KAMAL_HOST journalctl -t kamal-proxy --since -1h
```

(`scripts/harden-server.sh` puts `deploy` in `systemd-journal`, so it can read these; it is in `docker` already, which is root-equivalent, so that grants nothing new.)

## Backups: none, on purpose

The `tg-mcp-data` volume on the server is the database's only copy, and nothing backs it up. Losing it means every user connects again (QR login and Consent); nothing else is lost, since the server stores no messages.

A backup wouldn't spare them that. Refresh Tokens rotate on every refresh, and the database keeps only verifiers and Wrapped keys, never Tokens. Yesterday's copy has none of the Wrapped keys for today's Tokens, so restoring it still makes every client log in again. A copy is harmless (without Tokens it opens no Telegram session, ADR-0003) but useless, so there is none to keep safe.

## First deploy

Run the wizard from a local clone (needs `gh` logged in) and follow it:

```sh
scripts/setup-deploy.sh
```

It collects the Telegram api_id/api_hash, makes sure your personal key opens `root@<server>`, waits for the DNS A record, generates a dedicated deploy key, hardens the server with `scripts/harden-server.sh` (see below), pins the server's SSH host key and triggers the first deploy. It writes these to GitHub (and the same values to your local, git-ignored `.env`):

| Name | Kind | What |
| --- | --- | --- |
| `TG_MCP_API_ID`, `TG_MCP_API_HASH` | secret | Production's own Telegram app (my.telegram.org); never the Built-in one |
| `TG_MCP_BUILTIN_API_ID`, `TG_MCP_BUILTIN_API_HASH` | secret | The Built-in app of release images ([ADR-0007](adr/0007-built-in-telegram-app-in-official-images.md)). Set by hand, not by the wizard: a **second** Telegram app, registered at my.telegram.org from a **second** phone number, never production's. The release workflow fails without them. |
| `SSH_PRIVATE_KEY` | secret | Deploy key, `~/.ssh/tg-mcp-deploy` |
| `KAMAL_HOST`, `KAMAL_SSH_USER` | variable | Server IP and `deploy` |
| `SSH_KNOWN_HOSTS` | variable | Pinned host key; CI refuses any other |

The registry password is the workflow's own `GITHUB_TOKEN`; nothing to set. Re-running the wizard is safe: it keeps saved values and the hardening script is idempotent.

## Server baseline

`scripts/harden-server.sh` (Debian/Ubuntu, run as root, idempotent) leaves the server like this:

- **SSH:** public keys only (passwords and keyboard-interactive off), root allowed with a key only, `AllowUsers root deploy`, 3 attempts per connection. The config is a drop-in named `00-tg-mcp.conf` so it beats cloud-init's `50-cloud-init.conf` (sshd keeps the first value it reads), and it is checked with `sshd -t` before the reload. The script refuses to run if root has no key, so it can't lock you out.
- **`deploy` user:** what Kamal and CI log in as. Member of `docker` and `systemd-journal` (reads the logs), password locked, only the CI deploy key. Note that `docker` group membership is root-equivalent: the point is a separate, revocable key, not a sandbox. You keep administering as root with your personal key.
- **Firewall (ufw):** incoming denied except 22 (rate-limited), 80 and 443.
- **fail2ban:** sshd jail, 5 failures in 10 min → 1 h ban.
- **Updates:** unattended-upgrades for the distro security pocket and Docker CE; reboots at 04:30 UTC when an update needs it. Containers come back on their own (`--restart unless-stopped`); in-memory state is lost as on a deploy.
- **Docker CE** from download.docker.com, daemon with `live-restore` (containers survive a dockerd upgrade), `no-new-privileges`, and journald as the log driver.
- **journald:** entries older than 14 days are deleted, at most 1 GB (see Logs above).

Caveat: Docker writes its own iptables rules, so a port published with `-p` bypasses ufw. Kamal publishes only kamal-proxy's 80/443 and keeps the app on the internal Docker network, so nothing else is exposed. Don't add `publish`/`-p` options without a firewall rule in mind.

## Day-to-day

From a local clone with the variables above exported (`set -a; . ./.env; set +a`) and `KAMAL_REGISTRY_PASSWORD` set to a GitHub token with `read:packages`:

```sh
kamal app logs -f        # follow logs
kamal rollback <sha>     # switch back to a previous image (kamal app containers lists them)
```

A rollback can't cross a database migration: an image older than a migration the database already ran refuses to start (sqlx: "migration N was previously applied but is missing"). Check `migrations/` between the two commits; if one was added, fix forward instead.

The image is a static musl binary on `distroless/static` (no shell, no libc: MTProto connects to IP addresses, so nothing needs a resolver or a TLS library), so `kamal app exec` with a shell does not work, by design. It sets `TG_MCP_DB_PATH=/data/tg-mcp.db` and `MIMALLOC_ARENA_EAGER_COMMIT=0` (mimalloc's eager arena commit would hold about 9 MB more at idle). `scripts/container-smoke-test.sh <image>` runs any build the way a self-hoster would; CI runs it on every push.

## Is it up? The `stats` line and the `/healthz` monitor

There is no metrics stack. Once an hour, and once on a graceful shutdown, the server logs one `info` line `stats` (target `tg_mcp::stats`) with numbers only:

```text
INFO tg_mcp::stats: stats reason="hourly" grants=12 live_clients=3 login_flows=0 media_links=1 tool_calls=get_messages:41 list_chats:9 tool_errors=invalid_params:2 flood_waits=search:1/12s refused=oauth:3 logins=1 revocations=0
```

| Field | What it counts |
| --- | --- |
| `reason` | `hourly`, or `shutdown` for the line logged as the process stops |
| `grants` | Grants in the database now (left out if the database couldn't be read) |
| `live_clients` | Live clients in the Client pool now |
| `login_flows` | pending Login flows now |
| `media_links` | live Media links now |
| `tool_calls` | over the hour: tool calls by tool name |
| `tool_errors` | over the hour: failed tool calls by kind: `scope` (outside the Grant's Scopes), `invalid_params`, `internal` (Telegram, our limits or the server), `unknown_tool`, `other` |
| `flood_waits` | over the hour: Telegram's `FLOOD_WAIT`s to Live clients' requests per method class (`default`, `search`, `files`, `transcribe`, `writes`) as `count/longest wait`; the Login flow's aren't counted |
| `refused` | over the hour: requests our own limits refused: `request_budget` (the Account's request budget), `writes_budget` (its Writes budget), `send_cap` (a Chat's Send cap), `oauth` (`429` from an OAuth endpoint) |
| `logins` | over the hour: Login flows that ended in a new Grant |
| `revocations` | over the hour: Grants revoked, by their client (`/revoke`) or for a reused refresh Token; a Grant deleted because its Telegram session was ended elsewhere doesn't count |

`-` means none. The counters live in memory: they reset with each line, and a restart (every deploy) loses them, so the first line after a deploy covers less than an hour. The line names no user, Account, Grant, chat or message and carries no titles: only our tool names, error kinds, method classes and limits.

The logs are coloured (ANSI escapes around field names), so strip them before matching a field:

```sh
nocolor() { sed 's/\x1b\[[0-9;]*m//g'; }
kamal app logs --since 24h | nocolor | grep 'tg_mcp::stats'          # the last day, one line an hour
kamal app logs -f | nocolor | grep 'tg_mcp::stats'                   # wait for the next one
kamal app logs --since 24h | nocolor | grep 'reason="shutdown"'      # the last line of each stopped container
```

### The external monitor (maintainer, once)

`GET https://connector.tgmcp.dev/healthz` answers `200 ok` while the process serves HTTP. It doesn't ask Telegram anything, so it says the server is up, not that every Account can reach Telegram (the `stats` line's `tool_errors` and `flood_waits` say that). A free [UptimeRobot](https://uptimerobot.com) monitor e-mails when it stops answering:

1. Sign up at uptimerobot.com (free plan) with the e-mail address that should get the alerts, and confirm it.
2. **New monitor** → type **HTTP(s)**.
3. **URL**: `https://connector.tgmcp.dev/healthz`. **Friendly name**: `tg-mcp connector`. **Interval**: 5 minutes (the free plan's shortest).
4. **Advanced**: keyword monitoring is not needed; leave the method `GET` and the timeout at its default. Turn on SSL certificate expiry alerts if the plan offers them (kamal-proxy renews the certificate; an expiring one means renewal broke).
5. **Alert contacts**: tick the e-mail contact from step 1. **Create monitor**.
6. Check it fires: stop the app for a little over one interval (`kamal app stop`, wait about 6 minutes, `kamal app boot`), expect a "Down" e-mail and then an "Up" one. Or, without downtime, pause and resume the monitor and check that it shows "Up" with a response time.
7. Write on #64 which service and which address get the alerts.

A deploy restarts the container in seconds behind kamal-proxy and stays under one interval, so it doesn't page.

## Landing page

`https://tgmcp.dev` is GitHub Pages, published from `site/` by `.github/workflows/pages.yml` on every push to `main` that touches it. There is no build step: what is in `site/` is what is served. The custom domain lives in **Settings → Pages**, not in a `CNAME` file (Pages ignores it when deploying from Actions).

DNS for `tgmcp.dev`, at its registrar:

| Name | Type | Value |
| --- | --- | --- |
| `@` | A | `185.199.108.153`, `185.199.109.153`, `185.199.110.153`, `185.199.111.153` |
| `@` | AAAA | `2606:50c0:8000::153`, `2606:50c0:8001::153`, `2606:50c0:8002::153`, `2606:50c0:8003::153` |
| `www` | CNAME | `qelphybox.github.io` |
| `connector` | A | the server's IPv4 (`KAMAL_HOST`), no CDN proxying |

`.dev` is on the HSTS preload list, so browsers only ever use HTTPS: the site and the connector open once their certificates are issued (Pages: after the domain check in Settings → Pages; kamal-proxy: on the first HTTPS request).

## Releases

Production follows `main`; releases are for self-hosters. Bump `version` in `Cargo.toml`, merge, then tag:

```sh
git tag v0.1.0 && git push origin v0.1.0
```

`.github/workflows/release.yml` checks the tag matches `Cargo.toml` and that the Built-in app secrets (`TG_MCP_BUILTIN_API_ID`, `TG_MCP_BUILTIN_API_HASH`, see the table above) are set, compiles them in through BuildKit secrets, builds `linux/amd64` and `linux/arm64` (the Dockerfile cross-compiles, no emulation) once, and pushes the result with the tags `0.1.0`, `0.1` and `latest` to both registries, so both carry the same digest:

- **Docker Hub, `qelphybox/tg-mcp`**: the image's home. The release also syncs the repository's full description from `README.md` (at most 25,000 bytes, CI checks; relative links become absolute). The short description is set by hand on Docker Hub: "Unofficial Telegram MCP server: read, search, sum up chats; draft and send where you allow."
- **GHCR, `ghcr.io/qelphybox/tg-mcp`**: a mirror of release tags for installs that already pull from it, until the 0.3.x releases end (announced in the CHANGELOG).

It then creates a GitHub Release naming both images. Run [the acceptance checks](acceptance.md) before tagging and put the memory numbers in the release notes.

Every push to `main` also builds both architectures and pushes `qelphybox/tg-mcp:<git sha>` and `:edge` to Docker Hub (`.github/workflows/publish.yml`), with the Built-in app when its secrets are set. `latest` and the version tags move only on releases. Production doesn't deploy these yet: Kamal still builds its own image (above).

Docker Hub setup, once, by the maintainer:

| Name | Kind | What |
| --- | --- | --- |
| `DOCKERHUB_USERNAME` | variable | The Docker Hub account that owns `qelphybox/tg-mcp`. While it or `DOCKERHUB_TOKEN` is unset (forks), every Docker Hub step is skipped and releases go to GHCR only |
| `DOCKERHUB_TOKEN` | secret | A Docker Hub personal access token with **Read & Write** access: pushes `<sha>`, `edge` and release tags. It exists only in GitHub Secrets and is used only by `publish.yml` and `release.yml`. Add it before (or together with) the variable |
| `DOCKERHUB_DESCRIPTION_TOKEN` | secret, optional | A second token with **Read, Write, Delete** access, which Docker Hub's API requires to change a repository's description. Used only by the release's description step; without it the README isn't synced |

Create the repository `qelphybox/tg-mcp` on Docker Hub (public) before the first push.

The GHCR package is private until its visibility is set to public once (GitHub → Packages → tg-mcp → Package settings).
