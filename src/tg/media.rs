//! What media a message carries (#39), from TL.
//!
//! [`describe`] turns a message's `media` into a [`Media`]: its kind and the
//! metadata Telegram sends along with the message (duration, size, MIME type,
//! file name, dimensions). Nothing is downloaded, and nothing that would let
//! anyone download it leaves this module: ids, access hashes, file
//! references and DC ids stay in TL. Whoever needs the file itself (#36)
//! fetches the message again by Chat and id and takes it from there, with the
//! same [`document_kind`] deciding what kind of file it is.
//!
//! Every `MessageMedia` constructor of the layer is named in [`describe`],
//! without a wildcard, so a layer bump that adds one doesn't compile until it
//! is sorted into a kind.

use grammers_client::tl;

use super::{
    messages::{Media, MediaKind, Poll, PollOption},
    view::{Picture, PictureSize, SizeForm},
};

/// The media `media` describes; none for no media, `messageMediaEmpty` and a
/// link preview (`messageMediaWebPage`: the link is in the text).
pub fn describe(media: Option<&tl::enums::MessageMedia>) -> Option<Media> {
    use tl::enums::MessageMedia as M;
    let other = || Some(Media::from(MediaKind::Other));
    match media? {
        M::Empty | M::WebPage(_) => None,
        // A live photo's video is part of the photo.
        M::Photo(p) => Some(photo(p.photo.as_ref())),
        M::Document(d) => Some(document(d)),
        M::Geo(_) | M::GeoLive(_) | M::Venue(_) => Some(MediaKind::Location.into()),
        M::Contact(_) => Some(MediaKind::Contact.into()),
        M::Poll(p) => Some(poll_of(p)),
        M::Dice(_) => other(),
        M::Story(_) => other(),
        M::Game(_) => other(),
        M::Invoice(_) => other(),
        M::Giveaway(_) => other(),
        M::GiveawayResults(_) => other(),
        // Paid media is locked until paid for; what is inside isn't ours to say.
        M::PaidMedia(_) => other(),
        M::ToDo(_) => other(),
        M::VideoStream(_) => other(),
        // Media of a newer layer than this server's.
        M::Unsupported => other(),
    }
}

/// A poll, from the message alone (#62): its question and every option, and
/// the results as far as Telegram shows them to this Account. Cut to size
/// once sanitised, with the page (`bound_history`).
fn poll_of(media: &tl::types::MessageMediaPoll) -> Media {
    let tl::enums::Poll::Poll(poll) = &media.poll;
    let tl::enums::PollResults::Results(results) = &media.results;
    let tl::enums::TextWithEntities::Entities(question) = &poll.question;
    let shown: Vec<&tl::types::PollAnswerVoters> = results
        .results
        .iter()
        .flatten()
        .map(|tl::enums::PollAnswerVoters::Voters(v)| v)
        .collect();
    let answers: Vec<&tl::types::PollAnswer> = poll
        .answers
        .iter()
        .filter_map(|answer| match answer {
            tl::enums::PollAnswer::Answer(answer) => Some(answer),
            // Only sent by clients creating a poll.
            tl::enums::PollAnswer::InputPollAnswer(_) => None,
        })
        .collect();
    let options = answers
        .iter()
        .map(|answer| {
            let tl::enums::TextWithEntities::Entities(text) = &answer.text;
            let result = shown.iter().find(|v| v.option == answer.option);
            PollOption {
                text: text.text.clone(),
                votes: result.and_then(|v| v.voters).and_then(count),
                // Results the same for everyone (`min`) carry no one's vote.
                chosen: result.is_some_and(|v| v.chosen && !results.min),
                correct: poll.quiz && result.is_some_and(|v| v.correct),
            }
        })
        .collect();
    Media {
        poll: Some(Poll {
            question: question.text.clone(),
            options,
            options_truncated: false,
            total_voters: results.total_voters.and_then(count),
            closed: poll.closed,
            multiple_choice: poll.multiple_choice,
            quiz: poll.quiz,
            anonymous: !poll.public_voters,
        }),
        ..MediaKind::Poll.into()
    }
}

/// A count Telegram sends as `i32`; a negative one is none.
fn count(n: i32) -> Option<u32> {
    u32::try_from(n).ok()
}

/// A photo, with its largest size. A self-destructing photo that is gone
/// comes without one: only the kind is left.
fn photo(photo: Option<&tl::enums::Photo>) -> Media {
    let mut media = Media::from(MediaKind::Photo);
    let Some(tl::enums::Photo::Photo(photo)) = photo else {
        return media;
    };
    if let Some(largest) = largest_size(photo) {
        media.width = dimension(largest.w);
        media.height = dimension(largest.h);
        media.size_bytes = largest
            .size
            .and_then(|s| u64::try_from(s).ok())
            .filter(|&s| s > 0);
    }
    media
}

/// One size of a photo that is a file of its own (not an inline thumbnail).
struct PhotoSize<'a> {
    /// Telegram's name for the size (`m`, `x`, `y`, …): what getFile asks for.
    r#type: &'a str,
    w: i32,
    h: i32,
    /// Bytes; a progressive size's last (full) one.
    size: Option<i32>,
}

/// The largest size of `photo`, by pixels: what the photo is, and what
/// get_media downloads. Inline thumbnails (cached, stripped, path) are no
/// size of the photo.
fn largest_size(photo: &tl::types::Photo) -> Option<PhotoSize<'_>> {
    photo
        .sizes
        .iter()
        .filter_map(|size| match size {
            tl::enums::PhotoSize::Size(s) => Some(PhotoSize {
                r#type: &s.r#type,
                w: s.w,
                h: s.h,
                size: Some(s.size),
            }),
            tl::enums::PhotoSize::Progressive(s) => Some(PhotoSize {
                r#type: &s.r#type,
                w: s.w,
                h: s.h,
                size: s.sizes.iter().copied().max(),
            }),
            tl::enums::PhotoSize::Empty(_)
            | tl::enums::PhotoSize::PhotoCachedSize(_)
            | tl::enums::PhotoSize::PhotoStrippedSize(_)
            | tl::enums::PhotoSize::PhotoPathSize(_) => None,
        })
        .max_by_key(|s| i64::from(s.w) * i64::from(s.h))
}

