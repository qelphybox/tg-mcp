# The Session key lives only in Tokens

A database dump must not give access to any Telegram session, yet a server restart must not force re-login. So each Grant has a random Session key that encrypts its Telegram session (XChaCha20-Poly1305), and the key itself is never stored: every Token we issue is `<token id>.<256-bit secret>`, and per Token we store only a verifier and a Wrapped key, both derived from the secret with HKDF-SHA256. A request (or a refresh) presents a Token, we verify it, unwrap the Session key and decrypt the Telegram session in memory. The server is its own OAuth 2.1 authorization server because no external IdP could carry this key material.

## Consequences

- Refresh rotates Tokens; each new Token gets its own Wrapped key, the old one is deleted with its Token.
- The authorization code is short-lived and held in memory only (with the Session key); a restart mid-login means logging in again.
- Revoking a Grant (and calling `auth.logOut`) needs a live Token, since only a Token can decrypt the Telegram session.
- Losing every Token of a Grant makes its Telegram session unrecoverable by design; the user logs in again.
- A Media link is downloaded without a Token (an agent's shell has none). Like the authorization code, it is issued from a Token-authenticated call and keeps what the download needs (a reference to the Grant's Live client) in memory only, for a few minutes and a few downloads; a restart kills it. Nothing about it is persisted.
