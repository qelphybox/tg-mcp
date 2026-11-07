//! Showing the model the picture of a message (#34), independent of grammers/TL.
//!
//! [`locate_album`] finds the message and, when it is in an album (Telegram's
//! `grouped_id`), the album's other messages, with one `getMessages` call
//! over the ids around it: an album has at most 10 messages with
//! consecutive ids, so [`ALBUM_REACH`] on either side holds all of it.
//! Outside channels those ids count across all of the Account's private
//! chats and basic groups, so only the asked Chat's messages are kept, as a
//! reply chain does. Each message comes with its [`Picture`]: the sizes
//! Telegram keeps of a photo, or the thumbnails of a document (video, video
//! note, GIF, sticker, file).
//!
//! [`view`] then picks one size per picture ([`choices`]: Telegram's own
//! JPEG sizes by their type letter, never an inline one), fits the pictures
//! into [`MAX_VIEW_BYTES`] ([`plan`]: smaller sizes first, then fewer album
//! items), downloads each through [`Download`] (`upload.getFile` in method
//! class Files, as get_media `download` does) and names its image type from
//! its bytes ([`image_type`]). Nothing is decoded, converted or kept: the
//! bytes go out as Telegram sent them.

use std::{future::Future, sync::Arc};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    Telegram, TgError,
    chats::ChatRef,
    download::{Download, MediaFile},
    media::{FileLocation, StoredFile},
    messages::{Media, MediaKind, MessageAt},
};

/// Image bytes one view may carry (before base64), all images together.
pub const MAX_VIEW_BYTES: u64 = 1024 * 1024;

/// Images one view shows at most.
pub const MAX_VIEW_IMAGES: usize = 4;

/// Ids fetched on either side of the asked message to find its album.
/// Telegram's albums have at most 10 messages, with consecutive ids.
pub const ALBUM_REACH: i32 = 9;

/// The longest side of Telegram's `x` size; without one, the standard view
/// takes the largest size that fits in it.
const STANDARD_SIDE: u32 = 800;

/// Which size of a picture to show.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ViewSize {
    /// Telegram's `x` size, about 800 px on the longer side.
    #[default]
    Standard,
    /// Telegram's `y` size, about 1280 px, when the photo has one.
    Large,
}

/// How Telegram keeps one size of a photo, or one thumbnail of a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SizeForm {
    /// `photoSize`: a file of its own (a JPEG; some thumbnails are WebP).
    File,
    /// `photoSizeProgressive`: a progressive JPEG, whole at its largest size.
    Progressive,
    /// `photoCachedSize`: tiny, its bytes inline in the message.
    Cached,
    /// `photoStrippedSize` (type `i`): a JPEG without its header, inline; no
    /// image as it is.
    Stripped,
    /// `photoPathSize` (type `j`): a sticker's outline as an SVG path, inline.
    Path,
}

/// One size of a photo, or one thumbnail of a document, as the message
/// describes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PictureSize {
    /// Telegram's name for it (`s`, `m`, `x`, `y`, `w`, `i`, `j`, …): what
    /// `upload.getFile` asks for.
    pub r#type: String,
    pub form: SizeForm,
    /// Pixels; 0 when Telegram doesn't say (inline forms).
    pub w: u32,
    pub h: u32,
    /// The file's size: a progressive size's largest, an inline one's bytes.
    pub bytes: u64,
}

impl PictureSize {
    /// A size `upload.getFile` serves, with a size to download exactly.
    fn fetchable(&self) -> bool {
        matches!(self.form, SizeForm::File | SizeForm::Progressive) && self.bytes > 0
    }

    fn longest_side(&self) -> u32 {
        self.w.max(self.h)
    }

    fn pixels(&self) -> u64 {
        u64::from(self.w) * u64::from(self.h)
    }
}

/// What can be shown of a message's media: a photo's sizes or a document's
/// thumbnails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picture {
    /// The photo ([`StoredFile::Photo`]) or the document
    /// ([`StoredFile::DocumentThumb`]), its `thumb_size` left empty:
    /// [`Picture::at`] names one.
    pub location: FileLocation,
    pub sizes: Vec<PictureSize>,
}

