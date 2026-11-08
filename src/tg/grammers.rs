//! The real Telegram: QR login and the Client pool, on grammers (pinned exactly).

use std::{
    sync::{Arc, Weak},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use grammers_client::{Client, InvocationError, SignInError, client::PasswordToken, tl};
use grammers_mtsender::{ConnectionParams, SenderPool};
use grammers_session::{Session, updates::UpdatesLike};
use tokio::sync::mpsc;
use zeroize::Zeroizing;

use crate::limits::{AccountLimiter, Limits, MethodClass};

use super::{
    Account, Connector, LoggedIn, Login, LoginAttempt, LoginState, Telegram, TgError,
    chats::{
        self, ChatId, ChatInfo, ChatKind, ChatPage, ChatRef, ChatsQuery, Cursor, DialogsAnswer,
        DialogsRequest, RawDialog, RawMessage, RawPeer, RawProfile, RawUserStatus,
    },
    download::{self, FileError, FilePart, FileRequest, MediaFile, RawMediaMessage},
    find::{self, FindRequest, FoundAnswer, FoundChats, QUERY_TOO_SHORT},
    folders::{self, ChatFolder, RawChatFolder},
    media,
    media::{FileLocation, StoredFile},
    members::{
        self, ChannelAccess, GroupAnswer, MemberPage, MemberSource, MembersQuery,
        ParticipantsAnswer, ParticipantsFilter, ParticipantsRequest, RawMember, Role,
    },
    messages::{
        HistoryAnswer, HistoryQuery, HistoryRequest, MessageAnswer, MessageAt, MessagePage,
        MessageRequest, RawHistoryMessage, RawReaction, Reaction, read_history,
    },
    pool::{Evicted, Live, Pool},
    rich::message_text,
    search::{
        self, ChatType, MediaFilter, QUERY_REQUIRED, RawFoundMessage, RequestScope, SearchAnswer,
        SearchFrom, SearchPage, SearchQuery, SearchRequest, SearchScope,
    },
    session::SessionStore,
    topics::{self, RawForums, RawTopic},
    transcribe::{
        self, Refusal, TranscribedAudio, Transcription, TranscriptionFeed, TranscriptionUpdate,
        Trial,
    },
    view::{self, Album, RawViewMessage},
};
use crate::sessions::{OpenedGrant, Vault};

/// How long revocation waits for `auth.logOut` before giving up on Telegram.
const LOG_OUT_TIMEOUT: Duration = Duration::from_secs(10);

/// Re-export the QR token this long before Telegram says it expires.
const QR_REFRESH_MARGIN: i64 = 5;

/// Our registered Telegram app.
#[derive(Clone)]
pub struct AppCredentials {
    pub api_id: i32,
    pub api_hash: String,
}

impl std::fmt::Debug for AppCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppCredentials")
            .field("api_id", &self.api_id)
            .field("api_hash", &"<redacted>")
            .finish()
    }
}

/// Retry policy of Live clients: one retry after a network error, nothing else.
/// FLOOD_WAIT goes up to the Account's limiter (crate::limits), which blocks the
/// request's method class for the whole Account; grammers' default would sleep
/// inside one call, unseen.
struct RetryIoOnce;

impl grammers_mtsender::RetryPolicy for RetryIoOnce {
    fn should_retry(
        &self,
        ctx: &grammers_mtsender::RetryContext,
    ) -> std::ops::ControlFlow<(), Duration> {
        match ctx.error {
            InvocationError::Io(_) if ctx.fail_count.get() <= 1 => {
                std::ops::ControlFlow::Continue(Duration::from_secs(1))
            }
            _ => std::ops::ControlFlow::Break(()),
        }
    }
}

#[derive(Clone, Copy)]
enum Purpose {
    /// QR login: grammers' default (short FLOOD_WAITs slept through), proven on prod.
    Login,
    /// Live clients: requests go through the Account's limiter.
    Live,
}

/// How this server shows up in the user's Telegram "Active sessions".
fn connection_params(purpose: Purpose) -> ConnectionParams {
    let retry_policy: Box<dyn grammers_mtsender::RetryPolicy> = match purpose {
        Purpose::Login => Box::new(grammers_mtsender::AutoSleep::default()),
        Purpose::Live => Box::new(RetryIoOnce),
    };
    ConnectionParams {
        retry_policy,
        device_model: "Unofficial MCP Server".into(),
        system_version: "tg-mcp".into(),
        app_version: env!("CARGO_PKG_VERSION").into(),
        ..ConnectionParams::default()
    }
}

/// A connected grammers client over our [`SessionStore`].
struct Connected {
    client: Client,
    store: Arc<SessionStore>,
    updates: mpsc::Receiver<UpdatesLike>,
}

fn connect(credentials: &AppCredentials, store: SessionStore, purpose: Purpose) -> Connected {
    let store = Arc::new(store);
    let SenderPool {
        runner,
        handle,
        updates,
    } = SenderPool::with_configuration(
        store.clone(),
        credentials.api_id,
        connection_params(purpose),
    );
    let client = Client::new(handle);
    tokio::spawn(runner.run());
    Connected {
        client,
        store,
        updates,
    }
}

fn upstream(err: InvocationError) -> TgError {
    TgError::Upstream(err.to_string())
}

/// The seconds Telegram asks to wait, if `err` is a flood of the Account.
///
/// grammers splits `FLOOD_WAIT_31` into name `FLOOD_WAIT` and value 31. Both
/// `FLOOD_WAIT_X` and `FLOOD_PREMIUM_WAIT_X` (a non-Premium Account's file transfer
/// limit) are code 420 floods. `SLOWMODE_WAIT_X`, also 420, is not: it limits sending
/// to one Chat, not the Account, so it must not block a whole class.
fn flood_seconds(err: &InvocationError) -> Option<u32> {
    match err {
        InvocationError::Rpc(rpc) if rpc.code == 420 && rpc.name.starts_with("FLOOD_") => {
            Some(rpc.value.unwrap_or(60))
        }
        _ => None,
    }
}

/// Like [`upstream`], but surfaces `FLOOD_WAIT_X` so the Login flow can honour it.
fn login_error(err: InvocationError) -> TgError {
    flood_error(err, MethodClass::Default)
}

/// A flood as [`TgError::FloodWait`] of `class`; anything else is upstream.
fn flood_error(err: InvocationError, class: MethodClass) -> TgError {
    match flood_seconds(&err) {
        Some(seconds) => TgError::FloodWait { seconds, class },
        None => upstream(err),
    }
}

/// The FLOOD_WAIT window a request falls in, by its TL method name.
fn method_class<R: tl::Identifiable>() -> MethodClass {
    MethodClass::of(tl::name_for_id(R::CONSTRUCTOR_ID))
}

/// Errors meaning "this Telegram session is gone": logged out, revoked, banned.
fn is_dead_session(err: &InvocationError) -> bool {
    matches!(err, InvocationError::Rpc(rpc) if rpc.code == 401)
}

fn account_from(user: tl::enums::User) -> Result<Account, TgError> {
    match user {
        tl::enums::User::User(u) => Ok(Account {
            id: u.id,
            first_name: u.first_name.unwrap_or_default(),
            last_name: u.last_name,
            username: u.username,
        }),
        tl::enums::User::Empty(_) => {
            Err(TgError::Upstream("Telegram returned an empty user".into()))
        }
    }
}

