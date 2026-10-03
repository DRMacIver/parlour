//! Moderation records (DESIGN.md §8): reports and appeals, posts held for
//! approval, the moderation log, staff notes, silences and suspensions,
//! moving posts between threads, redacting revisions, and the addresses
//! members use.
//!
//! These functions don't check permissions or clear the page cache; the
//! decisions are made in [`crate::moderation`]. Reading the log, which
//! anyone may do, is in [`crate::content::moderation`] so that it goes
//! through the access layer.

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::accounts::Role;
use crate::content::UserRef;
use crate::db::{Db, Handle, KEY};

fn user_ref(id: Option<i64>, username: Option<String>, name: Option<String>) -> Option<UserRef> {
    Some(UserRef {
        id: id?,
        username: username?,
        display_name: name?,
    })
}

// ---------------------------------------------------------------------------
// Posts held for approval (DESIGN.md §7)

/// Where a held post goes once approved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeldPlace {
    /// A new thread in a space, perhaps quote-forking a post.
    Thread {
        space_id: i64,
        title: String,
        forked_from_post_id: Option<i64>,
    },
    /// A reply in a thread.
    Reply {
        thread_id: i64,
        reply_to_post_id: Option<i64>,
    },
}

#[derive(Clone, Debug)]
pub struct HeldPost {
    pub id: i64,
    pub author: UserRef,
    pub place: HeldPlace,
    pub body_md: String,
    /// `held`, `approved` or `rejected`.
    pub status: String,
    /// Once approved, the post it became.
    pub post_id: Option<i64>,
    pub created_at: DateTime<Utc>,
}

/// Holds a post for a moderator to look at.
pub async fn hold<'c>(
    db: impl Handle<'c>,
    author_id: i64,
    place: &HeldPlace,
    body_md: &str,
) -> sqlx::Result<i64> {
    let mut conn = db.open(KEY).acquire().await?;
    let (space_id, title, forked_from, thread_id, reply_to) = match place {
        HeldPlace::Thread {
            space_id,
            title,
            forked_from_post_id,
        } => (
            Some(*space_id),
            Some(title.as_str()),
            *forked_from_post_id,
            None,
            None,
        ),
        HeldPlace::Reply {
            thread_id,
            reply_to_post_id,
        } => (None, None, None, Some(*thread_id), *reply_to_post_id),
    };
    sqlx::query_scalar!(
        r#"INSERT INTO held_posts (author_id, space_id, title, forked_from_post_id, thread_id,
                                   reply_to_post_id, body_md)
           VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id"#,
        author_id,
        space_id,
        title,
        forked_from,
        thread_id,
        reply_to,
        body_md,
    )
    .fetch_one(&mut *conn)
    .await
}

struct HeldRow {
    id: i64,
    author_id: i64,
    username: String,
    display_name: String,
    space_id: Option<i64>,
    title: Option<String>,
    forked_from_post_id: Option<i64>,
    thread_id: Option<i64>,
    reply_to_post_id: Option<i64>,
    body_md: String,
    status: String,
    post_id: Option<i64>,
    created_at: DateTime<Utc>,
}

impl HeldRow {
    fn into_held(self) -> HeldPost {
        let place = match (self.space_id, self.title, self.thread_id) {
            (Some(space_id), Some(title), _) => HeldPlace::Thread {
                space_id,
                title,
                forked_from_post_id: self.forked_from_post_id,
            },
            (_, _, thread_id) => HeldPlace::Reply {
                thread_id: thread_id.expect("a held reply has a thread"),
                reply_to_post_id: self.reply_to_post_id,
            },
        };
        HeldPost {
            id: self.id,
            author: UserRef {
                id: self.author_id,
                username: self.username,
                display_name: self.display_name,
            },
            place,
            body_md: self.body_md,
            status: self.status,
            post_id: self.post_id,
            created_at: self.created_at,
        }
    }
}

pub async fn held_post(db: &Db, id: i64) -> sqlx::Result<Option<HeldPost>> {
    let row = sqlx::query_as!(
        HeldRow,
        r#"SELECT h.id, h.author_id, u.username AS "username!: String", u.display_name, h.space_id,
                  h.title, h.forked_from_post_id, h.thread_id, h.reply_to_post_id, h.body_md, h.status,
                  h.post_id, h.created_at
           FROM held_posts h JOIN users u ON u.id = h.author_id WHERE h.id = $1"#,
        id
    )
    .fetch_optional(db.pool())
    .await?;
    Ok(row.map(HeldRow::into_held))
}

/// Posts waiting for a moderator, oldest first. Those whose authors have
/// since been deleted are left out.
pub async fn waiting_held(db: &Db) -> sqlx::Result<Vec<HeldPost>> {
    let rows = sqlx::query_as!(
        HeldRow,
        r#"SELECT h.id, h.author_id, u.username AS "username!: String", u.display_name, h.space_id,
                  h.title, h.forked_from_post_id, h.thread_id, h.reply_to_post_id, h.body_md, h.status,
                  h.post_id, h.created_at
           FROM held_posts h JOIN users u ON u.id = h.author_id
           WHERE h.status = 'held' AND u.deleted_at IS NULL
           ORDER BY h.created_at, h.id"#
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows.into_iter().map(HeldRow::into_held).collect())
}

/// Takes a held post to decide on it, locking it until the transaction
/// ends, so that two moderators can't both approve it. `None` if its
/// status isn't `status` (`held`, waiting; or `rejected`, for reversing a
/// rejection).
pub async fn claim_held<'c>(
    db: impl Handle<'c>,
    id: i64,
    status: &str,
) -> sqlx::Result<Option<HeldPost>> {
    let mut conn = db.open(KEY).acquire().await?;
    let row = sqlx::query_as!(
        HeldRow,
        r#"SELECT h.id, h.author_id, u.username AS "username!: String", u.display_name, h.space_id,
                  h.title, h.forked_from_post_id, h.thread_id, h.reply_to_post_id, h.body_md, h.status,
                  h.post_id, h.created_at
           FROM held_posts h JOIN users u ON u.id = h.author_id
           WHERE h.id = $1 AND h.status = $2
           FOR UPDATE OF h"#,
        id,
        status
    )
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(HeldRow::into_held))
}

