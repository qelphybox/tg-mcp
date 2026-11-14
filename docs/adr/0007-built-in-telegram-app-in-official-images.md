# Built-in Telegram app credentials in official images

Every Login flow goes through a Telegram app: an api_id/api_hash pair registered at my.telegram.org. Until now a self-hoster had to register their own, a form that is confusing and often fails. Official images now carry a Built-in app: its api_id and api_hash are compiled into the binary from CI secrets at build time and never appear in the source tree. A self-hoster's `TG_MCP_API_ID` and `TG_MCP_API_HASH` still win when both are set.

The values are not secret in practice. Anyone can pull the image and read them out of the binary, as with every distributed Telegram client (Telegram's own apps included). Hiding them is therefore not what protects anything. What protects production is that the Built-in app is a **separate Telegram app, on a separate phone number**, from production's (Telegram allows one api_id per number). If someone abuses it through a self-hosted instance, and Telegram answers with `API_ID_PUBLISHED_FLOOD` or bans it, only the Built-in app suffers; production keeps logging in through its own app, which it keeps passing explicitly (`TG_MCP_API_ID` / `TG_MCP_API_HASH` in `config/deploy.yml`).

## Decision

- Precedence: env (`TG_MCP_API_ID` and `TG_MCP_API_HASH`, both non-empty) → Built-in app → fake Telegram (`TG_MCP_FAKE_TELEGRAM=1`) → a startup error that names both ways out: set your own app, or run the official image.
- The Built-in app is read at compile time from the optional build variables `TG_MCP_BUILTIN_API_ID` and `TG_MCP_BUILTIN_API_HASH`. `cargo build`, fork CI and `main` CI build without them and behave as before.
- In the Dockerfile they arrive only as BuildKit secret mounts (`tg_builtin_api_id`, `tg_builtin_api_hash`), never as build arguments, which show in `docker history` and in provenance attestations.
- Release builds pass `TG_MCP_BUILTIN_APP=required`. That fails the build when the secrets are missing, and it changes the cache key, so a layer cached from a build without them can't end up in a release. The release workflow also checks for the secrets first and fails with a clear message.
- BuildKit's cache key ignores secrets, so the release rebuilds its build stage without the cache (`no-cache-filters: build`): rotated secrets always reach the release. The build stage's layer, binary included, does go into the GitHub Actions cache, which is consistent with the values not being secret.
- A Built-in api_id that isn't a number fails the build (a `const` parse), not the server's start.
- The startup log names the source (`env` or `builtin`) and the api_id, never the hash.

## Considered Options

- **Keep requiring every self-hoster to register an app.** Rejected: it is the step most likely to stop someone from trying tg-mcp.
- **Ship production's own app in the image.** Rejected: one abusive self-hosted instance could get the app that every hosted user logs in through blocked.
- **Obfuscate the values in the binary.** Rejected: it hides nothing from anyone who wants them, and suggests a secrecy that doesn't exist.

## Consequences

- Telegram shows the Built-in app's name and version on every Account's Active sessions screen for logins through a self-hosted official image, so the app's title keeps "Unofficial".
- Built-in-app abuse is handled by rotating it: register a new app on another number, update the secrets, cut a release. Older images keep the old values until their owners upgrade.
- An image built without the secrets (a fork, a local `docker build`) needs `TG_MCP_API_ID` / `TG_MCP_API_HASH` as before.
