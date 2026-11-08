-- Refresh Token reuse detection (ADR-0003, #16). A refresh no longer deletes the
-- refresh Token it consumes: the row stays, marked with `consumed_at`, so a later
-- reuse is recognised (by its verifier) and revokes the Grant. For the grace
-- window the row is still fully usable; after it the Wrapped key is set to NULL,
-- leaving only id, Grant, verifier and the original expiry.
--
-- SQLite can't drop NOT NULL from a column, so `tokens` is rebuilt. Nothing
-- references `tokens`, so dropping and renaming it keeps every foreign key intact.

CREATE TABLE tokens_new (
    id           TEXT    PRIMARY KEY,                 -- base64url, public half of the Token
    grant_id     TEXT    NOT NULL REFERENCES grants(id) ON DELETE CASCADE,
    kind         TEXT    NOT NULL CHECK (kind IN ('access', 'refresh')),
    verifier     BLOB    NOT NULL,                    -- HKDF(secret, "verifier")
    wrapped_key  BLOB,                                -- nonce || XChaCha20-Poly1305(HKDF(secret, "wrap")), Session key; NULL once consumed and past the grace window
    expires_at   INTEGER NOT NULL,
    created_at   INTEGER NOT NULL,
    consumed_at  INTEGER,                             -- unix seconds of the first refresh with it; refresh Tokens only
    CHECK (consumed_at IS NULL OR kind = 'refresh'),
    CHECK (wrapped_key IS NOT NULL OR consumed_at IS NOT NULL)
) STRICT;

INSERT INTO tokens_new (id, grant_id, kind, verifier, wrapped_key, expires_at, created_at)
SELECT id, grant_id, kind, verifier, wrapped_key, expires_at, created_at FROM tokens;

DROP TABLE tokens;
ALTER TABLE tokens_new RENAME TO tokens;

CREATE INDEX tokens_grant_id ON tokens(grant_id);
CREATE INDEX tokens_expires_at ON tokens(expires_at);
CREATE INDEX tokens_consumed_at ON tokens(consumed_at) WHERE consumed_at IS NOT NULL;
