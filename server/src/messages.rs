//! Direct messages (DESIGN.md §3.7): starting conversations, sending in
//! them, adding people, leaving, and blocking, with the safety controls:
//!
//! - the site can turn messages off (staff can still send);
//! - new members can't start conversations (DESIGN.md §7);
//! - a conversation has a cap on its recipients, and starting them is rate
//!   limited;
//! - each member says who may start conversations with them: everyone,
//!   people they've interacted with, or nobody;
//! - members can block each other: neither can then write to the other in
//!   a new conversation or one between the two of them, nor add the other
//!   to one, and nobody can add either to a conversation the other is in;
//!   the blocker doesn't see the other's posts (they are folded away) or
//!   hear about them;
//! - adding someone to a group conversation takes what starting one does
//!   (new members can't), and someone who left can't be added back.
//!
//! Staff are never blocked or turned away: a member can't block a
//! moderator or admin, and staff may write to anyone.
//!
//! Messages are posts in private threads, so editing, deleting, reacting
//! and read markers are the same as for any post ([`crate::writing`]).

use crate::AppState;
use crate::changes::Change;
use crate::content::{self, ThreadTarget};
use crate::error::AppError;
use crate::notify;
use crate::ratelimit::{Action, Scope};
use crate::settings::{self, MessageSettings};
use crate::social::{self, DmPolicy, Recipient};
use crate::store::{self, NewConversation};
use crate::viewer::{UserCtx, Viewer};
use crate::writing::{self, Outcome, Posted, Refused, check_body, clean_body, clean_title};

pub const TURNED_OFF: &str = "Direct messages are turned off on this forum.";

/// The usernames in a "To" field: separated by commas or spaces, with or
/// without `@`, each once.
pub fn parse_recipients(to: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for name in to.split(|c: char| c == ',' || c == ';' || c.is_whitespace()) {
        let name = name.trim().trim_start_matches('@');
        if !name.is_empty() && !names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
            names.push(name.to_owned());
        }
    }
    names
}

/// Why a member who isn't staff may not start conversations, if they
/// may not.
pub async fn start_refusal(state: &AppState, user: &UserCtx) -> Result<Option<String>, AppError> {
    if user.role.is_staff() {
        return Ok(None);
    }
    let rules = settings::message_settings(state.db()).await?;
    if !rules.enabled {
        return Ok(Some(TURNED_OFF.to_owned()));
    }
    let standing = social::standing(state.db(), user.id).await?;
    if standing.days < i64::from(rules.new_user_days)
        || standing.posts < i64::from(rules.new_user_posts)
    {
        return Ok(Some(format!(
            "New members can’t start conversations yet. You can once your account is {} old and you have written {}.",
            crate::format::plural(i64::from(rules.new_user_days), "day", "days"),
            crate::format::plural(i64::from(rules.new_user_posts), "post", "posts"),
        )));
    }
    Ok(None)
}

/// Whether `sender` may write to `recipient` in a new conversation (or
/// add them to one), and if not why not. Staff may write to anyone.
async fn may_write_to(
    state: &AppState,
    sender: &UserCtx,
    recipient: &Recipient,
) -> Result<Result<(), String>, AppError> {
    let db = state.db();
    if sender.role.is_staff() {
        return Ok(Ok(()));
    }
    let refused = Err(format!(
        "{} isn’t taking messages from you.",
        recipient.user.username
    ));
    // A block either way stops it, except that staff can't be blocked.
    if social::has_blocked(db, recipient.user.id, sender.id).await? {
        return Ok(refused);
    }
    if !recipient.role.is_staff() && social::has_blocked(db, sender.id, recipient.user.id).await? {
        return Ok(Err(format!(
            "You’ve blocked {}. Unblock them first.",
            recipient.user.username
        )));
    }
    match recipient.dm_policy {
        DmPolicy::Everyone => Ok(Ok(())),
        DmPolicy::Nobody => Ok(Err(format!(
            "{} isn’t taking messages.",
            recipient.user.username
        ))),
        DmPolicy::Interacted => {
            if social::interacted(db, recipient.user.id, sender.id).await? {
                Ok(Ok(()))
            } else {
                Ok(refused)
            }
        }
    }
}

/// Looks up the members named in `to` and checks the sender may write to
/// them all, collecting the problems.
async fn recipients(
    state: &AppState,
    sender: &UserCtx,
    names: &[String],
    already: usize,
    rules: &MessageSettings,
    problems: &mut Vec<String>,
) -> Result<Vec<Recipient>, AppError> {
    let found = social::members_named(state.db(), names).await?;
    for name in names {
        if !found
            .iter()
            .any(|r| r.user.username.eq_ignore_ascii_case(name))
        {
            problems.push(format!("There’s no member called {name}."));
        }
    }
    if found.iter().any(|r| r.user.id == sender.id) {
        problems.push("You can’t send a message to yourself.".to_owned());
    }
    let others: Vec<Recipient> = found
        .into_iter()
        .filter(|r| r.user.id != sender.id)
        .collect();
    if others.len() + already > rules.max_recipients as usize {
        problems.push(format!(
            "A conversation can have at most {} besides you.",
            crate::format::plural(i64::from(rules.max_recipients), "person", "people")
        ));
    }
    for recipient in &others {
        if let Err(why) = may_write_to(state, sender, recipient).await? {
            problems.push(why);
        }
    }
    Ok(others)
}

