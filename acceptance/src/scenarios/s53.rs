//! `s53`: acceptance §53, Drafts. `save_draft` writes a formatted Draft that
//! Telegram holds as written, only with `draft`; `get_drafts` reads them back.
//!
//! Every Draft goes to the fixtures (A, B, the forum), never to Saved
//! Messages: Saved Messages holds the run's lease.

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use crate::{
    fixtures::Fixtures,
    run::{Run, Scenario, Session},
    world::SeenDraft,
};

pub const SCENARIO: Scenario = Scenario {
    id: "s53",
    title: "Drafts: formatted, replaced, cleared, in Topics, only with `draft`",
    run: |run| Box::pin(s53(run)),
};

/// The Markdown of §53 step 2: every kind of entity, after an emoji (two
/// UTF-16 units) that must not shift them.
const MARKDOWN: &str = "😀 **Release** _notes_: run `cargo test`, see [the PR](https://github.com/qelphybox/tg-mcp)\n```rust\nfn main() {}\n```";

async fn s53(run: &mut Run) -> Result<()> {
    // 1. Only a `draft` Grant has save_draft.
    let reader = run.login(&[]).await?;
    let fixtures = Fixtures::resolve(&reader, run.world.peer_account).await?;
    let tools = reader.tools().await?;
    ensure!(
        tools.iter().any(|t| t == "get_drafts") && !tools.iter().any(|t| t == "save_draft"),
        "a read Grant lists {tools:?}"
    );
    let before = run.world.driver.draft(&fixtures.a, None).await?;
    let refused = reader
        .try_call(
            "save_draft",
            json!({ "chat_id": fixtures.a, "text": run.mark("refused") }),
        )
        .await?;
    match refused {
        Err(message) if message.contains("needs the `draft` Scope") => {}
        other => bail!("save_draft without `draft`: {other:?}"),
    }
    ensure!(
        run.world.driver.draft(&fixtures.a, None).await? == before,
        "a refused save_draft changed A's Draft"
    );
    run.passed(
        "a read Grant lists get_drafts, not save_draft; calling it is refused and writes nothing",
    );

    let drafter = run.login(&["draft"]).await?;
    let tools = drafter.tools().await?;
    ensure!(
        tools.iter().any(|t| t == "save_draft") && tools.iter().any(|t| t == "get_drafts"),
        "a draft Grant lists {tools:?}"
    );
    run.passed("a draft Grant lists save_draft and get_drafts");

    // 2. Markdown becomes formatting; plain stays as typed.
    let text = run.mark(MARKDOWN);
    let saved = save(&drafter, json!({ "chat_id": fixtures.g, "text": text })).await?;
    ensure!(
        saved["status"] == "saved" && saved["entities"] == 5,
        "save_draft answered {saved}"
    );
    let seen = held(run, &fixtures.g, None).await?;
    check_markdown(&seen)?;
    run.passed("Markdown: Telegram holds bold, italic, code, a link and a code block on exactly their words");
    run.human(
        "Group G (tgmcp-accept · G) on the phone and on Telegram Desktop: is **Release** bold, _notes_ italic, `cargo test` monospace, \"the PR\" a link and `fn main() {}` a code block?",
        "yes, on both",
    );
    run.human(
        "Send that Draft from the phone: does the sent message keep the formatting, and does \"the PR\" open github.com/qelphybox/tg-mcp?",
        "yes (then delete the sent message)",
    );

    let plain = run.mark("**not bold** _not italic_");
    let saved = save(
        &drafter,
        json!({ "chat_id": fixtures.b, "text": plain, "format": "plain" }),
    )
    .await?;
    ensure!(saved["entities"] == 0, "plain save_draft answered {saved}");
    let seen = held(run, &fixtures.b, None).await?;
    ensure!(
        seen.text == plain && seen.entities.is_empty(),
        "plain: Telegram holds {seen:?}"
    );
    run.passed("format plain: the asterisks and underscores stay as typed, no formatting");

    // 3. A Draft replaces the one there, and "" clears it.
    for word in ["first", "second"] {
        save(
            &drafter,
            json!({ "chat_id": fixtures.b, "text": run.mark(word) }),
        )
        .await?;
    }
    let seen = held(run, &fixtures.b, None).await?;
    ensure!(
        seen.text.starts_with("second "),
        "after first then second, B holds {:?}",
        seen.text
    );
    let listed = drafts(&drafter).await?;
    let in_b: Vec<_> = listed
        .iter()
        .filter(|d| d["chat"]["id"] == json!(fixtures.b))
        .collect();
    ensure!(
        in_b.len() == 1
            && in_b[0]["text"]
                .as_str()
                .unwrap_or("")
                .starts_with("second "),
        "get_drafts lists for B: {in_b:?}"
    );
    let cleared = save(&drafter, json!({ "chat_id": fixtures.b, "text": "" })).await?;
    ensure!(
        cleared["status"] == "cleared",
        "clearing answered {cleared}"
    );
    ensure!(
        run.world.driver.draft(&fixtures.b, None).await?.is_none(),
        "B still holds a Draft after clearing"
    );
    ensure!(
        !drafts(&drafter)
            .await?
            .iter()
            .any(|d| d["chat"]["id"] == json!(fixtures.b)),
        "get_drafts still lists B after clearing"
    );
    run.passed("a Draft replaces the one there; \"\" clears it, in Telegram and in get_drafts");

    // 4. reply_to.
    run.world
        .peer
        .say_to_test_account(run.world.test_account, &run.mark("the Peer asks"))
        .await?;
    let peer_says = newest_from(&drafter, &fixtures.a, run.world.peer_account, None).await?;
    let about = run.mark("about this");
    save(
        &drafter,
        json!({ "chat_id": fixtures.a, "text": about, "reply_to": peer_says }),
    )
    .await?;
    let seen = held(run, &fixtures.a, None).await?;
    ensure!(
        seen.reply_to.map(i64::from) == Some(peer_says),
        "the Draft replies to {:?}, not to {peer_says}",
        seen.reply_to
    );
    let listed = drafts(&drafter).await?;
    ensure!(
        listed
            .iter()
            .any(|d| d["chat"]["id"] == json!(fixtures.a) && d["reply_to"] == json!(peer_says)),
        "get_drafts doesn't show A's reply_to {peer_says}: {listed:?}"
    );
    run.passed("reply_to: Telegram holds the reply, get_drafts shows it");
    run.human(
        "Chat A's message field on the phone: is there a reply bar quoting the Peer's latest message?",
        "yes",
    );

    // 5. A forum needs a Topic.
    let forum = &fixtures.forum;
    let releases = fixtures.releases;
    let no_topic = drafter
        .try_call(
            "save_draft",
            json!({ "chat_id": forum, "text": run.mark("x") }),
        )
        .await?;
    match no_topic {
        Err(m) if m.contains("is a forum") => {}
        other => bail!("save_draft in a forum without a Topic: {other:?}"),
    }
    ensure!(
        run.world.driver.draft(forum, None).await?.is_none(),
        "a refused forum Draft was written"
    );
    let in_releases = run.mark("in releases");
    let saved = save(
        &drafter,
        json!({ "chat_id": forum, "topic": releases, "text": in_releases }),
    )
    .await?;
    ensure!(saved["topic"] == json!(releases), "answered {saved}");
    let in_general = run.mark("in general");
    save(
        &drafter,
        json!({ "chat_id": forum, "topic": 1, "text": in_general }),
    )
    .await?;
    let r = held(run, forum, Some(releases as i32)).await?;
    let g = held(run, forum, Some(1)).await?;
    ensure!(
        r.text == in_releases && g.text == in_general,
        "Releases holds {:?}, General {:?}",
        r.text,
        g.text
    );
    run.passed("topic: each Topic holds its own Draft");

    let in_topic = newest_in_topic(&drafter, forum, releases).await?;
    let a_reply = run.mark("a reply");
    let saved = save(
        &drafter,
        json!({ "chat_id": forum, "reply_to": in_topic, "text": a_reply }),
    )
    .await?;
    ensure!(
        saved["topic"] == json!(releases),
        "reply_to alone answered {saved}"
    );
    let r = held(run, forum, Some(releases as i32)).await?;
    ensure!(r.text == a_reply, "Releases holds {:?}", r.text);
    run.passed("reply_to alone picks the reply's Topic");

    let in_general_msg = newest_in_topic(&drafter, forum, 1).await?;
    let mismatch = drafter
        .try_call(
            "save_draft",
            json!({ "chat_id": forum, "topic": releases, "reply_to": in_general_msg, "text": run.mark("x") }),
        )
        .await?;
    match mismatch {
        Err(m) if m.contains("is in Topic") => {}
        other => bail!("a reply_to in another Topic: {other:?}"),
    }
    let not_forum = drafter
        .try_call(
            "save_draft",
            json!({ "chat_id": fixtures.b, "topic": 1, "text": run.mark("x") }),
        )
        .await?;
    match not_forum {
        Err(m) if m.contains("is not a forum") => {}
        other => bail!("topic on a group that is no forum: {other:?}"),
    }
    let listed = drafts(&drafter).await?;
    let topics: Vec<_> = listed
        .iter()
        .filter(|d| d["chat"]["id"] == json!(forum))
        .map(|d| d["topic"].as_i64().unwrap_or(1))
        .collect();
    ensure!(
        topics.contains(&releases) && topics.contains(&1),
        "get_drafts lists the forum's Drafts in Topics {topics:?}"
    );
    run.passed("a reply_to in another Topic and topic outside a forum are refused; get_drafts names each Topic");

    // 6. Too long is an error, never split.
    let before = run.world.driver.draft(&fixtures.a, None).await?;
    let long = drafter
        .try_call(
            "save_draft",
            json!({ "chat_id": fixtures.a, "text": run.mark(&"lorem ".repeat(900)), "format": "plain" }),
        )
        .await?;
    match long {
        Err(m) if m.contains("4096") && m.contains("not split") => {}
        other => bail!("a 5400-character Draft: {other:?}"),
    }
    ensure!(
        run.world.driver.draft(&fixtures.a, None).await? == before,
        "a refused long Draft changed A's Draft"
    );
    run.passed("over 4096 characters: an error naming the length; A's Draft unchanged");

    // 7. get_drafts lists them newest first, a Draft typed elsewhere too.
    let typed = run.mark("typed on the phone");
    run.world.driver.type_draft(&fixtures.b, &typed).await?;
    let listed = drafts(&drafter).await?;
    ensure!(
        listed.first().and_then(|d| d["text"].as_str()) == Some(typed.as_str()),
        "the newest Draft (typed elsewhere) is not first: {listed:?}"
    );
    let dates: Vec<&str> = listed.iter().filter_map(|d| d["date"].as_str()).collect();
    ensure!(
        dates.windows(2).all(|w| w[0] >= w[1]),
        "get_drafts is not newest first: {dates:?}"
    );
    ensure!(
        listed.iter().any(|d| d["chat"]["is_self"] == json!(true)
            && d["text"]
                .as_str()
                .unwrap_or("")
                .starts_with("tgmcp-accept lease ")),
        "Saved Messages (holding the lease) is not listed with is_self: {listed:?}"
    );
    run.passed(
        "get_drafts: newest first, a Draft typed elsewhere included, Saved Messages marked is_self",
    );

    // 8 is the run's log and database check; 9: clean up.
    clean_up(run, &drafter, &fixtures).await
}