/// Records the decision on a held post claimed with [`claim_held`]:
/// approved as `post_id`, or (with `None`) rejected.
pub async fn decide_held<'c>(
    db: impl Handle<'c>,
    id: i64,
    decided_by: i64,
    post_id: Option<i64>,
) -> sqlx::Result<()> {
    let mut conn = db.open(KEY).acquire().await?;
    sqlx::query!(
        r#"UPDATE held_posts SET status = CASE WHEN $3::bigint IS NULL THEN 'rejected' ELSE 'approved' END,
                  post_id = $3, decided_by = $2, decided_at = now()
           WHERE id = $1"#,
        id,
        decided_by,
        post_id,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Reports, contact messages and appeals

/// What a report is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReportAbout {
    /// A post, by `author_id`, in thread `thread_id` (perhaps a
    /// conversation, which the report reveals to staff).
    Post {
        post_id: i64,
        thread_id: i64,
        author_id: i64,
    },
    /// A member.
    User { user_id: i64 },
    /// A message sent with the contact form.
    Contact,
    /// An appeal by `user_id` against moderation action `action_id`.
    Appeal { action_id: i64, user_id: i64 },
}

pub struct NewReport<'a> {
    pub reporter_id: Option<i64>,
    pub contact_email: Option<&'a str>,
    pub about: ReportAbout,
    /// One of [`REASONS`], or `contact` or `appeal`.
    pub reason: &'a str,
    pub details: &'a str,
}

/// The reasons a member can give when reporting, with how they read.
pub const REASONS: [(&str, &str); 5] = [
    ("spam", "Spam"),
    ("harassment", "Harassment or unwanted contact"),
    ("personal", "Personal information"),
    ("inappropriate", "Inappropriate or against the rules"),
    ("other", "Something else"),
];

/// How a report's reason reads.
pub fn reason_label(reason: &str) -> &'static str {
    match reason {
        "contact" => "Message to the moderators",
        "appeal" => "Appeal",
        _ => REASONS
            .iter()
            .find(|(key, _)| *key == reason)
            .map_or("Something else", |(_, label)| label),
    }
}

/// Files a report. `None` if the reporter already has an open report about
/// the same thing, or has already contested the same action.
pub async fn insert_report<'c>(
    db: impl Handle<'c>,
    report: &NewReport<'_>,
) -> sqlx::Result<Option<i64>> {
    let mut conn = db.open(KEY).acquire().await?;
    let (kind, post_id, user_id, thread_id, action_id) = match &report.about {
        ReportAbout::Post {
            post_id,
            thread_id,
            author_id,
        } => (
            "post",
            Some(*post_id),
            Some(*author_id),
            Some(*thread_id),
            None,
        ),
        ReportAbout::User { user_id } => ("user", None, Some(*user_id), None, None),
        ReportAbout::Contact => ("contact", None, None, None, None),
        ReportAbout::Appeal { action_id, user_id } => {
            ("appeal", None, Some(*user_id), None, Some(*action_id))
        }
    };
    let inserted = sqlx::query_scalar!(
        r#"INSERT INTO reports (kind, reporter_id, contact_email, post_id, user_id, thread_id, mod_action_id,
                                reason, details)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING id"#,
        kind,
        report.reporter_id,
        report.contact_email as _,
        post_id,
        user_id,
        thread_id,
        action_id,
        report.reason,
        report.details,
    )
    .fetch_one(&mut *conn)
    .await;
    match inserted {
        Ok(id) => Ok(Some(id)),
        Err(sqlx::Error::Database(err)) if err.is_unique_violation() => Ok(None),
        Err(err) => Err(err),
    }
}

/// An open report, as the queue shows it.
#[derive(Clone, Debug)]
pub struct OpenReport {
    pub id: i64,
    /// `post`, `user`, `contact` or `appeal`.
    pub kind: String,
    pub post_id: Option<i64>,
    pub user_id: Option<i64>,
    pub thread_id: Option<i64>,
    pub mod_action_id: Option<i64>,
    pub reporter: Option<UserRef>,
    pub contact_email: Option<String>,
    pub reason: String,
    pub details: String,
    pub created_at: DateTime<Utc>,
}

/// Every open report, oldest first.
pub async fn open_reports(db: &Db) -> sqlx::Result<Vec<OpenReport>> {
    let rows = sqlx::query!(
        r#"SELECT r.id, r.kind, r.post_id, r.user_id, r.thread_id, r.mod_action_id,
                  r.contact_email AS "contact_email: String", r.reason, r.details, r.created_at,
                  u.id AS "reporter_id?", u.username AS "reporter_username?: String",
                  u.display_name AS "reporter_name?"
           FROM reports r LEFT JOIN users u ON u.id = r.reporter_id
           WHERE r.status = 'open'
           ORDER BY r.created_at, r.id"#
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| OpenReport {
            id: row.id,
            kind: row.kind,
            post_id: row.post_id,
            user_id: row.user_id,
            thread_id: row.thread_id,
            mod_action_id: row.mod_action_id,
            reporter: user_ref(row.reporter_id, row.reporter_username, row.reporter_name),
            contact_email: row.contact_email,
            reason: row.reason,
            details: row.details,
            created_at: row.created_at,
        })
        .collect())
}

/// How many things are waiting for a moderator: open reports (counting
/// several about one post or member as one) and held posts.
pub async fn queue_size(db: &Db) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"SELECT moderation_queue_size() AS "n!""#)
        .fetch_one(db.pool())
        .await
}

