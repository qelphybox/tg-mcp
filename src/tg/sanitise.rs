//! Hygiene for Telegram content on its way to the model.
//!
//! Message text, titles, names, usernames and descriptions are written by other
//! people, and any of them may try to talk to the model ("ignore previous
//! instructions and …"). The structural boundary is typed JSON: content only
//! ever sits in string values. On top of that, every such string loses the
//! characters that hide text from a human reading the transcript while the
//! model still reads it: Unicode format characters (general category `Cf`:
//! zero-width spaces, bidi overrides and isolates, BOM, invisible operators,
//! tag characters, …) and control characters other than line breaks and tabs.
//! Detecting injection attempts by their content is not attempted.
//!
//! The seam is the [`Telegram`] trait itself: tools read through [`Sanitised`],
//! which sanitises every answer of the Live client or the fake before a tool
//! sees it, so length caps applied by the tools count what is left. A new
//! `Telegram` method doesn't compile until [`Sanitised`] forwards it, and its
//! result type has to be [`Untrusted`]; a new field doesn't compile until its
//! type's [`Untrusted`] impl says whether it is content.

use std::{cmp::Ordering, sync::Arc};

use super::{
    Account, Telegram, TgError,
    chats::{ChatInfo, ChatPage, ChatRef, ChatSummary, MessagePreview},
    messages::{ChatMessage, HistoryQuery, MessagePage, Sender},
};

/// Unicode general category `Cf` (format characters), sorted. Identical in
/// Unicode 15.1 through 18.0; checked against `unicodedata` and the
/// `unicode-properties` tables rather than pulling a crate in for 21 ranges.
const FORMAT: &[(char, char)] = &[
    ('\u{00AD}', '\u{00AD}'),   // soft hyphen
    ('\u{0600}', '\u{0605}'),   // Arabic number signs
    ('\u{061C}', '\u{061C}'),   // Arabic letter mark
    ('\u{06DD}', '\u{06DD}'),   // Arabic end of ayah
    ('\u{070F}', '\u{070F}'),   // Syriac abbreviation mark
    ('\u{0890}', '\u{0891}'),   // Arabic pound / piastre mark above
    ('\u{08E2}', '\u{08E2}'),   // Arabic disputed end of ayah
    ('\u{180E}', '\u{180E}'),   // Mongolian vowel separator
    ('\u{200B}', '\u{200F}'),   // ZWSP, ZWNJ, ZWJ, LRM, RLM
    ('\u{202A}', '\u{202E}'),   // bidi embeddings and overrides (LRE … RLO)
    ('\u{2060}', '\u{2064}'),   // word joiner, invisible operators
    ('\u{2066}', '\u{206F}'),   // bidi isolates, deprecated format controls
    ('\u{FEFF}', '\u{FEFF}'),   // BOM / zero-width no-break space
    ('\u{FFF9}', '\u{FFFB}'),   // interlinear annotation
    ('\u{110BD}', '\u{110BD}'), // Kaithi number sign
    ('\u{110CD}', '\u{110CD}'), // Kaithi number sign above
    ('\u{13430}', '\u{1343F}'), // Egyptian hieroglyph format controls
    ('\u{1BCA0}', '\u{1BCA3}'), // shorthand format controls
    ('\u{1D173}', '\u{1D17A}'), // musical symbol format controls
    ('\u{E0001}', '\u{E0001}'), // language tag
    ('\u{E0020}', '\u{E007F}'), // tag characters ("ASCII smuggling")
];

const ZWJ: char = '\u{200D}';
const EMOJI_PRESENTATION: char = '\u{FE0F}';

