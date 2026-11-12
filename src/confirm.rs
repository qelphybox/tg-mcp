//! Pending sends and their confirmation page (#45, ADR-0005).
//!
//! A message that needs the user's confirmation becomes a pending send: the
//! message (Chat, text and entities, `reply_to`, `topic`, and the
//! `random_id` it will be sent with), what the page shows of it, and a
//! reference to the Grant's Live client, held **in memory only** for
//! [`PENDING_TTL`] like an authorization code or a Media link (ADR-0003). It
//! is decided once (sent, cancelled or made a Draft) and a restart drops it.
//! Its URL is `<public base>/confirm/<256-bit secret>`; the store keys it by
//! the secret's SHA-256, and neither is ever logged, nor is the text.
//!
//! `GET /confirm/{id}` shows the Chat, the text as it will look, the message
//! replied to and who asks; `POST /confirm/{id}` decides, with a CSRF token,
//! under a strict CSP. Sending and cancelling need the Confirmation cookie
//! the Consent screen set in its browser ([`cookie`]); without it the page
//! only offers "Make it a Draft", so the model's own shell, which has the URL
//! but not the cookie, cannot send. Both are rate limited per client IP.
//!
//! A tool waits for the decision ([`PendingSends::wait`]) when its client
//! surfaces the page through URL elicitation, and otherwise hands out the
//! URL ([`crate::mcp`]). The send itself is the Live client's
//! `messages.sendMessage`, so the Writes budget and the Chat's Send cap count
//! it when the user confirms, and may still refuse it then.

pub mod cookie;
mod page;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use axum::{
    Router,
    extract::{DefaultBodyLimit, Form, Path, State},
    http::{HeaderMap, StatusCode, header},
    middleware,
    response::Response,
    routing::get,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::{sync::watch, time::Instant};

use crate::{
    auth::{
        AuthState, Caller,
        rate_limit::{self, Endpoint},
    },
    limits::WriteLimit,
    links::expires_at,
    sessions::GrantId,
    tg::{
        Telegram, TgError,
        chats::{ChatId, ChatKind},
        compose,
        drafts::{NewDraft, SavedDraft},
        markdown::FormattedText,
        messages::HistoryQuery,
        sanitise::Sanitised,
        send::{self, NewMessage, SentMessage},
    },
};

/// How long a pending send waits for the user.
pub const PENDING_TTL: Duration = Duration::from_secs(10 * 60);
/// Pending sends one Grant may have at once; each keeps its Live client.
pub const MAX_PENDING_PER_GRANT: usize = 10;
/// Pending sends the server holds at once.
pub const MAX_PENDING: usize = 1000;
/// Longest Chat or Topic title the page shows, in characters.
const MAX_TITLE_CHARS: usize = 128;
/// Longest preview of the message replied to, in characters.
const MAX_REPLY_PREVIEW_CHARS: usize = 200;

/// What the page shows of a pending send besides its text, read through the
/// Grant's Live client when it is made (sanitised: no hidden characters).
#[derive(Debug, Clone)]
struct Shown {
    chat: ChatId,
    chat_title: String,
    chat_kind: ChatKind,
    is_self: bool,
    /// In a forum: the Topic it goes to, and its title when known.
    topic: Option<(i32, Option<String>)>,
    reply: Option<ReplyPreview>,
}

/// The message a pending send replies to, shortened.
#[derive(Debug, Clone)]
struct ReplyPreview {
    id: i32,
    /// Who wrote it; `None` when the Account did.
    from: Option<String>,
    text: String,
}

/// How a pending send ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The user pressed Send and Telegram took it.
    Sent(SentMessage),
    /// The user pressed "Make it a Draft": it is in the Chat's message field.
    Drafted(SavedDraft),
    /// The user pressed Cancel, or declined to open the page.
    Cancelled,
    /// Nobody decided within [`PENDING_TTL`].
    Expired,
    /// The user confirmed, but nothing was sent (or saved): a limit, a
    /// `FLOOD_WAIT`, `PEER_FLOOD`, Telegram's refusal, or the Grant's end.
    Refused(Refusal),
}

/// Why a confirmed send (or Draft) didn't happen, for the page and the tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// What happened, ours, never Telegram's text: shown to the user and
    /// returned to the model.
    pub message: String,
    /// Seconds before trying again makes sense, for a wait.
    pub retry_after_s: Option<u32>,
}

