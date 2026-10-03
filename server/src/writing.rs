//! Writes made by members (DESIGN.md §3.2–3.5): new threads, replies,
//! edits, deletions, reactions, quote-forks and read markers.
//!
//! Each operation finds what it is about through [`crate::content`] as the
//! viewer, so that nothing the viewer can't see can be written to; checks
//! the viewer may do it with [`crate::permissions`]; writes with
//! [`crate::store`]; and then clears the anonymous pages it changed with
//! [`AppState::changed`].

use crate::AppState;
use crate::changes::Change;
use crate::content::{
    self, Deps, LatestScope, Place, PostTarget, SectionTab, SpaceTarget, ThreadTarget,
};
use crate::error::AppError;
use crate::markdown;
use crate::mod_records::{self, HeldPlace};
use crate::notify;
use crate::permissions::at_most_as_visible;
use crate::ratelimit::{Action, Scope};
use crate::social::{self, WatchLevel, Watched};
use crate::store::{self, Edited, NewPost, NewThread};
use crate::viewer::{UserCtx, Viewer};

pub const MAX_TITLE_CHARS: usize = 150;
pub const MAX_BODY_CHARS: usize = 50_000;
/// How many reaction types can be enabled at once.
pub const MAX_REACTIONS: usize = 8;

/// Why a write was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// The thing doesn't exist, or the viewer can't see it.
    NotFound,
    /// The viewer isn't logged in.
    LogIn,
    /// The viewer can see it but may not do this.
    NotAllowed(&'static str),
    /// The same, with a reason worked out at the time.
    NotAllowedBecause(String),
    /// What was sent has problems, described for the form.
    Invalid(Vec<String>),
    /// Too many writes recently.
    RateLimited,
}

pub type Outcome<T> = Result<Result<T, Refused>, AppError>;

/// Where a new or edited post is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Posted {
    pub thread_id: i64,
    pub post_id: i64,
}

/// What became of a new post: posted, or held for a moderator to approve
/// (as the held post with this id).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Written {
    Posted(Posted),
    Held(i64),
}

pub const SILENCED: &str = "A moderator has silenced your account for now, so you can read but not post, edit, react or send messages.";
pub const SUSPENDED: &str = "Your account is suspended for now.";

/// Why a member may not write at all just now, if they may not.
pub fn restriction(user: &UserCtx) -> Option<&'static str> {
    if user.is_suspended() {
        Some(SUSPENDED)
    } else if user.is_silenced() {
        Some(SILENCED)
    } else {
        None
    }
}

/// Whether a member is new (DESIGN.md §7): until their account is old
/// enough and they have written enough, whichever comes later, with the
/// thresholds in the message settings. New members get lower posting and
/// message rates, can't start conversations, and have their first posts
/// held for approval.
pub struct Newness {
    pub new: bool,
    /// Their published posts in spaces.
    pub posts: i64,
}

pub async fn newness(state: &AppState, user: &UserCtx) -> Result<Newness, AppError> {
    if user.is_staff() {
        return Ok(Newness {
            new: false,
            posts: 0,
        });
    }
    let rules = crate::settings::message_settings(state.db()).await?;
    let standing = social::standing(state.db(), user.id).await?;
    Ok(Newness {
        new: standing.days < i64::from(rules.new_user_days)
            || standing.posts < i64::from(rules.new_user_posts),
        posts: standing.posts,
    })
}

/// Whether a member's next post in a space must wait for a moderator:
/// they are new and fewer than the site's number of their posts have been
/// published.
pub async fn must_hold(state: &AppState, user: &UserCtx) -> Result<bool, AppError> {
    let hold = crate::settings::moderation_settings(state.db())
        .await?
        .hold_posts;
    if hold == 0 || user.is_staff() {
        return Ok(false);
    }
    let newness = newness(state, user).await?;
    Ok(newness.new && newness.posts < i64::from(hold))
}

