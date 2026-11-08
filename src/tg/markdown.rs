//! A small Markdown subset turned into Telegram's text and entities (#43),
//! for every tool that writes text: `save_draft` now, `send_message` later.
//!
//! Supported, CommonMark-like:
//!
//! - `**bold**`, `*italic*` and `_italic_`, nested in each other and in links;
//!   `***both***`;
//! - `` `inline code` `` and fenced code blocks (```` ```lang ```` on the first
//!   line is the block's language), both verbatim: nothing inside is markup;
//! - `[text](url)` for `http://`, `https://`, `tg://` and `mailto:` links.
//!
//! Everything else is text, never an error: a marker without its partner
//! (`2 * 3`, an unclosed `` ` ``, `[` without `](url)`) stays as typed. A
//! marker opens only before a non-space and closes only after one, and `_`
//! never opens or closes inside a word (`snake_case` stays), so stray
//! characters rarely turn into formatting. A backslash before any ASCII
//! punctuation character writes that character as is (`\*`, `\_`, `\[`,
//! `\\`; also `\.` and `\!`, which Telegram's own MarkdownV2 asks for).
//! Not supported, so left as typed: `__…__`, `~~…~~`, `||…||`, headings,
//! lists, quotes, images and HTML.
//!
//! Entity offsets and lengths are in UTF-16 code units, as Telegram counts
//! them: an emoji outside the Basic Multilingual Plane takes two.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How a tool reads the `text` it writes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TextFormat {
    /// A Markdown subset: `**bold**`, `*italic*` or `_italic_`, `` `code` ``,
    /// ```` ``` ```` code blocks (optionally ```` ```lang ````) and
    /// `[text](https://…)` links; `\` before a punctuation character writes
    /// it as is. Anything else, and markers without a partner, stay as typed.
    #[default]
    Markdown,
    /// The text exactly as given, without formatting.
    Plain,
}

/// Text as Telegram takes it: the characters, and the entities that format them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormattedText {
    pub text: String,
    /// Sorted by offset, an enclosing entity before the ones inside it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entities: Vec<Entity>,
}

/// One formatted span of a [`FormattedText`], in UTF-16 code units.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entity {
    pub kind: EntityKind,
    pub offset: i32,
    pub length: i32,
}

/// The kinds of entity the Markdown subset makes (`messageEntity*`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EntityKind {
    Bold,
    Italic,
    Code,
    /// A code block; `language` is empty when the fence names none.
    Pre {
        language: String,
    },
    TextUrl {
        url: String,
    },
}

/// The longest language name a code fence may give.
const MAX_LANGUAGE_CHARS: usize = 32;
/// Link schemes the subset turns into links; any other `[x](y)` stays text.
const LINK_SCHEMES: [&str; 4] = ["http://", "https://", "tg://", "mailto:"];

impl FormattedText {
    /// `text` without formatting.
    pub fn plain(text: &str) -> Self {
        Self {
            text: text.to_string(),
            entities: Vec::new(),
        }
    }

    /// `text` read as `format`.
    pub fn new(text: &str, format: TextFormat) -> Self {
        match format {
            TextFormat::Markdown => markdown(text),
            TextFormat::Plain => Self::plain(text),
        }
    }
}

/// The Markdown subset of this module in `text`, as Telegram's text and
/// entities. Never fails: what isn't markup is text.
pub fn markdown(text: &str) -> FormattedText {
    let mut tokens = tokenize(text);
    pair(&mut tokens);
    render(&tokens)
}

// ── Tokens ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    /// `*`: italic.
    Star,
    /// `**`: bold.
    DoubleStar,
    /// `_`: italic.
    Underscore,
}

impl Mark {
    fn raw(self) -> &'static str {
        match self {
            Mark::Star => "*",
            Mark::DoubleStar => "**",
            Mark::Underscore => "_",
        }
    }

    fn kind(self) -> EntityKind {
        match self {
            Mark::DoubleStar => EntityKind::Bold,
            Mark::Star | Mark::Underscore => EntityKind::Italic,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Text(String),
    Code(String),
    Pre {
        language: String,
        body: String,
    },
    Delimiter {
        mark: Mark,
        can_open: bool,
        can_close: bool,
    },
    /// `[`.
    LinkOpen,
    /// `](url)`, with what was typed in case it is no link after all.
    LinkClose {
        url: String,
        raw: String,
    },
}

/// What a token turned out to be once delimiters are paired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Written as typed.
    Literal,
    /// Opens the entity that the token at this index closes.
    Opens,
    /// Closes the entity opened by the token at this index.
    Closes(usize),
}

