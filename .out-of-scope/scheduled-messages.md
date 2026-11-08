# Scheduled messages

tg-mcp does not read the Account's scheduled messages (messages queued to be sent later).

## Why this is out of scope

Reading them only matters together with writing them: "what did I schedule?" is a question about the Account's own outgoing queue, not about its chats. In v0.1 the server is read-only, and the v0.2.0 write tools (ADR-0005) send now, with a human confirming each send; scheduling would add a second, delayed path for a message to leave the Account after the confirmation, which the confirmation model doesn't cover.

Revisit only if a write tool for delayed sending is ever accepted; reading the queue would then come with it.

## Prior requests

- Decided while grilling the gaps after #26 (2026-11-08), prompted by prior art that exposes `get-scheduled`.
