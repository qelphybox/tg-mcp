//! The Test account's test Chats (ADR-0009), recognised by a title prefix:
//! created and repaired by `cargo accept fixtures`, found by role by scenarios.

use anyhow::{Context, Result, ensure};
use grammers_client::tl;
use serde_json::{Value, json};

use crate::{
    run::McpSession,
    telegram::{TgClient, telegram},
};

/// Every fixture's title starts with this.
pub const PREFIX: &str = "tgmcp-accept · ";
pub const B: &str = "tgmcp-accept · B";
pub const FORUM: &str = "tgmcp-accept · forum";
pub const G: &str = "tgmcp-accept · G";
pub const WORK: &str = "tgmcp-accept · Work";
/// The forum's Topic besides General.
pub const RELEASES: &str = "Releases";
/// The one reaction G allows.
pub const G_REACTION: &str = "👍";
/// What the Peer first writes in A.
pub const A_HELLO: &str = "tgmcp-accept · fixture A";
/// The message in the forum's Releases Topic.
pub const RELEASE_NOTES: &str = "tgmcp-accept · release notes";

/// G's `n`th message.
pub fn g_message(n: i32) -> String {
    format!("tgmcp-accept · G message {n}")
}

/// The fixtures by role, as the server names them.
#[derive(Debug, Clone)]
pub struct Fixtures {
    /// The private chat with the Peer.
    pub a: String,
    /// A group with the Peer.
    pub b: String,
    /// A forum supergroup.
    pub forum: String,
    /// The forum's "Releases" Topic id.
    pub releases: i64,
    /// A group the Test account administers, with 👍 only.
    pub g: String,
    /// The Chat folder holding A and the forum.
    pub work: i64,
    /// The Test account's Saved Messages.
    pub saved: String,
}

