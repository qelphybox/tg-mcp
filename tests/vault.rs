//! Session vault behaviour through its public API, on a real SQLite file.

use std::time::Duration;

use tempfile::TempDir;
use tg_mcp::sessions::{GrantId, PausedClock, RevokeReason, TokenTtl, Vault, VaultError};

const SESSION: &[u8] = b"grammers-session-bytes: auth key, dc, peers";

async fn vault(ttl: TokenTtl) -> (Vault, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let vault = Vault::open_path(&dir.path().join("vault.db"), ttl)
        .await
        .unwrap();
    (vault, dir)
}

/// A vault whose time stands still until the test advances `clock`.
async fn paused_vault(ttl: TokenTtl) -> (Vault, PausedClock, TempDir) {
    let (vault, dir) = vault(ttl).await;
    let clock = PausedClock::new();
    (vault.with_clock(clock.clone()), clock, dir)
}

/// How long a consumed refresh Token keeps working (ADR-0003).
const GRACE: Duration = Duration::from_secs(60);

fn assert_invalid<T: std::fmt::Debug>(result: Result<T, VaultError>) {
    match result {
        Err(VaultError::InvalidToken) => {}
        other => panic!("expected InvalidToken, got {other:?}"),
    }
}

#[tokio::test]
async fn access_token_opens_the_grant() {
    let (vault, _dir) = vault(TokenTtl::default()).await;
    let issued = vault.create_grant("client", 42, SESSION).await.unwrap();

    let grant = vault.open(issued.access.expose()).await.unwrap();
    assert_eq!(grant.grant_id, issued.grant_id);
    assert_eq!(grant.account_id, 42);
    assert_eq!(grant.telegram_session.as_slice(), SESSION);
}

#[tokio::test]
async fn bad_tokens_are_all_the_same_invalid_token() {
    let (vault, _dir) = vault(TokenTtl::default()).await;
    let issued = vault.create_grant("client", 42, SESSION).await.unwrap();
    let access = issued.access.expose();
    let (id, secret) = access.split_once('.').unwrap();

    // Flip one character of the secret, keeping it well-formed.
    let mut forged_secret: Vec<char> = secret.chars().collect();
    forged_secret[0] = if forged_secret[0] == 'A' { 'B' } else { 'A' };
    let forged = format!("{id}.{}", forged_secret.into_iter().collect::<String>());

    assert_invalid(vault.open("").await);
    assert_invalid(vault.open("garbage").await);
    assert_invalid(vault.open(&access[..access.len() - 3]).await);
    assert_invalid(vault.open(&forged).await);
    // Right secret, unknown id.
    assert_invalid(
        vault
            .open(&format!("tgm_AAAAAAAAAAAAAAAAAAAAAA.{secret}"))
            .await,
    );
    // A refresh Token is not an access Token.
    assert_invalid(vault.open(issued.refresh.expose()).await);
    // An access Token can't refresh.
    assert_invalid(vault.refresh(access, "client").await);
}

#[tokio::test]
async fn expired_tokens_are_rejected() {
    let ttl = TokenTtl {
        access: Duration::ZERO,
        refresh: Duration::ZERO,
    };
    let (vault, _dir) = vault(ttl).await;
    let issued = vault.create_grant("client", 42, SESSION).await.unwrap();

    assert_invalid(vault.open(issued.access.expose()).await);
    assert_invalid(vault.refresh(issued.refresh.expose(), "client").await);
    assert_eq!(vault.purge_expired().await.unwrap(), 2);
}

#[tokio::test]
async fn refresh_rotates_and_the_new_refresh_token_carries_on() {
    let (vault, clock, _dir) = paused_vault(TokenTtl::default()).await;
    let first = vault.create_grant("client", 42, SESSION).await.unwrap();

    let second = vault
        .refresh(first.refresh.expose(), "client")
        .await
        .unwrap();
    assert_eq!(second.grant_id, first.grant_id);
    assert_ne!(second.access.expose(), first.access.expose());
    assert_ne!(second.refresh.expose(), first.refresh.expose());

    let grant = vault.open(second.access.expose()).await.unwrap();
    assert_eq!(grant.telegram_session.as_slice(), SESSION);
    // A consumed refresh Token is never an access Token, even in its grace window.
    assert_invalid(vault.open(first.refresh.expose()).await);

    // The new refresh Token keeps the chain going after the window; reusing the
    // old one then is theft (reuse_after_the_grace_window_revokes_the_grant).
    clock.advance(GRACE);
    vault
        .refresh(second.refresh.expose(), "client")
        .await
        .unwrap();
}