/// Unicode general category `Cf`.
pub fn is_format(c: char) -> bool {
    FORMAT
        .binary_search_by(|&(first, last)| {
            if last < c {
                Ordering::Less
            } else if first > c {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        })
        .is_ok()
}

/// The blocks emoji in ZWJ sequences come from: technical and miscellaneous
/// symbols up to U+2BFF (⚕ ♀ ❄ ❤ ➡ ⬛ …) and the supplementary emoji blocks
/// (U+1F000–U+1FAFF, skin tones included).
fn is_emoji(c: char) -> bool {
    matches!(c, '\u{2300}'..='\u{2BFF}' | '\u{1F000}'..='\u{1FAFF}')
}

/// Strip what hides text from a human: every `Cf` character, and control
/// characters other than `\n` and `\t` (CRLF and a lone CR become `\n`, the
/// rest a space). Everything visible stays, emoji included: flags, skin tones
/// and keycaps contain no `Cf`. ZWJ (U+200D) is `Cf` too, but it is kept where
/// it joins two emoji (👨‍👩‍👧, 🏳️‍🌈, 👩🏽‍💻), so those still render as one
/// glyph; anywhere else it goes. Tag characters always go, so the subdivision
/// flags (England, Scotland, Wales) show as a plain black flag 🏴.
///
/// Characters, not length, are its business: callers cap the result.
pub fn sanitise(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ZWJ => {
                let after_emoji = out
                    .chars()
                    .next_back()
                    .is_some_and(|prev| is_emoji(prev) || prev == EMOJI_PRESENTATION);
                let before_emoji = chars.peek().is_some_and(|&next| is_emoji(next));
                if after_emoji && before_emoji {
                    out.push(c);
                }
            }
            '\n' | '\t' => out.push(c),
            '\r' if chars.peek() == Some(&'\n') => {}
            '\r' => out.push('\n'),
            c if c.is_control() => out.push(' '),
            c if is_format(c) => {}
            c => out.push(c),
        }
    }
    out
}

/// A tool result whose strings written on Telegram can be sanitised in place.
/// Implementations destructure exhaustively, so a new field must be sorted
/// into content (sanitised) or ours (ids, dates, kinds, counts) to compile.
pub trait Untrusted {
    fn sanitise(&mut self);
}

fn clean(field: &mut String) {
    *field = sanitise(field);
}

fn clean_opt(field: &mut Option<String>) {
    if let Some(value) = field {
        clean(value);
    }
}

impl Untrusted for Account {
    fn sanitise(&mut self) {
        let Account {
            id: _,
            first_name,
            last_name,
            username,
        } = self;
        clean(first_name);
        clean_opt(last_name);
        clean_opt(username);
    }
}

impl Untrusted for ChatPage {
    fn sanitise(&mut self) {
        let ChatPage {
            chats,
            next_cursor: _,
        } = self;
        for chat in chats {
            let ChatSummary {
                id: _,
                title,
                kind: _,
                username,
                is_self: _,
                unread_count: _,
                pinned: _,
                last_message,
            } = chat;
            clean(title);
            clean_opt(username);
            if let Some(MessagePreview { date: _, text }) = last_message {
                clean(text);
            }
        }
    }
}

impl Untrusted for MessagePage {
    fn sanitise(&mut self) {
        let MessagePage {
            chat_id: _,
            messages,
            next_before_id: _,
        } = self;
        for message in messages {
            let ChatMessage {
                id: _,
                date: _,
                sender,
                outgoing: _,
                text,
                reply_to: _,
                forwarded: _,
                edited: _,
            } = message;
            clean(text);
            if let Some(Sender { id: _, name }) = sender {
                clean(name);
            }
        }
    }
}

impl Untrusted for ChatInfo {
    fn sanitise(&mut self) {
        let ChatInfo {
            id: _,
            title,
            kind: _,
            username,
            is_self: _,
            description,
            member_count: _,
        } = self;
        clean(title);
        clean_opt(username);
        clean_opt(description);
        // Nothing but hidden characters is no description.
        if description.as_deref().is_some_and(|d| d.trim().is_empty()) {
            *description = None;
        }
    }
}

fn sanitised<T: Untrusted>(mut value: T) -> T {
    value.sanitise();
    value
}

/// The one way tools reach Telegram: every answer comes back sanitised.
pub struct Sanitised(Arc<dyn Telegram>);

impl Sanitised {
    pub fn new(telegram: Arc<dyn Telegram>) -> Self {
        Self(telegram)
    }
}

#[async_trait::async_trait]
impl Telegram for Sanitised {
    async fn whoami(&self) -> Result<Account, TgError> {
        self.0.whoami().await.map(sanitised)
    }

