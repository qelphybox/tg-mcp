//! The harness's own Telegram logins (the Driver and the Peer), on the same
//! pinned grammers as the server, kept in the server's snapshot format.

use std::{
    io::{BufRead, Write},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use grammers_client::{Client, InvocationError, SenderPool, SignInError, tl};
use grammers_mtsender::ConnectionParams;
use tg_mcp::tg::session::SessionStore;
use zeroize::Zeroizing;

use crate::{
    blocked,
    config::{self, App},
    world::{Driver, Party, Peer, SeenDraft, SeenEntity},
};

/// The device the server's sessions show as in Telegram's Active sessions.
const SERVER_DEVICE: &str = "Unofficial MCP Server";

/// Which of the harness's logins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Driver,
    Peer,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Role::Driver => "Driver",
            Role::Peer => "Peer",
        }
    }

    fn session_key(self) -> &'static str {
        match self {
            Role::Driver => config::DRIVER_SESSION,
            Role::Peer => config::PEER_SESSION,
        }
    }

    fn id_key(self) -> &'static str {
        match self {
            Role::Driver => config::ACCOUNT_ID,
            Role::Peer => config::PEER_ID,
        }
    }
}

/// A logged-in Telegram client of the Test account or the Peer.
pub struct TgClient {
    pub(crate) client: Client,
    role: Role,
}

impl Drop for TgClient {
    fn drop(&mut self) {
        self.client.disconnect();
    }
}

fn connect(app: &App, store: SessionStore, role: Role) -> (Client, Arc<SessionStore>) {
    let store = Arc::new(store);
    let SenderPool { runner, handle, .. } = SenderPool::with_configuration(
        store.clone(),
        app.api_id,
        ConnectionParams {
            device_model: format!("tg-mcp acceptance {}", role.name()),
            system_version: "tg-mcp acceptance".into(),
            app_version: env!("CARGO_PKG_VERSION").into(),
            ..ConnectionParams::default()
        },
    );
    tokio::spawn(runner.run());
    (Client::new(handle), store)
}

/// A Telegram error as the run sees it: blocked when Telegram limits the
/// account, else a plain failure.
pub(crate) fn telegram(err: InvocationError, what: &str) -> anyhow::Error {
    if let InvocationError::Rpc(rpc) = &err
        && let Some(b) = blocked::rpc(rpc.code, &rpc.name, rpc.value)
    {
        return anyhow::Error::new(b).context(what.to_string());
    }
    anyhow::Error::new(err).context(what.to_string())
}

impl TgClient {
    /// Connects with a stored login.
    pub fn open(app: &App, snapshot: &[u8], role: Role) -> Result<Self> {
        let store = SessionStore::from_snapshot(snapshot)
            .map_err(|e| anyhow::anyhow!("the {} login is unreadable: {e}", role.name()))?;
        let (client, _) = connect(app, store, role);
        Ok(Self { client, role })
    }

    /// The logged-in account's user id.
    pub async fn me(&self) -> Result<i64> {
        let users = self
            .client
            .invoke(&tl::functions::users::GetUsers {
                id: vec![tl::enums::InputUser::UserSelf],
            })
            .await
            .map_err(|e| telegram(e, &format!("the {}'s own account", self.role.name())))?;
        match users.into_iter().next() {
            Some(tl::enums::User::User(u)) => Ok(u.id),
            _ => bail!("Telegram didn't name the {}'s account", self.role.name()),
        }
    }
}

impl TgClient {
    /// The account's first and last name and username, as Telegram has them.
    pub async fn names(&self) -> Result<Vec<String>> {
        let users = self
            .client
            .invoke(&tl::functions::users::GetUsers {
                id: vec![tl::enums::InputUser::UserSelf],
            })
            .await
            .map_err(|e| telegram(e, &format!("the {}'s own account", self.role.name())))?;
        Ok(match users.into_iter().next() {
            Some(tl::enums::User::User(u)) => [u.first_name, u.last_name, u.username]
                .into_iter()
                .flatten()
                .filter(|n| !n.is_empty())
                .collect(),
            _ => Vec::new(),
        })
    }
}