async fn save(session: &Session, args: Value) -> Result<Value> {
    session.call("save_draft", args).await
}

async fn drafts(session: &Session) -> Result<Vec<Value>> {
    let answer = session.call("get_drafts", json!({})).await?;
    Ok(answer["drafts"].as_array().cloned().unwrap_or_default())
}

/// The Draft Telegram holds in `chat` (and `topic`), which must exist.
async fn held(run: &Run, chat: &str, topic: Option<i32>) -> Result<SeenDraft> {
    run.world
        .driver
        .draft(chat, topic)
        .await?
        .with_context(|| format!("Telegram holds no Draft in {chat} (topic {topic:?})"))
}

fn check_markdown(seen: &SeenDraft) -> Result<()> {
    let want = [
        ("bold", "Release"),
        ("italic", "notes"),
        ("code", "cargo test"),
        ("text_url", "the PR"),
        ("pre", "fn main() {}"),
    ];
    for (kind, words) in want {
        let entity = seen
            .entities
            .iter()
            .find(|e| e.kind == kind)
            .with_context(|| format!("Telegram holds no {kind} entity ({:?})", seen.entities))?;
        let covered = entity.covers(&seen.text);
        ensure!(
            covered.trim_end_matches('\n') == words,
            "the {kind} entity covers {covered:?}, not {words:?}"
        );
    }
    let link = seen.entities.iter().find(|e| e.kind == "text_url");
    ensure!(
        link.and_then(|l| l.url.as_deref()) == Some("https://github.com/qelphybox/tg-mcp"),
        "the link goes to {:?}",
        link.and_then(|l| l.url.clone())
    );
    ensure!(
        !seen.text.contains("**") && !seen.text.contains("```"),
        "Markdown marks left in the text: {:?}",
        seen.text
    );
    Ok(())
}

