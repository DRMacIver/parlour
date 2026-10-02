//! Moderation (DESIGN.md §8): reports, the contact form and appeals; the
//! queue; and what moderators do, each recorded in the moderation log with
//! a reason.
//!
//! Every action here checks that the viewer is staff (a moderator or admin
//! who isn't suspended), finds what it is about through
//! [`crate::content`] as that viewer, makes the change with
//! [`crate::mod_records`] and [`crate::store`], records it, settles the
//! reports it answers, tells the member it affects (with a link to contest
//! it), and clears the anonymous pages it changed.
//!
//! Entries in the log are public when what they are about is public: a
//! post or thread in a public category (both before and after, for a
//! move), or a member, whose name is public anyway. Everything about
//! members-only, group or private content, staff notes, dismissed reports
//! and appeals is for staff only. The public log still shows a thread's
//! title only to those who can read the thread now.
//!
//! Silences and suspensions can't be put on staff: an admin must take
//! someone's role away first.

use chrono::{Duration, Utc};
use serde_json::json;

use crate::AppState;
use crate::changes::Change;
use crate::content::{self, Place, PostTarget, ThreadTarget, UserRef};
use crate::error::AppError;
use crate::mod_records::{
    self, ActionRecord, HeldPlace, HeldPost, MemberRecord, NewAction, NewReport, OpenReport,
    ReportAbout, Restriction, Settles,
};
use crate::notify;
use crate::permissions::{Audience, at_most_as_visible};
use crate::ratelimit::{Action, Scope};
use crate::sessions;
use crate::social::{self, WatchLevel, Watched};
use crate::store::{self, NewPost, NewThread};
use crate::viewer::{UserCtx, Viewer};
use crate::writing::{self, Outcome, Refused};

pub const MAX_REASON_CHARS: usize = 500;
pub const MAX_DETAILS_CHARS: usize = 2000;
pub const MAX_NOTE_CHARS: usize = 2000;
/// The longest silence or suspension, in days: about ten years, which is
/// as good as for ever.
pub const MAX_DAYS: u32 = 3650;

/// Tidies a reason or note: one line of plain text.
pub fn clean_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Checks a moderator's reason, which every action needs.
pub fn check_reason(reason: &str, problems: &mut Vec<String>) {
    let reason = clean_line(reason);
    if reason.is_empty() {
        problems.push("Give a reason. It goes in the moderation log.".to_owned());
    } else if reason.chars().count() > MAX_REASON_CHARS {
        problems.push(format!("Keep the reason to {MAX_REASON_CHARS} characters."));
    }
}

fn reason_problems(reason: &str) -> Option<Refused> {
    let mut problems = Vec::new();
    check_reason(reason, &mut problems);
    (!problems.is_empty()).then_some(Refused::Invalid(problems))
}

/// The viewer, if they are staff. Visitors are asked to log in, and
/// members are told there's nothing here, so that the moderation pages
/// don't announce themselves.
pub fn staff(viewer: &Viewer) -> Result<&UserCtx, Refused> {
    match viewer.user() {
        None => Err(Refused::LogIn),
        Some(user) if user.is_staff() => Ok(user),
        Some(_) => Err(Refused::NotFound),
    }
}

macro_rules! staff_or_refuse {
    ($viewer:expr) => {
        match staff($viewer) {
            Ok(user) => user,
            Err(refusal) => return Ok(Err(refusal)),
        }
    };
}

/// Parses a list of post numbers such as "12, 14-20 23", each between 1
/// and `max`, in order and without repeats. Ranges may run either way.
pub fn parse_numbers(text: &str, max: i64) -> Result<Vec<i32>, String> {
    let mut numbers = std::collections::BTreeSet::new();
    let bad = || format!("List post numbers from 1 to {max}, such as “12, 14-20”.");
    for part in text
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|p| !p.is_empty())
    {
        let part = part.trim_start_matches('№');
        let (from, to) = match part.split_once(['-', '–']) {
            Some((a, b)) => (a, b),
            None => (part, part),
        };
        let parse = |s: &str| -> Option<i64> {
            s.trim()
                .parse::<i64>()
                .ok()
                .filter(|n| (1..=max).contains(n))
        };
        let (Some(a), Some(b)) = (parse(from), parse(to)) else {
            return Err(bad());
        };
        let (a, b) = (a.min(b), a.max(b));
        if numbers.len() as i64 + (b - a + 1) > 10_000 {
            return Err("Move at most 10,000 posts at once.".to_owned());
        }
        for n in a..=b {
            numbers.insert(i32::try_from(n).map_err(|_| bad())?);
        }
    }
    if numbers.is_empty() {
        return Err(bad());
    }
    Ok(numbers.into_iter().collect())
}

/// The thread a moderator means by what they typed: its number, or a link
/// to it (`/t/123/…`, with or without the site's address).
pub fn parse_thread_ref(text: &str) -> Option<i64> {
    let text = text.trim();
    if let Ok(n) = text.parse::<i64>() {
        return (n >= 1).then_some(n);
    }
    let after = &text[text.find("/t/")? + 3..];
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    let rest = &after[digits.len()..];
    if !(rest.is_empty() || rest.starts_with(['/', '?', '#'])) {
        return None;
    }
    digits.parse::<i64>().ok().filter(|&n| n >= 1)
}

fn is_public(audience: &Audience) -> bool {
    *audience == Audience::Public
}

/// Where a post is, as a notice to its author says: never by its title,
/// since a notice is kept as written while the thread may later move
/// somewhere its author can't read. The page for contesting an action
/// names the thread, to those who may read it then.
fn place_name(thread: &ThreadTarget) -> &'static str {
    match &thread.place {
        Place::Category(_) => "a thread",
        Place::Conversation(_) => "a conversation",
    }
}

