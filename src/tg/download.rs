//! Downloading the media of one message (#36), independent of grammers/TL.
//!
//! [`locate`] finds a message's file: the adapter fetches the message by id
//! (`channels.getMessages` / `messages.getMessages`, as a reply chain does)
//! and reduces each message it finds to a [`RawMediaMessage`]; the message
//! must be the asked Chat's (the same check as a chain's), and its media must
//! have a file. [`Download`] then reads that file part by part with
//! `upload.getFile` ([`FileRequest`]), the way Telegram serves it: in parts
//! of [`PART_SIZE`] at offsets that are multiples of it, from the DC the file
//! is in (a `FILE_MIGRATE_X` names another one), with a file reference that
//! may expire (the message is fetched again for a fresh one, once). A
//! `FLOOD_WAIT` is the adapter's to honour (method class Files), never
//! retried around here. The fake answers the same requests with a model of
//! Telegram.

use std::{fmt, future::Future, sync::Arc};

use super::{
    Telegram, TgError,
    chats::ChatRef,
    media::FileLocation,
    messages::{Media, MediaKind, MessageAt, MessageRequest, in_chat},
};

/// Bytes asked for per `upload.getFile`: grammers' own download part size
/// (it is Telegram's 512 KB upload part too). Telegram wants the limit
/// divisible by 4 KB and dividing 1 MB, the offset divisible by the limit's
/// alignment, and no part crossing a 1 MB boundary; whole parts at
/// multiples of this size satisfy all of it.
pub const PART_SIZE: i32 = 512 * 1024;

/// `FILE_MIGRATE_X` answers followed in a row for one part. Telegram names
/// the file's DC at once; a longer chain is a bug, not a reason to loop.
const MAX_MIGRATIONS: usize = 2;

/// A message's media with its file: what to tell about it (`media`, as
/// get_messages describes it) and where the file is (`location`, which
/// never leaves the server).
#[derive(Clone, PartialEq, Eq)]
pub struct MediaFile {
    pub media: Media,
    pub location: FileLocation,
    /// The file's size in bytes, as Telegram gives it with the message.
    pub size: u64,
}

impl fmt::Debug for MediaFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MediaFile")
            .field("kind", &self.media.kind)
            .field("size", &self.size)
            .field("location", &self.location)
            .finish_non_exhaustive()
    }
}

/// One message as a `getMessages` call for its id found it, reduced to its
/// media and that media's file.
#[derive(Debug, Clone)]
pub struct RawMediaMessage {
    pub at: MessageAt,
    pub media: Option<Media>,
    /// The file and its size (`None`: Telegram didn't give it), if the
    /// media has a file that is still there.
    pub file: Option<(FileLocation, Option<u64>)>,
}

/// The file of message `id` of `chat`, with one `get_message` call. A
/// message that isn't there, has no media, or whose media has no file (a
/// location, contact or poll, or media that is gone) is the caller's to fix.
pub async fn locate<G, Fut>(chat: ChatRef, id: i32, get_message: G) -> Result<MediaFile, TgError>
where
    G: FnOnce(MessageRequest) -> Fut,
    Fut: Future<Output = Result<Vec<RawMediaMessage>, TgError>>,
{
    let found = get_message(MessageRequest { id }).await?;
    let message = in_chat(
        found,
        MessageAt {
            chat: chat.chat(),
            id,
        },
        |m| m.at,
    )
    .ok_or_else(|| {
        TgError::InvalidInput(format!(
            "message {id} not found in this chat (deleted, or the id is from another chat)"
        ))
    })?;
    let Some(media) = message.media else {
        return Err(TgError::InvalidInput(format!(
            "message {id} has no media: get_messages shows its text"
        )));
    };
    let Some((location, size)) = message.file else {
        return Err(TgError::InvalidInput(no_file(id, media.kind)));
    };
    let size = size.ok_or_else(|| {
        TgError::InvalidInput(format!(
            "Telegram doesn't say how big message {id}'s file is, so it can't be downloaded"
        ))
    })?;
    Ok(MediaFile {
        media,
        location,
        size,
    })
}