struct Tokens {
    tokens: Vec<Token>,
    roles: Vec<Role>,
}

fn tokenize(text: &str) -> Tokens {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut buf = String::new();
    let flush = |buf: &mut String, tokens: &mut Vec<Token>| {
        if !buf.is_empty() {
            tokens.push(Token::Text(std::mem::take(buf)));
        }
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' if chars.get(i + 1).is_some_and(char::is_ascii_punctuation) => {
                buf.push(chars[i + 1]);
                i += 2;
            }
            '`' if starts_with(&chars, i, "```") => match code_block(&chars, i) {
                Some((token, next)) => {
                    flush(&mut buf, &mut tokens);
                    tokens.push(token);
                    i = next;
                }
                None => {
                    buf.push_str("```");
                    i += 3;
                }
            },
            '`' => match find(&chars, i + 1, '`').filter(|&end| end > i + 1) {
                Some(end) => {
                    flush(&mut buf, &mut tokens);
                    tokens.push(Token::Code(chars[i + 1..end].iter().collect()));
                    i = end + 1;
                }
                None => {
                    buf.push('`');
                    i += 1;
                }
            },
            '*' | '_' => {
                let run = chars[i..].iter().take_while(|&&r| r == c).count();
                let before = i.checked_sub(1).map(|b| chars[b]);
                let after = chars.get(i + run).copied();
                let marks: &[Mark] = match (c, run) {
                    ('_', 1) => &[Mark::Underscore],
                    ('*', 1) => &[Mark::Star],
                    ('*', 2) => &[Mark::DoubleStar],
                    // `***x***`: bold outside, italic inside, closed inside out.
                    ('*', 3) if closes(before) => &[Mark::Star, Mark::DoubleStar],
                    ('*', 3) => &[Mark::DoubleStar, Mark::Star],
                    _ => &[],
                };
                if marks.is_empty() {
                    buf.extend(std::iter::repeat_n(c, run));
                } else {
                    flush(&mut buf, &mut tokens);
                    let inside_word = |side: Option<char>| side.is_some_and(char::is_alphanumeric);
                    let underscore = c == '_';
                    for &mark in marks {
                        tokens.push(Token::Delimiter {
                            mark,
                            can_open: opens(after) && !(underscore && inside_word(before)),
                            can_close: closes(before) && !(underscore && inside_word(after)),
                        });
                    }
                }
                i += run;
            }
            '[' => {
                flush(&mut buf, &mut tokens);
                tokens.push(Token::LinkOpen);
                i += 1;
            }
            ']' if chars.get(i + 1) == Some(&'(') => match link_target(&chars, i + 2) {
                Some((url, next)) => {
                    flush(&mut buf, &mut tokens);
                    tokens.push(Token::LinkClose {
                        url,
                        raw: chars[i..next].iter().collect(),
                    });
                    i = next;
                }
                None => {
                    buf.push(']');
                    i += 1;
                }
            },
            c => {
                buf.push(c);
                i += 1;
            }
        }
    }
    flush(&mut buf, &mut tokens);
    let roles = vec![Role::Literal; tokens.len()];
    Tokens { tokens, roles }
}

/// A marker may open before a non-space character.
fn opens(after: Option<char>) -> bool {
    after.is_some_and(|c| !c.is_whitespace())
}

/// A marker may close after a non-space character.
fn closes(before: Option<char>) -> bool {
    before.is_some_and(|c| !c.is_whitespace())
}

fn starts_with(chars: &[char], at: usize, pattern: &str) -> bool {
    pattern
        .chars()
        .enumerate()
        .all(|(i, p)| chars.get(at + i) == Some(&p))
}

fn find(chars: &[char], from: usize, c: char) -> Option<usize> {
    chars
        .get(from..)?
        .iter()
        .position(|&x| x == c)
        .map(|p| from + p)
}

/// A fenced code block starting at `at` (its opening ```` ``` ````): the
/// token and where the text goes on, or `None` when it is never closed or
/// empty. A first line of only a language name gives the block's language;
/// the line breaks right after the opening and right before the closing
/// fence are the fences', not the code's.
fn code_block(chars: &[char], at: usize) -> Option<(Token, usize)> {
    let after_fence = at + 3;
    let language_line = find(chars, after_fence, '\n').and_then(|nl| {
        let name = &chars[after_fence..nl];
        let is_language = name.len() <= MAX_LANGUAGE_CHARS
            && name
                .iter()
                .all(|c| c.is_ascii_alphanumeric() || "+#-_.".contains(*c));
        is_language.then(|| (name.iter().collect::<String>(), nl + 1))
    });
    let (language, body_start) = language_line.unwrap_or((String::new(), after_fence));
    let close = (body_start..chars.len()).find(|&i| starts_with(chars, i, "```"))?;
    let mut body: String = chars[body_start..close].iter().collect();
    if body.ends_with('\n') {
        body.pop();
    }
    if body.is_empty() {
        return None;
    }
    Some((Token::Pre { language, body }, close + 3))
}