/// Uses up some of a member's allowance for posting (or, in a
/// conversation, messaging), with the lower allowance for new members on
/// top.
async fn allowed_to_post(
    state: &AppState,
    user: &UserCtx,
    action: Action,
) -> Result<bool, AppError> {
    let key = user.id.to_string();
    if !state.limiter().check(action, Scope::User, &key) {
        return Ok(false);
    }
    if newness(state, user).await?.new {
        let stricter = match action {
            Action::Message => Action::NewMemberMessage,
            _ => Action::NewMemberPost,
        };
        return Ok(state.limiter().check(stricter, Scope::User, &key));
    }
    Ok(true)
}

/// Browsers send line breaks as CRLF. Leading blank lines and trailing
/// whitespace mean nothing, but leading spaces do (code blocks).
pub fn clean_body(body: &str) -> String {
    body.replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim_start_matches('\n')
        .trim_end()
        .to_owned()
}

pub fn clean_title(title: &str) -> String {
    title.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Checks a tidied body: it must be short enough, not too deeply nested
/// (see [`markdown::MAX_NESTING`]) and show something.
pub fn check_body(body: &str, problems: &mut Vec<String>) {
    if body.chars().count() > MAX_BODY_CHARS {
        problems.push(format!(
            "That’s too long: posts can be up to {} characters.",
            crate::format::count(MAX_BODY_CHARS as i64)
        ));
    } else if markdown::nesting_depth(body) > markdown::MAX_NESTING {
        problems.push(
            "That’s nested too deeply: take out some of the quotes inside quotes, lists inside lists or emphasis inside emphasis."
                .to_owned(),
        );
    } else if markdown::is_blank(body) {
        problems.push("Write something first.".to_owned());
    }
}

pub fn check_title(title: &str, problems: &mut Vec<String>) {
    if title.is_empty() {
        problems.push("Give the thread a title.".to_owned());
    } else if title.chars().count() > MAX_TITLE_CHARS {
        problems.push(format!("Titles can be up to {MAX_TITLE_CHARS} characters."));
    }
}

fn allowed(state: &AppState, viewer: &Viewer, action: Action) -> bool {
    match viewer.user_id() {
        Some(id) => state.limiter().check(action, Scope::User, &id.to_string()),
        None => false,
    }
}

/// Whether the viewer may preview a post now, using up some of their
/// allowance: rendering takes time, so previews are rate limited too.
pub fn may_preview(state: &AppState, viewer: &Viewer) -> bool {
    allowed(state, viewer, Action::Preview)
}

/// Why a member who can read a space may not start a thread in it.
pub fn start_refusal(space: &SpaceTarget) -> &'static str {
    if space.rules.archived {
        "This space is archived, so nothing more can be posted in it."
    } else {
        "You can’t start threads in this space."
    }
}

/// Whether the viewer may start threads in a space they can read.
pub fn may_start_in(viewer: &Viewer, space: &SpaceTarget) -> bool {
    space.rules.may_start(viewer)
}

/// Whether the viewer may reply in a space thread they can read.
pub fn may_reply_in(viewer: &Viewer, space: &SpaceTarget, locked: bool) -> bool {
    space.rules.may_reply(viewer, locked)
}

/// Why the viewer may not reply in a thread or conversation they can
/// read, if they may not.
pub async fn reply_refusal(
    state: &AppState,
    viewer: &Viewer,
    thread: &ThreadTarget,
) -> Result<Option<&'static str>, AppError> {
    let Some(user) = viewer.user() else {
        return Ok(Some("Log in to reply."));
    };
    Ok(match &thread.place {
        Place::Conversation(_) => crate::messages::send_refusal(state, viewer, thread).await?,
        Place::Space(space) => {
            if let Some(why) = restriction(user) {
                Some(why)
            } else if space.rules.archived {
                Some("This space is archived, so nothing more can be posted in it.")
            } else if !may_reply_in(viewer, space, false) {
                Some("You can’t reply in this space.")
            } else if !may_reply_in(viewer, space, thread.locked) {
                Some("This thread is locked.")
            } else {
                None
            }
        }
    })
}