/// Tells a member about something a moderator did that affects them.
async fn tell(
    state: &AppState,
    user_id: i64,
    actor: &UserCtx,
    message: String,
    action_id: Option<i64>,
) -> Result<(), AppError> {
    let message: String = message.chars().take(1000).collect();
    notify::moderation(state, user_id, Some(actor.id), &message, action_id).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Reporting

/// Files a report about a post the viewer can read: any post in a
/// category, or a message in one of their conversations, which shows that
/// conversation (and no other) to the moderators.
pub async fn report_post(
    state: &AppState,
    viewer: &Viewer,
    post_id: i64,
    reason: &str,
    details: &str,
) -> Outcome<()> {
    let Some(user) = viewer.user() else {
        return Ok(Err(Refused::LogIn));
    };
    let Some(post) = content::post_target(state.db(), viewer, post_id).await? else {
        return Ok(Err(Refused::NotFound));
    };
    if !post.is_live() {
        return Ok(Err(Refused::NotFound));
    }
    if !user.may_act() {
        return Ok(Err(Refused::NotAllowed(writing::SUSPENDED)));
    }
    if post.author.id == user.id {
        return Ok(Err(Refused::NotAllowed("You can’t report your own post.")));
    }
    if let Some(conversation) = post.thread.conversation()
        && !conversation.includes(user.id)
    {
        // Staff reading a reported conversation act on it instead.
        return Ok(Err(Refused::NotAllowed(
            "Only the people in a conversation can report its messages.",
        )));
    }
    let details = match check_report(reason, details) {
        Ok(details) => details,
        Err(refusal) => return Ok(Err(refusal)),
    };
    if !state
        .limiter()
        .check(Action::Report, Scope::User, &user.id.to_string())
    {
        return Ok(Err(Refused::RateLimited));
    }
    let filed = mod_records::insert_report(
        state.db(),
        &NewReport {
            reporter_id: Some(user.id),
            contact_email: None,
            about: ReportAbout::Post {
                post_id,
                thread_id: post.thread.thread.id,
                author_id: post.author.id,
            },
            reason,
            details: &details,
        },
    )
    .await?;
    if filed.is_none() {
        return Ok(Err(Refused::NotAllowed(
            "You’ve already reported this post. The moderators will look at it.",
        )));
    }
    Ok(Ok(()))
}

fn check_report(reason: &str, details: &str) -> Result<String, Refused> {
    let details = details.trim().replace("\r\n", "\n");
    let mut problems = Vec::new();
    if !mod_records::REASONS.iter().any(|(key, _)| *key == reason) {
        problems.push("Choose a reason.".to_owned());
    }
    if details.chars().count() > MAX_DETAILS_CHARS {
        problems.push(format!(
            "Keep it to {} characters.",
            crate::format::count(MAX_DETAILS_CHARS as i64)
        ));
    }
    if problems.is_empty() {
        Ok(details)
    } else {
        Err(Refused::Invalid(problems))
    }
}

/// Files a report about a member.
pub async fn report_member(
    state: &AppState,
    viewer: &Viewer,
    username: &str,
    reason: &str,
    details: &str,
) -> Outcome<UserRef> {
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
    if !user.may_act() {
        return Ok(Err(Refused::NotAllowed(writing::SUSPENDED)));
    }
    if member.user.id == user.id {
        return Ok(Err(Refused::NotAllowed("You can’t report yourself.")));
    }
    let details = match check_report(reason, details) {
        Ok(details) => details,
        Err(refusal) => return Ok(Err(refusal)),
    };
    if !state
        .limiter()
        .check(Action::Report, Scope::User, &user.id.to_string())
    {
        return Ok(Err(Refused::RateLimited));
    }
    let filed = mod_records::insert_report(
        state.db(),
        &NewReport {
            reporter_id: Some(user.id),
            contact_email: None,
            about: ReportAbout::User {
                user_id: member.user.id,
            },
            reason,
            details: &details,
        },
    )
    .await?;
    if filed.is_none() {
        return Ok(Err(Refused::NotAllowed(
            "You’ve already reported this member. The moderators will look at it.",
        )));
    }
    Ok(Ok(member.user))
}

/// Whether text could be an email address, roughly: something, an `@`,
/// and a domain with a dot, with no spaces.
pub fn plausible_email(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !email.contains(char::is_whitespace)
        && email.len() <= 254
}

/// Sends the moderators a message with the contact form, which works
/// without an account. Limited by the sender's address.
pub async fn contact(
    state: &AppState,
    viewer: &Viewer,
    ip: &str,
    email: &str,
    message: &str,
) -> Outcome<()> {
    let email = email.trim();
    let message = message.trim().replace("\r\n", "\n");
    let mut problems = Vec::new();
    if message.is_empty() {
        problems.push("Write your message.".to_owned());
    } else if message.chars().count() > MAX_DETAILS_CHARS {
        problems.push(format!(
            "Keep it to {} characters.",
            crate::format::count(MAX_DETAILS_CHARS as i64)
        ));
    }
    if !email.is_empty() && !plausible_email(email) {
        problems.push("That doesn’t look like an email address.".to_owned());
    }
    if !problems.is_empty() {
        return Ok(Err(Refused::Invalid(problems)));
    }
    if !state.limiter().check(Action::Contact, Scope::Ip, ip) {
        return Ok(Err(Refused::RateLimited));
    }
    let email = if email.is_empty() {
        viewer.user().map(|u| u.email.clone())
    } else {
        Some(email.to_owned())
    };
    mod_records::insert_report(
        state.db(),
        &NewReport {
            reporter_id: viewer.user_id(),
            contact_email: email.as_deref(),
            about: ReportAbout::Contact,
            reason: "contact",
            details: &message,
        },
    )
    .await?;
    Ok(Ok(()))
}

/// An action the viewer may contest: one that affected them (see
/// [`ActionRecord::affected`]) that they haven't contested yet.
pub async fn appealable(
    state: &AppState,
    viewer: &Viewer,
    action_id: i64,
) -> Outcome<ActionRecord> {
    let Some(user) = viewer.user() else {
        return Ok(Err(Refused::LogIn));
    };
    let Some(action) = mod_records::action(state.db(), action_id).await? else {
        return Ok(Err(Refused::NotFound));
    };
    if !action.affected().contains(&user.id) || !appeals_allowed(&action.action) {
        return Ok(Err(Refused::NotFound));
    }
    if mod_records::appealed_by(state.db(), action.id, user.id).await? {
        return Ok(Err(Refused::NotAllowed(
            "You’ve already contested this. The moderators will look at it again.",
        )));
    }
    Ok(Ok(action))
}

/// Which actions their members may contest: those taken against them.
fn appeals_allowed(action: &str) -> bool {
    matches!(
        action,
        "hide_post"
            | "delete_post"
            | "edit_post"
            | "silence"
            | "suspend"
            | "suspend_branch"
            | "reject_post"
            | "delete_thread"
            | "lock_thread"
            | "redact_revision"
    )
}

/// The viewer contests an action taken against them. Suspended members
/// can do this, and nothing else.
pub async fn appeal(
    state: &AppState,
    viewer: &Viewer,
    action_id: i64,
    details: &str,
) -> Outcome<()> {
    let action = match appealable(state, viewer, action_id).await? {
        Ok(action) => action,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let user = viewer.user().expect("checked above");
    let details = details.trim().replace("\r\n", "\n");
    if details.is_empty() || details.chars().count() > MAX_DETAILS_CHARS {
        return Ok(Err(Refused::Invalid(vec![format!(
            "Say why, in up to {} characters.",
            crate::format::count(MAX_DETAILS_CHARS as i64)
        )])));
    }
    if !state
        .limiter()
        .check(Action::Report, Scope::User, &user.id.to_string())
    {
        return Ok(Err(Refused::RateLimited));
    }
    mod_records::insert_report(
        state.db(),
        &NewReport {
            reporter_id: Some(user.id),
            contact_email: None,
            about: ReportAbout::Appeal {
                action_id: action.id,
                user_id: user.id,
            },
            reason: "appeal",
            details: &details,
        },
    )
    .await?;
    Ok(Ok(()))
}

// ---------------------------------------------------------------------------
// The queue

/// Something waiting for a moderator.
pub struct QueueItem {
    pub first_at: chrono::DateTime<Utc>,
    pub kind: QueueKind,
}

pub enum QueueKind {
    /// Reports about a post, which the moderator can read (a reported
    /// message's conversation is open to staff).
    Post {
        post: Box<PostTarget>,
        author: Box<MemberRecord>,
        reports: Vec<OpenReport>,
    },
    /// Reports about a post that has gone since (deleted, or its thread
    /// was).
    PostGone {
        post_id: i64,
        reports: Vec<OpenReport>,
    },
    /// Reports about a member.
    Member {
        member: Box<MemberRecord>,
        reports: Vec<OpenReport>,
    },
    /// A message sent with the contact form.
    Contact { report: OpenReport },
    /// A member contesting an action.
    Appeal {
        report: OpenReport,
        action: Box<content::moderation::LogEntry>,
        member: Option<Box<MemberRecord>>,
    },
    /// A new member's post waiting for approval.
    Held {
        held: Box<HeldPost>,
        author: Box<MemberRecord>,
        /// Where it would go: the category or thread, as a link.
        place: (String, String),
    },
}

/// Everything waiting for a moderator, oldest first. Several reports about
/// one post or one member are one item.
pub async fn queue(state: &AppState, viewer: &Viewer) -> Result<Vec<QueueItem>, AppError> {
    let db = state.db();
    let mut items: Vec<QueueItem> = Vec::new();
    let mut by_post: Vec<(i64, Vec<OpenReport>)> = Vec::new();
    let mut by_user: Vec<(i64, Vec<OpenReport>)> = Vec::new();
    for report in mod_records::open_reports(db).await? {
        match report.kind.as_str() {
            "post" => {
                let post_id = report.post_id.expect("post reports have posts");
                match by_post.iter_mut().find(|(id, _)| *id == post_id) {
                    Some((_, reports)) => reports.push(report),
                    None => by_post.push((post_id, vec![report])),
                }
            }
            "user" => {
                let user_id = report.user_id.expect("member reports have members");
                match by_user.iter_mut().find(|(id, _)| *id == user_id) {
                    Some((_, reports)) => reports.push(report),
                    None => by_user.push((user_id, vec![report])),
                }
            }
            "appeal" => {
                let action_id = report.mod_action_id.expect("appeals have actions");
                let Some(action) = content::moderation::entry(db, viewer, action_id).await? else {
                    continue;
                };
                let member = match &action.user {
                    Some(user) => mod_records::member_record(db, &user.username)
                        .await?
                        .map(Box::new),
                    None => None,
                };
                items.push(QueueItem {
                    first_at: report.created_at,
                    kind: QueueKind::Appeal {
                        report,
                        action: Box::new(action),
                        member,
                    },
                });
            }
            _ => items.push(QueueItem {
                first_at: report.created_at,
                kind: QueueKind::Contact { report },
            }),
        }
    }
    for (post_id, reports) in by_post {
        let first_at = reports[0].created_at;
        let post = content::post_target(db, viewer, post_id).await?;
        let kind = match post.filter(|p| p.body.is_some()) {
            Some(post) => match mod_records::member_record(db, &post.author.username).await? {
                Some(author) => QueueKind::Post {
                    post: Box::new(post),
                    author: Box::new(author),
                    reports,
                },
                None => QueueKind::PostGone { post_id, reports },
            },
            None => QueueKind::PostGone { post_id, reports },
        };
        items.push(QueueItem { first_at, kind });
    }
    for (user_id, reports) in by_user {
        let first_at = reports[0].created_at;
        let Some(member) = member_by_id(state, user_id).await? else {
            continue;
        };
        items.push(QueueItem {
            first_at,
            kind: QueueKind::Member {
                member: Box::new(member),
                reports,
            },
        });
    }
    for held in mod_records::waiting_held(db).await? {
        let Some(author) = mod_records::member_record(db, &held.author.username).await? else {
            continue;
        };
        let place = held_place(state, viewer, &held.place).await?;
        items.push(QueueItem {
            first_at: held.created_at,
            kind: QueueKind::Held {
                held: Box::new(held),
                author: Box::new(author),
                place,
            },
        });
    }
    items.sort_by_key(|item| item.first_at);
    Ok(items)
}

/// Where a held post would go, as a label and a link.
pub async fn held_place(
    state: &AppState,
    viewer: &Viewer,
    place: &HeldPlace,
) -> Result<(String, String), AppError> {
    Ok(match place {
        HeldPlace::Thread {
            category_id, title, ..
        } => match content::category_by_id(state.db(), viewer, *category_id).await? {
            Some(c) => (
                format!("{} › new thread “{title}”", c.category.name),
                format!("/c/{}", c.category.slug),
            ),
            None => (format!("new thread “{title}”"), "/".to_owned()),
        },
        HeldPlace::Reply { thread_id, .. } => {
            match content::thread_target(state.db(), viewer, *thread_id).await? {
                Some(t) => {
                    let category = t
                        .category()
                        .map(|c| format!("{} › ", c.category.name))
                        .unwrap_or_default();
                    (format!("{category}reply in “{}”", t.name), t.href())
                }
                None => (
                    "a reply in a thread that has gone".to_owned(),
                    "/".to_owned(),
                ),
            }
        }
    })
}

async fn member_by_id(state: &AppState, user_id: i64) -> Result<Option<MemberRecord>, AppError> {
    Ok(match sessions::user_ctx(state.db(), user_id).await? {
        Some(user) => mod_records::member_record(state.db(), &user.username).await?,
        None => None,
    })
}

// ---------------------------------------------------------------------------
// Settling reports

/// Settles the reports a decision answers (if `report` names one), and
/// records the dismissal when nothing else was done.
async fn settle_reports(
    state: &AppState,
    user: &UserCtx,
    which: Option<Settles>,
    resolution: &str,
) -> Result<(), AppError> {
    if let Some(which) = which {
        mod_records::settle(state.db(), which, false, user.id, resolution).await?;
    }
    Ok(())
}

/// Dismisses a report (with the others about the same post or member):
/// nothing needs doing.
pub async fn dismiss(
    state: &AppState,
    viewer: &Viewer,
    report_id: i64,
    reason: &str,
) -> Outcome<()> {
    let user = staff_or_refuse!(viewer);
    let Some(report) = mod_records::report(state.db(), report_id).await? else {
        return Ok(Err(Refused::NotFound));
    };
    let reason = clean_line(reason);
    let reason = if reason.is_empty() {
        "No action needed".to_owned()
    } else {
        reason
    };
    if let Some(refusal) = reason_problems(&reason) {
        return Ok(Err(refusal));
    }
    let which = settles_for(&report);
    mod_records::settle(state.db(), which, true, user.id, &reason).await?;
    mod_records::record(
        state.db(),
        &NewAction {
            actor_id: user.id,
            action: "dismiss_report",
            target_kind: "report",
            target_id: report.id,
            user_id: report.user_id,
            thread_id: report.thread_id,
            post_id: report.post_id,
            reason: &reason,
            public: false,
            details: json!({ "kind": report.kind }),
        },
    )
    .await?;
    state.changed(Change::ModLogged).await?;
    Ok(Ok(()))
}

fn settles_for(report: &OpenReport) -> Settles {
    match (report.kind.as_str(), report.post_id, report.user_id) {
        ("post", Some(post_id), _) => Settles::Post(post_id),
        ("user", _, Some(user_id)) => Settles::User(user_id),
        _ => Settles::Report(report.id),
    }
}

/// The reports a decision taken from the queue answers: those about
/// report `report_id`'s post or member.
async fn answering(state: &AppState, report_id: Option<i64>) -> Result<Option<Settles>, AppError> {
    Ok(match report_id {
        Some(id) => mod_records::report(state.db(), id)
            .await?
            .map(|r| settles_for(&r)),
        None => None,
    })
}

// ---------------------------------------------------------------------------
// Posts

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostAction {
    /// Hide it from everyone but staff.
    Hide,
    /// Show a hidden post again.
    Unhide,
    /// Delete it for everyone, with its history.
    Delete,
    /// Undo a moderator's deletion.
    Restore,
}

impl PostAction {
    pub fn parse(s: &str) -> Option<PostAction> {
        Some(match s {
            "hide" => PostAction::Hide,
            "unhide" => PostAction::Unhide,
            "delete" => PostAction::Delete,
            "restore" => PostAction::Restore,
            _ => return None,
        })
    }

    fn key(self) -> &'static str {
        match self {
            PostAction::Hide => "hide_post",
            PostAction::Unhide => "unhide_post",
            PostAction::Delete => "delete_post",
            PostAction::Restore => "restore_post",
        }
    }
}

/// A post as staff see it, deleted or not, for moderating.
pub async fn post_for_staff(
    state: &AppState,
    viewer: &Viewer,
    post_id: i64,
) -> Outcome<PostTarget> {
    staff_or_refuse!(viewer);
    match content::post_target(state.db(), viewer, post_id).await? {
        Some(post) => Ok(Ok(post)),
        None => Ok(Err(Refused::NotFound)),
    }
}

/// Hides, shows, deletes or restores a post.
pub async fn moderate_post(
    state: &AppState,
    viewer: &Viewer,
    post_id: i64,
    action: PostAction,
    reason: &str,
    report: Option<i64>,
) -> Outcome<PostTarget> {
    let user = staff_or_refuse!(viewer);
    let Some(post) = content::post_target(state.db(), viewer, post_id).await? else {
        return Ok(Err(Refused::NotFound));
    };
    let reason = clean_line(reason);
    if let Some(refusal) = reason_problems(&reason) {
        return Ok(Err(refusal));
    }
    let db = state.db();
    let changed = match action {
        PostAction::Hide => mod_records::set_hidden(db, post_id, Some(user.id)).await?,
        PostAction::Unhide => mod_records::set_hidden(db, post_id, None).await?,
        PostAction::Delete => mod_records::set_deleted(db, post_id, user.id, false).await?,
        PostAction::Restore => mod_records::set_deleted(db, post_id, user.id, true).await?,
    };
    if !changed {
        return Ok(Err(Refused::NotAllowed(match action {
            PostAction::Hide => "That post is already hidden or deleted.",
            PostAction::Unhide => "That post isn’t hidden.",
            PostAction::Delete => "That post is already deleted.",
            PostAction::Restore => "Only posts a moderator deleted can be restored.",
        })));
    }
    let action_id = mod_records::record(
        db,
        &NewAction {
            actor_id: user.id,
            action: action.key(),
            target_kind: "post",
            target_id: post_id,
            user_id: Some(post.author.id),
            thread_id: Some(post.thread.thread.id),
            post_id: Some(post_id),
            reason: &reason,
            public: is_public(&post.thread.audience()),
            details: json!({ "seq": post.seq }),
        },
    )
    .await?;
    let which = answering(state, report).await?.or(match action {
        PostAction::Hide | PostAction::Delete => Some(Settles::Post(post_id)),
        _ => None,
    });
    settle_reports(state, user, which, action.key()).await?;
    if post.author.id != user.id {
        let verb = match action {
            PostAction::Hide => "hid",
            PostAction::Unhide => "showed again",
            PostAction::Delete => "deleted",
            PostAction::Restore => "restored",
        };
        let contestable = matches!(action, PostAction::Hide | PostAction::Delete);
        tell(
            state,
            post.author.id,
            user,
            format!(
                "A moderator {verb} your post № {} in {}. Reason: {reason}",
                post.seq,
                place_name(&post.thread)
            ),
            contestable.then_some(action_id),
        )
        .await?;
    }
    state.changed(Change::PostRemoved { post_id }).await?;
    state.changed(Change::ModLogged).await?;
    Ok(Ok(post))
}

/// Records a staff member's edit of someone else's post (made by
/// [`writing::edit`]), and tells its author.
pub async fn edited_post(
    state: &AppState,
    user: &UserCtx,
    post: &PostTarget,
    reason: &str,
) -> Result<(), AppError> {
    let reason = clean_line(reason);
    let action_id = mod_records::record(
        state.db(),
        &NewAction {
            actor_id: user.id,
            action: "edit_post",
            target_kind: "post",
            target_id: post.post_id,
            user_id: Some(post.author.id),
            thread_id: Some(post.thread.thread.id),
            post_id: Some(post.post_id),
            reason: &reason,
            public: is_public(&post.thread.audience()),
            details: json!({ "seq": post.seq }),
        },
    )
    .await?;
    tell(
        state,
        post.author.id,
        user,
        format!(
            "A moderator edited your post № {} in {}. Reason: {reason}",
            post.seq,
            place_name(&post.thread)
        ),
        Some(action_id),
    )
    .await?;
    state.changed(Change::ModLogged).await?;
    Ok(())
}

/// Redacts an earlier version of a post: its text is removed for everyone.
pub async fn redact(
    state: &AppState,
    viewer: &Viewer,
    post_id: i64,
    rev: i32,
    reason: &str,
) -> Outcome<PostTarget> {
    let user = staff_or_refuse!(viewer);
    let Some(post) = content::post_target(state.db(), viewer, post_id).await? else {
        return Ok(Err(Refused::NotFound));
    };
    let reason = clean_line(reason);
    if let Some(refusal) = reason_problems(&reason) {
        return Ok(Err(refusal));
    }
    if !mod_records::redact_revision(state.db(), post_id, rev, user.id).await? {
        return Ok(Err(Refused::NotAllowed(
            "There’s no such earlier version, or it is already redacted. To change the current version, edit the post.",
        )));
    }
    let action_id = mod_records::record(
        state.db(),
        &NewAction {
            actor_id: user.id,
            action: "redact_revision",
            target_kind: "revision",
            target_id: post_id,
            user_id: Some(post.author.id),
            thread_id: Some(post.thread.thread.id),
            post_id: Some(post_id),
            reason: &reason,
            public: is_public(&post.thread.audience()),
            details: json!({ "seq": post.seq, "rev": rev }),
        },
    )
    .await?;
    if post.author.id != user.id {
        tell(
            state,
            post.author.id,
            user,
            format!(
                "A moderator redacted version {rev} of your post № {} in {}. Reason: {reason}",
                post.seq,
                place_name(&post.thread)
            ),
            Some(action_id),
        )
        .await?;
    }
    state.changed(Change::PostEdited { post_id }).await?;
    state.changed(Change::ModLogged).await?;
    Ok(Ok(post))
}

// ---------------------------------------------------------------------------
// Images

/// Takes an image down everywhere (DESIGN.md §8, §9): from every post
/// that shows it, quoted or not, and as anyone's picture. Its file is
/// deleted and it can't be uploaded again. `name` is the image's file
/// name, as in its address.
pub async fn remove_image(
    state: &AppState,
    viewer: &Viewer,
    name: &str,
    reason: &str,
) -> Outcome<()> {
    let user = staff_or_refuse!(viewer);
    let Some((sha256, ext)) = crate::uploads::parse_name(name) else {
        return Ok(Err(Refused::NotFound));
    };
    let reason = clean_line(reason);
    if let Some(refusal) = reason_problems(&reason) {
        return Ok(Err(refusal));
    }
    let mut tx = state.db().begin().await?;
    let Some(removed) = crate::upload_records::remove(&mut tx, sha256, ext, user.id).await? else {
        return Ok(Err(Refused::NotFound));
    };
    // Images may be in conversations and groups' categories, so the entry
    // is for staff only.
    let action_id = mod_records::record(
        &mut tx,
        &NewAction {
            actor_id: user.id,
            action: "remove_image",
            target_kind: "upload",
            target_id: removed.upload_id,
            user_id: removed.uploader_id,
            thread_id: None,
            post_id: None,
            reason: &reason,
            public: false,
            details: json!({ "sha256": sha256, "ext": ext }),
        },
    )
    .await?;
    tx.commit().await?;
    crate::uploads::remove(&state.config().uploads_dir, sha256, ext).await?;
    if let Some(uploader) = removed.uploader_id
        && uploader != user.id
    {
        tell(
            state,
            uploader,
            user,
            format!("A moderator removed an image you uploaded. Reason: {reason}"),
            Some(action_id),
        )
        .await?;
    }
    for user_id in removed.avatar_user_ids {
        state.changed(Change::UserChanged { user_id }).await?;
    }
    state.changed(Change::ModLogged).await?;
    Ok(Ok(()))
}

// ---------------------------------------------------------------------------
// Threads

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThreadAction {
    Lock,
    Unlock,
    Pin,
    Unpin,
    Rename(String),
    /// Move to the category with this slug.
    Move(String),
    Delete,
    Restore,
}

/// Locks, pins, renames, moves or deletes a thread (or undoes it).
pub async fn moderate_thread(
    state: &AppState,
    viewer: &Viewer,
    thread_id: i64,
    action: ThreadAction,
    reason: &str,
) -> Outcome<i64> {
    let user = staff_or_refuse!(viewer);
    let db = state.db();
    let reason = clean_line(reason);
    // A deleted thread can only be restored, from its page in the
    // moderation pages, which the log links to.
    let thread = match &action {
        ThreadAction::Restore => None,
        _ => match content::thread_target(db, viewer, thread_id).await? {
            Some(t) if t.category().is_some() => Some(t),
            _ => return Ok(Err(Refused::NotFound)),
        },
    };
    let mut problems = Vec::new();
    check_reason(&reason, &mut problems);
    let mut destination = None;
    match &action {
        ThreadAction::Rename(title) => {
            writing::check_title(&writing::clean_title(title), &mut problems)
        }
        ThreadAction::Move(slug) => {
            destination = content::category_target(db, viewer, slug).await?;
            match (&destination, &thread) {
                (None, _) => problems.push("Choose a category to move it to.".to_owned()),
                (Some(to), Some(t))
                    if Some(to.category.id) == t.category().map(|c| c.category.id) =>
                {
                    problems.push("It is already in that category.".to_owned())
                }
                _ => {}
            }
        }
        _ => {}
    }
    if !problems.is_empty() {
        return Ok(Err(Refused::Invalid(problems)));
    }
    let audience = thread.as_ref().map(ThreadTarget::audience);
    let mut previous_category_id = None;
    let mut details = json!({});
    let (key, changed) = match &action {
        ThreadAction::Lock => (
            "lock_thread",
            mod_records::set_locked(db, thread_id, true).await?,
        ),
        ThreadAction::Unlock => (
            "unlock_thread",
            mod_records::set_locked(db, thread_id, false).await?,
        ),
        ThreadAction::Pin => (
            "pin_thread",
            mod_records::set_pinned(db, thread_id, true).await?,
        ),
        ThreadAction::Unpin => (
            "unpin_thread",
            mod_records::set_pinned(db, thread_id, false).await?,
        ),
        ThreadAction::Rename(title) => {
            let title = writing::clean_title(title);
            let old = mod_records::rename_thread(db, thread_id, &title).await?;
            if let Some(old) = &old {
                details = json!({ "old_title": old, "new_title": title });
            }
            ("rename_thread", old.is_some())
        }
        ThreadAction::Move(_) => {
            let to = destination.as_ref().expect("checked above");
            previous_category_id = store::move_thread(db, thread_id, to.category.id).await?;
            details = json!({ "from_category_id": previous_category_id, "to_category_id": to.category.id });
            ("move_thread", previous_category_id.is_some())
        }
        ThreadAction::Delete => (
            "delete_thread",
            mod_records::set_thread_deleted(db, thread_id, false).await?,
        ),
        ThreadAction::Restore => (
            "restore_thread",
            mod_records::set_thread_deleted(db, thread_id, true).await?,
        ),
    };
    if !changed {
        return Ok(Err(Refused::NotAllowed("Nothing needed changing.")));
    }
    // Restoring: find it now that it can be read again.
    let thread = match thread {
        Some(t) => t,
        None => match content::thread_target(db, viewer, thread_id).await? {
            Some(t) => t,
            None => return Ok(Err(Refused::NotFound)),
        },
    };
    let after = thread.audience();
    let public = is_public(&audience.unwrap_or(after.clone()))
        && match &destination {
            Some(to) => is_public(&to.audience()),
            None => true,
        };
    let author_id = content::moderation::thread_author(db, thread_id).await?;
    let action_id = mod_records::record(
        db,
        &NewAction {
            actor_id: user.id,
            action: key,
            target_kind: "thread",
            target_id: thread_id,
            user_id: author_id,
            thread_id: Some(thread_id),
            post_id: None,
            reason: &reason,
            public,
            details,
        },
    )
    .await?;
    if let Some(author_id) = author_id.filter(|&a| a != user.id)
        && matches!(action, ThreadAction::Delete | ThreadAction::Lock)
    {
        let verb = if action == ThreadAction::Delete {
            "deleted"
        } else {
            "locked"
        };
        tell(
            state,
            author_id,
            user,
            format!("A moderator {verb} one of your threads. Reason: {reason}"),
            Some(action_id),
        )
        .await?;
    }
    state
        .changed(Change::ThreadChanged {
            thread_id,
            previous_category_id,
        })
        .await?;
    state.changed(Change::ModLogged).await?;
    Ok(Ok(thread_id))
}

/// Where posts are moved to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Destination {
    /// An existing thread, as the moderator typed it (see
    /// [`parse_thread_ref`]).
    Existing(String),
    /// A new thread with this title, in the category with this slug.
    New { category: String, title: String },
}

