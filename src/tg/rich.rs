//! Rich messages (#50) as plain text.
//!
//! Some posts keep their content in `message.rich_message`, page blocks of
//! the Instant View kind (paragraphs, lists, tables, collapsible sections,
//! photos), and leave `message.message` empty. [`message_text`] flattens the
//! blocks into lines in document order, so such a post reads like any other
//! message: the result is message text, and goes the way all message text
//! goes (sanitiser, length caps, previews, search snippets).

use grammers_client::tl;

use super::sanitise::is_blank;

/// Most characters a Rich message flattens to; the rest is cut and marked
/// [`MORE`]. Four times a plain message's 4096, so a snippet still finds a
/// word deep in a long post, while the work per message stays bounded. The
/// tools cut further.
pub const MAX_CHARS: usize = 16_384;

/// Most levels of blocks inside blocks (a table in a collapsible section is
/// two); deeper blocks are left out.
pub const MAX_DEPTH: usize = 8;

/// Most levels of text styles inside each other (a bold word in a link in a
/// concatenation is three); deeper text is left out.
pub const MAX_TEXT_DEPTH: usize = 16;

/// The last line of a Rich message that goes on: cut at [`MAX_CHARS`], or
/// only part of it sent (`part`).
pub const MORE: &str = "[…]";

/// The text of a message whose `message` is `message` and whose Rich message
/// is `rich`: `message` when the Rich message has no text, the flattened
/// Rich message when `message` has nothing visible, else both, `message`
/// first and a blank line between.
pub fn message_text(message: String, rich: Option<&tl::enums::RichMessage>) -> String {
    let rich = rich.map(flatten).unwrap_or_default();
    if rich.is_empty() {
        message
    } else if is_blank(&message) {
        rich
    } else {
        format!("{message}\n\n{rich}")
    }
}

/// `rich` as plain lines, at most [`MAX_CHARS`] characters plus [`MORE`].
///
/// | Block | Lines |
/// |---|---|
/// | title, subtitle, kicker, author (not the date), header, subheader, heading 1–6, paragraph, preformatted, footer, math, thinking | its text |
/// | list, ordered list | `- item`, `- [x] item`; `3. item` (its `num`, else its `value`, else counted from `start`), nested lines indented |
/// | table | its title, then a row per line, cells joined by ` \| ` |
/// | details (collapsible) | its title, then its blocks, whether open or not |
/// | blockquote (collapsed or not), pullquote, blockquote of blocks | its text or blocks, then its caption |
/// | photo, video, audio, document, embed, map | `[photo]`, `[video]`, `[audio]`, `[file]`, `[media]`, `[location]` with its caption; a run of the same marker without captions is one line, `[10 photos]` |
/// | collage, slideshow, cover, embedded post | the author of a post, its blocks, then its caption |
/// | channel | `[channel]` |
/// | related articles | its title, then `- title` per article |
/// | unsupported, divider, anchor, button row | nothing (buttons are out of scope) |
///
/// Text styles read as their text (a link's text, not its URL; a custom
/// emoji as its emoji; a diff as the new text; math as its source); an inline
/// image is nothing. Lines with nothing visible (blank, or only hidden
/// characters such as a U+2800 spacer) are left out. Blocks deeper than
/// [`MAX_DEPTH`] and text deeper than [`MAX_TEXT_DEPTH`] are left out. A
/// Rich message sent only in part (`part`) ends with [`MORE`]: the rest needs
/// `messages.getRichMessage`, which no tool calls.
pub fn flatten(rich: &tl::enums::RichMessage) -> String {
    let tl::enums::RichMessage::Message(rich) = rich;
    let mut lines = Lines::default();
    lines.blocks(&rich.blocks, 0);
    let mut text = lines.text;
    if lines.cut || rich.part {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(MORE);
    }
    text
}

/// The flattened text so far.
#[derive(Default)]
struct Lines {
    text: String,
    chars: usize,
    /// [`MAX_CHARS`] reached: nothing more is added.
    cut: bool,
    /// What starts each line: two spaces per list level.
    indent: String,
    /// What starts the next line instead: a list item's marker.
    bullet: Option<String>,
    /// The last line, if it is a media marker without a caption.
    run: Option<Run>,
}

/// Media markers without a caption on one line, `[photo]` or `[10 photos]`.
struct Run {
    media: Media,
    count: usize,
    /// The indent the line is under.
    indent: String,
    /// Where the marker starts in the text, in bytes and in characters.
    start: usize,
    chars: usize,
}

/// What a media block shows as.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Media {
    Photo,
    Video,
    Audio,
    File,
    Embed,
    Location,
}