/// Where a file Telegram keeps is and what it is, for `upload.getFile`
/// (#36). It stays on the server: its access hash and file reference let
/// whoever holds this Account's session fetch the file, so it is never
/// serialised, and `Debug` leaves them out.
#[derive(Clone, PartialEq, Eq)]
pub struct FileLocation {
    pub file: StoredFile,
    pub id: i64,
    pub access_hash: i64,
    /// Telegram's proof that this Account saw the file in a message; it
    /// expires, and fetching the message again gives a fresh one.
    pub file_reference: Vec<u8>,
    /// The DC the message says the file is in.
    pub dc_id: i32,
}

/// Which of Telegram's file stores a file is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredFile {
    /// A photo, at one of its sizes.
    Photo { thumb_size: String },
    /// A document: any other file.
    Document,
    /// One of a document's thumbnails (#34).
    DocumentThumb { thumb_size: String },
}

impl StoredFile {
    /// Photos and documents are numbered apart.
    fn is_photo(&self) -> bool {
        matches!(self, Self::Photo { .. })
    }
}

impl FileLocation {
    /// The same file (a fresh file reference doesn't make another one).
    pub fn same_file(&self, other: &FileLocation) -> bool {
        self.file == other.file && self.id == other.id
    }

    /// The same photo or document, at any of its sizes: one file reference
    /// is good for all of them.
    pub fn same_object(&self, other: &FileLocation) -> bool {
        self.file.is_photo() == other.file.is_photo() && self.id == other.id
    }
}

impl std::fmt::Debug for FileLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileLocation")
            .field("file", &self.file)
            .field("dc_id", &self.dc_id)
            .finish_non_exhaustive()
    }
}

/// The file of a message's media and its size in bytes, if it has one:
/// a photo (its largest size, as [`describe`] reports it) or a document.
/// The size is `None` when Telegram doesn't say it. Everything else
/// (locations, contacts, polls, …) and media that is gone has no file.
pub fn file(media: Option<&tl::enums::MessageMedia>) -> Option<(FileLocation, Option<u64>)> {
    use tl::enums::MessageMedia as M;
    match media? {
        M::Photo(p) => {
            let Some(tl::enums::Photo::Photo(photo)) = &p.photo else {
                return None;
            };
            let largest = largest_size(photo)?;
            Some((
                FileLocation {
                    file: StoredFile::Photo {
                        thumb_size: largest.r#type.to_string(),
                    },
                    id: photo.id,
                    access_hash: photo.access_hash,
                    file_reference: photo.file_reference.clone(),
                    dc_id: photo.dc_id,
                },
                largest.size.and_then(|s| u64::try_from(s).ok()),
            ))
        }
        M::Document(d) => {
            let Some(tl::enums::Document::Document(doc)) = &d.document else {
                return None;
            };
            Some((
                FileLocation {
                    file: StoredFile::Document,
                    id: doc.id,
                    access_hash: doc.access_hash,
                    file_reference: doc.file_reference.clone(),
                    dc_id: doc.dc_id,
                },
                u64::try_from(doc.size).ok(),
            ))
        }
        _ => None,
    }
}

/// What can be shown of a message's media (#34): a photo's sizes, or a
/// document's thumbnails, each as Telegram describes it, inline ones
/// included (a view never picks those). None for media without either.
pub fn picture(media: Option<&tl::enums::MessageMedia>) -> Option<Picture> {
    use tl::enums::MessageMedia as M;
    let (location, sizes) = match media? {
        M::Photo(p) => {
            let Some(tl::enums::Photo::Photo(photo)) = &p.photo else {
                return None;
            };
            (
                FileLocation {
                    file: StoredFile::Photo {
                        thumb_size: String::new(),
                    },
                    id: photo.id,
                    access_hash: photo.access_hash,
                    file_reference: photo.file_reference.clone(),
                    dc_id: photo.dc_id,
                },
                &photo.sizes,
            )
        }
        M::Document(d) => {
            let Some(tl::enums::Document::Document(doc)) = &d.document else {
                return None;
            };
            (
                FileLocation {
                    file: StoredFile::DocumentThumb {
                        thumb_size: String::new(),
                    },
                    id: doc.id,
                    access_hash: doc.access_hash,
                    file_reference: doc.file_reference.clone(),
                    dc_id: doc.dc_id,
                },
                doc.thumbs.as_ref()?,
            )
        }
        _ => return None,
    };
    let sizes: Vec<PictureSize> = sizes.iter().filter_map(picture_size).collect();
    (!sizes.is_empty()).then_some(Picture { location, sizes })
}

fn picture_size(size: &tl::enums::PhotoSize) -> Option<PictureSize> {
    use tl::enums::PhotoSize as P;
    let pixels = |p: i32| u32::try_from(p).unwrap_or(0);
    let bytes = |b: usize| b as u64;
    Some(match size {
        P::Empty(_) => return None,
        P::Size(s) => PictureSize {
            r#type: s.r#type.clone(),
            form: SizeForm::File,
            w: pixels(s.w),
            h: pixels(s.h),
            bytes: u64::try_from(s.size).unwrap_or(0),
        },
        P::Progressive(s) => PictureSize {
            r#type: s.r#type.clone(),
            form: SizeForm::Progressive,
            w: pixels(s.w),
            h: pixels(s.h),
            bytes: s
                .sizes
                .iter()
                .copied()
                .max()
                .and_then(|b| u64::try_from(b).ok())
                .unwrap_or(0),
        },
        P::PhotoCachedSize(s) => PictureSize {
            r#type: s.r#type.clone(),
            form: SizeForm::Cached,
            w: pixels(s.w),
            h: pixels(s.h),
            bytes: bytes(s.bytes.len()),
        },
        P::PhotoStrippedSize(s) => PictureSize {
            r#type: s.r#type.clone(),
            form: SizeForm::Stripped,
            w: 0,
            h: 0,
            bytes: bytes(s.bytes.len()),
        },
        P::PhotoPathSize(s) => PictureSize {
            r#type: s.r#type.clone(),
            form: SizeForm::Path,
            w: 0,
            h: 0,
            bytes: bytes(s.bytes.len()),
        },
    })
}