/// Moves the posts with these numbers out of a thread, to the end of
/// another thread or into a new one (DESIGN.md §8: this covers splitting
/// and merging). See [`mod_records::move_posts`] for how numbers and
/// permalinks stay coherent. Returns the destination thread.
pub async fn move_posts(
    state: &AppState,
    viewer: &Viewer,
    thread_id: i64,
    numbers: &str,
    destination: &Destination,
    reason: &str,
) -> Outcome<i64> {
    let user = staff_or_refuse!(viewer);
    let db = state.db();
    let Some(from) = content::thread_target(db, viewer, thread_id).await? else {
        return Ok(Err(Refused::NotFound));
    };
    if from.category().is_none() {
        return Ok(Err(Refused::NotAllowed(
            "Messages can’t be moved out of a conversation.",
        )));
    }
    let reason = clean_line(reason);
    let mut problems = Vec::new();
    check_reason(&reason, &mut problems);
    let seqs = match parse_numbers(numbers, from.last_seq) {
        Ok(seqs) => seqs,
        Err(problem) => {
            problems.push(problem);
            Vec::new()
        }
    };
    let posts = mod_records::posts_at(db, thread_id, &seqs).await?;
    if !seqs.is_empty() && posts.is_empty() {
        problems.push("None of those numbers has a post in this thread.".to_owned());
    }
    // Where to: an existing thread the moderator can read, or a new one.
    enum To {
        Existing(ThreadTarget),
        New(content::CategoryTarget, String),
    }
    let to = match destination {
        Destination::Existing(text) => match parse_thread_ref(text) {
            Some(id) if id == thread_id => {
                problems.push("Choose a different thread to move them to.".to_owned());
                None
            }
            Some(id) => match content::thread_target(db, viewer, id).await? {
                Some(t) if t.category().is_some() => Some(To::Existing(t)),
                Some(_) => {
                    problems.push("Posts can’t be moved into a conversation.".to_owned());
                    None
                }
                None => {
                    problems.push("There’s no such thread.".to_owned());
                    None
                }
            },
            None => {
                problems.push("Give the number of the thread, or a link to it.".to_owned());
                None
            }
        },
        Destination::New { category, title } => {
            let title = writing::clean_title(title);
            writing::check_title(&title, &mut problems);
            match content::category_target(db, viewer, category).await? {
                Some(c) => Some(To::New(c, title)),
                None => {
                    problems.push("Choose a category for the new thread.".to_owned());
                    None
                }
            }
        }
    };
    let Some(to) = to.filter(|_| problems.is_empty()) else {
        return Ok(Err(Refused::Invalid(problems)));
    };
    let mut tx = db.begin().await?;
    let (to_id, to_audience) = match &to {
        To::Existing(t) => (t.thread.id, t.audience()),
        To::New(category, title) => {
            let (_, first_author, first_at) = posts[0];
            let id = mod_records::empty_thread(
                &mut tx,
                category.category.id,
                title,
                first_author,
                first_at,
            )
            .await?;
            (id, category.audience())
        }
    };
    let ids: Vec<i64> = posts.iter().map(|(id, _, _)| *id).collect();
    let moved = mod_records::move_posts(&mut tx, thread_id, &ids, to_id, user.id).await?;
    mod_records::record(
        &mut tx,
        &NewAction {
            actor_id: user.id,
            action: "move_posts",
            target_kind: "thread",
            target_id: thread_id,
            user_id: None,
            thread_id: Some(thread_id),
            post_id: None,
            reason: &reason,
            public: is_public(&from.audience()) && is_public(&to_audience),
            details: json!({
                "count": moved.posts.len(),
                "to_thread_id": to_id,
                "new_thread": matches!(to, To::New(..)),
                "emptied": moved.emptied,
            }),
        },
    )
    .await?;
    tx.commit().await?;
    for thread in [thread_id, to_id] {
        state
            .changed(Change::ThreadChanged {
                thread_id: thread,
                previous_category_id: None,
            })
            .await?;
    }
    // Quote-forks of the moved posts name the thread they are in.
    for (post_id, _) in &moved.posts {
        state
            .changed(Change::PostEdited { post_id: *post_id })
            .await?;
    }
    state.changed(Change::ModLogged).await?;
    Ok(Ok(to_id))
}