    async fn list_chats(
        &self,
        limit: usize,
        cursor: Option<&str>,
        archived: bool,
    ) -> Result<ChatPage, TgError> {
        self.0
            .list_chats(limit, cursor, archived)
            .await
            .map(sanitised)
    }

    async fn get_messages(
        &self,
        chat: ChatRef,
        query: HistoryQuery,
    ) -> Result<MessagePage, TgError> {
        self.0.get_messages(chat, query).await.map(sanitised)
    }

    async fn get_chat_info(&self, chat: ChatRef) -> Result<ChatInfo, TgError> {
        self.0.get_chat_info(chat).await.map(sanitised)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tg::chats::ChatKind;

    #[test]
    fn the_format_table_is_sorted_and_disjoint() {
        for pair in FORMAT.windows(2) {
            assert!(pair[0].0 <= pair[0].1, "{pair:?}");
            assert!(pair[0].1 < pair[1].0, "{pair:?}");
        }
        for &(first, last) in FORMAT {
            assert!(is_format(first) && is_format(last), "{first:?}..{last:?}");
        }
        // Neighbours that are not Cf.
        for c in [
            '\u{00AC}',
            '\u{00AE}',
            '\u{200A}',
            '\u{2010}',
            '\u{2065}',
            '\u{FEFE}',
            '\u{FFFC}',
            '\u{E0000}',
            '\u{E0002}',
            '\u{E001F}',
            '\u{E0080}',
        ] {
            assert!(!is_format(c), "{:04X}", c as u32);
        }
    }

    #[test]
    fn zero_width_and_invisible_characters_are_removed() {
        assert_eq!(
            sanitise("ig\u{200B}no\u{200C}re\u{2060} \u{FEFF}all\u{00AD} \u{2062}rules\u{180E}"),
            "ignore all rules"
        );
        assert_eq!(sanitise("\u{200E}left\u{200F}right\u{061C}"), "leftright");
    }

    #[test]
    fn bidi_overrides_and_isolates_are_removed() {
        // "\u{202E}txet" displays as "text" to a human but reads differently.
        assert_eq!(sanitise("a \u{202E}txet\u{202C} b"), "a txet b");
        assert_eq!(
            sanitise("\u{202A}\u{202B}\u{202D}x\u{2066}y\u{2067}z\u{2068}w\u{2069}"),
            "xyzw"
        );
    }

    #[test]
    fn tag_characters_are_removed_with_their_hidden_text() {
        // Tag characters mirror ASCII invisibly: "ignore" here.
        let hidden: String = "ignore"
            .chars()
            .map(|c| char::from_u32(0xE0000 + c as u32).unwrap())
            .collect();
        assert_eq!(
            sanitise(&format!("hello\u{E0001}{hidden}\u{E007F} world")),
            "hello world"
        );
        // A subdivision flag is a tag sequence: it degrades to the black flag.
        assert_eq!(
            sanitise("\u{1F3F4}\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}"),
            "\u{1F3F4}"
        );
    }

    #[test]
    fn scripts_and_emoji_survive() {
        for text in [
            "Привет, мир! Ёжик",
            "你好，世界。こんにちは 안녕하세요",
            "مرحبا שלום नमस्ते",
            "flags 🇺🇦 🇯🇵 🏳️ 🏴",
            "skin tones 👍🏽 👋🏿 🧑🏻",
            "keycaps 1️⃣ #️⃣ *️⃣",
            "families 👨‍👩‍👧 👨‍👩‍👧‍👦 🧑‍🤝‍🧑",
            "professions 👩🏽‍💻 👨‍⚕️ 🧑‍✈️ 🧔‍♂️",
            "others 🏳️‍🌈 🏴‍☠️ ❤️‍🔥 ❤️‍🩹 🐈‍⬛ 🐻‍❄️ 👁️‍🗨️ 🏃‍➡️",
            "symbols © ® ™ ❤ ☺ ½ → ∑ «quoted» — dash",
        ] {
            assert_eq!(sanitise(text), text);
        }
    }

    #[test]
    fn zwj_only_survives_between_emoji() {
        assert_eq!(sanitise("a\u{200D}b"), "ab");
        assert_eq!(sanitise("\u{200D}👩"), "👩");
        assert_eq!(sanitise("x\u{200D}👩"), "x👩");
        assert_eq!(sanitise("👩\u{200D}x"), "👩x");
        assert_eq!(sanitise("👩\u{200D}"), "👩");
        assert_eq!(sanitise("👩\u{200D}\u{200D}💻"), "👩\u{200D}💻");
        // Removing a hidden character doesn't leave a stray joiner behind.
        assert_eq!(sanitise("👩\u{200B}\u{200D}💻"), "👩\u{200D}💻");
    }

    #[test]
    fn line_breaks_and_tabs_stay_other_controls_go() {
        assert_eq!(sanitise("one\ntwo\tcol\n\nthree"), "one\ntwo\tcol\n\nthree");
        assert_eq!(sanitise("crlf\r\nline\rcr"), "crlf\nline\ncr");
        assert_eq!(
            sanitise("bell\u{7}esc\u{1B}[2Jnul\u{0}c1\u{85}"),
            "bell esc [2Jnul c1 "
        );
    }

    fn summary(title: &str, preview: &str) -> ChatSummary {
        ChatSummary {
            id: "user:1~AAAAAAAAAAE".into(),
            title: title.into(),
            kind: ChatKind::Private,
            username: Some("ali\u{200B}ce".into()),
            is_self: false,
            unread_count: 0,
            pinned: false,
            last_message: Some(MessagePreview {
                date: "2026-11-06T00:00:00Z".into(),
                text: preview.into(),
            }),
        }
    }

    #[test]
    fn every_content_field_is_sanitised_and_ids_are_left_alone() {
        let mut page = ChatPage {
            chats: vec![summary("Al\u{202E}ice", "hi\u{2066}!")],
            next_cursor: Some("cursor".into()),
        };
        page.sanitise();
        let chat = &page.chats[0];
        assert_eq!(chat.title, "Alice");
        assert_eq!(chat.username.as_deref(), Some("alice"));
        assert_eq!(chat.last_message.as_ref().unwrap().text, "hi!");
        assert_eq!(chat.id, "user:1~AAAAAAAAAAE");

        let mut info = ChatInfo {
            id: "channel:2".into(),
            title: "T\u{200B}eam".into(),
            kind: ChatKind::Channel,
            username: Some("te\u{FEFF}am".into()),
            is_self: false,
            description: Some("line\u{200F}\nnext".into()),
            member_count: Some(3),
        };
        info.sanitise();
        assert_eq!(
            (info.title.as_str(), info.username.as_deref()),
            ("Team", Some("team"))
        );
        assert_eq!(info.description.as_deref(), Some("line\nnext"));

        let mut ghost = ChatInfo {
            description: Some("\u{200B}\u{2060}".into()),
            ..info
        };
        ghost.sanitise();
        assert_eq!(ghost.description, None, "only hidden characters");

        let mut me = Account {
            id: 1,
            first_name: "M\u{200B}e".into(),
            last_name: Some("\u{202E}Last".into()),
            username: Some("m\u{2060}e".into()),
        };
        me.sanitise();
        assert_eq!(
            (
                me.first_name.as_str(),
                me.last_name.as_deref(),
                me.username.as_deref()
            ),
            ("Me", Some("Last"), Some("me"))
        );

        let mut history = MessagePage {
            chat_id: "user:1".into(),
            messages: vec![ChatMessage {
                id: 1,
                date: "2026-11-06T00:00:00Z".into(),
                sender: Some(Sender {
                    id: "user:1".into(),
                    name: "Bo\u{E0041}b".into(),
                }),
                outgoing: false,
                text: "a\u{200B}\nb".into(),
                reply_to: None,
                forwarded: false,
                edited: false,
            }],
            next_before_id: None,
        };
        history.sanitise();
        assert_eq!(history.messages[0].text, "a\nb");
        assert_eq!(history.messages[0].sender.as_ref().unwrap().name, "Bob");
    }
}
