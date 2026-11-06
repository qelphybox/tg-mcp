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

use crate::limits::{AccountLimiter, Limits};

use super::{
    Account, Connector, LoggedIn, Login, LoginAttempt, LoginState, Telegram, TgError,
    chats::{
        self, ChatId, ChatInfo, ChatKind, ChatPage, ChatRef, Cursor, RawDialog, RawMessage, RawPeer,
    },
    messages::{HistoryQuery, MessagePage, RawHistoryMessage, assemble_history},
    pool::{Live, Pool},
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
/// whole Account; grammers' default would sleep inside one call, unseen.
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

/// Like [`upstream`], but surfaces `FLOOD_WAIT_X` so the Login flow can honour it.
fn login_error(err: InvocationError) -> TgError {
    match &err {
        InvocationError::Rpc(rpc) if rpc.code == 420 && rpc.name == "FLOOD_WAIT" => {
            TgError::FloodWait {
                seconds: rpc.value.unwrap_or(60),
            }
        }
        _ => upstream(err),
    }
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
    /// Every request sent, serialized, so tests can see what reached Telegram.
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
    fn classify(&self, err: InvocationError) -> TgError {
        if is_dead_session(&err) {
            self.dead.store(true, std::sync::atomic::Ordering::SeqCst);
            TgError::BadSession(err.to_string())
        } else {
            // FLOOD_WAIT surfaces as TgError::FloodWait for the limiter; the rest is upstream.
            login_error(err)
        }
    }

    /// One request to Telegram, under the Account's limiter. `chat` gives
    /// "can't read this chat" errors a caller-facing meaning.
    async fn call<R: tl::RemoteCall>(
        &self,
        request: &R,
        chat: Option<ChatId>,
    ) -> Result<R::Return, TgError> {
        #[cfg(test)]
        self.sent
            .lock()
            .unwrap()
            .push(tl::Serializable::to_bytes(request));
        self.limiter
            .run(|| async {
                self.client.invoke(request).await.map_err(|err| match chat {
                    Some(chat) => self.classify_chat_error(err, chat),
                    None => self.classify(err),
                })
            })
            .await
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
        let archived = cursor.as_ref().map_or(archived, |c| c.archived);
        let (offset_date, offset_id, offset_peer) = match &cursor {
            Some(c) => (
                c.offset_date,
                c.offset_id,
                input_peer(c.offset_peer.parse()?, c.offset_access_hash),
            ),
            None => (0, 0, tl::enums::InputPeer::Empty),
        };
        let answer = self
            .call(
                &tl::functions::messages::GetDialogs {
                    exclude_pinned: false,
                    folder_id: archived.then_some(1),
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
        let peers: Vec<RawPeer> = users
            .into_iter()
            .filter_map(raw_user)
            .chain(chats.into_iter().filter_map(raw_chat))
            .collect();
        let messages: Vec<RawMessage> = messages.into_iter().filter_map(raw_message).collect();

        self.remember(&peers);
        Ok(chats::assemble_page(
            &dialogs, &peers, &messages, full_page, archived,
        ))
    }

    async fn get_messages(&self, id: ChatRef, query: HistoryQuery) -> Result<MessagePage, TgError> {
        let peer = self.resolve(id).await?;
        let chat = id.chat();
        let answer = self
            .call(
                &tl::functions::messages::GetHistory {
                    peer,
                    offset_id: query.before_id.unwrap_or(0),
                    // getHistory returns messages strictly before offset_date.
                    offset_date: query
                        .until
                        .map(|until| i32::try_from(until.saturating_add(1)).unwrap_or(i32::MAX))
                        .unwrap_or(0),
                    add_offset: 0,
                    limit: query.limit as i32,
                    max_id: 0,
                    min_id: 0,
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
        let full_page = messages.len() >= query.limit;
        let raw: Vec<RawHistoryMessage> = messages
            .into_iter()
            .filter_map(raw_history_message)
            .collect();
        let peers: Vec<RawPeer> = users
            .into_iter()
            .filter_map(raw_user)
            .chain(chats.into_iter().filter_map(raw_chat))
            .collect();
        self.remember(&peers);
        Ok(assemble_history(id, &raw, &peers, &query, full_page))
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
        let peers: Vec<RawPeer> = users
            .into_iter()
            .filter_map(raw_user)
            .chain(chats.into_iter().filter_map(raw_chat))
            .collect();
        self.remember(&peers);
        let peer = peers
            .iter()
            .find(|p| p.id == chat)
            .ok_or_else(|| TgError::Upstream(format!("Telegram didn't describe {chat}")))?;
        Ok(chats::chat_info(peer, description, members))
    }
}

/// How far `resolve` pages through the dialogs (per folder) looking for an unknown Chat.
const RESOLVE_MAX_PAGES: usize = 5;

impl LiveClient {
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
    fn classify_chat_error(&self, err: InvocationError, chat: ChatId) -> TgError {
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
        self.classify(err)
    }
}

fn raw_history_message(message: tl::enums::Message) -> Option<RawHistoryMessage> {
    match message {
        tl::enums::Message::Message(m) => {
            let text = if m.message.is_empty() {
                media_placeholder(m.media.as_ref()).to_string()
            } else {
                m.message
            };
            let reply_to = match m.reply_to {
                // Replies into another chat aren't addressable here.
                Some(tl::enums::MessageReplyHeader::Header(h)) if h.reply_to_peer_id.is_none() => {
                    h.reply_to_msg_id
                }
                _ => None,
            };
            Some(RawHistoryMessage {
                id: m.id,
                date: m.date,
                from: m.from_id.as_ref().map(chat_id),
                out: m.out,
                text,
                reply_to,
                forwarded: m.fwd_from.is_some(),
                edited: m.edit_date.is_some(),
            })
        }
        tl::enums::Message::Service(m) => Some(RawHistoryMessage {
            id: m.id,
            date: m.date,
            from: m.from_id.as_ref().map(chat_id),
            out: m.out,
            text: "[service message]".into(),
            reply_to: None,
            forwarded: false,
            edited: false,
        }),
        tl::enums::Message::Empty(_) => None,
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
        tl::enums::Message::Message(m) => {
            let text = if m.message.is_empty() {
                media_placeholder(m.media.as_ref()).to_string()
            } else {
                m.message
            };
            Some(RawMessage {
                chat: chat_id(&m.peer_id),
                id: m.id,
                date: m.date,
                text,
            })
        }
        tl::enums::Message::Service(m) => Some(RawMessage {
            chat: chat_id(&m.peer_id),
            id: m.id,
            date: m.date,
            text: "[service message]".into(),
        }),
        tl::enums::Message::Empty(_) => None,
    }
}

/// What to show instead of an empty text. Never includes media content.
fn media_placeholder(media: Option<&tl::enums::MessageMedia>) -> &'static str {
    use tl::enums::MessageMedia as M;
    match media {
        None | Some(M::Empty) => "",
        Some(M::Photo(_)) => "[photo]",
        Some(M::Document(d)) => match &d.document {
            Some(tl::enums::Document::Document(doc)) => doc
                .attributes
                .iter()
                .find_map(|a| match a {
                    tl::enums::DocumentAttribute::Audio(a) if a.voice => Some("[voice]"),
                    tl::enums::DocumentAttribute::Audio(_) => Some("[audio]"),
                    tl::enums::DocumentAttribute::Video(v) if v.round_message => {
                        Some("[video message]")
                    }
                    tl::enums::DocumentAttribute::Video(_) => Some("[video]"),
                    tl::enums::DocumentAttribute::Sticker(_) => Some("[sticker]"),
                    tl::enums::DocumentAttribute::Animated => Some("[gif]"),
                    _ => None,
                })
                .unwrap_or("[file]"),
            _ => "[file]",
        },
        Some(M::Geo(_) | M::GeoLive(_) | M::Venue(_)) => "[location]",
        Some(M::Contact(_)) => "[contact]",
        Some(M::Poll(_)) => "[poll]",
        Some(M::Dice(_)) => "[dice]",
        Some(M::Story(_)) => "[story]",
        Some(_) => "[media]",
    }
}

pub struct GrammersConnector {
    credentials: AppCredentials,
    vault: Arc<Vault>,
    limits: Arc<Limits>,
    pool: Pool<LiveClient>,
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
}

/// Every minute: evict idle Live clients and re-seal sessions that changed.
async fn sweep(connector: Weak<GrammersConnector>) {
    let mut every = tokio::time::interval(Duration::from_secs(60));
    loop {
        every.tick().await;
        let Some(connector) = connector.upgrade() else {
            return;
        };
        for (_, live, snapshot) in connector.pool.evict_idle() {
            if let Err(err) = connector.vault.save_session(&live.grant, &snapshot).await {
                tracing::warn!(grant_id = %live.grant.grant_id, error = %err, "re-sealing telegram session failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A user or channel 7 with access hash 99, as Telegram would send it (only
    /// the fields that aren't optional; the TL structs have 50 of them).
    fn tl_peers(min: bool) -> (tl::enums::User, tl::enums::Chat) {
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
        let rpc = |code, message: &str| {
            InvocationError::Rpc(
                tl::types::RpcError {
                    error_code: code,
                    error_message: message.into(),
                }
                .into(),
            )
        };
        assert!(matches!(
            login_error(rpc(420, "FLOOD_WAIT_31")),
            TgError::FloodWait { seconds: 31 }
        ));
        assert!(matches!(
            login_error(rpc(400, "PASSWORD_HASH_INVALID")),
            TgError::Upstream(_)
        ));
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

/// What `get_messages` / `get_chat_info` send to Telegram for each kind of id,
/// on a fresh Live client (empty peer cache) whose connection is never started:
/// every request is recorded, then fails at once, so nothing leaves the machine.
#[cfg(test)]
mod resolve_tests {
    use grammers_client::tl::{Identifiable, Serializable};

    use super::*;
    use crate::{
        limits::LimitsConfig,
        sessions::{TokenTtl, Vault},
    };

    const ACCOUNT: i64 = 42;

    async fn cold_client() -> (LiveClient, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open_path(&dir.path().join("vault.db"), TokenTtl::default())
            .await
            .unwrap();
        let issued = vault.create_grant("client", ACCOUNT, b"").await.unwrap();
        let grant = vault.open(issued.access.expose()).await.unwrap();
        let store = Arc::new(SessionStore::new());
        // The runner is dropped: no connection, every request fails right away.
        let SenderPool { handle, .. } = SenderPool::new(store.clone(), 0);
        let live = LiveClient {
            client: Client::new(handle),
            store,
            grant,
            peers: Default::default(),
            dead: Default::default(),
            limiter: Arc::new(AccountLimiter::new(LimitsConfig::default())),
            sent: Default::default(),
        };
        (live, dir)
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
            before_id: None,
            since: None,
            until: None,
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
}
