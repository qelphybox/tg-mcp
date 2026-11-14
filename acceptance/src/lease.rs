//! One run at a time on the Test account (ADR-0009).
//!
//! Local runs, CI and parallel agent sessions share the Test account; two runs
//! at once would overwrite each other's Drafts, reactions and read state. A run
//! takes a lease first: a Draft in the Test account's Saved Messages,
//! `tgmcp-accept lease <run-id> until <unix seconds>`, renewed while it runs
//! and cleared at its end.

use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, bail};
use tokio::task::JoinHandle;

use crate::world::Driver;

const PREFIX: &str = "tgmcp-accept lease ";

/// A lease as read from Saved Messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub run_id: String,
    /// Unix seconds.
    pub until: i64,
}

impl Lease {
    pub fn text(&self) -> String {
        format!("{PREFIX}{} until {}", self.run_id, self.until)
    }

    /// The lease in a Saved Messages Draft, if the Draft is one.
    pub fn parse(draft: &str) -> Option<Lease> {
        let rest = draft.trim().strip_prefix(PREFIX)?;
        let (run_id, until) = rest.split_once(" until ")?;
        let run_id = run_id.trim();
        if run_id.is_empty() || run_id.contains(char::is_whitespace) {
            return None;
        }
        Some(Lease {
            run_id: run_id.to_string(),
            until: until.trim().parse().ok()?,
        })
    }

    pub fn is_live(&self, now: i64) -> bool {
        self.until > now
    }
}

/// How a run takes and keeps its lease.
#[derive(Debug, Clone, Copy)]
pub struct Terms {
    /// How long a lease lasts without renewal.
    pub ttl: Duration,
    /// How long to wait for another run's lease before giving up.
    pub wait: Duration,
    /// How often to look again while waiting.
    pub poll: Duration,
}

impl Terms {
    /// 30 minutes, renewed every 10; wait up to 10 minutes for another run.
    pub const REAL: Terms = Terms {
        ttl: Duration::from_secs(30 * 60),
        wait: Duration::from_secs(10 * 60),
        poll: Duration::from_secs(15),
    };

    /// When to renew: a third of the way through.
    pub fn renew_every(&self) -> Duration {
        self.ttl / 3
    }
}

/// What taking the lease found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Saved Messages has no live lease of another run: take it.
    Take,
    /// Another run holds a live lease.
    Held(Lease),
}

/// Whether a run `run_id` may take the lease, given Saved Messages' Draft.
pub fn decide(draft: Option<&str>, run_id: &str, now: i64) -> Decision {
    match draft.and_then(Lease::parse) {
        Some(lease) if lease.run_id != run_id && lease.is_live(now) => Decision::Held(lease),
        _ => Decision::Take,
    }
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A taken lease: renewed in the background until released.
pub struct Held {
    driver: Arc<dyn Driver>,
    run_id: String,
    renewer: JoinHandle<()>,
}

/// Takes the lease for `run_id`, waiting for another run's to end or expire.
pub async fn take(driver: Arc<dyn Driver>, run_id: &str, terms: Terms) -> Result<Held> {
    let started = tokio::time::Instant::now();
    loop {
        let draft = driver.saved_draft().await?;
        match decide(draft.as_deref(), run_id, unix_now()) {
            Decision::Take => {
                write(driver.as_ref(), run_id, terms).await?;
                // Two runs may have written at once: the last write wins.
                let now_there = driver.saved_draft().await?;
                if let Decision::Held(other) = decide(now_there.as_deref(), run_id, unix_now()) {
                    wait_or_give_up(started, terms, &other).await?;
                    continue;
                }
                break;
            }
            Decision::Held(other) => wait_or_give_up(started, terms, &other).await?,
        }
    }
    let renewer = {
        let driver = driver.clone();
        let run_id = run_id.to_string();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(terms.renew_every()).await;
                let _ = write(driver.as_ref(), &run_id, terms).await;
            }
        })
    };
    Ok(Held {
        driver,
        run_id: run_id.to_string(),
        renewer,
    })
}

async fn write(driver: &dyn Driver, run_id: &str, terms: Terms) -> Result<()> {
    let lease = Lease {
        run_id: run_id.to_string(),
        until: unix_now() + terms.ttl.as_secs() as i64,
    };
    driver.set_saved_draft(&lease.text()).await
}

async fn wait_or_give_up(started: tokio::time::Instant, terms: Terms, other: &Lease) -> Result<()> {
    if started.elapsed() >= terms.wait {
        bail!(
            "lease held by run {} until {} (Unix time): another run is using the Test account",
            other.run_id,
            other.until
        );
    }
    tokio::time::sleep(terms.poll).await;
    Ok(())
}

impl Held {
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Stops renewing and clears the lease if it is still ours.
    pub async fn release(self) -> Result<()> {
        self.renewer.abort();
        let draft = self.driver.saved_draft().await?;
        if draft
            .as_deref()
            .and_then(Lease::parse)
            .is_some_and(|l| l.run_id == self.run_id)
        {
            self.driver.set_saved_draft("").await?;
        }
        Ok(())
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.renewer.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lease_reads_back_as_written() {
        let lease = Lease {
            run_id: "r1a2".into(),
            until: 1_800_000_000,
        };
        assert_eq!(lease.text(), "tgmcp-accept lease r1a2 until 1800000000");
        assert_eq!(Lease::parse(&lease.text()), Some(lease));
    }

    #[test]
    fn other_drafts_are_no_lease() {
        assert_eq!(Lease::parse("buy milk"), None);
        assert_eq!(Lease::parse("tgmcp-accept lease  until 5"), None);
        assert_eq!(Lease::parse("tgmcp-accept lease a b until 5"), None);
        assert_eq!(Lease::parse("tgmcp-accept lease abc until soon"), None);
    }

    #[test]
    fn a_live_lease_of_another_run_holds() {
        let held = decide(Some("tgmcp-accept lease other until 200"), "mine", 100);
        assert_eq!(
            held,
            Decision::Held(Lease {
                run_id: "other".into(),
                until: 200
            })
        );
    }

    #[test]
    fn an_expired_ours_or_missing_lease_is_taken() {
        assert_eq!(
            decide(Some("tgmcp-accept lease other until 100"), "mine", 100),
            Decision::Take,
            "expired at its until"
        );
        assert_eq!(
            decide(Some("tgmcp-accept lease mine until 900"), "mine", 100),
            Decision::Take
        );
        assert_eq!(decide(None, "mine", 100), Decision::Take);
        assert_eq!(decide(Some("a note"), "mine", 100), Decision::Take);
    }

    #[test]
    fn renewal_comes_well_before_expiry() {
        assert_eq!(Terms::REAL.renew_every(), Duration::from_secs(600));
        assert!(Terms::REAL.renew_every() < Terms::REAL.ttl);
    }
}