/// A file: its kind from its attributes (the flags on the message when the
/// document is gone, as with an expired self-destructing one), its metadata
/// from the document.
fn document(d: &tl::types::MessageMediaDocument) -> Media {
    let Some(tl::enums::Document::Document(doc)) = &d.document else {
        let kind = if d.round {
            MediaKind::VideoNote
        } else if d.voice {
            MediaKind::Voice
        } else if d.video {
            MediaKind::Video
        } else {
            MediaKind::Document
        };
        return kind.into();
    };
    let mut media = Media {
        size_bytes: u64::try_from(doc.size).ok().filter(|&s| s > 0),
        mime_type: Some(doc.mime_type.clone()).filter(|m| !m.is_empty()),
        ..Media::from(document_kind(doc))
    };
    for attribute in &doc.attributes {
        use tl::enums::DocumentAttribute as A;
        match attribute {
            A::Filename(f) => {
                media.file_name = Some(f.file_name.clone()).filter(|n| !n.is_empty());
            }
            A::Audio(a) => media.duration_s = u32::try_from(a.duration).ok(),
            A::Video(v) => {
                media.duration_s = seconds(v.duration);
                media.width = dimension(v.w);
                media.height = dimension(v.h);
            }
            // A video's own dimensions win over an image size.
            A::ImageSize(i) => {
                if media.width.is_none() {
                    media.width = dimension(i.w);
                    media.height = dimension(i.h);
                }
            }
            A::Animated | A::Sticker(_) | A::HasStickers | A::CustomEmoji(_) => {}
        }
    }
    media
}

/// What kind of file `doc` is, by all of its attributes rather than the
/// first that fits: Telegram lists them in no fixed order, and a GIF carries
/// a video attribute besides `animated`, a video sticker a video attribute
/// besides `sticker`.
pub fn document_kind(doc: &tl::types::Document) -> MediaKind {
    use tl::enums::DocumentAttribute as A;
    let has = |wanted: fn(&A) -> bool| doc.attributes.iter().any(wanted);
    if has(|a| matches!(a, A::Sticker(_) | A::CustomEmoji(_))) {
        MediaKind::Sticker
    } else if has(|a| matches!(a, A::Animated)) {
        MediaKind::Gif
    } else if has(|a| matches!(a, A::Video(v) if v.round_message)) {
        MediaKind::VideoNote
    } else if has(|a| matches!(a, A::Audio(a) if a.voice)) {
        MediaKind::Voice
    } else if has(|a| matches!(a, A::Audio(_))) {
        MediaKind::Audio
    } else if has(|a| matches!(a, A::Video(_))) {
        MediaKind::Video
    } else {
        MediaKind::Document
    }
}

/// Whole seconds of a video's duration (a double in TL), rounded.
fn seconds(duration: f64) -> Option<u32> {
    (duration.is_finite() && duration >= 0.0 && duration < f64::from(u32::MAX))
        .then(|| duration.round() as u32)
}

/// Pixels; 0 (or less) is Telegram not knowing.
fn dimension(pixels: i32) -> Option<u32> {
    u32::try_from(pixels).ok().filter(|&p| p > 0)
}

/// Media as Telegram sends it, for tests here and in the adapter.
#[cfg(test)]
pub(super) mod fixtures {
    use super::*;
    pub use tl::{enums, types};

    pub const HASH: i64 = 0x5EC2_E7AC_CE55_0001;
    pub const FILE_REFERENCE: &[u8] = b"\x01\x02secret-file-reference";

    pub fn photo_of(sizes: Vec<enums::PhotoSize>) -> enums::Photo {
        enums::Photo::Photo(types::Photo {
            has_stickers: false,
            id: 5_000_000_001,
            access_hash: HASH,
            file_reference: FILE_REFERENCE.to_vec(),
            date: 1_700_000_000,
            sizes,
            video_sizes: None,
            dc_id: 2,
        })
    }

    /// The sizes a photo comes with: a stripped inline thumbnail, `m` and
    /// `x` sizes, and a progressive `y` whose last size is the full one.
    pub fn full_photo() -> enums::Photo {
        photo_of(vec![
            enums::PhotoSize::PhotoStrippedSize(types::PhotoStrippedSize {
                r#type: "i".into(),
                bytes: vec![1; 40],
            }),
            enums::PhotoSize::Size(types::PhotoSize {
                r#type: "m".into(),
                w: 320,
                h: 240,
                size: 15_000,
            }),
            enums::PhotoSize::Size(types::PhotoSize {
                r#type: "x".into(),
                w: 800,
                h: 600,
                size: 70_000,
            }),
            enums::PhotoSize::Progressive(types::PhotoSizeProgressive {
                r#type: "y".into(),
                w: 1280,
                h: 960,
                sizes: vec![10_000, 40_000, 90_000, 152_331],
            }),
        ])
    }

    pub fn photo_media(
        photo: Option<enums::Photo>,
        ttl_seconds: Option<i32>,
    ) -> enums::MessageMedia {
        enums::MessageMedia::Photo(types::MessageMediaPhoto {
            spoiler: ttl_seconds.is_some(),
            live_photo: false,
            photo,
            ttl_seconds,
            video: None,
        })
    }

    pub fn doc(
        mime_type: &str,
        size: i64,
        attributes: Vec<enums::DocumentAttribute>,
    ) -> types::Document {
        types::Document {
            id: 6_000_000_001,
            access_hash: HASH,
            file_reference: FILE_REFERENCE.to_vec(),
            date: 1_700_000_000,
            mime_type: mime_type.into(),
            size,
            thumbs: None,
            video_thumbs: None,
            dc_id: 4,
            attributes,
        }
    }