impl Media {
    /// The marker for `count` of them: `[photo]`, `[3 photos]`.
    fn marker(self, count: usize) -> String {
        let (one, many) = match self {
            Media::Photo => ("photo", "photos"),
            Media::Video => ("video", "videos"),
            Media::Audio => ("audio", "audio"),
            Media::File => ("file", "files"),
            Media::Embed => ("media", "media"),
            Media::Location => ("location", "locations"),
        };
        if count == 1 {
            format!("[{one}]")
        } else {
            format!("[{count} {many}]")
        }
    }
}

impl Lines {
    fn blocks(&mut self, blocks: &[tl::enums::PageBlock], depth: usize) {
        if depth >= MAX_DEPTH {
            return;
        }
        for block in blocks {
            if self.cut {
                return;
            }
            self.block(block, depth);
        }
    }

    fn block(&mut self, block: &tl::enums::PageBlock, depth: usize) {
        use tl::enums::PageBlock as B;
        let inner = depth + 1;
        match block {
            B::Unsupported | B::Divider | B::Anchor(_) | B::ButtonRow(_) => {}
            B::Title(b) => self.text(&b.text),
            B::Subtitle(b) => self.text(&b.text),
            B::Kicker(b) => self.text(&b.text),
            B::AuthorDate(b) => self.text(&b.author),
            B::Header(b) => self.text(&b.text),
            B::Subheader(b) => self.text(&b.text),
            B::Heading1(b) => self.text(&b.text),
            B::Heading2(b) => self.text(&b.text),
            B::Heading3(b) => self.text(&b.text),
            B::Heading4(b) => self.text(&b.text),
            B::Heading5(b) => self.text(&b.text),
            B::Heading6(b) => self.text(&b.text),
            B::Paragraph(b) => self.text(&b.text),
            B::Preformatted(b) => self.text(&b.text),
            B::Footer(b) => self.text(&b.text),
            B::Thinking(b) => self.text(&b.text),
            B::Math(b) => self.line(&b.source),
            B::List(b) => {
                for item in &b.items {
                    match item {
                        tl::enums::PageListItem::Text(i) => {
                            self.item(&marker("-", i.checkbox, i.checked), |l| l.text(&i.text))
                        }
                        tl::enums::PageListItem::Blocks(i) => self
                            .item(&marker("-", i.checkbox, i.checked), |l| {
                                l.blocks(&i.blocks, inner)
                            }),
                    }
                }
            }
            B::OrderedList(b) => {
                let count = i32::try_from(b.items.len()).unwrap_or(i32::MAX);
                for (n, item) in (0..).zip(&b.items) {
                    let (checkbox, checked, num, value) = match item {
                        tl::enums::PageListOrderedItem::Text(i) => {
                            (i.checkbox, i.checked, &i.num, i.value)
                        }
                        tl::enums::PageListOrderedItem::Blocks(i) => {
                            (i.checkbox, i.checked, &i.num, i.value)
                        }
                    };
                    let number = match (num, value) {
                        (Some(num), _) => num.clone(),
                        (None, Some(value)) => value.to_string(),
                        (None, None) if b.reversed => {
                            b.start.unwrap_or(count).saturating_sub(n).to_string()
                        }
                        (None, None) => b.start.unwrap_or(1).saturating_add(n).to_string(),
                    };
                    let marker = marker(&format!("{number}."), checkbox, checked);
                    match item {
                        tl::enums::PageListOrderedItem::Text(i) => {
                            self.item(&marker, |l| l.text(&i.text))
                        }
                        tl::enums::PageListOrderedItem::Blocks(i) => {
                            self.item(&marker, |l| l.blocks(&i.blocks, inner))
                        }
                    }
                }
            }
            B::Table(b) => {
                self.text(&b.title);
                for tl::enums::PageTableRow::Row(row) in &b.rows {
                    let cells: Vec<String> = row
                        .cells
                        .iter()
                        .map(|tl::enums::PageTableCell::Cell(cell)| {
                            cell.text
                                .as_ref()
                                .map(|t| plain(t).replace(['\n', '\r'], " ").trim().to_string())
                                .unwrap_or_default()
                        })
                        .collect();
                    if cells.iter().any(|c| !c.is_empty()) {
                        self.line(&cells.join(" | "));
                    }
                }
            }
            B::Details(b) => {
                self.text(&b.title);
                self.blocks(&b.blocks, inner);
            }
            B::Blockquote(b) => {
                self.text(&b.text);
                self.text(&b.caption);
            }
            B::Pullquote(b) => {
                self.text(&b.text);
                self.text(&b.caption);
            }
            B::BlockquoteBlocks(b) => {
                self.blocks(&b.blocks, inner);
                self.text(&b.caption);
            }
            B::Photo(b) => self.media(Media::Photo, &b.caption),
            B::Video(b) => self.media(Media::Video, &b.caption),
            B::Audio(b) => self.media(Media::Audio, &b.caption),
            B::Document(b) => self.media(Media::File, &b.caption),
            B::Embed(b) => self.media(Media::Embed, &b.caption),
            B::Map(b) => self.media(Media::Location, &b.caption),
            B::InputPageBlockMap(b) => self.media(Media::Location, &b.caption),
            B::Collage(b) => {
                self.blocks(&b.items, inner);
                self.line(&caption(&b.caption));
            }
            B::Slideshow(b) => {
                self.blocks(&b.items, inner);
                self.line(&caption(&b.caption));
            }
            B::Cover(b) => self.blocks(std::slice::from_ref(&b.cover), inner),
            B::EmbedPost(b) => {
                self.line(&b.author);
                self.blocks(&b.blocks, inner);
                self.line(&caption(&b.caption));
            }
            B::Channel(_) => self.line("[channel]"),
            B::RelatedArticles(b) => {
                self.text(&b.title);
                for tl::enums::PageRelatedArticle::Article(article) in &b.articles {
                    if let Some(title) = &article.title {
                        self.item("-", |l| l.line(title));
                    }
                }
            }
        }
    }

