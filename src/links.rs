//! Media links (#36): a short-lived URL that lets whoever holds it download
//! the media of one message, without a Token (an agent's shell has none).
//!
//! ADR-0003: a link is issued by a Token-authenticated `get_media` call and
//! keeps, in memory only, what the download needs: the Grant's Live client
//! (behind `Arc<dyn Telegram>`), the Chat, the message and its file. It works
//! for [`LINK_TTL`] and at most [`MAX_DOWNLOADS`] downloads; revoking the
//! Grant drops its links at once, and a restart loses them all. Nothing about
//! a link is persisted or logged: the store keys links by the SHA-256 of
//! their secret, so even the secret isn't kept.
//!
//! `GET /media/{ticket}` is outside the Bearer middleware and outside CORS
//! (no page has any business reading it), rate-limited per client IP like
//! the other anonymous endpoints, and streams the file from Telegram part by
//! part ([`Download`]) into the response: never written to disk, never
//! buffered whole. Unknown, expired, used-up or revoked links are a bare 404.

use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Path, State},
    http::{HeaderValue, Method, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::get,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use schemars::JsonSchema;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::time::Instant;

use crate::{
    auth::{
        AuthState,
        rate_limit::{self, Endpoint},
    },
    sessions::GrantId,
    tg::{
        Telegram, TgError,
        chats::ChatRef,
        download::{Download, MediaFile},
        messages::{Media, MediaKind},
    },
};

/// How long a Media link works after it is issued.
pub const LINK_TTL: Duration = Duration::from_secs(5 * 60);
/// How many downloads one Media link allows.
pub const MAX_DOWNLOADS: u32 = 3;
/// Media links one Grant may hold at once; each keeps its Live client alive.
pub const MAX_LINKS_PER_GRANT: usize = 20;
/// The largest file a Media link is issued for.
pub const MAX_DOWNLOAD_BYTES: u64 = 20 * 1024 * 1024;
/// Longest suggested file name, in characters.
const MAX_FILE_NAME_CHARS: usize = 100;
/// Longest MIME type passed on as `Content-Type`.
const MAX_MIME_CHARS: usize = 100;

/// A Media link as `get_media` hands it out (mode `download`).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MediaDownload {
    /// The Media link. Download it with curl or wget before `expires_at`, at
    /// most 3 times. It is a credential: don't paste it anywhere else.
    pub url: String,
    /// When the link stops working (UTC).
    pub expires_at: String,
    /// What the response's `Content-Type` says: voice messages are
    /// `audio/ogg` (Opus), video notes `video/mp4`, photos `image/jpeg`.
    pub mime_type: String,
    /// Exactly how many bytes the download has.
    pub size_bytes: u64,
    /// Length in whole seconds (voice, audio, video, video note, GIF).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_s: Option<u32>,
    /// A safe local file name: the sender's file name (untrusted content,
    /// stripped of path separators and cut to 100 characters) or
    /// `<kind>-<message id>.<extension>`.
    pub suggested_filename: String,
}

/// One live Media link. Holds no secret: the store's key is its hash.
struct Ticket {
    grant_id: GrantId,
    telegram: Arc<dyn Telegram>,
    chat: ChatRef,
    message_id: i32,
    file: MediaFile,
    mime_type: String,
    file_name: String,
    expires: Instant,
    downloads_left: u32,
}

/// What one download through a Media link needs: taken from its ticket, which
/// stays behind with one download fewer.
pub struct Taken {
    key: [u8; 32],
    grant_id: GrantId,
    telegram: Arc<dyn Telegram>,
    chat: ChatRef,
    message_id: i32,
    file: MediaFile,
    mime_type: String,
    file_name: String,
}

/// No room for another Media link for this Grant.
#[derive(Debug, thiserror::Error)]
#[error(
    "too many media links are open for this account ({MAX_LINKS_PER_GRANT}); each expires 5 \
     minutes after it was made"
)]
pub struct TooManyLinks;

/// The Media links issued and not yet expired, in memory only (like the
/// authorization codes).
pub struct MediaLinks {
    /// `scheme://host[:port]` of the public URL, no trailing slash.
    base_url: String,
    tickets: Mutex<HashMap<[u8; 32], Ticket>>,
}

