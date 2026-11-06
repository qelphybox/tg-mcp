# SQLite for MVP storage

The MVP stores Grants, Tokens (verifiers + Wrapped keys) and encrypted Telegram sessions in SQLite via sqlx, behind a `Store` trait. The spec allowed "Postgres or SQLite"; supporting both from day one doubles migrations and tests before the first `list_chats` works. SQLite keeps the one-binary deploy with zero infrastructure and is enough for a single-node public instance. Postgres is a later, separate change behind the same trait, needed only for horizontal scaling.
