//! The `World` seam: everything the harness knows about the outside.
//!
//! A scenario talks to the server under test over HTTP and MCP, and to
//! Telegram only through the Driver and the Peer. The Fake world puts the real
//! server on a fake Telegram in-process; the Real world puts it on real
//! Telegram, logged into by the Test account and the Peer.

use std::{any::Any, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use tg_mcp::tg::chats::ChatRef;
use zeroize::Zeroizing;

/// Where the server under test answers.
#[derive(Debug, Clone)]
pub struct Server {
    /// Scheme, host and port, without a trailing slash.
    pub base: String,
}

impl Server {
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }
}

/// A Telegram account the harness is logged into: the Test account (through
/// the Driver) or the Peer.
#[async_trait]
pub trait Party: Send + Sync {
    /// The account's Telegram user id.
    async fn account_id(&self) -> Result<i64>;

    /// Accepts a QR login, as the account's phone does when it scans
    /// `tg://login?token=…` (`auth.acceptLoginToken`).
    async fn accept_login(&self, qr_url: &str) -> Result<()>;
}

/// A Draft as Telegram holds it, read by the Driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenDraft {
    pub text: String,
    pub entities: Vec<SeenEntity>,
    /// The message it replies to, if any.
    pub reply_to: Option<i32>,
}

/// What a formatting entity does: the kinds the server's Markdown makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityKind {
    Bold,
    Italic,
    Code,
    Pre,
    TextUrl,
    /// Any other kind Telegram holds.
    Other,
}

/// A formatting entity, offsets in UTF-16 code units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenEntity {
    pub kind: EntityKind,
    pub offset: usize,
    pub length: usize,
    pub url: Option<String>,
}

impl SeenEntity {
    /// The part of `text` this entity covers.
    pub fn covers(&self, text: &str) -> String {
        let units: Vec<u16> = text.encode_utf16().collect();
        let end = (self.offset + self.length).min(units.len());
        String::from_utf16_lossy(&units[self.offset.min(end)..end])
    }
}

/// The harness's own login to the Test account: it stands in for the phone.
#[async_trait]
pub trait Driver: Party {
    /// The Draft in `chat` (a chat id as the server issues it), in `topic`
    /// for a forum, as Telegram holds it on the Test account.
    async fn draft(&self, chat: &str, topic: Option<i32>) -> Result<Option<SeenDraft>>;

    /// Types a Draft into `chat` as another app would (plain text).
    async fn type_draft(&self, chat: &str, text: &str) -> Result<()>;

    /// Deletes message `id` the Test account sent in `chat` (A or a basic
    /// group: private-chat ids), for everyone.
    async fn delete_message(&self, chat: &str, id: i64) -> Result<()>;

    /// How many "Unofficial MCP Server" sessions created since `since` (Unix
    /// seconds) the Test account still has.
    async fn server_sessions_since(&self, since: i64) -> Result<usize>;

    /// The text of the Draft in the Test account's Saved Messages, if any.
    async fn saved_draft(&self) -> Result<Option<String>>;

    /// Replaces the Draft in Saved Messages (`""` clears it).
    async fn set_saved_draft(&self, text: &str) -> Result<()>;

    /// Ends the Test account's "Unofficial MCP Server" sessions created since
    /// `since` (Unix seconds) that a run left behind; returns how many. Telegram
    /// refuses this for 24 hours after the Driver's own login, which is fine.
    async fn end_server_sessions(&self, since: i64) -> Result<usize> {
        let _ = since;
        Ok(0)
    }
}

/// The second account, the other side of the Test account's Chats.
#[async_trait]
pub trait Peer: Party {
    /// Writes `text` to the Test account in their private chat (fixture A).
    async fn say_to_test_account(&self, test_account: i64, text: &str) -> Result<()>;

    /// Whether the Test account's latest messages to the Peer (in A) include
    /// one reading exactly `text`.
    async fn received_from_test_account(&self, test_account: i64, text: &str) -> Result<bool>;
}