/// A pending send just made: its URL, and the way to its outcome.
pub struct Requested {
    /// `<public base>/confirm/<id>`: shown to the user (or handed to the
    /// model when its client has no URL elicitation). A credential for
    /// Drafts only; never logged.
    pub url: String,
    /// When it expires (UTC, RFC 3339).
    pub expires_at: String,
    key: [u8; 32],
    expires: Instant,
    outcome: watch::Receiver<Option<Outcome>>,
}

/// Why no pending send was made.
#[derive(Debug, thiserror::Error)]
pub enum RequestError {
    /// Chat, reply or Topic don't check out, or Telegram refused to say.
    #[error(transparent)]
    Telegram(#[from] TgError),
    #[error(
        "{MAX_PENDING_PER_GRANT} messages are already waiting for the user's confirmation; each \
         waits 10 minutes at most. Wait for the user, or for them to expire"
    )]
    TooMany,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Waiting,
    /// Telegram is being asked: nobody else may decide, and it doesn't expire.
    Deciding,
}

/// One pending send. Holds no secret of its own but the CSRF token: the
/// store's key is its id's hash.
struct Entry {
    grant_id: GrantId,
    telegram: Arc<dyn Telegram>,
    message: NewMessage,
    shown: Shown,
    /// The Grant's Confirmation cookie verifier; `None` (a Grant sealed
    /// before #45): no browser can send, every browser can make a Draft.
    cookie: Option<[u8; 32]>,
    csrf: String,
    expires: Instant,
    expires_at: String,
    stage: Stage,
    outcome: watch::Sender<Option<Outcome>>,
}

/// The decisions the page offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Send,
    Cancel,
    Draft,
}

impl Decision {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "send" => Some(Self::Send),
            "cancel" => Some(Self::Cancel),
            "draft" => Some(Self::Draft),
            _ => None,
        }
    }

    /// Only the Consent browser may send, or cancel.
    fn needs_cookie(self) -> bool {
        self != Self::Draft
    }
}

/// What the page needs of a pending send.
#[derive(Clone)]
struct View {
    grant_id: GrantId,
    shown: Shown,
    text: FormattedText,
    cookie: Option<[u8; 32]>,
    csrf: String,
    expires_at: String,
    deciding: bool,
}

/// Why a decision wasn't taken up.
#[derive(Debug, PartialEq, Eq)]
enum NotTaken {
    /// Unknown, expired or decided: "no longer valid".
    Gone,
    /// The form's CSRF token isn't this pending send's.
    Csrf,
    /// Send or Cancel without the Confirmation cookie.
    WrongBrowser,
    /// Another decision is with Telegram right now.
    Busy,
}

/// A decision that asks Telegram, taken up: the pending send is `Deciding`
/// until [`PendingSends::finish`].
struct Job {
    key: [u8; 32],
    decision: Decision,
    grant_id: GrantId,
    telegram: Arc<dyn Telegram>,
    message: NewMessage,
}

impl Job {
    async fn run(&self) -> Result<Outcome, TgError> {
        let message = self.message.clone();
        match self.decision {
            Decision::Send => self.telegram.send_message(message).await.map(Outcome::Sent),
            Decision::Draft => self
                .telegram
                .save_draft(NewDraft {
                    chat: message.chat,
                    text: message.text,
                    reply_to: message.reply_to,
                    topic: message.topic,
                })
                .await
                .map(Outcome::Drafted),
            Decision::Cancel => Ok(Outcome::Cancelled),
        }
    }
}

/// After asking Telegram: decided, or back to waiting for another try.
enum Finished {
    Decided(Outcome),
    /// Telegram didn't answer; nothing is known to have happened, and the
    /// same `random_id` makes another try safe.
    TryAgain,
}

/// The pending sends made and not yet decided or expired.
pub struct PendingSends {
    /// `scheme://host[:port]` of the public URL, no trailing slash.
    base_url: String,
    entries: Mutex<HashMap<[u8; 32], Entry>>,
}

