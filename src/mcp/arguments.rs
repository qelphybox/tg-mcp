//! Every tool takes only the parameters its input schema lists (#60). A
//! misspelt one would otherwise be dropped silently and the tool answer
//! something else than asked (`archive` for `archived`: the main list, not the
//! Archive). The schema says so (`additionalProperties: false`), and a call
//! naming another key is refused before the tool runs, with the closest
//! known name.

use std::sync::Arc;

use rmcp::model::{JsonObject, Tool};

/// Close `tool`'s input schema to parameters it doesn't list.
pub fn forbid_others(tool: &mut Tool) {
    Arc::make_mut(&mut tool.input_schema).insert("additionalProperties".into(), false.into());
}

/// The parameters an input schema lists, in its order.
pub fn parameters(schema: &JsonObject) -> impl Iterator<Item = &str> {
    schema
        .get("properties")
        .and_then(|p| p.as_object())
        .into_iter()
        .flat_map(|p| p.keys().map(String::as_str))
}

/// Err with what to tell the caller when `arguments` has a key `tool`
/// doesn't take: the first such key in name order.
pub fn check(tool: &Tool, arguments: Option<&JsonObject>) -> Result<(), String> {
    let known: Vec<&str> = parameters(&tool.input_schema).collect();
    match arguments
        .into_iter()
        .flat_map(|a| a.keys())
        .find(|key| !known.contains(&key.as_str()))
    {
        Some(unknown) => Err(refusal_message(unknown, &known)),
        None => Ok(()),
    }
}

/// Longest unknown parameter name a refusal repeats.
const MAX_SHOWN_CHARS: usize = 40;

/// What a call with parameter `unknown` is told, given the `known` ones.
fn refusal_message(unknown: &str, known: &[&str]) -> String {
    let mut shown: String = unknown.chars().take(MAX_SHOWN_CHARS).collect();
    if shown.len() < unknown.len() {
        shown.push('…');
    }
    match closest(unknown, known) {
        Some(meant) => format!("unknown parameter `{shown}`; did you mean `{meant}`?"),
        None if known.is_empty() => {
            format!("unknown parameter `{shown}`; this tool takes no parameters")
        }
        None => format!(
            "unknown parameter `{shown}`; this tool takes: {}",
            known.join(", ")
        ),
    }
}

/// The known parameter `unknown` most likely means: the same letters and
/// digits whatever the case and `_` (`pinnedOnly`, `PINNED_ONLY`), or, for
/// names of 3 letters or more, a third of them edited at most (`archive`,
/// `chat` for `chat_id`).
fn closest<'a>(unknown: &str, known: &[&'a str]) -> Option<&'a str> {
    let letters = |s: &str| -> Vec<char> {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    };
    let unknown = letters(unknown);
    known
        .iter()
        .filter_map(|&name| {
            let name_letters = letters(name);
            let shorter = unknown.len().min(name_letters.len());
            let allowed = match shorter {
                0..3 => 0,
                _ => (unknown.len().max(name_letters.len()) / 3).max(1),
            };
            let distance = edit_distance(&unknown, &name_letters);
            (shorter > 0 && distance <= allowed).then_some((distance, name))
        })
        .min()
        .map(|(_, name)| name)
}

/// Levenshtein distance.
fn edit_distance(a: &[char], b: &[char]) -> usize {
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substituted = diagonal + usize::from(ca != cb);
            diagonal = row[j + 1];
            row[j + 1] = substituted.min(row[j] + 1).min(row[j + 1] + 1);
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_suggestion_needs_the_same_letters_or_a_few_edits() {
        let known = ["chat_id", "chat_type", "query", "limit", "q"];
        assert_eq!(closest("ChatID", &known), Some("chat_id"));
        assert_eq!(closest("chat", &known), Some("chat_id"));
        assert_eq!(closest("limt", &known), Some("limit"));
        assert_eq!(closest("Q", &known), Some("q"));
        // Too short, or too far, to guess.
        assert_eq!(closest("x", &known), None);
        assert_eq!(closest("_", &known), None);
        assert_eq!(closest("ab", &known), None);
        assert_eq!(closest("foo", &known), None);
        assert_eq!(closest("message", &known), None);
    }

    #[test]
    fn edit_distance_counts_insertions_deletions_and_substitutions() {
        let d = |a: &str, b: &str| {
            edit_distance(
                &a.chars().collect::<Vec<_>>(),
                &b.chars().collect::<Vec<_>>(),
            )
        };
        assert_eq!(d("", ""), 0);
        assert_eq!(d("kitten", "sitting"), 3);
        assert_eq!(d("archive", "archived"), 1);
        assert_eq!(d("flaw", "lawn"), 2);
        assert_eq!(d("ä", "a"), 1);
    }

    #[test]
    fn a_long_unknown_name_is_shortened_on_a_character_boundary() {
        let message = refusal_message(&"я".repeat(100), &["limit"]);
        assert!(message.starts_with(&format!("unknown parameter `{}…`", "я".repeat(40))));
    }
}