fn no_file(id: i32, kind: MediaKind) -> String {
    match kind {
        MediaKind::Location | MediaKind::Contact | MediaKind::Poll => format!(
            "message {id}'s media is a {}: it has no file to download; get_messages describes it",
            match kind {
                MediaKind::Location => "location",
                MediaKind::Contact => "contact",
                _ => "poll",
            }
        ),
        MediaKind::Other => format!(
            "message {id}'s media (a dice, story, game, invoice, giveaway, paid media, checklist \
             or live stream) has no file this server can download"
        ),
        _ => format!(
            "message {id}'s media is no longer available (e.g. a self-destructing one that \
             expired)"
        ),
    }
}

/// One `upload.getFile` call: `limit` bytes of the file at `location` from
/// `offset`, asked of DC `dc_id`. CDN redirects are not supported (the
/// request says so).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRequest {
    pub location: FileLocation,
    pub dc_id: i32,
    pub offset: i64,
    pub limit: i32,
}

/// What Telegram answered to one [`FileRequest`].
#[derive(Clone, PartialEq, Eq)]
pub enum FilePart {
    /// `upload.file`: up to `limit` bytes; fewer means the file ends here.
    Bytes(Vec<u8>),
    /// `upload.fileCdnRedirect`, which Telegram only sends to a request
    /// that supports CDNs.
    CdnRedirect,
}

impl fmt::Debug for FilePart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bytes(bytes) => write!(f, "Bytes(<{} bytes>)", bytes.len()),
            Self::CdnRedirect => f.write_str("CdnRedirect"),
        }
    }
}

/// Telegram refused a [`FileRequest`].
#[derive(Debug)]
pub enum FileError {
    /// `FILE_MIGRATE_X`: the file is in DC X; ask there.
    Migrate(i32),
    /// `FILE_REFERENCE_EXPIRED`: fetch the message again for a fresh one.
    ReferenceExpired,
    /// Anything else, `FLOOD_WAIT` (class Files) included.
    Failed(TgError),
}

impl From<TgError> for FileError {
    fn from(err: TgError) -> Self {
        Self::Failed(err)
    }
}

/// One download of one message's file, from the start: [`Download::next_part`]
/// until it says the file is done. Nothing happens between calls, so
/// dropping it (the client went away) stops the download.
pub struct Download {
    telegram: Arc<dyn Telegram>,
    chat: ChatRef,
    message_id: i32,
    location: FileLocation,
    /// Where to ask: the file's DC, or where Telegram sent us since.
    dc_id: i32,
    size: u64,
    offset: u64,
    /// The file reference was refreshed already: once per download.
    refreshed: bool,
}

impl Download {
    pub fn new(
        telegram: Arc<dyn Telegram>,
        chat: ChatRef,
        message_id: i32,
        file: &MediaFile,
    ) -> Self {
        Self {
            telegram,
            chat,
            message_id,
            dc_id: file.location.dc_id,
            location: file.location.clone(),
            size: file.size,
            offset: 0,
            refreshed: false,
        }
    }

    /// Bytes served so far.
    pub fn served(&self) -> u64 {
        self.offset
    }

    /// All of the file was served.
    pub fn is_done(&self) -> bool {
        self.offset >= self.size
    }