/// Whether the viewer may edit a post: it is theirs and live, they aren't
/// silenced, and its thread isn't locked unless they are staff. Staff may
/// also edit other people's posts that aren't deleted, giving a reason
/// (see [`edit`]).
pub fn may_edit(viewer: &Viewer, post: &PostTarget) -> bool {
    let Some(user) = viewer.user() else {
        return false;
    };
    if !user.may_write() {
        return false;
    }
    if viewer.is_staff() {
        return post.body.is_some();
    }
    user.id == post.author.id && post.is_live() && !post.thread.locked
}

/// Whether a staff member is editing someone else's post.
pub fn editing_as_moderator(viewer: &Viewer, post: &PostTarget) -> bool {
    viewer.is_staff() && viewer.user_id() != Some(post.author.id)
}

/// Whether the viewer may delete a post: it is theirs and live, and they
/// aren't suspended. (Staff delete other people's posts as moderation.)
pub fn may_delete(viewer: &Viewer, post: &PostTarget) -> bool {
    viewer
        .user()
        .is_some_and(|user| user.id == post.author.id && user.may_act())
        && post.is_live()
}

// ---------------------------------------------------------------------------

pub async fn new_thread(
    state: &AppState,
    viewer: &Viewer,
    space_slug: &str,
    title: &str,
    body: &str,
) -> Outcome<Written> {
    let Some(user) = viewer.user() else {
        return Ok(Err(Refused::LogIn));
    };
    let Some(space) = content::space_target(state.db(), viewer, space_slug).await? else {
        return Ok(Err(Refused::NotFound));
    };
    if let Some(why) = restriction(user) {
        return Ok(Err(Refused::NotAllowed(why)));
    }
    if !may_start_in(viewer, &space) {
        return Ok(Err(Refused::NotAllowed(start_refusal(&space))));
    }
    let (title, body) = (clean_title(title), clean_body(body));
    let mut problems = Vec::new();
    check_title(&title, &mut problems);
    check_body(&body, &mut problems);
    if !problems.is_empty() {
        return Ok(Err(Refused::Invalid(problems)));
    }
    if !allowed_to_post(state, user, Action::Post).await? {
        return Ok(Err(Refused::RateLimited));
    }
    if must_hold(state, user).await? {
        let place = HeldPlace::Thread {
            space_id: space.space.id,
            title,
            forked_from_post_id: None,
        };
        let id = mod_records::hold(state.db(), user.id, &place, &body).await?;
        return Ok(Ok(Written::Held(id)));
    }
    let created = create_thread(
        state,
        &NewThread {
            space_id: space.space.id,
            title,
            author_id: user.id,
            body_md: body,
            forked_from_post_id: None,
            pinned: false,
            locked: false,
            created_at: None,
        },
    )
    .await?;
    Ok(Ok(Written::Posted(Posted {
        thread_id: created.thread_id,
        post_id: created.first_post_id,
    })))
}

pub async fn reply(
    state: &AppState,
    viewer: &Viewer,
    thread_id: i64,
    body: &str,
    reply_to: Option<i64>,
) -> Outcome<Written> {
    let Some(user) = viewer.user() else {
        return Ok(Err(Refused::LogIn));
    };
    let Some(thread) = content::thread_target(state.db(), viewer, thread_id).await? else {
        return Ok(Err(Refused::NotFound));
    };
    if let Some(reason) = reply_refusal(state, viewer, &thread).await? {
        return Ok(Err(Refused::NotAllowed(reason)));
    }
    let body = clean_body(body);
    let mut problems = Vec::new();
    check_body(&body, &mut problems);
    if let Some(reply_to) = reply_to {
        let target = content::post_target(state.db(), viewer, reply_to).await?;
        if target.is_none_or(|post| post.thread.thread.id != thread_id) {
            problems.push("You can only reply to posts in this thread.".to_owned());
        }
    }
    if !problems.is_empty() {
        return Ok(Err(Refused::Invalid(problems)));
    }
    let action = match thread.place {
        Place::Space(_) => Action::Post,
        Place::Conversation(_) => Action::Message,
    };
    if !allowed_to_post(state, user, action).await? {
        return Ok(Err(Refused::RateLimited));
    }
    // Messages are never held: moderators don't read conversations.
    if action == Action::Post && must_hold(state, user).await? {
        let place = HeldPlace::Reply {
            thread_id,
            reply_to_post_id: reply_to,
        };
        let id = mod_records::hold(state.db(), user.id, &place, &body).await?;
        return Ok(Ok(Written::Held(id)));
    }
    let post_id = publish_reply(state, thread_id, user.id, body, reply_to).await?;
    Ok(Ok(Written::Posted(Posted { thread_id, post_id })))
}