/// Who logs in through the Login flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Who {
    /// The Test account, with its Cloud password.
    TestAccount,
    /// The Peer, which has no Cloud password.
    Peer,
}

/// One world a run happens in.
pub struct World {
    /// `fake` or `real`.
    pub name: &'static str,
    /// How the report names the world and its server, e.g. `real world, image …`.
    pub label: String,
    pub server: Server,
    pub driver: Arc<dyn Driver>,
    pub peer: Arc<dyn Peer>,
    /// Every Grant of the Test account must have this `whoami` id before its
    /// first write, or the run aborts.
    pub test_account: i64,
    pub peer_account: i64,
    /// The Test account's Cloud password, typed on the login page when asked.
    pub cloud_password: Option<Zeroizing<String>>,
    /// How often the Login flow polls the login page.
    pub poll_every: Duration,
    /// How long a QR login may take before the run fails.
    pub login_timeout: Duration,
    /// How a run takes and keeps the Test account's lease.
    pub lease: crate::lease::Terms,
    /// The server's logs and database, when the harness started the server.
    pub observed: Option<crate::invariants::Observed>,
    /// The Test account's names (first, last, username), kept out of the database.
    pub account_names: Vec<String>,
    /// Whatever must live as long as the world (e.g. the server under test).
    pub(crate) _keep: Vec<Box<dyn Any + Send + Sync>>,
}

impl World {
    /// The account `who` must be.
    pub fn account_of(&self, who: Who) -> i64 {
        match who {
            Who::TestAccount => self.test_account,
            Who::Peer => self.peer_account,
        }
    }

    pub fn party(&self, who: Who) -> &dyn Party {
        match who {
            Who::TestAccount => self.driver.as_ref(),
            Who::Peer => self.peer.as_ref(),
        }
    }

    /// The interlock on the harness's own logins: the Driver must be the Test
    /// account and the Peer the Peer, before anything is written.
    pub async fn check_parties(&self) -> Result<()> {
        let driver = self.driver.account_id().await?;
        if driver != self.test_account {
            bail!(
                "interlock: the Driver is logged into account {driver}, not the Test account {}; nothing was written",
                self.test_account
            );
        }
        let peer = self.peer.account_id().await?;
        if peer != self.peer_account {
            bail!(
                "interlock: the Peer is logged into account {peer}, not the configured Peer {}; nothing was written",
                self.peer_account
            );
        }
        if peer == driver {
            bail!("interlock: the Peer and the Test account are the same account {peer}");
        }
        Ok(())
    }
}

/// The token of a `tg://login?token=…` URL, as Telegram's apps read it.
pub fn login_token(qr_url: &str) -> Result<String> {
    // The URL carries a login token: errors never quote it.
    let url = url::Url::parse(qr_url).context("the QR login is not a URL")?;
    ensure!(
        url.scheme() == "tg" && url.host_str() == Some("login"),
        "not a tg://login URL"
    );
    url.query_pairs()
        .find(|(k, _)| k == "token")
        .map(|(_, v)| v.into_owned())
        .filter(|t| !t.is_empty())
        .context("tg://login URL without a token")
}

/// A chat id as the server issues it (`user:N~hash`), parsed.
pub fn chat_ref(chat: &str) -> Result<ChatRef> {
    use std::str::FromStr;
    ChatRef::from_str(chat).map_err(|e| anyhow::anyhow!("{chat}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_token_of_a_login_url() {
        assert_eq!(login_token("tg://login?token=AbC-_9").unwrap(), "AbC-_9");
        assert!(login_token("tg://resolve?domain=x").is_err());
        assert!(login_token("tg://login?token=").is_err());
        let err = login_token("nonsense secret-token").unwrap_err();
        assert!(!format!("{err:#}").contains("secret-token"), "{err:#}");
    }
}