fn qr_url(token: &[u8]) -> String {
    format!("tg://login?token={}", URL_SAFE_NO_PAD.encode(token))
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Did Telegram tell us the QR code was scanned and accepted?
fn has_login_token_update(updates: &UpdatesLike) -> bool {
    let UpdatesLike::Updates(updates) = updates else {
        return false;
    };
    let is_login = |u: &tl::enums::Update| matches!(u, tl::enums::Update::LoginToken);
    match updates {
        tl::enums::Updates::UpdateShort(short) => is_login(&short.update),
        tl::enums::Updates::Updates(full) => full.updates.iter().any(is_login),
        tl::enums::Updates::Combined(combined) => combined.updates.iter().any(is_login),
        _ => false,
    }
}

// ── QR login ────────────────────────────────────────────────────────────────

pub struct GrammersLogin {
    credentials: AppCredentials,
}

impl GrammersLogin {
    pub fn new(credentials: AppCredentials) -> Self {
        Self { credentials }
    }
}

#[async_trait::async_trait]
impl Login for GrammersLogin {
    async fn start(&self) -> Result<Box<dyn LoginAttempt>, TgError> {
        Ok(Box::new(QrAttempt {
            credentials: self.credentials.clone(),
            conn: connect(&self.credentials, SessionStore::new(), Purpose::Login),
            current: None,
            password_hint: None,
        }))
    }
}

struct QrAttempt {
    credentials: AppCredentials,
    conn: Connected,
    /// The QR currently shown and when Telegram expires it.
    current: Option<(String, i64)>,
    /// Set once Telegram said the Account has a Cloud password: its hint, if any.
    password_hint: Option<Option<String>>,
}

impl Drop for QrAttempt {
    fn drop(&mut self) {
        // Abandoned or finished: close the connection. A finished login reconnects
        // from the sealed snapshot through the Client pool.
        self.conn.client.disconnect();
    }
}

#[async_trait::async_trait]
impl LoginAttempt for QrAttempt {
    async fn poll(&mut self) -> Result<LoginState, TgError> {
        if let Some(hint) = &self.password_hint {
            return Ok(LoginState::PasswordNeeded { hint: hint.clone() });
        }
        let mut scanned = false;
        while let Ok(update) = self.conn.updates.try_recv() {
            scanned |= has_login_token_update(&update);
        }
        if let Some((url, expires)) = &self.current
            && !scanned
            && expires - unix_now() > QR_REFRESH_MARGIN
        {
            return Ok(LoginState::WaitingForScan {
                qr_url: url.clone(),
            });
        }

        let exported = self
            .conn
            .client
            .invoke(&tl::functions::auth::ExportLoginToken {
                api_id: self.credentials.api_id,
                api_hash: self.credentials.api_hash.clone(),
                except_ids: Vec::new(),
            })
            .await;
        let token = match exported {
            Ok(token) => token,
            Err(err) if err.is("SESSION_PASSWORD_NEEDED") => return self.need_password().await,
            Err(err) => return Err(login_error(err)),
        };
        self.handle(token).await
    }

    async fn submit_password(&mut self, password: Zeroizing<String>) -> Result<LoggedIn, TgError> {
        if self.password_hint.is_none() {
            return Err(TgError::Upstream("not waiting for a cloud password".into()));
        }
        // A fresh SRP challenge per attempt, so a retry never reuses a spent srp_id.
        let info = self.password_info().await?;
        // check_password unwraps these; a malformed answer must not panic the request.
        if info.current_algo.is_none() || info.srp_b.is_none() || info.srp_id.is_none() {
            return Err(TgError::Upstream(
                "telegram sent no SRP parameters for the cloud password".into(),
            ));
        }
        let checked = self
            .conn
            .client
            .check_password(PasswordToken::new(info), password.as_bytes())
            .await;
        drop(password);
        match checked {
            Ok(user) => self.logged_in(user.raw),
            Err(SignInError::InvalidPassword(_)) => Err(TgError::WrongPassword),
            Err(SignInError::Other(err)) => Err(login_error(err)),
            Err(other) => Err(TgError::Upstream(other.to_string())),
        }
    }
}

impl QrAttempt {
    async fn handle(&mut self, token: tl::enums::auth::LoginToken) -> Result<LoginState, TgError> {
        match token {
            tl::enums::auth::LoginToken::Token(t) => {
                let url = qr_url(&t.token);
                self.current = Some((url.clone(), i64::from(t.expires)));
                Ok(LoginState::WaitingForScan { qr_url: url })
            }
            tl::enums::auth::LoginToken::MigrateTo(m) => {
                // The account lives in another DC: make it home and finish there.
                let old_dc = self
                    .conn
                    .store
                    .home_dc_id()
                    .map_err(|e| TgError::Upstream(e.to_string()))?;
                self.conn
                    .store
                    .set_home_dc_id(m.dc_id)
                    .await
                    .map_err(|e| TgError::Upstream(e.to_string()))?;
                tracing::info!(from = old_dc, to = m.dc_id, "qr login migrates dc");
                let imported = self
                    .conn
                    .client
                    .invoke_in_dc(
                        m.dc_id,
                        &tl::functions::auth::ImportLoginToken { token: m.token },
                    )
                    .await;
                match imported {
                    Ok(tl::enums::auth::LoginToken::Success(s)) => self.finish(s.authorization),
                    Ok(_) => Err(TgError::Upstream(
                        "unexpected answer to importLoginToken".into(),
                    )),
                    Err(err) if err.is("SESSION_PASSWORD_NEEDED") => self.need_password().await,
                    Err(err) => Err(login_error(err)),
                }
            }
            tl::enums::auth::LoginToken::Success(s) => self.finish(s.authorization),
        }
    }

    fn finish(
        &mut self,
        authorization: tl::enums::auth::Authorization,
    ) -> Result<LoginState, TgError> {
        let tl::enums::auth::Authorization::Authorization(auth) = authorization else {
            return Err(TgError::Upstream(
                "this Telegram account needs to be signed up first".into(),
            ));
        };
        self.logged_in(auth.user).map(LoginState::LoggedIn)
    }

    fn logged_in(&self, user: tl::enums::User) -> Result<LoggedIn, TgError> {
        let account = account_from(user)?;
        let telegram_session = self.conn.store.snapshot()?;
        Ok(LoggedIn {
            account,
            telegram_session,
        })
    }

    /// The QR was accepted but the Account has a Cloud password: remember its hint.
    async fn need_password(&mut self) -> Result<LoginState, TgError> {
        let hint = self.password_info().await?.hint;
        tracing::info!("qr login needs the cloud password");
        self.password_hint = Some(hint.clone());
        Ok(LoginState::PasswordNeeded { hint })
    }

    /// `account.getPassword`: the hint and a fresh SRP challenge.
    async fn password_info(&self) -> Result<tl::types::account::Password, TgError> {
        let tl::enums::account::Password::Password(info) = self
            .conn
            .client
            .invoke(&tl::functions::account::GetPassword {})
            .await
            .map_err(login_error)?;
        Ok(info)
    }
}

// ── Client pool ─────────────────────────────────────────────────────────────

/// A Live client: a connected grammers client for one Grant, holding the opened
/// Grant (with its Session key) so an updated Telegram session can be re-sealed.
pub struct LiveClient {
    client: Client,
    store: Arc<SessionStore>,
    grant: OpenedGrant,
    /// Access hashes of users and channels seen so far, for ids handed out
    /// without one (before ids carried it). In memory only, and only as long as
    /// this Live client lives.
    peers: std::sync::Mutex<std::collections::HashMap<ChatId, i64>>,
    /// Whether each channel seen so far is a forum (`channel.forum`), so
    /// `topic` on a known Chat needs no lookup. In memory only, like `peers`.
    forums: std::sync::Mutex<std::collections::HashMap<ChatId, bool>>,
    /// Telegram said this session is gone (logged out / revoked from the app).
    dead: std::sync::atomic::AtomicBool,
    /// The Account's budget and FLOOD_WAIT state, shared with its other Grants.
    limiter: Arc<AccountLimiter>,
    /// This session's `updateTranscribedAudio` updates, for transcriptions
    /// waiting for their final text ([`forward_transcriptions`]).
    transcriptions: TranscriptionFeed,
    /// Every request handed to grammers (past the limiter), serialized, so tests can
    /// see what reached Telegram.
    #[cfg(test)]
    sent: std::sync::Mutex<Vec<Vec<u8>>>,
    /// Errors Telegram answers the next requests with, in order, so tests
    /// can see how the Live client reads them.
    #[cfg(test)]
    answers: std::sync::Mutex<std::collections::VecDeque<InvocationError>>,
}

impl Live for LiveClient {
    fn snapshot(&self) -> Result<zeroize::Zeroizing<Vec<u8>>, TgError> {
        self.store.snapshot()
    }

    fn shutdown(&self) {
        self.client.disconnect();
    }
}

impl LiveClient {
    fn classify(&self, err: InvocationError, class: MethodClass) -> TgError {
        if is_dead_session(&err) {
            self.dead.store(true, std::sync::atomic::Ordering::SeqCst);
            TgError::BadSession(err.to_string())
        } else if err.is("SEARCH_QUERY_EMPTY") {
            TgError::InvalidInput(QUERY_REQUIRED.into())
        } else if err.is("QUERY_TOO_SHORT") {
            TgError::InvalidInput(QUERY_TOO_SHORT.into())
        } else if err.is("FROM_PEER_INVALID") {
            TgError::InvalidInput("from: Telegram can't search for this sender here".into())
        } else {
            // FLOOD_WAIT surfaces as TgError::FloodWait for the limiter; the rest is upstream.
            flood_error(err, class)
        }
    }

    /// One request to Telegram, under the Account's limiter, in the FLOOD_WAIT
    /// window of its [`MethodClass`] (derived from the TL method, so a new request
    /// needs no tagging). `chat` gives "can't read this chat" errors a
    /// caller-facing meaning.
    async fn call<R: tl::RemoteCall + tl::Identifiable>(
        &self,
        request: &R,
        chat: Option<ChatId>,
    ) -> Result<R::Return, TgError> {
        self.call_refusing(request, chat, |_| None).await
    }

    /// Like [`Self::call`], but `refusal` first turns this request's own
    /// refusals (an error code that means something only here) into the
    /// caller's error. Floods still go to the limiter.
    async fn call_refusing<R: tl::RemoteCall + tl::Identifiable>(
        &self,
        request: &R,
        chat: Option<ChatId>,
        refusal: impl Fn(&InvocationError) -> Option<TgError> + Send + Sync,
    ) -> Result<R::Return, TgError> {
        let class = method_class::<R>();
        self.limiter
            .run(class, || async {
                #[cfg(test)]
                self.sent
                    .lock()
                    .unwrap()
                    .push(tl::Serializable::to_bytes(request));
                #[cfg(test)]
                let scripted = self.answers.lock().unwrap().pop_front();
                #[cfg(not(test))]
                let scripted = None;
                let answer = match scripted {
                    Some(err) => Err(err),
                    None => self.client.invoke(request).await,
                };
                answer.map_err(|err| match (refusal(&err), chat) {
                    (Some(refused), _) => refused,
                    (None, Some(chat)) => self.classify_chat_error(err, chat, class),
                    (None, None) => self.classify(err, class),
                })
            })
            .await
    }

    /// Like [`Self::call`], to DC `dc_id` (a file's DC): under the Account's
    /// limiter, in the request's [`MethodClass`], so a `FLOOD_WAIT` blocks that
    /// class for the Account and is never retried around. Every other error
    /// comes back as Telegram sent it, for the caller to read (`FILE_MIGRATE_X`,
    /// `FILE_REFERENCE_EXPIRED`, a DC without authorization).
    async fn call_in_dc<R: tl::RemoteCall + tl::Identifiable>(
        &self,
        dc_id: i32,
        request: &R,
    ) -> Result<Result<R::Return, InvocationError>, TgError> {
        let class = method_class::<R>();
        self.limiter
            .run(class, || async {
                #[cfg(test)]
                self.sent
                    .lock()
                    .unwrap()
                    .push(tl::Serializable::to_bytes(request));
                match self.client.invoke_in_dc(dc_id, request).await {
                    Ok(answer) => Ok(Ok(answer)),
                    Err(err) if flood_seconds(&err).is_some() => Err(flood_error(err, class)),
                    Err(err) => Ok(Err(err)),
                }
            })
            .await
    }

    fn home_dc(&self) -> Result<i32, TgError> {
        self.store
            .home_dc_id()
            .map_err(|e| TgError::Upstream(e.to_string()))
    }

    /// Log this session in to DC `dc_id` with an authorization exported from
    /// the home DC (`auth.exportAuthorization` → `auth.importAuthorization`),
    /// as grammers' own downloader does, but through the limiter.
    async fn authorize_in(&self, dc_id: i32) -> Result<(), TgError> {
        let tl::enums::auth::ExportedAuthorization::Authorization(exported) = self
            .call(&tl::functions::auth::ExportAuthorization { dc_id }, None)
            .await?;
        self.call_in_dc(
            dc_id,
            &tl::functions::auth::ImportAuthorization {
                id: exported.id,
                bytes: exported.bytes,
            },
        )
        .await?
        .map_err(upstream)?;
        tracing::info!(dc_id, "authorized in another dc for a download");
        Ok(())
    }

    fn is_dead(&self) -> bool {
        self.dead.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Telegram for LiveClient {
    async fn whoami(&self) -> Result<Account, TgError> {
        let me = self
            .call(
                &tl::functions::users::GetUsers {
                    id: vec![tl::enums::InputUser::UserSelf],
                },
                None,
            )
            .await?;
        account_from(
            me.into_iter()
                .next()
                .ok_or_else(|| TgError::Upstream("no self user".into()))?,
        )
    }

    async fn chat_folders(&self) -> Result<Vec<ChatFolder>, TgError> {
        Ok(self
            .dialog_filters()
            .await?
            .into_iter()
            .map(|f| f.folder)
            .collect())
    }
    async fn list_chats(&self, query: ChatsQuery) -> Result<ChatPage, TgError> {
        if let Some(folder) = query.chat_folder {
            return folders::list_folder_chats(
                &query,
                folder,
                || self.dialog_filters(),
                |peers| self.peer_dialogs(peers),
            )
            .await;
        }
        chats::list_chats(&query, |request| self.get_dialogs(request)).await
    }

    async fn get_messages(&self, id: ChatRef, query: HistoryQuery) -> Result<MessagePage, TgError> {
        let peer = self.resolve(id).await?;
        let peer = &peer;
        if query.topic.is_some() {
            self.require_forum(id).await?;
        }
        let mut page = read_history(
            id,
            &query,
            &|chat| self.cached(chat),
            |request| self.get_history(peer, id.chat(), request),
            |request| self.get_message(peer, id.chat(), request),
        )
        .await?;
        topics::complete(&mut page, |ids| self.topics_by_id(peer, id.chat(), ids)).await?;
        Ok(page)
    }

    async fn get_chat_info(&self, id: ChatRef) -> Result<ChatInfo, TgError> {
        let peer = self.resolve(id).await?;
        let chat = id.chat();
        let (description, members, users, chats, profile) = match peer {
            tl::enums::InputPeer::User(u) => {
                let tl::enums::users::UserFull::Full(full) = self
                    .call(
                        &tl::functions::users::GetFullUser {
                            // The Account itself: inputUserSelf always works and
                            // returns its own bio.
                            id: if u.user_id == self.grant.account_id {
                                tl::enums::InputUser::UserSelf
                            } else {
                                tl::enums::InputUser::User(tl::types::InputUser {
                                    user_id: u.user_id,
                                    access_hash: u.access_hash,
                                })
                            },
                        },
                        Some(chat),
                    )
                    .await?;
                let tl::enums::UserFull::Full(user) = full.full_user;
                let mut profile = full
                    .users
                    .iter()
                    .find_map(|other| match other {
                        tl::enums::User::User(other) if other.id == u.user_id => {
                            Some(raw_profile(other))
                        }
                        _ => None,
                    })
                    .unwrap_or_default();
                profile.bio = user.about;
                profile.bot_description = user
                    .bot_info
                    .and_then(|tl::enums::BotInfo::Info(info)| info.description);
                profile.birthday =
                    user.birthday
                        .map(|tl::enums::Birthday::Birthday(b)| chats::Birthday {
                            day: b.day,
                            month: b.month,
                            year: b.year,
                        });
                profile.common_chats_count = user.common_chats_count;
                (None, None, full.users, full.chats, Some(profile))
            }
            tl::enums::InputPeer::Chat(c) => {
                let tl::enums::messages::ChatFull::Full(full) = self
                    .call(
                        &tl::functions::messages::GetFullChat { chat_id: c.chat_id },
                        Some(chat),
                    )
                    .await?;
                let (about, members) = full_chat_details(full.full_chat);
                (about, members, full.users, full.chats, None)
            }
            tl::enums::InputPeer::Channel(c) => {
                let tl::enums::messages::ChatFull::Full(full) = self
                    .call(
                        &tl::functions::channels::GetFullChannel {
                            channel: tl::enums::InputChannel::Channel(tl::types::InputChannel {
                                channel_id: c.channel_id,
                                access_hash: c.access_hash,
                            }),
                        },
                        Some(chat),
                    )
                    .await?;
                let (about, members) = full_chat_details(full.full_chat);
                (about, members, full.users, full.chats, None)
            }
            _ => return Err(TgError::InvalidInput(format!("{chat} can't be looked up"))),
        };
        let peers = self.described(users, chats);
        let peer = peers
            .iter()
            .find(|p| p.id == chat)
            .ok_or_else(|| TgError::Upstream(format!("Telegram didn't describe {chat}")))?;
        let mut info = match profile {
            Some(profile) => chats::user_info(
                peer,
                profile,
                time::OffsetDateTime::now_utc().unix_timestamp(),
            ),
            None => chats::chat_info(peer, description, members),
        };
        // A forum's Topics (#28): the full channel's answer said it is one.
        if self.known_forum(chat) == Some(true) {
            (info.topics, info.topics_truncated) = self.list_topics(id).await?;
        }
        Ok(info)
    }

    async fn common_chats(&self, id: ChatRef, limit: usize) -> Result<FoundChats, TgError> {
        let chat = id.chat();
        let tl::enums::InputPeer::User(user) = self.resolve(id).await? else {
            return Err(TgError::InvalidInput(format!("{chat} is not a user")));
        };
        let answer = self
            .call(
                &tl::functions::messages::GetCommonChats {
                    user_id: tl::enums::InputUser::User(tl::types::InputUser {
                        user_id: user.user_id,
                        access_hash: user.access_hash,
                    }),
                    max_id: 0,
                    limit: i32::try_from(limit).unwrap_or(i32::MAX),
                },
                Some(chat),
            )
            .await?;
        let chats = match answer {
            tl::enums::messages::Chats::Chats(c) => c.chats,
            tl::enums::messages::Chats::Slice(c) => c.chats,
        };
        let peers = self.described(Vec::new(), chats);
        Ok(find::common_chats(&peers, &|chat| self.cached(chat), limit))
    }

    async fn get_members(&self, query: MembersQuery) -> Result<MemberPage, TgError> {
        members::get_members(&query, &|chat| self.cached(chat), self).await
    }

    async fn search_messages(&self, query: SearchQuery) -> Result<SearchPage, TgError> {
        if let SearchScope::Chat {
            chat,
            topic: Some(_),
            ..
        } = query.scope
        {
            self.require_forum(chat).await?;
        }
        let mut page = search::search(&query, &|chat| self.cached(chat), |request| {
            self.search(request)
        })
        .await?;
        topics::complete_search(&mut page, |chat, ids| async move {
            // Only a forum this client can address right away: a title is
            // not worth a walk through the dialog list.
            let hash = chat.access_hash().or_else(|| self.cached(chat.chat()));
            match hash {
                Some(hash) => {
                    let peer = input_peer(chat.chat(), Some(hash));
                    self.topics_by_id(&peer, chat.chat(), ids).await
                }
                None => Ok(Vec::new()),
            }
        })
        .await?;
        Ok(page)
    }

    async fn find_chats(&self, query: &str, limit: usize) -> Result<FoundChats, TgError> {
        find::find_chats(query, limit, &|chat| self.cached(chat), |request| {
            self.contacts_search(request)
        })
        .await
    }

    async fn media_file(&self, id: ChatRef, message_id: i32) -> Result<MediaFile, TgError> {
        let peer = self.resolve(id).await?;
        download::locate(id, message_id, |request| {
            self.get_media_message(&peer, id.chat(), request)
        })
        .await
    }

    async fn media_album(&self, id: ChatRef, message_id: i32) -> Result<Album, TgError> {
        let peer = self.resolve(id).await?;
        view::locate_album(id, message_id, |ids| {
            self.get_view_messages(&peer, id.chat(), ids)
        })
        .await
    }

    async fn transcribe(
        &self,
        id: ChatRef,
        message_id: i32,
        deadline: tokio::time::Instant,
    ) -> Result<Transcription, TgError> {
        let peer = self.resolve(id).await?;
        let chat = id.chat();
        transcribe::transcribe(
            id,
            message_id,
            deadline,
            |request| self.get_media_message(&peer, chat, request),
            &self.transcriptions,
            || self.transcribe_audio(&peer, chat, message_id),
        )
        .await
    }

    async fn get_file(&self, request: FileRequest) -> Result<FilePart, FileError> {
        let FileRequest {
            location,
            dc_id,
            offset,
            limit,
        } = request;
        let get_file = tl::functions::upload::GetFile {
            precise: false,
            cdn_supported: false,
            location: input_file_location(location),
            offset,
            limit,
        };
        let mut authorized = false;
        loop {
            let err = match self.call_in_dc(dc_id, &get_file).await? {
                Ok(tl::enums::upload::File::File(file)) => return Ok(FilePart::Bytes(file.bytes)),
                Ok(tl::enums::upload::File::CdnRedirect(_)) => return Ok(FilePart::CdnRedirect),
                Err(err) => err,
            };
            match file_refusal(&err, dc_id, self.home_dc()?) {
                FileRefusal::Migrate(Some(to)) => return Err(FileError::Migrate(to)),
                FileRefusal::Migrate(None) => {
                    return Err(TgError::Upstream("FILE_MIGRATE without a DC".into()).into());
                }
                FileRefusal::ReferenceExpired => return Err(FileError::ReferenceExpired),
                FileRefusal::NotAuthorizedInDc if !authorized => {
                    self.authorize_in(dc_id).await?;
                    authorized = true;
                }
                FileRefusal::NotAuthorizedInDc => return Err(upstream(err).into()),
                FileRefusal::Other => return Err(self.classify(err, MethodClass::Files).into()),
            }
        }
    }
}

/// What a refused `upload.getFile` to DC `dc_id` asks of the downloader.
#[derive(Debug, PartialEq, Eq)]
enum FileRefusal {
    /// `FILE_MIGRATE_X`: ask DC X.
    Migrate(Option<i32>),
    /// `FILE_REFERENCE_EXPIRED` / `FILE_REFERENCE_INVALID`: both mean "fetch
    /// the message again" (core.telegram.org/api/file-references).
    ReferenceExpired,
    /// `AUTH_KEY_UNREGISTERED` from a DC other than the home DC: this session
    /// isn't logged in there yet. Not a dead session (only the home DC says that).
    NotAuthorizedInDc,
    /// Anything else, for [`LiveClient::classify`].
    Other,
}

fn file_refusal(err: &InvocationError, dc_id: i32, home_dc: i32) -> FileRefusal {
    match err {
        InvocationError::Rpc(rpc) if rpc.code == FILE_MIGRATE => {
            FileRefusal::Migrate(rpc.value.and_then(|v| i32::try_from(v).ok()))
        }
        _ if err.is("FILE_REFERENCE_EXPIRED") || err.is("FILE_REFERENCE_INVALID") => {
            FileRefusal::ReferenceExpired
        }
        _ if err.is("AUTH_KEY_UNREGISTERED") && dc_id != home_dc => FileRefusal::NotAuthorizedInDc,
        _ => FileRefusal::Other,
    }
}

/// How far `resolve` pages through the dialogs (per folder) looking for an unknown Chat.
const RESOLVE_MAX_PAGES: usize = 5;

impl LiveClient {
    /// One `messages.getHistory` call; for pinned messages one
    /// `messages.search` call with the pinned filter (method class Search).
    async fn get_history(
        &self,
        peer: &tl::enums::InputPeer,
        chat: ChatId,
        request: HistoryRequest,
    ) -> Result<HistoryAnswer, TgError> {
        let HistoryRequest {
            offset_id,
            offset_date,
            add_offset,
            limit,
            max_id,
            min_id,
            pinned,
            topic,
        } = request;
        let unknown_topic =
            |err: &InvocationError| topic_refusal((topic != 0).then_some(topic), err);
        let answer = if pinned {
            self.call_refusing(
                &tl::functions::messages::Search {
                    peer: peer.clone(),
                    q: String::new(),
                    from_id: None,
                    saved_peer_id: None,
                    saved_reaction: None,
                    top_msg_id: (topic != 0).then_some(topic),
                    filter: tl::enums::MessagesFilter::InputMessagesFilterPinned,
                    min_date: 0,
                    max_date: 0,
                    offset_id,
                    add_offset,
                    limit,
                    max_id,
                    min_id,
                    hash: 0,
                },
                Some(chat),
                unknown_topic,
            )
            .await?
        } else if topic != 0 {
            // A Topic is the thread of its first message (General: of 1).
            self.call_refusing(
                &tl::functions::messages::GetReplies {
                    peer: peer.clone(),
                    msg_id: topic,
                    offset_id,
                    offset_date,
                    add_offset,
                    limit,
                    max_id,
                    min_id,
                    hash: 0,
                },
                Some(chat),
                unknown_topic,
            )
            .await?
        } else {
            self.call(
                &tl::functions::messages::GetHistory {
                    peer: peer.clone(),
                    offset_id,
                    offset_date,
                    add_offset,
                    limit,
                    max_id,
                    min_id,
                    hash: 0,
                },
                Some(chat),
            )
            .await?
        };
        let (messages, topics, chats, users) = match answer {
            tl::enums::messages::Messages::Messages(m) => (m.messages, m.topics, m.chats, m.users),
            tl::enums::messages::Messages::Slice(m) => (m.messages, m.topics, m.chats, m.users),
            tl::enums::messages::Messages::ChannelMessages(m) => {
                (m.messages, m.topics, m.chats, m.users)
            }
            tl::enums::messages::Messages::NotModified(_) => (vec![], vec![], vec![], vec![]),
        };
        let returned = messages.len();
        let messages: Vec<RawHistoryMessage> = messages
            .into_iter()
            .filter_map(raw_history_message)
            .collect();
        let forums = raw_forums(&chats, topics);
        let peers = self.described(users, chats);
        Ok(HistoryAnswer {
            messages,
            returned,
            peers,
            forums,
        })
    }

    /// One `channels.getMessages` (channels and supergroups) or
    /// `messages.getMessages` (anywhere else: it takes no peer, the id is the
    /// Account's) call for one message id.
    async fn get_message(
        &self,
        peer: &tl::enums::InputPeer,
        chat: ChatId,
        request: MessageRequest,
    ) -> Result<MessageAnswer, TgError> {
        let (messages, users, chats) = self.fetch_messages(peer, chat, vec![request.id]).await?;
        let forums = raw_forums(&chats, Vec::new());
        let peers = self.described(users, chats);
        Ok(MessageAnswer {
            messages: messages.into_iter().filter_map(raw_found_message).collect(),
            peers,
            forums,
        })
    }

    /// The same call as [`Self::get_message`], each message reduced to its
    /// media and that media's file (#36).
    async fn get_media_message(
        &self,
        peer: &tl::enums::InputPeer,
        chat: ChatId,
        request: MessageRequest,
    ) -> Result<Vec<RawMediaMessage>, TgError> {
        let (messages, users, chats) = self.fetch_messages(peer, chat, vec![request.id]).await?;
        self.described(users, chats);
        Ok(messages.into_iter().filter_map(raw_media_message).collect())
    }

    /// One `channels.getMessages` / `messages.getMessages` call for the ids
    /// around a message, each message reduced to what a view needs (#34).
    async fn get_view_messages(
        &self,
        peer: &tl::enums::InputPeer,
        chat: ChatId,
        ids: Vec<i32>,
    ) -> Result<Vec<RawViewMessage>, TgError> {
        let (messages, users, chats) = self.fetch_messages(peer, chat, ids).await?;
        self.described(users, chats);
        Ok(messages.into_iter().filter_map(raw_view_message).collect())
    }

    /// One `messages.transcribeAudio` call (method class Transcribe): under
    /// the limiter, its refusals the caller's ([`Refusal`]).
    async fn transcribe_audio(
        &self,
        peer: &tl::enums::InputPeer,
        chat: ChatId,
        msg_id: i32,
    ) -> Result<TranscribedAudio, TgError> {
        let answer = self
            .call_refusing(
                &tl::functions::messages::TranscribeAudio {
                    peer: peer.clone(),
                    msg_id,
                },
                Some(chat),
                |err| transcribe_refusal(err).map(|refusal| refusal.error(msg_id)),
            )
            .await?;
        Ok(transcribed_audio(answer))
    }

    /// The messages, users and chats of one `channels.getMessages` /
    /// `messages.getMessages` call for `ids`.
    async fn fetch_messages(
        &self,
        peer: &tl::enums::InputPeer,
        chat: ChatId,
        ids: Vec<i32>,
    ) -> Result<
        (
            Vec<tl::enums::Message>,
            Vec<tl::enums::User>,
            Vec<tl::enums::Chat>,
        ),
        TgError,
    > {
        let id = ids
            .into_iter()
            .map(|id| tl::enums::InputMessage::Id(tl::types::InputMessageId { id }))
            .collect();
        let answer = match peer {
            tl::enums::InputPeer::Channel(c) => {
                let channel = tl::enums::InputChannel::Channel(tl::types::InputChannel {
                    channel_id: c.channel_id,
                    access_hash: c.access_hash,
                });
                self.call(
                    &tl::functions::channels::GetMessages { channel, id },
                    Some(chat),
                )
                .await?
            }
            _ => {
                self.call(&tl::functions::messages::GetMessages { id }, Some(chat))
                    .await?
            }
        };
        Ok(match answer {
            tl::enums::messages::Messages::Messages(m) => (m.messages, m.users, m.chats),
            tl::enums::messages::Messages::Slice(m) => (m.messages, m.users, m.chats),
            tl::enums::messages::Messages::ChannelMessages(m) => (m.messages, m.users, m.chats),
            tl::enums::messages::Messages::NotModified(_) => (vec![], vec![], vec![]),
        })
    }

    /// One `messages.searchGlobal` or `messages.search` call (method class Search).
    async fn search(&self, request: SearchRequest) -> Result<SearchAnswer, TgError> {
        let SearchRequest {
            q,
            filter,
            min_date,
            max_date,
            limit,
            scope,
        } = request;
        let filter = messages_filter(filter);
        let answer = match scope {
            RequestScope::Global {
                chat_type,
                folder_id,
                offset_rate,
                offset_peer,
                offset_id,
            } => {
                // An unknown peer (no hash) can't be addressed: Empty, as
                // grammers' own iterator sends it.
                let offset_peer = offset_peer
                    .and_then(|peer| {
                        direct_input_peer(peer).or_else(|| {
                            self.cached(peer.chat())
                                .map(|hash| input_peer(peer.chat(), Some(hash)))
                        })
                    })
                    .unwrap_or(tl::enums::InputPeer::Empty);
                self.call(
                    &tl::functions::messages::SearchGlobal {
                        broadcasts_only: chat_type == Some(ChatType::Channels),
                        groups_only: chat_type == Some(ChatType::Groups),
                        users_only: chat_type == Some(ChatType::Private),
                        folder_id,
                        community: None,
                        q,
                        filter,
                        min_date,
                        max_date,
                        offset_rate,
                        offset_peer,
                        offset_id,
                        limit,
                    },
                    None,
                )
                .await?
            }
            RequestScope::Chat {
                chat,
                from,
                topic,
                offset_id,
            } => {
                let peer = self.resolve(chat).await?;
                let from_id = match from {
                    None => None,
                    Some(SearchFrom::Me) => Some(tl::enums::InputPeer::PeerSelf),
                    Some(SearchFrom::Peer(sender)) => Some(self.resolve(sender).await?),
                };
                self.call_refusing(
                    &tl::functions::messages::Search {
                        peer,
                        q,
                        from_id,
                        saved_peer_id: None,
                        saved_reaction: None,
                        top_msg_id: topic,
                        filter,
                        min_date,
                        max_date,
                        offset_id,
                        add_offset: 0,
                        limit,
                        max_id: 0,
                        min_id: 0,
                        hash: 0,
                    },
                    Some(chat.chat()),
                    |err| topic_refusal(topic, err),
                )
                .await?
            }
        };
        let (messages, topics, chats, users, complete, next_rate) = match answer {
            tl::enums::messages::Messages::Messages(m) => {
                (m.messages, m.topics, m.chats, m.users, true, None)
            }
            tl::enums::messages::Messages::Slice(m) => {
                (m.messages, m.topics, m.chats, m.users, false, m.next_rate)
            }
            tl::enums::messages::Messages::ChannelMessages(m) => {
                (m.messages, m.topics, m.chats, m.users, false, None)
            }
            tl::enums::messages::Messages::NotModified(_) => {
                (vec![], vec![], vec![], vec![], true, None)
            }
        };
        let returned = messages.len();
        let forums = raw_forums(&chats, topics);
        let peers = self.described(users, chats);
        Ok(SearchAnswer {
            messages: messages.into_iter().filter_map(raw_found_message).collect(),
            returned,
            complete,
            next_rate,
            peers,
            forums,
        })
    }

    /// One `contacts.search` call (method class Search).
    async fn contacts_search(&self, request: FindRequest) -> Result<FoundAnswer, TgError> {
        let tl::enums::contacts::Found::Found(found) = self
            .call(
                &tl::functions::contacts::Search {
                    broadcasts: false,
                    bots: false,
                    q: request.q,
                    limit: request.limit,
                },
                None,
            )
            .await?;
        let answer = found_answer(found);
        self.remember(&answer.peers);
        Ok(answer)
    }

    /// The peers an answer describes (its `users` and `chats`), remembered:
    /// every answer that describes peers goes through here (or `remember`).
    fn described(&self, users: Vec<tl::enums::User>, chats: Vec<tl::enums::Chat>) -> Vec<RawPeer> {
        self.forums
            .lock()
            .expect("forum cache lock")
            .extend(chats.iter().filter_map(forum_flag));
        let peers: Vec<RawPeer> = users
            .into_iter()
            .filter_map(raw_user)
            .chain(chats.into_iter().filter_map(raw_chat))
            .collect();
        self.remember(&peers);
        peers
    }

    fn remember(&self, peers: &[RawPeer]) {
        self.peers
            .lock()
            .expect("peer cache lock")
            .extend(peers.iter().filter_map(|p| Some((p.id, p.access_hash?))));
    }

    /// Whether a Chat is a forum, as the last answer that described it said.
    fn known_forum(&self, chat: ChatId) -> Option<bool> {
        self.forums
            .lock()
            .expect("forum cache lock")
            .get(&chat)
            .copied()
    }

    /// `topic` needs a forum: a user or basic group never is one; a channel
    /// is one if an answer said so, else one `channels.getChannels` asks.
    async fn require_forum(&self, id: ChatRef) -> Result<(), TgError> {
        let chat = id.chat();
        if !matches!(chat, ChatId::Channel(_)) {
            return Err(topics::not_a_forum(chat));
        }
        if self.known_forum(chat).is_none() {
            let channel = self.input_channel(id).await?;
            let chats = match self
                .call(
                    &tl::functions::channels::GetChannels { id: vec![channel] },
                    Some(chat),
                )
                .await?
            {
                tl::enums::messages::Chats::Chats(c) => c.chats,
                tl::enums::messages::Chats::Slice(c) => c.chats,
            };
            self.described(Vec::new(), chats);
        }
        match self.known_forum(chat) {
            Some(true) => Ok(()),
            _ => Err(topics::not_a_forum(chat)),
        }
    }

    /// One `messages.getForumTopicsByID` call: the Topics `ids` of a forum
    /// that still exist.
    async fn topics_by_id(
        &self,
        peer: &tl::enums::InputPeer,
        chat: ChatId,
        ids: Vec<i32>,
    ) -> Result<Vec<RawTopic>, TgError> {
        let tl::enums::messages::ForumTopics::Topics(answer) = self
            .call(
                &tl::functions::messages::GetForumTopicsById {
                    peer: peer.clone(),
                    topics: ids,
                },
                Some(chat),
            )
            .await?;
        self.described(answer.users, answer.chats);
        Ok(answer.topics.into_iter().filter_map(raw_topic).collect())
    }

    /// A forum's first [`topics::MAX_LISTED_TOPICS`] Topics, in Telegram's
    /// order (one `messages.getForumTopics` call), and whether it has more.
    async fn list_topics(
        &self,
        id: ChatRef,
    ) -> Result<(Option<Vec<topics::Topic>>, bool), TgError> {
        let peer = self.resolve(id).await?;
        let tl::enums::messages::ForumTopics::Topics(answer) = self
            .call(
                &tl::functions::messages::GetForumTopics {
                    peer,
                    q: None,
                    offset_date: 0,
                    offset_id: 0,
                    offset_topic: 0,
                    limit: topics::MAX_LISTED_TOPICS as i32,
                },
                Some(id.chat()),
            )
            .await?;
        self.described(answer.users, answer.chats);
        let raw: Vec<RawTopic> = answer.topics.into_iter().filter_map(raw_topic).collect();
        let (listed, truncated) =
            topics::listed(raw, usize::try_from(answer.count).unwrap_or_default());
        Ok((Some(listed), truncated))
    }

    fn cached(&self, chat: ChatId) -> Option<i64> {
        self.peers
            .lock()
            .expect("peer cache lock")
            .get(&chat)
            .copied()
    }

    /// The InputPeer for a chat id. Ids we issue carry everything needed (basic
    /// groups need no access hash). Ids without a hash, issued before ids carried
    /// it, come from the peer cache, refilled from the dialog list if this Live
    /// client hasn't seen them yet (e.g. right after a restart).
    async fn resolve(&self, id: ChatRef) -> Result<tl::enums::InputPeer, TgError> {
        if let Some(peer) = direct_input_peer(id) {
            return Ok(peer);
        }
        let chat = id.chat();
        if let Some(hash) = self.cached(chat) {
            return Ok(input_peer(chat, Some(hash)));
        }
        for archived in [false, true] {
            let mut cursor: Option<String> = None;
            for _ in 0..RESOLVE_MAX_PAGES {
                let page = self
                    .list_chats(ChatsQuery {
                        limit: 100,
                        cursor: cursor.clone(),
                        archived,
                        unread_only: false,
                        chat_folder: None,
                    })
                    .await?;
                if let Some(hash) = self.cached(chat) {
                    return Ok(input_peer(chat, Some(hash)));
                }
                match page.next_cursor {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
        }
        Err(TgError::InvalidInput(format!(
            "{chat} is not one of your chats; chat ids come from list_chats"
        )))
    }

    /// Like `classify`, but "you can't read this chat" is the caller's problem.
    fn classify_chat_error(
        &self,
        err: InvocationError,
        chat: ChatId,
        class: MethodClass,
    ) -> TgError {
        let inaccessible = [
            "CHANNEL_PRIVATE",
            "CHANNEL_INVALID",
            "PEER_ID_INVALID",
            "CHAT_ID_INVALID",
            "USER_ID_INVALID",
        ];
        if inaccessible.iter().any(|code| err.is(code)) {
            return TgError::InvalidInput(format!("{chat} can't be read by this account"));
        }
        self.classify(err, class)
    }
}

#[async_trait::async_trait]
impl MemberSource for LiveClient {
    async fn channel_access(&self, id: ChatRef) -> Result<ChannelAccess, TgError> {
        let chat = id.chat();
        let channel = self.input_channel(id).await?;
        let tl::enums::messages::ChatFull::Full(full) = self
            .call(
                &tl::functions::channels::GetFullChannel { channel },
                Some(chat),
            )
            .await?;
        // Admins see hidden members; the channel object says whether the
        // Account is one.
        let admin = full.chats.iter().any(|c| match c {
            tl::enums::Chat::Channel(c) if ChatId::Channel(c.id) == chat => {
                c.creator || c.admin_rights.is_some()
            }
            _ => false,
        });
        self.described(full.users, full.chats);
        match full.full_chat {
            tl::enums::ChatFull::ChannelFull(c) => Ok(ChannelAccess {
                can_list: c.can_view_participants,
                hidden: c.participants_hidden && !admin,
            }),
            _ => Err(TgError::Upstream(format!(
                "Telegram didn't describe {chat} as a channel"
            ))),
        }
    }

    async fn participants(
        &self,
        request: ParticipantsRequest,
    ) -> Result<ParticipantsAnswer, TgError> {
        let chat = request.channel.chat();
        let admins_only = request.filter == ParticipantsFilter::Admins;
        let channel = self.input_channel(request.channel).await?;
        let answer = self
            .call_refusing(
                &tl::functions::channels::GetParticipants {
                    channel,
                    filter: participants_filter(request.filter),
                    offset: request.offset,
                    limit: request.limit,
                    hash: 0,
                },
                Some(chat),
                |err| {
                    err.is("CHAT_ADMIN_REQUIRED")
                        .then(|| members::admin_required(chat, admins_only))
                },
            )
            .await?;
        match answer {
            tl::enums::channels::ChannelParticipants::Participants(p) => Ok(ParticipantsAnswer {
                count: p.count,
                returned: p.participants.len(),
                members: p
                    .participants
                    .into_iter()
                    .filter_map(raw_channel_member)
                    .collect(),
                peers: self.described(p.users, p.chats),
            }),
            // Only an answer to a request with a hash; ours has none.
            tl::enums::channels::ChannelParticipants::NotModified => Err(TgError::Upstream(
                "Telegram answered channelParticipantsNotModified without a hash".into(),
            )),
        }
    }

    async fn group_members(&self, chat_id: i64) -> Result<GroupAnswer, TgError> {
        let chat = ChatId::Chat(chat_id);
        let tl::enums::messages::ChatFull::Full(full) = self
            .call(
                &tl::functions::messages::GetFullChat { chat_id },
                Some(chat),
            )
            .await?;
        let peers = self.described(full.users, full.chats);
        let members = match full.full_chat {
            tl::enums::ChatFull::Full(c) => match c.participants {
                tl::enums::ChatParticipants::Participants(p) => {
                    Some(p.participants.into_iter().map(raw_chat_member).collect())
                }
                tl::enums::ChatParticipants::Forbidden(_) => None,
            },
            _ => {
                return Err(TgError::Upstream(format!(
                    "Telegram didn't describe {chat} as a basic group"
                )));
            }
        };
        Ok(GroupAnswer { members, peers })
    }
}

impl LiveClient {
    /// The InputChannel for a channel id; anything else can't have one.
    async fn input_channel(&self, id: ChatRef) -> Result<tl::enums::InputChannel, TgError> {
        match self.resolve(id).await? {
            tl::enums::InputPeer::Channel(c) => {
                Ok(tl::enums::InputChannel::Channel(tl::types::InputChannel {
                    channel_id: c.channel_id,
                    access_hash: c.access_hash,
                }))
            }
            _ => Err(TgError::InvalidInput(format!(
                "{} is not a supergroup or channel",
                id.chat()
            ))),
        }
    }
}

fn participants_filter(filter: ParticipantsFilter) -> tl::enums::ChannelParticipantsFilter {
    use tl::enums::ChannelParticipantsFilter as F;
    match filter {
        ParticipantsFilter::Recent => F::ChannelParticipantsRecent,
        ParticipantsFilter::Admins => F::ChannelParticipantsAdmins,
        ParticipantsFilter::Search(q) => {
            F::ChannelParticipantsSearch(tl::types::ChannelParticipantsSearch { q })
        }
    }
}

/// A supergroup's or channel's member; banned and left participants aren't.
fn raw_channel_member(participant: tl::enums::ChannelParticipant) -> Option<RawMember> {
    use tl::enums::ChannelParticipant as P;
    let (user, role) = match participant {
        P::Participant(p) => (p.user_id, Role::Member),
        P::ParticipantSelf(p) => (p.user_id, Role::Member),
        P::Creator(p) => (p.user_id, Role::Creator),
        P::Admin(p) => (p.user_id, Role::Admin),
        P::Banned(_) | P::Left(_) => return None,
    };
    Some(RawMember { user, role })
}

fn raw_chat_member(participant: tl::enums::ChatParticipant) -> RawMember {
    use tl::enums::ChatParticipant as P;
    let (user, role) = match participant {
        P::Participant(p) => (p.user_id, Role::Member),
        P::Creator(p) => (p.user_id, Role::Creator),
        P::Admin(p) => (p.user_id, Role::Admin),
    };
    RawMember { user, role }
}

/// A request for one Topic refused because the forum has no such Topic: the
/// caller's to fix.
fn topic_refusal(topic: Option<i32>, err: &InvocationError) -> Option<TgError> {
    topic
        .filter(|_| err.is("TOPIC_ID_INVALID") || err.is("MSG_ID_INVALID"))
        .map(topics::unknown_topic)
}

/// The Topic a forum message's reply header names (`forum_topic`): the
/// thread's top message, else the message it replies to, which is then the
/// Topic's first message. `None`: General, or no forum.
fn header_topic(header: Option<&tl::enums::MessageReplyHeader>) -> Option<i32> {
    match header {
        Some(tl::enums::MessageReplyHeader::Header(h)) if h.forum_topic => {
            h.reply_to_top_id.or(h.reply_to_msg_id)
        }
        _ => None,
    }
}

fn raw_history_message(message: tl::enums::Message) -> Option<RawHistoryMessage> {
    match message {
        tl::enums::Message::Message(m) => {
            let topic = header_topic(m.reply_to.as_ref());
            let (reply_to, reply_elsewhere) = match m.reply_to {
                // In a Topic, a message that names no top message only
                // points at the Topic's first one: no reply.
                Some(tl::enums::MessageReplyHeader::Header(h))
                    if h.forum_topic && h.reply_to_top_id.is_none() =>
                {
                    (None, None)
                }
                Some(tl::enums::MessageReplyHeader::Header(h)) => {
                    // reply_to_peer_id: the replied message is in another
                    // Chat (unless it names this one).
                    match (h.reply_to_msg_id, h.reply_to_peer_id.as_ref().map(chat_id)) {
                        (Some(id), Some(other)) if other != chat_id(&m.peer_id) => {
                            (None, Some(MessageAt { chat: other, id }))
                        }
                        (id, _) => (id, None),
                    }
                }
                // A story reply, or none.
                _ => (None, None),
            };
            Some(RawHistoryMessage {
                id: m.id,
                date: m.date,
                from: m.from_id.as_ref().map(chat_id),
                out: m.out,
                text: message_text(m.message, m.rich_message.as_ref()),
                media: media::describe(m.media.as_ref()),
                reply_to,
                reply_elsewhere,
                forwarded: m.fwd_from.is_some(),
                forwarded_from: m.fwd_from.as_ref().and_then(
                    |tl::enums::MessageFwdHeader::Header(h)| h.from_id.as_ref().map(chat_id),
                ),
                edited: m.edit_date.is_some(),
                reactions: raw_reactions(m.reactions),
                pinned: m.pinned,
                topic,
            })
        }
        tl::enums::Message::Service(m) => Some(RawHistoryMessage {
            // The message that created a Topic is that Topic's first.
            topic: match m.action {
                tl::enums::MessageAction::TopicCreate(_) => Some(m.id),
                _ => header_topic(m.reply_to.as_ref()),
            },
            id: m.id,
            date: m.date,
            from: m.from_id.as_ref().map(chat_id),
            out: m.out,
            text: "[service message]".into(),
            media: None,
            reply_to: None,
            reply_elsewhere: None,
            forwarded: false,
            forwarded_from: None,
            edited: false,
            reactions: raw_reactions(m.reactions),
            // A service message is never pinned.
            pinned: false,
        }),
        tl::enums::Message::Empty(_) => None,
    }
}

/// A message's reactions as Telegram counts them (`reactions.results`);
/// who reacted (`recent_reactions`, `top_reactors`) is left in TL.
fn raw_reactions(reactions: Option<tl::enums::MessageReactions>) -> Vec<RawReaction> {
    let Some(tl::enums::MessageReactions::Reactions(reactions)) = reactions else {
        return Vec::new();
    };
    reactions
        .results
        .into_iter()
        .filter_map(|tl::enums::ReactionCount::Count(c)| {
            let reaction = match c.reaction {
                tl::enums::Reaction::Emoji(e) => Reaction::Emoji(e.emoticon),
                tl::enums::Reaction::CustomEmoji(_) => Reaction::Custom,
                tl::enums::Reaction::Paid => Reaction::Paid,
                tl::enums::Reaction::Empty => return None,
            };
            Some(RawReaction {
                reaction,
                count: c.count,
            })
        })
        .collect()
}

/// A message reduced to where it is, its media (described as get_messages
/// describes it) and that media's file.
fn raw_media_message(message: tl::enums::Message) -> Option<RawMediaMessage> {
    match message {
        tl::enums::Message::Message(m) => Some(RawMediaMessage {
            at: MessageAt {
                chat: chat_id(&m.peer_id),
                id: m.id,
            },
            media: media::describe(m.media.as_ref()),
            file: media::file(m.media.as_ref()),
        }),
        tl::enums::Message::Service(m) => Some(RawMediaMessage {
            at: MessageAt {
                chat: chat_id(&m.peer_id),
                id: m.id,
            },
            media: None,
            file: None,
        }),
        tl::enums::Message::Empty(_) => None,
    }
}

/// A message reduced to where it is, its album, its media and what a view
/// can show of it.
fn raw_view_message(message: tl::enums::Message) -> Option<RawViewMessage> {
    match message {
        tl::enums::Message::Message(m) => Some(RawViewMessage {
            at: MessageAt {
                chat: chat_id(&m.peer_id),
                id: m.id,
            },
            grouped_id: m.grouped_id,
            media: media::describe(m.media.as_ref()),
            picture: media::picture(m.media.as_ref()),
        }),
        tl::enums::Message::Service(m) => Some(RawViewMessage {
            at: MessageAt {
                chat: chat_id(&m.peer_id),
                id: m.id,
            },
            grouped_id: None,
            media: None,
            picture: None,
        }),
        tl::enums::Message::Empty(_) => None,
    }
}

/// A search result: the message and the Chat it is in.
fn raw_found_message(message: tl::enums::Message) -> Option<RawFoundMessage> {
    let chat = match &message {
        tl::enums::Message::Message(m) => chat_id(&m.peer_id),
        tl::enums::Message::Service(m) => chat_id(&m.peer_id),
        tl::enums::Message::Empty(_) => return None,
    };
    Some(RawFoundMessage {
        chat,
        message: raw_history_message(message)?,
    })
}

/// Both lists of a `contacts.found` and the peers that describe them.
fn found_answer(found: tl::types::contacts::Found) -> FoundAnswer {
    FoundAnswer {
        my_results: found.my_results.iter().map(chat_id).collect(),
        results: found.results.iter().map(chat_id).collect(),
        peers: found
            .users
            .into_iter()
            .filter_map(raw_user)
            .chain(found.chats.into_iter().filter_map(raw_chat))
            .collect(),
    }
}

/// Which channels of an answer's `chats` are forums (non-min ones only: a
/// min channel says nothing reliable), and the Topics it describes.
fn raw_forums(chats: &[tl::enums::Chat], topics: Vec<tl::enums::ForumTopic>) -> RawForums {
    RawForums {
        forums: chats
            .iter()
            .filter_map(forum_flag)
            .filter_map(|(chat, forum)| forum.then_some(chat))
            .collect(),
        topics: topics.into_iter().filter_map(raw_topic).collect(),
    }
}

/// Whether a channel is a forum, unless it is described as min.
fn forum_flag(chat: &tl::enums::Chat) -> Option<(ChatId, bool)> {
    match chat {
        tl::enums::Chat::Channel(c) if !c.min => Some((ChatId::Channel(c.id), c.forum)),
        _ => None,
    }
}

/// A Topic Telegram describes; a deleted one is none.
fn raw_topic(topic: tl::enums::ForumTopic) -> Option<RawTopic> {
    match topic {
        tl::enums::ForumTopic::Topic(t) => Some(RawTopic {
            chat: chat_id(&t.peer),
            id: t.id,
            title: t.title,
            closed: t.closed,
            pinned: t.pinned,
        }),
        tl::enums::ForumTopic::Deleted(_) => None,
    }
}

/// Description and member count from a group's or channel's full info.
fn full_chat_details(full: tl::enums::ChatFull) -> (Option<String>, Option<i32>) {
    match full {
        tl::enums::ChatFull::Full(c) => {
            let members = match c.participants {
                tl::enums::ChatParticipants::Participants(p) => {
                    i32::try_from(p.participants.len()).ok()
                }
                tl::enums::ChatParticipants::Forbidden(_) => None,
            };
            (Some(c.about), members)
        }
        tl::enums::ChatFull::ChannelFull(c) => (Some(c.about), c.participants_count),
        _ => (None, None),
    }
}

// ── TL → plain values ───────────────────────────────────────────────────────

fn chat_id(peer: &tl::enums::Peer) -> ChatId {
    match peer {
        tl::enums::Peer::User(p) => ChatId::User(p.user_id),
        tl::enums::Peer::Chat(p) => ChatId::Chat(p.chat_id),
        tl::enums::Peer::Channel(p) => ChatId::Channel(p.channel_id),
    }
}

/// The InputPeer for an id that needs no lookup: a basic group, or a user or
/// channel whose id carries its access hash. `None`: look the hash up.
fn direct_input_peer(id: ChatRef) -> Option<tl::enums::InputPeer> {
    match (id.chat(), id.access_hash()) {
        (chat @ ChatId::Chat(_), _) => Some(input_peer(chat, None)),
        (chat, Some(hash)) => Some(input_peer(chat, Some(hash))),
        (_, None) => None,
    }
}

/// `FILE_MIGRATE_X`'s error code (grammers' own downloader matches on it).
/// Telegram's refusal of a `messages.transcribeAudio`, if `err` is one.
fn transcribe_refusal(err: &InvocationError) -> Option<Refusal> {
    match err {
        InvocationError::Rpc(rpc) => Refusal::of(&rpc.name),
        _ => None,
    }
}

fn transcribed_audio(answer: tl::enums::messages::TranscribedAudio) -> TranscribedAudio {
    let tl::enums::messages::TranscribedAudio::Audio(audio) = answer;
    TranscribedAudio {
        pending: audio.pending,
        transcription_id: audio.transcription_id,
        text: audio.text,
        // Both or neither (one flag in the schema).
        trial: audio.trial_remains_num.map(|remaining| Trial {
            remaining: u32::try_from(remaining).unwrap_or(0),
            resets_at: audio.trial_remains_until_date.unwrap_or(0),
        }),
    }
}

/// The `updateTranscribedAudio` updates in what Telegram pushed.
fn transcription_updates(updates: UpdatesLike) -> Vec<TranscriptionUpdate> {
    let UpdatesLike::Updates(updates) = updates else {
        return Vec::new();
    };
    let all = match updates {
        tl::enums::Updates::UpdateShort(short) => vec![short.update],
        tl::enums::Updates::Updates(full) => full.updates,
        tl::enums::Updates::Combined(combined) => combined.updates,
        _ => Vec::new(),
    };
    all.into_iter()
        .filter_map(|update| match update {
            tl::enums::Update::TranscribedAudio(t) => Some(TranscriptionUpdate {
                transcription_id: t.transcription_id,
                pending: t.pending,
                text: t.text,
            }),
            _ => None,
        })
        .collect()
}

/// Pass a Live client's transcription updates on to its feed, dropping
/// every other update (nothing else listens for them), until the client
/// is gone. Nothing is kept.
async fn forward_transcriptions(mut updates: mpsc::Receiver<UpdatesLike>, feed: TranscriptionFeed) {
    while let Some(pushed) = updates.recv().await {
        for update in transcription_updates(pushed) {
            tracing::debug!(
                transcription_id = update.transcription_id,
                pending = update.pending,
                chars = update.text.chars().count(),
                "transcription update"
            );
            // Nobody waiting is fine: the update is for no one now.
            let _ = feed.send(update);
        }
    }
}

const FILE_MIGRATE: i32 = 303;

fn input_file_location(location: FileLocation) -> tl::enums::InputFileLocation {
    let FileLocation {
        file,
        id,
        access_hash,
        file_reference,
        dc_id: _,
    } = location;
    match file {
        StoredFile::Photo { thumb_size } => tl::enums::InputFileLocation::InputPhotoFileLocation(
            tl::types::InputPhotoFileLocation {
                id,
                access_hash,
                file_reference,
                thumb_size,
            },
        ),
        StoredFile::Document => tl::enums::InputFileLocation::InputDocumentFileLocation(
            tl::types::InputDocumentFileLocation {
                id,
                access_hash,
                file_reference,
                thumb_size: String::new(),
            },
        ),
        StoredFile::DocumentThumb { thumb_size } => {
            tl::enums::InputFileLocation::InputDocumentFileLocation(
                tl::types::InputDocumentFileLocation {
                    id,
                    access_hash,
                    file_reference,
                    thumb_size,
                },
            )
        }
    }
}

fn input_peer(id: ChatId, access_hash: Option<i64>) -> tl::enums::InputPeer {
    let access_hash = access_hash.unwrap_or(0);
    match id {
        ChatId::User(user_id) => tl::enums::InputPeer::User(tl::types::InputPeerUser {
            user_id,
            access_hash,
        }),
        ChatId::Chat(chat_id) => tl::enums::InputPeer::Chat(tl::types::InputPeerChat { chat_id }),
        ChatId::Channel(channel_id) => tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
            channel_id,
            access_hash,
        }),
    }
}

impl LiveClient {
    /// One `messages.getDialogs` call for [`chats::list_chats`], through the
    /// Account's limiter like every request.
    async fn get_dialogs(&self, request: DialogsRequest) -> Result<DialogsAnswer, TgError> {
        let DialogsRequest {
            listing,
            after,
            limit,
        } = request;
        let (offset_date, offset_id, offset_peer) = match after {
            Some(Cursor {
                offset_date,
                offset_id,
                offset_peer: Some(peer),
                offset_access_hash,
                ..
            }) => (
                offset_date,
                offset_id,
                input_peer(peer.parse()?, offset_access_hash),
            ),
            _ => (0, 0, tl::enums::InputPeer::Empty),
        };
        let answer = self
            .call(
                &tl::functions::messages::GetDialogs {
                    exclude_pinned: listing.exclude_pinned(),
                    folder_id: Some(listing.folder_id()),
                    offset_date,
                    offset_id,
                    offset_peer,
                    limit: limit as i32,
                    hash: 0,
                },
                None,
            )
            .await?;
        let (dialogs, messages, chats, users) = match answer {
            tl::enums::messages::Dialogs::Dialogs(d) => (d.dialogs, d.messages, d.chats, d.users),
            tl::enums::messages::Dialogs::Slice(d) => (d.dialogs, d.messages, d.chats, d.users),
            tl::enums::messages::Dialogs::NotModified(_) => (vec![], vec![], vec![], vec![]),
        };
        let now = unix_now();
        Ok(DialogsAnswer {
            sent: dialogs.len(),
            dialogs: dialogs
                .into_iter()
                .filter_map(|d| raw_dialog(d, now))
                .collect(),
            peers: self.described(users, chats),
            messages: messages.into_iter().filter_map(raw_message).collect(),
        })
    }
}

impl LiveClient {
    /// One `messages.getDialogFilters` call: the Account's Chat folders.
    async fn dialog_filters(&self) -> Result<Vec<RawChatFolder>, TgError> {
        let tl::enums::messages::DialogFilters::Filters(answer) = self
            .call(&tl::functions::messages::GetDialogFilters {}, None)
            .await?;
        let me = self.grant.account_id;
        Ok(answer
            .filters
            .into_iter()
            .filter_map(|filter| raw_chat_folder(filter, me))
            .collect())
    }

    /// One `messages.getPeerDialogs` call for [`folders::list_folder_chats`]:
    /// the dialogs Telegram has of `peers`. A Chat named without its access
    /// hash is asked for with the peer cache's, the Account's own Saved
    /// Messages as `inputPeerSelf`, or not at all (no call when none is left).
    async fn peer_dialogs(&self, peers: Vec<ChatRef>) -> Result<DialogsAnswer, TgError> {
        let me = ChatId::User(self.grant.account_id);
        let peers: Vec<tl::enums::InputDialogPeer> = peers
            .into_iter()
            .filter_map(|id| {
                let peer = direct_input_peer(id)
                    .or_else(|| (id.chat() == me).then_some(tl::enums::InputPeer::PeerSelf))
                    .or_else(|| Some(input_peer(id.chat(), Some(self.cached(id.chat())?))))?;
                Some(tl::enums::InputDialogPeer::Peer(
                    tl::types::InputDialogPeer { peer },
                ))
            })
            .collect();
        if peers.is_empty() {
            return Ok(DialogsAnswer::default());
        }
        let tl::enums::messages::PeerDialogs::Dialogs(answer) = self
            .call(&tl::functions::messages::GetPeerDialogs { peers }, None)
            .await?;
        let now = unix_now();
        Ok(DialogsAnswer {
            sent: answer.dialogs.len(),
            dialogs: answer
                .dialogs
                .into_iter()
                .filter_map(|d| raw_dialog(d, now))
                .collect(),
            peers: self.described(answer.users, answer.chats),
            messages: answer
                .messages
                .into_iter()
                .filter_map(raw_message)
                .collect(),
        })
    }
}

/// A Chat folder as whoami and list_chats read it; the "All chats" entry
/// (`dialogFilterDefault`) is none. `me` is the Account, for a folder that
/// names its own Saved Messages as `inputPeerSelf`.
fn raw_chat_folder(filter: tl::enums::DialogFilter, me: i64) -> Option<RawChatFolder> {
    let refs = |peers: Vec<tl::enums::InputPeer>| -> Vec<ChatRef> {
        peers.iter().filter_map(|p| folder_peer(p, me)).collect()
    };
    let title = |title: tl::enums::TextWithEntities| {
        let tl::enums::TextWithEntities::Entities(title) = title;
        title.text
    };
    match filter {
        tl::enums::DialogFilter::Filter(f) => Some(RawChatFolder {
            folder: ChatFolder {
                id: f.id,
                title: title(f.title),
                emoji: f.emoticon,
                // Every flag that adds or drops Chats by what they are.
                rules: f.contacts
                    || f.non_contacts
                    || f.groups
                    || f.broadcasts
                    || f.bots
                    || f.exclude_muted
                    || f.exclude_read
                    || f.exclude_archived,
            },
            pinned: refs(f.pinned_peers),
            included: refs(f.include_peers),
            excluded: refs(f.exclude_peers)
                .into_iter()
                .map(|r| r.chat())
                .collect(),
        }),
        // A shared Chat folder (chatlist) is a list by construction.
        tl::enums::DialogFilter::Chatlist(f) => Some(RawChatFolder {
            folder: ChatFolder {
                id: f.id,
                title: title(f.title),
                emoji: f.emoticon,
                rules: false,
            },
            pinned: refs(f.pinned_peers),
            included: refs(f.include_peers),
            excluded: Vec::new(),
        }),
        tl::enums::DialogFilter::Default => None,
    }
}

/// A Chat a folder names, with its access hash when the folder carries one.
/// A peer known only from a message has none: it is addressed through the
/// peer cache, if at all.
fn folder_peer(peer: &tl::enums::InputPeer, me: i64) -> Option<ChatRef> {
    Some(match peer {
        tl::enums::InputPeer::User(u) => ChatRef::new(ChatId::User(u.user_id), Some(u.access_hash)),
        tl::enums::InputPeer::Chat(c) => ChatRef::new(ChatId::Chat(c.chat_id), None),
        tl::enums::InputPeer::Channel(c) => {
            ChatRef::new(ChatId::Channel(c.channel_id), Some(c.access_hash))
        }
        tl::enums::InputPeer::PeerSelf => ChatRef::new(ChatId::User(me), None),
        tl::enums::InputPeer::UserFromMessage(u) => ChatRef::new(ChatId::User(u.user_id), None),
        tl::enums::InputPeer::ChannelFromMessage(c) => {
            ChatRef::new(ChatId::Channel(c.channel_id), None)
        }
        tl::enums::InputPeer::Empty => return None,
    })
}

/// A dialog as list_chats reads it, muted or not at Unix time `now`.
fn raw_dialog(dialog: tl::enums::Dialog, now: i64) -> Option<RawDialog> {
    match dialog {
        tl::enums::Dialog::Dialog(d) => Some(RawDialog {
            chat: chat_id(&d.peer),
            top_message: d.top_message,
            unread_count: d.unread_count,
            pinned: d.pinned,
            folder_id: d.folder_id.unwrap_or(0),
            unread_mark: d.unread_mark,
            muted: match d.notify_settings {
                tl::enums::PeerNotifySettings::Settings(settings) => {
                    chats::is_muted(settings.mute_until, now)
                }
            },
        }),
        // The archive folder entry and communities aren't Chats.
        _ => None,
    }
}

fn raw_user(user: tl::enums::User) -> Option<RawPeer> {
    let tl::enums::User::User(u) = user else {
        return None;
    };
    let title = if u.deleted {
        "Deleted Account".to_string()
    } else {
        [u.first_name.as_deref(), u.last_name.as_deref()]
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    };
    Some(RawPeer {
        id: ChatId::User(u.id),
        title,
        kind: if u.bot {
            ChatKind::Bot
        } else {
            ChatKind::Private
        },
        username: u.username,
        // A "min" user's hash only works in the message it came with: never hand it out.
        access_hash: u.access_hash.filter(|_| !u.min),
        is_self: u.is_self,
    })
}

/// What the user object says of a user's profile: status, flags, phone.
/// The rest comes from `users.getFullUser`'s `UserFull`.
fn raw_profile(user: &tl::types::User) -> RawProfile {
    RawProfile {
        status: user.status.as_ref().map(|status| match status {
            tl::enums::UserStatus::Empty => RawUserStatus::Empty,
            tl::enums::UserStatus::Online(s) => RawUserStatus::Online { expires: s.expires },
            tl::enums::UserStatus::Offline(s) => RawUserStatus::Offline {
                was_online: s.was_online,
            },
            tl::enums::UserStatus::Recently(_) => RawUserStatus::Recently,
            tl::enums::UserStatus::LastWeek(_) => RawUserStatus::LastWeek,
            tl::enums::UserStatus::LastMonth(_) => RawUserStatus::LastMonth,
        }),
        bot: user.bot,
        verified: user.verified,
        premium: user.premium,
        scam: user.scam,
        fake: user.fake,
        // Telegram leaves it out unless the user shows it to the Account.
        phone: user.phone.clone(),
        ..RawProfile::default()
    }
}

fn raw_chat(chat: tl::enums::Chat) -> Option<RawPeer> {
    let channel_kind = |megagroup| {
        if megagroup {
            ChatKind::Supergroup
        } else {
            ChatKind::Channel
        }
    };
    match chat {
        tl::enums::Chat::Chat(c) => Some(RawPeer {
            id: ChatId::Chat(c.id),
            title: c.title,
            kind: ChatKind::Group,
            username: None,
            access_hash: None,
            is_self: false,
        }),
        tl::enums::Chat::Forbidden(c) => Some(RawPeer {
            id: ChatId::Chat(c.id),
            title: c.title,
            kind: ChatKind::Group,
            username: None,
            access_hash: None,
            is_self: false,
        }),
        tl::enums::Chat::Channel(c) => Some(RawPeer {
            id: ChatId::Channel(c.id),
            title: c.title,
            kind: channel_kind(c.megagroup),
            username: c.username,
            access_hash: c.access_hash.filter(|_| !c.min),
            is_self: false,
        }),
        tl::enums::Chat::ChannelForbidden(c) => Some(RawPeer {
            id: ChatId::Channel(c.id),
            title: c.title,
            kind: channel_kind(c.megagroup),
            username: None,
            access_hash: Some(c.access_hash),
            is_self: false,
        }),
        _ => None,
    }
}

fn raw_message(message: tl::enums::Message) -> Option<RawMessage> {
    match message {
        tl::enums::Message::Message(m) => Some(RawMessage {
            chat: chat_id(&m.peer_id),
            id: m.id,
            date: m.date,
            media: media::describe(m.media.as_ref()).map(|media| media.kind),
            text: message_text(m.message, m.rich_message.as_ref()),
        }),
        tl::enums::Message::Service(m) => Some(RawMessage {
            chat: chat_id(&m.peer_id),
            id: m.id,
            date: m.date,
            text: "[service message]".into(),
            media: None,
        }),
        tl::enums::Message::Empty(_) => None,
    }
}

fn messages_filter(filter: Option<MediaFilter>) -> tl::enums::MessagesFilter {
    use tl::enums::MessagesFilter as F;
    match filter {
        None => F::InputMessagesFilterEmpty,
        Some(MediaFilter::Photo) => F::InputMessagesFilterPhotos,
        Some(MediaFilter::Video) => F::InputMessagesFilterVideo,
        Some(MediaFilter::Document) => F::InputMessagesFilterDocument,
        Some(MediaFilter::Link) => F::InputMessagesFilterUrl,
        Some(MediaFilter::Voice) => F::InputMessagesFilterVoice,
        Some(MediaFilter::VideoNote) => F::InputMessagesFilterRoundVideo,
        Some(MediaFilter::Gif) => F::InputMessagesFilterGif,
    }
}

pub struct GrammersConnector {
    credentials: AppCredentials,
    vault: Arc<Vault>,
    limits: Arc<Limits>,
    pool: Pool<LiveClient>,
    /// Held while Live clients taken out of the pool are re-sealed, so shutdown
    /// waits for a sweep's in-flight saves instead of exiting under them.
    resealing: tokio::sync::Mutex<()>,
}

impl GrammersConnector {
    pub fn new(
        credentials: AppCredentials,
        vault: Arc<Vault>,
        limits: Arc<Limits>,
        idle_ttl: Duration,
    ) -> Arc<Self> {
        let connector = Arc::new(Self {
            credentials,
            vault,
            limits,
            pool: Pool::new(idle_ttl),
            resealing: tokio::sync::Mutex::new(()),
        });
        tokio::spawn(sweep(Arc::downgrade(&connector)));
        connector
    }

    async fn open(&self, grant: OpenedGrant) -> Result<LiveClient, TgError> {
        let grant_account = grant.account_id;
        let store = SessionStore::from_snapshot(&grant.telegram_session)?;
        let Connected {
            client,
            store,
            updates,
        } = connect(&self.credentials, store, Purpose::Live);
        let transcriptions = transcribe::feed();
        tokio::spawn(forward_transcriptions(updates, transcriptions.clone()));
        // Prove the session still works before handing it out.
        let live = LiveClient {
            client,
            store,
            grant,
            peers: Default::default(),
            forums: Default::default(),
            dead: Default::default(),
            limiter: self.limits.for_account(grant_account),
            transcriptions,
            #[cfg(test)]
            sent: Default::default(),
            #[cfg(test)]
            answers: Default::default(),
        };
        match live.whoami().await {
            Ok(_) => Ok(live),
            Err(err) => {
                live.shutdown();
                Err(err)
            }
        }
    }

    /// Seal each changed Telegram session back into its Grant. Returns how many
    /// were saved and how many failed.
    async fn reseal(&self, changed: Vec<Evicted<LiveClient>>) -> (usize, usize) {
        let (mut saved, mut failed) = (0, 0);
        for (_, live, snapshot) in changed {
            match self.vault.save_session(&live.grant, &snapshot).await {
                Ok(()) => saved += 1,
                Err(err) => {
                    failed += 1;
                    tracing::warn!(grant_id = %live.grant.grant_id, error = %err, "re-sealing telegram session failed");
                }
            }
        }
        (saved, failed)
    }
}

#[async_trait::async_trait]
impl Connector for GrammersConnector {
    async fn connect(&self, grant: OpenedGrant) -> Result<Arc<dyn Telegram>, TgError> {
        let key = grant.grant_id.as_str().to_string();
        let snapshot = grant.telegram_session.clone();
        let live = self
            .pool
            .get_or_open(&key, &snapshot, || self.open(grant))
            .await?;
        if live.is_dead() {
            // Revoked in Telegram since it was opened: drop it so the client re-authorizes.
            self.pool.remove(&key);
            return Err(TgError::BadSession("telegram session was revoked".into()));
        }
        Ok(live)
    }

    async fn log_out(&self, grant: OpenedGrant) -> Result<(), TgError> {
        // A pooled Live client may hold a newer session than the sealed one (e.g.
        // after a DC migration); log out with that one.
        let snapshot = match self.pool.remove(grant.grant_id.as_str()) {
            Some(live) => live.snapshot()?,
            None => grant.telegram_session.clone(),
        };
        drop(grant);
        let Connected { client, .. } = connect(
            &self.credentials,
            SessionStore::from_snapshot(&snapshot)?,
            Purpose::Live,
        );
        let result = tokio::time::timeout(
            LOG_OUT_TIMEOUT,
            client.invoke(&tl::functions::auth::LogOut {}),
        )
        .await;
        client.disconnect();
        match result {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(err)) if is_dead_session(&err) => Ok(()),
            Ok(Err(err)) => Err(upstream(err)),
            Err(_) => Err(TgError::Upstream("auth.logOut timed out".into())),
        }
    }

    async fn shutdown(&self) {
        let _resealing = self.resealing.lock().await;
        let live_clients = self.pool.len();
        let (resealed, failed) = self.reseal(self.pool.drain()).await;
        tracing::info!(live_clients, resealed, failed, "client pool flushed");
    }
}

/// Every minute: evict idle Live clients and re-seal sessions that changed.
async fn sweep(connector: Weak<GrammersConnector>) {
    let mut every = tokio::time::interval(Duration::from_secs(60));
    loop {
        every.tick().await;
        let Some(connector) = connector.upgrade() else {
            return;
        };
        let _resealing = connector.resealing.lock().await;
        connector.reseal(connector.pool.evict_idle()).await;
    }
}

/// An RPC error the way grammers hands it over: parsed from Telegram's
/// `rpc_error` (`FLOOD_WAIT_31` → name `FLOOD_WAIT`, value 31) and tagged with the
/// request that caused it.
#[cfg(test)]
fn rpc<R: tl::Identifiable>(code: i32, message: &str) -> InvocationError {
    InvocationError::Rpc(
        grammers_mtsender::RpcError::from(tl::types::RpcError {
            error_code: code,
            error_message: message.into(),
        })
        .with_caused_by(R::CONSTRUCTOR_ID),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A user or channel 7 with access hash 99, as Telegram would send it (only
    /// the fields that aren't optional; the TL structs have 50 of them).
    pub(super) fn tl_peers(min: bool) -> (tl::enums::User, tl::enums::Chat) {
        use grammers_client::tl::{Deserializable, Identifiable, Serializable};
        let user_flags: u32 = 1 | if min { 1 << 20 } else { 0 };
        let mut user = tl::types::User::CONSTRUCTOR_ID.to_le_bytes().to_vec();
        for part in [
            user_flags.to_bytes(),
            0u32.to_bytes(),
            7i64.to_bytes(),
            99i64.to_bytes(),
        ] {
            user.extend(part);
        }
        let channel_flags: u32 = (1 << 13) | if min { 1 << 12 } else { 0 };
        let mut channel = tl::types::Channel::CONSTRUCTOR_ID.to_le_bytes().to_vec();
        for part in [
            channel_flags.to_bytes(),
            0u32.to_bytes(),
            7i64.to_bytes(),
            99i64.to_bytes(),
            String::new().to_bytes(),
            tl::enums::ChatPhoto::Empty.to_bytes(),
            0i32.to_bytes(),
        ] {
            channel.extend(part);
        }
        (
            tl::enums::User::from_bytes(&user).unwrap(),
            tl::enums::Chat::from_bytes(&channel).unwrap(),
        )
    }

    #[test]
    fn dialogs_keep_their_folder_and_pin() {
        use grammers_client::tl::{Deserializable, Identifiable, Serializable};
        let mut no_settings = tl::types::PeerNotifySettings::CONSTRUCTOR_ID.to_bytes();
        no_settings.extend(0u32.to_bytes());
        let dialog = |folder_id, pinned| {
            tl::enums::Dialog::Dialog(tl::types::Dialog {
                pinned,
                unread_mark: false,
                view_forum_as_messages: false,
                peer: tl::enums::Peer::Channel(tl::types::PeerChannel { channel_id: 7 }),
                top_message: 55,
                read_inbox_max_id: 0,
                read_outbox_max_id: 0,
                unread_count: 3,
                unread_mentions_count: 0,
                unread_reactions_count: 0,
                unread_poll_votes_count: 0,
                notify_settings: tl::enums::PeerNotifySettings::from_bytes(&no_settings).unwrap(),
                pts: None,
                draft: None,
                folder_id,
                ttl_period: None,
            })
        };
        let archived = raw_dialog(dialog(Some(1), true), 0).unwrap();
        assert_eq!(
            archived,
            RawDialog {
                chat: ChatId::Channel(7),
                top_message: 55,
                unread_count: 3,
                pinned: true,
                folder_id: 1,
                unread_mark: false,
                muted: false,
            }
        );
        // Telegram leaves the flag out for the main list.
        let main = raw_dialog(dialog(None, false), 0).unwrap();
        assert_eq!((main.folder_id, main.pinned), (0, false));
    }

    #[test]
    fn dialogs_say_whether_they_are_muted_or_marked_unread() {
        const NOW: i64 = 1_700_000_000;
        let dialog = |unread_mark, mute_until| {
            tl::enums::Dialog::Dialog(tl::types::Dialog {
                pinned: false,
                unread_mark,
                view_forum_as_messages: false,
                peer: tl::enums::Peer::User(tl::types::PeerUser { user_id: 3 }),
                top_message: 9,
                read_inbox_max_id: 0,
                read_outbox_max_id: 0,
                unread_count: 0,
                unread_mentions_count: 0,
                unread_reactions_count: 0,
                unread_poll_votes_count: 0,
                notify_settings: tl::enums::PeerNotifySettings::Settings(
                    tl::types::PeerNotifySettings {
                        show_previews: None,
                        silent: None,
                        mute_until,
                        ios_sound: None,
                        android_sound: None,
                        other_sound: None,
                        stories_muted: None,
                        stories_hide_sender: None,
                        stories_ios_sound: None,
                        stories_android_sound: None,
                        stories_other_sound: None,
                    },
                ),
                pts: None,
                draft: None,
                folder_id: None,
                ttl_period: None,
            })
        };
        let read = |d| {
            let d: RawDialog = raw_dialog(d, NOW).unwrap();
            (d.unread_mark, d.muted)
        };
        assert_eq!(read(dialog(true, None)), (true, false));
        assert_eq!(read(dialog(false, Some(i32::MAX))), (false, true));
        assert_eq!(read(dialog(false, Some(NOW as i32 + 60))), (false, true));
        assert_eq!(read(dialog(false, Some(NOW as i32 - 60))), (false, false));
    }

    #[test]
    fn min_peers_hashes_are_never_handed_out() {
        // A "min" hash only works inside the message it came with.
        let (user, channel) = tl_peers(true);
        assert_eq!(raw_user(user).unwrap().access_hash, None);
        assert_eq!(raw_chat(channel).unwrap().access_hash, None);

        let (user, channel) = tl_peers(false);
        assert_eq!(raw_user(user).unwrap().access_hash, Some(99));
        assert_eq!(raw_chat(channel).unwrap().access_hash, Some(99));
    }

    #[test]
    fn a_users_status_flags_and_phone_come_from_the_user_object() {
        let (tl::enums::User::User(mut user), _) = tl_peers(false) else {
            unreachable!()
        };
        // Nothing shown: no status, no phone, no flags.
        assert_eq!(raw_profile(&user), RawProfile::default());

        user.bot = true;
        user.verified = true;
        user.premium = true;
        user.scam = true;
        user.fake = true;
        user.phone = Some("79991234567".into());
        let profile = raw_profile(&user);
        assert!(profile.bot && profile.verified && profile.premium);
        assert!(profile.scam && profile.fake);
        assert_eq!(profile.phone.as_deref(), Some("79991234567"));

        use tl::{enums::UserStatus as S, types};
        for (status, raw) in [
            (S::Empty, RawUserStatus::Empty),
            (
                S::Online(types::UserStatusOnline { expires: 5 }),
                RawUserStatus::Online { expires: 5 },
            ),
            (
                S::Offline(types::UserStatusOffline { was_online: 6 }),
                RawUserStatus::Offline { was_online: 6 },
            ),
            (
                S::Recently(types::UserStatusRecently { by_me: true }),
                RawUserStatus::Recently,
            ),
            (
                S::LastWeek(types::UserStatusLastWeek { by_me: false }),
                RawUserStatus::LastWeek,
            ),
            (
                S::LastMonth(types::UserStatusLastMonth { by_me: false }),
                RawUserStatus::LastMonth,
            ),
        ] {
            user.status = Some(status);
            assert_eq!(raw_profile(&user).status, Some(raw));
        }
    }

    #[test]
    fn members_roles_come_from_the_participant_kind_and_the_rest_are_no_members() {
        use tl::{enums::ChannelParticipant as P, types};
        let admin_rights = || {
            tl::enums::ChatAdminRights::Rights(types::ChatAdminRights {
                change_info: false,
                post_messages: false,
                edit_messages: false,
                delete_messages: false,
                ban_users: false,
                invite_users: false,
                pin_messages: false,
                add_admins: false,
                anonymous: false,
                manage_call: false,
                other: false,
                manage_topics: false,
                post_stories: false,
                edit_stories: false,
                delete_stories: false,
                manage_direct_messages: false,
                manage_ranks: false,
                manage_linked_peers: false,
                manage_welcome_messages: false,
            })
        };
        let channel = [
            P::Participant(types::ChannelParticipant {
                user_id: 1,
                date: 0,
                subscription_until_date: None,
                rank: None,
            }),
            P::ParticipantSelf(types::ChannelParticipantSelf {
                via_request: false,
                user_id: 2,
                inviter_id: 1,
                date: 0,
                subscription_until_date: None,
                rank: None,
            }),
            P::Creator(types::ChannelParticipantCreator {
                user_id: 3,
                admin_rights: admin_rights(),
                rank: None,
            }),
            P::Admin(types::ChannelParticipantAdmin {
                can_edit: false,
                is_self: false,
                user_id: 4,
                inviter_id: None,
                promoted_by: 3,
                date: 0,
                admin_rights: admin_rights(),
                rank: Some("boss".into()),
            }),
            P::Left(types::ChannelParticipantLeft {
                peer: tl::enums::Peer::User(types::PeerUser { user_id: 5 }),
            }),
        ];
        let members: Vec<RawMember> = channel.into_iter().filter_map(raw_channel_member).collect();
        let member = |user, role| RawMember { user, role };
        assert_eq!(
            members,
            [
                member(1, Role::Member),
                member(2, Role::Member),
                member(3, Role::Creator),
                member(4, Role::Admin),
            ]
        );

        use tl::enums::ChatParticipant as C;
        assert_eq!(
            raw_chat_member(C::Creator(types::ChatParticipantCreator {
                user_id: 9,
                rank: None
            })),
            member(9, Role::Creator)
        );
        assert_eq!(
            raw_chat_member(C::Admin(types::ChatParticipantAdmin {
                user_id: 8,
                inviter_id: 9,
                date: 0,
                rank: None
            })),
            member(8, Role::Admin)
        );
        assert_eq!(
            raw_chat_member(C::Participant(types::ChatParticipant {
                user_id: 7,
                inviter_id: 9,
                date: 0,
                rank: None
            })),
            member(7, Role::Member)
        );
    }

    #[test]
    fn a_contacts_search_answer_keeps_both_lists_and_no_min_hash() {
        let user = tl::enums::Peer::User(tl::types::PeerUser { user_id: 7 });
        let channel = tl::enums::Peer::Channel(tl::types::PeerChannel { channel_id: 7 });
        let (min_user, _) = tl_peers(true);
        let (_, full_channel) = tl_peers(false);
        let answer = found_answer(tl::types::contacts::Found {
            my_results: vec![user.clone()],
            results: vec![channel, user],
            chats: vec![full_channel],
            users: vec![min_user],
        });
        assert_eq!(answer.my_results, [ChatId::User(7)]);
        assert_eq!(answer.results, [ChatId::Channel(7), ChatId::User(7)]);
        let hashes: Vec<(ChatId, Option<i64>)> =
            answer.peers.iter().map(|p| (p.id, p.access_hash)).collect();
        assert_eq!(
            hashes,
            [(ChatId::User(7), None), (ChatId::Channel(7), Some(99))]
        );
    }

    /// Message `id` in `peer`, replying as `reply_to` says (only the fields
    /// that aren't optional, plus `reply_to`).
    fn tl_message(
        id: i32,
        peer: tl::enums::Peer,
        reply_to: Option<tl::enums::MessageReplyHeader>,
    ) -> tl::enums::Message {
        use grammers_client::tl::{Deserializable, Identifiable, Serializable};
        let mut bytes = tl::types::Message::CONSTRUCTOR_ID.to_le_bytes().to_vec();
        let flags: u32 = if reply_to.is_some() { 1 << 3 } else { 0 };
        bytes.extend(flags.to_bytes());
        bytes.extend(0u32.to_bytes());
        bytes.extend(id.to_bytes());
        bytes.extend(peer.to_bytes());
        if let Some(reply_to) = reply_to {
            bytes.extend(reply_to.to_bytes());
        }
        bytes.extend(100i32.to_bytes());
        bytes.extend(String::from("text").to_bytes());
        tl::enums::Message::from_bytes(&bytes).unwrap()
    }

    fn header(msg_id: Option<i32>, peer: Option<tl::enums::Peer>) -> tl::enums::MessageReplyHeader {
        tl::enums::MessageReplyHeader::Header(tl::types::MessageReplyHeader {
            reply_to_scheduled: false,
            forum_topic: false,
            quote: false,
            reply_to_ephemeral: false,
            reply_to_msg_id: msg_id,
            reply_to_peer_id: peer,
            reply_from: None,
            reply_media: None,
            reply_to_top_id: None,
            quote_text: None,
            quote_entities: None,
            quote_offset: None,
            todo_item_id: None,
            poll_option: None,
        })
    }

    #[test]
    fn a_reply_into_another_chat_is_kept_apart_from_reply_to() {
        let channel = |channel_id| tl::enums::Peer::Channel(tl::types::PeerChannel { channel_id });
        let reply = |reply_to| {
            let found = raw_found_message(tl_message(30, channel(5), reply_to)).unwrap();
            assert_eq!(found.chat, ChatId::Channel(5));
            (found.message.reply_to, found.message.reply_elsewhere)
        };
        assert_eq!(reply(None), (None, None));
        assert_eq!(reply(Some(header(Some(20), None))), (Some(20), None));
        // reply_to_peer_id naming the chat itself is still this chat.
        assert_eq!(
            reply(Some(header(Some(20), Some(channel(5))))),
            (Some(20), None)
        );
        assert_eq!(
            reply(Some(header(Some(7), Some(channel(9))))),
            (
                None,
                Some(MessageAt {
                    chat: ChatId::Channel(9),
                    id: 7
                })
            )
        );
        // A story reply is no message reply.
        let story = tl::enums::MessageReplyHeader::MessageReplyStoryHeader(
            tl::types::MessageReplyStoryHeader {
                peer: channel(9),
                story_id: 3,
            },
        );
        assert_eq!(reply(Some(story)), (None, None));
        // messageEmpty (deleted, or not there) is no message.
        let empty = tl::enums::Message::Empty(tl::types::MessageEmpty {
            id: 30,
            peer_id: Some(channel(5)),
        });
        assert!(raw_found_message(empty).is_none());
    }

    #[test]
    fn a_forward_keeps_its_original_author_apart_from_its_sender() {
        let user = |user_id| tl::enums::Peer::User(tl::types::PeerUser { user_id });
        // Saved Messages of Account 42: Telegram names the Account as sender
        // and leaves `out` unset.
        let saved = |author: Option<tl::enums::Peer>, from_name: Option<&str>| {
            let tl::enums::Message::Message(mut m) = tl_message(30, user(42), None) else {
                unreachable!()
            };
            m.from_id = Some(user(42));
            m.fwd_from = Some(tl::enums::MessageFwdHeader::Header(
                tl::types::MessageFwdHeader {
                    imported: false,
                    saved_out: false,
                    from_id: author,
                    from_name: from_name.map(Into::into),
                    date: 90,
                    channel_post: None,
                    post_author: None,
                    saved_from_peer: None,
                    saved_from_msg_id: None,
                    saved_from_id: None,
                    saved_from_name: None,
                    saved_date: None,
                    psa_type: None,
                },
            ));
            raw_history_message(tl::enums::Message::Message(m)).unwrap()
        };
        let alices = saved(Some(user(7)), None);
        assert_eq!(
            (
                alices.from,
                alices.out,
                alices.forwarded,
                alices.forwarded_from
            ),
            (Some(ChatId::User(42)), false, true, Some(ChatId::User(7)))
        );
        // An author hiding their account is named only by name: no id.
        let hidden = saved(None, Some("Hidden"));
        assert_eq!((hidden.forwarded, hidden.forwarded_from), (true, None));
        let plain = raw_history_message(tl_message(30, user(42), None)).unwrap();
        assert_eq!((plain.forwarded, plain.forwarded_from), (false, None));
    }

    #[test]
    fn a_rich_message_is_read_as_its_flattened_text() {
        use crate::tg::fake::rich_listing;
        let channel = tl::enums::Peer::Channel(tl::types::PeerChannel { channel_id: 5 });
        let rich = |message: &str| {
            let tl::enums::Message::Message(mut m) = tl_message(30, channel.clone(), None) else {
                unreachable!()
            };
            m.message = message.into();
            m.rich_message = Some(rich_listing());
            tl::enums::Message::Message(m)
        };
        let flat = crate::tg::rich::flatten(&rich_listing());
        assert!(flat.contains("Комнаты | 4"));
        let history = raw_history_message(rich("")).unwrap();
        assert_eq!(history.text, flat);
        assert_eq!(raw_message(rich("")).unwrap().text, flat);
        let found = raw_found_message(rich("Подпись")).unwrap();
        assert_eq!(found.message.text, format!("Подпись\n\n{flat}"));
    }

    #[test]
    fn a_messages_file_comes_with_the_media_get_messages_describes() {
        use crate::tg::media::fixtures::{
            audio, doc, doc_media, full_photo, geo, photo_media, video,
        };
        let channel = tl::enums::Peer::Channel(tl::types::PeerChannel { channel_id: 5 });
        let with = |media: Option<tl::enums::MessageMedia>| {
            let tl::enums::Message::Message(mut m) = tl_message(30, channel.clone(), None) else {
                unreachable!()
            };
            m.media = media;
            tl::enums::Message::Message(m)
        };
        let medias = [
            Some(photo_media(Some(full_photo()), None)),
            Some(doc_media(Some(doc(
                "audio/ogg",
                23_211,
                vec![audio(true, 9)],
            )))),
            Some(doc_media(Some(doc(
                "video/mp4",
                900_000,
                vec![video(true, 12.0, 384, 384)],
            )))),
            Some(photo_media(None, Some(10))),
            Some(tl::enums::MessageMedia::Geo(tl::types::MessageMediaGeo {
                geo: geo(),
            })),
            None,
        ];
        for media in medias {
            let message = with(media.clone());
            let history = raw_history_message(message.clone()).unwrap();
            let found = raw_media_message(message).unwrap();
            assert_eq!(
                found.at,
                MessageAt {
                    chat: ChatId::Channel(5),
                    id: 30
                }
            );
            // The very description get_messages gives, kind included.
            assert_eq!(found.media, history.media, "{media:?}");
            assert_eq!(found.file, media::file(media.as_ref()));
        }
    }

    #[test]
    fn a_view_message_has_its_album_media_and_every_size_of_its_picture() {
        use crate::tg::{
            media::fixtures::{doc, doc_media, full_photo, photo_media, video},
            view::{PictureSize, SizeForm},
        };
        let user = tl::enums::Peer::User(tl::types::PeerUser { user_id: 7 });
        let with = |grouped_id, media: Option<tl::enums::MessageMedia>| {
            let tl::enums::Message::Message(mut m) = tl_message(31, user.clone(), None) else {
                unreachable!()
            };
            m.grouped_id = grouped_id;
            m.media = media;
            tl::enums::Message::Message(m)
        };
        let photo = Some(photo_media(Some(full_photo()), None));
        let found = raw_view_message(with(Some(-5_000_000_000_123), photo.clone())).unwrap();
        assert_eq!(
            found.at,
            MessageAt {
                chat: ChatId::User(7),
                id: 31
            }
        );
        assert_eq!(found.grouped_id, Some(-5_000_000_000_123));
        assert_eq!(found.media, media::describe(photo.as_ref()));
        let picture = found.picture.unwrap();
        assert_eq!(picture, media::picture(photo.as_ref()).unwrap());
        // The stripped size is there, marked as such; `y` is progressive.
        let forms: Vec<(&str, SizeForm)> = picture
            .sizes
            .iter()
            .map(|s| (s.r#type.as_str(), s.form))
            .collect();
        assert_eq!(
            forms,
            [
                ("i", SizeForm::Stripped),
                ("m", SizeForm::File),
                ("x", SizeForm::File),
                ("y", SizeForm::Progressive)
            ]
        );

        // A video note with its thumbnail, in no album.
        let mut round = doc("video/mp4", 900_000, vec![video(true, 9.0, 384, 384)]);
        round.thumbs = Some(vec![tl::enums::PhotoSize::Size(tl::types::PhotoSize {
            r#type: "m".into(),
            w: 320,
            h: 320,
            size: 12_000,
        })]);
        let found = raw_view_message(with(None, Some(doc_media(Some(round))))).unwrap();
        assert_eq!(found.grouped_id, None);
        assert_eq!(
            found.picture.unwrap().sizes,
            [PictureSize {
                r#type: "m".into(),
                form: SizeForm::File,
                w: 320,
                h: 320,
                bytes: 12_000,
            }]
        );
        let plain = raw_view_message(with(None, None)).unwrap();
        assert_eq!((plain.media, plain.picture), (None, None));
    }

    #[test]
    fn reactions_and_the_pin_are_read_from_the_message_itself() {
        let channel = tl::enums::Peer::Channel(tl::types::PeerChannel { channel_id: 5 });
        let count = |reaction, count| {
            tl::enums::ReactionCount::Count(tl::types::ReactionCount {
                chosen_order: Some(1),
                reaction,
                count,
            })
        };
        let emoji =
            |e: &str| tl::enums::Reaction::Emoji(tl::types::ReactionEmoji { emoticon: e.into() });
        let custom = |document_id| {
            tl::enums::Reaction::CustomEmoji(tl::types::ReactionCustomEmoji { document_id })
        };
        let reactions = tl::enums::MessageReactions::Reactions(tl::types::MessageReactions {
            min: false,
            can_see_list: true,
            reactions_as_tags: false,
            results: vec![
                count(emoji("👍"), 5),
                count(custom(77), 2),
                count(tl::enums::Reaction::Paid, 40),
                count(custom(78), 1),
                count(tl::enums::Reaction::Empty, 9),
            ],
            recent_reactions: Some(vec![]),
            top_reactors: Some(vec![]),
        });
        let tl::enums::Message::Message(mut m) = tl_message(30, channel.clone(), None) else {
            unreachable!()
        };
        m.reactions = Some(reactions);
        m.pinned = true;
        let raw = raw_history_message(tl::enums::Message::Message(m)).unwrap();
        assert!(raw.pinned);
        assert_eq!(
            raw.reactions,
            [
                RawReaction {
                    reaction: Reaction::Emoji("👍".into()),
                    count: 5
                },
                RawReaction {
                    reaction: Reaction::Custom,
                    count: 2
                },
                RawReaction {
                    reaction: Reaction::Paid,
                    count: 40
                },
                RawReaction {
                    reaction: Reaction::Custom,
                    count: 1
                },
            ]
        );
        let plain = raw_history_message(tl_message(31, channel, None)).unwrap();
        assert_eq!((plain.reactions, plain.pinned), (vec![], false));
    }

    #[test]
    fn media_is_described_beside_the_text_never_in_it() {
        use crate::tg::{
            media::fixtures::{audio, doc, doc_media, full_photo, photo_media},
            messages::MediaKind,
        };
        let channel = tl::enums::Peer::Channel(tl::types::PeerChannel { channel_id: 5 });
        let with = |message: &str, media: Option<tl::enums::MessageMedia>| {
            let tl::enums::Message::Message(mut m) = tl_message(30, channel.clone(), None) else {
                unreachable!()
            };
            m.message = message.into();
            m.media = media;
            tl::enums::Message::Message(m)
        };
        let voice = || doc_media(Some(doc("audio/ogg", 23_211, vec![audio(true, 7)])));
        let link_preview = || {
            tl::enums::MessageMedia::WebPage(tl::types::MessageMediaWebPage {
                force_large_media: false,
                force_small_media: false,
                manual: false,
                safe: false,
                webpage: tl::enums::WebPage::Pending(tl::types::WebPagePending {
                    id: 1,
                    url: Some("https://example.com".into()),
                    date: 1,
                }),
            })
        };
        let cases = [
            (
                with("Кухня", Some(photo_media(Some(full_photo()), None))),
                "Кухня",
                Some(MediaKind::Photo),
            ),
            (with("", Some(voice())), "", Some(MediaKind::Voice)),
            (
                with("see https://example.com", Some(link_preview())),
                "see https://example.com",
                None,
            ),
            (
                with("", Some(tl::enums::MessageMedia::Unsupported)),
                "",
                Some(MediaKind::Other),
            ),
            (with("plain", None), "plain", None),
        ];
        for (message, text, kind) in cases {
            let history = raw_history_message(message.clone()).unwrap();
            assert_eq!(
                (
                    history.text.as_str(),
                    history.media.as_ref().map(|m| m.kind)
                ),
                (text, kind)
            );
            let preview = raw_message(message).unwrap();
            assert_eq!((preview.text.as_str(), preview.media), (text, kind));
        }
        let history = raw_history_message(with("", Some(voice()))).unwrap();
        let voice = history.media.unwrap();
        assert_eq!(
            (
                voice.duration_s,
                voice.size_bytes,
                voice.mime_type.as_deref()
            ),
            (Some(7), Some(23_211), Some("audio/ogg"))
        );
    }

    #[test]
    fn a_refused_file_part_says_what_to_do() {
        type GetFile = tl::functions::upload::GetFile;
        let refusal = |code, message, dc_id| file_refusal(&rpc::<GetFile>(code, message), dc_id, 2);
        assert_eq!(
            refusal(303, "FILE_MIGRATE_4", 2),
            FileRefusal::Migrate(Some(4))
        );
        for expired in ["FILE_REFERENCE_EXPIRED", "FILE_REFERENCE_INVALID"] {
            assert_eq!(refusal(400, expired, 4), FileRefusal::ReferenceExpired);
        }
        // Not logged in to the file's DC yet; in the home DC it's a dead session.
        assert_eq!(
            refusal(401, "AUTH_KEY_UNREGISTERED", 4),
            FileRefusal::NotAuthorizedInDc
        );
        assert_eq!(refusal(401, "AUTH_KEY_UNREGISTERED", 2), FileRefusal::Other);
        for other in [
            (400, "FILE_ID_INVALID"),
            (400, "LIMIT_INVALID"),
            (420, "FLOOD_WAIT_30"),
        ] {
            assert_eq!(
                refusal(other.0, other.1, 4),
                FileRefusal::Other,
                "{other:?}"
            );
        }
    }

    #[test]
    fn qr_url_is_tg_login_with_base64url_token() {
        assert_eq!(qr_url(&[0xfb, 0xff, 0x01]), "tg://login?token=-_8B");
    }

    #[test]
    fn login_token_update_is_detected_in_every_container() {
        let short = tl::enums::Updates::UpdateShort(tl::types::UpdateShort {
            update: tl::enums::Update::LoginToken,
            date: 0,
        });
        assert!(has_login_token_update(&UpdatesLike::Updates(short)));
        let full = tl::enums::Updates::Updates(tl::types::Updates {
            updates: vec![tl::enums::Update::LoginToken],
            users: vec![],
            chats: vec![],
            date: 0,
            seq: 0,
        });
        assert!(has_login_token_update(&UpdatesLike::Updates(full)));
        assert!(!has_login_token_update(&UpdatesLike::Updates(
            tl::enums::Updates::TooLong
        )));
    }

    fn transcribed(text: &str, pending: bool, trial: Option<(i32, i32)>) -> tl::enums::Update {
        tl::enums::Update::TranscribedAudio(tl::types::UpdateTranscribedAudio {
            pending,
            peer: tl::enums::Peer::User(tl::types::PeerUser { user_id: 7 }),
            msg_id: 525_381,
            transcription_id: 4_242 + i64::from(trial.is_some()),
            text: text.into(),
        })
    }

    #[test]
    fn a_transcribe_audio_answer_keeps_its_text_id_and_trial() {
        let answer = |pending, trial: Option<(i32, i32)>| {
            tl::enums::messages::TranscribedAudio::Audio(tl::types::messages::TranscribedAudio {
                pending,
                transcription_id: 4_242,
                text: "Это голосовое".into(),
                trial_remains_num: trial.map(|t| t.0),
                trial_remains_until_date: trial.map(|t| t.1),
            })
        };
        assert_eq!(
            transcribed_audio(answer(true, None)),
            TranscribedAudio {
                pending: true,
                transcription_id: 4_242,
                text: "Это голосовое".into(),
                trial: None,
            }
        );
        assert_eq!(
            transcribed_audio(answer(false, Some((2, 1_794_000_000)))).trial,
            Some(Trial {
                remaining: 2,
                resets_at: 1_794_000_000
            })
        );
        assert_eq!(
            transcribed_audio(answer(false, Some((-1, 1)))).trial,
            Some(Trial {
                remaining: 0,
                resets_at: 1
            })
        );
    }

    #[test]
    fn transcription_updates_are_found_in_every_container_and_nothing_else_is() {
        let final_one = TranscriptionUpdate {
            transcription_id: 4_242,
            pending: false,
            text: "Всем пока".into(),
        };
        let short = tl::enums::Updates::UpdateShort(tl::types::UpdateShort {
            update: transcribed("Всем пока", false, None),
            date: 0,
        });
        assert_eq!(
            transcription_updates(UpdatesLike::Updates(short)),
            std::slice::from_ref(&final_one)
        );
        let full = tl::enums::Updates::Updates(tl::types::Updates {
            updates: vec![
                tl::enums::Update::LoginToken,
                transcribed("Всем", true, None),
                transcribed("Всем пока", false, None),
            ],
            users: vec![],
            chats: vec![],
            date: 0,
            seq: 0,
        });
        let both = transcription_updates(UpdatesLike::Updates(full));
        assert_eq!(both.len(), 2);
        assert!(both[0].pending && !both[1].pending);
        let combined = tl::enums::Updates::Combined(tl::types::UpdatesCombined {
            updates: vec![transcribed("Всем пока", false, None)],
            users: vec![],
            chats: vec![],
            date: 0,
            seq_start: 0,
            seq: 0,
        });
        assert_eq!(
            transcription_updates(UpdatesLike::Updates(combined)),
            [final_one]
        );
        for nothing in [
            UpdatesLike::Updates(tl::enums::Updates::TooLong),
            UpdatesLike::ConnectionClosed,
        ] {
            assert!(transcription_updates(nothing).is_empty());
        }
    }

    #[tokio::test]
    async fn pushed_transcriptions_reach_the_feed_until_the_session_ends() {
        let (tx, rx) = mpsc::channel(8);
        let feed = transcribe::feed();
        let mut listening = feed.subscribe();
        let forwarding = tokio::spawn(forward_transcriptions(rx, feed));
        let short = |update| {
            UpdatesLike::Updates(tl::enums::Updates::UpdateShort(tl::types::UpdateShort {
                update,
                date: 0,
            }))
        };
        tx.send(short(tl::enums::Update::LoginToken)).await.unwrap();
        tx.send(short(transcribed("Всем пока", false, None)))
            .await
            .unwrap();
        let got = tokio::time::timeout(Duration::from_secs(5), listening.recv())
            .await
            .expect("the update was passed on")
            .unwrap();
        assert_eq!((got.transcription_id, got.pending), (4_242, false));
        assert_eq!(got.text, "Всем пока");
        drop(tx);
        forwarding.await.unwrap();
        assert!(matches!(
            listening.recv().await,
            Err(tokio::sync::broadcast::error::RecvError::Closed)
        ));
    }

    #[test]
    fn telegrams_transcription_refusals_are_told_apart_from_floods() {
        type TranscribeAudio = tl::functions::messages::TranscribeAudio;
        for (code, message, refusal) in [
            (403, "PREMIUM_ACCOUNT_REQUIRED", Refusal::PremiumRequired),
            (400, "MSG_VOICE_MISSING", Refusal::VoiceMissing),
            (400, "MSG_VOICE_TOO_LONG", Refusal::TooLong),
            (400, "TRANSCRIPTION_FAILED", Refusal::Failed),
        ] {
            assert_eq!(
                transcribe_refusal(&rpc::<TranscribeAudio>(code, message)),
                Some(refusal),
                "{message}"
            );
        }
        // A used-up trial: a flood of class Transcribe for the limiter.
        let flood = rpc::<TranscribeAudio>(420, "FLOOD_WAIT_259200");
        assert_eq!(transcribe_refusal(&flood), None);
        assert!(matches!(
            flood_error(flood, method_class::<TranscribeAudio>()),
            TgError::FloodWait {
                seconds: 259_200,
                class: MethodClass::Transcribe
            }
        ));
        for other in [(400, "MSG_ID_INVALID"), (400, "PEER_ID_INVALID")] {
            assert_eq!(
                transcribe_refusal(&rpc::<TranscribeAudio>(other.0, other.1)),
                None
            );
        }
    }

    #[test]
    fn flood_wait_is_surfaced_with_its_seconds() {
        assert!(matches!(
            login_error(rpc::<tl::functions::auth::ExportLoginToken>(
                420,
                "FLOOD_WAIT_31"
            )),
            TgError::FloodWait {
                seconds: 31,
                class: MethodClass::Default
            }
        ));
        assert!(matches!(
            login_error(rpc::<tl::functions::auth::CheckPassword>(
                400,
                "PASSWORD_HASH_INVALID"
            )),
            TgError::Upstream(_)
        ));
    }

    #[test]
    fn account_floods_are_flood_waits_of_the_requests_class() {
        type GetFile = tl::functions::upload::GetFile;
        let floods = [
            ("FLOOD_WAIT_31", MethodClass::Search, 31),
            ("FLOOD_WAIT_7", MethodClass::Default, 7),
            ("FLOOD_PREMIUM_WAIT_5", MethodClass::Files, 5),
        ];
        for (message, class, seconds) in floods {
            match flood_error(rpc::<GetFile>(420, message), class) {
                TgError::FloodWait {
                    seconds: s,
                    class: c,
                } => assert_eq!((s, c), (seconds, class), "{message}"),
                other => panic!("{message} must be a flood, got {other:?}"),
            }
        }
        // Slow mode limits one Chat, not the Account: never a class-wide window.
        for not_a_flood in [(420, "SLOWMODE_WAIT_10"), (400, "FILE_REFERENCE_EXPIRED")] {
            assert!(matches!(
                flood_error(
                    rpc::<GetFile>(not_a_flood.0, not_a_flood.1),
                    MethodClass::Files
                ),
                TgError::Upstream(_)
            ));
        }
    }

    #[test]
    fn each_request_type_falls_in_its_flood_wait_class() {
        use tl::functions::{channels, contacts, messages, upload, users};
        let search = [
            method_class::<messages::SearchGlobal>(),
            method_class::<messages::Search>(),
            method_class::<contacts::Search>(),
        ];
        assert_eq!(search, [MethodClass::Search; 3]);
        let files = [
            method_class::<upload::GetFile>(),
            method_class::<upload::GetCdnFile>(),
            method_class::<upload::GetFileHashes>(),
            method_class::<upload::GetCdnFileHashes>(),
            method_class::<upload::GetWebFile>(),
            method_class::<upload::ReuploadCdnFile>(),
        ];
        assert_eq!(files, [MethodClass::Files; 6]);
        // What the reading tools send today.
        let default = [
            method_class::<users::GetUsers>(),
            method_class::<messages::GetDialogs>(),
            method_class::<messages::GetHistory>(),
            method_class::<users::GetFullUser>(),
            method_class::<messages::GetFullChat>(),
            method_class::<channels::GetFullChannel>(),
            method_class::<messages::GetMessages>(),
            method_class::<channels::GetMessages>(),
        ];
        assert_eq!(default, [MethodClass::Default; 8]);
        assert_eq!(
            method_class::<messages::TranscribeAudio>(),
            MethodClass::Transcribe
        );
    }

    #[test]
    fn credentials_debug_hides_api_hash() {
        let creds = AppCredentials {
            api_id: 1,
            api_hash: "0123456789abcdef".into(),
        };
        assert!(!format!("{creds:?}").contains("0123456789abcdef"));
    }
}

/// Talks to real Telegram over raw TCP (MTProto); run with `cargo test -- --ignored live_`
/// from a machine that can reach Telegram's DCs. Needs no credentials: auth-key
/// generation happens before api_id is checked.
#[cfg(test)]
mod live_tests {
    use grammers_session::Session;

    use super::*;

    #[tokio::test]
    #[ignore = "needs network access to Telegram"]
    async fn live_handshake_stores_an_auth_key_in_our_session() {
        let creds = AppCredentials {
            api_id: 0,
            api_hash: "invalid".into(),
        };
        let conn = connect(&creds, SessionStore::new(), Purpose::Login);
        let home = conn.store.home_dc_id().unwrap();
        assert!(
            conn.store
                .dc_option(home)
                .unwrap()
                .unwrap()
                .auth_key
                .is_none()
        );

        let _ = tracing_subscriber::fmt()
            .with_env_filter("grammers=debug")
            .with_test_writer()
            .try_init();
        let err = tokio::time::timeout(
            Duration::from_secs(30),
            conn.client.invoke(&tl::functions::help::GetNearestDc {}),
        )
        .await
        .expect("Telegram answers within 30 s")
        .expect_err("api_id 0 must be rejected");
        assert!(err.is("API_ID_INVALID"), "unexpected error: {err}");

        // The MTProto handshake ran through SessionStore and its key is in the snapshot.
        let restored = SessionStore::from_snapshot(&conn.store.snapshot().unwrap()).unwrap();
        assert!(
            restored
                .dc_option(home)
                .unwrap()
                .unwrap()
                .auth_key
                .is_some()
        );
        conn.client.disconnect();
    }
}

/// A Live client for `grant` whose connection is never started: the runner is
/// dropped, so every request fails right away and nothing leaves the machine.
#[cfg(test)]
fn offline_live(grant: OpenedGrant, store: Arc<SessionStore>) -> LiveClient {
    let SenderPool { handle, .. } = SenderPool::new(store.clone(), 0);
    LiveClient {
        client: Client::new(handle),
        store,
        grant,
        peers: Default::default(),
        forums: Default::default(),
        dead: Default::default(),
        limiter: Arc::new(AccountLimiter::new(crate::limits::LimitsConfig::default())),
        transcriptions: transcribe::feed(),
        sent: Default::default(),
        answers: Default::default(),
    }
}

#[cfg(test)]
mod shutdown_tests {
    use grammers_session::Session;

    use super::*;
    use crate::{
        limits::LimitsConfig,
        sessions::{TokenTtl, Vault},
        tg::{SHUTDOWN_TIMEOUT, shut_down},
    };

    #[tokio::test]
    async fn shutdown_reseals_a_session_that_changed_while_live() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Arc::new(
            Vault::open_path(&dir.path().join("vault.db"), TokenTtl::default())
                .await
                .unwrap(),
        );
        let sealed = SessionStore::new().snapshot().unwrap();
        let issued = vault.create_grant("client", 42, &sealed).await.unwrap();
        let connector = GrammersConnector::new(
            AppCredentials {
                api_id: 0,
                api_hash: "unused".into(),
            },
            vault.clone(),
            Arc::new(Limits::new(LimitsConfig::default())),
            Duration::from_secs(3600),
        );

        // A Live client, just used (an idle sweep would keep it)...
        let grant = vault.open(issued.access.expose()).await.unwrap();
        let key = grant.grant_id.as_str().to_string();
        let store = Arc::new(SessionStore::from_snapshot(&grant.telegram_session).unwrap());
        let live = offline_live(grant, store.clone());
        connector
            .pool
            .get_or_open(&key, &sealed, || async { Ok(live) })
            .await
            .unwrap();
        // ...whose session moved to another DC while live.
        let home = store.home_dc_id().unwrap();
        store.set_home_dc_id(home % 5 + 1).await.unwrap();
        let moved = store.snapshot().unwrap();
        assert_ne!(moved.as_slice(), sealed.as_slice());

        assert!(shut_down(connector.as_ref(), SHUTDOWN_TIMEOUT).await);
        assert_eq!(connector.pool.len(), 0, "no Live client survives shutdown");
        let reopened = vault.open(issued.access.expose()).await.unwrap();
        assert_eq!(reopened.telegram_session.as_slice(), moved.as_slice());
    }
}

/// What `list_chats` sends for each listing and page, and what `get_messages` /
/// `get_chat_info` send for each kind of id, on a fresh Live client (empty peer cache) whose connection is never started:
/// every request is recorded, then fails at once, so nothing leaves the machine.
#[cfg(test)]
mod resolve_tests {
    use grammers_client::tl::{Identifiable, Serializable};

    use super::*;
    use crate::{
        sessions::{TokenTtl, Vault},
        tg::search::SearchScope,
    };

    const ACCOUNT: i64 = 42;

    async fn cold_client() -> (LiveClient, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open_path(&dir.path().join("vault.db"), TokenTtl::default())
            .await
            .unwrap();
        let issued = vault.create_grant("client", ACCOUNT, b"").await.unwrap();
        let grant = vault.open(issued.access.expose()).await.unwrap();
        (offline_live(grant, Arc::new(SessionStore::new())), dir)
    }

    fn sent(live: &LiveClient) -> Vec<Vec<u8>> {
        live.sent.lock().unwrap().clone()
    }

    fn constructor(request: &[u8]) -> u32 {
        u32::from_le_bytes(request[..4].try_into().unwrap())
    }

    fn query() -> HistoryQuery {
        HistoryQuery {
            limit: 30,
            ..HistoryQuery::default()
        }
    }

    fn get_history(peer: tl::enums::InputPeer) -> Vec<u8> {
        tl::functions::messages::GetHistory {
            peer,
            offset_id: 0,
            offset_date: 0,
            add_offset: 0,
            limit: 30,
            max_id: 0,
            min_id: 0,
            hash: 0,
        }
        .to_bytes()
    }

    #[tokio::test]
    async fn issued_ids_go_straight_to_telegram_with_their_hash() {
        let (live, _dir) = cold_client().await;
        let channel = ChatRef::new(ChatId::Channel(5), Some(-1_234_567_890_123));
        let user = ChatRef::new(ChatId::User(7), Some(i64::MAX));

        assert!(live.get_messages(channel, query()).await.is_err());
        assert!(live.get_chat_info(channel).await.is_err());
        assert!(live.get_messages(user, query()).await.is_err());
        assert!(live.get_chat_info(user).await.is_err());

        // One request per call, the real one: no messages.getDialogs scan.
        assert_eq!(
            sent(&live),
            [
                get_history(tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
                    channel_id: 5,
                    access_hash: -1_234_567_890_123,
                })),
                tl::functions::channels::GetFullChannel {
                    channel: tl::enums::InputChannel::Channel(tl::types::InputChannel {
                        channel_id: 5,
                        access_hash: -1_234_567_890_123,
                    }),
                }
                .to_bytes(),
                get_history(tl::enums::InputPeer::User(tl::types::InputPeerUser {
                    user_id: 7,
                    access_hash: i64::MAX,
                })),
                tl::functions::users::GetFullUser {
                    id: tl::enums::InputUser::User(tl::types::InputUser {
                        user_id: 7,
                        access_hash: i64::MAX,
                    }),
                }
                .to_bytes(),
            ]
        );
    }

    #[tokio::test]
    async fn pinned_messages_are_telegrams_search_with_the_pinned_filter() {
        let (live, _dir) = cold_client().await;
        let chat = ChatRef::new(ChatId::Channel(5), Some(7));
        let pinned = |before_id| HistoryQuery {
            pinned_only: true,
            before_id,
            ..query()
        };
        assert!(live.get_messages(chat, pinned(None)).await.is_err());
        assert!(live.get_messages(chat, pinned(Some(40))).await.is_err());
        let search = |offset_id| {
            tl::functions::messages::Search {
                peer: tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
                    channel_id: 5,
                    access_hash: 7,
                }),
                q: String::new(),
                from_id: None,
                saved_peer_id: None,
                saved_reaction: None,
                top_msg_id: None,
                filter: tl::enums::MessagesFilter::InputMessagesFilterPinned,
                min_date: 0,
                max_date: 0,
                offset_id,
                add_offset: 0,
                limit: 30,
                max_id: 0,
                min_id: 0,
                hash: 0,
            }
            .to_bytes()
        };
        assert_eq!(sent(&live), [search(0), search(40)]);
        // Under the Account's limiter in the Search class, like any search.
        assert_eq!(
            method_class::<tl::functions::messages::Search>(),
            MethodClass::Search
        );
    }

    #[tokio::test]
    async fn windows_and_newer_pages_send_negative_add_offsets() {
        let (live, _dir) = cold_client().await;
        let chat = ChatRef::from(ChatId::Chat(4));
        let peer = tl::enums::InputPeer::Chat(tl::types::InputPeerChat { chat_id: 4 });
        let around = HistoryQuery {
            around_id: Some(500),
            ..query()
        };
        let after = HistoryQuery {
            after_id: Some(500),
            ..query()
        };
        assert!(live.get_messages(chat, around).await.is_err());
        assert!(live.get_messages(chat, after).await.is_err());
        let request = |offset_id, add_offset, min_id| {
            tl::functions::messages::GetHistory {
                peer: peer.clone(),
                offset_id,
                offset_date: 0,
                add_offset,
                limit: 30,
                max_id: 0,
                min_id,
                hash: 0,
            }
            .to_bytes()
        };
        assert_eq!(
            sent(&live),
            [
                // 500 and the 14 above it, then 15 below.
                request(500, -15, 0),
                // The 30 at and above 501, nothing at or below 500.
                request(501, -30, 500),
            ]
        );
    }

    #[tokio::test]
    async fn a_chain_asks_channels_get_messages_in_a_channel_and_messages_get_messages_elsewhere() {
        let (live, _dir) = cold_client().await;
        let chain = HistoryQuery {
            thread_of: Some(500),
            limit: 11,
            ..HistoryQuery::default()
        };
        let id = || {
            vec![tl::enums::InputMessage::Id(tl::types::InputMessageId {
                id: 500,
            })]
        };
        let channel = ChatRef::new(ChatId::Channel(5), Some(-77));
        let user = ChatRef::new(ChatId::User(7), Some(88));
        for chat in [channel, user, ChatId::Chat(4).into()] {
            // Offline: the first call fails, and the walk with it.
            assert!(live.get_messages(chat, chain).await.is_err());
        }
        assert_eq!(
            sent(&live),
            [
                tl::functions::channels::GetMessages {
                    channel: tl::enums::InputChannel::Channel(tl::types::InputChannel {
                        channel_id: 5,
                        access_hash: -77,
                    }),
                    id: id(),
                }
                .to_bytes(),
                tl::functions::messages::GetMessages { id: id() }.to_bytes(),
                tl::functions::messages::GetMessages { id: id() }.to_bytes(),
            ]
        );
    }

    #[tokio::test]
    async fn common_chats_ask_telegram_for_the_user_with_its_hash() {
        let (live, _dir) = cold_client().await;
        let user = ChatRef::new(ChatId::User(7), Some(-88));
        assert!(live.common_chats(user, 20).await.is_err());
        // Not a user: refused before anything is sent.
        let channel = ChatRef::new(ChatId::Channel(5), Some(1));
        assert!(matches!(
            live.common_chats(channel, 20).await,
            Err(TgError::InvalidInput(_))
        ));
        assert!(matches!(
            live.common_chats(ChatId::Chat(4).into(), 20).await,
            Err(TgError::InvalidInput(_))
        ));
        assert_eq!(
            sent(&live),
            [tl::functions::messages::GetCommonChats {
                user_id: tl::enums::InputUser::User(tl::types::InputUser {
                    user_id: 7,
                    access_hash: -88,
                }),
                max_id: 0,
                limit: 20,
            }
            .to_bytes()]
        );
    }

    fn members_query(chat: ChatRef, admins_only: bool) -> MembersQuery {
        MembersQuery {
            chat,
            query: None,
            admins_only,
            limit: 50,
            cursor: None,
        }
    }

    #[tokio::test]
    async fn members_are_asked_for_with_the_channels_hash_or_the_groups_id() {
        let (live, _dir) = cold_client().await;
        let channel = ChatRef::new(ChatId::Channel(5), Some(-77));
        let input = || {
            tl::enums::InputChannel::Channel(tl::types::InputChannel {
                channel_id: 5,
                access_hash: -77,
            })
        };
        // The first page asks whether members are hidden before listing them.
        assert!(
            live.get_members(members_query(channel, false))
                .await
                .is_err()
        );
        // Admins are listed whole, in one call.
        assert!(
            live.get_members(members_query(channel, true))
                .await
                .is_err()
        );
        assert!(
            live.get_members(members_query(ChatId::Chat(4).into(), false))
                .await
                .is_err()
        );
        // A user: refused before anything is sent.
        assert!(matches!(
            live.get_members(members_query(ChatRef::new(ChatId::User(7), Some(1)), false))
                .await,
            Err(TgError::InvalidInput(_))
        ));
        assert_eq!(
            sent(&live),
            [
                tl::functions::channels::GetFullChannel { channel: input() }.to_bytes(),
                tl::functions::channels::GetParticipants {
                    channel: input(),
                    filter: tl::enums::ChannelParticipantsFilter::ChannelParticipantsAdmins,
                    offset: 0,
                    limit: 200,
                    hash: 0,
                }
                .to_bytes(),
                tl::functions::messages::GetFullChat { chat_id: 4 }.to_bytes(),
            ]
        );
    }

    #[tokio::test]
    async fn a_member_search_is_telegrams_with_its_offset() {
        let (live, _dir) = cold_client().await;
        let channel = ChatRef::new(ChatId::Channel(5), Some(-77));
        let page = live
            .participants(ParticipantsRequest {
                channel,
                filter: ParticipantsFilter::Search("ann".into()),
                offset: 400,
                limit: 200,
            })
            .await;
        assert!(page.is_err());
        assert_eq!(
            sent(&live),
            [tl::functions::channels::GetParticipants {
                channel: tl::enums::InputChannel::Channel(tl::types::InputChannel {
                    channel_id: 5,
                    access_hash: -77,
                }),
                filter: tl::enums::ChannelParticipantsFilter::ChannelParticipantsSearch(
                    tl::types::ChannelParticipantsSearch { q: "ann".into() }
                ),
                offset: 400,
                limit: 200,
                hash: 0,
            }
            .to_bytes()]
        );
    }

    #[tokio::test]
    async fn basic_groups_need_no_lookup() {
        let (live, _dir) = cold_client().await;
        assert!(
            live.get_messages(ChatId::Chat(4).into(), query())
                .await
                .is_err()
        );
        assert_eq!(
            sent(&live),
            [get_history(tl::enums::InputPeer::Chat(
                tl::types::InputPeerChat { chat_id: 4 }
            ))]
        );
    }

    #[tokio::test]
    async fn the_peers_an_answer_describes_are_remembered_never_a_min_hash() {
        let (live, _dir) = cold_client().await;
        let (min_user, min_channel) = super::tests::tl_peers(true);
        live.described(vec![min_user], vec![min_channel]);
        assert_eq!(live.cached(ChatId::User(7)), None);
        assert_eq!(live.cached(ChatId::Channel(7)), None);

        let (user, channel) = super::tests::tl_peers(false);
        let peers = live.described(vec![user], vec![channel]);
        assert_eq!(peers.len(), 2);
        assert_eq!(live.cached(ChatId::User(7)), Some(99));
        assert_eq!(live.cached(ChatId::Channel(7)), Some(99));
    }

    #[tokio::test]
    async fn hashless_ids_use_the_peer_cache_then_the_dialog_list() {
        let (live, _dir) = cold_client().await;
        let legacy = ChatRef::from(ChatId::Channel(5));

        // Cold cache: the old path scans the dialog list first.
        assert!(live.get_messages(legacy, query()).await.is_err());
        assert!(live.get_chat_info(ChatId::User(7).into()).await.is_err());
        let first: Vec<u32> = sent(&live).iter().map(|r| constructor(r)).collect();
        assert_eq!(
            first,
            [
                tl::functions::messages::GetDialogs::CONSTRUCTOR_ID,
                tl::functions::messages::GetDialogs::CONSTRUCTOR_ID,
            ]
        );

        // Seen before (e.g. listed): the cached hash is used, no scan.
        live.sent.lock().unwrap().clear();
        live.remember(&[RawPeer {
            id: ChatId::Channel(5),
            title: "News".into(),
            kind: ChatKind::Channel,
            username: None,
            access_hash: Some(99),
            is_self: false,
        }]);
        assert!(live.get_messages(legacy, query()).await.is_err());
        assert_eq!(
            sent(&live),
            [get_history(tl::enums::InputPeer::Channel(
                tl::types::InputPeerChannel {
                    channel_id: 5,
                    access_hash: 99,
                }
            ))]
        );
    }

    /// A list_chats query without `unread_only`.
    fn listing(limit: usize, cursor: Option<&str>, archived: bool) -> ChatsQuery {
        ChatsQuery {
            limit,
            cursor: cursor.map(String::from),
            archived,
            unread_only: false,
            chat_folder: None,
        }
    }

    fn get_dialogs(
        exclude_pinned: bool,
        folder_id: i32,
        offset: (i32, i32, tl::enums::InputPeer),
    ) -> Vec<u8> {
        tl::functions::messages::GetDialogs {
            exclude_pinned,
            folder_id: Some(folder_id),
            offset_date: offset.0,
            offset_id: offset.1,
            offset_peer: offset.2,
            limit: 30,
            hash: 0,
        }
        .to_bytes()
    }

    #[tokio::test]
    async fn listings_name_their_folder_and_later_pages_skip_pinned_chats() {
        let (live, _dir) = cold_client().await;
        let top = || (0, 0, tl::enums::InputPeer::Empty);
        let after_channel = Cursor {
            offset_date: 1_700_000_000,
            offset_id: 55,
            offset_peer: Some("channel:5".into()),
            offset_access_hash: Some(-9),
            archived: false,
            unread_only: false,
        };
        let archive_top = Cursor {
            offset_date: 0,
            offset_id: 0,
            offset_peer: None,
            offset_access_hash: None,
            archived: true,
            unread_only: false,
        };

        assert!(live.list_chats(listing(30, None, false)).await.is_err());
        assert!(live.list_chats(listing(30, None, true)).await.is_err());
        // The cursor decides the folder, whatever `archived` says.
        let next = after_channel.encode();
        assert!(
            live.list_chats(listing(30, Some(&next), true))
                .await
                .is_err()
        );
        let next = archive_top.encode();
        assert!(
            live.list_chats(listing(30, Some(&next), false))
                .await
                .is_err()
        );

        let channel = tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
            channel_id: 5,
            access_hash: -9,
        });
        assert_eq!(
            sent(&live),
            [
                get_dialogs(false, 0, top()),
                get_dialogs(false, 1, top()),
                get_dialogs(true, 0, (1_700_000_000, 55, channel)),
                get_dialogs(true, 1, top()),
            ]
        );
    }

    #[tokio::test]
    async fn an_unread_scan_reads_whole_telegram_pages_through_the_limiter() {
        let (live, _dir) = cold_client().await;
        let unread = ChatsQuery {
            limit: 5,
            unread_only: true,
            ..ChatsQuery::default()
        };
        assert!(live.list_chats(unread.clone()).await.is_err());
        assert_eq!(
            sent(&live),
            [tl::functions::messages::GetDialogs {
                exclude_pinned: false,
                folder_id: Some(0),
                offset_date: 0,
                offset_id: 0,
                offset_peer: tl::enums::InputPeer::Empty,
                limit: chats::UNREAD_SCAN_PAGE_SIZE as i32,
                hash: 0,
            }
            .to_bytes()],
            "the offline connection fails the first page: the scan ends there"
        );

        live.sent.lock().unwrap().clear();
        flood_once::<tl::functions::messages::GetDialogs>(&live).await;
        match live.list_chats(unread).await {
            Err(TgError::FloodWait {
                class: MethodClass::Default,
                ..
            }) => {}
            other => panic!("expected the scan to fail fast, got {other:?}"),
        }
        assert!(sent(&live).is_empty(), "held back, never retried around");
    }

    /// Telegram floods `request` once with `FLOOD_WAIT_300`, reported the way the
    /// Live client reports it, which leaves its class blocked for the Account.
    async fn flood_once<R: tl::RemoteCall + tl::Identifiable>(live: &LiveClient) {
        let class = method_class::<R>();
        let flooded = live
            .limiter
            .run(class, || async {
                Err::<(), _>(live.classify(rpc::<R>(420, "FLOOD_WAIT_300"), class))
            })
            .await;
        assert!(matches!(
            flooded,
            Err(TgError::FloodWait { seconds: 300, .. })
        ));
        assert!(sent(live).is_empty(), "the flood itself bypassed `call`");
    }

    fn search() -> tl::functions::contacts::Search {
        tl::functions::contacts::Search {
            broadcasts: false,
            bots: false,
            q: "alice".into(),
            limit: 10,
        }
    }

    fn download() -> tl::functions::upload::GetCdnFile {
        tl::functions::upload::GetCdnFile {
            file_token: vec![1, 2, 3],
            offset: 0,
            limit: 4096,
        }
    }

    fn search_query(scope: SearchScope, text: &str) -> SearchQuery {
        SearchQuery {
            scope,
            text: text.into(),
            media: None,
            since: None,
            until: None,
            limit: 20,
            cursor: None,
        }
    }

    fn everywhere() -> SearchScope {
        SearchScope::Global {
            chat_type: None,
            archived: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn search_global(
        q: &str,
        chat_type: (bool, bool, bool),
        folder_id: Option<i32>,
        filter: tl::enums::MessagesFilter,
        dates: (i32, i32),
        offset: (i32, tl::enums::InputPeer, i32),
    ) -> Vec<u8> {
        tl::functions::messages::SearchGlobal {
            broadcasts_only: chat_type.0,
            groups_only: chat_type.1,
            users_only: chat_type.2,
            folder_id,
            community: None,
            q: q.into(),
            filter,
            min_date: dates.0,
            max_date: dates.1,
            offset_rate: offset.0,
            offset_peer: offset.1,
            offset_id: offset.2,
            limit: 20,
        }
        .to_bytes()
    }

    #[tokio::test]
    async fn a_global_search_sends_its_filters_and_continues_at_the_cursor() {
        let (live, _dir) = cold_client().await;
        use tl::enums::MessagesFilter as F;
        let groups_in_archive = SearchQuery {
            media: Some(MediaFilter::Photo),
            since: Some(1_000),
            until: Some(2_000),
            ..search_query(
                SearchScope::Global {
                    chat_type: Some(ChatType::Groups),
                    archived: Some(true),
                },
                "  deploy ",
            )
        };
        assert!(live.search_messages(groups_in_archive).await.is_err());
        let main_channels = search_query(
            SearchScope::Global {
                chat_type: Some(ChatType::Channels),
                archived: Some(false),
            },
            "deploy",
        );
        assert!(live.search_messages(main_channels).await.is_err());
        // Page 2: Telegram's rate, the last hit's chat with its hash, its id.
        let first = search_query(everywhere(), "deploy");
        let next = SearchQuery {
            cursor: Some(search::cursor_for(
                &first,
                Some(777),
                Some(ChatRef::new(ChatId::Channel(5), Some(-9))),
                31,
            )),
            ..first.clone()
        };
        assert!(live.search_messages(next).await.is_err());
        // A last hit Telegram gave no hash for, and none is cached: Empty.
        let next = SearchQuery {
            cursor: Some(search::cursor_for(
                &first,
                Some(778),
                Some(ChatId::User(6).into()),
                32,
            )),
            ..first
        };
        assert!(live.search_messages(next).await.is_err());

        let channel = tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
            channel_id: 5,
            access_hash: -9,
        });
        let empty = || tl::enums::InputPeer::Empty;
        assert_eq!(
            sent(&live),
            [
                // Inclusive bounds become Telegram's exclusive ones; the query is trimmed.
                search_global(
                    "deploy",
                    (false, true, false),
                    Some(1),
                    F::InputMessagesFilterPhotos,
                    (999, 2_001),
                    (0, empty(), 0)
                ),
                search_global(
                    "deploy",
                    (true, false, false),
                    Some(0),
                    F::InputMessagesFilterEmpty,
                    (0, 0),
                    (0, empty(), 0)
                ),
                search_global(
                    "deploy",
                    (false, false, false),
                    None,
                    F::InputMessagesFilterEmpty,
                    (0, 0),
                    (777, channel, 31)
                ),
                search_global(
                    "deploy",
                    (false, false, false),
                    None,
                    F::InputMessagesFilterEmpty,
                    (0, 0),
                    (778, empty(), 32)
                ),
            ]
        );
    }

    #[tokio::test]
    async fn a_search_in_one_chat_names_the_chat_and_the_sender() {
        let (live, _dir) = cold_client().await;
        let channel = ChatRef::new(ChatId::Channel(5), Some(7));
        let bob = ChatRef::new(ChatId::User(8), Some(-3));
        let mine = SearchQuery {
            media: Some(MediaFilter::VideoNote),
            ..search_query(
                SearchScope::Chat {
                    chat: channel,
                    from: Some(SearchFrom::Me),
                    topic: None,
                },
                "",
            )
        };
        assert!(live.search_messages(mine).await.is_err());
        let bobs = search_query(
            SearchScope::Chat {
                chat: channel,
                from: Some(SearchFrom::Peer(bob)),
                topic: None,
            },
            "",
        );
        let bobs = SearchQuery {
            media: Some(MediaFilter::VideoNote),
            cursor: Some(search::cursor_for(
                &SearchQuery {
                    media: Some(MediaFilter::VideoNote),
                    ..bobs.clone()
                },
                None,
                None,
                40,
            )),
            ..bobs
        };
        assert!(live.search_messages(bobs).await.is_err());

        let search = |from_id, offset_id| {
            tl::functions::messages::Search {
                peer: tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
                    channel_id: 5,
                    access_hash: 7,
                }),
                q: String::new(),
                from_id: Some(from_id),
                saved_peer_id: None,
                saved_reaction: None,
                top_msg_id: None,
                filter: tl::enums::MessagesFilter::InputMessagesFilterRoundVideo,
                min_date: 0,
                max_date: 0,
                offset_id,
                add_offset: 0,
                limit: 20,
                max_id: 0,
                min_id: 0,
                hash: 0,
            }
            .to_bytes()
        };
        assert_eq!(
            sent(&live),
            [
                search(tl::enums::InputPeer::PeerSelf, 0),
                search(
                    tl::enums::InputPeer::User(tl::types::InputPeerUser {
                        user_id: 8,
                        access_hash: -3,
                    }),
                    40
                ),
            ]
        );
    }

    #[tokio::test]
    async fn telegrams_search_refusals_are_the_callers_to_fix() {
        let (live, _dir) = cold_client().await;
        match live.classify(
            rpc::<tl::functions::messages::SearchGlobal>(400, "SEARCH_QUERY_EMPTY"),
            MethodClass::Search,
        ) {
            TgError::InvalidInput(message) => assert_eq!(message, search::QUERY_REQUIRED),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            live.classify(
                rpc::<tl::functions::messages::Search>(400, "FROM_PEER_INVALID"),
                MethodClass::Search,
            ),
            TgError::InvalidInput(_)
        ));
        match live.classify(
            rpc::<tl::functions::contacts::Search>(400, "QUERY_TOO_SHORT"),
            MethodClass::Search,
        ) {
            TgError::InvalidInput(message) => assert_eq!(message, find::QUERY_TOO_SHORT),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn find_chats_asks_contacts_search_for_more_than_the_caller_wants() {
        let (live, _dir) = cold_client().await;
        assert!(live.find_chats("acc", 10).await.is_err(), "offline");
        assert_eq!(
            sent(&live),
            [tl::functions::contacts::Search {
                broadcasts: false,
                bots: false,
                q: "acc".into(),
                limit: find::TELEGRAM_LIMIT,
            }
            .to_bytes()]
        );
    }

    #[tokio::test]
    async fn a_search_flood_holds_back_searches_but_reading_still_reaches_telegram() {
        let (live, _dir) = cold_client().await;
        let channel = ChatRef::new(ChatId::Channel(5), Some(7));
        flood_once::<tl::functions::contacts::Search>(&live).await;

        match live.call(&search(), None).await {
            Err(TgError::FloodWait {
                class: MethodClass::Search,
                ..
            }) => {}
            other => panic!("expected the search to fail fast, got {other:?}"),
        }
        // Both kinds of message search are held back without reaching Telegram.
        for scope in [
            everywhere(),
            SearchScope::Chat {
                chat: channel,
                from: None,
                topic: None,
            },
        ] {
            match live.search_messages(search_query(scope, "deploy")).await {
                Err(TgError::FloodWait {
                    class: MethodClass::Search,
                    ..
                }) => {}
                other => panic!("expected {scope:?} to fail fast, got {other:?}"),
            }
        }
        match live.find_chats("alice", 10).await {
            Err(TgError::FloodWait {
                class: MethodClass::Search,
                ..
            }) => {}
            other => panic!("expected find_chats to fail fast, got {other:?}"),
        }
        // The offline connection fails them, but they went out: not held back.
        assert!(!matches!(
            live.get_messages(channel, query()).await,
            Err(TgError::FloodWait { .. })
        ));
        assert!(!matches!(
            live.list_chats(listing(30, None, false)).await,
            Err(TgError::FloodWait { .. })
        ));
        assert!(!matches!(
            live.call(&download(), None).await,
            Err(TgError::FloodWait { .. })
        ));
        let reached: Vec<u32> = sent(&live).iter().map(|r| constructor(r)).collect();
        assert_eq!(
            reached,
            [
                // Not the search: it never reached Telegram.
                tl::functions::messages::GetHistory::CONSTRUCTOR_ID,
                tl::functions::messages::GetDialogs::CONSTRUCTOR_ID,
                tl::functions::upload::GetCdnFile::CONSTRUCTOR_ID,
            ]
        );
    }

    #[tokio::test]
    async fn a_reading_flood_holds_back_reading_but_not_search_or_files() {
        let (live, _dir) = cold_client().await;
        let channel = ChatRef::new(ChatId::Channel(5), Some(7));
        flood_once::<tl::functions::messages::GetHistory>(&live).await;

        for result in [
            live.get_messages(channel, query()).await.map(|_| ()),
            live.get_chat_info(channel).await.map(|_| ()),
            live.list_chats(listing(30, None, false)).await.map(|_| ()),
        ] {
            assert!(
                matches!(
                    result,
                    Err(TgError::FloodWait {
                        class: MethodClass::Default,
                        ..
                    })
                ),
                "{result:?}"
            );
        }
        assert!(!matches!(
            live.call(&search(), None).await,
            Err(TgError::FloodWait { .. })
        ));
        assert!(!matches!(
            live.call(&download(), None).await,
            Err(TgError::FloodWait { .. })
        ));
        assert!(!matches!(
            live.get_file(file_request()).await,
            Err(FileError::Failed(TgError::FloodWait { .. }))
        ));
        let reached: Vec<u32> = sent(&live).iter().map(|r| constructor(r)).collect();
        assert_eq!(
            reached,
            [
                // None of the reads: they never reached Telegram.
                tl::functions::contacts::Search::CONSTRUCTOR_ID,
                tl::functions::upload::GetCdnFile::CONSTRUCTOR_ID,
                tl::functions::upload::GetFile::CONSTRUCTOR_ID,
            ]
        );
    }

    fn file_request() -> FileRequest {
        FileRequest {
            location: FileLocation {
                file: StoredFile::Document,
                id: 6_000_000_001,
                access_hash: -99,
                file_reference: b"ref".to_vec(),
                dc_id: 4,
            },
            dc_id: 4,
            offset: 1024 * 1024,
            limit: download::PART_SIZE,
        }
    }

    #[tokio::test]
    async fn a_file_part_is_one_get_file_to_the_files_dc_through_the_limiter() {
        let (live, _dir) = cold_client().await;
        let mut photo = file_request();
        photo.location.file = StoredFile::Photo {
            thumb_size: "y".into(),
        };
        for request in [file_request(), photo] {
            // Offline: it fails, after reaching `call_in_dc` under the limiter.
            assert!(matches!(
                live.get_file(request).await,
                Err(FileError::Failed(TgError::Upstream(_)))
            ));
        }
        let get_file = |location| {
            tl::functions::upload::GetFile {
                precise: false,
                cdn_supported: false,
                location,
                offset: 1024 * 1024,
                limit: 512 * 1024,
            }
            .to_bytes()
        };
        assert_eq!(
            sent(&live),
            [
                get_file(tl::enums::InputFileLocation::InputDocumentFileLocation(
                    tl::types::InputDocumentFileLocation {
                        id: 6_000_000_001,
                        access_hash: -99,
                        file_reference: b"ref".to_vec(),
                        thumb_size: String::new(),
                    }
                )),
                get_file(tl::enums::InputFileLocation::InputPhotoFileLocation(
                    tl::types::InputPhotoFileLocation {
                        id: 6_000_000_001,
                        access_hash: -99,
                        file_reference: b"ref".to_vec(),
                        thumb_size: "y".into(),
                    }
                )),
            ]
        );
        assert!(!live.is_dead(), "an offline DC is no dead session");
    }

    #[tokio::test]
    async fn a_files_flood_holds_back_downloads_but_reading_still_reaches_telegram() {
        let (live, _dir) = cold_client().await;
        let channel = ChatRef::new(ChatId::Channel(5), Some(7));
        flood_once::<tl::functions::upload::GetFile>(&live).await;

        for _ in 0..2 {
            match live.get_file(file_request()).await {
                Err(FileError::Failed(TgError::FloodWait {
                    seconds,
                    class: MethodClass::Files,
                })) => assert!(seconds > 290, "{seconds}"),
                other => panic!("expected the download to fail fast, got {other:?}"),
            }
        }
        assert!(sent(&live).is_empty(), "no download reached Telegram");
        // Finding a message's file is reading: it still goes.
        assert!(live.media_file(channel, 500).await.is_err());
        assert!(live.get_messages(channel, query()).await.is_err());
        let reached: Vec<u32> = sent(&live).iter().map(|r| constructor(r)).collect();
        assert_eq!(
            reached,
            [
                tl::functions::channels::GetMessages::CONSTRUCTOR_ID,
                tl::functions::messages::GetHistory::CONSTRUCTOR_ID,
            ]
        );
    }

    #[tokio::test]
    async fn an_album_is_looked_up_with_one_get_messages_over_the_ids_around_it() {
        let (live, _dir) = cold_client().await;
        let ids = || {
            (491..=509)
                .map(|id| tl::enums::InputMessage::Id(tl::types::InputMessageId { id }))
                .collect::<Vec<_>>()
        };
        let channel = ChatRef::new(ChatId::Channel(5), Some(-77));
        let user = ChatRef::new(ChatId::User(7), Some(88));
        for chat in [channel, user] {
            // Offline: it fails, after one request.
            assert!(live.media_album(chat, 500).await.is_err());
        }
        assert_eq!(
            sent(&live),
            [
                tl::functions::channels::GetMessages {
                    channel: tl::enums::InputChannel::Channel(tl::types::InputChannel {
                        channel_id: 5,
                        access_hash: -77,
                    }),
                    id: ids(),
                }
                .to_bytes(),
                tl::functions::messages::GetMessages { id: ids() }.to_bytes(),
            ]
        );
    }

    #[tokio::test]
    async fn a_document_thumbnail_is_asked_for_by_its_size() {
        let (live, _dir) = cold_client().await;
        let mut thumb = file_request();
        thumb.location.file = StoredFile::DocumentThumb {
            thumb_size: "m".into(),
        };
        assert!(live.get_file(thumb).await.is_err());
        assert_eq!(
            sent(&live),
            [tl::functions::upload::GetFile {
                precise: false,
                cdn_supported: false,
                location: tl::enums::InputFileLocation::InputDocumentFileLocation(
                    tl::types::InputDocumentFileLocation {
                        id: 6_000_000_001,
                        access_hash: -99,
                        file_reference: b"ref".to_vec(),
                        thumb_size: "m".into(),
                    }
                ),
                offset: 1024 * 1024,
                limit: 512 * 1024,
            }
            .to_bytes()]
        );
    }

    #[tokio::test]
    async fn a_messages_file_is_looked_up_like_a_chain_message() {
        let (live, _dir) = cold_client().await;
        let id = || {
            vec![tl::enums::InputMessage::Id(tl::types::InputMessageId {
                id: 500,
            })]
        };
        let channel = ChatRef::new(ChatId::Channel(5), Some(-77));
        let user = ChatRef::new(ChatId::User(7), Some(88));
        for chat in [channel, user, ChatId::Chat(4).into()] {
            assert!(live.media_file(chat, 500).await.is_err());
        }
        assert_eq!(
            sent(&live),
            [
                tl::functions::channels::GetMessages {
                    channel: tl::enums::InputChannel::Channel(tl::types::InputChannel {
                        channel_id: 5,
                        access_hash: -77,
                    }),
                    id: id(),
                }
                .to_bytes(),
                tl::functions::messages::GetMessages { id: id() }.to_bytes(),
                tl::functions::messages::GetMessages { id: id() }.to_bytes(),
            ]
        );
    }
    #[tokio::test]
    async fn a_transcription_is_one_transcribe_audio_through_the_limiter() {
        let (live, _dir) = cold_client().await;
        let peer = tl::enums::InputPeer::User(tl::types::InputPeerUser {
            user_id: 7,
            access_hash: 88,
        });
        // Offline: it fails, after reaching `call` under the limiter.
        assert!(matches!(
            live.transcribe_audio(&peer, ChatId::User(7), 525_381).await,
            Err(TgError::Upstream(_))
        ));
        assert_eq!(
            sent(&live),
            [tl::functions::messages::TranscribeAudio {
                peer,
                msg_id: 525_381,
            }
            .to_bytes()]
        );
        // The message is looked up first; offline that is where it ends.
        let user = ChatRef::new(ChatId::User(7), Some(88));
        let deadline = tokio::time::Instant::now() + transcribe::TRANSCRIBE_WAIT;
        assert!(live.transcribe(user, 500, deadline).await.is_err());
        let reached: Vec<u32> = sent(&live).iter().map(|r| constructor(r)).collect();
        assert_eq!(
            reached[1..],
            [tl::functions::messages::GetMessages::CONSTRUCTOR_ID]
        );
    }

    #[tokio::test]
    async fn a_used_up_trial_holds_back_transcriptions_but_nothing_else() {
        let (live, _dir) = cold_client().await;
        let peer = tl::enums::InputPeer::User(tl::types::InputPeerUser {
            user_id: 7,
            access_hash: 88,
        });
        let channel = ChatRef::new(ChatId::Channel(5), Some(7));
        flood_once::<tl::functions::messages::TranscribeAudio>(&live).await;
        for _ in 0..2 {
            match live.transcribe_audio(&peer, ChatId::User(7), 525_381).await {
                Err(TgError::FloodWait {
                    seconds,
                    class: MethodClass::Transcribe,
                }) => assert!(seconds > 290, "{seconds}"),
                other => panic!("expected the transcription to fail fast, got {other:?}"),
            }
        }
        assert!(sent(&live).is_empty(), "no transcription reached Telegram");
        assert!(live.get_messages(channel, query()).await.is_err());
        assert!(live.media_file(channel, 500).await.is_err());
        let reached: Vec<u32> = sent(&live).iter().map(|r| constructor(r)).collect();
        assert_eq!(
            reached,
            [
                tl::functions::messages::GetHistory::CONSTRUCTOR_ID,
                tl::functions::channels::GetMessages::CONSTRUCTOR_ID,
            ]
        );
    }

    #[tokio::test]
    async fn telegrams_refusals_of_a_transcription_reach_the_caller_as_ours() {
        type TranscribeAudio = tl::functions::messages::TranscribeAudio;
        let (live, _dir) = cold_client().await;
        let peer = tl::enums::InputPeer::User(tl::types::InputPeerUser {
            user_id: 7,
            access_hash: 88,
        });
        live.answers.lock().unwrap().extend([
            rpc::<TranscribeAudio>(403, "PREMIUM_ACCOUNT_REQUIRED"),
            rpc::<TranscribeAudio>(400, "TRANSCRIPTION_FAILED"),
            rpc::<TranscribeAudio>(420, "FLOOD_WAIT_259200"),
        ]);
        let ask = || live.transcribe_audio(&peer, ChatId::User(7), 525_381);
        match ask().await {
            Err(TgError::InvalidInput(message)) => assert!(
                message.contains("Premium") && !message.contains("PREMIUM_ACCOUNT_REQUIRED"),
                "{message}"
            ),
            other => panic!("expected the Premium refusal, got {other:?}"),
        }
        match ask().await {
            Err(TgError::InvalidInput(message)) => {
                assert!(message.contains("couldn't transcribe"), "{message}")
            }
            other => panic!("expected the failure, got {other:?}"),
        }
        // A used-up trial: honoured for the Account, never asked again.
        for _ in 0..2 {
            assert!(matches!(
                ask().await,
                Err(TgError::FloodWait {
                    class: MethodClass::Transcribe,
                    ..
                })
            ));
        }
        assert_eq!(sent(&live).len(), 3, "the flood was not retried around");
    }

    #[tokio::test]
    async fn a_reading_flood_doesnt_hold_back_transcriptions() {
        let (live, _dir) = cold_client().await;
        let peer = tl::enums::InputPeer::User(tl::types::InputPeerUser {
            user_id: 7,
            access_hash: 88,
        });
        flood_once::<tl::functions::messages::GetHistory>(&live).await;
        assert!(matches!(
            live.transcribe_audio(&peer, ChatId::User(7), 525_381).await,
            Err(TgError::Upstream(_))
        ));
        assert_eq!(sent(&live).len(), 1, "the transcription reached Telegram");
    }
}

