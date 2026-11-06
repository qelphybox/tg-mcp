# SQLite for MVP storage

The MVP stores Grants, Tokens (verifiers + Wrapped keys) and encrypted Telegram sessions in SQLite via sqlx. The spec allowed "Postgres or SQLite"; supporting both from day one doubles migrations and tests before the first `list_chats` works. SQLite keeps the one-binary deploy with zero infrastructure and is enough for a single-node public instance. Postgres is a later, separate change, needed only for horizontal scaling.

## Consequences

- All SQL sits in the `sessions` module (plus OAuth client storage next to it). We deliberately did not add a `Store` trait up front: with a single backend it would be a seam with nothing on the other side. It gets introduced together with the Postgres implementation, and the module boundary keeps that change local.