/// Which open reports a decision settles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settles {
    /// Every open report about a post.
    Post(i64),
    /// Every open report about a member.
    User(i64),
    /// One report (a contact message or an appeal).
    Report(i64),
}

/// Closes the open reports a decision settles, as `resolved` (something was
/// done) or `dismissed`. Returns how many there were.
pub async fn settle<'c>(
    db: impl Handle<'c>,
    which: Settles,
    dismissed: bool,
    by: i64,
    resolution: &str,
) -> sqlx::Result<u64> {
    let mut conn = db.open(KEY).acquire().await?;
    let (post_id, user_id, report_id) = match which {
        Settles::Post(id) => (Some(id), None, None),
        Settles::User(id) => (None, Some(id), None),
        Settles::Report(id) => (None, None, Some(id)),
    };
    let resolution: String = resolution.chars().take(500).collect();
    let done = sqlx::query!(
        r#"UPDATE reports SET status = CASE WHEN $4 THEN 'dismissed' ELSE 'resolved' END,
                  resolved_by = $5, resolution = $6, resolved_at = now()
           WHERE status = 'open'
             AND ((kind = 'post' AND post_id = $1) OR (kind = 'user' AND user_id = $2) OR id = $3)"#,
        post_id,
        user_id,
        report_id,
        dismissed,
        by,
        resolution,
    )
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected())
}

pub async fn report(db: &Db, id: i64) -> sqlx::Result<Option<OpenReport>> {
    let row = sqlx::query!(
        r#"SELECT r.id AS "id!", r.kind AS "kind!", r.post_id, r.user_id, r.thread_id, r.mod_action_id,
                  r.contact_email AS "contact_email: String", r.reason AS "reason!", r.details AS "details!",
                  r.created_at AS "created_at!", r.status = 'open' AS "open!",
                  u.id AS "reporter_id?", u.username AS "reporter_username?: String",
                  u.display_name AS "reporter_name?"
           FROM reports r LEFT JOIN users u ON u.id = r.reporter_id
           WHERE r.id = $1"#,
        id
    )
    .fetch_optional(db.pool())
    .await?;
    Ok(row.filter(|row| row.open).map(|row| OpenReport {
        id: row.id,
        kind: row.kind,
        post_id: row.post_id,
        user_id: row.user_id,
        thread_id: row.thread_id,
        mod_action_id: row.mod_action_id,
        reporter: user_ref(row.reporter_id, row.reporter_username, row.reporter_name),
        contact_email: row.contact_email,
        reason: row.reason,
        details: row.details,
        created_at: row.created_at,
    }))
}

// ---------------------------------------------------------------------------
// The moderation log

pub struct NewAction<'a> {
    pub actor_id: i64,
    pub action: &'a str,
    /// `post`, `thread`, `user`, `revision`, `report` or `held_post`.
    pub target_kind: &'a str,
    pub target_id: i64,
    pub user_id: Option<i64>,
    pub thread_id: Option<i64>,
    pub post_id: Option<i64>,
    pub reason: &'a str,
    pub public: bool,
    pub details: Value,
}

/// Records something a moderator did.
pub async fn record<'c>(db: impl Handle<'c>, action: &NewAction<'_>) -> sqlx::Result<i64> {
    let mut conn = db.open(KEY).acquire().await?;
    let reason: String = action.reason.chars().take(500).collect();
    sqlx::query_scalar!(
        r#"INSERT INTO mod_actions (actor_id, action, target_kind, target_id, user_id, thread_id, post_id,
                                    reason, public, details)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING id"#,
        action.actor_id,
        action.action,
        action.target_kind,
        action.target_id,
        action.user_id,
        action.thread_id,
        action.post_id,
        reason,
        action.public,
        action.details,
    )
    .fetch_one(&mut *conn)
    .await
}

/// A recorded action, as its member sees it when contesting it.
#[derive(Clone, Debug)]
pub struct ActionRecord {
    pub id: i64,
    pub actor_id: i64,
    pub action: String,
    /// What it was done to, of the kind the action is about (see
    /// [`NewAction::target_kind`]).
    pub target_id: i64,
    pub user_id: Option<i64>,
    pub thread_id: Option<i64>,
    pub post_id: Option<i64>,
    pub reason: String,
    pub details: Value,
    pub created_at: DateTime<Utc>,
}

impl ActionRecord {
    /// The members it affected: its member, and for an invite branch's
    /// suspension everyone suspended with them.
    pub fn affected(&self) -> Vec<i64> {
        let mut ids: Vec<i64> = self.user_id.into_iter().collect();
        if let Some(others) = self.details["user_ids"].as_array() {
            ids.extend(others.iter().filter_map(Value::as_i64));
        }
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// When the silence or suspension it imposed was to end.
    pub fn until(&self) -> Option<DateTime<Utc>> {
        self.details["until"]
            .as_str()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Utc))
    }
}

pub async fn action(db: &Db, id: i64) -> sqlx::Result<Option<ActionRecord>> {
    let row = sqlx::query!(
        r#"SELECT a.id, a.actor_id, a.action, a.target_id, a.user_id, a.thread_id, a.post_id, a.reason,
                  a.details, a.created_at
           FROM mod_actions a WHERE a.id = $1"#,
        id
    )
    .fetch_optional(db.pool())
    .await?;
    Ok(row.map(|row| ActionRecord {
        id: row.id,
        actor_id: row.actor_id,
        action: row.action,
        target_id: row.target_id,
        user_id: row.user_id,
        thread_id: row.thread_id,
        post_id: row.post_id,
        reason: row.reason,
        details: row.details,
        created_at: row.created_at,
    }))
}

