-- OAuth 2.1 dynamic client registration (RFC 7591). Public clients only.
CREATE TABLE oauth_clients (
    id             TEXT    PRIMARY KEY,       -- client_id, random
    name           TEXT    NOT NULL,          -- client_name as registered, shown on the Consent screen
    redirect_uris  TEXT    NOT NULL,          -- JSON array
    created_at     INTEGER NOT NULL
) STRICT;

-- A Grant belongs to the client it was issued to: refresh Tokens only work for that client.
ALTER TABLE grants ADD COLUMN client_id TEXT NOT NULL DEFAULT '';