fn allowed(state: &AppState, user: &UserCtx, action: Action) -> bool {
    state
        .limiter()
        .check(action, Scope::User, &user.id.to_string())
}

/// Starts a conversation with the members named in `to`.
pub async fn start(
    state: &AppState,
    viewer: &Viewer,
    to: &str,
    title: &str,
    body: &str,
) -> Outcome<Posted> {
    let Some(user) = viewer.user() else {
        return Ok(Err(Refused::LogIn));
    };
    if let Some(why) = start_refusal(state, user).await? {
        return Ok(Err(Refused::NotAllowedBecause(why)));
    }
    let rules = settings::message_settings(state.db()).await?;
    let names = parse_recipients(to);
    let mut problems = Vec::new();
    let others = if names.is_empty() {
        problems.push("Say who the message is for.".to_owned());
        Vec::new()
    } else {
        recipients(state, user, &names, 0, &rules, &mut problems).await?
    };
    let (title, body) = (clean_title(title), clean_body(body));
    if title.chars().count() > writing::MAX_TITLE_CHARS {
        problems.push(format!(
            "Titles can be up to {} characters.",
            writing::MAX_TITLE_CHARS
        ));
    }
    check_body(&body, &mut problems);
    if !problems.is_empty() {
        return Ok(Err(Refused::Invalid(problems)));
    }
    if !allowed(state, user, Action::NewConversation) {
        return Ok(Err(Refused::RateLimited));
    }
    let mut tx = state.db().begin().await?;
    let created = store::create_conversation(
        &mut tx,
        &NewConversation {
            author_id: user.id,
            title,
            recipient_ids: others.iter().map(|r| r.user.id).collect(),
            body_md: body,
            created_at: None,
        },
    )
    .await?;
    notify::queue_post(&mut tx, created.first_post_id).await?;
    tx.commit().await?;
    state.jobs().wake();
    state
        .changed(Change::PostCreated {
            post_id: created.first_post_id,
        })
        .await?;
    Ok(Ok(Posted {
        thread_id: created.thread_id,
        post_id: created.first_post_id,
    }))
}

/// Why the viewer may not send a message in a conversation they are in,
/// if they may not.
pub async fn send_refusal(
    state: &AppState,
    viewer: &Viewer,
    thread: &ThreadTarget,
) -> Result<Option<&'static str>, AppError> {
    let (Some(user), Some(conversation)) = (viewer.user(), thread.conversation()) else {
        return Ok(Some("Log in to reply."));
    };
    if user.role.is_staff() {
        return Ok(None);
    }
    if !settings::message_settings(state.db()).await?.enabled {
        return Ok(Some(TURNED_OFF));
    }
    let others = conversation.others(Some(user.id));
    if others.is_empty() {
        return Ok(Some("Everyone else has left this conversation."));
    }
    // Between two people, a block either way ends the conversation.
    if let [other] = others.as_slice()
        && !other.staff
        && (social::has_blocked(state.db(), other.user.id, user.id).await?
            || social::has_blocked(state.db(), user.id, other.user.id).await?)
    {
        return Ok(Some("You can’t send messages in this conversation."));
    }
    Ok(None)
}

/// Adds a member to a group conversation the viewer is in.
pub async fn add_person(
    state: &AppState,
    viewer: &Viewer,
    thread_id: i64,
    username: &str,
) -> Outcome<i64> {
    let Some(user) = viewer.user() else {
        return Ok(Err(Refused::LogIn));
    };
    let Some(thread) = content::thread_target(state.db(), viewer, thread_id).await? else {
        return Ok(Err(Refused::NotFound));
    };
    let Some(conversation) = thread.conversation() else {
        return Ok(Err(Refused::NotFound));
    };
    if !conversation.is_group() {
        return Ok(Err(Refused::NotAllowed(
            "Only group conversations can have people added. Start a new conversation instead.",
        )));
    }
    if let Some(why) = send_refusal(state, viewer, &thread).await? {
        return Ok(Err(Refused::NotAllowed(why)));
    }
    // Adding someone starts them talking with everyone there, so it takes
    // what starting a conversation takes.
    if let Some(why) = start_refusal(state, user).await? {
        return Ok(Err(Refused::NotAllowedBecause(why)));
    }
    let rules = settings::message_settings(state.db()).await?;
    let names = parse_recipients(username);
    let mut problems = Vec::new();
    let found = match names.as_slice() {
        [] => {
            problems.push("Say who to add.".to_owned());
            Vec::new()
        }
        [_] => {
            let found = recipients(
                state,
                user,
                &names,
                conversation.participants.len() - 1,
                &rules,
                &mut problems,
            )
            .await?;
            for person in &found {
                let name = &person.user.username;
                if conversation.includes(person.user.id) {
                    problems.push("They’re already in this conversation.".to_owned());
                } else if social::left_conversation(state.db(), thread_id, person.user.id).await? {
                    // Leaving sticks: nobody can pull them back in.
                    problems.push(format!(
                        "{name} left this conversation, so they can’t be added back."
                    ));
                } else if social::blocked_in_conversation(state.db(), thread_id, person.user.id)
                    .await?
                {
                    problems.push(format!(
                        "{name} can’t be added: there’s a block between them and someone here."
                    ));
                }
            }
            found
        }
        _ => {
            problems.push("Add one person at a time.".to_owned());
            Vec::new()
        }
    };
    let Some(person) = found.first().filter(|_| problems.is_empty()) else {
        return Ok(Err(Refused::Invalid(problems)));
    };
    if !allowed(state, user, Action::NewConversation) {
        return Ok(Err(Refused::RateLimited));
    }
    store::add_participant(state.db(), thread_id, person.user.id, user.id).await?;
    notify::counts_changed(state, person.user.id);
    Ok(Ok(thread_id))
}

