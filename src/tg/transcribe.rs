//! Telegram's own transcription of a voice message or video message (#33),
//! independent of grammers/TL.
//!
//! The audio never leaves Telegram: [`transcribe`] asks Telegram for the
//! text (`messages.transcribeAudio`, core.telegram.org/api/transcribe) and
//! nothing else. It finds the message first ([`download::locate`]: one
//! `getMessages`, the asked Chat's message only) and refuses anything but a
//! voice message or a video message before Telegram is asked.
//!
//! Telegram may answer `pending`: the final text then comes in
//! `updateTranscribedAudio` updates with the same `transcription_id`. Like
//! Telegram's own apps (TDLib's `TranscriptionManager`, Telegram Desktop's
//! `Api::Transcribes`), [`transcribe`] asks once and listens for those
//! updates ([`TranscriptionFeed`], subscribed before asking so none is
//! missed), never asking again while it waits: the docs don't say whether a
//! second `transcribeAudio` for the same message spends the weekly trial of
//! an Account without Premium again, so one tool call never makes one. At
//! the deadline it gives up waiting and says `pending`; calling again later
//! asks Telegram once more, as an app does after a restart, and Telegram
//! answers from its own cache. Nothing of the text is kept here.
//!
//! Telegram's refusals become the caller's errors ([`Refusal`]); a
//! `FLOOD_WAIT` (method class Transcribe: without Premium, a used-up trial
//! is one, lasting until the trial resets) is the adapter's limiter's to
//! honour, never retried around here.

use std::{fmt, future::Future, time::Duration};

use tokio::{sync::broadcast, time::Instant};

use super::{
    TgError,
    chats::ChatRef,
    download::{self, RawMediaMessage},
    messages::{MediaKind, MessageRequest},
};

/// How long one get_media `transcribe` waits for Telegram's final text,
/// counted from when the tool was called.
pub const TRANSCRIBE_WAIT: Duration = Duration::from_secs(20);

/// What a `pending` result suggests waiting before calling again.
pub const RETRY_AFTER_S: u32 = 10;

/// Updates kept for a subscriber that is behind; only transcription updates
/// go through a feed, so a few are plenty.
const FEED_CAPACITY: usize = 64;

/// What one `messages.transcribeAudio` answered (`messages.transcribedAudio`).
#[derive(Clone, PartialEq, Eq)]
pub struct TranscribedAudio {
    /// Telegram is still transcribing: `text` is partial, and the rest comes
    /// in [`TranscriptionUpdate`]s with this `transcription_id`.
    pub pending: bool,
    pub transcription_id: i64,
    /// What was said: content, never logged.
    pub text: String,
    /// Only for an Account without Premium.
    pub trial: Option<Trial>,
}

impl fmt::Debug for TranscribedAudio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TranscribedAudio")
            .field("pending", &self.pending)
            .field("transcription_id", &self.transcription_id)
            .field(
                "text",
                &format_args!("<{} chars>", self.text.chars().count()),
            )
            .field("trial", &self.trial)
            .finish()
    }
}

/// The weekly free transcriptions of an Account without Premium
/// (`trial_remains_num`, `trial_remains_until_date`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trial {
    /// Transcriptions left.
    pub remaining: u32,
    /// When `remaining` is reset to the weekly number (unix time).
    pub resets_at: i32,
}

/// One `updateTranscribedAudio`: transcription `transcription_id`'s text so
/// far, final unless `pending`.
#[derive(Clone, PartialEq, Eq)]
pub struct TranscriptionUpdate {
    pub transcription_id: i64,
    pub pending: bool,
    pub text: String,
}

impl fmt::Debug for TranscriptionUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TranscriptionUpdate")
            .field("transcription_id", &self.transcription_id)
            .field("pending", &self.pending)
            .field(
                "text",
                &format_args!("<{} chars>", self.text.chars().count()),
            )
            .finish()
    }
}

/// Where the `updateTranscribedAudio` updates of one Account's Telegram
/// session go, for the transcriptions waiting for them. Held in memory only,
/// passed on as they come.
pub type TranscriptionFeed = broadcast::Sender<TranscriptionUpdate>;

pub fn feed() -> TranscriptionFeed {
    broadcast::channel(FEED_CAPACITY).0
}

/// Telegram's own refusals of `messages.transcribeAudio`
/// (core.telegram.org/method/messages.transcribeAudio).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// `PREMIUM_ACCOUNT_REQUIRED`: no Premium, and no trial to use.
    PremiumRequired,
    /// `MSG_VOICE_MISSING`: the message has no voice to transcribe.
    VoiceMissing,
    /// `MSG_VOICE_TOO_LONG`: longer than Telegram transcribes (for a trial,
    /// `transcribe_audio_trial_duration_max`).
    TooLong,
    /// `TRANSCRIPTION_FAILED`.
    Failed,
}