    /// A document as a message's media, with the flags Telegram sets on the
    /// media for videos, video notes and voice messages.
    pub fn doc_media(doc: Option<types::Document>) -> enums::MessageMedia {
        let kind = doc.as_ref().map(document_kind);
        enums::MessageMedia::Document(types::MessageMediaDocument {
            nopremium: false,
            spoiler: false,
            video: matches!(kind, Some(MediaKind::Video | MediaKind::VideoNote)),
            round: kind == Some(MediaKind::VideoNote),
            voice: kind == Some(MediaKind::Voice),
            document: doc.map(enums::Document::Document),
            alt_documents: None,
            video_cover: None,
            video_timestamp: None,
            ttl_seconds: None,
        })
    }

    pub fn file_name(name: &str) -> enums::DocumentAttribute {
        enums::DocumentAttribute::Filename(types::DocumentAttributeFilename {
            file_name: name.into(),
        })
    }

    pub fn audio(voice: bool, duration: i32) -> enums::DocumentAttribute {
        enums::DocumentAttribute::Audio(types::DocumentAttributeAudio {
            voice,
            duration,
            title: (!voice).then(|| "Song title".into()),
            performer: (!voice).then(|| "Performer".into()),
            waveform: voice.then(|| vec![0, 4, 31, 7]),
        })
    }

    pub fn video(round: bool, duration: f64, w: i32, h: i32) -> enums::DocumentAttribute {
        enums::DocumentAttribute::Video(types::DocumentAttributeVideo {
            round_message: round,
            supports_streaming: !round,
            nosound: false,
            duration,
            w,
            h,
            preload_prefix_size: None,
            video_start_ts: None,
            video_codec: None,
        })
    }

    pub fn image_size(w: i32, h: i32) -> enums::DocumentAttribute {
        enums::DocumentAttribute::ImageSize(types::DocumentAttributeImageSize { w, h })
    }

    pub fn sticker(alt: &str) -> enums::DocumentAttribute {
        enums::DocumentAttribute::Sticker(types::DocumentAttributeSticker {
            mask: false,
            alt: alt.into(),
            stickerset: enums::InputStickerSet::Id(types::InputStickerSetId {
                id: 7,
                access_hash: HASH,
            }),
            mask_coords: None,
        })
    }

    pub fn text(text: &str) -> enums::TextWithEntities {
        enums::TextWithEntities::Entities(types::TextWithEntities {
            text: text.into(),
            entities: Vec::new(),
        })
    }

    pub fn poll(quiz: bool, closed: bool, with_results: bool) -> enums::MessageMedia {
        let answer = |t: &str, option: u8| {
            enums::PollAnswer::Answer(types::PollAnswer {
                text: text(t),
                option: vec![option],
                media: None,
                added_by: None,
                date: None,
            })
        };
        enums::MessageMedia::Poll(Box::new(types::MessageMediaPoll {
            poll: enums::Poll::Poll(types::Poll {
                id: 1,
                closed,
                public_voters: false,
                multiple_choice: !quiz,
                quiz,
                open_answers: false,
                revoting_disabled: false,
                shuffle_answers: false,
                hide_results_until_close: false,
                creator: false,
                subscribers_only: false,
                question: text("Where do we eat?"),
                answers: vec![answer("Here", 0), answer("There", 1)],
                close_period: None,
                close_date: None,
                countries_iso2: None,
                hash: 0,
            }),
            results: enums::PollResults::Results(Box::new(types::PollResults {
                min: false,
                has_unread_votes: false,
                can_view_stats: false,
                results: with_results.then(Vec::new),
                total_voters: with_results.then_some(3),
                recent_voters: None,
                solution: quiz.then(|| "Because".into()),
                solution_entities: None,
                solution_media: None,
            })),
            attached_media: None,
        }))
    }

    pub fn geo() -> enums::GeoPoint {
        enums::GeoPoint::Point(types::GeoPoint {
            long: 37.6,
            lat: 55.7,
            access_hash: HASH,
            accuracy_radius: None,
        })
    }

    pub fn kind(media: &enums::MessageMedia) -> Option<MediaKind> {
        describe(Some(media)).map(|m| m.kind)
    }
}

#[cfg(test)]
mod tests {
    use super::{fixtures::*, *};

    #[test]
    fn a_photo_is_described_by_its_largest_size() {
        assert_eq!(
            describe(Some(&photo_media(Some(full_photo()), None))),
            Some(Media {
                width: Some(1280),
                height: Some(960),
                size_bytes: Some(152_331),
                ..Media::from(MediaKind::Photo)
            })
        );
        // Without a progressive size, the largest plain one.
        let plain = photo_of(vec![
            enums::PhotoSize::Size(types::PhotoSize {
                r#type: "w".into(),
                w: 2560,
                h: 1920,
                size: 900_000,
            }),
            enums::PhotoSize::Size(types::PhotoSize {
                r#type: "s".into(),
                w: 100,
                h: 75,
                size: 2_000,
            }),
            enums::PhotoSize::PhotoCachedSize(types::PhotoCachedSize {
                r#type: "s".into(),
                w: 9_000,
                h: 9_000,
                bytes: vec![1; 10],
            }),
        ]);
        let described = describe(Some(&photo_media(Some(plain), None))).unwrap();
        assert_eq!(
            (described.width, described.height, described.size_bytes),
            (Some(2560), Some(1920), Some(900_000))
        );
    }

    #[test]
    fn a_self_destructing_photo_that_is_gone_is_still_a_photo() {
        assert_eq!(
            describe(Some(&photo_media(None, Some(10)))),
            Some(MediaKind::Photo.into())
        );
        assert_eq!(
            describe(Some(&photo_media(
                Some(enums::Photo::Empty(types::PhotoEmpty { id: 1 })),
                Some(10)
            ))),
            Some(MediaKind::Photo.into())
        );
        // A self-destructing voice message that is gone: the media's flags.
        let gone = |round, voice, video| {
            describe(Some(&enums::MessageMedia::Document(
                types::MessageMediaDocument {
                    nopremium: false,
                    spoiler: false,
                    video,
                    round,
                    voice,
                    document: None,
                    alt_documents: None,
                    video_cover: None,
                    video_timestamp: None,
                    ttl_seconds: Some(30),
                },
            )))
        };
        assert_eq!(gone(false, true, false), Some(MediaKind::Voice.into()));
        assert_eq!(gone(true, false, true), Some(MediaKind::VideoNote.into()));
        assert_eq!(gone(false, false, true), Some(MediaKind::Video.into()));
        assert_eq!(gone(false, false, false), Some(MediaKind::Document.into()));
    }

