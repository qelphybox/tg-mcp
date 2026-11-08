-- Send allow-list (ADR-0005, #44). A Grant with the `send` Scope may write only to
-- the Chats chosen on the Consent screen, and send there without the user
-- confirming each message only if Consent said so. Both live in one blob sealed
-- with the Grant's Session key, like its Telegram session (ADR-0003):
--   nonce || XChaCha20-Poly1305(Session key, plaintext,
--                               AAD = "tg-mcp/send-allow-list/v1:" || grant id)
-- The plaintext (format version, the auto-send flag, the Chat ids, zeros up to a
-- multiple of 512 bytes) never touches the database, and the flag is inside so a
-- database writer can't turn confirmation off. The list never changes: other
-- Chats mean a new Grant.
--
-- NULL for a Grant without `send`, which is every Grant from before.

ALTER TABLE grants ADD COLUMN send_allow_list BLOB;