/// Adds a reply to a thread and queues the notifications about it.
pub(crate) async fn publish_reply(
    state: &AppState,
    thread_id: i64,
    author_id: i64,
    body: String,
    reply_to: Option<i64>,
) -> Result<i64, AppError> {
    let mut tx = state.db().begin().await?;
    let post_id = store::create_post(
        &mut tx,
        &NewPost {
            thread_id,
            author_id,
            body_md: body,
            reply_to_post_id: reply_to,
            created_at: None,
        },
    )
    .await?;
    store::read_own_post(&mut tx, post_id).await?;
    notify::queue_post(&mut tx, post_id).await?;
    tx.commit().await?;
    state.jobs().wake();
    state.changed(Change::PostCreated { post_id }).await?;
    Ok(post_id)
}

/// Changes a post's text. A staff member editing someone else's post gives
/// a `reason`, which goes in the moderation log, and its author is told.
pub async fn edit(
    state: &AppState,
    viewer: &Viewer,
    post_id: i64,
    body: &str,
    reason: Option<&str>,
) -> Outcome<Posted> {
    let Some(user) = viewer.user() else {
        return Ok(Err(Refused::LogIn));
    };
    let Some(post) = content::post_target(state.db(), viewer, post_id).await? else {
        return Ok(Err(Refused::NotFound));
    };
    if post.body.is_none() || (!post.is_live() && !viewer.is_staff()) {
        return Ok(Err(Refused::NotFound));
    }
    if !may_edit(viewer, &post) {
        return Ok(Err(Refused::NotAllowed(
            if let Some(why) = restriction(user) {
                why
            } else if post.thread.locked {
                "This thread is locked."
            } else {
                "You can only edit your own posts."
            },
        )));
    }
    let body = clean_body(body);
    let mut problems = Vec::new();
    check_body(&body, &mut problems);
    let as_moderator = editing_as_moderator(viewer, &post);
    let reason = reason.map(str::trim).unwrap_or_default();
    if as_moderator {
        crate::moderation::check_reason(reason, &mut problems);
    }
    if !problems.is_empty() {
        return Ok(Err(Refused::Invalid(problems)));
    }
    if !allowed(state, viewer, Action::Edit) {
        return Ok(Err(Refused::RateLimited));
    }
    let edited = store::edit_post(state.db(), post_id, user.id, &body).await?;
    if edited != Edited::Unchanged {
        if as_moderator {
            crate::moderation::edited_post(state, user, &post, reason).await?;
        }
        state.changed(Change::PostEdited { post_id }).await?;
    }
    Ok(Ok(Posted {
        thread_id: post.thread.thread.id,
        post_id,
    }))
}

pub async fn delete(state: &AppState, viewer: &Viewer, post_id: i64) -> Outcome<Posted> {
    if viewer.user().is_none() {
        return Ok(Err(Refused::LogIn));
    }
    let Some(post) = content::post_target(state.db(), viewer, post_id).await? else {
        return Ok(Err(Refused::NotFound));
    };
    if !post.is_live() {
        return Ok(Err(Refused::NotFound));
    }
    if !may_delete(viewer, &post) {
        return Ok(Err(Refused::NotAllowed(
            if viewer.user().is_some_and(UserCtx::is_suspended) {
                SUSPENDED
            } else {
                "You can only delete your own posts."
            },
        )));
    }
    if store::delete_post(state.db(), post_id).await? {
        state.changed(Change::PostRemoved { post_id }).await?;
    }
    Ok(Ok(Posted {
        thread_id: post.thread.thread.id,
        post_id,
    }))
}