impl Picture {
    /// Where size `size` of this picture is.
    pub fn at(&self, size: &PictureSize) -> FileLocation {
        let thumb_size = size.r#type.clone();
        FileLocation {
            file: match self.location.file {
                StoredFile::Photo { .. } => StoredFile::Photo { thumb_size },
                StoredFile::Document | StoredFile::DocumentThumb { .. } => {
                    StoredFile::DocumentThumb { thumb_size }
                }
            },
            ..self.location.clone()
        }
    }
}

/// One message as a `getMessages` call found it, reduced to what a view
/// needs.
#[derive(Debug, Clone)]
pub struct RawViewMessage {
    pub at: MessageAt,
    /// Telegram's album id: messages sent together share it.
    pub grouped_id: Option<i64>,
    pub media: Option<Media>,
    pub picture: Option<Picture>,
}

/// The asked message and the rest of its album, if it is in one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Album {
    /// The message asked for.
    pub asked: i32,
    /// In message order, the asked one included; only it when it is in no
    /// album.
    pub items: Vec<AlbumItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumItem {
    pub message_id: i32,
    pub media: Media,
    pub picture: Option<Picture>,
}

/// The ids one album lookup asks for: [`ALBUM_REACH`] on either side of
/// `id`, positive ones only.
pub fn album_ids(id: i32) -> Vec<i32> {
    (id.saturating_sub(ALBUM_REACH).max(1)..=id.saturating_add(ALBUM_REACH)).collect()
}

/// Message `id` of `chat` and the other messages of its album, with one
/// `get_messages` call for [`album_ids`]. Messages of other Chats (ids
/// outside channels are the Account's, not the Chat's) and of other albums
/// are left out. A message that isn't there or has no media is the
/// caller's to fix.
pub async fn locate_album<G, Fut>(chat: ChatRef, id: i32, get_messages: G) -> Result<Album, TgError>
where
    G: FnOnce(Vec<i32>) -> Fut,
    Fut: Future<Output = Result<Vec<RawViewMessage>, TgError>>,
{
    let found = get_messages(album_ids(id)).await?;
    let ours: Vec<RawViewMessage> = found
        .into_iter()
        .filter(|m| m.at.chat == chat.chat())
        .collect();
    let asked = ours.iter().find(|m| m.at.id == id).ok_or_else(|| {
        TgError::InvalidInput(format!(
            "message {id} not found in this chat (deleted, or the id is from another chat)"
        ))
    })?;
    if asked.media.is_none() {
        return Err(TgError::InvalidInput(format!(
            "message {id} has no media: get_messages shows its text"
        )));
    }
    let group = asked.grouped_id;
    let mut items: Vec<AlbumItem> = ours
        .into_iter()
        .filter(|m| m.at.id == id || (group.is_some() && m.grouped_id == group))
        .filter_map(|m| {
            Some(AlbumItem {
                message_id: m.at.id,
                media: m.media?,
                picture: m.picture,
            })
        })
        .collect();
    items.sort_by_key(|item| item.message_id);
    items.dedup_by_key(|item| item.message_id);
    Ok(Album { asked: id, items })
}