/// Whether a member has contested an action.
pub async fn appealed_by(db: &Db, action_id: i64, user_id: i64) -> sqlx::Result<bool> {
    sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM reports WHERE kind = 'appeal' AND mod_action_id = $1
                                                 AND user_id = $2) AS "appealed!""#,
        action_id,
        user_id
    )
    .fetch_one(db.pool())
    .await
}

/// The newest of these actions on a post or thread (`target_kind`), if
/// any.
pub async fn latest_action(
    db: &Db,
    target_kind: &str,
    target_id: i64,
    actions: &[&str],
) -> sqlx::Result<Option<i64>> {
    let actions: Vec<String> = actions.iter().map(|a| (*a).to_owned()).collect();
    sqlx::query_scalar!(
        "SELECT id FROM mod_actions WHERE target_kind = $1 AND target_id = $2 AND action = ANY($3)
         ORDER BY created_at DESC, id DESC LIMIT 1",
        target_kind,
        target_id,
        &actions
    )
    .fetch_optional(db.pool())
    .await
}

// ---------------------------------------------------------------------------
// Members

/// A member, as the user admin view shows them.
#[derive(Clone, Debug)]
pub struct MemberRecord {
    pub user: UserRef,
    pub email: String,
    pub email_verified: bool,
    pub role: Role,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub deleted: bool,
    pub silenced_until: Option<DateTime<Utc>>,
    pub suspended_until: Option<DateTime<Utc>>,
    pub invited_by: Option<UserRef>,
    /// Posts they have written that are still there, anywhere.
    pub posts: i64,
    /// How many people are below them in the invite tree.
    pub descendants: i64,
    /// How many of those a branch suspension would also suspend: the
    /// members below them who aren't staff and haven't left (see
    /// [`invite_branch`]).
    pub branch_below: i64,
}

impl MemberRecord {
    pub fn is_suspended(&self) -> bool {
        self.suspended_until.is_some_and(|t| t > Utc::now())
    }

    pub fn is_silenced(&self) -> bool {
        self.silenced_until.is_some_and(|t| t > Utc::now())
    }
}

/// A member by username, including deleted accounts.
pub async fn member_record(db: &Db, username: &str) -> sqlx::Result<Option<MemberRecord>> {
    if username.is_empty() || username.len() > 64 {
        return Ok(None);
    }
    let row = sqlx::query!(
        r#"WITH RECURSIVE below AS (
               SELECT id FROM users WHERE invited_by_user_id = (SELECT id FROM users WHERE username = $1::citext)
               UNION ALL
               SELECT u.id FROM users u JOIN below b ON u.invited_by_user_id = b.id
           )
           SELECT u.id AS "id!", u.username AS "username!: String", u.display_name AS "display_name!",
                  u.email AS "email!: String",
                  u.email_verified_at IS NOT NULL AS "verified!", u.role AS "role!", u.created_at AS "created_at!",
                  u.last_seen_at AS "last_seen_at?",
                  u.deleted_at IS NOT NULL AS "deleted!", u.silenced_until AS "silenced_until?",
                  u.suspended_until AS "suspended_until?",
                  i.id AS "inviter_id?", i.username AS "inviter_username?: String",
                  i.display_name AS "inviter_name?",
                  (SELECT count(*) FROM posts p WHERE p.author_id = u.id AND p.deleted_at IS NULL) AS "posts!",
                  (SELECT count(*) FROM below) AS "descendants!",
                  (SELECT count(*) FROM below b JOIN users x ON x.id = b.id
                   WHERE x.role = 'user' AND x.deleted_at IS NULL) AS "branch_below!"
           FROM users u LEFT JOIN users i ON i.id = u.invited_by_user_id
           WHERE u.username = $1::citext"#,
        username as _
    )
    .fetch_optional(db.pool())
    .await?;
    Ok(row.map(|row| MemberRecord {
        user: UserRef {
            id: row.id,
            username: row.username,
            display_name: row.display_name,
        },
        email: row.email,
        email_verified: row.verified,
        role: Role::parse(&row.role).unwrap_or(Role::User),
        created_at: row.created_at,
        last_seen_at: row.last_seen_at,
        deleted: row.deleted,
        silenced_until: row.silenced_until,
        suspended_until: row.suspended_until,
        invited_by: user_ref(row.inviter_id, row.inviter_username, row.inviter_name),
        posts: row.posts,
        descendants: row.descendants,
        branch_below: row.branch_below,
    }))
}

/// One row of the moderators' member list.
#[derive(Clone, Debug)]
pub struct MemberRow {
    pub user: UserRef,
    pub role: Role,
    pub created_at: DateTime<Utc>,
    pub silenced_until: Option<DateTime<Utc>>,
    pub suspended_until: Option<DateTime<Utc>>,
    pub open_reports: i64,
}

impl MemberRow {
    pub fn is_suspended(&self) -> bool {
        self.suspended_until.is_some_and(|t| t > Utc::now())
    }

    pub fn is_silenced(&self) -> bool {
        self.silenced_until.is_some_and(|t| t > Utc::now())
    }
}

