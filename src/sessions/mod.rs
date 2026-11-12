//! The `sessions` module: the Session vault from ADR-0003.
//!
//! A Grant's Telegram session is sealed with its Session key. The Session key is
//! never stored; each Token carries a secret from which we derive a verifier (to
//! recognise the Token) and a key-encryption key (to unwrap that Token's copy of
//! the Session key). A Grant's Send allow-list is sealed with the same key. A
//! database dump without Tokens opens nothing.
//!
//! All SQL lives in this file. ADR-0001 planned a `Store` trait; with one backend
//! it would be a seam with nothing on the other side, so it arrives with Postgres.

mod crypto;
mod token;

pub(crate) use crypto::random as random_bytes;

use std::{
    fmt,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use sqlx::SqlitePool;
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use self::crypto::KEY_LEN;
use crate::{
    allow_list::{SendAllowList, WriteRefused, may_write},
    scope::{Scope, Scopes},
    tg::chats::ChatId,
};

const SESSION_AAD: &[u8] = b"tg-mcp/telegram-session/v1:";
/// The Send allow-list's own label: its blob never opens as a Telegram
/// session, nor as another Grant's list (the Grant id follows it).
const SEND_ALLOW_LIST_AAD: &[u8] = b"tg-mcp/send-allow-list/v1:";
const WRAP_AAD: &[u8] = b"tg-mcp/wrapped-key/v1:";
const VERIFIER_INFO: &[u8] = b"tg-mcp/token-verifier/v1";
const WRAP_INFO: &[u8] = b"tg-mcp/token-wrap/v1";

/// How long a consumed refresh Token stays fully usable (ADR-0003): two client
/// processes refreshing at once, or a retry after a lost response, both work.
pub const REFRESH_GRACE: Duration = Duration::from_secs(60);

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

#[cfg(test)]
impl From<String> for GrantId {
    fn from(id: String) -> Self {
        Self(id)
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
    /// The Grant's Scopes: what both Tokens give access to.
    pub scopes: Scopes,
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
    /// What this Grant may do, as chosen on Consent.
    pub scopes: Scopes,
    /// With `send`: the Chats it may write to, as chosen on Consent.
    pub send_allow_list: Option<SendAllowList>,
    pub telegram_session: Zeroizing<Vec<u8>>,
    key: Zeroizing<[u8; KEY_LEN]>,
}

impl OpenedGrant {
    /// May this Grant write to `chat` (send, react, mark as read)? Only with
    /// the `send` Scope and only into its Send allow-list.
    pub fn may_write_to(&self, chat: ChatId) -> Result<(), WriteRefused> {
        may_write(self.scopes, self.send_allow_list.as_ref(), chat)
    }
}

impl fmt::Debug for OpenedGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenedGrant")
            .field("grant_id", &self.grant_id)
            .field("account_id", &self.account_id)
            .field("scopes", &self.scopes)
            .field("send_allow_list", &self.send_allow_list)
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
    /// A valid Token presented by a client it was not issued to.
    #[error("token was issued to another client")]
    WrongClient,
    /// A genuine refresh Token was presented where it never may be: reused after
    /// its grace window, or by a client the Grant wasn't issued to. One of the two
    /// presenters is a thief, so the Grant is already deleted (with its Telegram
    /// session and every Token). The caller drops what it holds of the Grant in
    /// memory and answers as for [`VaultError::InvalidToken`].
    #[error("grant {grant_id} revoked: {reason}")]
    GrantRevoked {
        grant_id: GrantId,
        reason: RevokeReason,
    },
    /// The Token verified but its data does not decrypt: the database was altered.
    #[error("vault data for token {token_id} failed authentication")]
    Corrupt { token_id: String },
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// Why a Grant was revoked without its owner asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevokeReason {
    /// A consumed refresh Token came back after its grace window.
    RefreshReuse,
    /// A refresh Token came from a client the Grant wasn't issued to.
    ForeignClient,
}

impl RevokeReason {
    pub fn as_str(self) -> &'static str {
        match self {
            RevokeReason::RefreshReuse => "refresh_reuse",
            RevokeReason::ForeignClient => "foreign_client",
        }
    }
}

