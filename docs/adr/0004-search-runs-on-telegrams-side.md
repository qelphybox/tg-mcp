# Search runs on Telegram's side

Search tools call Telegram's own search (`messages.searchGlobal`, `messages.search`, `contacts.search`) and keep no index of their own: no full-text index, no embeddings, no in-memory message cache. A local index would mean downloading whole histories (a guaranteed `FLOOD_WAIT`) and holding message content at rest or in memory, which breaks the spec's "never cached to disk" and "idle Account ≈ 0 memory" promises and the point of ADR-0003: a database dump must hold nothing worth stealing. Telegram's search is lexical (no semantics, weak morphology); the model makes up for it by running several short queries (word forms, synonyms, transliteration) and narrowing by date and sender, which the tool descriptions teach it.

## Considered Options

- Local archive with full-text and vector search (as in groupultra/telegram-search): better recall, semantic matches, works for scripts without word boundaries. Rejected: plaintext history at rest, message content sent to an embedding provider, and bulk sync.
- In-memory index per Live client: no data at rest, but needs full history downloads on every cold start and costs memory per active Account.

## Consequences

- Results come in Telegram's order (newest first), not by relevance, and the server returns no hit count, since Telegram's total is not reliable under sender/date filters.
- Search quality for a language is whatever Telegram's search gives; languages without word boundaries (e.g. Chinese) are not a target.
- Revisiting this means a new ADR that also revisits ADR-0003's threat model.