/// Members whose usernames or display names contain `search` (all of them
/// if it is empty), or with `restricted` only those silenced or suspended
/// now, newest first.
pub async fn members(
    db: &Db,
    search: &str,
    restricted: bool,
    limit: i64,
    offset: i64,
) -> sqlx::Result<(i64, Vec<MemberRow>)> {
    let pattern = format!(
        "%{}%",
        search
            .trim()
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    );
    let total = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM users u
           WHERE u.deleted_at IS NULL
             AND (u.username::text ILIKE $1 OR u.display_name ILIKE $1)
             AND (NOT $2 OR u.silenced_until > now() OR u.suspended_until > now())"#,
        pattern,
        restricted,
    )
    .fetch_one(db.pool())
    .await?;
    let rows = sqlx::query!(
        r#"SELECT u.id, u.username AS "username!: String", u.display_name, u.role, u.created_at,
                  u.silenced_until, u.suspended_until,
                  (SELECT count(*) FROM reports r WHERE r.status = 'open' AND r.user_id = u.id
                     AND r.kind IN ('post', 'user')) AS "open_reports!"
           FROM users u
           WHERE u.deleted_at IS NULL
             AND (u.username::text ILIKE $1 OR u.display_name ILIKE $1)
             AND (NOT $2 OR u.silenced_until > now() OR u.suspended_until > now())
           ORDER BY u.created_at DESC, u.id DESC
           LIMIT $3 OFFSET $4"#,
        pattern,
        restricted,
        limit,
        offset,
    )
    .fetch_all(db.pool())
    .await?;
    Ok((
        total,
        rows.into_iter()
            .map(|row| MemberRow {
                user: UserRef {
                    id: row.id,
                    username: row.username,
                    display_name: row.display_name,
                },
                role: Role::parse(&row.role).unwrap_or(Role::User),
                created_at: row.created_at,
                silenced_until: row.silenced_until,
                suspended_until: row.suspended_until,
                open_reports: row.open_reports,
            })
            .collect(),
    ))
}

/// Which restriction a moderator sets or lifts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restriction {
    Silence,
    Suspend,
}

/// Silences or suspends members until `until` (or, with `None`, lifts it).
/// Only members who aren't staff and haven't been deleted are changed;
/// returns those who were.
pub async fn restrict<'c>(
    db: impl Handle<'c>,
    user_ids: &[i64],
    restriction: Restriction,
    until: Option<DateTime<Utc>>,
) -> sqlx::Result<Vec<i64>> {
    let mut conn = db.open(KEY).acquire().await?;
    let suspend = restriction == Restriction::Suspend;
    sqlx::query_scalar!(
        r#"UPDATE users SET silenced_until = CASE WHEN $2 THEN silenced_until ELSE $3 END,
                            suspended_until = CASE WHEN $2 THEN $3 ELSE suspended_until END
           WHERE id = ANY($1) AND role = 'user' AND deleted_at IS NULL
           RETURNING id"#,
        user_ids,
        suspend,
        until,
    )
    .fetch_all(&mut *conn)
    .await
}

/// Lifts the silence or suspension of these members that ends at `until`
/// and is still in force, leaving alone any that a later action changed.
/// Returns those it lifted.
pub async fn lift_if_until<'c>(
    db: impl Handle<'c>,
    user_ids: &[i64],
    restriction: Restriction,
    until: DateTime<Utc>,
) -> sqlx::Result<Vec<i64>> {
    let mut conn = db.open(KEY).acquire().await?;
    let suspend = restriction == Restriction::Suspend;
    sqlx::query_scalar!(
        r#"UPDATE users SET silenced_until = CASE WHEN $2 THEN silenced_until ELSE NULL END,
                            suspended_until = CASE WHEN $2 THEN NULL ELSE suspended_until END
           WHERE id = ANY($1)
             AND CASE WHEN $2 THEN suspended_until ELSE silenced_until END > now()
             AND abs(extract(epoch FROM CASE WHEN $2 THEN suspended_until ELSE silenced_until END - $3))
                 < 0.001
           RETURNING id"#,
        user_ids,
        suspend,
        until,
    )
    .fetch_all(&mut *conn)
    .await
}

/// How many of these members are silenced (or suspended) now.
pub async fn restricted_now(
    db: &Db,
    user_ids: &[i64],
    restriction: Restriction,
) -> sqlx::Result<i64> {
    let suspend = restriction == Restriction::Suspend;
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM users
           WHERE id = ANY($1)
             AND CASE WHEN $2 THEN suspended_until ELSE silenced_until END > now()"#,
        user_ids,
        suspend,
    )
    .fetch_one(db.pool())
    .await
}

/// A member and everyone below them in the invite tree, who aren't staff
/// and haven't been deleted, nearest first.
pub async fn invite_branch(db: &Db, user_id: i64) -> sqlx::Result<Vec<UserRef>> {
    let rows = sqlx::query!(
        r#"WITH RECURSIVE branch(id, depth) AS (
               SELECT id, 0 FROM users WHERE id = $1
               UNION ALL
               SELECT u.id, b.depth + 1 FROM users u JOIN branch b ON u.invited_by_user_id = b.id
           )
           SELECT u.id AS "id!", u.username AS "username!: String", u.display_name AS "display_name!"
           FROM branch b JOIN users u ON u.id = b.id
           WHERE u.role = 'user' AND u.deleted_at IS NULL
           ORDER BY b.depth, u.created_at, u.id"#,
        user_id
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| UserRef {
            id: row.id,
            username: row.username,
            display_name: row.display_name,
        })
        .collect())
}

/// Revokes the invites these members made that could still be used.
pub async fn revoke_invites_of<'c>(db: impl Handle<'c>, user_ids: &[i64]) -> sqlx::Result<u64> {
    let mut conn = db.open(KEY).acquire().await?;
    let done = sqlx::query!(
        "UPDATE invites SET revoked_at = now() WHERE created_by = ANY($1) AND revoked_at IS NULL
           AND invite_status(revoked_at, uses, max_uses, expires_at) = 'valid'",
        user_ids
    )
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected())
}

/// What deleting a spammer would take with them besides their own posts:
/// the threads they started, and the replies other members wrote in them.
pub async fn spammer_reach(db: &Db, user_id: i64) -> sqlx::Result<(i64, i64)> {
    let row = sqlx::query!(
        r#"SELECT (SELECT count(*) FROM threads
                   WHERE author_id = $1 AND deleted_at IS NULL AND space_id IS NOT NULL) AS "threads!",
                  (SELECT count(*) FROM posts p JOIN threads t ON t.id = p.thread_id
                   WHERE t.author_id = $1 AND t.deleted_at IS NULL AND t.space_id IS NOT NULL
                     AND p.author_id <> $1 AND p.deleted_at IS NULL) AS "replies!""#,
        user_id
    )
    .fetch_one(db.pool())
    .await?;
    Ok((row.threads, row.replies))
}

