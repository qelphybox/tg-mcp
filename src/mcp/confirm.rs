//! Delivering a message a tool sends (#45): what `send_message` (#46) calls
//! once it has checked and formatted its input. Only into the caller's Send
//! allow-list, and never the same text to the same Chat twice within a
//! minute of a send ([`crate::recent_sends`]); then:
//!
//! - a Grant with Auto-send sends at once (its Send allow-list was chosen at
//!   Consent, and only it may be written to);
//! - otherwise the message becomes a pending send ([`crate::confirm`]). A
//!   client that declared URL-mode elicitation is asked to open the
//!   confirmation page, and the call waits (at most until the pending send
//!   expires) for the user's decision: `sent`, `cancelled`, `drafted` or
//!   `expired`. Any other client gets `pending_confirmation` with the page's
//!   URL at once, for the model to hand to the user.
//!
//! Either way the send is the Live client's, so the Writes budget and the
//! Chat's Send cap count it when it goes to Telegram; a confirmed send they
//! refuse is an error saying when to retry.

use std::str::FromStr;

use rmcp::{
    ErrorData,
    model::{CustomNotification, ElicitationAction, ProtocolVersion, ServerNotification},
    schemars,
    service::{ElicitationMode, RequestContext, RoleServer},
};
use serde::Serialize;

use super::{TgMcp, tool_error};
use crate::{
    auth::Caller,
    confirm::{Outcome, PENDING_TTL, RequestError},
    recent_sends::{Duplicate, SendKey},
    tg::{
        chats::{CHAT_REF_MAX_LEN, rfc3339},
        send::NewMessage,
    },
};

/// What became of a message a tool sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SendStatus {
    /// Telegram took it: it is in the chat.
    Sent,
    /// The user cancelled it, or declined to look at it. Nothing was sent.
    Cancelled,
    /// The user made it a Draft: it is in the chat's message field, and
    /// only the user sends it.
    Drafted,
    /// Nobody decided within 10 minutes. Nothing was sent.
    Expired,
    /// It waits for the user to confirm it at `confirm_url` until
    /// `expires_at`: give the user the link. Whether it went out shows later
    /// in get_messages (sent) or get_drafts (made a Draft).
    PendingConfirmation,
}

/// What a tool that sends returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct SendResult {
    pub status: SendStatus,
    /// The chat, as given.
    #[schemars(length(max = CHAT_REF_MAX_LEN))]
    pub chat_id: String,
    /// `sent`: the new message's id in the chat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<i32>,
    /// `sent`: when Telegram dated it (UTC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 40))]
    pub date: Option<String>,
    /// `sent` or `drafted` in a forum: the Topic it went to (General is 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<i32>,
    /// `pending_confirmation`: the page where the user confirms it. Only the
    /// browser where the user connected this app can send from it; any other
    /// can only make it a Draft.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 2048))]
    pub confirm_url: Option<String>,
    /// `pending_confirmation`: when the link stops working (UTC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 40))]
    pub expires_at: Option<String>,
}

impl SendResult {
    fn new(status: SendStatus, message: &NewMessage) -> Self {
        Self {
            status,
            chat_id: message.chat.to_string(),
            message_id: None,
            date: None,
            topic: None,
            reply_to: None,
            confirm_url: None,
            expires_at: None,
        }
    }
}

/// What the client shows the user with the confirmation page. Never the
/// text or the Chat: those are on the page.
const ELICITATION_MESSAGE: &str = "Telegram: a message waits for your confirmation before it is \
                                   sent. Open the page to check it, then send it, cancel it or \
                                   make it a Draft.";

