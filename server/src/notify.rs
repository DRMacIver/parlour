//! Who hears about what (DESIGN.md §9): in-app notifications, and email
//! for members who are away.
//!
//! A new post queues a `notify` job in the same transaction that writes
//! it. The job works out who might want to know:
//!
//! - in a conversation, everyone else in it who is subscribed to it (a
//!   `message`): everyone in a conversation is, until they unsubscribe;
//! - in a space thread, members mentioned (`mention`) and members
//!   subscribed to the thread (`subscription`): its author, and anyone who
//!   subscribed from its page. Replying subscribes nobody, and nothing
//!   subscribes anyone to a space.
//!
//! Each member gets at most one notification per post, a mention rather
//! than a subscription. Nobody is notified of their own posts, or of posts
//! by someone they blocked (unless that someone is staff). And nobody is
//! notified of a post they can't read: each candidate is checked by asking
//! the access layer for the post as them. Muting a space takes it off a
//! member's lists (DESIGN.md §3.5), not out of their notifications.
//!
//! Each new notification also queues an email job, due once the site's
//! delay has passed. When it runs it sends one email covering all of the
//! member's notifications that are still unread, haven't been emailed, are
//! about posts they can still read, aren't from someone they have since
//! blocked or (for new posts and messages) in a thread they have since
//! unsubscribed from, are of kinds they want emailed, and are newer than the
//! last time the member was seen on the site.

use std::collections::BTreeMap;

use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{Duration, Utc};
use serde_json::json;

use crate::AppState;
use crate::content::{self, notifications::Kind};
use crate::db::Handle;
use crate::emails;
use crate::jobs;
use crate::live::Event;
use crate::mail;
use crate::notifications::{self as stored, PostFacts};
use crate::sessions;
use crate::settings;
use crate::social::{self, EmailTopic};
use crate::store;
use crate::tokens;
use crate::viewer::Viewer;

pub const POST_JOB: &str = "notify";
pub const EMAIL_JOB: &str = "notification_email";

/// Queues the notifications about a new post. Call in the transaction
/// that writes the post, and wake the job workers after committing.
pub async fn queue_post<'c>(db: impl Handle<'c>, post_id: i64) -> sqlx::Result<()> {
    jobs::enqueue(db, POST_JOB, &json!({ "post_id": post_id })).await?;
    Ok(())
}

pub async fn run_post_job(state: AppState, payload: serde_json::Value) -> anyhow::Result<()> {
    let post_id = payload["post_id"]
        .as_i64()
        .context("notify job without a post_id")?;
    post_created(&state, post_id).await?;
    Ok(())
}

/// Which kind wins when one post concerns a member in several ways.
fn rank(kind: Kind) -> u8 {
    match kind {
        Kind::Message => 3,
        Kind::Mention => 2,
        Kind::Subscription => 1,
        Kind::Moderation => 0,
    }
}

fn consider(candidates: &mut BTreeMap<i64, Kind>, user_id: i64, kind: Kind) {
    candidates
        .entry(user_id)
        .and_modify(|k| {
            if rank(kind) > rank(*k) {
                *k = kind
            }
        })
        .or_insert(kind);
}

/// Who should be told about a post, before checking that they may read it.
async fn candidates(state: &AppState, post: &PostFacts) -> sqlx::Result<BTreeMap<i64, Kind>> {
    let db = state.db();
    let mut candidates = BTreeMap::new();
    let subscribers = stored::subscribers(db, post.thread_id).await?;
    match post.space_id {
        None => {
            for user in stored::participants(db, post.thread_id).await? {
                if subscribers.contains(&user) {
                    consider(&mut candidates, user, Kind::Message);
                }
            }
        }
        Some(_) => {
            for user in subscribers {
                consider(&mut candidates, user, Kind::Subscription);
            }
            let mentioned = store::render_post(db, &post.body_md).await?.mentions;
            for user in stored::ids_named(db, &mentioned).await? {
                consider(&mut candidates, user, Kind::Mention);
            }
        }
    }
    candidates.remove(&post.author_id);
    if !post.author_is_staff {
        for user in stored::blocked_by(db, post.author_id).await? {
            candidates.remove(&user);
        }
    }
    Ok(candidates)
}

/// The access layer's view for a member, as they are now.
async fn viewer_for(state: &AppState, user_id: i64) -> sqlx::Result<Option<Viewer>> {
    Ok(sessions::user_ctx(state.db(), user_id)
        .await?
        .map(Viewer::User))
}