/// Forum Topics (#28): what the TL says of a message's Topic and of forums,
/// and the requests `topic` sends.
#[cfg(test)]
mod topic_tests {
    use grammers_client::tl::{Deserializable, Identifiable, Serializable};

    use super::*;
    use crate::sessions::{TokenTtl, Vault};

    async fn cold_client() -> (LiveClient, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open_path(&dir.path().join("vault.db"), TokenTtl::default())
            .await
            .unwrap();
        let issued = vault.create_grant("client", 42, b"").await.unwrap();
        let grant = vault.open(issued.access.expose()).await.unwrap();
        (offline_live(grant, Arc::new(SessionStore::new())), dir)
    }

    fn sent(live: &LiveClient) -> Vec<Vec<u8>> {
        live.sent.lock().unwrap().clone()
    }

    fn forum_peer() -> tl::enums::Peer {
        tl::enums::Peer::Channel(tl::types::PeerChannel { channel_id: 5 })
    }

    /// A message of channel 5 with `reply_to` (no other optional field).
    fn message(id: i32, reply_to: Option<tl::enums::MessageReplyHeader>) -> tl::enums::Message {
        let mut bytes = tl::types::Message::CONSTRUCTOR_ID.to_le_bytes().to_vec();
        let flags: u32 = if reply_to.is_some() { 1 << 3 } else { 0 };
        bytes.extend(flags.to_bytes());
        bytes.extend(0u32.to_bytes());
        bytes.extend(id.to_bytes());
        bytes.extend(forum_peer().to_bytes());
        if let Some(reply_to) = reply_to {
            bytes.extend(reply_to.to_bytes());
        }
        bytes.extend(100i32.to_bytes());
        bytes.extend(String::from("text").to_bytes());
        tl::enums::Message::from_bytes(&bytes).unwrap()
    }