    #[test]
    fn a_voice_message_has_its_duration_size_and_type() {
        let voice = doc("audio/ogg", 23_211, vec![audio(true, 7)]);
        assert_eq!(
            describe(Some(&doc_media(Some(voice)))),
            Some(Media {
                duration_s: Some(7),
                size_bytes: Some(23_211),
                mime_type: Some("audio/ogg".into()),
                ..Media::from(MediaKind::Voice)
            })
        );
    }

    #[test]
    fn every_kind_of_file_is_told_apart_whatever_the_attribute_order() {
        let cases = [
            (
                "music",
                doc(
                    "audio/mpeg",
                    5_000_000,
                    vec![audio(false, 215), file_name("song.mp3")],
                ),
                Media {
                    duration_s: Some(215),
                    size_bytes: Some(5_000_000),
                    mime_type: Some("audio/mpeg".into()),
                    file_name: Some("song.mp3".into()),
                    ..Media::from(MediaKind::Audio)
                },
            ),
            (
                "video",
                doc(
                    "video/mp4",
                    8_000_000,
                    vec![video(false, 12.6, 1280, 720), file_name("clip.mp4")],
                ),
                Media {
                    duration_s: Some(13),
                    size_bytes: Some(8_000_000),
                    mime_type: Some("video/mp4".into()),
                    file_name: Some("clip.mp4".into()),
                    width: Some(1280),
                    height: Some(720),
                    ..Media::from(MediaKind::Video)
                },
            ),
            (
                "video note",
                doc("video/mp4", 900_000, vec![video(true, 9.0, 384, 384)]),
                Media {
                    duration_s: Some(9),
                    size_bytes: Some(900_000),
                    mime_type: Some("video/mp4".into()),
                    width: Some(384),
                    height: Some(384),
                    ..Media::from(MediaKind::VideoNote)
                },
            ),
            (
                // As Telegram sends a GIF: the video attribute first.
                "gif",
                doc(
                    "video/mp4",
                    300_000,
                    vec![
                        video(false, 2.4, 480, 270),
                        file_name("giphy.mp4"),
                        enums::DocumentAttribute::Animated,
                    ],
                ),
                Media {
                    duration_s: Some(2),
                    size_bytes: Some(300_000),
                    mime_type: Some("video/mp4".into()),
                    file_name: Some("giphy.mp4".into()),
                    width: Some(480),
                    height: Some(270),
                    ..Media::from(MediaKind::Gif)
                },
            ),
            (
                "sticker",
                doc(
                    "image/webp",
                    25_000,
                    vec![
                        image_size(512, 512),
                        sticker("😀"),
                        file_name("sticker.webp"),
                    ],
                ),
                Media {
                    size_bytes: Some(25_000),
                    mime_type: Some("image/webp".into()),
                    file_name: Some("sticker.webp".into()),
                    width: Some(512),
                    height: Some(512),
                    ..Media::from(MediaKind::Sticker)
                },
            ),
            (
                // A video sticker: a video attribute before the sticker one.
                "video sticker",
                doc(
                    "video/webm",
                    40_000,
                    vec![
                        video(false, 3.0, 512, 512),
                        file_name("sticker.webm"),
                        sticker("🎉"),
                    ],
                ),
                Media {
                    duration_s: Some(3),
                    size_bytes: Some(40_000),
                    mime_type: Some("video/webm".into()),
                    file_name: Some("sticker.webm".into()),
                    width: Some(512),
                    height: Some(512),
                    ..Media::from(MediaKind::Sticker)
                },
            ),
            (
                "animated sticker",
                doc(
                    "application/x-tgsticker",
                    12_000,
                    vec![
                        image_size(512, 512),
                        sticker("🐱"),
                        file_name("AnimatedSticker.tgs"),
                    ],
                ),
                Media {
                    size_bytes: Some(12_000),
                    mime_type: Some("application/x-tgsticker".into()),
                    file_name: Some("AnimatedSticker.tgs".into()),
                    width: Some(512),
                    height: Some(512),
                    ..Media::from(MediaKind::Sticker)
                },
            ),
            (
                "pdf",
                doc("application/pdf", 120_000, vec![file_name("report.pdf")]),
                Media {
                    size_bytes: Some(120_000),
                    mime_type: Some("application/pdf".into()),
                    file_name: Some("report.pdf".into()),
                    ..Media::from(MediaKind::Document)
                },
            ),
            (
                "image sent as a file",
                doc(
                    "image/png",
                    2_000_000,
                    vec![image_size(3000, 2000), file_name("scan.png")],
                ),
                Media {
                    size_bytes: Some(2_000_000),
                    mime_type: Some("image/png".into()),
                    file_name: Some("scan.png".into()),
                    width: Some(3000),
                    height: Some(2000),
                    ..Media::from(MediaKind::Document)
                },
            ),
            (
                // Nothing Telegram leaves out, or sends as 0, is made up.
                "bare file",
                doc("", 0, vec![image_size(0, 0)]),
                Media::from(MediaKind::Document),
            ),
        ];
        for (name, document, expected) in cases {
            assert_eq!(
                describe(Some(&doc_media(Some(document.clone())))),
                Some(expected.clone()),
                "{name}"
            );
            // Reversed attributes: the same.
            let reversed = types::Document {
                attributes: document.attributes.iter().rev().cloned().collect(),
                ..document
            };
            assert_eq!(
                describe(Some(&doc_media(Some(reversed)))),
                Some(expected),
                "{name}, reversed"
            );
        }
    }

