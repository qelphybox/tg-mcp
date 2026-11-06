-- Session vault (ADR-0003). Nothing here is usable without a Token:
-- Telegram sessions are sealed with a per-Grant Session key that is only
-- stored wrapped under keys derived from Token secrets.

CREATE TABLE grants (
    id                 TEXT    PRIMARY KEY,           -- uuid v4
    account_id         INTEGER NOT NULL,              -- Telegram user id
    telegram_session   BLOB    NOT NULL,              -- nonce || XChaCha20-Poly1305(Session key)
    created_at         INTEGER NOT NULL,              -- unix seconds
    updated_at         INTEGER NOT NULL
) STRICT;

CREATE TABLE tokens (
    id           TEXT    PRIMARY KEY,                 -- base64url, public half of the Token
    grant_id     TEXT    NOT NULL REFERENCES grants(id) ON DELETE CASCADE,
    kind         TEXT    NOT NULL CHECK (kind IN ('access', 'refresh')),
    verifier     BLOB    NOT NULL,                    -- HKDF(secret, "verifier")
    wrapped_key  BLOB    NOT NULL,                    -- nonce || XChaCha20-Poly1305(HKDF(secret, "wrap")), Session key
    expires_at   INTEGER NOT NULL,
    created_at   INTEGER NOT NULL
) STRICT;

CREATE INDEX tokens_grant_id ON tokens(grant_id);
CREATE INDEX tokens_expires_at ON tokens(expires_at);