/// Makes the notifications about a new post. Returns the members notified.
/// Safe to run more than once: a member never gets two notifications
/// about one post.
pub async fn post_created(state: &AppState, post_id: i64) -> sqlx::Result<Vec<i64>> {
    let db = state.db();
    let Some(post) = stored::post_facts(db, post_id).await? else {
        return Ok(Vec::new());
    };
    if !post.live {
        return Ok(Vec::new());
    }
    let mut notified = Vec::new();
    for (user_id, kind) in candidates(state, &post).await? {
        let Some(viewer) = viewer_for(state, user_id).await? else {
            continue;
        };
        let readable = content::post_target(db, &viewer, post_id)
            .await?
            .is_some_and(|p| p.is_live());
        if !readable {
            continue;
        }
        if let Some(id) =
            stored::insert(db, user_id, kind, post.author_id, post.thread_id, post_id).await?
        {
            notified.push(user_id);
            created(state, user_id, id).await?;
        }
    }
    Ok(notified)
}

/// After a notification is made: update the member's open pages, and plan
/// an email for if they don't see it.
async fn created(state: &AppState, user_id: i64, notification_id: i64) -> sqlx::Result<()> {
    state.live().publish(Event::Counts { user_id });
    let delay = settings::notification_settings(state.db())
        .await?
        .email_delay_minutes;
    jobs::enqueue_at(
        state.db(),
        EMAIL_JOB,
        &json!({ "notification_id": notification_id }),
        Some(Utc::now() + Duration::minutes(i64::from(delay))),
    )
    .await?;
    state.jobs().wake();
    Ok(())
}

/// Tells a member something from the moderators, with no post attached.
/// This is the hook moderation actions (M7) use.
pub async fn moderation(
    state: &AppState,
    user_id: i64,
    actor_id: Option<i64>,
    message: &str,
    action_id: Option<i64>,
) -> sqlx::Result<i64> {
    let id = stored::insert_moderation(state.db(), user_id, actor_id, message, action_id).await?;
    created(state, user_id, id).await?;
    Ok(id)
}

/// Tells any open pages that a member's counts may have changed.
pub fn counts_changed(state: &AppState, user_id: i64) {
    state.live().publish(Event::Counts { user_id });
}

/// Records that a member has seen a page of a thread, up to post
/// `up_to_seq`: their read marker advances and their notifications about
/// those posts are read.
pub async fn seen_thread(
    state: &AppState,
    user_id: i64,
    thread_id: i64,
    up_to_seq: i64,
) -> sqlx::Result<()> {
    stored::seen_thread(state.db(), user_id, thread_id, up_to_seq).await?;
    // Reading a conversation changes the messages count even when there
    // was no notification to clear.
    counts_changed(state, user_id);
    Ok(())
}

// ---------------------------------------------------------------------------
// Email

/// The email preference that covers a kind of notification.
pub fn topic(kind: Kind) -> EmailTopic {
    match kind {
        Kind::Mention => EmailTopic::Mentions,
        Kind::Subscription => EmailTopic::Subscriptions,
        Kind::Message => EmailTopic::Messages,
        Kind::Moderation => EmailTopic::Moderation,
    }
}

/// One item in a notification email.
pub struct EmailItem {
    pub headline: String,
    pub text: String,
    pub url: String,
}

pub async fn run_email_job(state: AppState, payload: serde_json::Value) -> anyhow::Result<()> {
    let id = payload["notification_id"]
        .as_i64()
        .context("notification email job without a notification_id")?;
    email_if_away(&state, id).await?;
    Ok(())
}