/// The sizes of `item`'s picture a view may show, the one `want` asks for
/// first, then the smaller ones by bytes, largest first; or why there is
/// nothing to show.
pub fn choices(item: &AlbumItem, want: ViewSize) -> Result<Vec<&PictureSize>, String> {
    let id = item.message_id;
    let what = match item.media.kind {
        MediaKind::Photo => "photo",
        MediaKind::Video => "video",
        MediaKind::VideoNote => "video message",
        MediaKind::Gif => "GIF",
        MediaKind::Sticker => "sticker",
        MediaKind::Document => "file",
        MediaKind::Voice | MediaKind::Audio => {
            return Err(format!(
                "message {id} is audio: there is no picture to view; mode download gets the file"
            ));
        }
        MediaKind::Location | MediaKind::Contact | MediaKind::Poll | MediaKind::Other => {
            return Err(format!(
                "message {id}'s media has no picture to view; get_messages describes it"
            ));
        }
    };
    let fetchable: Vec<&PictureSize> = item
        .picture
        .iter()
        .flat_map(|p| &p.sizes)
        .filter(|s| s.fetchable())
        .collect();
    let Some(preferred) = preferred(&fetchable, want) else {
        // What Telegram did send, to tell "no thumbnail" from one in a form
        // we don't take: Telegram's size names, forms and sizes only.
        tracing::info!(
            message_id = id,
            kind = item.media.kind.name(),
            sizes = %sizes_summary(item),
            "media view found no fetchable picture size"
        );
        return Err(match item.media.kind {
            MediaKind::Photo => format!(
                "message {id}'s photo is no longer available (e.g. a self-destructing one that \
                 expired)"
            ),
            MediaKind::Document => format!(
                "message {id}'s file has no thumbnail to view; mode download gets the file itself"
            ),
            _ => format!(
                "Telegram has no thumbnail for message {id}'s {what}; mode download gets the file"
            ),
        });
    };
    let mut smaller: Vec<&PictureSize> = fetchable
        .into_iter()
        .filter(|s| s.bytes < preferred.bytes)
        .collect();
    smaller.sort_by_key(|s| std::cmp::Reverse(s.bytes));
    Ok(std::iter::once(preferred).chain(smaller).collect())
}

/// `item`'s picture sizes for a log line, e.g. `i:Stripped:0x0:41b,m:File:320x180:0b`,
/// or `none` when its media has no picture at all. Size names are cut to
/// what Telegram uses (a few ASCII letters), so nothing else can get in.
fn sizes_summary(item: &AlbumItem) -> String {
    let Some(picture) = &item.picture else {
        return "none".into();
    };
    picture
        .sizes
        .iter()
        .take(12)
        .map(|s| {
            let name: String = s
                .r#type
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .take(4)
                .collect();
            format!("{name}:{:?}:{}x{}:{}b", s.form, s.w, s.h, s.bytes)
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The size `want` asks for: `y` for a large view when there is one, else
/// `x`, else the largest that fits in `x`'s box, else the smallest.
fn preferred<'a>(sizes: &[&'a PictureSize], want: ViewSize) -> Option<&'a PictureSize> {
    let named = |name: &str| sizes.iter().copied().find(|s| s.r#type == name);
    if want == ViewSize::Large
        && let Some(large) = named("y")
    {
        return Some(large);
    }
    named("x")
        .or_else(|| {
            sizes
                .iter()
                .copied()
                .filter(|s| s.w > 0 && s.longest_side() <= STANDARD_SIDE)
                .max_by_key(|s| s.pixels())
        })
        .or_else(|| sizes.iter().copied().min_by_key(|s| s.bytes))
}

/// Which candidate of each item to show so that all of them together are at
/// most `cap` bytes: `None` for an item left out. `items` holds each item's
/// candidate sizes in bytes, largest first, as [`choices`] orders them.
/// Items are left out from the end, never `target`, only when even their
/// smallest sizes don't fit; then the largest current size is stepped down
/// until everything fits. `Err` is `target`'s smallest size when it alone
/// doesn't fit.
pub fn plan(items: &[Vec<u64>], target: usize, cap: u64) -> Result<Vec<Option<usize>>, u64> {
    let smallest = |i: usize| items[i].last().copied().unwrap_or(0);
    let mut kept = vec![true; items.len()];
    let mut least: u64 = (0..items.len()).map(smallest).sum();
    while least > cap {
        let Some(drop) = (0..items.len()).rev().find(|&i| kept[i] && i != target) else {
            return Err(smallest(target));
        };
        kept[drop] = false;
        least -= smallest(drop);
    }
    let mut choice = vec![0usize; items.len()];
    let total = |choice: &[usize]| -> u64 {
        (0..items.len())
            .filter(|&i| kept[i])
            .map(|i| items[i][choice[i]])
            .sum()
    };
    while total(&choice) > cap {
        let step = (0..items.len())
            .filter(|&i| kept[i] && choice[i] + 1 < items[i].len())
            .max_by_key(|&i| (items[i][choice[i]], std::cmp::Reverse(i)))
            .expect("the smallest sizes fit, so some size can step down");
        choice[step] += 1;
    }
    Ok((0..items.len())
        .map(|i| kept[i].then_some(choice[i]))
        .collect())
}

/// What kind of image `bytes` are, by their first bytes: the image types
/// MCP clients show. Anything else (an animated sticker's Lottie, a stripped
/// thumbnail) is none.
pub fn image_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else {
        None
    }
}