impl fmt::Display for RevokeReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
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

/// The vault's time, standing still until [`PausedClock::advance`] moves it: for
/// tests of the time-bound rules (expiry, the refresh grace window). Without one
/// the vault reads the system clock. Clones share one time.
#[derive(Debug, Clone)]
pub struct PausedClock(Arc<AtomicI64>);

impl PausedClock {
    /// Paused at the current system time.
    pub fn new() -> Self {
        Self(Arc::new(AtomicI64::new(unix_now())))
    }

    pub fn advance(&self, by: Duration) {
        let secs = i64::try_from(by.as_secs()).unwrap_or(i64::MAX);
        self.0.fetch_add(secs, Ordering::SeqCst);
    }

    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

impl Default for PausedClock {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Vault {
    pool: SqlitePool,
    ttl: TokenTtl,
    clock: Option<PausedClock>,
}

impl Vault {
    /// Open (creating if missing) the SQLite database at `path` and run migrations.
    pub async fn open_path(path: &Path, ttl: TokenTtl) -> Result<Self, VaultError> {
        Ok(Self::new(crate::db::open(path).await?, ttl))
    }

    /// A vault over an already migrated pool (see [`crate::db::open`]).
    pub fn new(pool: SqlitePool, ttl: TokenTtl) -> Self {
        Self {
            pool,
            ttl,
            clock: None,
        }
    }

    /// Read the time from `clock` instead of the system clock (tests).
    pub fn with_clock(mut self, clock: PausedClock) -> Self {
        self.clock = Some(clock);
        self
    }

    fn now(&self) -> i64 {
        self.clock.as_ref().map_or_else(unix_now, PausedClock::now)
    }

    /// Whether a refresh Token consumed at `consumed_at` is still in its grace window.
    fn in_grace(consumed_at: i64, now: i64) -> bool {
        now < consumed_at.saturating_add(grace_secs())
    }

    /// How long the Tokens it issues live.
    pub fn ttl(&self) -> TokenTtl {
        self.ttl
    }

    /// Close the pool, checkpointing the WAL into the main database file.
    pub async fn close(self) {
        self.pool.close().await;
    }

    /// [`Vault::create_grant_with_scopes`] for a Grant that may only read.
    pub async fn create_grant(
        &self,
        client_id: &str,
        account_id: i64,
        telegram_session: &[u8],
    ) -> Result<IssuedTokens, VaultError> {
        self.create_grant_with_scopes(client_id, account_id, telegram_session, Scopes::READ_ONLY)
            .await
    }

    /// [`Vault::create_grant_with_send`] without a Send allow-list.
    pub async fn create_grant_with_scopes(
        &self,
        client_id: &str,
        account_id: i64,
        telegram_session: &[u8],
        scopes: Scopes,
    ) -> Result<IssuedTokens, VaultError> {
        self.create_grant_with_send(client_id, account_id, telegram_session, scopes, None)
            .await
    }