impl PendingSends {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            entries: Mutex::default(),
        }
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<[u8; 32], Entry>> {
        self.entries.lock().expect("pending sends lock")
    }

    /// A pending send of `message` for `caller`, who must be allowed to
    /// write to its Chat. Reads what the page shows through the caller's
    /// Live client (the Chat, the message replied to) and checks where it
    /// goes ([`compose::place`]): a forum without a Topic, or a reply to a
    /// message the Chat doesn't have, is an error now, not on the page.
    pub async fn request(
        &self,
        caller: &Caller,
        message: NewMessage,
    ) -> Result<Requested, RequestError> {
        let chat = message.chat.chat();
        caller
            .may_write_to(chat)
            .map_err(|refused| TgError::InvalidInput(refused.to_string()))?;
        let shown = shown(&Sanitised::new(caller.telegram.clone()), &message).await?;
        let cookie = caller
            .send_allow_list
            .as_ref()
            .and_then(|list| list.confirm_cookie().copied());

        let id = URL_SAFE_NO_PAD.encode(crate::sessions::random_bytes::<32>());
        let key = key(&id);
        let now = Instant::now();
        let expires_at = expires_at(SystemTime::now() + PENDING_TTL);
        let (sender, receiver) = watch::channel(None);
        let mut entries = self.entries();
        expire_waiting(&mut entries, now);
        if entries.len() >= MAX_PENDING
            || entries
                .values()
                .filter(|e| e.grant_id == caller.grant_id)
                .count()
                >= MAX_PENDING_PER_GRANT
        {
            return Err(RequestError::TooMany);
        }
        tracing::info!(
            grant_id = %caller.grant_id,
            chat = %chat,
            chars = message.text.text.chars().count(),
            entities = message.text.entities.len(),
            "pending send made"
        );
        entries.insert(
            key,
            Entry {
                grant_id: caller.grant_id.clone(),
                telegram: caller.telegram.clone(),
                message,
                shown,
                cookie,
                csrf: URL_SAFE_NO_PAD.encode(crate::sessions::random_bytes::<32>()),
                expires: now + PENDING_TTL,
                expires_at: expires_at.clone(),
                stage: Stage::Waiting,
                outcome: sender,
            },
        );
        Ok(Requested {
            url: format!("{}/confirm/{id}", self.base_url),
            expires_at,
            key,
            expires: now + PENDING_TTL,
            outcome: receiver,
        })
    }

    /// The outcome of `requested`: once the user decides, or
    /// [`Outcome::Expired`] when nobody did in time. A decision that is with
    /// Telegram at the deadline is waited for.
    pub async fn wait(&self, requested: Requested) -> Outcome {
        let Requested {
            key,
            expires,
            mut outcome,
            ..
        } = requested;
        let mut past_deadline = false;
        loop {
            if let Some(decided) = outcome.borrow_and_update().clone() {
                return decided;
            }
            let deadline = async {
                if past_deadline {
                    std::future::pending::<()>().await;
                }
                tokio::time::sleep_until(expires).await;
            };
            tokio::select! {
                changed = outcome.changed() => {
                    if changed.is_err() {
                        // Dropped without an outcome: it can't be decided any more.
                        return outcome.borrow().clone().unwrap_or(Outcome::Expired);
                    }
                }
                () = deadline => {
                    past_deadline = true;
                    if self.expire(&key) {
                        return Outcome::Expired;
                    }
                }
            }
        }
    }

    /// Cancel `requested` unless a decision is with Telegram already (the
    /// user declined to open the page). Returns whether it was cancelled.
    pub fn cancel(&self, requested: &Requested) -> bool {
        let mut entries = self.entries();
        match entries.get(&requested.key) {
            Some(entry) if entry.stage == Stage::Waiting => {
                let entry = entries.remove(&requested.key).expect("just found");
                tracing::info!(grant_id = %entry.grant_id, chat = %entry.shown.chat, "pending send cancelled by the client");
                entry.outcome.send_replace(Some(Outcome::Cancelled));
                true
            }
            _ => false,
        }
    }

    /// Forget a pending send whose time is up, unless it is being decided.
    fn expire(&self, key: &[u8; 32]) -> bool {
        let mut entries = self.entries();
        match entries.get(key) {
            Some(entry) if entry.stage == Stage::Waiting && entry.expires <= Instant::now() => {
                let entry = entries.remove(key).expect("just found");
                entry.outcome.send_replace(Some(Outcome::Expired));
                true
            }
            _ => false,
        }
    }

    /// Drop every pending send of a Grant (revoked, or its session ended),
    /// and with them their hold on its Live client. Returns how many. One
    /// that is with Telegram right now is left to finish: it may be sent
    /// already, and its outcome must say so.
    pub fn revoke_grant(&self, grant_id: &GrantId) -> usize {
        let mut entries = self.entries();
        let before = entries.len();
        entries.retain(|_, entry| {
            if &entry.grant_id != grant_id || entry.stage == Stage::Deciding {
                return true;
            }
            entry.outcome.send_replace(Some(Outcome::Refused(Refusal {
                message: "this connection was disconnected before the message was confirmed; \
                          nothing was sent"
                    .into(),
                retry_after_s: None,
            })));
            false
        });
        before - entries.len()
    }

    /// Drop pending sends nobody decided in time. Returns how many.
    pub fn sweep(&self) -> usize {
        let mut entries = self.entries();
        let before = entries.len();
        expire_waiting(&mut entries, Instant::now());
        before - entries.len()
    }

    pub fn len(&self) -> usize {
        self.entries().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// What the page shows of pending send `id`, if it is still valid.
    fn view(&self, id: &str) -> Option<View> {
        let key = key(id);
        let mut entries = self.entries();
        expire_one(&mut entries, &key);
        let entry = entries.get(&key)?;
        Some(View {
            grant_id: entry.grant_id.clone(),
            shown: entry.shown.clone(),
            text: entry.message.text.clone(),
            cookie: entry.cookie,
            csrf: entry.csrf.clone(),
            expires_at: entry.expires_at.clone(),
            deciding: entry.stage == Stage::Deciding,
        })
    }

    /// Take up `decision` on pending send `id`, posted with `csrf`, from a
    /// browser that has the Confirmation cookie (`confirmed_browser`) or
    /// not. Cancel is decided here; Send and Draft come back as a [`Job`]
    /// for Telegram, the pending send `Deciding` until [`Self::finish`].
    fn begin(
        &self,
        id: &str,
        csrf: &str,
        decision: Decision,
        confirmed_browser: bool,
    ) -> Result<Result<Job, Outcome>, NotTaken> {
        let key = key(id);
        let mut entries = self.entries();
        expire_one(&mut entries, &key);
        let entry = entries.get_mut(&key).ok_or(NotTaken::Gone)?;
        if !bool::from(entry.csrf.as_bytes().ct_eq(csrf.as_bytes())) {
            return Err(NotTaken::Csrf);
        }
        if decision.needs_cookie() && !confirmed_browser {
            return Err(NotTaken::WrongBrowser);
        }
        if entry.stage == Stage::Deciding {
            return Err(NotTaken::Busy);
        }
        if decision == Decision::Cancel {
            let entry = entries.remove(&key).expect("just found");
            tracing::info!(grant_id = %entry.grant_id, chat = %entry.shown.chat, "pending send cancelled");
            entry.outcome.send_replace(Some(Outcome::Cancelled));
            return Ok(Err(Outcome::Cancelled));
        }
        entry.stage = Stage::Deciding;
        Ok(Ok(Job {
            key,
            decision,
            grant_id: entry.grant_id.clone(),
            telegram: entry.telegram.clone(),
            message: entry.message.clone(),
        }))
    }

    /// What Telegram answered `job`: the pending send is decided, or, when
    /// Telegram didn't answer, waits again for another try.
    fn finish(&self, job: &Job, answer: Result<Outcome, TgError>) -> Finished {
        let chat = job.message.chat.chat();
        let outcome = match answer {
            Ok(outcome) => outcome,
            Err(TgError::Upstream(err)) => {
                tracing::warn!(grant_id = %job.grant_id, chat = %chat, error = %err, "confirmed write got no answer from telegram; it may be tried again");
                if let Some(entry) = self.entries().get_mut(&job.key) {
                    entry.stage = Stage::Waiting;
                }
                return Finished::TryAgain;
            }
            Err(err) => {
                tracing::warn!(grant_id = %job.grant_id, chat = %chat, error = %err, "confirmed write refused");
                Outcome::Refused(refusal(&err, job.decision))
            }
        };
        match &outcome {
            Outcome::Sent(sent) => tracing::info!(
                grant_id = %job.grant_id,
                chat = %chat,
                message_id = sent.id,
                topic = sent.topic,
                reply_to = sent.reply_to,
                "pending send confirmed: message sent"
            ),
            Outcome::Drafted(saved) => tracing::info!(
                grant_id = %job.grant_id,
                chat = %chat,
                topic = saved.topic,
                reply_to = saved.reply_to,
                "pending send made a draft"
            ),
            _ => {}
        }
        if let Some(entry) = self.entries().remove(&job.key) {
            entry.outcome.send_replace(Some(outcome.clone()));
        }
        Finished::Decided(outcome)
    }

    #[cfg(test)]
    fn entry_expires(&self, url: &str) -> Option<Instant> {
        let id = url.rsplit_once('/')?.1;
        self.entries().get(&key(id)).map(|e| e.expires)
    }
}

