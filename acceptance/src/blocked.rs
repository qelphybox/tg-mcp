//! "Blocked": Telegram limited the Test account (or the Peer), so the run
//! stops. Never a bug in the code, never retried around (ADR-0009).

use std::fmt;

/// Why a run can't go on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    pub reason: String,
}

impl fmt::Display for Blocked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "blocked: {}", self.reason)
    }
}

impl std::error::Error for Blocked {}

impl Blocked {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// A Telegram RPC error (as grammers splits it: name without digits, and the
/// number) that blocks a run, said in words.
pub fn rpc(code: i32, name: &str, value: Option<u32>) -> Option<Blocked> {
    let reason = match name {
        "FLOOD_WAIT" | "FLOOD_PREMIUM_WAIT" => format!(
            "Telegram asks to wait {} s ({name}); run again after that",
            value.unwrap_or(0)
        ),
        "PEER_FLOOD" => {
            "Telegram limits the account for spam (PEER_FLOOD): see @SpamBot from that account"
                .into()
        }
        "FROZEN_METHOD_INVALID" | "FROZEN_PARTICIPANT_MISSING" => format!(
            "Telegram froze the account ({name}): it is read-only until the appeal is resolved"
        ),
        "USER_RESTRICTED" => "Telegram restricted the account (USER_RESTRICTED)".into(),
        _ if code == 401 => format!(
            "the harness's own login is no longer valid ({name}): log in again with `cargo accept login driver|peer`"
        ),
        _ => return None,
    };
    Some(Blocked { reason })
}

/// A message of our server (a tool error, a login page) saying Telegram
/// limited the account.
pub fn server_message(message: &str) -> Option<Blocked> {
    let blocked = message.contains("(FLOOD_WAIT)")
        || message.contains("PEER_FLOOD")
        || message.contains("FROZEN_")
        || message.starts_with("Telegram limits ");
    blocked.then(|| Blocked::new(format!("the server says: {}", one_line(message))))
}

/// The [`Blocked`] in an error's chain, if any.
pub fn find(err: &anyhow::Error) -> Option<&Blocked> {
    err.chain().find_map(|e| e.downcast_ref::<Blocked>())
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telegram_limits_block() {
        let flood = rpc(420, "FLOOD_WAIT", Some(31)).unwrap();
        assert!(flood.reason.contains("31 s"), "{flood}");
        assert!(rpc(420, "FLOOD_PREMIUM_WAIT", Some(5)).is_some());
        assert!(rpc(400, "PEER_FLOOD", None).is_some());
        assert!(rpc(420, "FROZEN_METHOD_INVALID", None).is_some());
        assert!(rpc(400, "FROZEN_PARTICIPANT_MISSING", None).is_some());
        assert!(rpc(400, "USER_RESTRICTED", None).is_some());
        assert!(rpc(401, "AUTH_KEY_UNREGISTERED", None).is_some());
    }

    #[test]
    fn other_errors_are_failures() {
        assert_eq!(rpc(400, "MESSAGE_TOO_LONG", None), None);
        assert_eq!(rpc(400, "PEER_ID_INVALID", None), None);
        assert_eq!(rpc(420, "SLOWMODE_WAIT", Some(10)), None);
        assert_eq!(rpc(500, "INTERNAL", None), None);
    }

    #[test]
    fn server_messages_about_limits_block() {
        assert!(server_message(
            "Telegram asks this account to wait 30 s before writing again (FLOOD_WAIT). Nothing was written."
        )
        .is_some());
        assert!(server_message("Telegram refused this write with PEER_FLOOD: …").is_some());
        assert!(server_message("Telegram limits logins. Try again in 30 seconds.").is_some());
        assert!(
            server_message("Telegram limits password attempts. Try again in 2 minutes.").is_some()
        );
        assert_eq!(server_message("unknown parameter `foo`"), None);
        assert_eq!(server_message("save_draft needs the `draft` Scope"), None);
    }

    #[test]
    fn found_anywhere_in_an_error_chain() {
        let err = anyhow::Error::new(Blocked::new("x"))
            .context("login")
            .context("s53");
        assert_eq!(find(&err).unwrap().reason, "x");
        assert!(find(&anyhow::anyhow!("plain")).is_none());
    }
}