/// Adds (`add`) or takes back a reaction.
pub async fn react(
    state: &AppState,
    viewer: &Viewer,
    post_id: i64,
    emoji: &str,
    add: bool,
) -> Outcome<Posted> {
    let Some(user) = viewer.user() else {
        return Ok(Err(Refused::LogIn));
    };
    let Some(post) = content::post_target(state.db(), viewer, post_id).await? else {
        return Ok(Err(Refused::NotFound));
    };
    if !post.is_live() {
        return Ok(Err(Refused::NotFound));
    }
    // Staff can read a reported conversation without being in it, but only
    // those in it write in it, reactions included.
    if let Some(conversation) = post.thread.conversation()
        && !conversation.includes(user.id)
    {
        return Ok(Err(Refused::NotAllowed("You aren’t in this conversation.")));
    }
    if let Some(why) = restriction(user) {
        return Ok(Err(Refused::NotAllowed(why)));
    }
    if post.thread.locked && !viewer.is_staff() {
        return Ok(Err(Refused::NotAllowed("This thread is locked.")));
    }
    let enabled = content::reaction_types(state.db(), true).await?;
    if add && !enabled.iter().any(|t| t.emoji == emoji) {
        return Ok(Err(Refused::Invalid(vec![
            "Choose one of the reactions offered.".to_owned(),
        ])));
    }
    if !allowed(state, viewer, Action::React) {
        return Ok(Err(Refused::RateLimited));
    }
    let changed = if add {
        store::add_reaction(state.db(), post_id, user.id, emoji).await?
    } else {
        store::remove_reaction(state.db(), post_id, user.id, emoji).await?
    };
    if changed {
        state.changed(Change::ReactionChanged { post_id }).await?;
    }
    Ok(Ok(Posted {
        thread_id: post.thread.thread.id,
        post_id,
    }))
}

/// The spaces a post could be quoted into: those the viewer may post
/// in that are at most as visible as the post's own (DESIGN.md §3.1).
pub async fn fork_destinations(
    state: &AppState,
    viewer: &Viewer,
    source: &PostTarget,
) -> Result<Vec<SpaceTarget>, AppError> {
    let source_audience = source.thread.audience();
    Ok(content::readable_spaces(state.db(), viewer)
        .await?
        .into_iter()
        .filter(|c| may_start_in(viewer, c) && at_most_as_visible(&source_audience, &c.audience()))
        .collect())
}

/// Starts a new thread in `space_slug` that embeds post `post_id`.
pub async fn fork(
    state: &AppState,
    viewer: &Viewer,
    post_id: i64,
    space_slug: &str,
    title: &str,
    body: &str,
) -> Outcome<Written> {
    let Some(user) = viewer.user() else {
        return Ok(Err(Refused::LogIn));
    };
    let Some(source) = content::post_target(state.db(), viewer, post_id).await? else {
        return Ok(Err(Refused::NotFound));
    };
    if !source.is_live() {
        return Ok(Err(Refused::NotFound));
    }
    if let Some(why) = restriction(user) {
        return Ok(Err(Refused::NotAllowed(why)));
    }
    let (title, body) = (clean_title(title), clean_body(body));
    let mut problems = Vec::new();
    let destination = fork_destinations(state, viewer, &source)
        .await?
        .into_iter()
        .find(|c| c.space.slug == space_slug);
    if destination.is_none() {
        problems.push(
            "Choose a space from the list: you can only quote a post somewhere at least as private as where it is."
                .to_owned(),
        );
    }
    check_title(&title, &mut problems);
    check_body(&body, &mut problems);
    let Some(destination) = destination.filter(|_| problems.is_empty()) else {
        return Ok(Err(Refused::Invalid(problems)));
    };
    if !allowed_to_post(state, user, Action::Post).await? {
        return Ok(Err(Refused::RateLimited));
    }
    if must_hold(state, user).await? {
        let place = HeldPlace::Thread {
            space_id: destination.space.id,
            title,
            forked_from_post_id: Some(post_id),
        };
        let id = mod_records::hold(state.db(), user.id, &place, &body).await?;
        return Ok(Ok(Written::Held(id)));
    }
    let created = create_thread(
        state,
        &NewThread {
            space_id: destination.space.id,
            title,
            author_id: user.id,
            body_md: body,
            forked_from_post_id: Some(post_id),
            pinned: false,
            locked: false,
            created_at: None,
        },
    )
    .await?;
    Ok(Ok(Written::Posted(Posted {
        thread_id: created.thread_id,
        post_id: created.first_post_id,
    })))
}