    fn service(
        id: i32,
        reply_to: Option<tl::enums::MessageReplyHeader>,
        action: tl::enums::MessageAction,
    ) -> tl::enums::Message {
        tl::enums::Message::Service(tl::types::MessageService {
            out: false,
            mentioned: false,
            media_unread: false,
            reactions_are_possible: false,
            silent: false,
            post: false,
            legacy: false,
            id,
            from_id: None,
            peer_id: forum_peer(),
            saved_peer_id: None,
            reply_to,
            date: 100,
            action,
            reactions: None,
            ttl_period: None,
        })
    }

    /// A reply header: `forum_topic`, `reply_to_msg_id`, `reply_to_top_id`.
    fn header(
        forum_topic: bool,
        msg_id: i32,
        top_id: Option<i32>,
    ) -> tl::enums::MessageReplyHeader {
        tl::enums::MessageReplyHeader::Header(tl::types::MessageReplyHeader {
            reply_to_scheduled: false,
            forum_topic,
            quote: false,
            reply_to_ephemeral: false,
            reply_to_msg_id: Some(msg_id),
            reply_to_peer_id: None,
            reply_from: None,
            reply_media: None,
            reply_to_top_id: top_id,
            quote_text: None,
            quote_entities: None,
            quote_offset: None,
            todo_item_id: None,
            poll_option: None,
        })
    }