    /// The next part of the file, or `None` once all of it (exactly its
    /// size) was returned. A file Telegram ends before its size is an error,
    /// and bytes past its size are dropped.
    pub async fn next_part(&mut self) -> Result<Option<Vec<u8>>, TgError> {
        let left = self.size.saturating_sub(self.offset);
        if left == 0 {
            return Ok(None);
        }
        let expected = left.min(PART_SIZE as u64) as usize;
        let mut migrations = 0;
        loop {
            let request = FileRequest {
                location: self.location.clone(),
                dc_id: self.dc_id,
                offset: self.offset as i64,
                limit: PART_SIZE,
            };
            match self.telegram.get_file(request).await {
                Ok(FilePart::Bytes(mut bytes)) => {
                    if bytes.len() < expected {
                        return Err(TgError::Upstream(format!(
                            "telegram ended the file at {} of {} bytes",
                            self.offset + bytes.len() as u64,
                            self.size
                        )));
                    }
                    bytes.truncate(expected);
                    self.offset += expected as u64;
                    return Ok(Some(bytes));
                }
                Ok(FilePart::CdnRedirect) => {
                    return Err(TgError::Upstream(
                        "telegram redirected the download to a CDN, which this server doesn't \
                         support"
                            .into(),
                    ));
                }
                Err(FileError::Migrate(dc_id)) => {
                    migrations += 1;
                    if migrations > MAX_MIGRATIONS {
                        return Err(TgError::Upstream(
                            "telegram kept sending the download to another DC".into(),
                        ));
                    }
                    self.dc_id = dc_id;
                }
                Err(FileError::ReferenceExpired) => self.refresh().await?,
                Err(FileError::Failed(err)) => return Err(err),
            }
        }
    }

    /// A fresh file reference from the message, once per download. The
    /// message must still carry the same photo or document (a link is for
    /// one file), and the same file at the same size when this download is
    /// of the file itself rather than of one of its sizes or thumbnails
    /// (#34). One file reference is good for all of them.
    async fn refresh(&mut self) -> Result<(), TgError> {
        if self.refreshed {
            return Err(TgError::Upstream(
                "the file reference expired again right after it was refreshed".into(),
            ));
        }
        self.refreshed = true;
        let fresh = self.telegram.media_file(self.chat, self.message_id).await?;
        let resized = fresh.location.file == self.location.file && fresh.size != self.size;
        if !fresh.location.same_object(&self.location) || resized {
            return Err(TgError::InvalidInput(
                "the message's media changed since the link was made".into(),
            ));
        }
        self.location.file_reference = fresh.location.file_reference;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        limits::MethodClass,
        tg::{
            chats::ChatId,
            fake::{
                FAKE_FILE_DC, FakeData, FakeTelegram, FileRequestSeen, default_account,
                fake_access_hash, fake_file_bytes, fake_file_id,
            },
            messages::RawHistoryMessage,
        },
    };

    const ALICE: ChatId = ChatId::User(101);
    const BOB: ChatId = ChatId::User(202);

    fn message(id: i32, media: Option<Media>) -> RawHistoryMessage {
        RawHistoryMessage {
            id,
            date: 1_700_000_000 + id,
            from: None,
            out: false,
            text: String::new(),
            media,
            reply_to: None,
            reply_elsewhere: None,
            forwarded: false,
            forwarded_from: None,
            edited: false,
            reactions: Vec::new(),
            pinned: false,
        }
    }

    fn voice(size: u64) -> Media {
        Media {
            duration_s: Some(3),
            size_bytes: Some(size),
            mime_type: Some("audio/ogg".into()),
            ..Media::from(MediaKind::Voice)
        }
    }

    fn fake(alice: Vec<RawHistoryMessage>) -> Arc<FakeTelegram> {
        Arc::new(FakeTelegram::new(
            FakeData::new(default_account())
                .with_messages(ALICE, alice)
                .with_messages(BOB, vec![message(900, Some(voice(10)))]),
        ))
    }

    fn alice() -> ChatRef {
        ChatRef::new(ALICE, fake_access_hash(ALICE))
    }

    /// Every part of message `id`'s file, through a fresh [`Download`].
    async fn download(fake: &Arc<FakeTelegram>, id: i32) -> Result<Vec<u8>, TgError> {
        let file = fake.media_file(alice(), id).await?;
        let mut download = Download::new(fake.clone(), alice(), id, &file);
        let mut bytes = Vec::new();
        while let Some(part) = download.next_part().await? {
            assert!(!part.is_empty(), "an empty part before the end");
            bytes.extend(part);
        }
        assert_eq!(download.served(), bytes.len() as u64);
        Ok(bytes)
    }