/// Emails a member about notification `id` and any others waiting, if
/// they still haven't seen them. Returns whether an email was queued.
pub async fn email_if_away(state: &AppState, id: i64) -> anyhow::Result<bool> {
    let db = state.db();
    let Some(pending) = stored::pending(db, id).await? else {
        return Ok(false);
    };
    if pending.read || pending.emailed {
        return Ok(false);
    }
    let Some(to) = stored::addressee(db, pending.user_id).await? else {
        return Ok(false);
    };
    let delay = settings::notification_settings(db)
        .await?
        .email_delay_minutes;
    let cutoff = Utc::now() - Duration::minutes(i64::from(delay));
    if pending.created_at > cutoff {
        // Not due yet (the delay was made longer since): try again then.
        jobs::enqueue_at(
            db,
            EMAIL_JOB,
            &json!({ "notification_id": id }),
            Some(pending.created_at + Duration::minutes(i64::from(delay))),
        )
        .await?;
        return Ok(false);
    }
    // Claimed in the transaction that queues the email, so two jobs for
    // the same member running at once can't both email the same
    // notifications: the second waits for the first, then finds nothing.
    let mut tx = db.begin().await?;
    let due = stored::claim_for_email(&mut tx, pending.user_id, cutoff).await?;
    let viewer = match viewer_for(state, pending.user_id).await? {
        Some(viewer) if !to.deleted && to.verified => viewer,
        _ => {
            tx.commit().await?;
            return Ok(false);
        }
    };
    let prefs = social::email_prefs(db, pending.user_id).await?;
    let site_name = settings::site_name(db).await?;
    let mut items = Vec::new();
    let mut topics = Vec::new();
    for due in &due {
        // They have since blocked whoever it's from, or unsubscribed from
        // where it happened.
        if due.quiet {
            continue;
        }
        // Seen on the site since it was made: they have had the chance to
        // see it there, so it isn't emailed.
        if to.last_seen_at.is_some_and(|seen| seen > due.created_at) {
            continue;
        }
        let topic = topic(due.kind);
        if !prefs.wants(topic) {
            continue;
        }
        let item = match due.post_id {
            None => EmailItem {
                headline: "A note from the moderators".to_owned(),
                text: due.message.clone().unwrap_or_default(),
                url: state.config().url("/notifications"),
            },
            Some(post_id) => {
                // Read as the member, through the access layer, now.
                let Some(post) = content::post_target(db, &viewer, post_id).await? else {
                    continue;
                };
                let Some(text) = &post.body else { continue };
                let Some(location) = content::post_location(db, &viewer, post_id).await? else {
                    continue;
                };
                let place = if post.thread.conversation().is_some()
                    && post.thread.thread.title.is_empty()
                {
                    "your conversation".to_owned()
                } else {
                    format!("“{}”", post.thread.name.clone())
                };
                EmailItem {
                    headline: format!("{} {} {place}", post.author.display_name, due.kind.verb()),
                    text: text.markdown.clone(),
                    url: state.config().url(&location.href(post_id)),
                }
            }
        };
        if !topics.contains(&topic) {
            topics.push(topic);
        }
        items.push(item);
    }
    if items.is_empty() {
        tx.commit().await?;
        return Ok(false);
    }
    let unsubscribe_topic = match topics.as_slice() {
        [one] => *one,
        _ => EmailTopic::All,
    };
    let secret = settings::signing_secret(db).await?;
    let unsubscribe = state.config().url(&format!(
        "/unsubscribe/{}",
        unsubscribe_token(&secret, pending.user_id, unsubscribe_topic)
    ));
    let email = emails::notifications(
        &to.email,
        &site_name,
        &to.display_name,
        &items,
        &state.config().url("/settings#email"),
        &unsubscribe,
        unsubscribe_topic.describe(),
    );
    mail::queue(&mut tx, &email).await?;
    tx.commit().await?;
    state.jobs().wake();
    Ok(true)
}

// ---------------------------------------------------------------------------
// One-click unsubscribe (RFC 8058)

/// The token in an unsubscribe link: who and what, signed with the site's
/// secret so it can't be made up, and needing nothing stored.
pub fn unsubscribe_token(secret: &[u8], user_id: i64, topic: EmailTopic) -> String {
    signed_token(secret, user_id, topic.as_str())
}

/// A token for a topic by the name it had when the link was made.
fn signed_token(secret: &[u8], user_id: i64, topic: &str) -> String {
    let message = format!("unsubscribe:{user_id}:{topic}");
    let mac = tokens::sign(secret, &message);
    format!("{user_id}.{topic}.{}", URL_SAFE_NO_PAD.encode(&mac[..16]))
}