impl Fixtures {
    /// Finds every fixture through the server, as a scenario sees them.
    pub async fn resolve(session: &McpSession, peer: i64) -> Result<Fixtures> {
        let mut a = None;
        let mut saved = None;
        let mut by_title = std::collections::HashMap::new();
        let mut cursor: Option<String> = None;
        for _ in 0..10 {
            let mut args = json!({ "limit": 100 });
            if let Some(c) = &cursor {
                args["cursor"] = json!(c);
            }
            let page = session.call("list_chats", args).await?;
            for chat in page["chats"].as_array().into_iter().flatten() {
                let id = chat["id"].as_str().unwrap_or_default().to_string();
                if id.starts_with(&format!("user:{peer}~")) || id == format!("user:{peer}") {
                    a = Some(id.clone());
                }
                if chat["is_self"] == json!(true) {
                    saved = Some(id.clone());
                }
                if let Some(title) = chat["title"].as_str()
                    && title.starts_with(PREFIX)
                {
                    by_title.insert(title.to_string(), id);
                }
            }
            cursor = page["next_cursor"].as_str().map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        let missing =
            |what: &str| format!("fixture {what} is missing: run `cargo accept fixtures`");
        let take = |title: &str| by_title.get(title).cloned().with_context(|| missing(title));
        let forum = take(FORUM)?;
        let info = session
            .call("get_chat_info", json!({ "chat_id": forum }))
            .await?;
        let releases = info["topics"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|t| t["title"] == json!(RELEASES))
            .and_then(|t| t["id"].as_i64())
            .with_context(|| missing("forum Topic Releases"))?;
        let me = session.call("whoami", json!({})).await?;
        let work = me["chat_folders"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|f| f["title"] == json!(WORK))
            .and_then(|f| f["id"].as_i64())
            .with_context(|| missing(WORK))?;
        Ok(Fixtures {
            a: a.with_context(|| missing("A (the chat with the Peer)"))?,
            b: take(B)?,
            forum,
            releases,
            g: take(G)?,
            work,
            saved: saved.context("Saved Messages is not in the chat list")?,
        })
    }
}

// ── Creating and repairing them on real Telegram ───────────────────────────

/// A group or supergroup among the Test account's dialogs.
struct FixtureChat {
    title: String,
    peer: tl::enums::InputPeer,
    forum: bool,
    /// A basic group's id (for messages.getFullChat).
    chat_id: Option<i64>,
}

/// Makes sure every fixture exists and is as described; returns what it did
/// (empty: nothing to do).
pub async fn ensure(driver: &TgClient, peer: &TgClient) -> Result<Vec<String>> {
    let mut done = Vec::new();
    let me = driver.self_user().await?;
    let them = peer.self_user().await?;
    let me_name = me.username.clone().context(
        "the Test account has no username: set one in Telegram → Settings → Username (the Peer finds it by name)",
    )?;
    let them_name = them.username.clone().context(
        "the Peer has no username: set one in Telegram → Settings → Username (the Test account finds it by name)",
    )?;
    let peer_user = resolve_user(driver, &them_name).await?;
    let peer_input_peer = input_peer_of(&peer_user);
    let peer_input_user = input_user_of(&peer_user);

    // A: the Peer writes first, so the Test account never writes to a stranger.
    if !has_dialog_with(driver, them.id).await? {
        let me_from_peer = resolve_user(peer, &me_name).await?;
        peer.send_text(
            input_peer_of(&me_from_peer),
            "tgmcp-accept · fixture A",
            None,
        )
        .await?;
        done.push("A: the Peer wrote to the Test account".into());
    }

    let mut chats = dialogs(driver).await?;

    if !chats.iter().any(|c| c.title == B) {
        create_group(driver, B, peer_input_user.clone()).await?;
        done.push(format!("created {B}"));
    }
    if !chats.iter().any(|c| c.title == G) {
        create_group(driver, G, peer_input_user.clone()).await?;
        done.push(format!("created {G}"));
    }
    if !chats.iter().any(|c| c.title == FORUM) {
        create_forum(driver, peer_input_user.clone()).await?;
        done.push(format!("created {FORUM}"));
    }
    if done.iter().any(|d| d.starts_with("created")) {
        chats = dialogs(driver).await?;
    }

    let g = chats
        .iter()
        .find(|c| c.title == G)
        .context("G wasn't created")?;
    done.extend(repair_g(driver, g).await?);
    let forum = chats
        .iter()
        .find(|c| c.title == FORUM)
        .context("the forum wasn't created")?;
    ensure!(
        forum.forum,
        "{FORUM} exists but has no Topics: delete it in Telegram and run again"
    );
    done.extend(repair_forum(driver, forum).await?);
    done.extend(repair_work(driver, &[peer_input_peer, forum.peer.clone()]).await?);
    Ok(done)
}

async fn resolve_user(client: &TgClient, username: &str) -> Result<tl::types::User> {
    let tl::enums::contacts::ResolvedPeer::Peer(found) = client
        .client
        .invoke(&tl::functions::contacts::ResolveUsername {
            username: username.to_string(),
            referer: None,
        })
        .await
        .map_err(|e| telegram(e, "contacts.resolveUsername"))?;
    found
        .users
        .into_iter()
        .find_map(|u| match u {
            tl::enums::User::User(u) => Some(u),
            _ => None,
        })
        .with_context(|| format!("@{username} is not a user"))
}

fn input_peer_of(user: &tl::types::User) -> tl::enums::InputPeer {
    tl::enums::InputPeer::User(tl::types::InputPeerUser {
        user_id: user.id,
        access_hash: user.access_hash.unwrap_or(0),
    })
}

fn input_user_of(user: &tl::types::User) -> tl::enums::InputUser {
    tl::enums::InputUser::User(tl::types::InputUser {
        user_id: user.id,
        access_hash: user.access_hash.unwrap_or(0),
    })
}

async fn has_dialog_with(client: &TgClient, user_id: i64) -> Result<bool> {
    Ok(client.dialogs().await?.dialogs.iter().any(|d| match d {
        tl::enums::Dialog::Dialog(d) => {
            matches!(&d.peer, tl::enums::Peer::User(u) if u.user_id == user_id)
        }
        _ => false,
    }))
}

/// The groups and supergroups among the dialogs, with their titles.
async fn dialogs(client: &TgClient) -> Result<Vec<FixtureChat>> {
    Ok(client
        .dialogs()
        .await?
        .chats
        .into_iter()
        .filter_map(|c| match c {
            tl::enums::Chat::Chat(c) if !c.deactivated => Some(FixtureChat {
                title: c.title,
                peer: tl::enums::InputPeer::Chat(tl::types::InputPeerChat { chat_id: c.id }),
                forum: false,
                chat_id: Some(c.id),
            }),
            tl::enums::Chat::Channel(c) if !c.left => Some(FixtureChat {
                title: c.title,
                peer: tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
                    channel_id: c.id,
                    access_hash: c.access_hash.unwrap_or(0),
                }),
                forum: c.forum,
                chat_id: None,
            }),
            _ => None,
        })
        .collect())
}

