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

Run the wizard from a local clone and follow it:

```sh
scripts/setup-deploy.sh
```

It collects the Telegram api_id/api_hash, server IP and SSH user, waits for the DNS A record, installs a dedicated deploy key, pins the server's SSH host key and triggers `kamal setup`. It writes these to GitHub (and the same values to your local, git-ignored `.env`):

| Name | Kind | What |
| --- | --- | --- |
| `TG_MCP_API_ID`, `TG_MCP_API_HASH` | secret | Telegram app credentials (my.telegram.org) |
| `SSH_PRIVATE_KEY` | secret | Deploy key, `~/.ssh/tg-mcp-deploy` |
| `KAMAL_HOST`, `KAMAL_SSH_USER` | variable | Server IP and SSH user |
| `SSH_KNOWN_HOSTS` | variable | Pinned host key; CI refuses any other |

The registry password is the workflow's own `GITHUB_TOKEN`; nothing to set.

## Day-to-day

From a local clone with the variables above exported (`set -a; . ./.env; set +a`) and `KAMAL_REGISTRY_PASSWORD` set to a GitHub token with `read:packages`:

```sh
kamal app logs -f        # follow logs
kamal rollback <sha>     # switch back to a previous image (kamal app containers lists them)
```

The image is distroless (no shell), so `kamal app exec` with a shell does not work, by design.
