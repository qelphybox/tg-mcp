//! The `World` seam: everything the harness knows about the outside.
//!
//! A scenario talks to the server under test over HTTP and MCP, and to
//! Telegram only through the Driver. The Fake world puts the real server on a
//! fake Telegram in-process; the Real world (#135) puts it on real Telegram.

use std::{any::Any, sync::Arc, time::Duration};

use async_trait::async_trait;

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

/// The harness's own login to the Test account: it stands in for the phone.
#[async_trait]
pub trait Driver: Send + Sync {
    /// Accepts a QR login, as the Test account's phone does when it scans
    /// `tg://login?token=…` (`auth.acceptLoginToken`).
    async fn accept_login(&self, qr_url: &str) -> anyhow::Result<()>;
}

/// One world a run happens in.
pub struct World {
    pub server: Server,
    pub driver: Arc<dyn Driver>,
    /// The Test account: every Grant's `whoami` must be this id before its
    /// first write, or the run aborts.
    pub test_account: i64,
    /// How often the Login flow polls the login page.
    pub poll_every: Duration,
    /// How long a QR login may take before the run fails.
    pub login_timeout: Duration,
    /// Whatever must live as long as the world (e.g. an in-process server).
    pub(crate) _keep: Vec<Box<dyn Any + Send + Sync>>,
}
