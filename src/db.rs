//! The SQLite database: one pool, migrations embedded in the binary (ADR-0001).

use std::{path::Path, str::FromStr, time::Duration};

use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};

/// Open (creating if missing) the database at `path` and bring its schema up to date.
pub async fn open(path: &Path) -> Result<SqlitePool, sqlx::Error> {
    let options = SqliteConnectOptions::from_str("sqlite:")?
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        // Deleted content is overwritten with zeros, not left in free space: a
        // swept Wrapped key (ADR-0003) or a deleted Grant leaves the file for real.
        .pragma("secure_delete", "ON")
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}