    /// One list item: its first line starts with `marker`, the others are
    /// indented under it.
    fn item(&mut self, marker: &str, body: impl FnOnce(&mut Self)) {
        let indent = self.indent.len();
        self.bullet = Some(format!("{}{marker} ", self.indent));
        self.indent.push_str("  ");
        body(self);
        self.bullet = None;
        self.indent.truncate(indent);
    }

    /// A media marker with its caption. Without one (or with nothing visible
    /// in it), it joins the run of the same marker on the line before, if
    /// there is one (not across list items or indents).
    fn media(&mut self, media: Media, of: &tl::enums::PageCaption) {
        let caption = caption(of);
        if !is_blank(&caption) {
            self.line(&format!("{} {caption}", media.marker(1)));
            return;
        }
        if self.cut {
            return;
        }
        let extends = self.bullet.is_none()
            && self
                .run
                .as_ref()
                .is_some_and(|run| run.media == media && run.indent == self.indent);
        if let Some(run) = self.run.as_mut().filter(|_| extends) {
            run.count += 1;
            let (start, chars, marker) = (run.start, run.chars, media.marker(run.count));
            self.text.truncate(start);
            self.chars = chars;
            self.push(&marker);
            return;
        }
        let marker = media.marker(1);
        self.line(&marker);
        if !self.cut {
            self.run = Some(Run {
                media,
                count: 1,
                indent: self.indent.clone(),
                start: self.text.len() - marker.len(),
                chars: self.chars - marker.chars().count(),
            });
        }
    }

    fn text(&mut self, text: &tl::enums::RichText) {
        self.line(&plain(text));
    }

    /// `text` line by line, lines with nothing visible left out.
    fn line(&mut self, text: &str) {
        for line in text.lines() {
            if is_blank(line) {
                continue;
            }
            self.run = None;
            if !self.text.is_empty() {
                self.push("\n");
            }
            let prefix = self.bullet.take().unwrap_or_else(|| self.indent.clone());
            self.push(&prefix);
            self.push(line.trim_end());
        }
    }

    /// `s`, or as much of it as [`MAX_CHARS`] leaves room for.
    fn push(&mut self, s: &str) {
        if self.cut {
            return;
        }
        let room = MAX_CHARS - self.chars;
        let len = s.chars().count();
        if len > room {
            self.text.extend(s.chars().take(room));
            self.chars = MAX_CHARS;
            self.cut = true;
        } else {
            self.text.push_str(s);
            self.chars += len;
        }
    }
}

/// A list item's marker: `-` or `3.`, then the checkbox if it has one.
fn marker(mark: &str, checkbox: bool, checked: bool) -> String {
    match (checkbox, checked) {
        (false, _) => mark.to_string(),
        (true, true) => format!("{mark} [x]"),
        (true, false) => format!("{mark} [ ]"),
    }
}

/// A caption's text and credit, on one line.
fn caption(caption: &tl::enums::PageCaption) -> String {
    let tl::enums::PageCaption::Caption(caption) = caption;
    let text = plain(&caption.text);
    let credit = plain(&caption.credit);
    format!("{} {}", text.trim(), credit.trim())
        .trim()
        .to_string()
}

/// `text` without its styles.
fn plain(text: &tl::enums::RichText) -> String {
    let mut out = String::new();
    write_plain(text, 0, &mut out);
    out
}

