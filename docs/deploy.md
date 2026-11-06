# Deploy

Production runs at `https://tgmcp.qelphybox.space` on one VPS, deployed with [Kamal 2](https://kamal-deploy.org): `config/deploy.yml`, `.kamal/secrets`, `.github/workflows/deploy.yml`.

## How a rollout works

1. GitHub Actions builds the image from `Dockerfile` and pushes it to `ghcr.io/qelphybox/tg-mcp:<git sha>`.
2. Kamal SSHes into the server, pulls the image and starts the new container next to the old one.
3. kamal-proxy polls `/healthz` on the new container; once it answers, traffic switches over. The old container drains for up to `drain_timeout` (30 s), then gets SIGTERM.

Triggers: every push to `main`, or **Actions → Deploy → Run workflow** (pick `setup` for a fresh server, `deploy` otherwise). Manual runs only appear once the workflow is on `main`.

kamal-proxy terminates TLS with a Let's Encrypt certificate for the host. `text/event-stream` responses (MCP streams) pass through unbuffered; `response_timeout` only bounds the wait for response headers.

## What survives a deploy

- **Survives:** everything in SQLite (Grants, Tokens, encrypted Telegram sessions) on the `tg-mcp-data` volume, mounted at `/data`.
- **Lost:** in-memory state: open MCP sessions (clients re-initialize on their own), Live clients (reconnect lazily on the next call), authorization codes and logins in progress (the user starts the browser flow again).

## First deploy

Run the wizard from a local clone (needs `gh` logged in) and follow it:

```sh
scripts/setup-deploy.sh
```

It collects the Telegram api_id/api_hash, makes sure your personal key opens `root@<server>`, waits for the DNS A record, generates a dedicated deploy key, hardens the server with `scripts/harden-server.sh` (see below), pins the server's SSH host key and triggers the first deploy. It writes these to GitHub (and the same values to your local, git-ignored `.env`):

| Name | Kind | What |
| --- | --- | --- |
| `TG_MCP_API_ID`, `TG_MCP_API_HASH` | secret | Telegram app credentials (my.telegram.org) |
| `SSH_PRIVATE_KEY` | secret | Deploy key, `~/.ssh/tg-mcp-deploy` |
| `KAMAL_HOST`, `KAMAL_SSH_USER` | variable | Server IP and `deploy` |
| `SSH_KNOWN_HOSTS` | variable | Pinned host key; CI refuses any other |

The registry password is the workflow's own `GITHUB_TOKEN`; nothing to set. Re-running the wizard is safe: it keeps saved values and the hardening script is idempotent.

## Server baseline

`scripts/harden-server.sh` (Debian/Ubuntu, run as root, idempotent) leaves the server like this:

- **SSH:** public keys only (passwords and keyboard-interactive off), root allowed with a key only, `AllowUsers root deploy`, 3 attempts per connection. The config is a drop-in named `00-tg-mcp.conf` so it beats cloud-init's `50-cloud-init.conf` (sshd keeps the first value it reads), and it is checked with `sshd -t` before the reload. The script refuses to run if root has no key, so it can't lock you out.
- **`deploy` user:** what Kamal and CI log in as. Member of `docker`, password locked, only the CI deploy key. Note that `docker` group membership is root-equivalent: the point is a separate, revocable key, not a sandbox. You keep administering as root with your personal key.
- **Firewall (ufw):** incoming denied except 22 (rate-limited), 80 and 443.
- **fail2ban:** sshd jail, 5 failures in 10 min → 1 h ban.
- **Updates:** unattended-upgrades for the distro security pocket and Docker CE; reboots at 04:30 UTC when an update needs it. Containers come back on their own (`--restart unless-stopped`); in-memory state is lost as on a deploy.
- **Docker CE** from download.docker.com, daemon with `live-restore` (containers survive a dockerd upgrade), `no-new-privileges`, and log rotation.

Caveat: Docker writes its own iptables rules, so a port published with `-p` bypasses ufw. Kamal publishes only kamal-proxy's 80/443 and keeps the app on the internal Docker network, so nothing else is exposed. Don't add `publish`/`-p` options without a firewall rule in mind.

## Day-to-day

From a local clone with the variables above exported (`set -a; . ./.env; set +a`) and `KAMAL_REGISTRY_PASSWORD` set to a GitHub token with `read:packages`:

```sh
kamal app logs -f        # follow logs
kamal rollback <sha>     # switch back to a previous image (kamal app containers lists them)
```

The image is distroless (no shell), so `kamal app exec` with a shell does not work, by design.