/// What deleting a spammer removed.
#[derive(Clone, Debug, Default)]
pub struct Removed {
    /// Their posts that were still there.
    pub post_ids: Vec<i64>,
    /// The threads they started.
    pub thread_ids: Vec<i64>,
}

/// Deletes a spammer's account and everything they wrote: every post, the
/// threads they started, their sessions and unused invites, and the posts
/// held for them. The account's row stays, marked deleted, so that its
/// username and email address can't be used again. Staff can't be deleted
/// this way; `None` if the member is staff or already deleted.
pub async fn delete_spammer<'c>(
    db: impl Handle<'c>,
    user_id: i64,
    by: i64,
) -> sqlx::Result<Option<Removed>> {
    let mut tx = db.open(KEY).begin().await?;
    let deleted = sqlx::query_scalar!(
        "UPDATE users SET deleted_at = now() WHERE id = $1 AND role = 'user' AND deleted_at IS NULL
         RETURNING id",
        user_id
    )
    .fetch_optional(&mut *tx)
    .await?;
    if deleted.is_none() {
        return Ok(None);
    }
    let post_ids = sqlx::query_scalar!(
        "UPDATE posts SET deleted_at = now(), deleted_by = $2
         WHERE author_id = $1 AND deleted_at IS NULL RETURNING id",
        user_id,
        by
    )
    .fetch_all(&mut *tx)
    .await?;
    let thread_ids = sqlx::query_scalar!(
        "UPDATE threads SET deleted_at = now(), version = version + 1
         WHERE author_id = $1 AND deleted_at IS NULL AND space_id IS NOT NULL RETURNING id",
        user_id
    )
    .fetch_all(&mut *tx)
    .await?;
    sqlx::query!("DELETE FROM sessions WHERE user_id = $1", user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query!(
        "UPDATE invites SET revoked_at = now() WHERE created_by = $1 AND revoked_at IS NULL",
        user_id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE held_posts SET status = 'rejected', decided_by = $2, decided_at = now()
         WHERE author_id = $1 AND status = 'held'",
        user_id,
        by
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(Removed {
        post_ids,
        thread_ids,
    }))
}

pub struct Note {
    pub id: i64,
    pub author: UserRef,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

pub async fn notes(db: &Db, user_id: i64) -> sqlx::Result<Vec<Note>> {
    let rows = sqlx::query!(
        r#"SELECT n.id, n.body, n.created_at, u.id AS author_id, u.username AS "username!: String",
                  u.display_name
           FROM user_notes n JOIN users u ON u.id = n.author_id
           WHERE n.user_id = $1 ORDER BY n.created_at DESC, n.id DESC"#,
        user_id
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| Note {
            id: row.id,
            author: UserRef {
                id: row.author_id,
                username: row.username,
                display_name: row.display_name,
            },
            body: row.body,
            created_at: row.created_at,
        })
        .collect())
}

pub async fn add_note<'c>(
    db: impl Handle<'c>,
    user_id: i64,
    author_id: i64,
    body: &str,
) -> sqlx::Result<i64> {
    let mut conn = db.open(KEY).acquire().await?;
    sqlx::query_scalar!(
        "INSERT INTO user_notes (user_id, author_id, body) VALUES ($1, $2, $3) RETURNING id",
        user_id,
        author_id,
        body
    )
    .fetch_one(&mut *conn)
    .await
}

// ---------------------------------------------------------------------------
// Addresses