/// A link's `url)` starting at `at`, right after `](`: the URL (`\)` writes
/// a `)`, as `\` does before any punctuation) and where the text goes on. `None` for an empty URL, one with a
/// space or control character, one without its `)`, or another scheme.
fn link_target(chars: &[char], at: usize) -> Option<(String, usize)> {
    let mut url = String::new();
    let mut i = at;
    loop {
        match *chars.get(i)? {
            ')' => break,
            '\\' if chars.get(i + 1).is_some_and(char::is_ascii_punctuation) => {
                url.push(chars[i + 1]);
                i += 2;
            }
            c if c.is_whitespace() || c.is_control() => return None,
            c => {
                url.push(c);
                i += 1;
            }
        }
    }
    let lower = url.to_ascii_lowercase();
    let linkable = LINK_SCHEMES
        .iter()
        .any(|scheme| lower.starts_with(scheme) && lower.len() > scheme.len());
    linkable.then_some((url, i + 1))
}

// ── Pairing ─────────────────────────────────────────────────────────────────

/// Pair openers with closers, innermost first, with a stack: a closer
/// closes the nearest opener of its kind that no link starts after, and
/// openers left between them stay text, as do openers never closed. A link
/// closes the nearest `[`; links don't nest, so the `[`s before it stay text.
/// A pair with nothing between them stays text too.
fn pair(tokens: &mut Tokens) {
    let mut open: Vec<usize> = Vec::new();
    for i in 0..tokens.tokens.len() {
        match &tokens.tokens[i] {
            &Token::Delimiter {
                mark,
                can_open,
                can_close,
            } => {
                let opener = can_close
                    .then(|| {
                        open.iter()
                            .rev()
                            .take_while(|&&o| tokens.tokens[o] != Token::LinkOpen)
                            .find(|&&o| {
                                matches!(tokens.tokens[o], Token::Delimiter { mark: m, .. } if m == mark)
                            })
                            .copied()
                    })
                    .flatten()
                    .filter(|&o| o + 1 < i);
                match opener {
                    Some(o) => {
                        tokens.roles[o] = Role::Opens;
                        tokens.roles[i] = Role::Closes(o);
                        let at = open.iter().position(|&x| x == o).expect("on the stack");
                        open.truncate(at);
                    }
                    None if can_open => open.push(i),
                    None => {}
                }
            }
            Token::LinkOpen => open.push(i),
            Token::LinkClose { .. } => {
                let opener = open
                    .iter()
                    .rposition(|&o| tokens.tokens[o] == Token::LinkOpen)
                    .filter(|&at| open[at] + 1 < i);
                if let Some(at) = opener {
                    let o = open[at];
                    tokens.roles[o] = Role::Opens;
                    tokens.roles[i] = Role::Closes(o);
                    open.truncate(at);
                    open.retain(|&x| tokens.tokens[x] != Token::LinkOpen);
                }
            }
            Token::Text(_) | Token::Code(_) | Token::Pre { .. } => {}
        }
    }
}

// ── Rendering ───────────────────────────────────────────────────────────────