    fn topic_and_reply(message: tl::enums::Message) -> (Option<i32>, Option<i32>) {
        let m = raw_history_message(message).unwrap();
        (m.topic, m.reply_to)
    }

    #[test]
    fn a_message_names_its_topic_and_replies_only_when_it_replies() {
        // In Topic 10, replying to nothing: the header points at the Topic's
        // first message, which is no reply.
        assert_eq!(
            topic_and_reply(message(11, Some(header(true, 10, None)))),
            (Some(10), None)
        );
        // In Topic 10, replying to 11.
        assert_eq!(
            topic_and_reply(message(12, Some(header(true, 11, Some(10))))),
            (Some(10), Some(11))
        );
        // General (or no forum): a reply names no Topic.
        assert_eq!(
            topic_and_reply(message(13, Some(header(false, 2, None)))),
            (None, Some(2))
        );
        assert_eq!(topic_and_reply(message(14, None)), (None, None));
        // The message that created Topic 20 is in it; a service message in a
        // Topic names it in its header.
        let created = tl::enums::MessageAction::TopicCreate(tl::types::MessageActionTopicCreate {
            title_missing: false,
            title: "Bugs".into(),
            icon_color: 0,
            icon_emoji_id: None,
        });
        assert_eq!(
            topic_and_reply(service(20, None, created)),
            (Some(20), None)
        );
        assert_eq!(
            topic_and_reply(service(
                21,
                Some(header(true, 20, None)),
                tl::enums::MessageAction::PinMessage
            )),
            (Some(20), None)
        );
    }