impl Requested {
    /// Whether this pending send is still undecided (for tests and logs).
    pub fn is_open(&self) -> bool {
        self.outcome.borrow().is_none()
    }
}

fn key(id: &str) -> [u8; 32] {
    Sha256::digest(id.as_bytes()).into()
}

/// Expire every waiting entry past its time.
fn expire_waiting(entries: &mut HashMap<[u8; 32], Entry>, now: Instant) {
    entries.retain(|_, entry| {
        if entry.stage == Stage::Deciding || entry.expires > now {
            return true;
        }
        entry.outcome.send_replace(Some(Outcome::Expired));
        false
    });
}

fn expire_one(entries: &mut HashMap<[u8; 32], Entry>, key: &[u8; 32]) {
    if entries
        .get(key)
        .is_some_and(|e| e.stage == Stage::Waiting && e.expires <= Instant::now())
        && let Some(entry) = entries.remove(key)
    {
        entry.outcome.send_replace(Some(Outcome::Expired));
    }
}

/// What the page shows of `message`, and the check that it can go where it
/// says: one `get_chat_info`, and for a reply one `get_messages` around it.
async fn shown(telegram: &Sanitised, message: &NewMessage) -> Result<Shown, TgError> {
    let chat = message.chat.chat();
    if message.text.text.trim().is_empty() {
        return Err(TgError::InvalidInput(
            "a message needs text: write some".into(),
        ));
    }
    let info = telegram.get_chat_info(message.chat).await?;
    let replied = match message.reply_to {
        Some(id) => {
            let page = telegram
                .get_messages(
                    message.chat,
                    HistoryQuery {
                        limit: 1,
                        around_id: Some(id),
                        ..HistoryQuery::default()
                    },
                )
                .await?;
            let titles = page.topics;
            let found = page.messages.into_iter().find(|m| m.id == id);
            let Some(found) = found else {
                return Err(TgError::InvalidInput(format!(
                    "reply_to {id} is not a message of {chat}"
                )));
            };
            Some((found, titles))
        }
        None => None,
    };
    let is_forum = info.topics.is_some();
    let reply_topic = replied.as_ref().map(|(m, _)| m.topic);
    let placement = compose::place(
        chat,
        message.reply_to,
        message.topic,
        || std::future::ready(Ok(is_forum)),
        |_| std::future::ready(Ok(reply_topic)),
    )
    .await?;
    let topic = placement.topic.map(|id| {
        let listed = info
            .topics
            .iter()
            .flatten()
            .find(|t| t.id == id)
            .map(|t| t.title.clone());
        let from_reply = replied
            .as_ref()
            .and_then(|(_, titles)| titles.as_ref())
            .and_then(|titles| titles.0.get(&id).cloned());
        (
            id,
            listed.or(from_reply).map(|t| shorten(&t, MAX_TITLE_CHARS)),
        )
    });
    let reply = replied.map(|(m, _)| ReplyPreview {
        id: m.id,
        from: if m.outgoing {
            None
        } else {
            Some(m.sender.map_or_else(|| info.title.clone(), |s| s.name))
                .map(|name| shorten(&name, MAX_TITLE_CHARS))
        },
        text: if m.text.trim().is_empty() {
            match m.media {
                Some(_) => "[media]".into(),
                None => "[no text]".into(),
            }
        } else {
            shorten(&m.text, MAX_REPLY_PREVIEW_CHARS)
        },
    });
    Ok(Shown {
        chat,
        chat_title: shorten(&info.title, MAX_TITLE_CHARS),
        chat_kind: info.kind,
        is_self: info.is_self,
        topic,
        reply,
    })
}