    #[tokio::test]
    async fn the_bytes_served_are_the_file_whatever_its_size() {
        const PART: u64 = PART_SIZE as u64;
        let sizes = [
            0,
            1,
            4095,
            4096,
            PART - 1,
            PART,
            PART + 1,
            // Across 1 MB boundaries, ending mid-part.
            2 * 1024 * 1024 + 1,
            5 * PART + 4097,
        ];
        let messages = sizes
            .iter()
            .zip(1..)
            .map(|(&size, id)| message(id, Some(voice(size))))
            .collect();
        let fake = fake(messages);
        for (&size, id) in sizes.iter().zip(1..) {
            let before = fake.files().requests().len();
            let bytes = download(&fake, id).await.unwrap();
            assert_eq!(bytes.len() as u64, size);
            assert!(
                bytes == fake_file_bytes(fake_file_id(ALICE, id), size),
                "size {size}: the bytes differ from the file"
            );
            // One request per part, each a whole part at its offset (the
            // fake refuses misaligned ones, as Telegram does).
            let requests = &fake.files().requests()[before..];
            assert_eq!(requests.len() as u64, size.div_ceil(PART), "size {size}");
            for (i, request) in requests.iter().enumerate() {
                assert_eq!(
                    *request,
                    FileRequestSeen {
                        dc_id: FAKE_FILE_DC,
                        offset: i as i64 * PART_SIZE as i64,
                        limit: PART_SIZE,
                    }
                );
            }
        }
    }

    #[tokio::test]
    async fn a_file_in_another_dc_is_fetched_there_after_one_migration() {
        let fake = fake(vec![message(1, Some(voice(3 * PART_SIZE as u64)))]);
        fake.files().move_to_dc(4);
        let bytes = download(&fake, 1).await.unwrap();
        assert!(bytes == fake_file_bytes(fake_file_id(ALICE, 1), 3 * PART_SIZE as u64));
        let dcs: Vec<i32> = fake.files().requests().iter().map(|r| r.dc_id).collect();
        assert_eq!(dcs, [FAKE_FILE_DC, 4, 4, 4]);
    }

    #[tokio::test]
    async fn an_expired_file_reference_is_refreshed_from_the_message_once() {
        let size = 3 * PART_SIZE as u64;
        let fake = fake(vec![message(1, Some(voice(size)))]);
        let file = fake.media_file(alice(), 1).await.unwrap();
        let mut download = Download::new(fake.clone(), alice(), 1, &file);
        let mut bytes = download.next_part().await.unwrap().unwrap();
        // Telegram expires it mid-download.
        fake.files().expire_references();
        while let Some(part) = download.next_part().await.unwrap() {
            bytes.extend(part);
        }
        assert!(bytes == fake_file_bytes(fake_file_id(ALICE, 1), size));
        assert_eq!(fake.files().lookups(), 2, "found, then refreshed once");
        assert_eq!(fake.files().requests().len(), 4, "the refused part again");
    }

    #[tokio::test]
    async fn a_thumbnails_expired_reference_is_refreshed_from_the_message_too() {
        use crate::tg::{
            fake::{FakeFormat, FakePicture, FakeSize, fake_picture_bytes},
            view::SizeForm,
        };
        let video = Media {
            mime_type: Some("video/mp4".into()),
            size_bytes: Some(3 * PART_SIZE as u64),
            ..Media::from(MediaKind::Video)
        };
        let fake = Arc::new(FakeTelegram::new(
            FakeData::new(default_account())
                .with_messages(ALICE, vec![message(1, Some(video))])
                .with_pictures(
                    ALICE,
                    vec![FakePicture {
                        message_id: 1,
                        grouped_id: None,
                        sizes: Some(vec![FakeSize::new("m", SizeForm::File, (320, 180), 9_000)]),
                    }],
                ),
        ));
        let album = fake.media_album(alice(), 1).await.unwrap();
        let picture = album.items[0].picture.clone().unwrap();
        let file = MediaFile {
            media: album.items[0].media.clone(),
            location: picture.at(&picture.sizes[0]),
            size: 9_000,
        };
        // Telegram expires it between the lookup and the download.
        fake.files().expire_references();
        let mut download = Download::new(fake.clone(), alice(), 1, &file);
        let bytes = download.next_part().await.unwrap().unwrap();
        assert!(
            bytes == fake_picture_bytes(fake_file_id(ALICE, 1), "m", 9_000, FakeFormat::Jpeg),
            "the thumbnail, not the video"
        );
        assert_eq!(fake.files().lookups(), 1, "refreshed once");
        assert_eq!(fake.files().requests().len(), 2, "the refused part again");
    }