impl MediaLinks {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            tickets: Mutex::default(),
        }
    }

    fn tickets(&self) -> std::sync::MutexGuard<'_, HashMap<[u8; 32], Ticket>> {
        self.tickets.lock().expect("media links lock")
    }

    /// A new Media link to `file`, message `message_id`'s media in `chat`,
    /// downloaded through `telegram`: the Grant's own Live client, the one
    /// that found the file.
    pub fn issue(
        &self,
        grant_id: &GrantId,
        telegram: Arc<dyn Telegram>,
        chat: ChatRef,
        message_id: i32,
        file: MediaFile,
    ) -> Result<MediaDownload, TooManyLinks> {
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).expect("OS randomness");
        let ticket = URL_SAFE_NO_PAD.encode(secret);
        let mime_type = content_type(&file.media);
        let file_name = suggested_file_name(&file.media, message_id, &mime_type);
        let download = MediaDownload {
            url: format!("{}/media/{ticket}", self.base_url),
            expires_at: expires_at(SystemTime::now() + LINK_TTL),
            mime_type: mime_type.clone(),
            size_bytes: file.size,
            duration_s: file.media.duration_s,
            suggested_filename: file_name.clone(),
        };
        let now = Instant::now();
        let mut tickets = self.tickets();
        tickets.retain(|_, t| t.expires > now);
        if tickets.values().filter(|t| &t.grant_id == grant_id).count() >= MAX_LINKS_PER_GRANT {
            return Err(TooManyLinks);
        }
        tracing::info!(
            grant_id = %grant_id,
            size_bytes = file.size,
            "media link issued"
        );
        tickets.insert(
            key(&ticket),
            Ticket {
                grant_id: grant_id.clone(),
                telegram,
                chat,
                message_id,
                file,
                mime_type,
                file_name,
                expires: now + LINK_TTL,
                downloads_left: MAX_DOWNLOADS,
            },
        );
        Ok(download)
    }

    /// One download of the link `ticket`, if it is known, not expired and not
    /// used up. An expired link is forgotten on the spot.
    pub fn take(&self, ticket: &str) -> Option<Taken> {
        let key = key(ticket);
        let mut tickets = self.tickets();
        let entry = tickets.get_mut(&key)?;
        if entry.expires <= Instant::now() || entry.downloads_left == 0 {
            tickets.remove(&key);
            return None;
        }
        entry.downloads_left -= 1;
        Some(Taken {
            key,
            grant_id: entry.grant_id.clone(),
            telegram: entry.telegram.clone(),
            chat: entry.chat,
            message_id: entry.message_id,
            file: entry.file.clone(),
            mime_type: entry.mime_type.clone(),
            file_name: entry.file_name.clone(),
        })
    }

    /// Give back a download that served no byte (Telegram refused at once).
    fn refund(&self, key: &[u8; 32]) {
        if let Some(ticket) = self.tickets().get_mut(key) {
            ticket.downloads_left = (ticket.downloads_left + 1).min(MAX_DOWNLOADS);
        }
    }

    /// Forget one link: what it leads to is gone.
    fn forget(&self, key: &[u8; 32]) {
        self.tickets().remove(key);
    }

    /// Drop every link of a Grant (it was revoked, or its Telegram session
    /// ended), and with them their hold on its Live client. Returns how many.
    pub fn revoke_grant(&self, grant_id: &GrantId) -> usize {
        let mut tickets = self.tickets();
        let before = tickets.len();
        tickets.retain(|_, t| &t.grant_id != grant_id);
        before - tickets.len()
    }

    /// Drop expired and used-up links. Returns how many.
    pub fn sweep(&self) -> usize {
        let now = Instant::now();
        let mut tickets = self.tickets();
        let before = tickets.len();
        tickets.retain(|_, t| t.expires > now && t.downloads_left > 0);
        before - tickets.len()
    }

    pub fn len(&self) -> usize {
        self.tickets().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn key(ticket: &str) -> [u8; 32] {
    Sha256::digest(ticket.as_bytes()).into()
}

fn expires_at(at: SystemTime) -> String {
    time::OffsetDateTime::from(at)
        .replace_nanosecond(0)
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_default()
}

/// `GET /media/{ticket}`, rate-limited per client IP. Only for the router.
pub fn router(auth: Arc<AuthState>) -> Router {
    Router::new()
        .route(
            "/media/{ticket}",
            get(serve).route_layer(middleware::from_fn_with_state(
                (auth.clone(), Endpoint::Media),
                rate_limit::limit,
            )),
        )
        .with_state(auth)
}

/// Stream the file of a live Media link. `HEAD` (which `get` also routes
/// here) spends no download and answers 405.
async fn serve(
    State(auth): State<Arc<AuthState>>,
    method: Method,
    Path(ticket): Path<String>,
) -> Response {
    if method != Method::GET {
        let mut response = bare(StatusCode::METHOD_NOT_ALLOWED);
        response
            .headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("GET"));
        return response;
    }
    let Some(taken) = auth.links.take(&ticket) else {
        return bare(StatusCode::NOT_FOUND);
    };
    // Revocation drops the Grant's links; a Grant deleted any other way
    // (its Tokens all expired) ends them here.
    match auth.vault.grant_exists(&taken.grant_id).await {
        Ok(true) => {}
        Ok(false) => {
            auth.links.revoke_grant(&taken.grant_id);
            return bare(StatusCode::NOT_FOUND);
        }
        Err(err) => {
            tracing::error!(error = %err, "checking a media link's grant failed");
            auth.links.refund(&taken.key);
            return bare(StatusCode::SERVICE_UNAVAILABLE);
        }
    }

    let mut download = Download::new(
        taken.telegram.clone(),
        taken.chat,
        taken.message_id,
        &taken.file,
    );
    // The first part before the headers: if Telegram refuses at once, the
    // answer can say so (and the download is given back).
    let first = match download.next_part().await {
        Ok(first) => first,
        Err(err) => return refused(&auth, &taken, err),
    };
    tracing::info!(
        grant_id = %taken.grant_id,
        size_bytes = taken.file.size,
        "media link download started"
    );
    let headers = [
        (
            header::CONTENT_TYPE,
            HeaderValue::from_str(&taken.mime_type)
                .unwrap_or(HeaderValue::from_static("application/octet-stream")),
        ),
        (header::CONTENT_LENGTH, HeaderValue::from(taken.file.size)),
        (
            header::CONTENT_DISPOSITION,
            content_disposition(&taken.file_name),
        ),
        (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        (
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ),
        (
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ),
        (
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("default-src 'none'; sandbox"),
        ),
    ];
    (
        headers,
        Body::from_stream(parts(download, first, taken.grant_id)),
    )
        .into_response()
}

/// Telegram refused the first part: say so without detail. A link whose
/// message or file is gone, or whose Grant's session ended, is dead.
fn refused(auth: &AuthState, taken: &Taken, err: TgError) -> Response {
    tracing::warn!(grant_id = %taken.grant_id, error = %err, "media link download refused");
    match err {
        TgError::InvalidInput(_) => {
            auth.links.forget(&taken.key);
            bare(StatusCode::NOT_FOUND)
        }
        TgError::BadSession(_) => {
            auth.links.revoke_grant(&taken.grant_id);
            bare(StatusCode::NOT_FOUND)
        }
        TgError::FloodWait { seconds, .. }
        | TgError::RateLimited { seconds }
        | TgError::WritesLimited { seconds, .. } => {
            auth.links.refund(&taken.key);
            let mut response = bare(StatusCode::SERVICE_UNAVAILABLE);
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(seconds.max(1)));
            response
        }
        TgError::Upstream(_) | TgError::WrongPassword | TgError::PeerFlood => {
            auth.links.refund(&taken.key);
            bare(StatusCode::BAD_GATEWAY)
        }
    }
}

