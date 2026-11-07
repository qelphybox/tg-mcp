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
        self, ChatId, ChatInfo, ChatKind, ChatPage, ChatRef, Cursor, Listing, RawDialog,
        RawMessage, RawPeer,
    },
    download::{self, FileError, FilePart, FileRequest, MediaFile, RawMediaMessage},
    find::{self, FindRequest, FoundAnswer, FoundChats, QUERY_TOO_SHORT},
    media,
    media::{FileLocation, StoredFile},
    messages::{
        HistoryAnswer, HistoryQuery, HistoryRequest, MessageAnswer, MessageAt, MessagePage,
        MessageRequest, RawHistoryMessage, read_history,
    },
    pool::{Evicted, Live, Pool},
    rich::message_text,
    search::{
        self, ChatType, MediaFilter, QUERY_REQUIRED, RawFoundMessage, RequestScope, SearchAnswer,
        SearchFrom, SearchPage, SearchQuery, SearchRequest,
    },
    session::SessionStore,
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
    /// Telegram said this session is gone (logged out / revoked from the app).
    dead: std::sync::atomic::AtomicBool,
    /// The Account's budget and FLOOD_WAIT state, shared with its other Grants.
    limiter: Arc<AccountLimiter>,
    /// Every request handed to grammers (past the limiter), serialized, so tests can
    /// see what reached Telegram.
    #[cfg(test)]
    sent: std::sync::Mutex<Vec<Vec<u8>>>,
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
        let class = method_class::<R>();
        self.limiter
            .run(class, || async {
                #[cfg(test)]
                self.sent
                    .lock()
                    .unwrap()
                    .push(tl::Serializable::to_bytes(request));
                self.client.invoke(request).await.map_err(|err| match chat {
                    Some(chat) => self.classify_chat_error(err, chat, class),
                    None => self.classify(err, class),
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
    async fn list_chats(
        &self,
        limit: usize,
        cursor: Option<&str>,
        archived: bool,
    ) -> Result<ChatPage, TgError> {
        let cursor = cursor.map(Cursor::decode).transpose()?;
        let listing = Listing {
            archived: cursor.as_ref().map_or(archived, |c| c.archived),
            first_page: cursor.is_none(),
        };
        let (offset_date, offset_id, offset_peer) = match &cursor {
            Some(Cursor {
                offset_date,
                offset_id,
                offset_peer: Some(peer),
                offset_access_hash,
                ..
            }) => (
                *offset_date,
                *offset_id,
                input_peer(peer.parse()?, *offset_access_hash),
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
        // A full page may have more after it; a short one is the end.
        let full_page = dialogs.len() >= limit;
        let dialogs: Vec<RawDialog> = dialogs.into_iter().filter_map(raw_dialog).collect();
        let peers = self.described(users, chats);
        let messages: Vec<RawMessage> = messages.into_iter().filter_map(raw_message).collect();

        Ok(chats::assemble_page(
            &dialogs, &peers, &messages, limit, full_page, listing,
        ))
    }

    async fn get_messages(&self, id: ChatRef, query: HistoryQuery) -> Result<MessagePage, TgError> {
        let peer = self.resolve(id).await?;
        let peer = &peer;
        read_history(
            id,
            &query,
            &|chat| self.cached(chat),
            |request| self.get_history(peer, id.chat(), request),
            |request| self.get_message(peer, id.chat(), request),
        )
        .await
    }

    async fn get_chat_info(&self, id: ChatRef) -> Result<ChatInfo, TgError> {
        let peer = self.resolve(id).await?;
        let chat = id.chat();
        let (description, members, users, chats) = match peer {
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
                (user.about, None, full.users, full.chats)
            }
            tl::enums::InputPeer::Chat(c) => {
                let tl::enums::messages::ChatFull::Full(full) = self
                    .call(
                        &tl::functions::messages::GetFullChat { chat_id: c.chat_id },
                        Some(chat),
                    )
                    .await?;
                let (about, members) = full_chat_details(full.full_chat);
                (about, members, full.users, full.chats)
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
                (about, members, full.users, full.chats)
            }
            _ => return Err(TgError::InvalidInput(format!("{chat} can't be looked up"))),
        };
        let peers = self.described(users, chats);
        let peer = peers
            .iter()
            .find(|p| p.id == chat)
            .ok_or_else(|| TgError::Upstream(format!("Telegram didn't describe {chat}")))?;
        Ok(chats::chat_info(peer, description, members))
    }

    async fn search_messages(&self, query: SearchQuery) -> Result<SearchPage, TgError> {
        search::search(&query, &|chat| self.cached(chat), |request| {
            self.search(request)
        })
        .await
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
    /// One `messages.getHistory` call.
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
        } = request;
        let answer = self
            .call(
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
            .await?;
        let (messages, chats, users) = match answer {
            tl::enums::messages::Messages::Messages(m) => (m.messages, m.chats, m.users),
            tl::enums::messages::Messages::Slice(m) => (m.messages, m.chats, m.users),
            tl::enums::messages::Messages::ChannelMessages(m) => (m.messages, m.chats, m.users),
            tl::enums::messages::Messages::NotModified(_) => (vec![], vec![], vec![]),
        };
        let returned = messages.len();
        let messages: Vec<RawHistoryMessage> = messages
            .into_iter()
            .filter_map(raw_history_message)
            .collect();
        let peers = self.described(users, chats);
        Ok(HistoryAnswer {
            messages,
            returned,
            peers,
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
        let (messages, users, chats) = self.fetch_message(peer, chat, request).await?;
        let peers = self.described(users, chats);
        Ok(MessageAnswer {
            messages: messages.into_iter().filter_map(raw_found_message).collect(),
            peers,
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
        let (messages, users, chats) = self.fetch_message(peer, chat, request).await?;
        self.described(users, chats);
        Ok(messages.into_iter().filter_map(raw_media_message).collect())
    }

    /// The messages, users and chats of one `channels.getMessages` /
    /// `messages.getMessages` call for one id.
    async fn fetch_message(
        &self,
        peer: &tl::enums::InputPeer,
        chat: ChatId,
        MessageRequest { id }: MessageRequest,
    ) -> Result<
        (
            Vec<tl::enums::Message>,
            Vec<tl::enums::User>,
            Vec<tl::enums::Chat>,
        ),
        TgError,
    > {
        let id = vec![tl::enums::InputMessage::Id(tl::types::InputMessageId {
            id,
        })];
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
                offset_id,
            } => {
                let peer = self.resolve(chat).await?;
                let from_id = match from {
                    None => None,
                    Some(SearchFrom::Me) => Some(tl::enums::InputPeer::PeerSelf),
                    Some(SearchFrom::Peer(sender)) => Some(self.resolve(sender).await?),
                };
                self.call(
                    &tl::functions::messages::Search {
                        peer,
                        q,
                        from_id,
                        saved_peer_id: None,
                        saved_reaction: None,
                        top_msg_id: None,
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
                )
                .await?
            }
        };
        let (messages, chats, users, complete, next_rate) = match answer {
            tl::enums::messages::Messages::Messages(m) => {
                (m.messages, m.chats, m.users, true, None)
            }
            tl::enums::messages::Messages::Slice(m) => {
                (m.messages, m.chats, m.users, false, m.next_rate)
            }
            tl::enums::messages::Messages::ChannelMessages(m) => {
                (m.messages, m.chats, m.users, false, None)
            }
            tl::enums::messages::Messages::NotModified(_) => (vec![], vec![], vec![], true, None),
        };
        let returned = messages.len();
        let peers = self.described(users, chats);
        Ok(SearchAnswer {
            messages: messages.into_iter().filter_map(raw_found_message).collect(),
            returned,
            complete,
            next_rate,
            peers,
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
                let page = self.list_chats(100, cursor.as_deref(), archived).await?;
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

fn raw_history_message(message: tl::enums::Message) -> Option<RawHistoryMessage> {
    match message {
        tl::enums::Message::Message(m) => {
            let (reply_to, reply_elsewhere) = match m.reply_to {
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
            })
        }
        tl::enums::Message::Service(m) => Some(RawHistoryMessage {
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
        }),
        tl::enums::Message::Empty(_) => None,
    }
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

fn raw_dialog(dialog: tl::enums::Dialog) -> Option<RawDialog> {
    match dialog {
        tl::enums::Dialog::Dialog(d) => Some(RawDialog {
            chat: chat_id(&d.peer),
            top_message: d.top_message,
            unread_count: d.unread_count,
            pinned: d.pinned,
            folder_id: d.folder_id.unwrap_or(0),
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
        let Connected { client, store, .. } = connect(&self.credentials, store, Purpose::Live);
        // Prove the session still works before handing it out.
        let live = LiveClient {
            client,
            store,
            grant,
            peers: Default::default(),
            dead: Default::default(),
            limiter: self.limits.for_account(grant_account),
            #[cfg(test)]
            sent: Default::default(),
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
        let archived = raw_dialog(dialog(Some(1), true)).unwrap();
        assert_eq!(
            archived,
            RawDialog {
                chat: ChatId::Channel(7),
                top_message: 55,
                unread_count: 3,
                pinned: true,
                folder_id: 1,
            }
        );
        // Telegram leaves the flag out for the main list.
        let main = raw_dialog(dialog(None, false)).unwrap();
        assert_eq!((main.folder_id, main.pinned), (0, false));
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
        dead: Default::default(),
        limiter: Arc::new(AccountLimiter::new(crate::limits::LimitsConfig::default())),
        sent: Default::default(),
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
        };
        let archive_top = Cursor {
            offset_date: 0,
            offset_id: 0,
            offset_peer: None,
            offset_access_hash: None,
            archived: true,
        };

        assert!(live.list_chats(30, None, false).await.is_err());
        assert!(live.list_chats(30, None, true).await.is_err());
        // The cursor decides the folder, whatever `archived` says.
        let next = after_channel.encode();
        assert!(live.list_chats(30, Some(&next), true).await.is_err());
        let next = archive_top.encode();
        assert!(live.list_chats(30, Some(&next), false).await.is_err());

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
                },
                "",
            )
        };
        assert!(live.search_messages(mine).await.is_err());
        let bobs = search_query(
            SearchScope::Chat {
                chat: channel,
                from: Some(SearchFrom::Peer(bob)),
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
            live.list_chats(30, None, false).await,
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
            live.list_chats(30, None, false).await.map(|_| ()),
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
}