impl TgMcp {
    /// Send `message` for `caller` as its Grant allows: at once with
    /// Auto-send, else once the user confirms it (see the module docs).
    /// `context` is the tool call's: its client is asked to open the
    /// confirmation page when it can be ([`elicits_urls`]). The Chat must be
    /// in the caller's Send allow-list, and the same text must not have gone
    /// to it in the last minute.
    pub(super) async fn deliver(
        &self,
        context: &RequestContext<RoleServer>,
        caller: &Caller,
        message: NewMessage,
    ) -> Result<SendResult, ErrorData> {
        let peer = &context.peer;
        caller
            .may_write_to(message.chat.chat())
            .map_err(|refused| ErrorData::invalid_params(refused.to_string(), None))?;
        let key = self
            .recent_sends
            .key(caller.account_id, message.chat.chat(), &message.text);
        if caller
            .send_allow_list
            .as_ref()
            .is_some_and(|list| list.auto_send())
        {
            let claim = self
                .recent_sends
                .claim(key)
                .map_err(|duplicate| refuse_duplicate(caller, &key, duplicate))?;
            let sent = caller
                .telegram
                .send_message(message.clone())
                .await
                .map_err(tool_error)?;
            claim.sent(sent.id);
            tracing::info!(
                grant_id = %caller.grant_id,
                chat = %sent.chat,
                message_id = sent.id,
                "message sent without confirmation (auto-send)"
            );
            return outcome(&message, Outcome::Sent(sent));
        }
        self.recent_sends
            .check(&key)
            .map_err(|duplicate| refuse_duplicate(caller, &key, duplicate))?;
        let Some(confirmations) = &self.confirmations else {
            return Err(ErrorData::internal_error(
                "this server can't ask the user to confirm a message",
                None,
            ));
        };
        let requested = confirmations
            .request(caller, message.clone())
            .await
            .map_err(|err| match err {
                RequestError::Telegram(err) => tool_error(err),
                too_many @ RequestError::TooMany => {
                    ErrorData::internal_error(too_many.to_string(), None)
                }
            })?;
        let pending = || SendResult {
            confirm_url: Some(requested.url.clone()),
            expires_at: Some(requested.expires_at.clone()),
            ..SendResult::new(SendStatus::PendingConfirmation, &message)
        };
        if !elicits_urls(context) {
            return Ok(pending());
        }
        let Ok(url) = url::Url::from_str(&requested.url) else {
            return Ok(pending());
        };
        let elicitation_id = uuid::Uuid::new_v4().to_string();
        match peer
            .elicit_url_with_timeout(
                ELICITATION_MESSAGE,
                url,
                elicitation_id.clone(),
                Some(PENDING_TTL),
            )
            .await
        {
            Ok(ElicitationAction::Accept) => {
                let decided = confirmations.wait(requested).await;
                // Best effort: tells the client the page is done with.
                let complete = CustomNotification::new(
                    "notifications/elicitation/complete",
                    Some(serde_json::json!({ "elicitationId": elicitation_id })),
                );
                let _ = peer
                    .send_notification(ServerNotification::CustomNotification(complete))
                    .await;
                outcome(&message, decided)
            }
            Ok(_declined_or_dismissed) => {
                // Unless the user is pressing a button on the page right now.
                if confirmations.cancel(&requested) {
                    tracing::info!(grant_id = %caller.grant_id, "confirmation page declined in the client");
                    return outcome(&message, Outcome::Cancelled);
                }
                outcome(&message, confirmations.wait(requested).await)
            }
            Err(err) => {
                // The page still works: the model hands the link on.
                tracing::warn!(grant_id = %caller.grant_id, error = %err, "url elicitation failed; returning the confirmation link");
                Ok(pending())
            }
        }
    }
}

/// What a send of a message already sent says. Ids only in the log.
fn refuse_duplicate(caller: &Caller, key: &SendKey, duplicate: Duplicate) -> ErrorData {
    let message_id = match duplicate {
        Duplicate::Sent { id, .. } => Some(id),
        Duplicate::Sending { .. } => None,
    };
    tracing::info!(
        grant_id = %caller.grant_id,
        chat = %key.chat(),
        message_id,
        "same text to the same chat within a minute: not sent again"
    );
    ErrorData::invalid_params(duplicate.to_string(), None)
}

/// Can this call's client be asked to open a URL? It declared URL-mode
/// elicitation, and its protocol lets a server send it requests (2026-07-28
/// forbids them: such a client gets the link instead, never a request it
/// would leave unanswered).
fn elicits_urls(context: &RequestContext<RoleServer>) -> bool {
    context
        .protocol_version()
        .is_some_and(|version| version < ProtocolVersion::V_2026_07_28)
        && context
            .peer
            .supported_elicitation_modes()
            .contains(&ElicitationMode::Url)
}

/// The tool's answer for how a send ended. A confirmed send Telegram (or our
/// limits) refused is an error, saying when to retry.
fn outcome(message: &NewMessage, outcome: Outcome) -> Result<SendResult, ErrorData> {
    Ok(match outcome {
        Outcome::Sent(sent) => SendResult {
            message_id: Some(sent.id),
            date: Some(rfc3339(sent.date)),
            topic: sent.topic,
            reply_to: sent.reply_to,
            ..SendResult::new(SendStatus::Sent, message)
        },
        Outcome::Drafted(saved) => SendResult {
            topic: saved.topic,
            reply_to: saved.reply_to,
            ..SendResult::new(SendStatus::Drafted, message)
        },
        Outcome::Cancelled => SendResult::new(SendStatus::Cancelled, message),
        Outcome::Expired => SendResult::new(SendStatus::Expired, message),
        Outcome::Refused(refusal) => {
            return Err(ErrorData::internal_error(refusal.message, None));
        }
    })
}

#[cfg(test)]
mod tests;