async fn create_group(client: &TgClient, title: &str, with: tl::enums::InputUser) -> Result<()> {
    client
        .client
        .invoke(&tl::functions::messages::CreateChat {
            users: vec![with],
            title: title.to_string(),
            ttl_period: None,
        })
        .await
        .map_err(|e| telegram(e, "messages.createChat"))?;
    Ok(())
}

async fn create_forum(client: &TgClient, with: tl::enums::InputUser) -> Result<()> {
    client
        .client
        .invoke(&tl::functions::channels::CreateChannel {
            broadcast: false,
            megagroup: true,
            for_import: false,
            forum: true,
            title: FORUM.to_string(),
            about: "tg-mcp acceptance fixture (ADR-0009)".into(),
            geo_point: None,
            address: None,
            ttl_period: None,
        })
        .await
        .map_err(|e| telegram(e, "channels.createChannel"))?;
    let forum = dialogs(client)
        .await?
        .into_iter()
        .find(|c| c.title == FORUM)
        .context("the new forum is not among the dialogs")?;
    if let Some(channel) = crate::telegram::input_channel(&forum.peer) {
        client
            .client
            .invoke(&tl::functions::channels::InviteToChannel {
                channel,
                users: vec![with],
            })
            .await
            .map_err(|e| telegram(e, "channels.inviteToChannel"))?;
    }
    Ok(())
}

/// G allows 👍 only and has a few messages.
async fn repair_g(client: &TgClient, g: &FixtureChat) -> Result<Vec<String>> {
    let mut done = Vec::new();
    let chat_id = g.chat_id.context("G is not a basic group")?;
    let tl::enums::messages::ChatFull::Full(full) = client
        .client
        .invoke(&tl::functions::messages::GetFullChat { chat_id })
        .await
        .map_err(|e| telegram(e, "messages.getFullChat"))?;
    let only_thumbs_up = match full.full_chat {
        tl::enums::ChatFull::Full(f) => matches!(
            f.available_reactions,
            Some(tl::enums::ChatReactions::Some(ref some))
                if some.reactions.len() == 1
                    && matches!(&some.reactions[0],
                        tl::enums::Reaction::Emoji(e) if e.emoticon == G_REACTION)
        ),
        _ => false,
    };
    if !only_thumbs_up {
        client
            .client
            .invoke(&tl::functions::messages::SetChatAvailableReactions {
                peer: g.peer.clone(),
                available_reactions: tl::enums::ChatReactions::Some(tl::types::ChatReactionsSome {
                    reactions: vec![tl::enums::Reaction::Emoji(tl::types::ReactionEmoji {
                        emoticon: G_REACTION.into(),
                    })],
                }),
                reactions_limit: None,
                paid_enabled: None,
            })
            .await
            .map_err(|e| telegram(e, "messages.setChatAvailableReactions"))?;
        done.push(format!("{G}: reactions set to {G_REACTION} only"));
    }
    let count = history_count(client, g.peer.clone()).await?;
    for n in count..3 {
        client
            .send_text(g.peer.clone(), &g_message(n as i32 + 1), None)
            .await?;
        done.push(format!("{G}: posted message {}", n + 1));
    }
    Ok(done)
}

/// How many messages a chat has, up to 10 (service messages count).
async fn history_count(client: &TgClient, peer: tl::enums::InputPeer) -> Result<usize> {
    Ok(client
        .history(peer, 10)
        .await?
        .iter()
        .filter(|m| matches!(m, tl::enums::Message::Message(_)))
        .count())
}