    #[tokio::test]
    async fn a_reference_that_keeps_expiring_ends_the_download_after_one_refresh() {
        let fake = fake(vec![message(1, Some(voice(10)))]);
        fake.files().references_never_last();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), download(&fake, 1))
            .await
            .expect("no refresh loop");
        assert!(matches!(result, Err(TgError::Upstream(_))), "{result:?}");
        assert_eq!(fake.files().lookups(), 2, "found, then refreshed once");
        assert_eq!(fake.files().requests().len(), 2);
    }

    #[tokio::test]
    async fn a_files_flood_mid_download_ends_it_without_a_retry() {
        let fake = fake(vec![message(1, Some(voice(3 * PART_SIZE as u64)))]);
        fake.files().flood_after(1, 30);
        let file = fake.media_file(alice(), 1).await.unwrap();
        let mut download = Download::new(fake.clone(), alice(), 1, &file);
        assert!(download.next_part().await.unwrap().is_some());
        let flooded = download.next_part().await;
        assert!(
            matches!(
                flooded,
                Err(TgError::FloodWait {
                    seconds: 30,
                    class: MethodClass::Files
                })
            ),
            "{flooded:?}"
        );
        assert_eq!(fake.files().requests().len(), 2, "never retried around");
    }

    #[tokio::test]
    async fn a_file_telegram_ends_early_is_an_error_not_a_short_download() {
        let size = 2 * PART_SIZE as u64 + 10;
        for cut in [0, 100, PART_SIZE as u64, size - 1] {
            let fake = fake(vec![message(1, Some(voice(size)))]);
            fake.files().cut_files_at(cut);
            let result = download(&fake, 1).await;
            assert!(
                matches!(&result, Err(TgError::Upstream(e)) if e.contains("ended the file")),
                "cut at {cut}: {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn only_media_with_a_file_in_the_asked_chat_is_found() {
        let fake = fake(vec![
            message(1, Some(voice(10))),
            message(2, None),
            message(3, Some(MediaKind::Location.into())),
            message(4, Some(MediaKind::Poll.into())),
            message(5, Some(MediaKind::Contact.into())),
            message(6, Some(MediaKind::Other.into())),
            // A photo that is gone: Telegram says only that it was a photo.
            message(7, Some(MediaKind::Photo.into())),
        ]);
        let error = |id| {
            let fake = fake.clone();
            async move {
                match fake.media_file(alice(), id).await {
                    Err(TgError::InvalidInput(message)) => message,
                    other => panic!("{id}: {other:?}"),
                }
            }
        };
        assert!(error(2).await.contains("has no media"));
        assert!(error(3).await.contains("is a location: it has no file"));
        assert!(error(4).await.contains("is a poll"));
        assert!(error(5).await.contains("is a contact"));
        assert!(error(6).await.contains("no file this server can download"));
        assert!(error(7).await.contains("no longer available"));
        assert!(error(8).await.contains("not found in this chat"));
        // Bob's message: outside channels the id finds it, but it isn't Alice's.
        assert!(error(900).await.contains("not found in this chat"));

        let file = fake.media_file(alice(), 1).await.unwrap();
        assert_eq!(file.media, voice(10));
        assert_eq!(file.size, 10);
    }
}