/// The file, part by part: `first`, then each next part when the client is
/// ready for it. Nothing runs in the background, so a client that goes away
/// stops the download. A failure mid-file ends the response early, which the
/// client sees against `Content-Length`.
fn parts(
    download: Download,
    first: Option<Vec<u8>>,
    grant_id: GrantId,
) -> impl futures_util::Stream<Item = Result<Bytes, io::Error>> + Send + 'static {
    enum Next {
        First(Download, Option<Vec<u8>>, GrantId),
        More(Download, GrantId),
        Done,
    }
    // Logged with the last part: once `Content-Length` bytes are out, the
    // stream may never be asked for more.
    let part = |part: Vec<u8>, download: Download, grant_id: GrantId| {
        if download.is_done() {
            tracing::info!(grant_id = %grant_id, bytes = download.served(), "media link download finished");
        }
        Some((Ok(Bytes::from(part)), Next::More(download, grant_id)))
    };
    futures_util::stream::unfold(
        Next::First(download, first, grant_id),
        move |next| async move {
            let (mut download, grant_id) = match next {
                Next::First(download, Some(first), grant_id) => {
                    return part(first, download, grant_id);
                }
                Next::First(download, None, grant_id) | Next::More(download, grant_id) => {
                    (download, grant_id)
                }
                Next::Done => return None,
            };
            match download.next_part().await {
                Ok(Some(next)) => part(next, download, grant_id),
                Ok(None) => None,
                Err(err) => {
                    tracing::warn!(
                        grant_id = %grant_id,
                        bytes = download.served(),
                        error = %err,
                        "media link download failed midway"
                    );
                    Some((Err(io::Error::other("download failed")), Next::Done))
                }
            }
        },
    )
}