/// `text` on one line, cut to `max` characters with an ellipsis.
fn shorten(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if flat.chars().count() <= max {
        return flat;
    }
    let mut cut: String = flat.chars().take(max - 1).collect();
    cut.push('…');
    cut
}

/// What the page and the tool say when Telegram, or our limits in front of
/// it, refused a confirmed send or Draft. Ours, never Telegram's text.
fn refusal(err: &TgError, decision: Decision) -> Refusal {
    let what = match decision {
        Decision::Draft => "The Draft was not saved",
        _ => "The message was not sent",
    };
    let wait = |seconds: u32, why: String| Refusal {
        message: format!(
            "{why}. {what}, and it is not queued: try again in {seconds} s (ask for it again)."
        ),
        retry_after_s: Some(seconds),
    };
    match err {
        TgError::WritesLimited {
            seconds,
            limit: WriteLimit::Chat,
        } => wait(
            *seconds,
            "Too many messages to this chat in the last minute (the server's Send cap)".into(),
        ),
        TgError::WritesLimited {
            seconds,
            limit: WriteLimit::Account,
        } => wait(
            *seconds,
            "This account's writes budget (messages, reactions, marking read, Drafts per 10 \
             minutes) is used up"
                .into(),
        ),
        TgError::FloodWait { seconds, .. } => wait(
            *seconds,
            "Telegram asks this account to wait before writing again (FLOOD_WAIT)".into(),
        ),
        TgError::RateLimited { seconds } => wait(
            *seconds,
            "Too many requests for this account right now".into(),
        ),
        TgError::SlowMode { seconds } => wait(
            *seconds,
            "This group is in slow mode: each member may send one message per set interval".into(),
        ),
        TgError::PeerFlood => Refusal {
            message: format!(
                "Telegram limits this account for spam (PEER_FLOOD). {what}. This is not a wait: \
                 don't retry, more attempts can make the limit longer; @SpamBot in Telegram says \
                 when it ends."
            ),
            retry_after_s: None,
        },
        TgError::BadSession(_) => Refusal {
            message: format!(
                "This connection's Telegram session has ended. {what}; connect again."
            ),
            retry_after_s: None,
        },
        TgError::InvalidInput(msg) if msg == send::ALREADY_SENT => Refusal {
            message: "It was already sent: Telegram has this very message from an earlier try, \
                      and it is not sent twice."
                .into(),
            retry_after_s: None,
        },
        TgError::InvalidInput(msg) => Refusal {
            message: format!("{what}: {msg}."),
            retry_after_s: None,
        },
        TgError::Upstream(_) | TgError::WrongPassword => Refusal {
            message: format!("{what}: Telegram didn't answer."),
            retry_after_s: None,
        },
    }
}