#[async_trait]
impl Party for TgClient {
    async fn account_id(&self) -> Result<i64> {
        self.me().await
    }

    async fn accept_login(&self, qr_url: &str) -> Result<()> {
        let token = crate::fake::login_token(qr_url)?;
        let token = URL_SAFE_NO_PAD
            .decode(token.trim_end_matches('='))
            .context("the QR's token is not base64url")?;
        self.client
            .invoke(&tl::functions::auth::AcceptLoginToken { token })
            .await
            .map_err(|e| telegram(e, "auth.acceptLoginToken"))?;
        Ok(())
    }
}

/// A chat id as the server issues it (`user:N~hash`), as Telegram's input peer:
/// the server and the Driver are the same account, so the hash is valid for both.
pub(crate) fn input_peer(chat: &str) -> Result<tl::enums::InputPeer> {
    use std::str::FromStr;
    use tg_mcp::tg::chats::{ChatId, ChatRef};
    let parsed = ChatRef::from_str(chat).map_err(|e| anyhow::anyhow!("{chat}: {e}"))?;
    let hash = parsed.access_hash().unwrap_or(0);
    Ok(match parsed.chat() {
        ChatId::User(user_id) => tl::enums::InputPeer::User(tl::types::InputPeerUser {
            user_id,
            access_hash: hash,
        }),
        ChatId::Chat(chat_id) => tl::enums::InputPeer::Chat(tl::types::InputPeerChat { chat_id }),
        ChatId::Channel(channel_id) => tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
            channel_id,
            access_hash: hash,
        }),
    })
}

fn seen(draft: tl::enums::DraftMessage, topic: Option<i32>) -> Option<SeenDraft> {
    let tl::enums::DraftMessage::Message(m) = draft else {
        return None;
    };
    if m.message.is_empty() {
        return None;
    }
    let entities = m
        .entities
        .unwrap_or_default()
        .into_iter()
        .map(|e| {
            let (kind, offset, length, url) = match e {
                tl::enums::MessageEntity::Bold(e) => ("bold", e.offset, e.length, None),
                tl::enums::MessageEntity::Italic(e) => ("italic", e.offset, e.length, None),
                tl::enums::MessageEntity::Code(e) => ("code", e.offset, e.length, None),
                tl::enums::MessageEntity::Pre(e) => ("pre", e.offset, e.length, None),
                tl::enums::MessageEntity::TextUrl(e) => {
                    ("text_url", e.offset, e.length, Some(e.url))
                }
                _ => ("other", 0, 0, None),
            };
            SeenEntity {
                kind: kind.into(),
                offset: offset.max(0) as usize,
                length: length.max(0) as usize,
                url,
            }
        })
        .collect();
    let reply_to = match m.reply_to {
        Some(tl::enums::InputReplyTo::Message(r)) if Some(r.reply_to_msg_id) != topic => {
            Some(r.reply_to_msg_id)
        }
        _ => None,
    };
    Some(SeenDraft {
        text: m.message,
        entities,
        reply_to,
    })
}

impl TgClient {
    async fn dialog_draft(&self, peer: tl::enums::InputPeer) -> Result<Option<SeenDraft>> {
        let tl::enums::messages::PeerDialogs::Dialogs(found) = self
            .client
            .invoke(&tl::functions::messages::GetPeerDialogs {
                peers: vec![tl::enums::InputDialogPeer::Peer(
                    tl::types::InputDialogPeer { peer },
                )],
            })
            .await
            .map_err(|e| telegram(e, "messages.getPeerDialogs"))?;
        Ok(found.dialogs.into_iter().find_map(|d| match d {
            tl::enums::Dialog::Dialog(d) => d.draft.and_then(|draft| seen(draft, None)),
            _ => None,
        }))
    }