#[tokio::test]
async fn a_consumed_refresh_token_keeps_working_for_its_grace_window() {
    let (vault, clock, _dir) = paused_vault(TokenTtl::default()).await;
    let first = vault.create_grant("client", 42, SESSION).await.unwrap();

    let second = vault
        .refresh(first.refresh.expose(), "client")
        .await
        .unwrap();
    // A retry after a lost response, just before the window closes.
    clock.advance(GRACE - Duration::from_secs(1));
    let third = vault
        .refresh(first.refresh.expose(), "client")
        .await
        .unwrap();

    assert_ne!(third.refresh.expose(), second.refresh.expose());
    for pair in [&second, &third] {
        assert_eq!(pair.grant_id, first.grant_id);
        let grant = vault.open(pair.access.expose()).await.unwrap();
        assert_eq!(grant.telegram_session.as_slice(), SESSION);
        vault
            .refresh(pair.refresh.expose(), "client")
            .await
            .unwrap();
    }
}

/// The Token with one character of its secret changed, still well-formed.
fn forged(token: &str) -> String {
    let (id, secret) = token.split_once('.').unwrap();
    let mut chars: Vec<char> = secret.chars().collect();
    chars[0] = if chars[0] == 'A' { 'B' } else { 'A' };
    format!("{id}.{}", chars.into_iter().collect::<String>())
}

fn assert_revoked<T: std::fmt::Debug>(
    result: Result<T, VaultError>,
    grant: &GrantId,
    why: RevokeReason,
) {
    match result {
        Err(VaultError::GrantRevoked { grant_id, reason })
            if &grant_id == grant && reason == why => {}
        other => panic!("expected GrantRevoked({grant}, {why:?}), got {other:?}"),
    }
}

#[tokio::test]
async fn reuse_after_the_grace_window_revokes_the_grant() {
    let (vault, clock, _dir) = paused_vault(TokenTtl::default()).await;
    let first = vault.create_grant("client", 42, SESSION).await.unwrap();
    let other = vault.create_grant("client", 7, SESSION).await.unwrap();
    let second = vault
        .refresh(first.refresh.expose(), "client")
        .await
        .unwrap();
    clock.advance(GRACE);

    assert_revoked(
        vault.refresh(first.refresh.expose(), "client").await,
        &first.grant_id,
        RevokeReason::RefreshReuse,
    );

    assert!(!vault.grant_exists(&first.grant_id).await.unwrap());
    assert_invalid(vault.open(second.access.expose()).await);
    assert_invalid(vault.refresh(second.refresh.expose(), "client").await);
    assert_invalid(vault.refresh(first.refresh.expose(), "client").await);
    // Other Grants are untouched.
    vault.open(other.access.expose()).await.unwrap();
}

#[tokio::test]
async fn a_genuine_refresh_token_from_another_client_revokes_the_grant() {
    let (vault, _dir) = vault(TokenTtl::default()).await;

    // A live refresh Token.
    let live = vault.create_grant("client", 42, SESSION).await.unwrap();
    assert_revoked(
        vault.refresh(live.refresh.expose(), "other-client").await,
        &live.grant_id,
        RevokeReason::ForeignClient,
    );
    assert!(!vault.grant_exists(&live.grant_id).await.unwrap());
    assert_invalid(vault.open(live.access.expose()).await);

    // A consumed one, still in its grace window.
    let first = vault.create_grant("client", 42, SESSION).await.unwrap();
    let second = vault
        .refresh(first.refresh.expose(), "client")
        .await
        .unwrap();
    assert_revoked(
        vault.refresh(first.refresh.expose(), "other-client").await,
        &first.grant_id,
        RevokeReason::ForeignClient,
    );
    assert_invalid(vault.open(second.access.expose()).await);
    assert_invalid(vault.refresh(second.refresh.expose(), "client").await);
}

#[tokio::test]
async fn a_forged_secret_for_a_consumed_token_revokes_nothing() {
    let (vault, clock, _dir) = paused_vault(TokenTtl::default()).await;
    let first = vault.create_grant("client", 42, SESSION).await.unwrap();
    let second = vault
        .refresh(first.refresh.expose(), "client")
        .await
        .unwrap();
    let forged = forged(first.refresh.expose());

    assert_invalid(vault.refresh(&forged, "client").await);
    clock.advance(GRACE);
    assert_invalid(vault.refresh(&forged, "client").await);
    assert_invalid(vault.refresh(&forged, "other-client").await);

    assert!(vault.grant_exists(&first.grant_id).await.unwrap());
    vault.open(second.access.expose()).await.unwrap();
    vault
        .refresh(second.refresh.expose(), "client")
        .await
        .unwrap();
}

