//! Every scenario, by id. Ids match the acceptance sections they replace.

use crate::run::Scenario;

mod login;
mod login_peer;
mod s53;
mod smoke;

/// Every scenario, in the order a full run takes them.
pub static ALL: &[Scenario] = &[
    login::SCENARIO,
    login_peer::SCENARIO,
    s53::SCENARIO,
    smoke::SCENARIO,
];

/// The scenario with this id.
pub fn find(id: &str) -> Option<&'static Scenario> {
    ALL.iter().find(|s| s.id == id)
}