/// The newest message `from` sent in `chat`.
async fn newest_from(session: &Session, chat: &str, from: i64, topic: Option<i64>) -> Result<i64> {
    let mut args = json!({ "chat_id": chat, "limit": 20 });
    if let Some(t) = topic {
        args["topic"] = json!(t);
    }
    let page = session.call("get_messages", args).await?;
    page["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| {
            m["sender"]["id"]
                .as_str()
                .is_some_and(|s| s.starts_with(&format!("user:{from}")))
        })
        .and_then(|m| m["id"].as_i64())
        .with_context(|| format!("no message from {from} in {chat}"))
}

/// The newest message in a forum Topic (1: General).
async fn newest_in_topic(session: &Session, forum: &str, topic: i64) -> Result<i64> {
    let page = session
        .call(
            "get_messages",
            json!({ "chat_id": forum, "topic": topic, "limit": 5 }),
        )
        .await?;
    page["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["id"].as_i64())
        .find(|id| *id != topic)
        .with_context(|| format!("no message in Topic {topic} of {forum}"))
}

/// Clears the Drafts in B and the forum. The Human checks are asked at the
/// end of the run, so the formatted Draft in G and the reply in A stay until
/// the person has looked at them.
async fn clean_up(run: &mut Run, drafter: &Session, fixtures: &Fixtures) -> Result<()> {
    save(drafter, json!({ "chat_id": fixtures.b, "text": "" })).await?;
    for topic in [fixtures.releases, 1] {
        save(
            drafter,
            json!({ "chat_id": fixtures.forum, "topic": topic, "text": "" }),
        )
        .await?;
    }
    run.passed("cleared the Drafts in B and the forum; G and A keep theirs for the Human checks");
    run.human(
        "When the checks above are done: clear the Drafts in G and in A on the phone",
        "both message fields are empty",
    );
    Ok(())
}