/// `GET` and `POST /confirm/{id}`, rate limited per client IP. Only for the
/// router.
pub fn router(auth: Arc<AuthState>) -> Router {
    Router::new()
        .route(
            "/confirm/{id}",
            get(show)
                .post(decide)
                .route_layer(middleware::from_fn_with_state(
                    (auth.clone(), Endpoint::Confirm),
                    rate_limit::limit,
                )),
        )
        .layer(DefaultBodyLimit::max(4 * 1024))
        .with_state(auth)
}

/// Who asks for a pending send: the OAuth client its Grant was issued to.
/// `Ok(None)`: the Grant is gone.
async fn requester(auth: &AuthState, grant_id: &GrantId) -> Result<Option<page::Requester>, ()> {
    let client_id = match auth.vault.grant_client(grant_id).await {
        Ok(Some(client_id)) => client_id,
        Ok(None) => return Ok(None),
        Err(err) => {
            tracing::error!(error = %err, "looking up a pending send's grant failed");
            return Err(());
        }
    };
    match auth.clients.get(&client_id).await {
        Ok(client) => Ok(Some(page::Requester::of(client.as_ref()))),
        Err(err) => {
            tracing::error!(error = %err, "loading a pending send's client failed");
            Err(())
        }
    }
}

/// A pending send that is still valid, with who asks for it; else the page
/// to answer with. A Grant deleted since (revoked through a stolen Token, or
/// its Tokens all expired) takes its pending sends with it.
#[allow(clippy::result_large_err)]
async fn valid(auth: &AuthState, id: &str) -> Result<(View, page::Requester), Response> {
    let Some(view) = auth.confirmations.view(id) else {
        return Err(page::no_longer_valid());
    };
    match requester(auth, &view.grant_id).await {
        Ok(Some(requester)) => Ok((view, requester)),
        Ok(None) => {
            auth.confirmations.revoke_grant(&view.grant_id);
            Err(page::no_longer_valid())
        }
        Err(()) => Err(page::unavailable()),
    }
}