impl Refusal {
    /// The refusal an RPC error name stands for, if it is one.
    pub fn of(name: &str) -> Option<Self> {
        Some(match name {
            "PREMIUM_ACCOUNT_REQUIRED" => Self::PremiumRequired,
            "MSG_VOICE_MISSING" => Self::VoiceMissing,
            "MSG_VOICE_TOO_LONG" => Self::TooLong,
            "TRANSCRIPTION_FAILED" => Self::Failed,
            _ => return None,
        })
    }

    /// What the caller is told: what happened and what to do instead.
    pub fn error(self, id: i32) -> TgError {
        TgError::InvalidInput(match self {
            Self::PremiumRequired => format!(
                "Telegram refused to transcribe message {id}: transcription needs Telegram \
                 Premium on this account (or one of the few free ones a week, which this \
                 account doesn't get). Use mode download and transcribe the file locally."
            ),
            Self::VoiceMissing => format!(
                "Telegram has no voice to transcribe in message {id}. Use mode download \
                 for the file."
            ),
            Self::TooLong => format!(
                "message {id} is too long for Telegram to transcribe (without Premium, free \
                 transcriptions only take short messages). Use mode download and transcribe \
                 the file locally."
            ),
            Self::Failed => format!(
                "Telegram couldn't transcribe message {id} (e.g. no speech in it). Use mode \
                 download and transcribe the file locally."
            ),
        })
    }
}

/// What get_media `transcribe` found.
#[derive(Clone, PartialEq, Eq)]
pub struct Transcription {
    /// `voice` or `video_note`.
    pub kind: MediaKind,
    pub transcript: Transcript,
    pub trial: Option<Trial>,
}

impl fmt::Debug for Transcription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Transcription")
            .field("kind", &self.kind)
            .field("transcript", &self.transcript)
            .field("trial", &self.trial)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum Transcript {
    /// Telegram's final text.
    Done(String),
    /// Still being transcribed when the wait ended.
    Pending,
}

impl fmt::Debug for Transcript {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Done(text) => write!(f, "Done(<{} chars>)", text.chars().count()),
            Self::Pending => f.write_str("Pending"),
        }
    }
}

