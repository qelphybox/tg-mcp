# Scheduled messages

tg-mcp does not read the Account's scheduled messages (messages queued to be sent later).

## Why this is out of scope

Reading them only matters together with writing them: "what did I schedule?" is a question about the Account's own outgoing queue, not about its chats. In v0.1 the server is read-only, and the write tools (ADR-0005, ADR-0008) send now, within limits the server checks at that moment and in the tool call the client approved; scheduling would add a second, delayed path for a message to leave the Account later, outside both.

Revisit only if a write tool for delayed sending is ever accepted; reading the queue would then come with it.

## Prior requests

- Decided while grilling the gaps after #26 (2026-11-08), prompted by prior art that exposes `get-scheduled`.
