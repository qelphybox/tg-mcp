//! The `World` seam: everything the harness knows about the outside.
//!
//! A scenario talks to the server under test over HTTP and MCP, and to
//! Telegram only through the Driver and the Peer. The Fake world puts the real
//! server on a fake Telegram in-process; the Real world puts it on real
//! Telegram, logged into by the Test account and the Peer.

use std::{any::Any, sync::Arc, time::Duration};

use anyhow::{Result, bail};
use async_trait::async_trait;
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

/// The harness's own login to the Test account: it stands in for the phone.
#[async_trait]
pub trait Driver: Party {
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
pub trait Peer: Party {}

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
    /// `fake` or `real`, for the report.
    pub name: &'static str,
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