/// Takes the viewer out of a conversation.
pub async fn leave(state: &AppState, viewer: &Viewer, thread_id: i64) -> Outcome<()> {
    let Some(user) = viewer.user() else {
        return Ok(Err(Refused::LogIn));
    };
    let Some(thread) = content::thread_target(state.db(), viewer, thread_id).await? else {
        return Ok(Err(Refused::NotFound));
    };
    if thread.conversation().is_none() {
        return Ok(Err(Refused::NotFound));
    }
    store::leave_conversation(state.db(), thread_id, user.id).await?;
    notify::counts_changed(state, user.id);
    Ok(Ok(()))
}

/// Who a block is about.
pub async fn member_to_block(
    state: &AppState,
    viewer: &Viewer,
    username: &str,
) -> Outcome<Recipient> {
    let Some(user) = viewer.user() else {
        return Ok(Err(Refused::LogIn));
    };
    let Some(member) = social::members_named(state.db(), &[username.to_owned()])
        .await?
        .into_iter()
        .next()
    else {
        return Ok(Err(Refused::NotFound));
    };
    if member.user.id == user.id {
        return Ok(Err(Refused::NotAllowed("You can’t block yourself.")));
    }
    if member.role.is_staff() {
        return Ok(Err(Refused::NotAllowed(
            "Moderators and admins can’t be blocked. If one of them is causing you trouble, tell another.",
        )));
    }
    Ok(Ok(member))
}

/// The viewer blocks (or, with `block` false, unblocks) a member.
pub async fn block(
    state: &AppState,
    viewer: &Viewer,
    username: &str,
    block: bool,
) -> Outcome<Recipient> {
    let Some(user) = viewer.user() else {
        return Ok(Err(Refused::LogIn));
    };
    if !block {
        let Some(member) = social::members_named(state.db(), &[username.to_owned()])
            .await?
            .into_iter()
            .next()
        else {
            return Ok(Err(Refused::NotFound));
        };
        social::unblock(state.db(), user.id, member.user.id).await?;
        notify::counts_changed(state, user.id);
        return Ok(Ok(member));
    }
    let member = match member_to_block(state, viewer, username).await? {
        Ok(member) => member,
        Err(refusal) => return Ok(Err(refusal)),
    };
    social::block(state.db(), user.id, member.user.id).await?;
    notify::counts_changed(state, user.id);
    Ok(Ok(member))
}

/// Whether the viewer may start a conversation with this member, for
/// offering a "Send a message" link on their profile.
pub async fn may_message(
    state: &AppState,
    viewer: &Viewer,
    member: &Recipient,
) -> Result<bool, AppError> {
    let Some(user) = viewer.user() else {
        return Ok(false);
    };
    if user.id == member.user.id || start_refusal(state, user).await?.is_some() {
        return Ok(false);
    }
    Ok(may_write_to(state, user, member).await?.is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;

    #[test]
    fn recipients_are_listed_loosely() {
        assert_eq!(parse_recipients(""), Vec::<String>::new());
        assert_eq!(parse_recipients("jonah"), vec!["jonah"]);
        assert_eq!(
            parse_recipients(" @jonah, maya;kit  Jonah "),
            vec!["jonah", "maya", "kit"]
        );
    }

    #[hegel::test(test_cases = 300)]
    fn parsed_recipients_are_distinct_and_clean(tc: TestCase) {
        let to = tc.draw(gs::text());
        let names = parse_recipients(&to);
        for (i, name) in names.iter().enumerate() {
            assert!(!name.is_empty());
            assert!(!name.contains(|c: char| c == ',' || c == ';' || c.is_whitespace()));
            assert!(!name.starts_with('@'));
            for other in &names[..i] {
                assert!(!other.eq_ignore_ascii_case(name), "{names:?}");
            }
        }
        // Listing them again gives the same names.
        assert_eq!(parse_recipients(&names.join(", ")), names);
    }
}