// ---------------------------------------------------------------------------
// Members

/// A member as moderators see them, by username.
pub async fn member(state: &AppState, viewer: &Viewer, username: &str) -> Outcome<MemberRecord> {
    staff_or_refuse!(viewer);
    match mod_records::member_record(state.db(), username).await? {
        Some(member) => Ok(Ok(member)),
        None => Ok(Err(Refused::NotFound)),
    }
}

async fn target_member(
    state: &AppState,
    viewer: &Viewer,
    username: &str,
) -> Result<Result<MemberRecord, Refused>, AppError> {
    let member = match member(state, viewer, username).await? {
        Ok(member) => member,
        Err(refusal) => return Ok(Err(refusal)),
    };
    if member.deleted {
        return Ok(Err(Refused::NotFound));
    }
    if member.role.is_staff() {
        return Ok(Err(Refused::NotAllowed(
            "Moderators and admins can’t be silenced, suspended or deleted. An admin can take their role away first.",
        )));
    }
    Ok(Ok(member))
}

/// How long, from a number of days typed into a form.
pub fn parse_days(text: &str) -> Option<u32> {
    text.trim()
        .parse::<u32>()
        .ok()
        .filter(|d| (1..=MAX_DAYS).contains(d))
}

fn days_problem() -> String {
    format!("Choose how many days, from 1 to {MAX_DAYS}.")
}