/// The forum has a Topic "Releases" (the first Topic besides General, renamed
/// back if someone renamed it) with a message in it.
async fn repair_forum(client: &TgClient, forum: &FixtureChat) -> Result<Vec<String>> {
    let other = client
        .forum_topics(forum.peer.clone())
        .await?
        .into_iter()
        .filter(|t| t.id != 1)
        .min_by_key(|t| t.id);
    match other {
        Some(t) if t.title == RELEASES => Ok(Vec::new()),
        Some(t) => {
            client
                .client
                .invoke(&tl::functions::messages::EditForumTopic {
                    peer: forum.peer.clone(),
                    topic_id: t.id,
                    title: Some(RELEASES.into()),
                    icon_emoji_id: None,
                    closed: None,
                    hidden: None,
                })
                .await
                .map_err(|e| telegram(e, "messages.editForumTopic"))?;
            Ok(vec![format!("{FORUM}: Topic renamed back to {RELEASES}")])
        }
        None => {
            client
                .client
                .invoke(&tl::functions::messages::CreateForumTopic {
                    title_missing: false,
                    peer: forum.peer.clone(),
                    title: RELEASES.into(),
                    icon_color: None,
                    icon_emoji_id: None,
                    random_id: crate::telegram::random_id(),
                    send_as: None,
                })
                .await
                .map_err(|e| telegram(e, "messages.createForumTopic"))?;
            let created = repair_forum_topic_id(client, forum).await?;
            client
                .send_text(forum.peer.clone(), RELEASE_NOTES, Some(created))
                .await?;
            Ok(vec![format!(
                "{FORUM}: created Topic {RELEASES} with a message"
            )])
        }
    }
}

async fn repair_forum_topic_id(client: &TgClient, forum: &FixtureChat) -> Result<i32> {
    client
        .forum_topics(forum.peer.clone())
        .await?
        .into_iter()
        .find(|t| t.title == RELEASES)
        .map(|t| t.id)
        .context("the new Topic is not listed")
}

/// The Chat folder Work: an explicit list holding A and the forum.
async fn repair_work(client: &TgClient, members: &[tl::enums::InputPeer]) -> Result<Vec<String>> {
    let tl::enums::messages::DialogFilters::Filters(all) = client
        .client
        .invoke(&tl::functions::messages::GetDialogFilters {})
        .await
        .map_err(|e| telegram(e, "messages.getDialogFilters"))?;
    let mut used = Vec::new();
    let mut existing = None;
    for f in all.filters {
        if let tl::enums::DialogFilter::Filter(f) = f {
            used.push(f.id);
            let tl::enums::TextWithEntities::Entities(title) = &f.title;
            if title.text == WORK {
                existing = Some(f);
            }
        }
    }
    let same = |a: &tl::enums::InputPeer, b: &tl::enums::InputPeer| peer_key(a) == peer_key(b);
    let (id, mut include) = match &existing {
        Some(f) => (f.id, f.include_peers.clone()),
        None => (
            (2..=255)
                .find(|id| !used.contains(id))
                .context("no free Chat folder id")?,
            Vec::new(),
        ),
    };
    let missing: Vec<_> = members
        .iter()
        .filter(|m| !include.iter().any(|i| same(i, m)))
        .cloned()
        .collect();
    if existing.is_some() && missing.is_empty() {
        return Ok(Vec::new());
    }
    include.extend(missing);
    client
        .client
        .invoke(&tl::functions::messages::UpdateDialogFilter {
            id,
            filter: Some(tl::enums::DialogFilter::Filter(tl::types::DialogFilter {
                contacts: false,
                non_contacts: false,
                groups: false,
                broadcasts: false,
                bots: false,
                exclude_muted: false,
                exclude_read: false,
                exclude_archived: false,
                title_noanimate: false,
                id,
                title: tl::enums::TextWithEntities::Entities(tl::types::TextWithEntities {
                    text: WORK.into(),
                    entities: Vec::new(),
                }),
                emoticon: None,
                color: None,
                pinned_peers: Vec::new(),
                include_peers: include,
                exclude_peers: Vec::new(),
            })),
        })
        .await
        .map_err(|e| telegram(e, "messages.updateDialogFilter"))?;
    Ok(vec![if existing.is_some() {
        format!("{WORK}: added the missing chats")
    } else {
        format!("created {WORK}")
    }])
}

/// What identifies a peer, whatever its access hash.
fn peer_key(peer: &tl::enums::InputPeer) -> Option<(u8, i64)> {
    match peer {
        tl::enums::InputPeer::User(u) => Some((0, u.user_id)),
        tl::enums::InputPeer::Chat(c) => Some((1, c.chat_id)),
        tl::enums::InputPeer::Channel(c) => Some((2, c.channel_id)),
        _ => None,
    }
}

/// For tests and reports: the chat ids of fixtures as JSON.
pub fn describe(f: &Fixtures) -> Value {
    json!({
        "a": f.a, "b": f.b, "forum": f.forum, "releases": f.releases,
        "g": f.g, "work": f.work, "saved": f.saved,
    })
}