    fn channel(forum: bool, min: bool) -> tl::enums::Chat {
        let (_, chat) = super::tests::tl_peers(min);
        let tl::enums::Chat::Channel(c) = chat else {
            unreachable!()
        };
        tl::enums::Chat::Channel(tl::types::Channel { forum, ..c })
    }

    fn forum_topic(id: i32, title: &str) -> tl::enums::ForumTopic {
        tl::enums::ForumTopic::Topic(tl::types::ForumTopic {
            my: false,
            closed: id == 20,
            pinned: id == 10,
            short: false,
            hidden: false,
            title_missing: false,
            id,
            date: 100,
            peer: tl::enums::Peer::Channel(tl::types::PeerChannel { channel_id: 7 }),
            title: title.into(),
            icon_color: 0,
            icon_emoji_id: None,
            top_message: id,
            read_inbox_max_id: 0,
            read_outbox_max_id: 0,
            unread_count: 0,
            unread_mentions_count: 0,
            unread_reactions_count: 0,
            unread_poll_votes_count: 0,
            from_id: tl::enums::Peer::User(tl::types::PeerUser { user_id: 1 }),
            notify_settings: tl::enums::PeerNotifySettings::Settings(
                tl::types::PeerNotifySettings {
                    show_previews: None,
                    silent: None,
                    mute_until: None,
                    ios_sound: None,
                    android_sound: None,
                    other_sound: None,
                    stories_muted: None,
                    stories_hide_sender: None,
                    stories_ios_sound: None,
                    stories_android_sound: None,
                    stories_other_sound: None,
                },
            ),
            draft: None,
        })
    }