    async fn topic_draft(
        &self,
        peer: tl::enums::InputPeer,
        topic: i32,
    ) -> Result<Option<SeenDraft>> {
        let tl::enums::messages::ForumTopics::Topics(topics) = self
            .client
            .invoke(&tl::functions::messages::GetForumTopics {
                peer: peer.clone(),
                q: None,
                offset_date: 0,
                offset_id: 0,
                offset_topic: 0,
                limit: 100,
            })
            .await
            .map_err(|e| telegram(e, "messages.getForumTopics"))?;
        let draft = topics.topics.into_iter().find_map(|t| match t {
            tl::enums::ForumTopic::Topic(t) if t.id == topic => Some(t.draft),
            _ => None,
        });
        match draft {
            Some(Some(d)) => Ok(seen(d, Some(topic))),
            // General's Draft may be the chat's own.
            _ if topic == 1 => self.dialog_draft(peer).await,
            _ => Ok(None),
        }
    }
}

#[async_trait]
impl Driver for TgClient {
    async fn draft(&self, chat: &str, topic: Option<i32>) -> Result<Option<SeenDraft>> {
        let peer = input_peer(chat)?;
        match topic {
            None => self.dialog_draft(peer).await,
            Some(t) => self.topic_draft(peer, t).await,
        }
    }

    async fn delete_message(&self, chat: &str, id: i64) -> Result<()> {
        let id = i32::try_from(id).context("a message id out of range")?;
        match input_peer(chat)? {
            tl::enums::InputPeer::Channel(c) => {
                self.client
                    .invoke(&tl::functions::channels::DeleteMessages {
                        channel: tl::enums::InputChannel::Channel(tl::types::InputChannel {
                            channel_id: c.channel_id,
                            access_hash: c.access_hash,
                        }),
                        id: vec![id],
                    })
                    .await
                    .map_err(|e| telegram(e, "channels.deleteMessages"))?;
            }
            _ => {
                self.client
                    .invoke(&tl::functions::messages::DeleteMessages {
                        revoke: true,
                        id: vec![id],
                    })
                    .await
                    .map_err(|e| telegram(e, "messages.deleteMessages"))?;
            }
        }
        Ok(())
    }

    async fn server_sessions_since(&self, since: i64) -> Result<usize> {
        let tl::enums::account::Authorizations::Authorizations(all) = self
            .client
            .invoke(&tl::functions::account::GetAuthorizations {})
            .await
            .map_err(|e| telegram(e, "account.getAuthorizations"))?;
        Ok(all
            .authorizations
            .into_iter()
            .filter(|tl::enums::Authorization::Authorization(a)| {
                !a.current && a.device_model == SERVER_DEVICE && i64::from(a.date_created) >= since
            })
            .count())
    }

    async fn type_draft(&self, chat: &str, text: &str) -> Result<()> {
        self.client
            .invoke(&tl::functions::messages::SaveDraft {
                no_webpage: true,
                invert_media: false,
                reply_to: None,
                peer: input_peer(chat)?,
                message: text.to_string(),
                entities: None,
                media: None,
                effect: None,
                suggested_post: None,
                rich_message: None,
            })
            .await
            .map_err(|e| telegram(e, "messages.saveDraft"))?;
        Ok(())
    }
    async fn saved_draft(&self) -> Result<Option<String>> {
        let tl::enums::messages::PeerDialogs::Dialogs(found) = self
            .client
            .invoke(&tl::functions::messages::GetPeerDialogs {
                peers: vec![tl::enums::InputDialogPeer::Peer(
                    tl::types::InputDialogPeer {
                        peer: tl::enums::InputPeer::PeerSelf,
                    },
                )],
            })
            .await
            .map_err(|e| telegram(e, "messages.getPeerDialogs"))?;
        Ok(found.dialogs.into_iter().find_map(|d| match d {
            tl::enums::Dialog::Dialog(d) => match d.draft {
                Some(tl::enums::DraftMessage::Message(m)) if !m.message.is_empty() => {
                    Some(m.message)
                }
                _ => None,
            },
            _ => None,
        }))
    }