/// Notes that a member is using an address now.
pub async fn record_ip<'c>(db: impl Handle<'c>, user_id: i64, ip: &str) -> sqlx::Result<()> {
    let mut conn = db.open(KEY).acquire().await?;
    let ip: String = ip.chars().take(64).collect();
    sqlx::query!(
        "INSERT INTO user_ips (user_id, ip) VALUES ($1, $2)
         ON CONFLICT (user_id, ip) DO UPDATE SET last_seen_at = now()",
        user_id,
        ip
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// An address a member has used, and who else has used it.
pub struct IpRecord {
    pub ip: String,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub others: Vec<UserRef>,
}

/// The addresses a member has used within the retention period, most
/// recent first, each with the other accounts seen at it.
pub async fn ips(db: &Db, user_id: i64, retention_days: i32) -> sqlx::Result<Vec<IpRecord>> {
    let rows = sqlx::query!(
        r#"SELECT mine.ip, mine.first_seen_at, mine.last_seen_at,
                  array_remove(array_agg(o.id ORDER BY o.id), NULL) AS "other_ids!",
                  array_remove(array_agg(o.username::text ORDER BY o.id), NULL) AS "other_usernames!",
                  array_remove(array_agg(o.display_name ORDER BY o.id), NULL) AS "other_names!"
           FROM user_ips mine
           LEFT JOIN user_ips theirs ON theirs.ip = mine.ip AND theirs.user_id <> mine.user_id
             AND theirs.last_seen_at > now() - make_interval(days => $2)
           LEFT JOIN users o ON o.id = theirs.user_id
           WHERE mine.user_id = $1 AND mine.last_seen_at > now() - make_interval(days => $2)
           GROUP BY mine.ip, mine.first_seen_at, mine.last_seen_at
           ORDER BY mine.last_seen_at DESC"#,
        user_id,
        retention_days,
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| IpRecord {
            ip: row.ip,
            first_seen_at: row.first_seen_at,
            last_seen_at: row.last_seen_at,
            others: row
                .other_ids
                .into_iter()
                .zip(row.other_usernames)
                .zip(row.other_names)
                .map(|((id, username), display_name)| UserRef {
                    id,
                    username,
                    display_name,
                })
                .collect(),
        })
        .collect())
}

/// Forgets addresses older than the retention period: those not used
/// since, and those recorded with sessions that began before it.
pub async fn forget_old_ips(db: &Db, retention_days: i32) -> sqlx::Result<u64> {
    let done = sqlx::query!(
        "DELETE FROM user_ips WHERE last_seen_at < now() - make_interval(days => $1)",
        retention_days
    )
    .execute(db.pool())
    .await?;
    sqlx::query!(
        "UPDATE sessions SET ip = NULL WHERE ip IS NOT NULL AND created_at < now() - make_interval(days => $1)",
        retention_days
    )
    .execute(db.pool())
    .await?;
    Ok(done.rows_affected())
}

// ---------------------------------------------------------------------------
// Posts and threads

/// Hides a post (with `Some(by)`) or shows it again. Returns whether it
/// changed.
pub async fn set_hidden<'c>(
    db: impl Handle<'c>,
    post_id: i64,
    by: Option<i64>,
) -> sqlx::Result<bool> {
    let mut conn = db.open(KEY).acquire().await?;
    let done = sqlx::query!(
        "UPDATE posts SET hidden_at = CASE WHEN $2::bigint IS NULL THEN NULL ELSE now() END, hidden_by = $2
         WHERE id = $1 AND deleted_at IS NULL AND (hidden_at IS NULL) = ($2::bigint IS NOT NULL)",
        post_id,
        by
    )
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Deletes a post as a moderator, or (with `restore`) undoes a moderator's
/// deletion. Returns whether it changed.
pub async fn set_deleted<'c>(
    db: impl Handle<'c>,
    post_id: i64,
    by: i64,
    restore: bool,
) -> sqlx::Result<bool> {
    let mut conn = db.open(KEY).acquire().await?;
    let done = if restore {
        sqlx::query!(
            "UPDATE posts SET deleted_at = NULL, deleted_by = NULL
             WHERE id = $1 AND deleted_at IS NOT NULL AND deleted_by IS NOT NULL AND deleted_by <> author_id",
            post_id
        )
        .execute(&mut *conn)
        .await?
    } else {
        sqlx::query!(
            "UPDATE posts SET deleted_at = now(), deleted_by = $2 WHERE id = $1 AND deleted_at IS NULL",
            post_id,
            by
        )
        .execute(&mut *conn)
        .await?
    };
    Ok(done.rows_affected() == 1)
}

/// Locks or unlocks a thread. Returns whether it changed.
pub async fn set_locked<'c>(
    db: impl Handle<'c>,
    thread_id: i64,
    locked: bool,
) -> sqlx::Result<bool> {
    let mut conn = db.open(KEY).acquire().await?;
    let done = sqlx::query!(
        "UPDATE threads SET locked = $2, version = version + 1 WHERE id = $1 AND locked <> $2",
        thread_id,
        locked
    )
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Pins or unpins a thread. Returns whether it changed.
pub async fn set_pinned<'c>(
    db: impl Handle<'c>,
    thread_id: i64,
    pinned: bool,
) -> sqlx::Result<bool> {
    let mut conn = db.open(KEY).acquire().await?;
    let done = sqlx::query!(
        "UPDATE threads SET pinned = $2, version = version + 1 WHERE id = $1 AND pinned <> $2",
        thread_id,
        pinned
    )
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Renames a thread, giving it the slug of its new title. Returns the old
/// title, or `None` if it was already called that.
pub async fn rename_thread<'c>(
    db: impl Handle<'c>,
    thread_id: i64,
    title: &str,
) -> sqlx::Result<Option<String>> {
    let mut conn = db.open(KEY).acquire().await?;
    sqlx::query_scalar!(
        r#"UPDATE threads t SET title = $2, slug = $3, version = t.version + 1
           FROM threads old WHERE old.id = t.id AND t.id = $1 AND old.title <> $2
           RETURNING old.title"#,
        thread_id,
        title,
        crate::store::slugify(title),
    )
    .fetch_optional(&mut *conn)
    .await
}

/// Deletes a space thread, or (with `restore`) brings it back. Returns
/// whether it changed.
pub async fn set_thread_deleted<'c>(
    db: impl Handle<'c>,
    thread_id: i64,
    restore: bool,
) -> sqlx::Result<bool> {
    let mut conn = db.open(KEY).acquire().await?;
    let done = sqlx::query!(
        "UPDATE threads SET deleted_at = CASE WHEN $2 THEN NULL ELSE now() END,
                            merged_into_thread_id = NULL, version = version + 1
         WHERE id = $1 AND space_id IS NOT NULL AND (deleted_at IS NULL) <> $2",
        thread_id,
        restore
    )
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Makes an empty thread in a space, for posts moved out of another
/// one to start. Its author is the author of the first post moved into it.
pub async fn empty_thread<'c>(
    db: impl Handle<'c>,
    space_id: i64,
    title: &str,
    author_id: i64,
    created_at: DateTime<Utc>,
) -> sqlx::Result<i64> {
    let mut conn = db.open(KEY).acquire().await?;
    sqlx::query_scalar!(
        r#"INSERT INTO threads (space_id, visibility, title, slug, author_id, created_at, last_post_at)
           VALUES ($1, 'public', $2, $3, $4, $5, $5) RETURNING id"#,
        space_id,
        title,
        crate::store::slugify(title),
        author_id,
        created_at,
    )
    .fetch_one(&mut *conn)
    .await
}

/// The posts at these numbers in a thread, in order: their ids, authors
/// and when they were written. Numbers with no post (moved away) are left
/// out.
pub async fn posts_at<'c>(
    db: impl Handle<'c>,
    thread_id: i64,
    seqs: &[i32],
) -> sqlx::Result<Vec<(i64, i64, DateTime<Utc>)>> {
    let mut conn = db.open(KEY).acquire().await?;
    let rows = sqlx::query!(
        "SELECT id, author_id, created_at FROM posts WHERE thread_id = $1 AND seq = ANY($2) ORDER BY seq",
        thread_id,
        seqs
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.id, row.author_id, row.created_at))
        .collect())
}