    #[test]
    fn an_answer_says_which_chats_are_forums_and_which_topics_it_describes() {
        let forums = raw_forums(
            &[channel(true, false)],
            vec![
                forum_topic(10, "Releases"),
                forum_topic(20, "Bugs"),
                tl::enums::ForumTopic::Deleted(tl::types::ForumTopicDeleted { id: 30 }),
            ],
        );
        let topic = |id, title: &str, closed, pinned| RawTopic {
            chat: ChatId::Channel(7),
            id,
            title: title.into(),
            closed,
            pinned,
        };
        assert_eq!(
            forums,
            RawForums {
                forums: vec![ChatId::Channel(7)],
                topics: vec![
                    topic(10, "Releases", false, true),
                    topic(20, "Bugs", true, false)
                ],
            }
        );
        // A supergroup that is no forum, and a min channel, say nothing.
        assert_eq!(
            raw_forums(&[channel(false, false)], Vec::new()),
            RawForums::default()
        );
        assert_eq!(forum_flag(&channel(true, true)), None);
        assert_eq!(
            forum_flag(&channel(false, false)),
            Some((ChatId::Channel(7), false))
        );
    }

    fn peer() -> tl::enums::InputPeer {
        tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
            channel_id: 5,
            access_hash: 7,
        })
    }

    fn forum() -> ChatRef {
        ChatRef::new(ChatId::Channel(5), Some(7))
    }

    fn query(topic: i32) -> HistoryQuery {
        HistoryQuery {
            limit: 30,
            topic: Some(topic),
            ..HistoryQuery::default()
        }
    }

    #[tokio::test]
    async fn topic_on_a_user_or_basic_group_is_refused_without_asking_telegram() {
        let (live, _dir) = cold_client().await;
        let user = ChatRef::new(ChatId::User(8), Some(3));
        for chat in [user, ChatId::Chat(4).into()] {
            let err = live.get_messages(chat, query(1)).await.unwrap_err();
            assert!(
                matches!(&err, TgError::InvalidInput(e) if e.contains("not a forum")),
                "{err}"
            );
        }
        assert_eq!(sent(&live), Vec::<Vec<u8>>::new());
    }

    #[tokio::test]
    async fn a_channel_not_seen_yet_is_asked_whether_it_is_a_forum() {
        let (live, _dir) = cold_client().await;
        assert!(live.get_messages(forum(), query(10)).await.is_err());
        assert_eq!(
            sent(&live),
            [tl::functions::channels::GetChannels {
                id: vec![tl::enums::InputChannel::Channel(tl::types::InputChannel {
                    channel_id: 5,
                    access_hash: 7,
                })],
            }
            .to_bytes()]
        );
    }

    #[tokio::test]
    async fn a_topic_is_read_with_get_replies_and_searched_with_top_msg_id() {
        let (live, _dir) = cold_client().await;
        // An earlier answer described channel 5 as a forum.
        live.forums.lock().unwrap().insert(ChatId::Channel(5), true);
        let around = HistoryQuery {
            around_id: Some(40),
            limit: 5,
            ..query(10)
        };
        let pinned = HistoryQuery {
            pinned_only: true,
            ..query(1)
        };
        for q in [query(10), around, pinned] {
            assert!(live.get_messages(forum(), q).await.is_err());
        }
        let search = SearchQuery {
            scope: SearchScope::Chat {
                chat: forum(),
                from: None,
                topic: Some(10),
            },
            text: "deploy".into(),
            media: None,
            since: None,
            until: None,
            limit: 20,
            cursor: None,
        };
        assert!(live.search_messages(search).await.is_err());
        let replies = |offset_id, add_offset, limit| {
            tl::functions::messages::GetReplies {
                peer: peer(),
                msg_id: 10,
                offset_id,
                offset_date: 0,
                add_offset,
                limit,
                max_id: 0,
                min_id: 0,
                hash: 0,
            }
            .to_bytes()
        };
        let search = |q: &str, top_msg_id, filter, limit| {
            tl::functions::messages::Search {
                peer: peer(),
                q: q.into(),
                from_id: None,
                saved_peer_id: None,
                saved_reaction: None,
                top_msg_id: Some(top_msg_id),
                filter,
                min_date: 0,
                max_date: 0,
                offset_id: 0,
                add_offset: 0,
                limit,
                max_id: 0,
                min_id: 0,
                hash: 0,
            }
            .to_bytes()
        };
        assert_eq!(
            sent(&live),
            [
                replies(0, 0, 30),
                replies(40, -3, 5),
                search(
                    "",
                    1,
                    tl::enums::MessagesFilter::InputMessagesFilterPinned,
                    30
                ),
                search(
                    "deploy",
                    10,
                    tl::enums::MessagesFilter::InputMessagesFilterEmpty,
                    20
                ),
            ]
        );
    }

    #[test]
    fn a_topic_telegram_doesnt_know_is_the_callers_to_fix() {
        type Replies = tl::functions::messages::GetReplies;
        for code in ["TOPIC_ID_INVALID", "MSG_ID_INVALID"] {
            let refused = topic_refusal(Some(99), &rpc::<Replies>(400, code));
            assert!(
                matches!(&refused, Some(TgError::InvalidInput(e)) if e.contains("topic 99")),
                "{code}: {refused:?}"
            );
            // Without a Topic it is no Topic's fault.
            assert!(topic_refusal(None, &rpc::<Replies>(400, code)).is_none());
        }
        assert!(topic_refusal(Some(99), &rpc::<Replies>(400, "CHANNEL_PRIVATE")).is_none());
        assert!(topic_refusal(Some(99), &rpc::<Replies>(420, "FLOOD_WAIT_30")).is_none());
    }

    #[test]
    fn topic_requests_are_reading_calls() {
        use tl::functions::{channels, messages};
        assert_eq!(
            [
                method_class::<messages::GetReplies>(),
                method_class::<messages::GetForumTopics>(),
                method_class::<messages::GetForumTopicsById>(),
                method_class::<channels::GetChannels>(),
            ],
            [MethodClass::Default; 4]
        );
    }
}