/// Telegram's transcription of message `id` of `chat`, waiting for the final
/// text until `deadline`.
///
/// `get_message` is one `getMessages` for the id (as [`download::locate`]
/// takes it); `transcribe_audio` is the one `messages.transcribeAudio` call;
/// `feed` is where this Account's transcription updates arrive.
pub async fn transcribe<G, GFut, C, CFut>(
    chat: ChatRef,
    id: i32,
    deadline: Instant,
    get_message: G,
    feed: &TranscriptionFeed,
    transcribe_audio: C,
) -> Result<Transcription, TgError>
where
    G: FnOnce(MessageRequest) -> GFut,
    GFut: Future<Output = Result<Vec<RawMediaMessage>, TgError>>,
    C: FnOnce() -> CFut,
    CFut: Future<Output = Result<TranscribedAudio, TgError>>,
{
    let kind = download::locate(chat, id, get_message).await?.media.kind;
    if !matches!(kind, MediaKind::Voice | MediaKind::VideoNote) {
        return Err(TgError::InvalidInput(not_transcribable(id, kind)));
    }
    // Before asking, so an update that comes right after the answer is kept.
    let mut updates = feed.subscribe();
    let answer = transcribe_audio().await?;
    let done = |text| Transcription {
        kind,
        transcript: Transcript::Done(text),
        trial: answer.trial,
    };
    if !answer.pending {
        return Ok(done(answer.text));
    }
    let wanted = answer.transcription_id;
    tracing::info!(
        message_id = id,
        transcription_id = wanted,
        "transcription pending; waiting for telegram's update"
    );
    let final_text = async {
        loop {
            match updates.recv().await {
                Ok(update) if update.transcription_id == wanted && !update.pending => {
                    return Some(update.text);
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                // The Telegram session is gone: nothing more will come.
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    };
    Ok(match tokio::time::timeout_at(deadline, final_text).await {
        Ok(Some(text)) => done(text),
        Ok(None) | Err(_) => Transcription {
            kind,
            transcript: Transcript::Pending,
            trial: answer.trial,
        },
    })
}

fn not_transcribable(id: i32, kind: MediaKind) -> String {
    let instead = match kind {
        MediaKind::Photo | MediaKind::Sticker | MediaKind::Gif | MediaKind::Video => {
            "mode view shows it, mode download gets the file"
        }
        MediaKind::Location | MediaKind::Contact | MediaKind::Poll | MediaKind::Other => {
            "get_messages describes it"
        }
        _ => "mode download gets the file, to transcribe locally",
    };
    format!(
        "message {id} is a {}: a {} can't be transcribed. Telegram transcribes only voice \
         messages and video messages (video_note); {instead}.",
        kind.name(),
        kind.name()
    )
}

#[cfg(test)]
mod tests {
    use std::{
        future::ready,
        sync::{Arc, Mutex},
    };

    use super::*;
    use crate::{
        limits::MethodClass,
        tg::{
            chats::ChatId,
            media::{FileLocation, StoredFile},
            messages::{Media, MessageAt},
        },
    };

    const CHAT: ChatId = ChatId::User(7);

    fn message(id: i32, kind: MediaKind) -> RawMediaMessage {
        RawMediaMessage {
            at: MessageAt { chat: CHAT, id },
            media: Some(Media::from(kind)),
            file: Some((
                FileLocation {
                    file: StoredFile::Document,
                    id: 1,
                    access_hash: 2,
                    file_reference: vec![3],
                    dc_id: 2,
                },
                Some(9_000),
            )),
        }
    }

    fn found(
        kind: MediaKind,
    ) -> impl FnOnce(MessageRequest) -> std::future::Ready<Result<Vec<RawMediaMessage>, TgError>>
    {
        move |request| ready(Ok(vec![message(request.id, kind)]))
    }

    fn answer(pending: bool, text: &str) -> TranscribedAudio {
        TranscribedAudio {
            pending,
            transcription_id: 77,
            text: text.into(),
            trial: None,
        }
    }

    fn update(transcription_id: i64, pending: bool, text: &str) -> TranscriptionUpdate {
        TranscriptionUpdate {
            transcription_id,
            pending,
            text: text.into(),
        }
    }

    /// `updates` sent to `feed` at their offsets from now, like Telegram
    /// pushing them after its answer.
    fn push_later(feed: &TranscriptionFeed, updates: Vec<(u64, TranscriptionUpdate)>) {
        let feed = feed.clone();
        tokio::spawn(async move {
            let start = Instant::now();
            for (at, update) in updates {
                tokio::time::sleep_until(start + Duration::from_secs(at)).await;
                let _ = feed.send(update);
            }
        });
    }

    async fn run(
        kind: MediaKind,
        feed: &TranscriptionFeed,
        answer: Result<TranscribedAudio, TgError>,
        calls: &Arc<Mutex<u32>>,
    ) -> Result<Transcription, TgError> {
        let calls = calls.clone();
        transcribe(
            ChatRef::from(CHAT),
            5,
            Instant::now() + TRANSCRIBE_WAIT,
            found(kind),
            feed,
            move || {
                *calls.lock().unwrap() += 1;
                ready(answer)
            },
        )
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn a_final_answer_is_the_text_at_once() {
        let (feed, calls) = (feed(), Arc::default());
        let start = Instant::now();
        let got = run(MediaKind::Voice, &feed, Ok(answer(false, "hello")), &calls)
            .await
            .unwrap();
        assert_eq!(got.transcript, Transcript::Done("hello".into()));
        assert_eq!(got.kind, MediaKind::Voice);
        assert_eq!(start.elapsed(), Duration::ZERO);
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_pending_answer_waits_for_its_own_final_update_only() {
        let (feed, calls) = (feed(), Arc::default());
        push_later(
            &feed,
            vec![
                (1, update(77, true, "hel")),
                // Another message's transcription finishing first.
                (2, update(78, false, "someone else")),
                (4, update(77, false, "hello there")),
            ],
        );
        let start = Instant::now();
        let got = run(MediaKind::VideoNote, &feed, Ok(answer(true, "he")), &calls)
            .await
            .unwrap();
        assert_eq!(got.transcript, Transcript::Done("hello there".into()));
        assert_eq!(start.elapsed(), Duration::from_secs(4));
        assert_eq!(*calls.lock().unwrap(), 1, "asked once, then only listened");
    }

    #[tokio::test(start_paused = true)]
    async fn an_update_right_after_the_answer_is_not_missed() {
        let (feed, calls) = (feed(), Arc::<Mutex<u32>>::default());
        let sender = feed.clone();
        let got = transcribe(
            ChatRef::from(CHAT),
            5,
            Instant::now() + TRANSCRIBE_WAIT,
            found(MediaKind::Voice),
            &feed,
            move || {
                *calls.lock().unwrap() += 1;
                // Telegram's update overtakes its own answer.
                sender.send(update(77, false, "fast")).unwrap();
                ready(Ok(answer(true, "")))
            },
        )
        .await
        .unwrap();
        assert_eq!(got.transcript, Transcript::Done("fast".into()));
    }

    #[tokio::test(start_paused = true)]
    async fn the_wait_ends_at_the_deadline_with_pending() {
        let (feed, calls) = (feed(), Arc::default());
        push_later(
            &feed,
            vec![
                (5, update(77, true, "still")),
                (25, update(77, false, "late")),
            ],
        );
        let start = Instant::now();
        let got = run(MediaKind::Voice, &feed, Ok(answer(true, "st")), &calls)
            .await
            .unwrap();
        assert_eq!(got.transcript, Transcript::Pending);
        assert_eq!(start.elapsed(), TRANSCRIBE_WAIT);
        assert_eq!(*calls.lock().unwrap(), 1, "never asked again while waiting");
    }

    #[tokio::test(start_paused = true)]
    async fn the_trial_left_comes_with_the_result() {
        let (feed, calls) = (feed(), Arc::default());
        let trial = Some(Trial {
            remaining: 1,
            resets_at: 1_794_000_000,
        });
        push_later(&feed, vec![(1, update(77, false, "x"))]);
        let got = run(
            MediaKind::Voice,
            &feed,
            Ok(TranscribedAudio {
                trial,
                ..answer(true, "")
            }),
            &calls,
        )
        .await
        .unwrap();
        assert_eq!(got.trial, trial);
    }

    #[tokio::test(start_paused = true)]
    async fn only_voice_and_video_messages_are_sent_to_telegram() {
        for kind in [
            MediaKind::Photo,
            MediaKind::Video,
            MediaKind::Audio,
            MediaKind::Document,
            MediaKind::Sticker,
            MediaKind::Gif,
        ] {
            let (feed, calls) = (feed(), Arc::default());
            let err = run(kind, &feed, Ok(answer(false, "x")), &calls)
                .await
                .unwrap_err();
            let message = err.to_string();
            assert!(
                matches!(err, TgError::InvalidInput(_))
                    && message.contains(&format!("{} can't be transcribed", kind.name()))
                    && message.contains("mode download"),
                "{message}"
            );
            assert_eq!(*calls.lock().unwrap(), 0, "{kind:?} never reached Telegram");
        }
        let photo = not_transcribable(5, MediaKind::Photo);
        assert!(photo.contains("mode view"), "{photo}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_message_of_another_chat_is_never_transcribed() {
        let (feed, calls) = (feed(), Arc::<Mutex<u32>>::default());
        let counted = calls.clone();
        let err = transcribe(
            ChatRef::from(CHAT),
            5,
            Instant::now() + TRANSCRIBE_WAIT,
            |request| {
                let mut elsewhere = message(request.id, MediaKind::Voice);
                elsewhere.at.chat = ChatId::User(8);
                ready(Ok(vec![elsewhere]))
            },
            &feed,
            move || {
                *counted.lock().unwrap() += 1;
                ready(Ok(answer(false, "x")))
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not found in this chat"), "{err}");
        assert_eq!(*calls.lock().unwrap(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_flood_ends_the_call_without_asking_again() {
        let (feed, calls) = (feed(), Arc::default());
        let err = run(
            MediaKind::Voice,
            &feed,
            Err(TgError::FloodWait {
                seconds: 86_400,
                class: MethodClass::Transcribe,
            }),
            &calls,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            TgError::FloodWait {
                seconds: 86_400,
                ..
            }
        ));
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[test]
    fn telegrams_refusals_say_what_to_do_instead() {
        for (name, refusal) in [
            ("PREMIUM_ACCOUNT_REQUIRED", Refusal::PremiumRequired),
            ("MSG_VOICE_MISSING", Refusal::VoiceMissing),
            ("MSG_VOICE_TOO_LONG", Refusal::TooLong),
            ("TRANSCRIPTION_FAILED", Refusal::Failed),
        ] {
            assert_eq!(Refusal::of(name), Some(refusal));
            let message = refusal.error(5).to_string();
            assert!(
                message.contains("message 5") && message.contains("mode download"),
                "{message}"
            );
            assert!(!message.contains(name), "no raw RPC error: {message}");
        }
        assert!(
            Refusal::PremiumRequired
                .error(5)
                .to_string()
                .contains("Premium")
        );
        for other in ["FLOOD_WAIT", "MSG_ID_INVALID", "PEER_ID_INVALID", ""] {
            assert_eq!(Refusal::of(other), None, "{other}");
        }
    }

    #[test]
    fn debug_output_never_carries_the_text() {
        let secret = "мой пароль 1234";
        let all = format!(
            "{:?} {:?} {:?}",
            answer(true, secret),
            update(1, false, secret),
            Transcription {
                kind: MediaKind::Voice,
                transcript: Transcript::Done(secret.into()),
                trial: None,
            }
        );
        assert!(!all.contains(secret), "{all}");
        assert!(all.contains("<15 chars>"), "{all}");
    }
}