/// Creates a space thread with its first post, has its author watch it,
/// and queues the notifications about it.
pub(crate) async fn create_thread(
    state: &AppState,
    thread: &NewThread,
) -> Result<store::CreatedThread, AppError> {
    let mut tx = state.db().begin().await?;
    let created = store::create_thread(&mut tx, thread).await?;
    social::set_watch(
        &mut tx,
        thread.author_id,
        Watched::Thread(created.thread_id),
        WatchLevel::Watching,
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
    Ok(created)
}

/// Marks every thread the viewer can see in a space as read.
pub async fn mark_space_read(
    state: &AppState,
    viewer: &Viewer,
    space_slug: &str,
) -> Outcome<SpaceTarget> {
    let Some(user) = viewer.user() else {
        return Ok(Err(Refused::LogIn));
    };
    let Some(space) = content::space_target(state.db(), viewer, space_slug).await? else {
        return Ok(Err(Refused::NotFound));
    };
    let threads = content::visible_thread_ids(state.db(), viewer, &[space.space.id]).await?;
    store::mark_threads_read(state.db(), user.id, &threads).await?;
    Ok(Ok(space))
}

/// Marks every thread the viewer can see in the spaces of one section of
/// their layout as read (DESIGN.md §3.5). Not found if the section isn't
/// theirs.
pub async fn mark_section_read(
    state: &AppState,
    viewer: &Viewer,
    section_id: i64,
) -> Outcome<SectionTab> {
    let Some(user) = viewer.user() else {
        return Ok(Err(Refused::LogIn));
    };
    let mut deps = Deps::new();
    let layout = content::layout_sections(state.db(), viewer, &mut deps).await?;
    let Some(section) = layout.sections.into_iter().find(|s| s.id == section_id) else {
        return Ok(Err(Refused::NotFound));
    };
    let threads =
        content::scope_thread_ids(state.db(), viewer, LatestScope::Section(section_id)).await?;
    store::mark_threads_read(state.db(), user.id, &threads).await?;
    Ok(Ok(section))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;

    #[test]
    fn bodies_are_tidied() {
        assert_eq!(
            clean_body("\r\n\r\n    code\r\nmore  \n\n"),
            "    code\nmore"
        );
        assert_eq!(clean_title("  Heat   with\tfive "), "Heat with five");
    }

    fn problems_with(body: &str) -> Vec<String> {
        let mut problems = Vec::new();
        check_body(body, &mut problems);
        problems
    }

    #[test]
    fn bodies_must_show_something() {
        assert_eq!(problems_with("Hello"), Vec::<String>::new());
        assert_eq!(
            problems_with("<sarcasm>yes</sarcasm>"),
            Vec::<String>::new()
        );
        for body in [
            "",
            "  ",
            "[l]: https://example.com",
            "[](https://example.com)",
        ] {
            assert_eq!(
                problems_with(body),
                vec!["Write something first."],
                "{body:?}"
            );
        }
    }

    #[test]
    fn bodies_may_not_nest_too_deeply() {
        let deep = format!("{}x", "> ".repeat(crate::markdown::MAX_NESTING));
        let problems = problems_with(&deep);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("nested too deeply"), "{problems:?}");
        assert!(problems_with(&format!("{}x", "> ".repeat(10))).is_empty());
    }

    #[test]
    fn long_bodies_are_refused_before_they_are_parsed() {
        let problems = problems_with(&"> ".repeat(MAX_BODY_CHARS));
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("too long"), "{problems:?}");
    }

    #[hegel::test(test_cases = 300)]
    fn tidying_is_idempotent(tc: TestCase) {
        let text = tc.draw(gs::text());
        let body = clean_body(&text);
        assert_eq!(clean_body(&body), body);
        assert!(!body.contains('\r'));
        let title = clean_title(&text);
        assert_eq!(clean_title(&title), title);
    }
}