/// The member and topic an unsubscribe token names, if it is genuine.
pub fn check_unsubscribe_token(secret: &[u8], token: &str) -> Option<(i64, EmailTopic)> {
    let mut parts = token.split('.');
    let (user, topic, _, None) = (parts.next()?, parts.next()?, parts.next()?, parts.next()) else {
        return None;
    };
    let user_id: i64 = user.parse().ok().filter(|&n: &i64| n >= 1)?;
    if user != user_id.to_string() {
        return None;
    }
    // Checked against the name in the link, which may be one a topic had
    // before (see `EmailTopic::parse`).
    let parsed = EmailTopic::parse(topic)?;
    let expected = signed_token(secret, user_id, topic);
    tokens::same(expected.as_bytes(), token.as_bytes()).then_some((user_id, parsed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator};

    #[test]
    fn the_most_specific_kind_wins() {
        let mut c = BTreeMap::new();
        consider(&mut c, 1, Kind::Subscription);
        consider(&mut c, 1, Kind::Mention);
        consider(&mut c, 1, Kind::Subscription);
        assert_eq!(c[&1], Kind::Mention);
        consider(&mut c, 2, Kind::Mention);
        consider(&mut c, 2, Kind::Message);
        assert_eq!(c[&2], Kind::Message);
    }

    #[test]
    fn every_kind_has_an_email_topic_of_its_own() {
        let kinds = [
            Kind::Mention,
            Kind::Subscription,
            Kind::Message,
            Kind::Moderation,
        ];
        for kind in kinds {
            assert_eq!(Kind::parse(kind.as_str()), Some(kind));
            assert_ne!(topic(kind), EmailTopic::All);
        }
        assert_eq!(topic(Kind::Subscription), EmailTopic::Subscriptions);
        for old in ["reply", "watched_thread", "watched_space"] {
            assert_eq!(Kind::parse(old), None);
        }
    }

    /// Unsubscribe links in emails sent before milestone 11 name topics
    /// that have since been renamed or folded into subscriptions; they
    /// still work, and turn off the emails about subscribed threads.
    #[test]
    fn links_from_before_subscriptions_still_unsubscribe() {
        let secret = b"secret";
        for old in ["watching", "replies"] {
            let mac = tokens::sign(secret, &format!("unsubscribe:7:{old}"));
            let token = format!("7.{old}.{}", URL_SAFE_NO_PAD.encode(&mac[..16]));
            assert_eq!(
                check_unsubscribe_token(secret, &token),
                Some((7, EmailTopic::Subscriptions))
            );
            // And can't be turned into a link for another member.
            let forged = format!("8.{old}.{}", URL_SAFE_NO_PAD.encode(&mac[..16]));
            assert_eq!(check_unsubscribe_token(secret, &forged), None);
        }
    }

    #[hegel::test(test_cases = 300)]
    fn unsubscribe_tokens_round_trip(tc: TestCase) {
        let secret = tc.draw(gs::binary().min_size(1).max_size(64));
        let user = tc.draw(gs::integers::<i64>().min_value(1));
        let topic = tc.draw(gs::sampled_from(EmailTopic::ALL.to_vec()).print_as_debug());
        let token = unsubscribe_token(&secret, user, topic);
        assert_eq!(
            check_unsubscribe_token(&secret, &token),
            Some((user, topic))
        );
        // The token is URL-safe.
        assert!(
            token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
            "{token}"
        );
    }

    #[hegel::test(test_cases = 300)]
    fn unsubscribe_tokens_cannot_be_altered(tc: TestCase) {
        let secret = tc.draw(gs::binary().min_size(1).max_size(64));
        let user = tc.draw(gs::integers::<i64>().min_value(1));
        let other = tc.draw(gs::integers::<i64>().min_value(1));
        let topic = tc.draw(gs::sampled_from(EmailTopic::ALL.to_vec()).print_as_debug());
        let other_topic = tc.draw(gs::sampled_from(EmailTopic::ALL.to_vec()).print_as_debug());
        tc.assume(user != other || topic != other_topic);
        let token = unsubscribe_token(&secret, user, topic);
        let mac = token.rsplit('.').next().unwrap();
        let forged = format!("{other}.{}.{mac}", other_topic.as_str());
        assert_eq!(check_unsubscribe_token(&secret, &forged), None);
        let other_secret = tc.draw(gs::binary().min_size(1).max_size(64));
        tc.assume(other_secret != secret);
        assert_eq!(check_unsubscribe_token(&other_secret, &token), None);
    }

    #[hegel::test(test_cases = 300)]
    fn checking_any_text_never_panics(tc: TestCase) {
        let text = tc.draw(gs::text());
        let _ = check_unsubscribe_token(b"secret", &text);
    }
}
