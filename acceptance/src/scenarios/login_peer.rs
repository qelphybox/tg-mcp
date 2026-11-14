//! `login-peer`: the Login flow for an account without a Cloud password (the
//! Peer), the other path of acceptance §1/§2.

use anyhow::{Result, ensure};

use crate::{
    run::{Run, Scenario},
    world::Who,
};

pub const SCENARIO: Scenario = Scenario {
    id: "login-peer",
    title: "An account without a Cloud password logs in straight to Consent",
    run: |run| Box::pin(login_peer(run)),
};

async fn login_peer(run: &mut Run) -> Result<()> {
    let session = run.login_as(Who::Peer, &[]).await?;
    ensure!(
        session.grant.scope == "read",
        "a read-only Consent granted `{}`",
        session.grant.scope
    );
    run.passed("no Cloud password step for an account without one");
    Ok(())
}