/// What moving posts did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Moved {
    /// The posts moved, in their old order, with their new numbers.
    pub posts: Vec<(i64, i64)>,
    /// Whether the thread they left is now empty, and so was merged into
    /// the destination.
    pub emptied: bool,
}

/// Moves posts (given by id) from one thread to the end of another,
/// keeping their order (DESIGN.md §3.3, §8):
///
/// - each takes the next number in the destination, so no page there
///   changes which numbers it holds;
/// - each leaves a [`post_moves`](crate) entry at its old number, so no
///   page of the thread it left changes either, and links to its old place
///   lead on to it;
/// - it keeps its id, so its permalink `/p/{id}` (and anything embedding
///   it, its history and reactions) follow it;
/// - "in reply to" links between posts that end up in different threads
///   are dropped, since a reply is always to a post in the same thread;
/// - notifications about it follow it.
///
/// If no posts are left in the source thread, it is deleted and marked as
/// merged into the destination, whose address it then leads to.
pub async fn move_posts<'c>(
    db: impl Handle<'c>,
    from_thread: i64,
    post_ids: &[i64],
    to_thread: i64,
    by: i64,
) -> sqlx::Result<Moved> {
    let mut tx = db.open(KEY).begin().await?;
    // Lock both threads, in a fixed order so that two moves can't deadlock.
    sqlx::query!(
        "SELECT id FROM threads WHERE id = ANY($1) ORDER BY id FOR UPDATE",
        &[from_thread, to_thread][..]
    )
    .fetch_all(&mut *tx)
    .await?;
    let moving = sqlx::query!(
        "SELECT id, seq, reply_to_post_id, created_at FROM posts
         WHERE thread_id = $1 AND id = ANY($2) ORDER BY seq FOR UPDATE",
        from_thread,
        post_ids
    )
    .fetch_all(&mut *tx)
    .await?;
    let ids: Vec<i64> = moving.iter().map(|p| p.id).collect();
    // Replies that would point across threads lose their link: those
    // staying behind that reply to posts leaving, and those leaving that
    // reply to posts staying.
    sqlx::query!(
        "UPDATE posts SET reply_to_post_id = NULL
         WHERE thread_id = $1 AND (
             (id <> ALL($2) AND reply_to_post_id = ANY($2))
             OR (id = ANY($2) AND reply_to_post_id IS NOT NULL AND reply_to_post_id <> ALL($2)))",
        from_thread,
        &ids
    )
    .execute(&mut *tx)
    .await?;
    // Links among the posts moving are put back once all have moved.
    sqlx::query!(
        "UPDATE posts SET reply_to_post_id = NULL WHERE id = ANY($1)",
        &ids
    )
    .execute(&mut *tx)
    .await?;
    let mut moved = Vec::new();
    for post in &moving {
        let seq = sqlx::query_scalar!(
            "UPDATE threads SET last_seq = last_seq + 1, last_post_at = greatest(last_post_at, $2),
                                version = version + 1
             WHERE id = $1 RETURNING last_seq",
            to_thread,
            post.created_at,
        )
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query!(
            "INSERT INTO post_moves (post_id, from_thread_id, from_seq, to_thread_id, moved_by)
             VALUES ($1, $2, $3, $4, $5)",
            post.id,
            from_thread,
            post.seq,
            to_thread,
            by
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "UPDATE posts SET thread_id = $2, seq = $3 WHERE id = $1",
            post.id,
            to_thread,
            seq
        )
        .execute(&mut *tx)
        .await?;
        moved.push((post.id, i64::from(seq)));
    }
    for post in &moving {
        if let Some(reply_to) = post.reply_to_post_id.filter(|r| ids.contains(r)) {
            sqlx::query!(
                "UPDATE posts SET reply_to_post_id = $2 WHERE id = $1",
                post.id,
                reply_to
            )
            .execute(&mut *tx)
            .await?;
        }
    }
    sqlx::query!(
        "UPDATE notifications SET thread_id = $2 WHERE post_id = ANY($1)",
        &ids,
        to_thread
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE threads SET version = version + 1 WHERE id = $1",
        from_thread
    )
    .execute(&mut *tx)
    .await?;
    let emptied = sqlx::query_scalar!(
        r#"UPDATE threads SET deleted_at = now(), merged_into_thread_id = $2
           WHERE id = $1 AND NOT EXISTS (SELECT 1 FROM posts WHERE thread_id = $1)
           RETURNING id"#,
        from_thread,
        to_thread
    )
    .fetch_optional(&mut *tx)
    .await?
    .is_some();
    tx.commit().await?;
    Ok(Moved {
        posts: moved,
        emptied,
    })
}

/// Removes the text of an earlier version of a post for everyone, keeping
/// its entry in the history. Returns whether there was such a version
/// that wasn't redacted already.
pub async fn redact_revision<'c>(
    db: impl Handle<'c>,
    post_id: i64,
    rev: i32,
    by: i64,
) -> sqlx::Result<bool> {
    let mut conn = db.open(KEY).acquire().await?;
    let done = sqlx::query!(
        "UPDATE post_revisions SET body_md = '', body_html = '', redacted_at = now(), redacted_by = $3
         WHERE post_id = $1 AND rev = $2 AND redacted_at IS NULL",
        post_id,
        rev,
        by
    )
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// The earlier versions of a post that haven't been redacted.
pub async fn unredacted_revisions(db: &Db, post_id: i64) -> sqlx::Result<Vec<i32>> {
    sqlx::query_scalar!(
        "SELECT rev FROM post_revisions WHERE post_id = $1 AND redacted_at IS NULL ORDER BY rev",
        post_id
    )
    .fetch_all(db.pool())
    .await
}
