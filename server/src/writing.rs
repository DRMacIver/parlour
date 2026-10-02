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
use crate::content::{self, CategoryTarget, PostTarget, ThreadTarget};
use crate::error::AppError;
use crate::permissions::{at_most_as_visible, may_post, may_reply};
use crate::ratelimit::{Action, Scope};
use crate::store::{self, Edited, NewPost, NewThread};
use crate::viewer::Viewer;

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

pub fn check_body(body: &str, problems: &mut Vec<String>) {
    if body.trim().is_empty() {
        problems.push("Write something first.".to_owned());
    } else if body.chars().count() > MAX_BODY_CHARS {
        problems.push(format!(
            "That’s too long: posts can be up to {} characters.",
            crate::format::count(MAX_BODY_CHARS as i64)
        ));
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

/// Whether the viewer may start threads in a category they can read.
pub fn may_post_in(viewer: &Viewer, category: &CategoryTarget) -> bool {
    may_post(viewer, &category.audience(), &category.write())
}

/// Whether the viewer may reply in a thread they can read.
pub fn may_reply_in(viewer: &Viewer, thread: &ThreadTarget) -> bool {
    may_reply(
        viewer,
        &thread.category.audience(),
        &thread.category.write(),
        thread.locked,
    )
}

/// Why the viewer may not reply in a thread they can read, if they may not.
pub fn reply_refusal(viewer: &Viewer, thread: &ThreadTarget) -> Option<&'static str> {
    if viewer.user().is_none() {
        Some("Log in to reply.")
    } else if !may_post_in(viewer, &thread.category) {
        Some("You can’t post in this category.")
    } else if !may_reply_in(viewer, thread) {
        Some("This thread is locked.")
    } else {
        None
    }
}

/// Whether the viewer may edit or delete a post: it is theirs and live,
/// and (to edit) its thread isn't locked unless they are staff.
pub fn may_edit(viewer: &Viewer, post: &PostTarget) -> bool {
    viewer.user_id() == Some(post.author.id)
        && post.is_live()
        && (!post.thread.locked || viewer.is_staff())
}

pub fn may_delete(viewer: &Viewer, post: &PostTarget) -> bool {
    viewer.user_id() == Some(post.author.id) && post.is_live()
}

// ---------------------------------------------------------------------------

pub async fn new_thread(
    state: &AppState,
    viewer: &Viewer,
    category_slug: &str,
    title: &str,
    body: &str,
) -> Outcome<Posted> {
    let Some(user) = viewer.user() else {
        return Ok(Err(Refused::LogIn));
    };
    let Some(category) = content::category_target(state.db(), viewer, category_slug).await? else {
        return Ok(Err(Refused::NotFound));
    };
    if !may_post_in(viewer, &category) {
        return Ok(Err(Refused::NotAllowed("You can’t post in this category.")));
    }
    let (title, body) = (clean_title(title), clean_body(body));
    let mut problems = Vec::new();
    check_title(&title, &mut problems);
    check_body(&body, &mut problems);
    if !problems.is_empty() {
        return Ok(Err(Refused::Invalid(problems)));
    }
    if !allowed(state, viewer, Action::Post) {
        return Ok(Err(Refused::RateLimited));
    }
    let created = store::create_thread(
        state.db(),
        &NewThread {
            category_id: category.category.id,
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

pub async fn reply(
    state: &AppState,
    viewer: &Viewer,
    thread_id: i64,
    body: &str,
    reply_to: Option<i64>,
) -> Outcome<Posted> {
    let Some(user) = viewer.user() else {
        return Ok(Err(Refused::LogIn));
    };
    let Some(thread) = content::thread_target(state.db(), viewer, thread_id).await? else {
        return Ok(Err(Refused::NotFound));
    };
    if let Some(reason) = reply_refusal(viewer, &thread) {
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
    if !allowed(state, viewer, Action::Post) {
        return Ok(Err(Refused::RateLimited));
    }
    let post_id = store::create_post(
        state.db(),
        &NewPost {
            thread_id,
            author_id: user.id,
            body_md: body,
            reply_to_post_id: reply_to,
            created_at: None,
        },
    )
    .await?;
    state.changed(Change::PostCreated { post_id }).await?;
    Ok(Ok(Posted { thread_id, post_id }))
}

pub async fn edit(state: &AppState, viewer: &Viewer, post_id: i64, body: &str) -> Outcome<Posted> {
    let Some(user) = viewer.user() else {
        return Ok(Err(Refused::LogIn));
    };
    let Some(post) = content::post_target(state.db(), viewer, post_id).await? else {
        return Ok(Err(Refused::NotFound));
    };
    if !post.is_live() {
        return Ok(Err(Refused::NotFound));
    }
    if !may_edit(viewer, &post) {
        return Ok(Err(Refused::NotAllowed(if post.thread.locked {
            "This thread is locked."
        } else {
            "You can only edit your own posts."
        })));
    }
    let body = clean_body(body);
    let mut problems = Vec::new();
    check_body(&body, &mut problems);
    if !problems.is_empty() {
        return Ok(Err(Refused::Invalid(problems)));
    }
    if !allowed(state, viewer, Action::Edit) {
        return Ok(Err(Refused::RateLimited));
    }
    let edited = store::edit_post(state.db(), post_id, user.id, &body).await?;
    if edited != Edited::Unchanged {
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
            "You can only delete your own posts.",
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

/// The categories a post could be quoted into: those the viewer may post
/// in that are at most as visible as the post's own (DESIGN.md §3.1).
pub async fn fork_destinations(
    state: &AppState,
    viewer: &Viewer,
    source: &PostTarget,
) -> Result<Vec<CategoryTarget>, AppError> {
    let source_audience = source.thread.category.audience();
    Ok(content::readable_categories(state.db(), viewer)
        .await?
        .into_iter()
        .filter(|c| may_post_in(viewer, c) && at_most_as_visible(&source_audience, &c.audience()))
        .collect())
}

/// Starts a new thread in `category_slug` that embeds post `post_id`.
pub async fn fork(
    state: &AppState,
    viewer: &Viewer,
    post_id: i64,
    category_slug: &str,
    title: &str,
    body: &str,
) -> Outcome<Posted> {
    let Some(user) = viewer.user() else {
        return Ok(Err(Refused::LogIn));
    };
    let Some(source) = content::post_target(state.db(), viewer, post_id).await? else {
        return Ok(Err(Refused::NotFound));
    };
    if !source.is_live() {
        return Ok(Err(Refused::NotFound));
    }
    let (title, body) = (clean_title(title), clean_body(body));
    let mut problems = Vec::new();
    let destination = fork_destinations(state, viewer, &source)
        .await?
        .into_iter()
        .find(|c| c.category.slug == category_slug);
    if destination.is_none() {
        problems.push(
            "Choose a category from the list: you can only quote a post somewhere at least as private as where it is."
                .to_owned(),
        );
    }
    check_title(&title, &mut problems);
    check_body(&body, &mut problems);
    let Some(destination) = destination.filter(|_| problems.is_empty()) else {
        return Ok(Err(Refused::Invalid(problems)));
    };
    if !allowed(state, viewer, Action::Post) {
        return Ok(Err(Refused::RateLimited));
    }
    let created = store::create_thread(
        state.db(),
        &NewThread {
            category_id: destination.category.id,
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

/// Marks every thread the viewer can see in a category as read.
pub async fn mark_category_read(
    state: &AppState,
    viewer: &Viewer,
    category_slug: &str,
) -> Outcome<CategoryTarget> {
    let Some(user) = viewer.user() else {
        return Ok(Err(Refused::LogIn));
    };
    let Some(category) = content::category_target(state.db(), viewer, category_slug).await? else {
        return Ok(Err(Refused::NotFound));
    };
    let threads = content::visible_thread_ids(state.db(), viewer, category.category.id).await?;
    store::mark_threads_read(state.db(), user.id, &threads).await?;
    Ok(Ok(category))
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