#[cfg(test)]
mod folder_tests {
    use grammers_client::tl::Serializable;

    use super::*;
    use crate::sessions::{TokenTtl, Vault};

    const ACCOUNT: i64 = 42;

    async fn cold_client() -> (LiveClient, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open_path(&dir.path().join("vault.db"), TokenTtl::default())
            .await
            .unwrap();
        let issued = vault.create_grant("client", ACCOUNT, b"").await.unwrap();
        let grant = vault.open(issued.access.expose()).await.unwrap();
        (offline_live(grant, Arc::new(SessionStore::new())), dir)
    }

    fn sent(live: &LiveClient) -> Vec<Vec<u8>> {
        live.sent.lock().unwrap().clone()
    }

    fn title(text: &str) -> tl::enums::TextWithEntities {
        tl::enums::TextWithEntities::Entities(tl::types::TextWithEntities {
            text: text.into(),
            entities: Vec::new(),
        })
    }

    fn channel(id: i64, access_hash: i64) -> tl::enums::InputPeer {
        tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
            channel_id: id,
            access_hash,
        })
    }

    /// Folder 4 "Work" without a rule: pins channel 5, includes basic group
    /// 6, the Account's Saved Messages and user 7 known only from a message,
    /// excludes channel 8.
    fn work() -> tl::types::DialogFilter {
        tl::types::DialogFilter {
            contacts: false,
            non_contacts: false,
            groups: false,
            broadcasts: false,
            bots: false,
            exclude_muted: false,
            exclude_read: false,
            exclude_archived: false,
            title_noanimate: true,
            id: 4,
            title: title("Work"),
            emoticon: Some("💼".into()),
            color: Some(3),
            pinned_peers: vec![channel(5, -9)],
            include_peers: vec![
                tl::enums::InputPeer::Chat(tl::types::InputPeerChat { chat_id: 6 }),
                tl::enums::InputPeer::PeerSelf,
                tl::enums::InputPeer::UserFromMessage(Box::new(
                    tl::types::InputPeerUserFromMessage {
                        peer: channel(5, -9),
                        msg_id: 10,
                        user_id: 7,
                    },
                )),
                tl::enums::InputPeer::Empty,
            ],
            exclude_peers: vec![channel(8, 1)],
        }
    }

    #[test]
    fn a_folder_is_read_with_its_title_text_and_the_chats_it_names() {
        let folder = raw_chat_folder(tl::enums::DialogFilter::Filter(work()), ACCOUNT).unwrap();
        assert_eq!(
            folder,
            RawChatFolder {
                folder: ChatFolder {
                    id: 4,
                    title: "Work".into(),
                    emoji: Some("💼".into()),
                    rules: false,
                },
                pinned: vec![ChatRef::new(ChatId::Channel(5), Some(-9))],
                included: vec![
                    ChatRef::new(ChatId::Chat(6), None),
                    ChatRef::new(ChatId::User(ACCOUNT), None),
                    ChatRef::new(ChatId::User(7), None),
                ],
                excluded: vec![ChatId::Channel(8)],
            }
        );
    }

    #[test]
    fn every_flag_that_picks_chats_by_kind_or_state_is_a_rule_and_nothing_else_is() {
        type Flag = fn(&mut tl::types::DialogFilter);
        let rules: [(&str, Flag); 8] = [
            ("contacts", |f| f.contacts = true),
            ("non_contacts", |f| f.non_contacts = true),
            ("groups", |f| f.groups = true),
            ("broadcasts", |f| f.broadcasts = true),
            ("bots", |f| f.bots = true),
            ("exclude_muted", |f| f.exclude_muted = true),
            ("exclude_read", |f| f.exclude_read = true),
            ("exclude_archived", |f| f.exclude_archived = true),
        ];
        let rules_of = |filter| {
            raw_chat_folder(filter, ACCOUNT)
                .map(|f| f.folder.rules)
                .unwrap()
        };
        for (name, set) in rules {
            let mut filter = work();
            set(&mut filter);
            assert!(rules_of(tl::enums::DialogFilter::Filter(filter)), "{name}");
        }
        assert!(!rules_of(tl::enums::DialogFilter::Filter(work())));

        // A shared folder (chatlist) is a list by construction.
        let chatlist = tl::enums::DialogFilter::Chatlist(tl::types::DialogFilterChatlist {
            has_my_invites: true,
            title_noanimate: false,
            id: 9,
            title: title("Shared"),
            emoticon: None,
            color: None,
            pinned_peers: Vec::new(),
            include_peers: vec![channel(5, -9)],
        });
        let chatlist = raw_chat_folder(chatlist, ACCOUNT).unwrap();
        assert!(!chatlist.folder.rules);
        assert_eq!(chatlist.folder.title, "Shared");
        assert_eq!(chatlist.included.len(), 1);

        // "All chats" is no Chat folder.
        assert_eq!(
            raw_chat_folder(tl::enums::DialogFilter::Default, ACCOUNT),
            None
        );
    }

    #[tokio::test]
    async fn folder_chats_are_asked_for_with_get_peer_dialogs_and_their_hashes() {
        let (live, _dir) = cold_client().await;
        let folder = raw_chat_folder(tl::enums::DialogFilter::Filter(work()), ACCOUNT).unwrap();
        let peers = [folder.pinned, folder.included].concat();

        assert!(live.peer_dialogs(peers).await.is_err());
        // User 7 has no hash and the peer cache doesn't know it: not asked for.
        let dialog_peer =
            |peer| tl::enums::InputDialogPeer::Peer(tl::types::InputDialogPeer { peer });
        assert_eq!(
            sent(&live),
            [tl::functions::messages::GetPeerDialogs {
                peers: vec![
                    dialog_peer(channel(5, -9)),
                    dialog_peer(tl::enums::InputPeer::Chat(tl::types::InputPeerChat {
                        chat_id: 6
                    })),
                    dialog_peer(tl::enums::InputPeer::PeerSelf),
                ],
            }
            .to_bytes()]
        );

        // Nothing addressable left: no call at all.
        live.sent.lock().unwrap().clear();
        let none = live
            .peer_dialogs(vec![ChatRef::new(ChatId::User(7), None)])
            .await
            .unwrap();
        assert_eq!(none, DialogsAnswer::default());
        assert!(sent(&live).is_empty());
    }

    #[tokio::test]
    async fn a_folder_listing_starts_with_get_dialog_filters_through_the_limiter() {
        let (live, _dir) = cold_client().await;
        let query = ChatsQuery {
            limit: 10,
            chat_folder: Some(4),
            ..ChatsQuery::default()
        };
        assert!(live.list_chats(query.clone()).await.is_err());
        assert!(live.chat_folders().await.is_err());
        let filters = tl::functions::messages::GetDialogFilters {}.to_bytes();
        assert_eq!(sent(&live), [filters.clone(), filters]);

        // Telegram's FLOOD_WAIT holds back the next listing, never retried.
        live.sent.lock().unwrap().clear();
        live.answers
            .lock()
            .unwrap()
            .push_back(rpc::<tl::functions::messages::GetDialogFilters>(
                420,
                "FLOOD_WAIT_300",
            ));
        assert!(matches!(
            live.chat_folders().await,
            Err(TgError::FloodWait { seconds: 300, .. })
        ));
        assert!(matches!(
            live.list_chats(query).await,
            Err(TgError::FloodWait { .. })
        ));
        assert_eq!(sent(&live).len(), 1, "only the flooded request went out");
    }
}