    async fn set_saved_draft(&self, text: &str) -> Result<()> {
        self.client
            .invoke(&tl::functions::messages::SaveDraft {
                no_webpage: true,
                invert_media: false,
                reply_to: None,
                peer: tl::enums::InputPeer::PeerSelf,
                message: text.to_string(),
                entities: None,
                media: None,
                effect: None,
                suggested_post: None,
                rich_message: None,
            })
            .await
            .map_err(|e| telegram(e, "messages.saveDraft"))?;
        Ok(())
    }

    async fn end_server_sessions(&self, since: i64) -> Result<usize> {
        let tl::enums::account::Authorizations::Authorizations(all) = self
            .client
            .invoke(&tl::functions::account::GetAuthorizations {})
            .await
            .map_err(|e| telegram(e, "account.getAuthorizations"))?;
        let mut ended = 0;
        for tl::enums::Authorization::Authorization(a) in all.authorizations {
            if a.current || a.device_model != SERVER_DEVICE || i64::from(a.date_created) < since {
                continue;
            }
            match self
                .client
                .invoke(&tl::functions::account::ResetAuthorization { hash: a.hash })
                .await
            {
                Ok(_) => ended += 1,
                Err(e) if e.is("FRESH_RESET_AUTHORISATION_FORBIDDEN") => bail!(
                    "Telegram doesn't let a login younger than 24 hours end other sessions; \
                     the run's Grants were revoked through /revoke anyway"
                ),
                Err(e) => return Err(telegram(e, "account.resetAuthorization")),
            }
        }
        Ok(ended)
    }
}

impl TgClient {
    /// The Test account as the Peer addresses it: their chat (fixture A) puts
    /// its user, with the access hash, among the Peer's dialogs.
    async fn test_account_peer(&self, test_account: i64) -> Result<tl::enums::InputPeer> {
        let answer = self
            .client
            .invoke(&tl::functions::messages::GetDialogs {
                exclude_pinned: false,
                folder_id: None,
                offset_date: 0,
                offset_id: 0,
                offset_peer: tl::enums::InputPeer::Empty,
                limit: 100,
                hash: 0,
            })
            .await
            .map_err(|e| telegram(e, "messages.getDialogs"))?;
        let users = match answer {
            tl::enums::messages::Dialogs::Dialogs(d) => d.users,
            tl::enums::messages::Dialogs::Slice(d) => d.users,
            tl::enums::messages::Dialogs::NotModified(_) => Vec::new(),
        };
        let access_hash = users
            .into_iter()
            .find_map(|u| match u {
                tl::enums::User::User(u) if u.id == test_account => u.access_hash,
                _ => None,
            })
            .context("the Peer has no chat with the Test account: run `cargo accept fixtures`")?;
        Ok(tl::enums::InputPeer::User(tl::types::InputPeerUser {
            user_id: test_account,
            access_hash,
        }))
    }
}

#[async_trait]
impl Peer for TgClient {
    async fn received_from_test_account(&self, test_account: i64, text: &str) -> Result<bool> {
        let history = self
            .client
            .invoke(&tl::functions::messages::GetHistory {
                peer: self.test_account_peer(test_account).await?,
                offset_id: 0,
                offset_date: 0,
                add_offset: 0,
                limit: 20,
                max_id: 0,
                min_id: 0,
                hash: 0,
            })
            .await
            .map_err(|e| telegram(e, "messages.getHistory"))?;
        let messages = match history {
            tl::enums::messages::Messages::Messages(m) => m.messages,
            tl::enums::messages::Messages::Slice(m) => m.messages,
            tl::enums::messages::Messages::ChannelMessages(m) => m.messages,
            tl::enums::messages::Messages::NotModified(_) => Vec::new(),
        };
        Ok(messages.iter().any(|m| match m {
            tl::enums::Message::Message(m) => !m.out && m.message == text,
            _ => false,
        }))
    }