#[tokio::test]
async fn concurrent_refreshes_with_one_token_both_succeed() {
    let (vault, _dir) = vault(TokenTtl::default()).await;
    let issued = vault.create_grant("client", 42, SESSION).await.unwrap();
    let token = issued.refresh.expose();

    let (a, b) = tokio::join!(
        vault.refresh(token, "client"),
        vault.refresh(token, "client")
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_ne!(a.refresh.expose(), b.refresh.expose());
    for pair in [&a, &b] {
        vault.open(pair.access.expose()).await.unwrap();
    }
}

#[tokio::test]
async fn updated_session_is_visible_through_every_live_token() {
    let (vault, _dir) = vault(TokenTtl::default()).await;
    let first = vault.create_grant("client", 42, SESSION).await.unwrap();
    let second = vault
        .refresh(first.refresh.expose(), "client")
        .await
        .unwrap();

    let grant = vault.open(first.access.expose()).await.unwrap();
    vault
        .save_session(&grant, b"session after DC migration")
        .await
        .unwrap();

    for access in [&first.access, &second.access] {
        let reopened = vault.open(access.expose()).await.unwrap();
        assert_eq!(
            reopened.telegram_session.as_slice(),
            b"session after DC migration"
        );
    }
}

#[tokio::test]
async fn deleting_a_grant_kills_its_tokens() {
    let (vault, _dir) = vault(TokenTtl::default()).await;
    let issued = vault.create_grant("client", 42, SESSION).await.unwrap();
    let other = vault
        .create_grant("client", 7, b"someone else")
        .await
        .unwrap();
    let grant = vault.open(issued.access.expose()).await.unwrap();

    vault.delete_grant(&issued.grant_id).await.unwrap();

    assert_invalid(vault.open(issued.access.expose()).await);
    assert_invalid(vault.refresh(issued.refresh.expose(), "client").await);
    assert_invalid(vault.save_session(&grant, b"late write").await);
    vault.open(other.access.expose()).await.unwrap();
}

#[tokio::test]
async fn grants_survive_reopening_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vault.db");
    let issued = {
        let vault = Vault::open_path(&path, TokenTtl::default()).await.unwrap();
        let issued = vault.create_grant("client", 42, SESSION).await.unwrap();
        vault.close().await;
        issued
    };

    // A server restart: same file, nothing in memory.
    let vault = Vault::open_path(&path, TokenTtl::default()).await.unwrap();
    let grant = vault.open(issued.access.expose()).await.unwrap();
    assert_eq!(grant.telegram_session.as_slice(), SESSION);
}

#[tokio::test]
async fn debug_output_never_contains_secrets() {
    let (vault, _dir) = vault(TokenTtl::default()).await;
    let issued = vault.create_grant("client", 42, SESSION).await.unwrap();
    let grant = vault.open(issued.access.expose()).await.unwrap();

    let printed = format!("{issued:?} {grant:?}");
    assert!(!printed.contains(issued.access.expose()));
    assert!(!printed.contains(issued.refresh.expose()));
    assert!(!printed.contains("grammers-session-bytes"));
    assert!(
        printed.contains(issued.grant_id.as_str()),
        "ids stay visible: {printed}"
    );
}

#[tokio::test]
async fn either_token_opens_the_grant_for_its_own_client_only() {
    let (vault, _dir) = vault(TokenTtl::default()).await;
    let issued = vault.create_grant("client", 42, SESSION).await.unwrap();

    for token in [&issued.access, &issued.refresh] {
        let grant = vault
            .open_for_client(token.expose(), "client")
            .await
            .unwrap();
        assert_eq!(grant.grant_id, issued.grant_id);
        assert_eq!(grant.telegram_session.as_slice(), SESSION);
        match vault.open_for_client(token.expose(), "other-client").await {
            Err(VaultError::WrongClient) => {}
            other => panic!("expected WrongClient, got {other:?}"),
        }
    }
    assert_invalid(vault.open_for_client("garbage", "client").await);
    vault.delete_grant(&issued.grant_id).await.unwrap();
    assert_invalid(
        vault
            .open_for_client(issued.refresh.expose(), "client")
            .await,
    );
}

