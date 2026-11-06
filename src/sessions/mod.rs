//! The `sessions` module: the Session vault from ADR-0003.
//!
//! A Grant's Telegram session is sealed with its Session key. The Session key is
//! never stored; each Token carries a secret from which we derive a verifier (to
//! recognise the Token) and a key-encryption key (to unwrap that Token's copy of
//! the Session key). A database dump without Tokens opens nothing.
//!
//! All SQL lives in this file. ADR-0001 planned a `Store` trait; with one backend
//! it would be a seam with nothing on the other side, so it arrives with Postgres.

mod crypto;
mod token;

use std::{
    fmt,
    path::Path,
    str::FromStr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use self::crypto::KEY_LEN;

const SESSION_AAD: &[u8] = b"tg-mcp/telegram-session/v1:";
const WRAP_AAD: &[u8] = b"tg-mcp/wrapped-key/v1:";
const VERIFIER_INFO: &[u8] = b"tg-mcp/token-verifier/v1";
const WRAP_INFO: &[u8] = b"tg-mcp/token-wrap/v1";

/// How long issued Tokens live.
#[derive(Debug, Clone, Copy)]
pub struct TokenTtl {
    pub access: Duration,
    pub refresh: Duration,
}

impl Default for TokenTtl {
    fn default() -> Self {
        Self {
            access: Duration::from_secs(60 * 60),
            refresh: Duration::from_secs(30 * 24 * 60 * 60),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GrantId(String);

impl GrantId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for GrantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A Token as handed to the client. `Debug` never prints it.
#[derive(Clone)]
pub struct Token(Zeroizing<String>);

impl Token {
    /// The bearer string. Only ever send it to the client it was issued for.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// An access + refresh Token pair, as an OAuth token response needs it.
#[derive(Debug, Clone)]
pub struct IssuedTokens {
    pub grant_id: GrantId,
    pub access: Token,
    pub access_expires_in: Duration,
    pub refresh: Token,
    pub refresh_expires_in: Duration,
}

/// A Grant opened with a valid access Token: its Telegram session in plaintext,
/// in memory only. Dropping it wipes the Session key and the session bytes.
pub struct OpenedGrant {
    pub grant_id: GrantId,
    pub account_id: i64,
    pub telegram_session: Zeroizing<Vec<u8>>,
    key: Zeroizing<[u8; KEY_LEN]>,
}

impl fmt::Debug for OpenedGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenedGrant")
            .field("grant_id", &self.grant_id)
            .field("account_id", &self.account_id)
            .field(
                "telegram_session",
                &format_args!("<{} bytes>", self.telegram_session.len()),
            )
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    /// Unknown, malformed, wrong-kind, expired or revoked Token. Deliberately one
    /// variant: callers (and attackers) learn nothing about which check failed.
    #[error("invalid token")]
    InvalidToken,
    /// The Token verified but its data does not decrypt: the database was altered.
    #[error("vault data for token {token_id} failed authentication")]
    Corrupt { token_id: String },
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Access,
    Refresh,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Access => "access",
            Kind::Refresh => "refresh",
        }
    }
}

pub struct Vault {
    pool: SqlitePool,
    ttl: TokenTtl,
}

impl Vault {
    /// Open (creating if missing) the SQLite database at `path` and run migrations.
    pub async fn open_path(path: &Path, ttl: TokenTtl) -> Result<Self, VaultError> {
        let options = SqliteConnectOptions::from_str("sqlite:")?
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool, ttl })
    }

    /// Close the pool, checkpointing the WAL into the main database file.
    pub async fn close(self) {
        self.pool.close().await;
    }

    /// Store a freshly logged-in Telegram session as a new Grant and issue its first
    /// Token pair. The Session key exists only inside the returned Tokens from now on.
    pub async fn create_grant(
        &self,
        account_id: i64,
        telegram_session: &[u8],
    ) -> Result<IssuedTokens, VaultError> {
        let grant_id = GrantId(uuid::Uuid::new_v4().to_string());
        let key = Zeroizing::new(crypto::random::<KEY_LEN>());
        let sealed = crypto::seal(&key, telegram_session, &session_aad(&grant_id));
        let now = unix_now();

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO grants (id, account_id, telegram_session, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(grant_id.as_str())
        .bind(account_id)
        .bind(sealed)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let issued = self.insert_pair(&mut tx, &grant_id, &key, now).await?;
        tx.commit().await?;

        tracing::info!(grant_id = %grant_id, "grant created");
        Ok(issued)
    }

    /// Verify an access Token and decrypt its Grant's Telegram session.
    pub async fn open(&self, access_token: &str) -> Result<OpenedGrant, VaultError> {
        let verified = self.verify(access_token, Kind::Access).await?;
        let row: Option<(i64, Vec<u8>)> =
            sqlx::query_as("SELECT account_id, telegram_session FROM grants WHERE id = ?")
                .bind(verified.grant_id.as_str())
                .fetch_optional(&self.pool)
                .await?;
        let (account_id, sealed) = row.ok_or(VaultError::InvalidToken)?;
        let telegram_session =
            crypto::open(&verified.key, &sealed, &session_aad(&verified.grant_id)).ok_or_else(
                || VaultError::Corrupt {
                    token_id: verified.token_id.clone(),
                },
            )?;
        Ok(OpenedGrant {
            grant_id: verified.grant_id,
            account_id,
            telegram_session,
            key: verified.key,
        })
    }

    /// Re-seal an opened Grant's (updated) Telegram session under the same Session key,
    /// so every live Token keeps working.
    pub async fn save_session(
        &self,
        grant: &OpenedGrant,
        telegram_session: &[u8],
    ) -> Result<(), VaultError> {
        let sealed = crypto::seal(&grant.key, telegram_session, &session_aad(&grant.grant_id));
        let updated =
            sqlx::query("UPDATE grants SET telegram_session = ?, updated_at = ? WHERE id = ?")
                .bind(sealed)
                .bind(unix_now())
                .bind(grant.grant_id.as_str())
                .execute(&self.pool)
                .await?;
        if updated.rows_affected() == 0 {
            // Revoked while it was open.
            return Err(VaultError::InvalidToken);
        }
        Ok(())
    }

    /// Exchange a refresh Token for a new pair. The old refresh Token is consumed;
    /// presenting it again (or racing it) fails.
    pub async fn refresh(&self, refresh_token: &str) -> Result<IssuedTokens, VaultError> {
        let verified = self.verify(refresh_token, Kind::Refresh).await?;
        let now = unix_now();
        let mut tx = self.pool.begin().await?;
        let consumed = sqlx::query("DELETE FROM tokens WHERE id = ?")
            .bind(&verified.token_id)
            .execute(&mut *tx)
            .await?;
        if consumed.rows_affected() != 1 {
            // A concurrent refresh with the same Token won.
            return Err(VaultError::InvalidToken);
        }
        let issued = self
            .insert_pair(&mut tx, &verified.grant_id, &verified.key, now)
            .await?;
        tx.commit().await?;
        Ok(issued)
    }

    /// Delete a Grant with its sealed Telegram session and all its Tokens.
    pub async fn delete_grant(&self, grant_id: &GrantId) -> Result<(), VaultError> {
        sqlx::query("DELETE FROM grants WHERE id = ?")
            .bind(grant_id.as_str())
            .execute(&self.pool)
            .await?;
        tracing::info!(grant_id = %grant_id, "grant deleted");
        Ok(())
    }

    /// Drop expired Tokens. Returns how many were removed.
    pub async fn purge_expired(&self) -> Result<u64, VaultError> {
        let purged = sqlx::query("DELETE FROM tokens WHERE expires_at <= ?")
            .bind(unix_now())
            .execute(&self.pool)
            .await?;
        Ok(purged.rows_affected())
    }

    async fn verify(&self, token: &str, expected: Kind) -> Result<Verified, VaultError> {
        let parsed = token::parse(token).ok_or(VaultError::InvalidToken)?;
        let TokenRow {
            grant_id,
            kind,
            verifier: stored_verifier,
            wrapped_key,
            expires_at,
        } = sqlx::query_as(
            "SELECT grant_id, kind, verifier, wrapped_key, expires_at FROM tokens WHERE id = ?",
        )
        .bind(&parsed.id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(VaultError::InvalidToken)?;

        let verifier = crypto::derive(parsed.secret.as_ref(), parsed.id.as_bytes(), VERIFIER_INFO);
        let secret_ok: bool = verifier.as_slice().ct_eq(&stored_verifier).into();
        if !secret_ok || kind != expected.as_str() || expires_at <= unix_now() {
            return Err(VaultError::InvalidToken);
        }

        let grant_id = GrantId(grant_id);
        let kek = crypto::derive(parsed.secret.as_ref(), parsed.id.as_bytes(), WRAP_INFO);
        let corrupt = || VaultError::Corrupt {
            token_id: parsed.id.clone(),
        };
        let unwrapped = crypto::open(&kek, &wrapped_key, &wrap_aad(&parsed.id, &grant_id))
            .ok_or_else(corrupt)?;
        let key: [u8; KEY_LEN] = unwrapped.as_slice().try_into().map_err(|_| corrupt())?;
        Ok(Verified {
            token_id: parsed.id,
            grant_id,
            key: Zeroizing::new(key),
        })
    }

    async fn insert_pair(
        &self,
        tx: &mut sqlx::SqliteConnection,
        grant_id: &GrantId,
        key: &[u8; KEY_LEN],
        now: i64,
    ) -> Result<IssuedTokens, VaultError> {
        let access = insert_token(tx, grant_id, key, Kind::Access, now, self.ttl.access).await?;
        let refresh = insert_token(tx, grant_id, key, Kind::Refresh, now, self.ttl.refresh).await?;
        Ok(IssuedTokens {
            grant_id: grant_id.clone(),
            access,
            access_expires_in: self.ttl.access,
            refresh,
            refresh_expires_in: self.ttl.refresh,
        })
    }
}

#[derive(sqlx::FromRow)]
struct TokenRow {
    grant_id: String,
    kind: String,
    verifier: Vec<u8>,
    wrapped_key: Vec<u8>,
    expires_at: i64,
}

struct Verified {
    token_id: String,
    grant_id: GrantId,
    key: Zeroizing<[u8; KEY_LEN]>,
}

async fn insert_token(
    conn: &mut sqlx::SqliteConnection,
    grant_id: &GrantId,
    key: &[u8; KEY_LEN],
    kind: Kind,
    now: i64,
    ttl: Duration,
) -> Result<Token, VaultError> {
    let id = token::new_id();
    let secret = Zeroizing::new(crypto::random::<{ token::SECRET_LEN }>());
    let verifier = crypto::derive(secret.as_ref(), id.as_bytes(), VERIFIER_INFO);
    let kek = crypto::derive(secret.as_ref(), id.as_bytes(), WRAP_INFO);
    let wrapped = crypto::seal(&kek, key, &wrap_aad(&id, grant_id));
    let expires_at = now.saturating_add(i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX));

    sqlx::query(
        "INSERT INTO tokens (id, grant_id, kind, verifier, wrapped_key, expires_at, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(grant_id.as_str())
    .bind(kind.as_str())
    .bind(verifier.as_slice())
    .bind(wrapped)
    .bind(expires_at)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(Token(token::format(&id, &secret)))
}

fn session_aad(grant_id: &GrantId) -> Vec<u8> {
    [SESSION_AAD, grant_id.as_str().as_bytes()].concat()
}

fn wrap_aad(token_id: &str, grant_id: &GrantId) -> Vec<u8> {
    [
        WRAP_AAD,
        token_id.as_bytes(),
        b":",
        grant_id.as_str().as_bytes(),
    ]
    .concat()
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every 32-byte window of `dump` tried as the Session key of `sealed`.
    fn any_window_opens(dump: &[u8], sealed: &[u8], aad: &[u8]) -> bool {
        dump.windows(KEY_LEN).any(|window| {
            let candidate: &[u8; KEY_LEN] = window.try_into().unwrap();
            crypto::open(candidate, sealed, aad).is_some()
        })
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    /// ADR-0003's core promise: whoever steals the database file, but no Token,
    /// opens no Telegram session.
    #[tokio::test]
    async fn database_dump_without_tokens_opens_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.db");
        let session = crypto::random::<256>();

        let vault = Vault::open_path(&path, TokenTtl::default()).await.unwrap();
        let issued = vault.create_grant(42, &session).await.unwrap();
        let rotated = vault.refresh(issued.refresh.expose()).await.unwrap();
        let opened = vault.open(rotated.access.expose()).await.unwrap();
        let real_key = *opened.key;
        vault.close().await;

        // The attacker's copy: the database file plus any WAL leftovers.
        let mut dump = std::fs::read(&path).unwrap();
        for suffix in ["-wal", "-shm"] {
            if let Ok(extra) = std::fs::read(format!("{}{suffix}", path.display())) {
                dump.extend(extra);
            }
        }
        let sealed: Vec<u8> = {
            let pool = SqlitePool::connect_with(SqliteConnectOptions::new().filename(&path))
                .await
                .unwrap();
            sqlx::query_scalar("SELECT telegram_session FROM grants")
                .fetch_one(&pool)
                .await
                .unwrap()
        };
        let aad = session_aad(&opened.grant_id);

        assert!(
            !contains(&dump, &session),
            "plaintext Telegram session in the dump"
        );
        assert!(
            !contains(&dump, &real_key),
            "plaintext Session key in the dump"
        );
        for token in [
            &issued.access,
            &issued.refresh,
            &rotated.access,
            &rotated.refresh,
        ] {
            let secret = token::parse(token.expose()).unwrap().secret;
            assert!(
                !contains(&dump, secret.as_ref()),
                "raw Token secret in the dump"
            );
            let encoded = token.expose().split_once('.').unwrap().1;
            assert!(
                !contains(&dump, encoded.as_bytes()),
                "encoded Token secret in the dump"
            );
        }
        assert!(
            !any_window_opens(&dump, &sealed, &aad),
            "a dump byte window decrypts the session"
        );

        // The search above is not vacuous: plant the key and it is found.
        let mut leaky = dump.clone();
        leaky.extend_from_slice(&real_key);
        assert!(any_window_opens(&leaky, &sealed, &aad));
    }
}
