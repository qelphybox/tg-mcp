//! "Never log message content, Tokens, McpSession keys or Telegram sessions",
//! checked after every local and CI run (ADR-0009): nothing the run wrote may
//! appear in the server's logs or database, no `tgm_` value in its logs, no
//! Test account name in its database. A hit is never printed, only located.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

/// Where the server's log is.
#[derive(Clone)]
pub enum LogSource {
    /// A file the server process writes (Real world).
    File(PathBuf),
    /// The in-process server's log (Fake world).
    Memory(Arc<Mutex<Vec<u8>>>),
}

impl LogSource {
    pub fn read(&self) -> std::io::Result<Vec<u8>> {
        match self {
            LogSource::File(path) => std::fs::read(path),
            LogSource::Memory(buf) => Ok(buf.lock().unwrap().clone()),
        }
    }

    /// Adds bytes to an in-memory log (for the harness's own tests).
    pub fn append(&self, bytes: &[u8]) {
        if let LogSource::Memory(buf) = self {
            buf.lock().unwrap().extend_from_slice(bytes);
        }
    }
}

/// What the harness can see of the server it started.
#[derive(Clone)]
pub struct Observed {
    pub log: LogSource,
    /// The SQLite database; its `-wal` and `-shm` files are read too.
    pub db: PathBuf,
}

impl Observed {
    /// The database's bytes, with its write-ahead log.
    pub fn db_bytes(&self) -> std::io::Result<Vec<u8>> {
        let mut bytes = std::fs::read(&self.db)?;
        for suffix in ["-wal", "-shm"] {
            let mut path = self.db.clone().into_os_string();
            path.push(suffix);
            if let Ok(more) = std::fs::read(PathBuf::from(path)) {
                bytes.extend_from_slice(&more);
            }
        }
        Ok(bytes)
    }
}

/// One thing that must not appear, and how the report names it (never by
/// its content).
#[derive(Debug, Clone)]
pub struct Needle {
    pub label: String,
    pub bytes: Vec<u8>,
}

/// Bodies shorter than this are too common to look for on their own; the
/// run's marker is always looked for.
const MIN_BODY: usize = 16;

/// What a run wrote, as needles: each marker, and each text's body when it is
/// long enough to be telling.
pub fn written(run_marker: &str, texts: &[String]) -> Vec<Needle> {
    let mut needles = vec![Needle {
        label: "the run's marker".into(),
        bytes: run_marker.as_bytes().to_vec(),
    }];
    for (i, text) in texts.iter().enumerate() {
        let body = text.replace(run_marker, "");
        let body = body.trim();
        if body.chars().count() >= MIN_BODY {
            needles.push(Needle {
                label: format!("text #{} the run wrote", i + 1),
                bytes: body.as_bytes().to_vec(),
            });
        }
    }
    needles
}

/// Account names to keep out of the database (too short ones are skipped).
pub fn names(names: &[String]) -> Vec<Needle> {
    names
        .iter()
        .filter(|n| n.chars().count() >= 4)
        .enumerate()
        .map(|(i, n)| Needle {
            label: format!("the Test account's name #{}", i + 1),
            bytes: n.as_bytes().to_vec(),
        })
        .collect()
}

/// Where each needle occurs in `haystack`: (label, byte offset of the first hit).
pub fn find(haystack: &[u8], needles: &[Needle]) -> Vec<(String, usize)> {
    needles
        .iter()
        .filter(|n| !n.bytes.is_empty())
        .filter_map(|n| {
            haystack
                .windows(n.bytes.len())
                .position(|w| w == n.bytes.as_slice())
                .map(|at| (n.label.clone(), at))
        })
        .collect()
}

/// A log without its ANSI colour codes, which can split a field from its value.
pub fn strip_ansi(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'[') {
            i += 2;
            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                i += 1;
            }
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// The line number (from 1) of byte `at`.
pub fn line_of(haystack: &[u8], at: usize) -> usize {
    haystack[..at.min(haystack.len())]
        .iter()
        .filter(|b| **b == b'\n')
        .count()
        + 1
}

/// The result of one invariant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    Pass,
    Fail(String),
    Skipped(String),
    /// Telegram limited the account while the run held its lease.
    Blocked(String),
}