    #[test]
    fn every_media_constructor_of_the_layer_has_a_kind() {
        use enums::MessageMedia as M;
        let other = Some(MediaKind::Other);
        let cases: Vec<(&str, M, Option<MediaKind>)> = vec![
            ("empty", M::Empty, None),
            (
                "link preview, pending",
                M::WebPage(types::MessageMediaWebPage {
                    force_large_media: false,
                    force_small_media: false,
                    manual: false,
                    safe: true,
                    webpage: enums::WebPage::Pending(types::WebPagePending {
                        id: 1,
                        url: Some("https://example.com".into()),
                        date: 1,
                    }),
                }),
                None,
            ),
            (
                "link preview, empty",
                M::WebPage(types::MessageMediaWebPage {
                    force_large_media: false,
                    force_small_media: false,
                    manual: true,
                    safe: false,
                    webpage: enums::WebPage::Empty(types::WebPageEmpty { id: 1, url: None }),
                }),
                None,
            ),
            (
                "geo",
                M::Geo(types::MessageMediaGeo { geo: geo() }),
                Some(MediaKind::Location),
            ),
            (
                "geo, empty point",
                M::Geo(types::MessageMediaGeo {
                    geo: enums::GeoPoint::Empty,
                }),
                Some(MediaKind::Location),
            ),
            (
                "live location",
                M::GeoLive(types::MessageMediaGeoLive {
                    geo: geo(),
                    heading: Some(90),
                    period: 900,
                    proximity_notification_radius: None,
                }),
                Some(MediaKind::Location),
            ),
            (
                "venue",
                M::Venue(types::MessageMediaVenue {
                    geo: geo(),
                    title: "Cafe".into(),
                    address: "Main st. 1".into(),
                    provider: "foursquare".into(),
                    venue_id: "abc".into(),
                    venue_type: "food".into(),
                }),
                Some(MediaKind::Location),
            ),
            (
                "contact",
                M::Contact(types::MessageMediaContact {
                    phone_number: "+10000000000".into(),
                    first_name: "Ann".into(),
                    last_name: String::new(),
                    vcard: String::new(),
                    user_id: 9,
                }),
                Some(MediaKind::Contact),
            ),
            ("poll", poll(false, false, false), Some(MediaKind::Poll)),
            ("closed quiz", poll(true, true, true), Some(MediaKind::Poll)),
            (
                "dice",
                M::Dice(types::MessageMediaDice {
                    value: 6,
                    emoticon: "🎲".into(),
                    game_outcome: None,
                }),
                other,
            ),
            (
                "story",
                M::Story(Box::new(types::MessageMediaStory {
                    via_mention: false,
                    peer: enums::Peer::User(types::PeerUser { user_id: 9 }),
                    id: 3,
                    story: None,
                })),
                other,
            ),
            (
                "game",
                M::Game(types::MessageMediaGame {
                    game: enums::Game::Game(types::Game {
                        id: 1,
                        access_hash: HASH,
                        short_name: "g".into(),
                        title: "Game".into(),
                        description: String::new(),
                        photo: enums::Photo::Empty(types::PhotoEmpty { id: 1 }),
                        document: None,
                    }),
                }),
                other,
            ),
            (
                "invoice",
                M::Invoice(Box::new(types::MessageMediaInvoice {
                    shipping_address_requested: false,
                    test: true,
                    title: "Pay".into(),
                    description: "for it".into(),
                    photo: None,
                    receipt_msg_id: None,
                    currency: "XTR".into(),
                    total_amount: 100,
                    start_param: String::new(),
                    extended_media: None,
                })),
                other,
            ),
            (
                "giveaway",
                M::Giveaway(types::MessageMediaGiveaway {
                    only_new_subscribers: false,
                    winners_are_visible: true,
                    channels: vec![1],
                    countries_iso2: None,
                    prize_description: Some("prize".into()),
                    quantity: 3,
                    months: Some(3),
                    stars: None,
                    until_date: 1,
                }),
                other,
            ),
            (
                "giveaway results",
                M::GiveawayResults(types::MessageMediaGiveawayResults {
                    only_new_subscribers: false,
                    refunded: false,
                    channel_id: 1,
                    additional_peers_count: None,
                    launch_msg_id: 1,
                    winners_count: 1,
                    unclaimed_count: 0,
                    winners: vec![9],
                    months: None,
                    stars: Some(50),
                    prize_description: None,
                    until_date: 1,
                }),
                other,
            ),
            (
                "paid media",
                M::PaidMedia(types::MessageMediaPaidMedia {
                    stars_amount: 10,
                    extended_media: Vec::new(),
                }),
                other,
            ),
            (
                "checklist",
                M::ToDo(types::MessageMediaToDo {
                    todo: enums::TodoList::List(types::TodoList {
                        others_can_append: false,
                        others_can_complete: false,
                        title: text("Groceries"),
                        list: Vec::new(),
                    }),
                    completions: None,
                }),
                other,
            ),
            (
                "live stream",
                M::VideoStream(types::MessageMediaVideoStream {
                    rtmp_stream: false,
                    call: enums::InputGroupCall::Call(types::InputGroupCall {
                        id: 1,
                        access_hash: HASH,
                    }),
                }),
                other,
            ),
            ("unsupported", M::Unsupported, other),
            (
                "photo",
                photo_media(Some(full_photo()), None),
                Some(MediaKind::Photo),
            ),
            (
                "document",
                doc_media(Some(doc("application/zip", 1, vec![file_name("a.zip")]))),
                Some(MediaKind::Document),
            ),
        ];
        for (name, media, expected) in &cases {
            assert_eq!(kind(media), *expected, "{name}");
        }
        assert_eq!(describe(None), None);
    }

    #[test]
    fn a_photos_file_is_its_largest_size_and_a_documents_the_document() {
        let (location, size) = file(Some(&photo_media(Some(full_photo()), None))).unwrap();
        assert_eq!(
            location,
            FileLocation {
                file: StoredFile::Photo {
                    thumb_size: "y".into()
                },
                id: 5_000_000_001,
                access_hash: HASH,
                file_reference: FILE_REFERENCE.to_vec(),
                dc_id: 2,
            }
        );
        // The size describe() reports for the photo.
        assert_eq!(size, Some(152_331));

        let voice = doc_media(Some(doc("audio/ogg", 23_211, vec![audio(true, 7)])));
        let (location, size) = file(Some(&voice)).unwrap();
        assert_eq!(
            (location.file, location.id, location.dc_id, size),
            (StoredFile::Document, 6_000_000_001, 4, Some(23_211))
        );
        assert_eq!(location.file_reference, FILE_REFERENCE);
    }