/// A status with no detail, never cached.
fn bare(status: StatusCode) -> Response {
    (
        status,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
    )
        .into_response()
}

/// The `Content-Type` of a download: the sender's MIME type if it looks like
/// one, else what the kind always is (Telegram photos are JPEG).
pub fn content_type(media: &Media) -> String {
    if media.kind == MediaKind::Photo {
        return "image/jpeg".into();
    }
    media
        .mime_type
        .as_deref()
        .map(str::to_ascii_lowercase)
        .filter(|m| is_mime_type(m))
        .unwrap_or_else(|| {
            match media.kind {
                MediaKind::Voice => "audio/ogg",
                MediaKind::Audio => "audio/mpeg",
                MediaKind::Video | MediaKind::VideoNote | MediaKind::Gif => "video/mp4",
                MediaKind::Sticker => "image/webp",
                _ => "application/octet-stream",
            }
            .into()
        })
}

/// `type/subtype` of RFC 6838 restricted names, nothing else (no parameters).
fn is_mime_type(mime: &str) -> bool {
    let name = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"!#$&^_.+-".contains(&b))
    };
    mime.len() <= MAX_MIME_CHARS
        && mime
            .split_once('/')
            .is_some_and(|(t, s)| name(t) && name(s))
}

/// What to save a download as: the sender's file name made safe, or
/// `<kind>-<message id>.<extension>`.
pub fn suggested_file_name(media: &Media, message_id: i32, mime_type: &str) -> String {
    media
        .file_name
        .as_deref()
        .map(safe_file_name)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| {
            format!(
                "{}-{message_id}.{}",
                kind_name(media.kind),
                extension(mime_type)
            )
        })
}

/// A file name without path separators, characters file systems refuse,
/// control or hidden characters, leading dots or trailing dots and spaces,
/// cut to [`MAX_FILE_NAME_CHARS`] keeping a short extension.
fn safe_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_control()
                || crate::tg::sanitise::is_format(c)
                || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
            {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned
        .trim_start_matches(['.', ' '])
        .trim_end_matches(['.', ' ']);
    if cleaned.chars().count() <= MAX_FILE_NAME_CHARS {
        return cleaned.to_string();
    }
    let (stem, extension) = match cleaned.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.chars().count() <= 10 => (stem, ext),
        _ => (cleaned, ""),
    };
    let keep = MAX_FILE_NAME_CHARS - extension.chars().count() - usize::from(!extension.is_empty());
    let stem: String = stem.chars().take(keep).collect();
    if extension.is_empty() {
        stem
    } else {
        format!("{}.{extension}", stem.trim_end_matches(['.', ' ']))
    }
}

fn kind_name(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Photo => "photo",
        MediaKind::Video => "video",
        MediaKind::VideoNote => "video_note",
        MediaKind::Voice => "voice",
        MediaKind::Audio => "audio",
        MediaKind::Document => "document",
        MediaKind::Sticker => "sticker",
        MediaKind::Gif => "gif",
        MediaKind::Location => "location",
        MediaKind::Contact => "contact",
        MediaKind::Poll => "poll",
        MediaKind::Other => "media",
    }
}

fn extension(mime_type: &str) -> &'static str {
    match mime_type {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        "audio/ogg" => "ogg",
        "audio/mpeg" => "mp3",
        "audio/mp4" | "audio/x-m4a" => "m4a",
        "application/pdf" => "pdf",
        "application/x-tgsticker" => "tgs",
        "application/zip" => "zip",
        "text/plain" => "txt",
        _ => "bin",
    }
}

