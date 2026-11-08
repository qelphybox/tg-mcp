//! EXPERIMENTAL, off by default (#58 prototype): a document's content for
//! get_media mode `document`, for clients without a shell.
//!
//! This exists to answer one question: does a client (claude.ai first) hand
//! an MCP embedded resource in a tool result to the model? It is not the
//! design of #58 and may be removed or rewritten once that is answered.
//!
//! [`fetch`] finds the message's file with [`Telegram::media_file`] (as
//! `download` does) and reads all of it with [`Download::next_part`]: one
//! `upload.getFile` per part, through the Account's limiter (method class
//! Files). Never grammers' `iter_download`. A PDF comes back as its bytes; a
//! plain-text file (`text/*`, JSON, CSV, Markdown, by MIME type or, without a
//! telling one, by extension) as its text, decoded lossily as UTF-8.
//! Everything else, and anything over [`MAX_DOCUMENT_BYTES`], is the caller's
//! error, pointing to `download`. Nothing is parsed or converted.
//!
//! The text is content written by other people: [`fetch`] is only reached
//! through [`super::sanitise::Sanitised::document`], which sanitises it (and
//! the file name and MIME type) like message text before any tool sees it.

use std::{fmt, sync::Arc};

use super::{
    Telegram, TgError,
    chats::ChatRef,
    download::{Download, MediaFile},
    messages::{Media, MediaKind},
};

/// The largest document this prototype reads into a tool result.
pub const MAX_DOCUMENT_BYTES: u64 = 5 * 1024 * 1024;

/// Extensions read as text when the MIME type says nothing (missing, or
/// `application/octet-stream`).
const TEXT_EXTENSIONS: [&str; 6] = ["txt", "text", "csv", "md", "markdown", "json"];

/// How a document is handed to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocumentContent {
    /// A PDF, as its bytes.
    Pdf(Vec<u8>),
    /// A plain-text file, as its text (lossy UTF-8) with its MIME type:
    /// the file's own when it is `text/*` or `application/json`, else
    /// `text/plain`.
    Text { mime_type: String, text: String },
}

/// A message's document and its content. `Debug` shows sizes only.
#[derive(Clone, PartialEq, Eq)]
pub struct Document {
    /// What get_messages says of it; `file_name` and `mime_type` are the
    /// sender's (sanitised by the time a tool sees them).
    pub media: Media,
    /// The file's size in bytes.
    pub size: u64,
    pub content: DocumentContent,
}

impl fmt::Debug for Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (delivered_as, len) = match &self.content {
            DocumentContent::Pdf(bytes) => ("blob", bytes.len()),
            DocumentContent::Text { text, .. } => ("text", text.len()),
        };
        f.debug_struct("Document")
            .field("size", &self.size)
            .field("delivered_as", &delivered_as)
            .field("content_len", &len)
            .finish_non_exhaustive()
    }
}

/// What a document of `media` would be delivered as, without fetching it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delivery {
    Pdf,
    Text,
}

/// PDF by MIME type or extension; text by `text/*` or JSON MIME type, or
/// by extension when the MIME type says nothing. None for anything else.
fn delivery(media: &Media) -> Option<Delivery> {
    let mime = media
        .mime_type
        .as_deref()
        .map(|m| m.trim().to_ascii_lowercase())
        .filter(|m| !m.is_empty());
    let extension = media
        .file_name
        .as_deref()
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, ext)| ext.trim().to_ascii_lowercase());
    match mime.as_deref() {
        Some("application/pdf") => return Some(Delivery::Pdf),
        Some(m) if m.starts_with("text/") || m == "application/json" => {
            return Some(Delivery::Text);
        }
        None | Some("application/octet-stream") => {}
        Some(_) => return None,
    }
    match extension.as_deref() {
        Some("pdf") => Some(Delivery::Pdf),
        Some(ext) if TEXT_EXTENSIONS.contains(&ext) => Some(Delivery::Text),
        _ => None,
    }
}

/// The MIME type a text document's resource carries.
fn text_mime_type(media: &Media) -> String {
    media
        .mime_type
        .as_deref()
        .map(|m| m.trim().to_ascii_lowercase())
        .filter(|m| {
            (m.starts_with("text/") || m == "application/json")
                && m.len() <= 64
                && m.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || b"/!#$&^_.+-".contains(&b)
                })
        })
        .unwrap_or_else(|| "text/plain".into())
}

