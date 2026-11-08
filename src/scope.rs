//! Scopes: the kinds of access a Grant has (ADR-0005). Shared by the OAuth
//! layer (requested at `/authorize`, chosen on Consent, returned by `/token`),
//! the vault (stored with the Grant) and the MCP layer (which tools a caller
//! sees and may call).

use std::fmt;

/// One kind of access. `read` is always granted; the others are chosen by the
/// user on the Consent screen, and changing them means a new Grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// Read chats, messages and media.
    Read,
    /// Write Drafts: text left in a Chat's input field; only the user sends it.
    Draft,
    /// Send messages, within the Grant's Send allow-list.
    Send,
    /// Organize the Account's chats.
    Organize,
}

impl Scope {
    /// Every Scope, in canonical order: the order of `scopes_supported` and of a
    /// granted `scope` string.
    pub const ALL: [Scope; 4] = [Scope::Read, Scope::Draft, Scope::Send, Scope::Organize];

    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Read => "read",
            Scope::Draft => "draft",
            Scope::Send => "send",
            Scope::Organize => "organize",
        }
    }

    /// The Scope named exactly `name` (OAuth scope tokens are case-sensitive).
    pub fn parse(name: &str) -> Option<Scope> {
        Scope::ALL.into_iter().find(|scope| scope.as_str() == name)
    }

    fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A set of Scopes that always contains [`Scope::Read`]. Displays as an OAuth
/// `scope` value: space-separated, in canonical order (`read draft`).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Scopes(u8);

/// Longest `scope` value `/authorize` accepts: every Scope once, with room for
/// repeats, never a reason to parse kilobytes.
pub const MAX_SCOPE_PARAM_LEN: usize = 256;

/// A requested `scope` value names a Scope this server doesn't have, or is too long.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown scope")]
pub struct UnknownScope;

impl Scopes {
    /// `read` alone: what every Grant has, and all a Grant from before Scopes has.
    pub const READ_ONLY: Scopes = Scopes(1);

    /// These Scopes plus `scope`.
    pub fn with(self, scope: Scope) -> Scopes {
        Scopes(self.0 | scope.bit())
    }

    pub fn contains(self, scope: Scope) -> bool {
        self.0 & scope.bit() != 0
    }

    /// In canonical order, `read` first.
    pub fn iter(self) -> impl Iterator<Item = Scope> {
        Scope::ALL.into_iter().filter(move |s| self.contains(*s))
    }

    /// Whether any Scope beyond `read` is in the set.
    pub fn writes(self) -> bool {
        self != Scopes::READ_ONLY
    }

    /// An OAuth `scope` request value (space-separated, RFC 6749 §3.3). Missing or
    /// empty means `read`; `read` is added either way. An unknown name fails the
    /// whole request.
    pub fn parse_request(value: Option<&str>) -> Result<Scopes, UnknownScope> {
        let value = value.unwrap_or_default();
        if value.len() > MAX_SCOPE_PARAM_LEN {
            return Err(UnknownScope);
        }
        value.split(' ').filter(|name| !name.is_empty()).try_fold(
            Scopes::READ_ONLY,
            |scopes, name| {
                Scope::parse(name)
                    .map(|s| scopes.with(s))
                    .ok_or(UnknownScope)
            },
        )
    }

    /// The Scopes stored with a Grant. A name this build doesn't know (written by
    /// a newer one) is left out: that only ever takes access away.
    pub fn from_stored(value: &str) -> Scopes {
        value
            .split(' ')
            .filter_map(Scope::parse)
            .fold(Scopes::READ_ONLY, Scopes::with)
    }
}

impl Default for Scopes {
    fn default() -> Self {
        Scopes::READ_ONLY
    }
}

impl FromIterator<Scope> for Scopes {
    fn from_iter<I: IntoIterator<Item = Scope>>(iter: I) -> Self {
        iter.into_iter().fold(Scopes::READ_ONLY, Scopes::with)
    }
}

impl fmt::Display for Scopes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, scope) in self.iter().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            f.write_str(scope.as_str())?;
        }
        Ok(())
    }
}

impl fmt::Debug for Scopes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Scopes({self})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_is_always_there_and_first() {
        assert_eq!(Scopes::READ_ONLY.to_string(), "read");
        assert_eq!(Scopes::default(), Scopes::READ_ONLY);
        let scopes = Scopes::READ_ONLY.with(Scope::Organize).with(Scope::Draft);
        assert_eq!(scopes.to_string(), "read draft organize");
        assert!(scopes.contains(Scope::Read));
        assert!(!scopes.contains(Scope::Send));
        assert!(scopes.writes());
        assert!(!Scopes::READ_ONLY.writes());
    }

    #[test]
    fn requests_parse_into_canonical_sets() {
        for (value, expected) in [
            (None, "read"),
            (Some(""), "read"),
            (Some("read"), "read"),
            (Some("draft"), "read draft"),
            (Some("draft read  draft"), "read draft"),
            (Some("organize send draft read"), "read draft send organize"),
        ] {
            assert_eq!(
                Scopes::parse_request(value).unwrap().to_string(),
                expected,
                "{value:?}"
            );
        }
    }

    #[test]
    fn unknown_or_oversized_requests_fail_whole() {
        for value in [
            "write",
            "read write",
            "READ",
            "Draft",
            "read\tdraft",
            "read,draft",
        ] {
            assert_eq!(
                Scopes::parse_request(Some(value)),
                Err(UnknownScope),
                "{value:?}"
            );
        }
        let long = "read ".repeat(MAX_SCOPE_PARAM_LEN / 5 + 1);
        assert_eq!(Scopes::parse_request(Some(&long)), Err(UnknownScope));
    }

    #[test]
    fn stored_values_round_trip_and_unknown_names_take_nothing() {
        for scopes in [
            Scopes::READ_ONLY,
            Scope::ALL.into_iter().collect(),
            [Scope::Draft].into_iter().collect(),
        ] {
            assert_eq!(Scopes::from_stored(&scopes.to_string()), scopes);
        }
        assert_eq!(
            Scopes::from_stored("read draft teleport"),
            [Scope::Draft].into_iter().collect()
        );
        assert_eq!(Scopes::from_stored(""), Scopes::READ_ONLY);
    }
}