    #[test]
    fn a_pictures_sizes_are_every_size_of_the_photo_or_thumbnail_of_the_document() {
        let size = |r#type: &str, form, w, h, bytes| PictureSize {
            r#type: r#type.into(),
            form,
            w,
            h,
            bytes,
        };
        let photo = picture(Some(&photo_media(Some(full_photo()), None))).unwrap();
        assert_eq!(
            photo.location,
            FileLocation {
                file: StoredFile::Photo {
                    thumb_size: String::new()
                },
                id: 5_000_000_001,
                access_hash: HASH,
                file_reference: FILE_REFERENCE.to_vec(),
                dc_id: 2,
            }
        );
        assert_eq!(
            photo.sizes,
            [
                // Inline: kept, but marked so (a view never sends it).
                size("i", SizeForm::Stripped, 0, 0, 40),
                size("m", SizeForm::File, 320, 240, 15_000),
                size("x", SizeForm::File, 800, 600, 70_000),
                // Progressive: the whole file is its largest size.
                size("y", SizeForm::Progressive, 1280, 960, 152_331),
            ]
        );
        let at = photo.at(&photo.sizes[2]);
        assert_eq!(
            at.file,
            StoredFile::Photo {
                thumb_size: "x".into()
            }
        );
        assert!(
            at.same_object(
                &file(Some(&photo_media(Some(full_photo()), None)))
                    .unwrap()
                    .0
            )
        );