/// The Confirmation cookie of this pending send's Grant, if the request has it.
fn confirmed_browser(view: &View, headers: &HeaderMap) -> Option<cookie::Matched> {
    view.cookie
        .as_ref()
        .and_then(|verifier| cookie::find(headers, verifier))
}

async fn show(
    State(auth): State<Arc<AuthState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let (view, requester) = match valid(&auth, &id).await {
        Ok(valid) => valid,
        Err(response) => return response,
    };
    let matched = confirmed_browser(&view, &headers);
    // A link followed from another site carries no SameSite=Strict cookie:
    // load the page once more from here, which does.
    if matched.is_none() && view.cookie.is_some() && from_another_site(&headers) {
        return page::reload_here();
    }
    let mut response = page::confirm(
        StatusCode::OK,
        &page::ConfirmView {
            id: &id,
            view: &view,
            requester: &requester,
            confirmed_browser: matched.is_some(),
            error: None,
        },
    );
    if let Some(matched) = matched {
        response.headers_mut().append(
            header::SET_COOKIE,
            cookie::renew(&matched, auth.vault.ttl().refresh, auth.is_https()),
        );
    }
    response
}

/// `Sec-Fetch-Site: cross-site`: the browser followed a link from another
/// site, and left SameSite=Strict cookies out.
fn from_another_site(headers: &HeaderMap) -> bool {
    headers
        .get("sec-fetch-site")
        .is_some_and(|v| v.as_bytes() == b"cross-site")
}

async fn decide(
    State(auth): State<Arc<AuthState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Response {
    let field = |name: &str| {
        pairs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    let (Some(csrf), Some(decision)) = (field("csrf"), field("decision").and_then(Decision::parse))
    else {
        return page::message(
            StatusCode::BAD_REQUEST,
            "Not understood",
            "Reload the page and press a button.",
        );
    };
    let (view, requester) = match valid(&auth, &id).await {
        Ok(valid) => valid,
        Err(response) => return response,
    };
    let confirmed = confirmed_browser(&view, &headers).is_some();
    let job = match auth.confirmations.begin(&id, csrf, decision, confirmed) {
        Ok(Ok(job)) => job,
        Ok(Err(outcome)) => return page::decided(&outcome),
        Err(NotTaken::Gone) => return page::no_longer_valid(),
        Err(NotTaken::Csrf) => {
            return page::message(
                StatusCode::FORBIDDEN,
                "Page out of date",
                "Reload the page and press the button again.",
            );
        }
        Err(NotTaken::WrongBrowser) => {
            tracing::info!(grant_id = %view.grant_id, chat = %view.shown.chat, "pending send decision refused: not the consent browser");
            return page::confirm(
                StatusCode::FORBIDDEN,
                &page::ConfirmView {
                    id: &id,
                    view: &view,
                    requester: &requester,
                    confirmed_browser: false,
                    error: Some(page::WRONG_BROWSER),
                },
            );
        }
        Err(NotTaken::Busy) => {
            return page::message(
                StatusCode::CONFLICT,
                "Already being handled",
                "This message is being sent or saved right now. Reload the page in a moment.",
            );
        }
    };
    // On its own task: a browser that goes away mid-send must not leave the
    // pending send undecided for good.
    let confirmations = auth.confirmations.clone();
    let finished = tokio::spawn(async move {
        let answer = job.run().await;
        confirmations.finish(&job, answer)
    })
    .await;
    let Ok(finished) = finished else {
        tracing::error!(grant_id = %view.grant_id, "a confirmed write's task failed");
        return page::unavailable();
    };
    match finished {
        Finished::Decided(outcome) => page::decided(&outcome),
        Finished::TryAgain => page::confirm(
            StatusCode::BAD_GATEWAY,
            &page::ConfirmView {
                id: &id,
                view: &View {
                    deciding: false,
                    ..view
                },
                requester: &requester,
                confirmed_browser: confirmed,
                error: Some(page::TRY_AGAIN),
            },
        ),
    }
}

#[cfg(test)]
mod tests;