fn write_plain(text: &tl::enums::RichText, depth: usize, out: &mut String) {
    use tl::enums::RichText as T;
    // Bytes, not characters: four bytes per character at most.
    if depth >= MAX_TEXT_DEPTH || out.len() > 4 * MAX_CHARS {
        return;
    }
    let inner = depth + 1;
    match text {
        T::TextEmpty | T::TextImage(_) => {}
        T::TextPlain(t) => out.push_str(&t.text),
        T::TextMath(t) => out.push_str(&t.source),
        T::TextCustomEmoji(t) => out.push_str(&t.alt),
        T::TextConcat(t) => {
            for text in &t.texts {
                write_plain(text, inner, out);
            }
        }
        T::TextBold(t) => write_plain(&t.text, inner, out),
        T::TextItalic(t) => write_plain(&t.text, inner, out),
        T::TextUnderline(t) => write_plain(&t.text, inner, out),
        T::TextStrike(t) => write_plain(&t.text, inner, out),
        T::TextFixed(t) => write_plain(&t.text, inner, out),
        T::TextUrl(t) => write_plain(&t.text, inner, out),
        T::TextEmail(t) => write_plain(&t.text, inner, out),
        T::TextSubscript(t) => write_plain(&t.text, inner, out),
        T::TextSuperscript(t) => write_plain(&t.text, inner, out),
        T::TextMarked(t) => write_plain(&t.text, inner, out),
        T::TextPhone(t) => write_plain(&t.text, inner, out),
        T::TextAnchor(t) => write_plain(&t.text, inner, out),
        T::TextSpoiler(t) => write_plain(&t.text, inner, out),
        T::TextMention(t) => write_plain(&t.text, inner, out),
        T::TextHashtag(t) => write_plain(&t.text, inner, out),
        T::TextBotCommand(t) => write_plain(&t.text, inner, out),
        T::TextCashtag(t) => write_plain(&t.text, inner, out),
        T::TextAutoUrl(t) => write_plain(&t.text, inner, out),
        T::TextAutoEmail(t) => write_plain(&t.text, inner, out),
        T::TextAutoPhone(t) => write_plain(&t.text, inner, out),
        T::TextBankCard(t) => write_plain(&t.text, inner, out),
        T::TextMentionName(t) => write_plain(&t.text, inner, out),
        T::TextDate(t) => write_plain(&t.text, inner, out),
        T::TextDiff(t) => write_plain(&t.text, inner, out),
        T::TextButton(t) => write_plain(&t.text, inner, out),
        T::TextTonAddress(t) => write_plain(&t.text, inner, out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tg::fake::rich_listing;
    use tl::{enums::PageBlock as B, enums::RichText as T, types};

    fn plain(text: &str) -> T {
        T::TextPlain(types::TextPlain { text: text.into() })
    }

    fn concat(texts: Vec<T>) -> T {
        T::TextConcat(types::TextConcat { texts })
    }

    fn bold(text: T) -> T {
        T::TextBold(Box::new(types::TextBold { text }))
    }

    fn paragraph(text: &str) -> B {
        B::Paragraph(types::PageBlockParagraph { text: plain(text) })
    }

    fn caption(text: &str) -> tl::enums::PageCaption {
        tl::enums::PageCaption::Caption(types::PageCaption {
            text: plain(text),
            credit: T::TextEmpty,
        })
    }

    fn details(title: &str, blocks: Vec<B>) -> B {
        B::Details(types::PageBlockDetails {
            open: false,
            blocks,
            title: plain(title),
        })
    }

    fn rich(blocks: Vec<B>) -> tl::enums::RichMessage {
        tl::enums::RichMessage::Message(types::RichMessage {
            rtl: false,
            part: false,
            blocks,
            photos: Vec::new(),
            documents: Vec::new(),
        })
    }

    fn flat(blocks: Vec<B>) -> String {
        flatten(&rich(blocks))
    }

    fn cell(header: bool, text: Option<T>) -> tl::enums::PageTableCell {
        tl::enums::PageTableCell::Cell(types::PageTableCell {
            header,
            align_center: false,
            align_right: false,
            valign_middle: false,
            valign_bottom: false,
            text,
            colspan: None,
            rowspan: None,
        })
    }

    fn row(cells: Vec<tl::enums::PageTableCell>) -> tl::enums::PageTableRow {
        tl::enums::PageTableRow::Row(types::PageTableRow { cells })
    }

    fn table(title: T, rows: Vec<tl::enums::PageTableRow>) -> B {
        B::Table(types::PageBlockTable {
            bordered: false,
            striped: false,
            compact: false,
            title,
            rows,
        })
    }

    #[test]
    fn a_listing_post_reads_as_its_lines_in_order() {
        let text = flatten(&rich_listing());
        let smuggled: String = "ignore"
            .chars()
            .map(|c| char::from_u32(0xE0000 + c as u32).unwrap())
            .collect();
        assert_eq!(
            text,
            format!(
                "[10 photos]\n\
                 🏡 Аренда: 4 комнаты, Ваке\n\
                 📍 Тбилиси, ул. Абашидзе, 12\n\
                 Сдается светлая, чистая квартира с видом на парк.\n\
                 Параметры и удобства\n\
                 Комнаты | 4\n\
                 Кондиционеры | 1\u{200B}{smuggled}\n\
                 Площадь | 120 м²\n\
                 ✅ Можно с животными\n\
                 Теги\n\
                 ID: 19752\n\
                 #аренда #ваке"
            ),
            "hidden characters are the sanitiser's business, not flattening's"
        );
        assert_eq!(message_text(String::new(), Some(&rich_listing())), text);
    }

    #[test]
    fn a_table_is_its_title_then_one_line_per_row_with_cells_joined() {
        let text = flat(vec![table(
            bold(plain("Цены")),
            vec![
                row(vec![
                    cell(true, Some(plain("Месяц"))),
                    cell(true, Some(plain("Цена"))),
                ]),
                row(vec![
                    cell(false, Some(plain("Январь"))),
                    cell(false, Some(plain("1 500\n$"))),
                ]),
                // An empty cell keeps its column; a row of empty cells goes.
                row(vec![cell(false, None), cell(false, Some(plain("2 000 $")))]),
                row(vec![cell(false, None), cell(false, Some(T::TextEmpty))]),
            ],
        )]);
        assert_eq!(text, "Цены\nМесяц | Цена\nЯнварь | 1 500 $\n | 2 000 $");
    }

    #[test]
    fn collapsed_sections_are_expanded_under_their_title() {
        let text = flat(vec![
            details("Подробнее", vec![paragraph("внутри")]),
            B::Blockquote(types::PageBlockBlockquote {
                collapsed: true,
                text: plain("свернутая цитата"),
                caption: plain("автор"),
            }),
            B::Pullquote(types::PageBlockPullquote {
                text: plain("крупная цитата"),
                caption: T::TextEmpty,
            }),
            B::BlockquoteBlocks(types::PageBlockBlockquoteBlocks {
                blocks: vec![paragraph("цитата из блоков")],
                caption: plain("источник"),
            }),
        ]);
        assert_eq!(
            text,
            "Подробнее\nвнутри\nсвернутая цитата\nавтор\nкрупная цитата\n\
             цитата из блоков\nисточник"
        );
    }

    #[test]
    fn list_items_are_marked_and_ordered_ones_numbered() {
        let item = |text: &str| {
            tl::enums::PageListItem::Text(types::PageListItemText {
                checkbox: false,
                checked: false,
                text: plain(text),
            })
        };
        let ordered = |num: Option<&str>, value: Option<i32>, text: &str| {
            tl::enums::PageListOrderedItem::Text(types::PageListOrderedItemText {
                checkbox: false,
                checked: false,
                num: num.map(str::to_string),
                text: plain(text),
                value,
                r#type: None,
            })
        };
        let list = |items| B::List(types::PageBlockList { items });
        let text = flat(vec![
            list(vec![
                item("балкон"),
                tl::enums::PageListItem::Text(types::PageListItemText {
                    checkbox: true,
                    checked: true,
                    text: plain("парковка"),
                }),
                tl::enums::PageListItem::Blocks(types::PageListItemBlocks {
                    checkbox: true,
                    checked: false,
                    blocks: vec![
                        paragraph("техника:\nкроме"),
                        list(vec![item("стиральная машина")]),
                    ],
                }),
            ]),
            B::OrderedList(types::PageBlockOrderedList {
                reversed: false,
                items: vec![
                    ordered(None, None, "первый"),
                    ordered(None, None, "второй"),
                    ordered(Some("IV"), None, "четвертый"),
                    ordered(None, Some(9), "девятый"),
                    tl::enums::PageListOrderedItem::Blocks(types::PageListOrderedItemBlocks {
                        checkbox: false,
                        checked: false,
                        num: None,
                        blocks: vec![paragraph("из блоков")],
                        value: None,
                        r#type: None,
                    }),
                ],
                start: Some(3),
                r#type: None,
            }),
            B::OrderedList(types::PageBlockOrderedList {
                reversed: true,
                items: vec![ordered(None, None, "два"), ordered(None, None, "один")],
                start: None,
                r#type: None,
            }),
        ]);
        assert_eq!(
            text,
            "- балкон\n- [x] парковка\n- [ ] техника:\n  кроме\n  - стиральная машина\n\
             3. первый\n4. второй\nIV. четвертый\n9. девятый\n7. из блоков\n\
             2. два\n1. один"
        );
    }

    #[test]
    fn media_blocks_are_markers_with_their_caption() {
        let photo = |id, text: &str| {
            B::Photo(types::PageBlockPhoto {
                spoiler: false,
                photo_id: id,
                caption: caption(text),
                url: None,
                webpage_id: None,
            })
        };
        let text = flat(vec![
            photo(1, "Кухня"),
            B::Video(types::PageBlockVideo {
                autoplay: false,
                r#loop: false,
                spoiler: false,
                video_id: 2,
                caption: caption(""),
            }),
            B::Audio(types::PageBlockAudio {
                audio_id: 3,
                caption: caption("гимн"),
            }),
            B::Document(types::PageBlockDocument {
                document_id: 4,
                caption: caption("договор.pdf"),
            }),
            B::Embed(types::PageBlockEmbed {
                full_width: false,
                allow_scrolling: false,
                url: Some("https://example.com/embed".into()),
                html: None,
                poster_photo_id: None,
                w: None,
                h: None,
                caption: caption(""),
            }),
            B::Map(types::PageBlockMap {
                geo: tl::enums::GeoPoint::Empty,
                zoom: 15,
                w: 100,
                h: 100,
                caption: caption("здесь"),
            }),
            B::Collage(types::PageBlockCollage {
                items: vec![photo(5, ""), photo(6, "")],
                caption: caption("коллаж"),
            }),
            B::Slideshow(types::PageBlockSlideshow {
                items: vec![photo(7, "слайд")],
                caption: caption(""),
            }),
            B::Cover(Box::new(types::PageBlockCover {
                cover: photo(8, "обложка"),
            })),
            B::EmbedPost(types::PageBlockEmbedPost {
                url: "https://example.com/post".into(),
                webpage_id: 0,
                author_photo_id: 0,
                author: "Автор".into(),
                date: 0,
                blocks: vec![paragraph("текст поста")],
                caption: caption(""),
            }),
            B::Channel(types::PageBlockChannel {
                channel: tl::enums::Chat::Empty(types::ChatEmpty { id: 9 }),
            }),
        ]);
        assert_eq!(
            text,
            "[photo] Кухня\n[video]\n[audio] гимн\n[file] договор.pdf\n[media]\n\
             [location] здесь\n[2 photos]\nколлаж\n[photo] слайд\n\
             [photo] обложка\nАвтор\nтекст поста\n[channel]"
        );
    }

    fn photo(text: &str) -> B {
        B::Photo(types::PageBlockPhoto {
            spoiler: false,
            photo_id: 1,
            caption: caption(text),
            url: None,
            webpage_id: None,
        })
    }

    fn photos(n: usize) -> Vec<B> {
        (0..n).map(|_| photo("")).collect()
    }

    #[test]
    fn a_run_of_the_same_media_without_captions_is_one_line_with_a_count() {
        let collage = |items, text: &str| {
            B::Collage(types::PageBlockCollage {
                items,
                caption: caption(text),
            })
        };
        // An album: a collage, a slideshow, or photo blocks in a row.
        assert_eq!(flat(vec![collage(photos(10), "")]), "[10 photos]");
        assert_eq!(flat(photos(10)), "[10 photos]");
        assert_eq!(
            flat(vec![B::Slideshow(types::PageBlockSlideshow {
                items: photos(3),
                caption: caption("слайды"),
            })]),
            "[3 photos]\nслайды"
        );
        // One is still singular; back-to-back collages are one run.
        assert_eq!(flat(photos(1)), "[photo]");
        assert_eq!(
            flat(vec![collage(photos(2), ""), collage(photos(3), "")]),
            "[5 photos]"
        );
        let video = || {
            B::Video(types::PageBlockVideo {
                autoplay: false,
                r#loop: false,
                spoiler: false,
                video_id: 2,
                caption: caption(""),
            })
        };
        let file = || {
            B::Document(types::PageBlockDocument {
                document_id: 4,
                caption: caption(""),
            })
        };
        assert_eq!(
            flat(vec![video(), video(), video(), file(), file(), photo("")]),
            "[3 videos]\n[2 files]\n[photo]"
        );
    }

    #[test]
    fn a_captioned_marker_or_other_text_breaks_a_run() {
        assert_eq!(
            flat(vec![
                photo(""),
                photo(""),
                photo("Кухня"),
                photo(""),
                photo("Спальня"),
                photo("Спальня"),
                paragraph("текст"),
                photo(""),
                // A spacer with nothing visible is no text, nor no caption.
                paragraph("\u{2800}"),
                photo("\u{2800}"),
            ]),
            "[2 photos]\n[photo] Кухня\n[photo]\n[photo] Спальня\n[photo] Спальня\n\
             текст\n[2 photos]"
        );
        // Each list item keeps its own marker.
        let item = |blocks| {
            tl::enums::PageListItem::Blocks(types::PageListItemBlocks {
                checkbox: false,
                checked: false,
                blocks,
            })
        };
        assert_eq!(
            flat(vec![
                B::List(types::PageBlockList {
                    items: vec![item(photos(2)), item(photos(1))],
                }),
                photo(""),
            ]),
            "- [2 photos]\n- [photo]\n[photo]"
        );
    }

    #[test]
    fn lines_with_nothing_visible_are_left_out() {
        assert_eq!(
            flat(vec![
                paragraph("до\n\u{2800}\n\u{200B}\u{3164}\nпосле"),
                paragraph("\u{2800}"),
                paragraph("конец"),
            ]),
            "до\nпосле\nконец"
        );
    }

    #[test]
    fn text_blocks_are_lines_and_blocks_without_text_are_left_out() {
        let t = plain;
        let text = flat(vec![
            B::Unsupported,
            B::Title(types::PageBlockTitle { text: t("title") }),
            B::Subtitle(types::PageBlockSubtitle {
                text: t("subtitle"),
            }),
            B::Kicker(types::PageBlockKicker { text: t("kicker") }),
            B::AuthorDate(types::PageBlockAuthorDate {
                author: t("author"),
                published_date: 1_700_000_000,
            }),
            B::Header(types::PageBlockHeader { text: t("header") }),
            B::Subheader(types::PageBlockSubheader {
                text: t("subheader"),
            }),
            B::Heading1(types::PageBlockHeading1 { text: t("h1") }),
            B::Heading2(types::PageBlockHeading2 { text: t("h2") }),
            B::Heading3(types::PageBlockHeading3 { text: t("h3") }),
            B::Heading4(types::PageBlockHeading4 { text: t("h4") }),
            B::Heading5(types::PageBlockHeading5 { text: t("h5") }),
            B::Heading6(types::PageBlockHeading6 { text: t("h6") }),
            B::Divider,
            B::Anchor(types::PageBlockAnchor { name: "a1".into() }),
            B::Preformatted(types::PageBlockPreformatted {
                text: t("fn main() {\n    run();\n}"),
                language: "rust".into(),
            }),
            B::Math(types::PageBlockMath {
                source: "e = mc^2".into(),
            }),
            B::Thinking(types::PageBlockThinking {
                text: t("thinking"),
            }),
            B::ButtonRow(types::PageBlockButtonRow {
                align_left: false,
                align_center: false,
                align_right: false,
                buttons: Vec::new(),
            }),
            B::RelatedArticles(types::PageBlockRelatedArticles {
                title: t("related"),
                articles: vec![tl::enums::PageRelatedArticle::Article(
                    types::PageRelatedArticle {
                        url: "https://example.com/a".into(),
                        webpage_id: 0,
                        title: Some("article".into()),
                        description: None,
                        photo_id: None,
                        author: None,
                        published_date: None,
                    },
                )],
            }),
            paragraph(""),
            paragraph("  "),
            B::Footer(types::PageBlockFooter { text: t("footer") }),
        ]);
        assert_eq!(
            text,
            "title\nsubtitle\nkicker\nauthor\nheader\nsubheader\nh1\nh2\nh3\nh4\nh5\nh6\n\
             fn main() {\n    run();\n}\ne = mc^2\nthinking\nrelated\n- article\nfooter"
        );
    }

    #[test]
    fn every_text_style_reads_as_its_text() {
        let p = plain;
        let text = concat(vec![
            T::TextEmpty,
            bold(p("bold ")),
            T::TextItalic(Box::new(types::TextItalic { text: p("italic ") })),
            T::TextUnderline(Box::new(types::TextUnderline { text: p("under ") })),
            T::TextStrike(Box::new(types::TextStrike { text: p("strike ") })),
            T::TextFixed(Box::new(types::TextFixed { text: p("fixed ") })),
            T::TextUrl(Box::new(types::TextUrl {
                text: p("link "),
                url: "https://example.com".into(),
                webpage_id: 0,
            })),
            T::TextEmail(Box::new(types::TextEmail {
                text: p("mail "),
                email: "a@example.com".into(),
            })),
            T::TextSubscript(Box::new(types::TextSubscript { text: p("sub ") })),
            T::TextSuperscript(Box::new(types::TextSuperscript { text: p("sup ") })),
            T::TextMarked(Box::new(types::TextMarked { text: p("marked ") })),
            T::TextPhone(Box::new(types::TextPhone {
                text: p("phone "),
                phone: "+995".into(),
            })),
            T::TextImage(types::TextImage {
                document_id: 1,
                w: 16,
                h: 16,
            }),
            T::TextAnchor(Box::new(types::TextAnchor {
                text: p("anchor "),
                name: "n".into(),
            })),
            T::TextMath(types::TextMath {
                source: "x^2 ".into(),
            }),
            T::TextCustomEmoji(types::TextCustomEmoji {
                document_id: 2,
                alt: "🔥 ".into(),
            }),
            T::TextSpoiler(Box::new(types::TextSpoiler {
                text: p("spoiler "),
            })),
            T::TextMention(Box::new(types::TextMention { text: p("@who ") })),
            T::TextHashtag(Box::new(types::TextHashtag { text: p("#tag ") })),
            T::TextBotCommand(Box::new(types::TextBotCommand { text: p("/start ") })),
            T::TextCashtag(Box::new(types::TextCashtag { text: p("$USD ") })),
            T::TextAutoUrl(Box::new(types::TextAutoUrl {
                text: p("example.com "),
            })),
            T::TextAutoEmail(Box::new(types::TextAutoEmail {
                text: p("b@example.com "),
            })),
            T::TextAutoPhone(Box::new(types::TextAutoPhone { text: p("+1 ") })),
            T::TextBankCard(Box::new(types::TextBankCard { text: p("4242 ") })),
            T::TextMentionName(Box::new(types::TextMentionName {
                text: p("Name "),
                user_id: 1,
            })),
            T::TextDate(Box::new(types::TextDate {
                relative: false,
                short_time: false,
                long_time: false,
                short_date: true,
                long_date: false,
                day_of_week: false,
                text: p("7 Nov "),
                date: 1_700_000_000,
            })),
            T::TextDiff(Box::new(types::TextDiff {
                text: p("new "),
                old_text: p("old "),
            })),
            T::TextButton(Box::new(types::TextButton {
                text: p("button "),
                r#type: tl::enums::InlineButtonType::Game,
                style: None,
            })),
            T::TextTonAddress(Box::new(types::TextTonAddress { text: p("UQ") })),
        ]);
        assert_eq!(
            flat(vec![B::Paragraph(types::PageBlockParagraph { text })]),
            "bold italic under strike fixed link mail sub sup marked phone anchor x^2 🔥 \
             spoiler @who #tag /start $USD example.com b@example.com +1 4242 Name 7 Nov \
             new button UQ"
        );
    }

    #[test]
    fn nesting_deeper_than_the_caps_is_left_out() {
        // Section 1 holds its line and section 2, and so on.
        let mut block = details("section 12", vec![paragraph("line 12")]);
        for n in (1..12).rev() {
            block = details(
                &format!("section {n}"),
                vec![paragraph(&format!("line {n}")), block],
            );
        }
        let text = flat(vec![block]);
        // Section n is at depth n - 1 and its line at depth n.
        let last = format!("line {}\nsection {MAX_DEPTH}", MAX_DEPTH - 1);
        assert!(text.ends_with(&last), "{text}");

        let mut deep = plain("too deep");
        for _ in 0..MAX_TEXT_DEPTH {
            deep = bold(deep);
        }
        let mut shallow = plain("deep enough");
        for _ in 0..MAX_TEXT_DEPTH - 1 {
            shallow = bold(shallow);
        }
        let text = flat(vec![
            B::Paragraph(types::PageBlockParagraph { text: deep }),
            B::Paragraph(types::PageBlockParagraph { text: shallow }),
        ]);
        assert_eq!(text, "deep enough");
    }

    #[test]
    fn a_long_rich_message_is_cut_and_marked() {
        let rows = (1..=2000)
            .map(|n| {
                row(vec![
                    cell(false, Some(plain(&format!("Квартира {n}")))),
                    cell(false, Some(plain(&format!("{n}00 $")))),
                ])
            })
            .collect();
        let text = flat(vec![table(T::TextEmpty, rows), paragraph("после")]);
        assert!(text.starts_with("Квартира 1 | 100 $\nКвартира 2 | 200 $\n"));
        let (kept, more) = text.rsplit_once('\n').unwrap();
        assert_eq!(more, MORE);
        assert_eq!(kept.chars().count(), MAX_CHARS);
        assert!(!text.contains("после"));
    }

    #[test]
    fn a_partial_rich_message_says_it_goes_on() {
        let mut part = rich(vec![paragraph("начало")]);
        let tl::enums::RichMessage::Message(m) = &mut part;
        m.part = true;
        assert_eq!(flatten(&part), format!("начало\n{MORE}"));
    }

    #[test]
    fn message_comes_first_and_the_rich_text_after_it() {
        let listing = rich(vec![paragraph("из блоков")]);
        assert_eq!(
            message_text("подпись".into(), Some(&listing)),
            "подпись\n\nиз блоков"
        );
        assert_eq!(
            message_text("\u{200B} ".into(), Some(&listing)),
            "из блоков"
        );
        assert_eq!(
            message_text("подпись".into(), Some(&rich(Vec::new()))),
            "подпись"
        );
        assert_eq!(message_text("подпись".into(), None), "подпись");
        assert_eq!(message_text(String::new(), None), "");
    }
}