#[tokio::test]
async fn purge_drops_grants_left_without_tokens() {
    let short = TokenTtl {
        access: Duration::from_secs(1),
        refresh: Duration::from_secs(1),
    };
    let (vault, _dir) = vault(short).await;
    let expiring = vault.create_grant("client", 42, SESSION).await.unwrap();
    let opened = vault.open(expiring.access.expose()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let kept = vault.create_grant("client", 7, SESSION).await.unwrap();
    let kept_opened = vault.open(kept.access.expose()).await.unwrap();

    assert_eq!(vault.purge_expired().await.unwrap(), 2);
    // The Grant row is gone too: writing its session finds nothing.
    assert_invalid(vault.save_session(&opened, b"late write").await);
    vault.save_session(&kept_opened, SESSION).await.unwrap();
}

#[tokio::test]
async fn the_sweep_drops_wrapped_keys_only_after_the_grace_window() {
    let (vault, clock, _dir) = paused_vault(TokenTtl::default()).await;
    let first = vault.create_grant("client", 42, SESSION).await.unwrap();
    vault
        .refresh(first.refresh.expose(), "client")
        .await
        .unwrap();

    assert_eq!(vault.drop_consumed_keys().await.unwrap(), 0);
    let retried = vault
        .refresh(first.refresh.expose(), "client")
        .await
        .unwrap();

    clock.advance(GRACE);
    assert_eq!(vault.drop_consumed_keys().await.unwrap(), 1);
    assert_eq!(vault.drop_consumed_keys().await.unwrap(), 0);
    // A consumed record without its key still recognises a reuse.
    assert_revoked(
        vault.refresh(first.refresh.expose(), "client").await,
        &first.grant_id,
        RevokeReason::RefreshReuse,
    );
    assert_invalid(vault.open(retried.access.expose()).await);
}

#[tokio::test]
async fn purge_drops_consumed_tokens_at_their_original_expiry() {
    let ttl = TokenTtl {
        access: Duration::from_secs(60 * 60),
        refresh: Duration::from_secs(2 * 60 * 60),
    };
    let (vault, clock, _dir) = paused_vault(ttl).await;
    let first = vault.create_grant("client", 42, SESSION).await.unwrap();
    let second = vault
        .refresh(first.refresh.expose(), "client")
        .await
        .unwrap();
    clock.advance(Duration::from_secs(30 * 60));
    let third = vault
        .refresh(second.refresh.expose(), "client")
        .await
        .unwrap();

    clock.advance(Duration::from_secs(90 * 60) - Duration::from_secs(1));
    // Two hours less a second in: the three access Tokens (1 h each) expired.
    assert_eq!(vault.purge_expired().await.unwrap(), 3);
    clock.advance(Duration::from_secs(1));
    // Two hours in: both consumed refresh Tokens reach their original expiry.
    assert_eq!(vault.purge_expired().await.unwrap(), 2);

    assert!(vault.grant_exists(&first.grant_id).await.unwrap());
    // Gone, not just expired: presenting it is a plain invalid Token.
    assert_invalid(vault.refresh(first.refresh.expose(), "client").await);
    vault
        .refresh(third.refresh.expose(), "client")
        .await
        .unwrap();
}

#[tokio::test]
async fn consumed_tokens_never_keep_a_grant_alive() {
    let dir = tempfile::tempdir().unwrap();
    let pool = tg_mcp::db::open(&dir.path().join("vault.db"))
        .await
        .unwrap();
    let clock = PausedClock::new();
    // Tokens issued under a long TTL, refreshed after a restart with a shorter one:
    // the consumed refresh Token outlives every live Token of its Grant.
    let long = Vault::new(pool.clone(), TokenTtl::default()).with_clock(clock.clone());
    let short = Vault::new(
        pool,
        TokenTtl {
            access: Duration::from_secs(60),
            refresh: Duration::from_secs(60),
        },
    )
    .with_clock(clock.clone());
    let first = long.create_grant("client", 42, SESSION).await.unwrap();
    short
        .refresh(first.refresh.expose(), "client")
        .await
        .unwrap();

    clock.advance(Duration::from_secs(60 * 60));
    // Both access Tokens and the new refresh Token; the consumed one has days left.
    assert_eq!(short.purge_expired().await.unwrap(), 3);
    assert!(!short.grant_exists(&first.grant_id).await.unwrap());
}
