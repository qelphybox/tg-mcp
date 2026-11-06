//! Session vault behaviour through its public API, on a real SQLite file.

use std::time::Duration;

use tempfile::TempDir;
use tg_mcp::sessions::{TokenTtl, Vault, VaultError};

const SESSION: &[u8] = b"grammers-session-bytes: auth key, dc, peers";

async fn vault(ttl: TokenTtl) -> (Vault, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let vault = Vault::open_path(&dir.path().join("vault.db"), ttl)
        .await
        .unwrap();
    (vault, dir)
}

fn assert_invalid<T: std::fmt::Debug>(result: Result<T, VaultError>) {
    match result {
        Err(VaultError::InvalidToken) => {}
        other => panic!("expected InvalidToken, got {other:?}"),
    }
}

#[tokio::test]
async fn access_token_opens_the_grant() {
    let (vault, _dir) = vault(TokenTtl::default()).await;
    let issued = vault.create_grant(42, SESSION).await.unwrap();

    let grant = vault.open(issued.access.expose()).await.unwrap();
    assert_eq!(grant.grant_id, issued.grant_id);
    assert_eq!(grant.account_id, 42);
    assert_eq!(grant.telegram_session.as_slice(), SESSION);
}

#[tokio::test]
async fn bad_tokens_are_all_the_same_invalid_token() {
    let (vault, _dir) = vault(TokenTtl::default()).await;
    let issued = vault.create_grant(42, SESSION).await.unwrap();
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
    assert_invalid(vault.refresh(access).await);
}

#[tokio::test]
async fn expired_tokens_are_rejected() {
    let ttl = TokenTtl {
        access: Duration::ZERO,
        refresh: Duration::ZERO,
    };
    let (vault, _dir) = vault(ttl).await;
    let issued = vault.create_grant(42, SESSION).await.unwrap();

    assert_invalid(vault.open(issued.access.expose()).await);
    assert_invalid(vault.refresh(issued.refresh.expose()).await);
    assert_eq!(vault.purge_expired().await.unwrap(), 2);
}

#[tokio::test]
async fn refresh_rotates_and_the_old_refresh_token_dies() {
    let (vault, _dir) = vault(TokenTtl::default()).await;
    let first = vault.create_grant(42, SESSION).await.unwrap();

    let second = vault.refresh(first.refresh.expose()).await.unwrap();
    assert_eq!(second.grant_id, first.grant_id);
    assert_ne!(second.access.expose(), first.access.expose());
    assert_ne!(second.refresh.expose(), first.refresh.expose());

    let grant = vault.open(second.access.expose()).await.unwrap();
    assert_eq!(grant.telegram_session.as_slice(), SESSION);

    assert_invalid(vault.refresh(first.refresh.expose()).await);
    // The new refresh Token keeps the chain going.
    vault.refresh(second.refresh.expose()).await.unwrap();
}

#[tokio::test]
async fn concurrent_refresh_with_one_token_has_one_winner() {
    let (vault, _dir) = vault(TokenTtl::default()).await;
    let issued = vault.create_grant(42, SESSION).await.unwrap();
    let token = issued.refresh.expose();

    let (a, b) = tokio::join!(vault.refresh(token), vault.refresh(token));
    assert_eq!(
        [a.is_ok(), b.is_ok()].iter().filter(|ok| **ok).count(),
        1,
        "exactly one refresh may succeed: {a:?} / {b:?}"
    );
}

#[tokio::test]
async fn updated_session_is_visible_through_every_live_token() {
    let (vault, _dir) = vault(TokenTtl::default()).await;
    let first = vault.create_grant(42, SESSION).await.unwrap();
    let second = vault.refresh(first.refresh.expose()).await.unwrap();

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
    let issued = vault.create_grant(42, SESSION).await.unwrap();
    let other = vault.create_grant(7, b"someone else").await.unwrap();
    let grant = vault.open(issued.access.expose()).await.unwrap();

    vault.delete_grant(&issued.grant_id).await.unwrap();

    assert_invalid(vault.open(issued.access.expose()).await);
    assert_invalid(vault.refresh(issued.refresh.expose()).await);
    assert_invalid(vault.save_session(&grant, b"late write").await);
    vault.open(other.access.expose()).await.unwrap();
}

#[tokio::test]
async fn grants_survive_reopening_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vault.db");
    let issued = {
        let vault = Vault::open_path(&path, TokenTtl::default()).await.unwrap();
        let issued = vault.create_grant(42, SESSION).await.unwrap();
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
    let issued = vault.create_grant(42, SESSION).await.unwrap();
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
