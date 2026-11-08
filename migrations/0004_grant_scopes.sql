-- Scopes on Grants (ADR-0005, #41). A Grant's Scopes are chosen on the Consent
-- screen and never change: other Scopes mean a new Grant. Plaintext: they reveal
-- nothing about the Account's chats. Space-separated, canonical order, `read`
-- always first (`read`, `read draft`, ...).
--
-- Every Grant from before Scopes could only read, so it becomes `read` and keeps
-- working as it did.

ALTER TABLE grants ADD COLUMN scopes TEXT NOT NULL DEFAULT 'read';