    async fn say_to_test_account(&self, test_account: i64, text: &str) -> Result<()> {
        let peer = self.test_account_peer(test_account).await?;
        self.client
            .invoke(&tl::functions::messages::SendMessage {
                no_webpage: true,
                silent: true,
                background: false,
                clear_draft: false,
                noforwards: false,
                update_stickersets_order: false,
                invert_media: false,
                allow_paid_floodskip: false,
                peer,
                reply_to: None,
                message: text.to_string(),
                random_id: {
                    let mut b = [0u8; 8];
                    getrandom::fill(&mut b).expect("the OS has randomness");
                    i64::from_le_bytes(b)
                },
                reply_markup: None,
                entities: None,
                schedule_date: None,
                schedule_repeat_period: None,
                send_as: None,
                quick_reply_shortcut: None,
                effect: None,
                allow_paid_stars: None,
                suggested_post: None,
                rich_message: None,
            })
            .await
            .map_err(|e| telegram(e, "messages.sendMessage"))?;
        Ok(())
    }
}

/// `cargo accept sign-in driver|peer`: the first login of the Test account
/// (Driver) or the Peer, by the code Telegram sends, typed by a person. Stores
/// the login and the account id in the local file.
pub async fn sign_in(role: Role) -> Result<()> {
    let app = App::load()?;
    let (client, store) = connect(&app, SessionStore::new(), role);
    let phone = prompt(&format!(
        "Phone number of the {} account (international format): ",
        match role {
            Role::Driver => "Test",
            Role::Peer => "Peer",
        }
    ))?;
    let token = client
        .request_login_code(phone.trim(), &app.api_hash)
        .await
        .map_err(|e| telegram(e, "auth.sendCode"))?;
    let code = prompt("The code Telegram sent: ")?;
    let user = match client.sign_in(&token, code.trim()).await {
        Ok(user) => user,
        Err(SignInError::PasswordRequired(password_token)) => {
            let password = match config::get(config::CLOUD_PASSWORD)? {
                Some(p) if role == Role::Driver => p,
                _ => Zeroizing::new(prompt_hidden("Cloud password (not shown): ")?),
            };
            client
                .check_password(password_token, password.as_bytes())
                .await
                .map_err(|e| anyhow::anyhow!("the Cloud password was refused: {e}"))?
        }
        Err(SignInError::InvalidCode) => bail!("Telegram refused the code"),
        Err(e) => bail!("signing in failed: {e}"),
    };
    let id = user.raw.id();
    let snapshot = store
        .snapshot()
        .map_err(|e| anyhow::anyhow!("the new login can't be saved: {e}"))?;
    let encoded = Zeroizing::new(STANDARD.encode(snapshot.as_slice()));
    config::set_in_file(role.session_key(), &encoded)?;
    let path = config::set_in_file(role.id_key(), &id.to_string())?;
    client.disconnect();
    println!(
        "The {} is logged in as account {id}; its login is in {} (mode 600).",
        role.name(),
        path.display()
    );
    Ok(())
}

fn prompt(question: &str) -> Result<String> {
    print!("{question}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// Reads a line with the terminal's echo off (where `stty` exists).
fn prompt_hidden(question: &str) -> Result<String> {
    let stty = |arg: &str| {
        std::process::Command::new("stty")
            .arg(arg)
            .stdin(std::process::Stdio::inherit())
            .status()
            .is_ok_and(|s| s.success())
    };
    let hidden = stty("-echo");
    let line = prompt(question);
    if hidden {
        stty("echo");
        println!();
    }
    line
}