/// `attachment` with the file name twice: a plain-ASCII `filename` (anything
/// else becomes `_`, so no quote, backslash or line break can end it) and
/// the full name percent-encoded in `filename*` (RFC 6266 / 8187).
fn content_disposition(name: &str) -> HeaderValue {
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || " ._-+()[],".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded: String = name
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    HeaderValue::from_str(&format!(
        "attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}"
    ))
    .unwrap_or(HeaderValue::from_static("attachment"))
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;

    use super::*;
    use crate::{
        auth::{clients::ClientRegistry, rate_limit::OAuthLimits},
        db,
        sessions::{TokenTtl, Vault},
        tg::{
            FakeConnector, FakeLogin,
            chats::ChatId,
            download::PART_SIZE,
            fake::{FakeData, FakeTelegram, default_account, fake_access_hash},
            messages::RawHistoryMessage,
        },
    };

    const ALICE: ChatId = ChatId::User(101);

    fn voice(size: u64, file_name: Option<&str>) -> Media {
        Media {
            duration_s: Some(9),
            size_bytes: Some(size),
            mime_type: Some("audio/ogg".into()),
            file_name: file_name.map(Into::into),
            ..Media::from(MediaKind::Voice)
        }
    }

    fn fake(size: u64) -> Arc<FakeTelegram> {
        let message = RawHistoryMessage {
            id: 1,
            date: 1_700_000_000,
            from: None,
            out: false,
            text: String::new(),
            media: Some(voice(size, None)),
            reply_to: None,
            reply_elsewhere: None,
            forwarded: false,
            forwarded_from: None,
            edited: false,
            reactions: Vec::new(),
            pinned: false,
            topic: None,
        };
        Arc::new(FakeTelegram::new(
            FakeData::new(default_account()).with_messages(ALICE, vec![message]),
        ))
    }

    fn alice() -> ChatRef {
        ChatRef::new(ALICE, fake_access_hash(ALICE))
    }

    async fn file(fake: &FakeTelegram) -> MediaFile {
        fake.media_file(alice(), 1).await.unwrap()
    }

    fn grant(id: &str) -> GrantId {
        GrantId::from(id.to_string())
    }

    /// The ticket of a link's URL.
    fn ticket(download: &MediaDownload) -> String {
        download.url.rsplit_once('/').unwrap().1.to_string()
    }

    #[tokio::test]
    async fn a_link_is_a_256_bit_secret_under_the_public_url_and_not_kept() {
        let fake = fake(10);
        let links = MediaLinks::new("https://tg.example.com/");
        let issue = || {
            links.issue(
                &grant("g"),
                fake.clone(),
                alice(),
                1,
                futures_executor(file(&fake)),
            )
        };
        let (a, b) = (issue().unwrap(), issue().unwrap());
        let ticket = ticket(&a);
        assert!(
            a.url.starts_with("https://tg.example.com/media/"),
            "{}",
            a.url
        );
        assert_eq!(URL_SAFE_NO_PAD.decode(&ticket).unwrap().len(), 32);
        assert_ne!(a.url, b.url);
        // Only its hash is a key.
        assert!(!links.tickets().contains_key(ticket.as_bytes()));
        assert!(links.tickets().contains_key(&key(&ticket)));
        assert_eq!(
            (a.mime_type.as_str(), a.size_bytes, a.duration_s),
            ("audio/ogg", 10, Some(9))
        );
        assert_eq!(a.suggested_filename, "voice-1.ogg");
    }

    #[tokio::test]
    async fn a_link_serves_three_downloads_then_is_gone() {
        let fake = fake(10);
        let links = MediaLinks::new("http://x");
        let link = links
            .issue(&grant("g"), fake.clone(), alice(), 1, file(&fake).await)
            .unwrap();
        for _ in 0..MAX_DOWNLOADS {
            assert!(links.take(&ticket(&link)).is_some());
        }
        assert!(links.take(&ticket(&link)).is_none());
        assert!(links.is_empty(), "a used-up link is forgotten");
        assert!(links.take("unknown").is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_link_works_for_five_minutes_only() {
        let fake = fake(10);
        let links = MediaLinks::new("http://x");
        let link = links
            .issue(&grant("g"), fake.clone(), alice(), 1, file(&fake).await)
            .unwrap();
        tokio::time::advance(LINK_TTL - Duration::from_secs(1)).await;
        assert!(links.take(&ticket(&link)).is_some());
        assert_eq!(links.sweep(), 0);
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(links.take(&ticket(&link)).is_none());
        assert!(links.is_empty(), "an expired link is forgotten");

        // The sweep drops expired links nobody asks for, and with them their
        // hold on the Live client.
        let link = links
            .issue(&grant("g"), fake.clone(), alice(), 1, file(&fake).await)
            .unwrap();
        let before = Arc::strong_count(&fake);
        tokio::time::advance(LINK_TTL).await;
        assert_eq!(links.sweep(), 1);
        assert_eq!(Arc::strong_count(&fake), before - 1);
        assert!(links.take(&ticket(&link)).is_none());
    }

    #[tokio::test]
    async fn revoking_a_grant_ends_its_links_and_only_its() {
        let fake = fake(10);
        let links = MediaLinks::new("http://x");
        let mine = links
            .issue(&grant("a"), fake.clone(), alice(), 1, file(&fake).await)
            .unwrap();
        let other = links
            .issue(&grant("b"), fake.clone(), alice(), 1, file(&fake).await)
            .unwrap();
        assert_eq!(links.revoke_grant(&grant("a")), 1);
        assert!(links.take(&ticket(&mine)).is_none());
        assert!(links.take(&ticket(&other)).is_some());
    }

    #[tokio::test]
    async fn a_grant_holds_a_bounded_number_of_links() {
        let fake = fake(10);
        let links = MediaLinks::new("http://x");
        let file = file(&fake).await;
        for _ in 0..MAX_LINKS_PER_GRANT {
            links
                .issue(&grant("a"), fake.clone(), alice(), 1, file.clone())
                .unwrap();
        }
        assert!(
            links
                .issue(&grant("a"), fake.clone(), alice(), 1, file.clone())
                .is_err()
        );
        assert!(
            links
                .issue(&grant("b"), fake.clone(), alice(), 1, file)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn the_stream_fetches_a_part_only_when_the_client_takes_one() {
        let size = 4 * PART_SIZE as u64;
        let fake = fake(size);
        let mut download = Download::new(fake.clone(), alice(), 1, &file(&fake).await);
        let first = download.next_part().await.unwrap();
        let mut stream = Box::pin(parts(download, first, grant("g")));
        assert!(stream.next().await.unwrap().is_ok());
        assert!(stream.next().await.unwrap().is_ok());
        assert_eq!(fake.files().requests().len(), 2);
        // The client went away: nothing more is fetched.
        drop(stream);
        tokio::task::yield_now().await;
        assert_eq!(fake.files().requests().len(), 2);
    }

    #[tokio::test]
    async fn a_failure_midway_ends_the_stream_with_an_error() {
        let fake = fake(3 * PART_SIZE as u64);
        fake.files().flood_after(1, 30);
        let mut download = Download::new(fake.clone(), alice(), 1, &file(&fake).await);
        let first = download.next_part().await.unwrap();
        let results: Vec<_> = parts(download, first, grant("g")).collect().await;
        assert_eq!(results.len(), 2, "the first part, then the error");
        assert!(results[0].is_ok() && results[1].is_err());
        assert_eq!(fake.files().requests().len(), 2, "never retried around");
    }

    #[test]
    fn content_types_are_the_senders_only_when_they_look_like_one() {
        let with = |kind: MediaKind, mime: Option<&str>| {
            content_type(&Media {
                mime_type: mime.map(Into::into),
                ..Media::from(kind)
            })
        };
        assert_eq!(with(MediaKind::Voice, Some("audio/ogg")), "audio/ogg");
        assert_eq!(
            with(MediaKind::Document, Some("Application/PDF")),
            "application/pdf"
        );
        assert_eq!(with(MediaKind::Photo, None), "image/jpeg");
        assert_eq!(with(MediaKind::Voice, None), "audio/ogg");
        assert_eq!(with(MediaKind::VideoNote, None), "video/mp4");
        for odd in [
            "text/html; charset=utf-8",
            "text/html\r\nSet-Cookie: a=b",
            "audio",
            "/ogg",
            "a/b/c",
        ] {
            assert_eq!(
                with(MediaKind::Document, Some(odd)),
                "application/octet-stream",
                "{odd:?}"
            );
        }
    }

    #[test]
    fn suggested_file_names_are_safe_and_bounded() {
        let named = |name: &str| suggested_file_name(&voice(1, Some(name)), 7, "audio/ogg");
        assert_eq!(named("билеты.pdf"), "билеты.pdf");
        assert_eq!(named("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(named("C:\\Windows\\a.exe"), "C__Windows_a.exe");
        assert_eq!(named(".hidden"), "hidden");
        assert_eq!(named("in\u{202E}voice.pdf"), "in_voice.pdf");
        assert_eq!(named("a\r\nb.txt"), "a__b.txt");
        assert_eq!(named(" .. "), "voice-7.ogg");
        let long = named(&format!("{}.pdf", "n".repeat(300)));
        assert_eq!(long.chars().count(), MAX_FILE_NAME_CHARS);
        assert!(long.ends_with("n.pdf"), "{long}");
        assert_eq!(
            suggested_file_name(&Media::from(MediaKind::VideoNote), 525_380, "video/mp4"),
            "video_note-525380.mp4"
        );
    }

    #[test]
    fn content_disposition_cannot_be_broken_out_of() {
        let header = content_disposition("a\"; filename=evil.html\r\nX: y\\билет.pdf");
        let header = header.to_str().unwrap();
        assert_eq!(
            header,
            "attachment; filename=\"a__ filename_evil.html__X_ y______.pdf\"; \
             filename*=UTF-8''a%22%3B%20filename%3Devil.html%0D%0AX%3A%20y%5C%D0%B1%D0%B8%D0%BB%D0%B5%D1%82.pdf"
        );
    }

    async fn auth_state() -> (Arc<AuthState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(&dir.path().join("t.db")).await.unwrap();
        let auth = AuthState::new(
            "http://x".into(),
            Arc::new(Vault::new(pool.clone(), TokenTtl::default())),
            ClientRegistry::new(pool),
            Arc::new(FakeLogin::default()),
            Arc::new(FakeConnector::default()),
            Arc::new(MediaLinks::new("http://x")),
            OAuthLimits::default(),
        );
        (Arc::new(auth), dir)
    }

    async fn get(auth: &Arc<AuthState>, ticket: &str) -> Response {
        serve(State(auth.clone()), Method::GET, Path(ticket.to_string())).await
    }

    #[tokio::test]
    async fn a_link_of_a_grant_deleted_any_way_is_dead() {
        let (auth, _dir) = auth_state().await;
        let issued = auth.vault.create_grant("client", 42, b"{}").await.unwrap();
        let grant_id = auth
            .vault
            .open(issued.access.expose())
            .await
            .unwrap()
            .grant_id
            .clone();
        let fake = fake(10);
        let issue = || async {
            let file = file(&fake).await;
            ticket(
                &auth
                    .links
                    .issue(&grant_id, fake.clone(), alice(), 1, file)
                    .unwrap(),
            )
        };
        let live = issue().await;
        let dead = issue().await;
        assert_eq!(get(&auth, &live).await.status(), StatusCode::OK);

        // Not revoked through us: its Tokens all expired and it was purged.
        auth.vault.delete_grant(&grant_id).await.unwrap();
        assert_eq!(get(&auth, &dead).await.status(), StatusCode::NOT_FOUND);
        assert!(auth.links.is_empty(), "every link of that grant is dropped");
    }

    #[tokio::test]
    async fn head_spends_no_download() {
        let (auth, _dir) = auth_state().await;
        let fake = fake(10);
        let link = auth
            .links
            .issue(&grant("g"), fake.clone(), alice(), 1, file(&fake).await)
            .unwrap();
        for _ in 0..5 {
            let response = serve(State(auth.clone()), Method::HEAD, Path(ticket(&link))).await;
            assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        }
        assert!(fake.files().requests().is_empty());
        assert_eq!(
            auth.links.tickets()[&key(&ticket(&link))].downloads_left,
            MAX_DOWNLOADS
        );
    }

    fn futures_executor<T>(future: impl std::future::Future<Output = T>) -> T {
        futures_util::FutureExt::now_or_never(future).expect("a fake answers at once")
    }
}