/// The text and entities of paired tokens.
fn render(tokens: &Tokens) -> FormattedText {
    fn write(out: &mut String, at: &mut i32, s: &str) {
        out.push_str(s);
        *at += s.encode_utf16().count() as i32;
    }
    let mut out = String::new();
    let mut at: i32 = 0;
    let mut starts: Vec<i32> = vec![0; tokens.tokens.len()];
    let mut entities = Vec::new();
    for (i, token) in tokens.tokens.iter().enumerate() {
        let role = tokens.roles[i];
        match (token, role) {
            (Token::Text(s), _) => write(&mut out, &mut at, s),
            (Token::Code(s), _) => {
                let start = at;
                write(&mut out, &mut at, s);
                entities.push(Entity {
                    kind: EntityKind::Code,
                    offset: start,
                    length: at - start,
                });
            }
            (Token::Pre { language, body }, _) => {
                let start = at;
                write(&mut out, &mut at, body);
                entities.push(Entity {
                    kind: EntityKind::Pre {
                        language: language.clone(),
                    },
                    offset: start,
                    length: at - start,
                });
            }
            (_, Role::Opens) => starts[i] = at,
            (Token::Delimiter { mark, .. }, Role::Closes(o)) => entities.push(Entity {
                kind: mark.kind(),
                offset: starts[o],
                length: at - starts[o],
            }),
            (Token::LinkClose { url, .. }, Role::Closes(o)) => entities.push(Entity {
                kind: EntityKind::TextUrl { url: url.clone() },
                offset: starts[o],
                length: at - starts[o],
            }),
            (Token::Delimiter { mark, .. }, _) => write(&mut out, &mut at, mark.raw()),
            (Token::LinkOpen, _) => write(&mut out, &mut at, "["),
            (Token::LinkClose { raw, .. }, _) => write(&mut out, &mut at, raw),
        }
    }
    // Entities are made as they close: of two on the same span, the later
    // one encloses the other, so it goes first.
    let mut made: Vec<(usize, Entity)> = entities
        .into_iter()
        .enumerate()
        .filter(|(_, e)| e.length > 0)
        .collect();
    made.sort_by_key(|(i, e)| (e.offset, std::cmp::Reverse(e.length), std::cmp::Reverse(*i)));
    FormattedText {
        text: out,
        entities: made.into_iter().map(|(_, e)| e).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(kind: EntityKind, offset: i32, length: i32) -> Entity {
        Entity {
            kind,
            offset,
            length,
        }
    }

    fn bold(offset: i32, length: i32) -> Entity {
        entity(EntityKind::Bold, offset, length)
    }

    fn italic(offset: i32, length: i32) -> Entity {
        entity(EntityKind::Italic, offset, length)
    }

    fn code(offset: i32, length: i32) -> Entity {
        entity(EntityKind::Code, offset, length)
    }

    fn link(url: &str, offset: i32, length: i32) -> Entity {
        entity(EntityKind::TextUrl { url: url.into() }, offset, length)
    }

    fn pre(language: &str, offset: i32, length: i32) -> Entity {
        entity(
            EntityKind::Pre {
                language: language.into(),
            },
            offset,
            length,
        )
    }

    #[track_caller]
    fn converts(markdown_text: &str, text: &str, entities: Vec<Entity>) {
        assert_eq!(
            markdown(markdown_text),
            FormattedText {
                text: text.into(),
                entities
            },
            "{markdown_text:?}"
        );
    }

    #[test]
    fn each_supported_kind_becomes_its_entity() {
        converts("a **b** c", "a b c", vec![bold(2, 1)]);
        converts("a *b* c", "a b c", vec![italic(2, 1)]);
        converts("a _b_ c", "a b c", vec![italic(2, 1)]);
        converts("run `cargo test`", "run cargo test", vec![code(4, 10)]);
        converts(
            "see [the docs](https://example.com/a_b*c) now",
            "see the docs now",
            vec![link("https://example.com/a_b*c", 4, 8)],
        );
        converts(
            "```rust\nfn main() {}\n```",
            "fn main() {}",
            vec![pre("rust", 0, 12)],
        );
        converts("```\nx = 1\n```", "x = 1", vec![pre("", 0, 5)]);
        converts("```one line```", "one line", vec![pre("", 0, 8)]);
    }

    #[test]
    fn plain_text_has_no_entities_and_plain_format_keeps_markup() {
        converts("just words, 100% plain.", "just words, 100% plain.", vec![]);
        let raw = "**not bold** `nor code`";
        assert_eq!(
            FormattedText::new(raw, TextFormat::Plain),
            FormattedText::plain(raw)
        );
        assert!(
            FormattedText::new(raw, TextFormat::Plain)
                .entities
                .is_empty()
        );
        assert_eq!(FormattedText::new("", TextFormat::Markdown).text, "");
    }

    #[test]
    fn entities_nest_and_come_sorted_outer_first() {
        converts(
            "**bold _and italic_ [link](tg://resolve?domain=x)**",
            "bold and italic link",
            vec![
                bold(0, 20),
                italic(5, 10),
                link("tg://resolve?domain=x", 16, 4),
            ],
        );
        converts(
            "[**bold link**](https://e.com)",
            "bold link",
            vec![link("https://e.com", 0, 9), bold(0, 9)],
        );
        converts("***both***", "both", vec![bold(0, 4), italic(0, 4)]);
        converts(
            "**bold `code` bold**",
            "bold code bold",
            vec![bold(0, 14), code(5, 4)],
        );
    }

    #[test]
    fn offsets_count_utf16_code_units() {
        // 😀 is two UTF-16 code units; é and я are one each.
        converts("😀 **é**", "😀 é", vec![bold(3, 1)]);
        converts(
            "**😀😀** _я_ `👍🏽`",
            "😀😀 я 👍🏽",
            vec![bold(0, 4), italic(5, 1), code(7, 4)],
        );
        let family = "👨\u{200D}👩\u{200D}👧";
        converts(
            &format!("[{family}](https://e.com) x"),
            &format!("{family} x"),
            vec![link("https://e.com", 0, 8)],
        );
    }

    #[test]
    fn unclosed_or_unpaired_markers_stay_as_typed() {
        for text in [
            "2 * 3 * 4",
            "a ** b",
            "**never closed",
            "closed** never opened",
            "* list item",
            "`unclosed code",
            "```\nunclosed block",
            "[no link]",
            "[text](no scheme)",
            "[text](javascript:alert(1))",
            "[text](ftp://x)",
            "[text](https://e.com",
            "](https://e.com)",
            "[](https://e.com)",
            "``",
            "````",
            "******",
            "a **** b",
        ] {
            converts(text, text, vec![]);
        }
    }

    #[test]
    fn underscores_inside_words_are_text() {
        converts("snake_case_name", "snake_case_name", vec![]);
        converts("__init__", "__init__", vec![]);
        converts("a_b _c_ d_", "a_b c d_", vec![italic(4, 1)]);
        // Stars may format part of a word.
        converts(
            "un*frigging*believable",
            "unfriggingbelievable",
            vec![italic(2, 8)],
        );
    }

    #[test]
    fn markers_need_a_non_space_on_their_inner_side() {
        converts("a * b * c", "a * b * c", vec![]);
        converts("a *b * c*", "a b * c", vec![italic(2, 5)]);
        converts("** x **", "** x **", vec![]);
    }

    #[test]
    fn crossing_markers_keep_the_first_pair_and_the_rest_as_text() {
        converts("**a *b** c*", "a *b c*", vec![bold(0, 4)]);
        converts("*a **b* c**", "a **b c**", vec![italic(0, 5)]);
        // A link's text closes no emphasis opened before it.
        converts(
            "**a [b** c](https://e.com)",
            "**a b** c",
            vec![link("https://e.com", 4, 5)],
        );
    }

    #[test]
    fn links_do_not_nest() {
        converts(
            "[a [b](https://b.com) c](https://a.com)",
            "[a b c](https://a.com)",
            vec![link("https://b.com", 3, 1)],
        );
    }

    #[test]
    fn backslash_writes_punctuation_as_is() {
        converts(r"\*not italic\*", "*not italic*", vec![]);
        converts(
            r"a \_b\_ \`c\` \[d\](https://e.com)",
            "a _b_ `c` [d](https://e.com)",
            vec![],
        );
        converts(
            r"MarkdownV2 style\. Done\!",
            "MarkdownV2 style. Done!",
            vec![],
        );
        converts(r"C:\Users\me", r"C:\Users\me", vec![]);
        converts(r"\\**b**", r"\b", vec![bold(1, 1)]);
        converts(
            r"[x](https://e.com/a_\(b\))",
            "x",
            vec![link("https://e.com/a_(b)", 0, 1)],
        );
    }

    #[test]
    fn code_is_verbatim() {
        converts("`**not bold** \\*`", "**not bold** \\*", vec![code(0, 15)]);
        converts(
            "```\n**x** [y](https://e.com)\n```",
            "**x** [y](https://e.com)",
            vec![pre("", 0, 24)],
        );
        // A first line that is no language name is code.
        converts(
            "```let x = 1;\nlet y = 2;\n```",
            "let x = 1;\nlet y = 2;",
            vec![pre("", 0, 21)],
        );
        converts(
            "before\n```python\nprint('hi')\n```\nafter **b**",
            "before\nprint('hi')\nafter b",
            vec![pre("python", 7, 11), bold(25, 1)],
        );
    }

    #[test]
    fn pathological_input_is_linear_enough() {
        // Openers never closed, and closers that find no opener of their kind.
        for text in [
            "**a *b _c [d ".repeat(2_000),
            format!("{}{}", "*a ".repeat(3_000), "b** ".repeat(3_000)),
            "`a ``` ".repeat(2_000),
        ] {
            let started = std::time::Instant::now();
            let converted = markdown(&text);
            assert!(started.elapsed() < std::time::Duration::from_secs(2));
            assert!(converted.text.chars().count() <= text.chars().count());
        }
    }
}