/// The content of message `message_id`'s document in `chat`, fetched whole
/// through `telegram` (the raw Live client or the fake: only
/// [`super::sanitise::Sanitised::document`] calls this, and sanitises the
/// result).
pub async fn fetch(
    telegram: Arc<dyn Telegram>,
    chat: ChatRef,
    message_id: i32,
) -> Result<Document, TgError> {
    let file = telegram.media_file(chat, message_id).await?;
    let delivery = check(message_id, &file)?;
    let mut download = Download::new(telegram, chat, message_id, &file);
    let mut bytes = Vec::with_capacity(file.size as usize);
    while let Some(part) = download.next_part().await? {
        bytes.extend(part);
    }
    let content = match delivery {
        Delivery::Pdf => DocumentContent::Pdf(bytes),
        Delivery::Text => DocumentContent::Text {
            mime_type: text_mime_type(&file.media),
            text: String::from_utf8_lossy(&bytes).into_owned(),
        },
    };
    Ok(Document {
        media: file.media,
        size: file.size,
        content,
    })
}

/// Whether `file` is a document this mode reads, and how; the error says
/// what to do instead.
fn check(message_id: i32, file: &MediaFile) -> Result<Delivery, TgError> {
    let kind = file.media.kind;
    if kind != MediaKind::Document {
        return Err(TgError::InvalidInput(format!(
            "message {message_id}'s media is a {}, not a document: mode document reads PDF and \
             plain-text files only; use mode download (or view / transcribe where they apply)",
            kind.name()
        )));
    }
    let Some(delivery) = delivery(&file.media) else {
        return Err(TgError::InvalidInput(format!(
            "message {message_id}'s document is neither a PDF nor a plain-text file (text/*, \
             JSON, CSV, Markdown): mode document can't read it; use mode download"
        )));
    };
    if file.size > MAX_DOCUMENT_BYTES {
        return Err(TgError::InvalidInput(format!(
            "message {message_id}'s document is {} bytes; mode document reads files up to 5 MB \
             ({MAX_DOCUMENT_BYTES} bytes); use mode download",
            file.size
        )));
    }
    Ok(delivery)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(mime: Option<&str>, name: Option<&str>) -> Media {
        Media {
            mime_type: mime.map(Into::into),
            file_name: name.map(Into::into),
            ..Media::from(MediaKind::Document)
        }
    }

    #[test]
    fn pdfs_and_plain_text_are_told_by_mime_type_or_extension() {
        let cases = [
            (Some("application/pdf"), None, Some(Delivery::Pdf)),
            (Some("Application/PDF"), Some("x.bin"), Some(Delivery::Pdf)),
            (None, Some("tickets.PDF"), Some(Delivery::Pdf)),
            (
                Some("application/octet-stream"),
                Some("a.pdf"),
                Some(Delivery::Pdf),
            ),
            (Some("text/plain"), None, Some(Delivery::Text)),
            (Some("text/csv"), Some("a.csv"), Some(Delivery::Text)),
            (Some("text/markdown"), None, Some(Delivery::Text)),
            (Some("application/json"), None, Some(Delivery::Text)),
            (None, Some("notes.md"), Some(Delivery::Text)),
            (
                Some("application/octet-stream"),
                Some("a.json"),
                Some(Delivery::Text),
            ),
            // A MIME type that says what it is wins over the extension.
            (Some("application/zip"), Some("a.txt"), None),
            (Some("image/png"), Some("a.pdf"), None),
            (None, Some("archive.zip"), None),
            (None, None, None),
        ];
        for (mime, name, expected) in cases {
            assert_eq!(delivery(&doc(mime, name)), expected, "{mime:?} {name:?}");
        }
    }

    #[test]
    fn a_text_resource_carries_the_files_text_mime_type_or_text_plain() {
        assert_eq!(
            text_mime_type(&doc(Some("text/csv"), None)),
            "text/csv".to_string()
        );
        assert_eq!(
            text_mime_type(&doc(Some("application/json"), None)),
            "application/json".to_string()
        );
        for mime in [None, Some("application/octet-stream"), Some("text/a b")] {
            assert_eq!(text_mime_type(&doc(mime, Some("a.md"))), "text/plain");
        }
    }
}