/// Every invariant over what the harness saw of the server, or each one
/// skipped when it saw nothing (a `--target` run).
pub fn check_all(
    observed: Option<&Observed>,
    marker: &str,
    texts: &[String],
    account_names: &[String],
) -> Vec<(String, Check)> {
    const LOG_TEXTS: &str = "server logs: nothing the run wrote";
    const LOG_TOKENS: &str = "server logs: no `tgm_` value";
    const DB_TEXTS: &str = "database: nothing the run wrote";
    const DB_NAMES: &str = "database: no Test account name";
    let Some(observed) = observed else {
        let why = "the server runs elsewhere (--target): no logs or database to read";
        return [LOG_TEXTS, LOG_TOKENS, DB_TEXTS, DB_NAMES]
            .into_iter()
            .map(|c| (c.to_string(), Check::Skipped(why.into())))
            .collect();
    };
    let written = written(marker, texts);
    let mut checks = Vec::new();
    match observed.log.read() {
        Ok(log) => {
            let log = strip_ansi(&log);
            checks.push((LOG_TEXTS.into(), verdict(&log, &written, "log line")));
            let token = [Needle {
                label: "a `tgm_` value".into(),
                bytes: b"tgm_".to_vec(),
            }];
            checks.push((LOG_TOKENS.into(), verdict(&log, &token, "log line")));
        }
        Err(e) => {
            let fail = Check::Fail(format!("the log couldn't be read: {e}"));
            checks.push((LOG_TEXTS.into(), fail.clone()));
            checks.push((LOG_TOKENS.into(), fail));
        }
    }
    match observed.db_bytes() {
        Ok(db) => {
            checks.push((DB_TEXTS.into(), verdict(&db, &written, "byte")));
            checks.push((DB_NAMES.into(), verdict(&db, &names(account_names), "byte")));
        }
        Err(e) => {
            let fail = Check::Fail(format!("the database couldn't be read: {e}"));
            checks.push((DB_TEXTS.into(), fail.clone()));
            checks.push((DB_NAMES.into(), fail));
        }
    }
    checks
}

fn verdict(haystack: &[u8], needles: &[Needle], unit: &str) -> Check {
    let hits = find(haystack, needles);
    if hits.is_empty() {
        return Check::Pass;
    }
    Check::Fail(
        hits.iter()
            .map(|(label, at)| {
                let place = if unit == "log line" {
                    format!("log line {}", line_of(haystack, *at))
                } else {
                    format!("byte {at}")
                };
                format!("{label} found at {place}")
            })
            .collect::<Vec<_>>()
            .join("; "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARKER: &str = "[tgmcp-accept abcdefghij]";

    #[test]
    fn finds_a_marker_in_plain_and_coloured_logs() {
        let needles = written(MARKER, &[format!("hi {MARKER}")]);
        let plain = format!("INFO draft saved\nDEBUG text=hi {MARKER}\n");
        let hits = find(plain.as_bytes(), &needles);
        assert_eq!(hits.len(), 1);
        assert_eq!(line_of(plain.as_bytes(), hits[0].1), 2);
        let coloured = "\x1b[2mtext\x1b[0m=hi [tgmcp-accept \x1b[1mabcdefghij]".as_bytes();
        assert!(find(coloured, &needles).is_empty(), "split by colour codes");
        assert_eq!(find(&strip_ansi(coloured), &needles).len(), 1);
    }

    #[test]
    fn finds_a_long_body_without_its_marker() {
        let text = format!("😀 **Release** _notes_: run cargo test {MARKER}");
        let needles = written(MARKER, &[text]);
        let leaked = "chars=40 text=😀 **Release** _notes_: run cargo test".as_bytes();
        assert_eq!(find(leaked, &needles).len(), 1, "the body alone is a leak");
    }

    #[test]
    fn short_bodies_and_the_prefix_alone_are_no_hits() {
        let needles = written(MARKER, &[format!("x {MARKER}")]);
        assert_eq!(needles.len(), 1, "\"x\" is too short to look for");
        let log = b"run [tgmcp-accept otherrun12] and [tgmcp-accept ";
        assert!(find(log, &needles).is_empty());
    }

    #[test]
    fn finds_inside_raw_database_pages() {
        let mut page = vec![0u8; 4096];
        page[1000..1000 + MARKER.len()].copy_from_slice(MARKER.as_bytes());
        let mut db = vec![0xffu8; 8192];
        db.extend(page);
        let hits = find(&db, &written(MARKER, &[]));
        assert_eq!(hits, vec![("the run's marker".to_string(), 8192 + 1000)]);
    }

    #[test]
    fn names_skip_what_is_too_short() {
        let needles = names(&["Al".into(), "tgmcp_test".into()]);
        assert_eq!(needles.len(), 1);
    }

    #[test]
    fn nothing_to_read_is_skipped_not_passed() {
        let checks = check_all(None, MARKER, &[], &[]);
        assert_eq!(checks.len(), 4);
        assert!(checks.iter().all(|(_, c)| matches!(c, Check::Skipped(_))));
    }
}