/// One image of a view.
#[derive(Clone, PartialEq, Eq)]
pub struct ViewImage {
    pub message_id: i32,
    pub kind: MediaKind,
    /// From the bytes ([`image_type`]), never from what the sender declared.
    pub mime_type: &'static str,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for ViewImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ViewImage")
            .field("message_id", &self.message_id)
            .field("kind", &self.kind)
            .field("mime_type", &self.mime_type)
            .field("bytes", &format_args!("<{} bytes>", self.bytes.len()))
            .finish_non_exhaustive()
    }
}

/// What a view shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    /// In message order.
    pub images: Vec<ViewImage>,
    /// Only when the message is in an album: all of its messages, in order.
    pub album: Option<Vec<i32>>,
    /// Some of the album isn't among `images`.
    pub album_truncated: bool,
}

/// The pictures of message `id` (and its album) of `chat`, `want`'s size,
/// fitted into [`MAX_VIEW_BYTES`] and [`MAX_VIEW_IMAGES`]. Everything goes
/// through `telegram`: one `getMessages`, then one [`Download`] per image.
pub async fn view(
    telegram: Arc<dyn Telegram>,
    chat: ChatRef,
    id: i32,
    want: ViewSize,
) -> Result<View, TgError> {
    let album = telegram.media_album(chat, id).await?;
    let asked = album
        .items
        .iter()
        .find(|item| item.message_id == album.asked)
        .ok_or_else(|| TgError::Upstream("the album lost the asked message".into()))?;
    choices(asked, want).map_err(TgError::InvalidInput)?;
    // Album items without a picture are left out.
    let viewable: Vec<(&AlbumItem, Vec<&PictureSize>)> = album
        .items
        .iter()
        .filter_map(|item| Some((item, choices(item, want).ok()?)))
        .collect();
    let target = viewable
        .iter()
        .position(|(item, _)| item.message_id == album.asked)
        .expect("the asked message has choices");
    // A window of the album in message order, with the asked message in it.
    let start = target.saturating_sub(MAX_VIEW_IMAGES - 1);
    let window = &viewable[start..(start + MAX_VIEW_IMAGES).min(viewable.len())];
    let sizes: Vec<Vec<u64>> = window
        .iter()
        .map(|(_, choices)| choices.iter().map(|s| s.bytes).collect())
        .collect();
    let picked = plan(&sizes, target - start, MAX_VIEW_BYTES).map_err(|smallest| {
        TgError::InvalidInput(format!(
            "message {id}'s smallest picture is {smallest} bytes, more than a view carries \
             ({MAX_VIEW_BYTES} bytes); mode download gets the file"
        ))
    })?;
    let mut images = Vec::new();
    for ((item, choices), pick) in window.iter().zip(picked) {
        let Some(pick) = pick else { continue };
        let size = choices[pick];
        let picture = item
            .picture
            .as_ref()
            .expect("an item with choices has a picture");
        let file = MediaFile {
            media: item.media.clone(),
            location: picture.at(size),
            size: size.bytes,
        };
        let mut download = Download::new(telegram.clone(), chat, item.message_id, &file);
        let mut bytes = Vec::with_capacity(size.bytes as usize);
        while let Some(part) = download.next_part().await? {
            bytes.extend(part);
        }
        let Some(mime_type) = image_type(&bytes) else {
            if item.message_id == album.asked {
                return Err(TgError::InvalidInput(format!(
                    "message {id}'s thumbnail is not an image clients can show (e.g. an \
                     animated sticker's); mode download gets the file"
                )));
            }
            continue;
        };
        let dimension = |pixels: u32| (pixels > 0).then_some(pixels);
        images.push(ViewImage {
            message_id: item.message_id,
            kind: item.media.kind,
            mime_type,
            width: dimension(size.w),
            height: dimension(size.h),
            bytes,
        });
    }
    let in_album = album.items.len() > 1;
    Ok(View {
        album_truncated: in_album && images.len() < album.items.len(),
        album: in_album.then(|| album.items.iter().map(|i| i.message_id).collect()),
        images,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tg::chats::ChatId;

    fn size(r#type: &str, form: SizeForm, w: u32, h: u32, bytes: u64) -> PictureSize {
        PictureSize {
            r#type: r#type.into(),
            form,
            w,
            h,
            bytes,
        }
    }

    fn item(kind: MediaKind, sizes: Vec<PictureSize>) -> AlbumItem {
        AlbumItem {
            message_id: 7,
            media: Media::from(kind),
            picture: Some(Picture {
                location: FileLocation {
                    file: StoredFile::Photo {
                        thumb_size: String::new(),
                    },
                    id: 1,
                    access_hash: 2,
                    file_reference: vec![3],
                    dc_id: 2,
                },
                sizes,
            }),
        }
    }

    /// A photo as Telegram sends one: stripped `i`, `m`, `x` and a
    /// progressive `y`.
    fn photo() -> Vec<PictureSize> {
        vec![
            size("i", SizeForm::Stripped, 0, 0, 1_000_000),
            size("m", SizeForm::File, 320, 240, 15_000),
            size("x", SizeForm::File, 800, 600, 70_000),
            size("y", SizeForm::Progressive, 1280, 960, 152_331),
        ]
    }

    fn types(choices: &[&PictureSize]) -> Vec<String> {
        choices.iter().map(|s| s.r#type.clone()).collect()
    }

    /// The types of the sizes a view of a photo of `sizes` may show.
    fn pick(sizes: Vec<PictureSize>, want: ViewSize) -> Vec<String> {
        types(&choices(&item(MediaKind::Photo, sizes), want).unwrap())
    }

    #[test]
    fn a_standard_view_is_x_and_a_large_one_y_then_smaller_sizes() {
        let photo = item(MediaKind::Photo, photo());
        let standard = choices(&photo, ViewSize::Standard).unwrap();
        assert_eq!(types(&standard), ["x", "m"]);
        let large = choices(&photo, ViewSize::Large).unwrap();
        assert_eq!(types(&large), ["y", "x", "m"]);
        assert_eq!(large[0].bytes, 152_331, "a progressive size's whole file");
    }

    #[test]
    fn inline_sizes_are_never_chosen() {
        // A huge stripped, cached or path size never wins, however it is
        // named or however small.
        for form in [SizeForm::Stripped, SizeForm::Cached, SizeForm::Path] {
            let sizes = vec![
                size("x", form, 800, 600, 50),
                size("y", form, 1280, 960, 60),
                size("m", SizeForm::File, 320, 240, 15_000),
            ];
            for want in [ViewSize::Standard, ViewSize::Large] {
                assert_eq!(pick(sizes.clone(), want), ["m"], "{form:?} {want:?}");
            }
        }
        // Only an inline size: nothing to view.
        let only_stripped = item(
            MediaKind::Video,
            vec![size("i", SizeForm::Stripped, 0, 0, 40)],
        );
        let error = choices(&only_stripped, ViewSize::Standard).unwrap_err();
        assert!(error.contains("no thumbnail"), "{error}");
    }

    #[test]
    fn without_x_the_largest_size_within_its_box_or_else_the_smallest() {
        // A small photo: its full size is `m`-sized, no `x`.
        let small = vec![
            size("s", SizeForm::File, 90, 67, 1_500),
            size("m", SizeForm::File, 320, 240, 15_000),
        ];
        assert_eq!(pick(small, ViewSize::Large), ["m", "s"]);
        // No `x`, but `y` and `w`: a standard view takes `m`, a large one `y`.
        let big = vec![
            size("m", SizeForm::File, 320, 240, 15_000),
            size("y", SizeForm::File, 1280, 960, 150_000),
            size("w", SizeForm::File, 2560, 1920, 600_000),
        ];
        assert_eq!(pick(big.clone(), ViewSize::Standard), ["m"]);
        assert_eq!(pick(big, ViewSize::Large), ["y", "m"]);
        // Only sizes larger than the box: the smallest.
        let huge = vec![
            size("w", SizeForm::File, 2560, 1920, 600_000),
            size("y", SizeForm::Progressive, 1280, 960, 150_000),
        ];
        assert_eq!(pick(huge, ViewSize::Standard), ["y"]);
    }

    #[test]
    fn a_missing_picture_is_logged_as_telegrams_sizes_only() {
        let gif = item(
            MediaKind::Gif,
            vec![
                size("i", SizeForm::Stripped, 0, 0, 41),
                size("m\u{202e}<x>", SizeForm::File, 320, 180, 0),
            ],
        );
        assert_eq!(sizes_summary(&gif), "i:Stripped:0x0:41b,mx:File:320x180:0b");
        let bare = AlbumItem {
            picture: None,
            ..item(MediaKind::Gif, vec![])
        };
        assert_eq!(sizes_summary(&bare), "none");
    }

    #[test]
    fn what_has_no_picture_says_so_and_what_to_do() {
        let none = |kind| AlbumItem {
            picture: None,
            ..item(kind, vec![])
        };
        let error = |kind| choices(&none(kind), ViewSize::Standard).unwrap_err();
        assert!(error(MediaKind::Photo).contains("no longer available"));
        for kind in [
            MediaKind::Video,
            MediaKind::VideoNote,
            MediaKind::Gif,
            MediaKind::Sticker,
        ] {
            let error = error(kind);
            assert!(
                error.contains("no thumbnail") && error.contains("mode download"),
                "{error}"
            );
        }
        assert!(error(MediaKind::Document).contains("has no thumbnail to view"));
        // Audio has no picture to show even with an album cover.
        for kind in [MediaKind::Voice, MediaKind::Audio] {
            let error = choices(&item(kind, photo()), ViewSize::Standard).unwrap_err();
            assert!(error.contains("is audio"), "{error}");
        }
        for kind in [MediaKind::Location, MediaKind::Poll, MediaKind::Other] {
            assert!(error(kind).contains("no picture to view"));
        }
    }

    #[test]
    fn the_cap_holds_exactly_its_bytes() {
        assert_eq!(plan(&[vec![100]], 0, 100), Ok(vec![Some(0)]));
        assert_eq!(plan(&[vec![101, 100]], 0, 100), Ok(vec![Some(1)]));
        assert_eq!(plan(&[vec![101]], 0, 100), Err(101));
        assert_eq!(
            plan(&[vec![60], vec![40]], 0, 100),
            Ok(vec![Some(0), Some(0)])
        );
        assert_eq!(plan(&[vec![60], vec![41]], 0, 100), Ok(vec![Some(0), None]));
    }

    #[test]
    fn smaller_sizes_come_before_fewer_images() {
        // Four x of 400 KB each: stepped down, largest first, until they fit.
        let four = vec![vec![400, 100, 10]; 4];
        let picked = plan(&four, 0, 1000).unwrap();
        assert!(picked.iter().all(Option::is_some), "{picked:?}");
        let total: u64 = picked
            .iter()
            .zip(&four)
            .map(|(p, sizes)| sizes[p.unwrap()])
            .sum();
        assert!(total <= 1000, "{total}");
        assert_eq!(picked, [Some(1), Some(1), Some(0), Some(0)]);
        // The smallest sizes don't all fit: the last ones go, never the
        // target, and the rest are as large as fits again.
        let tight = vec![
            vec![900, 300],
            vec![900, 300],
            vec![900, 300],
            vec![900, 300],
        ];
        assert_eq!(
            plan(&tight, 3, 1000),
            Ok(vec![Some(1), Some(1), None, Some(1)])
        );
        assert_eq!(
            plan(&tight, 0, 1000),
            Ok(vec![Some(1), Some(1), Some(1), None])
        );
        assert_eq!(
            plan(&tight, 0, 950),
            Ok(vec![Some(1), Some(1), Some(1), None])
        );
        assert_eq!(plan(&tight, 0, 899), Ok(vec![Some(1), Some(1), None, None]));
        assert_eq!(
            plan(&[vec![500], vec![900, 300]], 1, 600),
            Ok(vec![None, Some(1)])
        );
    }

    #[test]
    fn image_types_come_from_the_bytes() {
        assert_eq!(
            image_type(b"\xFF\xD8\xFF\xE0\0\x10JFIF"),
            Some("image/jpeg")
        );
        assert_eq!(image_type(b"\x89PNG\r\n\x1a\n\0\0"), Some("image/png"));
        assert_eq!(image_type(b"RIFF\x10\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(image_type(b"GIF89a\x01\0"), Some("image/gif"));
        for other in [
            &b"\x1F\x8B\x08\0"[..], // gzip: an animated sticker (TGS)
            b"\x1A\x45\xDF\xA3",    // WebM: a video sticker
            b"RIFF\x10\0\0\0WAVE",
            b"\xFF\xD8",
            b"",
            b"\x01\x28\x1e", // a stripped thumbnail: no JPEG header
        ] {
            assert_eq!(image_type(other), None, "{other:?}");
        }
    }

    fn raw(chat: ChatId, id: i32, grouped_id: Option<i64>) -> RawViewMessage {
        RawViewMessage {
            at: MessageAt { chat, id },
            grouped_id,
            media: Some(MediaKind::Photo.into()),
            picture: None,
        }
    }

    #[tokio::test]
    async fn an_album_is_its_own_chats_messages_of_its_group_in_order_from_one_call() {
        const ALICE: ChatId = ChatId::User(1);
        const BOB: ChatId = ChatId::User(2);
        let found = vec![
            raw(ALICE, 13, Some(5)),
            raw(ALICE, 11, Some(5)),
            // Bob's message among the ids, even of an album with the same id.
            raw(BOB, 12, Some(5)),
            raw(ALICE, 10, Some(4)),
            raw(ALICE, 14, None),
            raw(ALICE, 15, Some(5)),
        ];
        let mut asked_for = Vec::new();
        let album = locate_album(ALICE.into(), 13, |ids| {
            asked_for.push(ids);
            std::future::ready(Ok(found.clone()))
        })
        .await
        .unwrap();
        assert_eq!(asked_for, [(4..=22).collect::<Vec<_>>()]);
        let ids: Vec<i32> = album.items.iter().map(|i| i.message_id).collect();
        assert_eq!(ids, [11, 13, 15]);
        assert_eq!(album.asked, 13);

        // Not in an album: only it.
        let album = locate_album(ALICE.into(), 14, |_| std::future::ready(Ok(found.clone())))
            .await
            .unwrap();
        let ids: Vec<i32> = album.items.iter().map(|i| i.message_id).collect();
        assert_eq!(ids, [14]);
        // Bob's message isn't Alice's 12.
        let error = locate_album(ALICE.into(), 12, |_| std::future::ready(Ok(found.clone())))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("not found in this chat"),
            "{error}"
        );
    }

    #[test]
    fn album_ids_stay_positive() {
        assert_eq!(album_ids(1), (1..=10).collect::<Vec<_>>());
        assert_eq!(album_ids(100).len(), 19);
        assert_eq!(*album_ids(i32::MAX).last().unwrap(), i32::MAX);
    }
}