/// Silences or suspends a member for `days` days, or (with `None`) lifts
/// it early.
pub async fn restrict_member(
    state: &AppState,
    viewer: &Viewer,
    username: &str,
    restriction: Restriction,
    days: Option<u32>,
    reason: &str,
    report: Option<i64>,
) -> Outcome<MemberRecord> {
    let user = staff_or_refuse!(viewer);
    let member = match target_member(state, viewer, username).await? {
        Ok(member) => member,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let reason = clean_line(reason);
    if let Some(refusal) = reason_problems(&reason) {
        return Ok(Err(refusal));
    }
    let until = days.map(|d| Utc::now() + Duration::days(i64::from(d)));
    if days.is_none() {
        let active = match restriction {
            Restriction::Silence => member.is_silenced(),
            Restriction::Suspend => member.is_suspended(),
        };
        if !active {
            return Ok(Err(Refused::NotAllowed("There’s nothing to lift.")));
        }
    }
    let mut tx = state.db().begin().await?;
    mod_records::restrict(&mut tx, &[member.user.id], restriction, until).await?;
    let key = match (restriction, days) {
        (Restriction::Silence, Some(_)) => "silence",
        (Restriction::Suspend, Some(_)) => "suspend",
        (Restriction::Silence, None) => "unsilence",
        (Restriction::Suspend, None) => "unsuspend",
    };
    let action_id = mod_records::record(
        &mut tx,
        &NewAction {
            actor_id: user.id,
            action: key,
            target_kind: "user",
            target_id: member.user.id,
            user_id: Some(member.user.id),
            thread_id: None,
            post_id: None,
            reason: &reason,
            public: true,
            details: json!({ "days": days, "until": until.map(|u| u.to_rfc3339()) }),
        },
    )
    .await?;
    tx.commit().await?;
    let which = answering(state, report)
        .await?
        .or(days.map(|_| Settles::User(member.user.id)));
    settle_reports(state, user, which, key).await?;
    let message = match (restriction, until) {
        (Restriction::Silence, Some(until)) => format!(
            "A moderator silenced your account until {}: you can read, but not post, edit, react or send messages. Reason: {reason}",
            crate::format::datetime(&until)
        ),
        (Restriction::Suspend, Some(until)) => format!(
            "A moderator suspended your account until {}. Reason: {reason}",
            crate::format::datetime(&until)
        ),
        (Restriction::Silence, None) => format!("A moderator lifted your silence. {reason}"),
        (Restriction::Suspend, None) => format!("A moderator lifted your suspension. {reason}"),
    };
    tell(
        state,
        member.user.id,
        user,
        message,
        days.map(|_| action_id),
    )
    .await?;
    // What they can read has changed; their open pages hear about it.
    notify::counts_changed(state, member.user.id);
    state.changed(Change::ModLogged).await?;
    Ok(Ok(member))
}

/// Suspends a member and everyone below them in the invite tree (staff
/// excepted), and revokes the invites they could still use.
pub async fn suspend_branch(
    state: &AppState,
    viewer: &Viewer,
    username: &str,
    days: u32,
    reason: &str,
    report: Option<i64>,
) -> Outcome<Vec<UserRef>> {
    let user = staff_or_refuse!(viewer);
    let member = match target_member(state, viewer, username).await? {
        Ok(member) => member,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let reason = clean_line(reason);
    if let Some(refusal) = reason_problems(&reason) {
        return Ok(Err(refusal));
    }
    if parse_days(&days.to_string()).is_none() {
        return Ok(Err(Refused::Invalid(vec![days_problem()])));
    }
    let branch = mod_records::invite_branch(state.db(), member.user.id).await?;
    let ids: Vec<i64> = branch.iter().map(|u| u.id).collect();
    let until = Utc::now() + Duration::days(i64::from(days));
    let mut tx = state.db().begin().await?;
    let suspended = mod_records::restrict(&mut tx, &ids, Restriction::Suspend, Some(until)).await?;
    mod_records::revoke_invites_of(&mut tx, &ids).await?;
    let action_id = mod_records::record(
        &mut tx,
        &NewAction {
            actor_id: user.id,
            action: "suspend_branch",
            target_kind: "user",
            target_id: member.user.id,
            user_id: Some(member.user.id),
            thread_id: None,
            post_id: None,
            reason: &reason,
            public: true,
            details: json!({ "days": days, "until": until.to_rfc3339(), "user_ids": suspended }),
        },
    )
    .await?;
    tx.commit().await?;
    let which = answering(state, report)
        .await?
        .or(Some(Settles::User(member.user.id)));
    settle_reports(state, user, which, "suspend_branch").await?;
    for id in &suspended {
        tell(
            state,
            *id,
            user,
            format!(
                "A moderator suspended your account until {}, with everyone {} invited. Reason: {reason}",
                crate::format::datetime(&until),
                member.user.username
            ),
            Some(action_id),
        )
        .await?;
        notify::counts_changed(state, *id);
    }
    // Their invites were revoked, which the invite pages show.
    state.changed(Change::SiteChanged).await?;
    Ok(Ok(branch
        .into_iter()
        .filter(|u| suspended.contains(&u.id))
        .collect()))
}

/// What deleting a member as a spammer would remove, to ask about first.
pub struct SpammerPreview {
    pub member: MemberRecord,
    /// The threads they started, which go with everything in them.
    pub threads: i64,
    /// The replies other members wrote in those threads.
    pub replies: i64,
}

/// What deleting a member as a spammer would remove, if they can be.
pub async fn spammer_preview(
    state: &AppState,
    viewer: &Viewer,
    username: &str,
) -> Outcome<SpammerPreview> {
    let member = match target_member(state, viewer, username).await? {
        Ok(member) => member,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let (threads, replies) = mod_records::spammer_reach(state.db(), member.user.id).await?;
    Ok(Ok(SpammerPreview {
        member,
        threads,
        replies,
    }))
}

/// Deletes a spammer: their account and everything they wrote. It can't be
/// undone, so the pages ask first (see [`spammer_preview`]).
pub async fn delete_spammer(
    state: &AppState,
    viewer: &Viewer,
    username: &str,
    reason: &str,
) -> Outcome<UserRef> {
    let user = staff_or_refuse!(viewer);
    let member = match target_member(state, viewer, username).await? {
        Ok(member) => member,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let reason = clean_line(reason);
    if let Some(refusal) = reason_problems(&reason) {
        return Ok(Err(refusal));
    }
    let mut tx = state.db().begin().await?;
    let Some(removed) = mod_records::delete_spammer(&mut tx, member.user.id, user.id).await? else {
        return Ok(Err(Refused::NotFound));
    };
    mod_records::record(
        &mut tx,
        &NewAction {
            actor_id: user.id,
            action: "delete_spammer",
            target_kind: "user",
            target_id: member.user.id,
            user_id: Some(member.user.id),
            thread_id: None,
            post_id: None,
            reason: &reason,
            public: true,
            details: json!({ "posts": removed.post_ids.len(), "threads": removed.thread_ids.len() }),
        },
    )
    .await?;
    mod_records::settle(
        &mut tx,
        Settles::User(member.user.id),
        false,
        user.id,
        "delete_spammer",
    )
    .await?;
    for post_id in &removed.post_ids {
        mod_records::settle(
            &mut tx,
            Settles::Post(*post_id),
            false,
            user.id,
            "delete_spammer",
        )
        .await?;
    }
    tx.commit().await?;
    // Their posts and threads were anywhere, and their name was in the
    // member list: start the cache afresh.
    state.changed(Change::SiteChanged).await?;
    Ok(Ok(member.user))
}

/// Adds a staff-only note about a member.
pub async fn add_note(
    state: &AppState,
    viewer: &Viewer,
    username: &str,
    body: &str,
) -> Outcome<MemberRecord> {
    let user = staff_or_refuse!(viewer);
    let member = match member(state, viewer, username).await? {
        Ok(member) => member,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let body = body.trim().replace("\r\n", "\n");
    if body.is_empty() || body.chars().count() > MAX_NOTE_CHARS {
        return Ok(Err(Refused::Invalid(vec![format!(
            "Write a note of up to {} characters.",
            crate::format::count(MAX_NOTE_CHARS as i64)
        )])));
    }
    mod_records::add_note(state.db(), member.user.id, user.id, &body).await?;
    mod_records::record(
        state.db(),
        &NewAction {
            actor_id: user.id,
            action: "add_note",
            target_kind: "user",
            target_id: member.user.id,
            user_id: Some(member.user.id),
            thread_id: None,
            post_id: None,
            reason: "",
            public: false,
            details: json!({}),
        },
    )
    .await?;
    Ok(Ok(member))
}

// ---------------------------------------------------------------------------
// Held posts

/// Approves a held post: it is posted now, as its author, if they may
/// still post there.
pub async fn approve(state: &AppState, viewer: &Viewer, held_id: i64) -> Outcome<i64> {
    let user = staff_or_refuse!(viewer);
    let mut tx = state.db().begin().await?;
    let Some(held) = mod_records::claim_held(&mut tx, held_id, "held").await? else {
        return Ok(Err(Refused::NotFound));
    };
    let post_id = match publish_held(state, user, &mut tx, &held).await? {
        Ok(post_id) => post_id,
        Err(refusal) => return Ok(Err(refusal)),
    };
    tx.commit().await?;
    state.jobs().wake();
    tell(
        state,
        held.author.id,
        user,
        "A moderator approved your post, and it is up now.".to_owned(),
        None,
    )
    .await?;
    state.changed(Change::PostCreated { post_id }).await?;
    state.changed(Change::ModLogged).await?;
    Ok(Ok(post_id))
}

/// Posts a held post claimed with [`mod_records::claim_held`], as its
/// author, if they may still post there; and records it as approved.
async fn publish_held(
    state: &AppState,
    user: &UserCtx,
    tx: &mut crate::db::Tx,
    held: &HeldPost,
) -> Result<Result<i64, Refused>, AppError> {
    let db = state.db();
    // Checked again as the author, as they are now.
    let Some(author) = sessions::user_ctx(db, held.author.id).await? else {
        return Ok(Err(Refused::NotFound));
    };
    let author_viewer = Viewer::User(author.clone());
    if let Some(why) = writing::restriction(&author) {
        return Ok(Err(Refused::NotAllowedBecause(format!(
            "{} can’t post now: {why}",
            author.username
        ))));
    }
    let (post_id, thread_id, public) = match &held.place {
        HeldPlace::Thread {
            category_id,
            title,
            forked_from_post_id,
        } => {
            let Some(category) = content::category_by_id(db, &author_viewer, *category_id).await?
            else {
                return Ok(Err(Refused::NotAllowed(
                    "The author can no longer post in that category. Reject it instead.",
                )));
            };
            if !writing::may_post_in(&author_viewer, &category) {
                return Ok(Err(Refused::NotAllowed(
                    "The author can no longer post in that category. Reject it instead.",
                )));
            }
            if let Some(source) = forked_from_post_id {
                let ok = match content::post_target(db, &author_viewer, *source).await? {
                    Some(post) => {
                        post.is_live()
                            && at_most_as_visible(&post.thread.audience(), &category.audience())
                    }
                    None => false,
                };
                if !ok {
                    return Ok(Err(Refused::NotAllowed(
                        "The post it quotes is no longer available to quote there. Reject it instead.",
                    )));
                }
            }
            let created = store::create_thread(
                &mut *tx,
                &NewThread {
                    category_id: *category_id,
                    title: title.clone(),
                    author_id: author.id,
                    body_md: held.body_md.clone(),
                    forked_from_post_id: *forked_from_post_id,
                    pinned: false,
                    locked: false,
                    created_at: None,
                },
            )
            .await?;
            social::set_watch(
                &mut *tx,
                author.id,
                Watched::Thread(created.thread_id),
                WatchLevel::Watching,
            )
            .await?;
            (
                created.first_post_id,
                created.thread_id,
                is_public(&category.audience()),
            )
        }
        HeldPlace::Reply {
            thread_id,
            reply_to_post_id,
        } => {
            let Some(thread) = content::thread_target(db, &author_viewer, *thread_id).await? else {
                return Ok(Err(Refused::NotAllowed(
                    "The author can no longer see that thread. Reject it instead.",
                )));
            };
            if let Some(why) = writing::reply_refusal(state, &author_viewer, &thread).await? {
                return Ok(Err(Refused::NotAllowedBecause(format!(
                    "It can’t be posted there now: {why} Reject it instead."
                ))));
            }
            // The post replied to may have gone since.
            let reply_to = match reply_to_post_id {
                Some(id) => content::post_target(db, &author_viewer, *id)
                    .await?
                    .filter(|p| p.thread.thread.id == *thread_id)
                    .map(|p| p.post_id),
                None => None,
            };
            let post_id = store::create_post(
                &mut *tx,
                &NewPost {
                    thread_id: *thread_id,
                    author_id: author.id,
                    body_md: held.body_md.clone(),
                    reply_to_post_id: reply_to,
                    created_at: None,
                },
            )
            .await?;
            (post_id, *thread_id, is_public(&thread.audience()))
        }
    };
    notify::queue_post(&mut *tx, post_id).await?;
    mod_records::decide_held(&mut *tx, held.id, user.id, Some(post_id)).await?;
    mod_records::record(
        &mut *tx,
        &NewAction {
            actor_id: user.id,
            action: "approve_post",
            target_kind: "held_post",
            target_id: held.id,
            user_id: Some(author.id),
            thread_id: Some(thread_id),
            post_id: Some(post_id),
            reason: "",
            public,
            details: json!({}),
        },
    )
    .await?;
    Ok(Ok(post_id))
}

/// Rejects a held post: it is never posted.
pub async fn reject(state: &AppState, viewer: &Viewer, held_id: i64, reason: &str) -> Outcome<()> {
    let user = staff_or_refuse!(viewer);
    let reason = clean_line(reason);
    if let Some(refusal) = reason_problems(&reason) {
        return Ok(Err(refusal));
    }
    let mut tx = state.db().begin().await?;
    let Some(held) = mod_records::claim_held(&mut tx, held_id, "held").await? else {
        return Ok(Err(Refused::NotFound));
    };
    mod_records::decide_held(&mut tx, held.id, user.id, None).await?;
    let action_id = mod_records::record(
        &mut tx,
        &NewAction {
            actor_id: user.id,
            action: "reject_post",
            target_kind: "held_post",
            target_id: held.id,
            user_id: Some(held.author.id),
            thread_id: match held.place {
                HeldPlace::Reply { thread_id, .. } => Some(thread_id),
                HeldPlace::Thread { .. } => None,
            },
            post_id: None,
            reason: &reason,
            public: false,
            details: json!({}),
        },
    )
    .await?;
    tx.commit().await?;
    tell(
        state,
        held.author.id,
        user,
        format!("A moderator didn’t approve your post. Reason: {reason}"),
        Some(action_id),
    )
    .await?;
    state.changed(Change::ModLogged).await?;
    Ok(Ok(()))
}

/// A held post, for its author or staff to look at.
pub async fn held_for(state: &AppState, viewer: &Viewer, id: i64) -> Outcome<HeldPost> {
    let Some(user) = viewer.user() else {
        return Ok(Err(Refused::LogIn));
    };
    match mod_records::held_post(state.db(), id).await? {
        Some(held) if held.author.id == user.id || user.is_staff() => Ok(Ok(held)),
        _ => Ok(Err(Refused::NotFound)),
    }
}

// ---------------------------------------------------------------------------
// Appeals

/// What deciding an appeal did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reversal {
    /// The decision was kept.
    Kept,
    /// The decision was reversed, and what it did undone.
    Undone,
    /// The decision was reversed, but what it did has since been changed
    /// by a later, separate action, which was left alone.
    Superseded,
    /// The decision was reversed, but there was nothing to undo: it had run
    /// out or been undone already, or it can't be undone (an edit or a
    /// redaction).
    NothingToUndo,
}

/// Decides an appeal. Reversing it (`overturn`) undoes the action where it
/// can be undone and what it did still stands: showing a hidden post,
/// restoring a deleted post or thread, unlocking a thread, lifting a
/// silence or suspension, or posting a rejected post. A later, separate
/// action (a new suspension, a post hidden again for another reason) is
/// left alone. Reversing the appeal of the member an invite branch was
/// suspended for lifts the suspension of the whole branch; reversing the
/// appeal of anyone else in it lifts only theirs. Either way the member
/// is told.
pub async fn decide_appeal(
    state: &AppState,
    viewer: &Viewer,
    report_id: i64,
    overturn: bool,
    reason: &str,
) -> Outcome<Reversal> {
    let user = staff_or_refuse!(viewer);
    let Some(report) = mod_records::report(state.db(), report_id)
        .await?
        .filter(|r| r.kind == "appeal")
    else {
        return Ok(Err(Refused::NotFound));
    };
    let reason = clean_line(reason);
    if let Some(refusal) = reason_problems(&reason) {
        return Ok(Err(refusal));
    }
    let action_id = report.mod_action_id.expect("appeals have actions");
    let Some(action) = mod_records::action(state.db(), action_id).await? else {
        return Ok(Err(Refused::NotFound));
    };
    let member_id = report.user_id.expect("appeals have members");
    let db = state.db();
    let reversal = if overturn {
        match undo(state, user, &action, member_id).await? {
            Ok(reversal) => reversal,
            Err(refusal) => return Ok(Err(refusal)),
        }
    } else {
        Reversal::Kept
    };
    mod_records::settle(db, Settles::Report(report.id), !overturn, user.id, &reason).await?;
    mod_records::record(
        db,
        &NewAction {
            actor_id: user.id,
            action: if overturn {
                "overturn_appeal"
            } else {
                "uphold_appeal"
            },
            target_kind: "report",
            target_id: report.id,
            user_id: Some(member_id),
            thread_id: action.thread_id,
            post_id: action.post_id,
            reason: &reason,
            public: false,
            details: json!({
                "mod_action_id": action.id,
                "undone": reversal == Reversal::Undone,
            }),
        },
    )
    .await?;
    tell(
        state,
        member_id,
        user,
        if overturn {
            format!("The moderators looked again and reversed their decision. {reason}")
        } else {
            format!("The moderators looked again and are keeping their decision. {reason}")
        },
        None,
    )
    .await?;
    notify::counts_changed(state, member_id);
    state.changed(Change::ModLogged).await?;
    Ok(Ok(reversal))
}

/// Undoes an action contested by `member_id`, if what it did still
/// stands.
async fn undo(
    state: &AppState,
    user: &UserCtx,
    action: &ActionRecord,
    member_id: i64,
) -> Result<Result<Reversal, Refused>, AppError> {
    let db = state.db();
    // A member deleted since has nothing to get back.
    if member_by_id(state, member_id)
        .await?
        .is_none_or(|m| m.deleted)
    {
        return Ok(Ok(Reversal::NothingToUndo));
    }
    // Whether this is still the newest of the actions that set the same
    // state of a post or thread.
    let newest = |kind: &'static str, id: i64, family: &'static [&'static str]| async move {
        Ok::<bool, AppError>(
            mod_records::latest_action(db, kind, id, family).await? == Some(action.id),
        )
    };
    let thread_changed = |thread_id: i64| async move {
        state
            .changed(Change::ThreadChanged {
                thread_id,
                previous_category_id: None,
            })
            .await
    };
    let undone = |changed: bool| {
        if changed {
            Reversal::Undone
        } else {
            Reversal::NothingToUndo
        }
    };
    Ok(Ok(
        match (action.action.as_str(), action.post_id, action.thread_id) {
            ("hide_post", Some(post_id), _) => {
                if !newest("post", post_id, &["hide_post", "unhide_post"]).await? {
                    Reversal::Superseded
                } else if mod_records::set_hidden(db, post_id, None).await? {
                    state.changed(Change::PostRemoved { post_id }).await?;
                    Reversal::Undone
                } else {
                    // Deleted since.
                    Reversal::Superseded
                }
            }
            ("delete_post", Some(post_id), _) => {
                if !newest("post", post_id, &["delete_post", "restore_post"]).await? {
                    Reversal::Superseded
                } else {
                    let changed = mod_records::set_deleted(db, post_id, user.id, true).await?;
                    if changed {
                        state.changed(Change::PostRemoved { post_id }).await?;
                    }
                    undone(changed)
                }
            }
            ("delete_thread", _, Some(thread_id)) => {
                if !newest("thread", thread_id, &["delete_thread", "restore_thread"]).await? {
                    Reversal::Superseded
                } else {
                    let changed = mod_records::set_thread_deleted(db, thread_id, true).await?;
                    if changed {
                        thread_changed(thread_id).await?;
                    }
                    undone(changed)
                }
            }
            ("lock_thread", _, Some(thread_id)) => {
                if !newest("thread", thread_id, &["lock_thread", "unlock_thread"]).await? {
                    Reversal::Superseded
                } else {
                    let changed = mod_records::set_locked(db, thread_id, false).await?;
                    if changed {
                        thread_changed(thread_id).await?;
                    }
                    undone(changed)
                }
            }
            ("silence" | "suspend" | "suspend_branch", _, _) => {
                let restriction = if action.action == "silence" {
                    Restriction::Silence
                } else {
                    Restriction::Suspend
                };
                // The member the branch was suspended for contests it for
                // the whole branch; anyone else, for themselves.
                let ids = if action.user_id == Some(member_id) {
                    action.affected()
                } else {
                    vec![member_id]
                };
                let Some(until) = action.until() else {
                    return Ok(Ok(Reversal::NothingToUndo));
                };
                let lifted = mod_records::lift_if_until(db, &ids, restriction, until).await?;
                for id in &lifted {
                    notify::counts_changed(state, *id);
                    if *id != member_id {
                        tell(
                            state,
                            *id,
                            user,
                            "A moderator lifted your suspension: the suspension of the invite branch you were suspended with was reversed.".to_owned(),
                            None,
                        )
                        .await?;
                    }
                }
                if lifted.len() == ids.len() {
                    Reversal::Undone
                } else if mod_records::restricted_now(db, &ids, restriction).await? > 0 {
                    Reversal::Superseded
                } else {
                    undone(!lifted.is_empty())
                }
            }
            ("reject_post", _, _) => {
                let mut tx = db.begin().await?;
                let Some(held) =
                    mod_records::claim_held(&mut tx, action.target_id, "rejected").await?
                else {
                    return Ok(Ok(Reversal::NothingToUndo));
                };
                let post_id = match publish_held(state, user, &mut tx, &held).await? {
                    Ok(post_id) => post_id,
                    Err(refusal) => return Ok(Err(refusal)),
                };
                tx.commit().await?;
                state.jobs().wake();
                state.changed(Change::PostCreated { post_id }).await?;
                Reversal::Undone
            }
            _ => Reversal::NothingToUndo,
        },
    ))
}

// ---------------------------------------------------------------------------
// Forgetting addresses

/// The daily job that forgets addresses older than the retention period.
pub const FORGET_IPS_JOB: &str = "forget_ips";

pub async fn run_forget_ips(state: AppState, _payload: serde_json::Value) -> anyhow::Result<()> {
    let days = crate::settings::moderation_settings(state.db())
        .await?
        .ip_retention_days;
    let forgotten =
        mod_records::forget_old_ips(state.db(), i32::try_from(days).unwrap_or(i32::MAX)).await?;
    tracing::info!(forgotten, "forgot old addresses");
    crate::jobs::enqueue_at(
        state.db(),
        FORGET_IPS_JOB,
        &json!({}),
        Some(Utc::now() + Duration::days(1)),
    )
    .await?;
    Ok(())
}

/// Makes sure the daily job is planned: call when the server starts.
pub async fn schedule_forgetting(state: &AppState) -> sqlx::Result<()> {
    if !crate::jobs::has_pending(state.db(), FORGET_IPS_JOB).await? {
        crate::jobs::enqueue(state.db(), FORGET_IPS_JOB, &json!({})).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;

    #[test]
    fn numbers_are_listed_loosely() {
        assert_eq!(parse_numbers("12", 50), Ok(vec![12]));
        assert_eq!(
            parse_numbers("3, 1-2  5–4 №7", 50),
            Ok(vec![1, 2, 3, 4, 5, 7])
        );
        assert_eq!(parse_numbers("9-7", 50), Ok(vec![7, 8, 9]));
        for bad in ["", "0", "51", "a", "1-", "-3", "1--2", "1,,x"] {
            assert!(parse_numbers(bad, 50).is_err(), "{bad:?}");
        }
    }

    /// Whatever is typed, a list is either refused or holds distinct
    /// numbers in order, all within range; and listing them again gives
    /// the same list.
    #[hegel::test(test_cases = 500)]
    fn parsed_numbers_are_in_range_and_ordered(tc: TestCase) {
        let text = tc.draw(gs::text().max_size(40));
        let max = tc.draw(
            gs::integers::<i64>()
                .min_value(1)
                .max_value(i64::from(i32::MAX)),
        );
        if let Ok(numbers) = parse_numbers(&text, max) {
            assert!(!numbers.is_empty());
            assert!(numbers.windows(2).all(|w| w[0] < w[1]), "{numbers:?}");
            assert!(numbers.iter().all(|&n| n >= 1 && i64::from(n) <= max));
            let again: Vec<String> = numbers.iter().map(i32::to_string).collect();
            assert_eq!(parse_numbers(&again.join(", "), max), Ok(numbers));
        }
    }

    #[hegel::test(test_cases = 300)]
    fn ranges_cover_what_they_say(tc: TestCase) {
        let a = tc.draw(gs::integers::<i32>().min_value(1).max_value(500));
        let b = tc.draw(gs::integers::<i32>().min_value(1).max_value(500));
        let numbers = parse_numbers(&format!("{a}-{b}"), 500).unwrap();
        let expected: Vec<i32> = (a.min(b)..=a.max(b)).collect();
        assert_eq!(numbers, expected);
    }

    #[test]
    fn thread_references() {
        assert_eq!(parse_thread_ref("42"), Some(42));
        assert_eq!(parse_thread_ref(" /t/42/brass "), Some(42));
        assert_eq!(
            parse_thread_ref("https://forum.example.com/t/42/brass?page=2#p9"),
            Some(42)
        );
        assert_eq!(parse_thread_ref("/t/42"), Some(42));
        for bad in ["", "0", "-4", "/t/", "/t/4x", "/c/games", "brass"] {
            assert_eq!(parse_thread_ref(bad), None, "{bad:?}");
        }
    }

    /// Any thread id written as a number or a link is read back as itself.
    #[hegel::test(test_cases = 300)]
    fn thread_references_round_trip(tc: TestCase) {
        let id = tc.draw(gs::integers::<i64>().min_value(1));
        let slug = tc.draw(gs::text().max_size(20));
        let slug = store::slugify(&slug);
        assert_eq!(parse_thread_ref(&id.to_string()), Some(id));
        assert_eq!(parse_thread_ref(&format!("/t/{id}/{slug}")), Some(id));
        assert_eq!(
            parse_thread_ref(&format!("http://localhost:8080/t/{id}/{slug}?page=3")),
            Some(id)
        );
    }

    #[hegel::test(test_cases = 300)]
    fn parsing_anything_never_panics(tc: TestCase) {
        let text = tc.draw(gs::text());
        let _ = parse_thread_ref(&text);
        let _ = parse_numbers(&text, i64::MAX);
        let _ = parse_days(&text);
        let _ = plausible_email(&text);
    }

    #[test]
    fn reasons_are_required_and_short() {
        let mut problems = Vec::new();
        check_reason("  \n ", &mut problems);
        assert_eq!(problems.len(), 1);
        problems.clear();
        check_reason(&"x".repeat(MAX_REASON_CHARS + 1), &mut problems);
        assert_eq!(problems.len(), 1);
        problems.clear();
        check_reason("Spam", &mut problems);
        assert!(problems.is_empty());
        assert_eq!(clean_line(" a\n\tb  "), "a b");
    }

    #[test]
    fn days_are_bounded() {
        assert_eq!(parse_days("7"), Some(7));
        assert_eq!(parse_days("0"), None);
        assert_eq!(parse_days(&(MAX_DAYS + 1).to_string()), None);
        assert_eq!(parse_days("x"), None);
    }
}
