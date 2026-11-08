# Stories

tg-mcp does not read Telegram stories (the Account's own or anyone else's).

## Why this is out of scope

Stories are ephemeral by design: their authors post them to disappear within a day, often to a hand-picked audience. Feeding them into an AI client turns a deliberately short-lived, semi-private surface into something that can be summarised, quoted and kept. They are also a separate part of the API (peer stories, story ids, view lists) that would grow the tool surface for little value in the server's job, which is reading the Account's chats.

## Prior requests

- Decided while grilling the gaps after #26 (2026-11-08), prompted by prior art that exposes story tools.