    /// Store a freshly logged-in Telegram session as a new Grant of `client_id`
    /// with `scopes`, and issue its first Token pair. The Session key exists only
    /// inside the returned Tokens from now on. The Scopes never change after this
    /// (ADR-0005): other Scopes mean a new Grant.
    ///
    /// With `send` in `scopes`, `send_allow_list` is sealed with the Session key
    /// and stored with the Grant (without one, the Grant may write nowhere);
    /// without `send` it is not stored. It never changes either.
    pub async fn create_grant_with_send(
        &self,
        client_id: &str,
        account_id: i64,
        telegram_session: &[u8],
        scopes: Scopes,
        send_allow_list: Option<&SendAllowList>,
    ) -> Result<IssuedTokens, VaultError> {
        let grant_id = GrantId(uuid::Uuid::new_v4().to_string());
        let key = Zeroizing::new(crypto::random::<KEY_LEN>());
        let sealed = crypto::seal(&key, telegram_session, &session_aad(&grant_id));
        let send_allow_list = send_allow_list.filter(|_| scopes.contains(Scope::Send));
        let sealed_list = send_allow_list
            .map(|list| crypto::seal(&key, &list.encode(), &send_allow_list_aad(&grant_id)));
        let now = self.now();

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO grants (id, client_id, account_id, telegram_session, scopes, send_allow_list, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(grant_id.as_str())
        .bind(client_id)
        .bind(account_id)
        .bind(sealed)
        .bind(scopes.to_string())
        .bind(sealed_list)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let issued = self
            .insert_pair(&mut tx, &grant_id, scopes, &key, now)
            .await?;
        tx.commit().await?;

        // Counts only: the Chats themselves are as private as their names.
        tracing::info!(
            grant_id = %grant_id,
            scopes = %scopes,
            send_chats = send_allow_list.map_or(0, SendAllowList::len),
            auto_send = send_allow_list.is_some_and(SendAllowList::auto_send),
            "grant created"
        );
        Ok(issued)
    }

    /// Verify an access Token and decrypt its Grant's Telegram session.
    pub async fn open(&self, access_token: &str) -> Result<OpenedGrant, VaultError> {
        let verified = self.verify(access_token, &[Kind::Access]).await?;
        self.decrypt(verified).await
    }

    /// Verify a Token of either kind that was issued to `client_id`, and decrypt its
    /// Grant's Telegram session. For revocation: a client typically revokes with its
    /// refresh Token, and only a Token can open the session to log it out (ADR-0003).
    pub async fn open_for_client(
        &self,
        token: &str,
        client_id: &str,
    ) -> Result<OpenedGrant, VaultError> {
        let verified = self.verify(token, &[Kind::Access, Kind::Refresh]).await?;
        if !self.owned_by(&verified.grant_id, client_id).await? {
            return Err(VaultError::WrongClient);
        }
        self.decrypt(verified).await
    }

    async fn decrypt(&self, verified: Verified) -> Result<OpenedGrant, VaultError> {
        let GrantRow {
            account_id,
            telegram_session: sealed,
            scopes,
            send_allow_list: sealed_list,
        } = sqlx::query_as(
            "SELECT account_id, telegram_session, scopes, send_allow_list FROM grants WHERE id = ?",
        )
        .bind(verified.grant_id.as_str())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(VaultError::InvalidToken)?;
        let corrupt = || VaultError::Corrupt {
            token_id: verified.token_id.clone(),
        };
        let telegram_session =
            crypto::open(&verified.key, &sealed, &session_aad(&verified.grant_id))
                .ok_or_else(corrupt)?;
        let send_allow_list = sealed_list
            .map(|sealed| {
                crypto::open(
                    &verified.key,
                    &sealed,
                    &send_allow_list_aad(&verified.grant_id),
                )
                .and_then(|plain| SendAllowList::decode(&plain))
                .ok_or_else(corrupt)
            })
            .transpose()?;
        Ok(OpenedGrant {
            grant_id: verified.grant_id,
            account_id,
            scopes: Scopes::from_stored(&scopes),
            send_allow_list,
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
                .bind(self.now())
                .bind(grant.grant_id.as_str())
                .execute(&self.pool)
                .await?;
        if updated.rows_affected() == 0 {
            // Revoked while it was open.
            return Err(VaultError::InvalidToken);
        }
        Ok(())
    }

    /// Exchange a refresh Token for a new pair, with the Grant's Scopes as they are
    /// (a refresh never changes them). Only the client the Grant was issued to may
    /// do this. The presented refresh Token is consumed: it stays fully
    /// usable for [`REFRESH_GRACE`] after its first use (each use issues another
    /// pair), then it only serves to recognise a reuse.
    ///
    /// A refresh Token whose secret matches but that is past its grace window, or
    /// whose Grant belongs to another client, revokes the Grant
    /// ([`VaultError::GrantRevoked`]). A secret that doesn't match revokes nothing:
    /// only the real Token may end a Grant.
    pub async fn refresh(
        &self,
        refresh_token: &str,
        client_id: &str,
    ) -> Result<IssuedTokens, VaultError> {
        let now = self.now();
        let matched = self.find(refresh_token, &[Kind::Refresh], now).await?;
        let grant: Option<(String, String)> =
            sqlx::query_as("SELECT client_id, scopes FROM grants WHERE id = ?")
                .bind(matched.grant_id.as_str())
                .fetch_optional(&self.pool)
                .await?;
        let Some((owner, scopes)) = grant else {
            // Deleted since the Token was read.
            return Err(VaultError::InvalidToken);
        };
        if !bool::from(owner.as_bytes().ct_eq(client_id.as_bytes())) {
            return Err(self
                .revoke_stolen(matched.grant_id, RevokeReason::ForeignClient)
                .await);
        }
        let grant_id = matched.grant_id.clone();
        let Some(usable) = matched.usable(now) else {
            return Err(self
                .revoke_stolen(grant_id, RevokeReason::RefreshReuse)
                .await);
        };
        let verified = usable.open_key()?;
        let mut tx = self.pool.begin().await?;
        // The grace window counts from the first use, so a later use within it
        // leaves `consumed_at` as it is.
        let consumed = sqlx::query(
            "UPDATE tokens SET consumed_at = COALESCE(consumed_at, ?1)
             WHERE id = ?2 AND (consumed_at IS NULL OR consumed_at > ?1 - ?3)",
        )
        .bind(now)
        .bind(&verified.token_id)
        .bind(grace_secs())
        .execute(&mut *tx)
        .await?;
        if consumed.rows_affected() != 1 {
            // Revoked, or its grace window ended, since it was verified.
            return Err(VaultError::InvalidToken);
        }
        let scopes = Scopes::from_stored(&scopes);
        let issued = self
            .insert_pair(&mut tx, &verified.grant_id, scopes, &verified.key, now)
            .await?;
        tx.commit().await?;
        Ok(issued)
    }

    /// The OAuth client a Grant was issued to, while the Grant exists. Needs
    /// no Token and opens nothing: for the confirmation page, which carries
    /// no Token and shows who asks.
    pub async fn grant_client(&self, grant_id: &GrantId) -> Result<Option<String>, VaultError> {
        Ok(
            sqlx::query_scalar("SELECT client_id FROM grants WHERE id = ?")
                .bind(grant_id.as_str())
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// Whether a Grant still exists (it isn't revoked or purged). Needs no
    /// Token, and opens nothing: for Media links, which carry no Token.
    pub async fn grant_exists(&self, grant_id: &GrantId) -> Result<bool, VaultError> {
        let found = sqlx::query("SELECT 1 FROM grants WHERE id = ?")
            .bind(grant_id.as_str())
            .fetch_optional(&self.pool)
            .await?;
        Ok(found.is_some())
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

    /// Delete the Wrapped key of every refresh Token consumed longer than
    /// [`REFRESH_GRACE`] ago; its id, Grant, verifier and expiry stay, to recognise a
    /// reuse. Verification already refuses such a Token; this removes the key from
    /// the database file. Run it every minute. Returns how many keys were deleted.
    pub async fn drop_consumed_keys(&self) -> Result<u64, VaultError> {
        let dropped = sqlx::query(
            "UPDATE tokens SET wrapped_key = NULL
             WHERE consumed_at IS NOT NULL AND consumed_at <= ? - ? AND wrapped_key IS NOT NULL",
        )
        .bind(self.now())
        .bind(grace_secs())
        .execute(&self.pool)
        .await?;
        Ok(dropped.rows_affected())
    }

    /// Drop expired Tokens (consumed ones too, at their original expiry), then
    /// Grants left without a live Token: nothing can ever decrypt those again
    /// (ADR-0003), and a consumed Token keeps no Grant alive. Returns how many
    /// Tokens were removed.
    pub async fn purge_expired(&self) -> Result<u64, VaultError> {
        let purged = sqlx::query("DELETE FROM tokens WHERE expires_at <= ?")
            .bind(self.now())
            .execute(&self.pool)
            .await?;
        let orphans = sqlx::query(
            "DELETE FROM grants WHERE NOT EXISTS (
                 SELECT 1 FROM tokens WHERE tokens.grant_id = grants.id AND tokens.consumed_at IS NULL
             )",
        )
        .execute(&self.pool)
        .await?;
        if orphans.rows_affected() > 0 {
            tracing::info!(
                grants = orphans.rows_affected(),
                "grants without tokens deleted"
            );
        }
        Ok(purged.rows_affected())
    }

    /// Was this Grant issued to `client_id`? Constant-time on the id.
    async fn owned_by(&self, grant_id: &GrantId, client_id: &str) -> Result<bool, VaultError> {
        let owner: Option<String> = sqlx::query_scalar("SELECT client_id FROM grants WHERE id = ?")
            .bind(grant_id.as_str())
            .fetch_optional(&self.pool)
            .await?;
        Ok(owner.is_some_and(|owner| owner.as_bytes().ct_eq(client_id.as_bytes()).into()))
    }

    /// Verify a Token that is usable now: live, or consumed and in its grace window.
    async fn verify(&self, token: &str, accepted: &[Kind]) -> Result<Verified, VaultError> {
        let now = self.now();
        self.find(token, accepted, now)
            .await?
            .usable(now)
            .ok_or(VaultError::InvalidToken)?
            .open_key()
    }

    /// The stored record of a Token of an `accepted` kind whose secret matches its
    /// verifier and that hasn't expired. It may be consumed and past its grace
    /// window: see [`Matched::usable`].
    async fn find(&self, token: &str, accepted: &[Kind], now: i64) -> Result<Matched, VaultError> {
        let parsed = token::parse(token).ok_or(VaultError::InvalidToken)?;
        let TokenRow {
            grant_id,
            kind,
            verifier: stored_verifier,
            wrapped_key,
            expires_at,
            consumed_at,
        } = sqlx::query_as(
            "SELECT grant_id, kind, verifier, wrapped_key, expires_at, consumed_at
             FROM tokens WHERE id = ?",
        )
        .bind(&parsed.id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(VaultError::InvalidToken)?;

        let verifier = crypto::derive(parsed.secret.as_ref(), parsed.id.as_bytes(), VERIFIER_INFO);
        let secret_ok: bool = verifier.as_slice().ct_eq(&stored_verifier).into();
        let kind_ok = accepted.iter().any(|k| k.as_str() == kind);
        if !secret_ok || !kind_ok || expires_at <= now {
            return Err(VaultError::InvalidToken);
        }
        Ok(Matched {
            parsed,
            grant_id: GrantId(grant_id),
            wrapped_key,
            consumed_at,
        })
    }

    /// Delete a Grant whose genuine refresh Token was misused, and say so.
    async fn revoke_stolen(&self, grant_id: GrantId, reason: RevokeReason) -> VaultError {
        if let Err(err) = sqlx::query("DELETE FROM grants WHERE id = ?")
            .bind(grant_id.as_str())
            .execute(&self.pool)
            .await
        {
            return err.into();
        }
        tracing::warn!(
            grant_id = %grant_id,
            reason = reason.as_str(),
            "refresh token misused, grant revoked"
        );
        VaultError::GrantRevoked { grant_id, reason }
    }

    async fn insert_pair(
        &self,
        tx: &mut sqlx::SqliteConnection,
        grant_id: &GrantId,
        scopes: Scopes,
        key: &[u8; KEY_LEN],
        now: i64,
    ) -> Result<IssuedTokens, VaultError> {
        let access = insert_token(tx, grant_id, key, Kind::Access, now, self.ttl.access).await?;
        let refresh = insert_token(tx, grant_id, key, Kind::Refresh, now, self.ttl.refresh).await?;
        Ok(IssuedTokens {
            grant_id: grant_id.clone(),
            scopes,
            access,
            access_expires_in: self.ttl.access,
            refresh,
            refresh_expires_in: self.ttl.refresh,
        })
    }
}

/// What opening a Grant reads: all of it sealed but its Account and Scopes.
#[derive(sqlx::FromRow)]
struct GrantRow {
    account_id: i64,
    telegram_session: Vec<u8>,
    scopes: String,
    send_allow_list: Option<Vec<u8>>,
}

#[derive(sqlx::FromRow)]
struct TokenRow {
    grant_id: String,
    kind: String,
    verifier: Vec<u8>,
    wrapped_key: Option<Vec<u8>>,
    expires_at: i64,
    consumed_at: Option<i64>,
}

/// A Token record whose secret matched.
struct Matched {
    parsed: token::ParsedToken,
    grant_id: GrantId,
    wrapped_key: Option<Vec<u8>>,
    consumed_at: Option<i64>,
}

impl Matched {
    /// `Some` if the Token may be used now: never consumed, or consumed less than
    /// [`REFRESH_GRACE`] ago (with its Wrapped key still there).
    fn usable(self, now: i64) -> Option<Self> {
        let in_grace = self
            .consumed_at
            .is_none_or(|consumed_at| Vault::in_grace(consumed_at, now));
        (in_grace && self.wrapped_key.is_some()).then_some(self)
    }

    /// Unwrap the Session key with the Token's secret.
    fn open_key(self) -> Result<Verified, VaultError> {
        let Matched {
            parsed,
            grant_id,
            wrapped_key,
            ..
        } = self;
        let wrapped_key = wrapped_key.ok_or(VaultError::InvalidToken)?;
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

fn send_allow_list_aad(grant_id: &GrantId) -> Vec<u8> {
    [SEND_ALLOW_LIST_AAD, grant_id.as_str().as_bytes()].concat()
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

fn grace_secs() -> i64 {
    i64::try_from(REFRESH_GRACE.as_secs()).unwrap_or(i64::MAX)
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

    /// The attacker's copy: the database file plus any WAL leftovers.
    fn dump(path: &Path) -> Vec<u8> {
        let mut dump = std::fs::read(path).unwrap();
        for suffix in ["-wal", "-shm"] {
            if let Ok(extra) = std::fs::read(format!("{}{suffix}", path.display())) {
                dump.extend(extra);
            }
        }
        dump
    }

    /// Chats of a Send allow-list, with ids unlikely to turn up by chance.
    fn chosen_chats() -> SendAllowList {
        SendAllowList::new(
            [
                ChatId::User(4_711_000_042),
                ChatId::Chat(4_711_000_043),
                ChatId::Channel(1_987_654_321_987),
            ],
            true,
        )
        .unwrap()
    }

    fn sending() -> Scopes {
        [Scope::Send].into_iter().collect()
    }

    /// ADR-0003's core promise: whoever steals the database file, but no Token,
    /// opens no Telegram session and no Send allow-list, and doesn't see which
    /// Chats are on it. The first refresh Token is consumed and still in its
    /// grace window here, so its Wrapped key is in the file too.
    #[tokio::test]
    async fn database_dump_without_tokens_opens_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.db");
        let session = crypto::random::<256>();
        let list = chosen_chats();

        let vault = Vault::open_path(&path, TokenTtl::default()).await.unwrap();
        let issued = vault
            .create_grant_with_send("client", 42, &session, sending(), Some(&list))
            .await
            .unwrap();
        let rotated = vault
            .refresh(issued.refresh.expose(), "client")
            .await
            .unwrap();
        let opened = vault.open(rotated.access.expose()).await.unwrap();
        assert_eq!(opened.send_allow_list.as_ref(), Some(&list));
        let real_key = *opened.key;
        vault.close().await;

        let dump = dump(&path);
        let (sealed, sealed_list): (Vec<u8>, Vec<u8>) = {
            let pool =
                SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
                    .await
                    .unwrap();
            sqlx::query_as("SELECT telegram_session, send_allow_list FROM grants")
                .fetch_one(&pool)
                .await
                .unwrap()
        };
        let aad = session_aad(&opened.grant_id);
        let list_aad = send_allow_list_aad(&opened.grant_id);

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
        assert!(
            !any_window_opens(&dump, &sealed_list, &list_aad),
            "a dump byte window decrypts the Send allow-list"
        );
        assert!(
            !contains(&dump, &list.encode()[..6 + 3 * 9]),
            "plaintext Send allow-list in the dump"
        );
        for chat in list.chats() {
            let (kind, id) = match chat {
                ChatId::User(id) => (1u8, id),
                ChatId::Chat(id) => (2, id),
                ChatId::Channel(id) => (3, id),
            };
            for needle in [
                [&[kind][..], &id.to_le_bytes()].concat(),
                id.to_le_bytes().to_vec(),
                id.to_be_bytes().to_vec(),
                id.to_string().into_bytes(),
                chat.to_string().into_bytes(),
            ] {
                assert!(!contains(&dump, &needle), "{chat} in the dump");
            }
        }

        // The searches above are not vacuous: plant the key and both open.
        let mut leaky = dump.clone();
        leaky.extend_from_slice(&real_key);
        assert!(any_window_opens(&leaky, &sealed, &aad));
        assert!(any_window_opens(&leaky, &sealed_list, &list_aad));
    }

    /// A Send allow-list opens only as its own Grant's list: not swapped into
    /// the Telegram session's place, nor into another Grant (of the same
    /// Account, or with the same key), and the auto-send flag can't be flipped.
    #[tokio::test]
    async fn a_send_allow_list_opens_only_where_it_was_sealed() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open_path(&dir.path().join("vault.db"), TokenTtl::default())
            .await
            .unwrap();
        let list = chosen_chats();
        let mine = vault
            .create_grant_with_send("client", 42, b"session", sending(), Some(&list))
            .await
            .unwrap();
        let other = vault
            .create_grant_with_send("client", 42, b"session", sending(), Some(&list))
            .await
            .unwrap();
        let blobs = |grant: &GrantId| {
            let pool = vault.pool.clone();
            let id = grant.as_str().to_string();
            async move {
                sqlx::query_as::<_, (Vec<u8>, Vec<u8>)>(
                    "SELECT telegram_session, send_allow_list FROM grants WHERE id = ?",
                )
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        let (session, my_list) = blobs(&mine.grant_id).await;
        let (_, other_list) = blobs(&other.grant_id).await;
        let key = *vault.open(mine.access.expose()).await.unwrap().key;

        // The same key can't open one blob as the other.
        assert!(crypto::open(&key, &my_list, &session_aad(&mine.grant_id)).is_none());
        assert!(crypto::open(&key, &session, &send_allow_list_aad(&mine.grant_id)).is_none());

        let corrupt = |result: Result<OpenedGrant, VaultError>| {
            matches!(result, Err(VaultError::Corrupt { .. }))
        };
        let set_list = |grant: &GrantId, blob: Vec<u8>| {
            let pool = vault.pool.clone();
            let id = grant.as_str().to_string();
            async move {
                sqlx::query("UPDATE grants SET send_allow_list = ? WHERE id = ?")
                    .bind(blob)
                    .bind(id)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
        };
        // Another Grant's list, the session blob, or a flipped bit: refused.
        set_list(&mine.grant_id, other_list).await;
        assert!(corrupt(vault.open(mine.access.expose()).await));
        set_list(&mine.grant_id, session.clone()).await;
        assert!(corrupt(vault.open(mine.access.expose()).await));
        let mut flipped = my_list.clone();
        flipped[24 + 1] ^= 1; // inside the ciphertext's first bytes: version, flags
        set_list(&mine.grant_id, flipped).await;
        assert!(corrupt(vault.open(mine.access.expose()).await));
        // Its own: back as it was.
        set_list(&mine.grant_id, my_list).await;
        let opened = vault.open(mine.access.expose()).await.unwrap();
        assert_eq!(opened.send_allow_list, Some(list));
    }

    /// A Grant made before Grants had Scopes (the schema of migrations 0001-0003)
    /// can only have been read-only: after migrating it opens as `read`, its
    /// Tokens keep working, and a refresh keeps it `read`.
    #[tokio::test]
    async fn a_grant_from_before_scopes_reads_only_and_keeps_working() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.db");

        // The schema as it was before Scopes, and a Grant as that vault wrote it.
        let old_migrations = tempfile::tempdir().unwrap();
        let migrations = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        for entry in std::fs::read_dir(&migrations).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            if name.as_str() < "0004" {
                std::fs::copy(entry.path(), old_migrations.path().join(&name)).unwrap();
            }
        }
        let pool = SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true),
        )
        .await
        .unwrap();
        sqlx::migrate::Migrator::new(old_migrations.path())
            .await
            .unwrap()
            .run(&pool)
            .await
            .unwrap();
        let grant_id = GrantId(uuid::Uuid::new_v4().to_string());
        let key = Zeroizing::new(crypto::random::<KEY_LEN>());
        let now = unix_now();
        sqlx::query(
            "INSERT INTO grants (id, client_id, account_id, telegram_session, created_at, updated_at)
             VALUES (?, 'client', 42, ?, ?, ?)",
        )
        .bind(grant_id.as_str())
        .bind(crypto::seal(&key, b"old session", &session_aad(&grant_id)))
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        let ttl = TokenTtl::default();
        let mut conn = pool.acquire().await.unwrap();
        let access = insert_token(&mut conn, &grant_id, &key, Kind::Access, now, ttl.access)
            .await
            .unwrap();
        let refresh = insert_token(&mut conn, &grant_id, &key, Kind::Refresh, now, ttl.refresh)
            .await
            .unwrap();
        drop(conn);
        pool.close().await;

        let vault = Vault::open_path(&path, ttl).await.unwrap();
        let opened = vault.open(access.expose()).await.unwrap();
        assert_eq!(opened.scopes, Scopes::READ_ONLY);
        assert_eq!(opened.telegram_session.as_slice(), b"old session");
        let rotated = vault.refresh(refresh.expose(), "client").await.unwrap();
        assert_eq!(rotated.scopes, Scopes::READ_ONLY);
        let reopened = vault.open(rotated.access.expose()).await.unwrap();
        assert_eq!(reopened.scopes, Scopes::READ_ONLY);
    }

    /// After the grace window the sweep removes a consumed refresh Token's Wrapped
    /// key from the file itself, not just from the row: with that Token (say, from
    /// an old copy of a client's credential store) a dump opens nothing either.
    #[tokio::test]
    async fn swept_wrapped_keys_leave_the_database_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.db");
        let clock = PausedClock::new();
        let vault = Vault::open_path(&path, TokenTtl::default())
            .await
            .unwrap()
            .with_clock(clock.clone());
        let issued = vault.create_grant("client", 42, b"session").await.unwrap();
        vault
            .refresh(issued.refresh.expose(), "client")
            .await
            .unwrap();
        let consumed = token::parse(issued.refresh.expose()).unwrap();
        let wrapped: Vec<u8> = sqlx::query_scalar("SELECT wrapped_key FROM tokens WHERE id = ?")
            .bind(&consumed.id)
            .fetch_one(&vault.pool)
            .await
            .unwrap();

        clock.advance(REFRESH_GRACE);
        assert_eq!(vault.drop_consumed_keys().await.unwrap(), 1);
        vault.close().await;

        let dump = dump(&path);
        assert!(contains(&dump, consumed.id.as_bytes()), "the record stays");
        assert!(
            !contains(&dump, &wrapped),
            "a swept Wrapped key is still in the file"
        );
    }
}