        // A video and an animated sticker, with the thumbnails Telegram sends.
        let mut video = doc(
            "video/mp4",
            8_000_000,
            vec![video(false, 12.6, 1280, 720), file_name("clip.mp4")],
        );
        video.thumbs = Some(vec![
            enums::PhotoSize::PhotoStrippedSize(types::PhotoStrippedSize {
                r#type: "i".into(),
                bytes: vec![1; 30],
            }),
            enums::PhotoSize::Size(types::PhotoSize {
                r#type: "m".into(),
                w: 320,
                h: 180,
                size: 9_000,
            }),
        ]);
        let thumbs = picture(Some(&doc_media(Some(video)))).unwrap();
        assert_eq!(
            thumbs.location.file,
            StoredFile::DocumentThumb {
                thumb_size: String::new()
            }
        );
        assert_eq!(
            (thumbs.location.id, thumbs.location.dc_id),
            (6_000_000_001, 4)
        );
        assert_eq!(
            thumbs.sizes,
            [
                size("i", SizeForm::Stripped, 0, 0, 30),
                size("m", SizeForm::File, 320, 180, 9_000),
            ]
        );
        assert_eq!(
            thumbs.at(&thumbs.sizes[1]).file,
            StoredFile::DocumentThumb {
                thumb_size: "m".into()
            }
        );
        let mut tgs = doc(
            "application/x-tgsticker",
            12_000,
            vec![image_size(512, 512), sticker("🐱")],
        );
        tgs.thumbs = Some(vec![
            enums::PhotoSize::PhotoPathSize(types::PhotoPathSize {
                r#type: "j".into(),
                bytes: vec![2; 50],
            }),
            enums::PhotoSize::PhotoCachedSize(types::PhotoCachedSize {
                r#type: "s".into(),
                w: 100,
                h: 100,
                bytes: vec![3; 700],
            }),
            enums::PhotoSize::Empty(types::PhotoSizeEmpty { r#type: "z".into() }),
        ]);
        assert_eq!(
            picture(Some(&doc_media(Some(tgs)))).unwrap().sizes,
            [
                size("j", SizeForm::Path, 0, 0, 50),
                size("s", SizeForm::Cached, 100, 100, 700),
            ]
        );

        // No thumbnails, a gone photo, no file: no picture.
        let pdf = doc_media(Some(doc("application/pdf", 1, vec![file_name("a.pdf")])));
        let geo = enums::MessageMedia::Geo(types::MessageMediaGeo { geo: geo() });
        for media in [pdf, photo_media(None, Some(10)), doc_media(None), geo] {
            assert_eq!(picture(Some(&media)), None, "{media:?}");
        }
        assert_eq!(picture(None), None);
    }

    /// Answer `option`'s results: votes (if shown), chosen, correct.
    fn voters(
        option: u8,
        votes: Option<i32>,
        chosen: bool,
        correct: bool,
    ) -> enums::PollAnswerVoters {
        enums::PollAnswerVoters::Voters(types::PollAnswerVoters {
            chosen,
            correct,
            option: vec![option],
            voters: votes,
            recent_voters: None,
        })
    }

    /// [`poll`] with `answers`, its results `voters` (none: not shown to
    /// this Account) and `total`, edited by `edit`.
    fn poll_media(
        answers: &[&str],
        results: Option<Vec<enums::PollAnswerVoters>>,
        total: Option<i32>,
        edit: impl FnOnce(&mut types::Poll, &mut types::PollResults),
    ) -> Media {
        let enums::MessageMedia::Poll(mut media) = poll(false, false, false) else {
            unreachable!()
        };
        let enums::Poll::Poll(poll) = &mut media.poll;
        let enums::PollResults::Results(shown) = &mut media.results;
        poll.multiple_choice = false;
        poll.answers = answers
            .iter()
            .enumerate()
            .map(|(i, t)| {
                enums::PollAnswer::Answer(types::PollAnswer {
                    text: text(t),
                    option: vec![i as u8],
                    media: None,
                    added_by: None,
                    date: None,
                })
            })
            .collect();
        shown.results = results;
        shown.total_voters = total;
        shown.solution = None;
        edit(poll, shown);
        describe(Some(&enums::MessageMedia::Poll(media))).unwrap()
    }

    fn option(text: &str, votes: Option<u32>, chosen: bool, correct: bool) -> PollOption {
        PollOption {
            text: text.into(),
            votes,
            chosen,
            correct,
        }
    }

    #[test]
    fn an_open_poll_without_visible_results_has_its_question_and_options_only() {
        let media = poll_media(&["Here", "There"], None, None, |_, _| {});
        assert_eq!(media.kind, MediaKind::Poll);
        assert_eq!(
            media.poll,
            Some(Poll {
                question: "Where do we eat?".into(),
                options: vec![
                    option("Here", None, false, false),
                    option("There", None, false, false)
                ],
                options_truncated: false,
                total_voters: None,
                closed: false,
                multiple_choice: false,
                quiz: false,
                anonymous: true,
            })
        );
    }

    #[test]
    fn visible_results_give_votes_per_option_and_the_accounts_choices() {
        let results = vec![
            voters(0, Some(5), true, false),
            voters(1, Some(2), false, false),
            voters(2, Some(4), true, false),
        ];
        let media = poll_media(&["A", "B", "C"], Some(results), Some(9), |poll, _| {
            poll.multiple_choice = true;
            poll.public_voters = true;
        });
        let poll = media.poll.unwrap();
        assert_eq!(
            poll.options,
            [
                option("A", Some(5), true, false),
                option("B", Some(2), false, false),
                option("C", Some(4), true, false),
            ]
        );
        assert_eq!(poll.total_voters, Some(9));
        assert!(poll.multiple_choice && !poll.anonymous && !poll.closed && !poll.quiz);
    }

    #[test]
    fn a_closed_poll_says_so_and_shows_its_results() {
        let results = vec![
            voters(0, Some(1), false, false),
            voters(1, Some(0), false, false),
        ];
        let poll = poll_media(&["Yes", "No"], Some(results), Some(1), |poll, _| {
            poll.closed = true;
        })
        .poll
        .unwrap();
        assert!(poll.closed);
        assert_eq!(poll.options[1], option("No", Some(0), false, false));
    }

    #[test]
    fn a_quiz_marks_its_correct_answer_once_revealed() {
        let revealed = vec![
            voters(0, Some(3), true, false),
            voters(1, Some(1), false, true),
        ];
        let poll = poll_media(&["2", "4"], Some(revealed), Some(4), |poll, _| {
            poll.quiz = true;
        })
        .poll
        .unwrap();
        assert!(poll.quiz);
        assert_eq!(
            poll.options,
            [
                option("2", Some(3), true, false),
                option("4", Some(1), false, true)
            ]
        );

        // Not a quiz: no option is correct, whatever the flag says.
        let poll = poll_media(
            &["a"],
            Some(vec![voters(0, Some(1), false, true)]),
            Some(1),
            |_, _| {},
        )
        .poll
        .unwrap();
        assert!(!poll.options[0].correct);
    }

    #[test]
    fn results_the_same_for_everyone_say_nothing_of_the_accounts_choice() {
        // `min` results come without this Account's own vote.
        let results = vec![voters(0, Some(2), true, false)];
        let poll = poll_media(&["a"], Some(results), Some(2), |_, results| {
            results.min = true;
        })
        .poll
        .unwrap();
        assert_eq!(poll.options, [option("a", Some(2), false, false)]);
    }

    #[test]
    fn every_option_and_the_whole_text_are_read_for_the_page_to_cut() {
        let long_option = "o".repeat(150);
        let many: Vec<&str> = std::iter::repeat_n(long_option.as_str(), 14).collect();
        let poll = poll_media(&many, None, None, |poll, _| {
            poll.question = text(&"q".repeat(400));
        })
        .poll
        .unwrap();
        assert_eq!(poll.question.chars().count(), 400);
        assert_eq!(poll.options.len(), 14);
        assert!(!poll.options_truncated);
    }

    #[test]
    fn an_option_without_a_count_in_shown_results_has_no_votes() {
        let results = vec![
            voters(0, None, true, false),
            voters(1, Some(3), false, false),
        ];
        let poll = poll_media(&["a", "b"], Some(results), Some(4), |_, _| {})
            .poll
            .unwrap();
        assert_eq!(
            poll.options,
            [
                option("a", None, true, false),
                option("b", Some(3), false, false)
            ]
        );
    }

    #[test]
    fn media_without_a_file_has_none() {
        let gone = [
            photo_media(None, Some(10)),
            photo_media(Some(enums::Photo::Empty(types::PhotoEmpty { id: 1 })), None),
            doc_media(None),
            // A photo with only an inline thumbnail has no size to fetch.
            photo_media(
                Some(photo_of(vec![enums::PhotoSize::PhotoStrippedSize(
                    types::PhotoStrippedSize {
                        r#type: "i".into(),
                        bytes: vec![1; 40],
                    },
                )])),
                None,
            ),
        ];
        for media in &gone {
            assert_eq!(file(Some(media)), None, "{media:?}");
        }
        let geo = enums::MessageMedia::Geo(types::MessageMediaGeo { geo: geo() });
        for media in [geo, poll(false, false, false), enums::MessageMedia::Empty] {
            assert_eq!(file(Some(&media)), None);
        }
        assert_eq!(file(None), None);
    }

    #[test]
    fn a_file_location_prints_no_hash_or_reference() {
        let (location, _) = file(Some(&photo_media(Some(full_photo()), None))).unwrap();
        let printed = format!("{location:?}");
        for leak in [
            HASH.to_string(),
            "secret-file-reference".into(),
            "5000000001".into(),
        ] {
            assert!(!printed.contains(&leak), "{leak} in {printed}");
        }
        assert!(printed.contains("dc_id: 2"), "{printed}");
    }

    #[test]
    fn nothing_to_fetch_the_media_with_leaves_tl() {
        let media = [
            photo_media(Some(full_photo()), None),
            doc_media(Some(doc(
                "video/mp4",
                10,
                vec![video(false, 1.0, 2, 2), file_name("v.mp4")],
            ))),
        ];
        for media in &media {
            let json = serde_json::to_string(&describe(Some(media))).unwrap();
            for leak in [
                HASH.to_string(),
                "secret-file-reference".into(),
                "6000000001".into(),
                "5000000001".into(),
                "dc_id".into(),
                "access_hash".into(),
                "file_reference".into(),
            ] {
                assert!(!json.contains(&leak), "{leak} in {json}");
            }
        }
    }
}
