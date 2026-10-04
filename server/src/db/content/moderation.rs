//! The moderation log (DESIGN.md §8), as a viewer may see it.
//!
//! Everyone sees the public entries, which are about public content and
//! about members; staff see every entry. Each entry names the thread it is
//! about only if the viewer can read that thread now (by the same shared
//! condition as everything else), so a thread that has since moved
//! somewhere more private, and anything said about it such as its old
//! title, stays hidden from those who can't read it.

use serde_json::Value;

use super::*;

pub const ENTRIES_PER_PAGE: i64 = 50;

/// A piece of an entry's sentence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Bit {
    Text(String),
    Link { href: String, text: String },
}

#[derive(Clone, Debug)]
pub struct LogEntry {
    pub id: i64,
    pub actor: UserRef,
    pub action: String,
    /// The member it was about.
    pub user: Option<UserRef>,
    /// What was done, after the moderator's name.
    pub bits: Vec<Bit>,
    pub reason: String,
    pub public: bool,
    pub created_at: DateTime<Utc>,
}

impl LogEntry {
    /// The sentence as plain text, for notices and tests.
    pub fn text(&self) -> String {
        let mut text = String::new();
        for bit in &self.bits {
            match bit {
                Bit::Text(t) => text.push_str(t),
                Bit::Link { text: t, .. } => text.push_str(t),
            }
        }
        text
    }
}

pub struct LogPage {
    pub total: i64,
    pub entries: Vec<LogEntry>,
}

/// Which entries to list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Only public entries, whoever is looking (`/moderation`).
    Public,
    /// Every entry the viewer may see: all of them for staff, the public
    /// ones for anyone else.
    All,
    /// Every entry about one member, for staff.
    Member(i64),
}

struct Row {
    id: i64,
    action: String,
    reason: String,
    public: bool,
    details: Value,
    created_at: DateTime<Utc>,
    post_id: Option<i64>,
    actor_id: i64,
    actor_username: String,
    actor_name: String,
    user_id: Option<i64>,
    username: Option<String>,
    user_name: Option<String>,
    thread_id: Option<i64>,
    thread_title: Option<String>,
    thread_slug: Option<String>,
    thread_private: Option<bool>,
    thread_live: Option<bool>,
    thread_readable: Option<bool>,
    to_thread_id: Option<i64>,
    to_title: Option<String>,
    to_slug: Option<String>,
    to_live: Option<bool>,
    to_readable: Option<bool>,
    from_space: Option<String>,
    from_space_slug: Option<String>,
    to_space: Option<String>,
    to_space_slug: Option<String>,
}

async fn rows(
    db: &Db,
    viewer: &Viewer,
    scope: Scope,
    only: Option<i64>,
    limit: i64,
    offset: i64,
) -> sqlx::Result<(i64, Vec<Row>)> {
    let filter = viewer.filter();
    let everything = viewer.is_staff() && scope != Scope::Public;
    let member = match scope {
        Scope::Member(id) => Some(id),
        _ => None,
    };
    let total = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM mod_actions a
           WHERE ($1 OR a.public) AND ($2::bigint IS NULL OR a.user_id = $2)
             AND ($3::bigint IS NULL OR a.id = $3)"#,
        everything,
        member,
        only,
    )
    .fetch_one(db.pool())
    .await?;
    let rows = sqlx::query_as!(
        Row,
        r#"
        SELECT a.id AS "id!", a.action AS "action!", a.reason AS "reason!", a.public AS "public!",
               a.details AS "details!", a.created_at AS "created_at!", a.post_id AS "post_id?",
               actor.id AS "actor_id!", actor.username AS "actor_username!: String",
               actor.display_name AS "actor_name!",
               u.id AS "user_id?", u.username AS "username?: String", u.display_name AS "user_name?",
               t.id AS "thread_id?", t.title AS "thread_title?", t.slug AS "thread_slug?",
               t.space_id IS NULL AS "thread_private?", t.deleted_at IS NULL AS "thread_live?",
               (thread_readable(t, $4, $5, $6, $9)
                OR dm_readable(t.id, t.visibility, $6)) AS "thread_readable?",
               t2.id AS "to_thread_id?", t2.title AS "to_title?", t2.slug AS "to_slug?",
               t2.deleted_at IS NULL AS "to_live?",
               thread_readable(t2, $4, $5, $6, $9) AS "to_readable?",
               CASE WHEN space_readable(c1.visibility, c1.id, $4, $5) THEN c1.name END AS "from_space?",
               CASE WHEN space_readable(c1.visibility, c1.id, $4, $5) THEN c1.slug END AS "from_space_slug?",
               CASE WHEN space_readable(c2.visibility, c2.id, $4, $5) THEN c2.name END AS "to_space?",
               CASE WHEN space_readable(c2.visibility, c2.id, $4, $5) THEN c2.slug END AS "to_space_slug?"
        FROM mod_actions a
        JOIN users actor ON actor.id = a.actor_id
        LEFT JOIN users u ON u.id = a.user_id
        LEFT JOIN threads t ON t.id = a.thread_id
        LEFT JOIN threads t2 ON t2.id = (a.details->>'to_thread_id')::bigint
        LEFT JOIN spaces c1 ON c1.id = (a.details->>'from_space_id')::bigint
        LEFT JOIN spaces c2 ON c2.id = (a.details->>'to_space_id')::bigint
        WHERE ($1 OR a.public) AND ($2::bigint IS NULL OR a.user_id = $2)
          AND ($3::bigint IS NULL OR a.id = $3)
        ORDER BY a.created_at DESC, a.id DESC
        LIMIT $7 OFFSET $8
        "#,
        everything,
        member,
        only,
        &filter.visibilities,
        &filter.restricted_space_ids,
        filter.dm_viewer,
        limit,
        offset,
        &filter.group_ids,
    )
    .fetch_all(db.pool())
    .await?;
    Ok((total, rows))
}

/// One page of the log.
pub async fn log(
    db: &Db,
    viewer: &Viewer,
    scope: Scope,
    page: i64,
    deps: &mut Deps,
) -> sqlx::Result<LogPage> {
    deps.add(Tag::ModLog);
    let Some(offset) = page_offset(page, ENTRIES_PER_PAGE) else {
        return Ok(LogPage {
            total: 0,
            entries: Vec::new(),
        });
    };
    let (total, rows) = rows(db, viewer, scope, None, ENTRIES_PER_PAGE, offset).await?;
    let entries = rows
        .into_iter()
        .map(|row| {
            for thread in [row.thread_id, row.to_thread_id].into_iter().flatten() {
                deps.add(Tag::Thread(thread));
            }
            describe(row, viewer.is_staff())
        })
        .collect();
    Ok(LogPage { total, entries })
}

/// One entry, if the viewer may see it.
pub async fn entry(db: &Db, viewer: &Viewer, id: i64) -> sqlx::Result<Option<LogEntry>> {
    let (_, rows) = rows(db, viewer, Scope::All, Some(id), 1, 0).await?;
    Ok(rows
        .into_iter()
        .next()
        .map(|row| describe(row, viewer.is_staff())))
}

/// The entries about one member that they may see, for contesting.
pub async fn entry_for_member(db: &Db, viewer: &Viewer, id: i64) -> sqlx::Result<Option<LogEntry>> {
    // A member always sees the entries about themselves, even ones not in
    // the public log, but only as far as the threads they can read.
    let Some(user_id) = viewer.user_id() else {
        return Ok(None);
    };
    // An invite branch's suspension is about everyone suspended with it.
    let theirs = sqlx::query_scalar!(
        r#"SELECT user_id = $2
                  OR (action = 'suspend_branch' AND details->'user_ids' @> to_jsonb($2::bigint))
                  AS "theirs!"
           FROM mod_actions WHERE id = $1"#,
        id,
        user_id
    )
    .fetch_optional(db.pool())
    .await?;
    if theirs != Some(true) {
        return Ok(None);
    }
    let filter = viewer.filter();
    let rows = sqlx::query_as!(
        Row,
        r#"
        SELECT a.id AS "id!", a.action AS "action!", a.reason AS "reason!", a.public AS "public!",
               a.details AS "details!", a.created_at AS "created_at!", a.post_id AS "post_id?",
               actor.id AS "actor_id!", actor.username AS "actor_username!: String",
               actor.display_name AS "actor_name!",
               u.id AS "user_id?", u.username AS "username?: String", u.display_name AS "user_name?",
               t.id AS "thread_id?", t.title AS "thread_title?", t.slug AS "thread_slug?",
               t.space_id IS NULL AS "thread_private?", t.deleted_at IS NULL AS "thread_live?",
               (thread_readable(t, $2, $3, $4, $5)
                OR dm_readable(t.id, t.visibility, $4)) AS "thread_readable?",
               NULL::bigint AS "to_thread_id?", NULL::text AS "to_title?", NULL::text AS "to_slug?",
               NULL::boolean AS "to_live?", NULL::boolean AS "to_readable?",
               NULL::text AS "from_space?", NULL::text AS "from_space_slug?",
               NULL::text AS "to_space?", NULL::text AS "to_space_slug?"
        FROM mod_actions a
        JOIN users actor ON actor.id = a.actor_id
        LEFT JOIN users u ON u.id = a.user_id
        LEFT JOIN threads t ON t.id = a.thread_id
        WHERE a.id = $1
        "#,
        id,
        &filter.visibilities,
        &filter.restricted_space_ids,
        filter.dm_viewer,
        &filter.group_ids,
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .into_iter()
        .next()
        .map(|row| describe(row, viewer.is_staff())))
}

fn text(s: impl Into<String>) -> Bit {
    Bit::Text(s.into())
}

fn thread_bit(
    id: Option<i64>,
    title: Option<String>,
    slug: Option<String>,
    private: Option<bool>,
    live: Option<bool>,
    readable: Option<bool>,
    staff: bool,
) -> Bit {
    let live = live == Some(true);
    match (id, title, slug, readable) {
        (Some(id), _, _, Some(true)) if private == Some(true) => Bit::Link {
            href: format!("/messages/{id}"),
            text: "a conversation".to_owned(),
        },
        (Some(id), Some(title), Some(slug), Some(true)) if live => Bit::Link {
            href: crate::format::thread_href(&id, &slug),
            text: format!("“{title}”"),
        },
        // Only staff are told what a deleted thread was called, and are
        // led to where it can be restored.
        (Some(id), Some(title), _, Some(true)) if staff => Bit::Link {
            href: format!("/mod/t/{id}"),
            text: format!("“{title}” (deleted)"),
        },
        (Some(_), _, _, Some(true)) => text("a thread that has since been deleted"),
        (Some(_), ..) => text("a thread you can’t see"),
        (None, ..) => text("a thread"),
    }
}

fn space_bit(name: Option<String>, slug: Option<String>) -> Bit {
    match (name, slug) {
        (Some(name), Some(slug)) => Bit::Link {
            href: format!("/c/{slug}"),
            text: name,
        },
        _ => text("a space you can’t see"),
    }
}

fn describe(row: Row, staff: bool) -> LogEntry {
    let user = match (row.user_id, row.username, row.user_name) {
        (Some(id), Some(username), Some(display_name)) => Some(UserRef {
            id,
            username,
            display_name,
        }),
        _ => None,
    };
    let who = || match &user {
        Some(u) => Bit::Link {
            href: format!("/u/{}", u.username),
            text: u.username.clone(),
        },
        None => text("someone"),
    };
    // Whether the viewer may know the thread's title, past or present.
    let readable = row.thread_readable == Some(true) && (row.thread_live == Some(true) || staff);
    let thread = thread_bit(
        row.thread_id,
        row.thread_title.clone(),
        row.thread_slug.clone(),
        row.thread_private,
        row.thread_live,
        row.thread_readable,
        staff,
    );
    let seq = row.details["seq"].as_i64();
    let post = || match seq {
        Some(seq) => format!("post № {seq}"),
        None => "a post".to_owned(),
    };
    let days = row.details["days"].as_i64();
    let for_days = || match days {
        Some(d) => format!(" for {}", crate::format::plural(d, "day", "days")),
        None => String::new(),
    };
    // A link to the post itself, where the viewer can follow it: for
    // staff, to where it can be moderated (and restored, if deleted).
    let post_link = match row.post_id {
        Some(id) if readable && row.thread_live == Some(true) => Bit::Link {
            href: if staff {
                format!("/mod/p/{id}")
            } else {
                format!("/p/{id}")
            },
            text: post(),
        },
        _ => text(post()),
    };
    let on_post = |verb: &str| {
        vec![
            text(format!("{verb} ")),
            post_link.clone(),
            text(" by "),
            who(),
            text(" in "),
            thread.clone(),
        ]
    };
    let bits = match row.action.as_str() {
        "hide_post" => on_post("hid"),
        "unhide_post" => on_post("showed again"),
        "delete_post" => on_post("deleted"),
        "restore_post" => on_post("restored"),
        "edit_post" => on_post("edited"),
        "redact_revision" => {
            let rev = row.details["rev"].as_i64().unwrap_or_default();
            let mut bits = vec![text(format!("redacted version {rev} of "))];
            bits.extend(on_post("").into_iter().map(|b| match b {
                Bit::Text(t) => Bit::Text(t.trim_start().to_owned()),
                other => other,
            }));
            bits
        }
        "lock_thread" => vec![text("locked "), thread],
        "unlock_thread" => vec![text("unlocked "), thread],
        "pin_thread" => vec![text("pinned "), thread],
        "unpin_thread" => vec![text("unpinned "), thread],
        "delete_thread" => vec![text("deleted the thread "), thread],
        "restore_thread" => vec![text("restored the thread "), thread],
        "rename_thread" => {
            let mut bits = vec![text("renamed "), thread];
            if readable && let Some(old) = row.details["old_title"].as_str() {
                bits.push(text(format!(" (it was “{old}”)")));
            }
            bits
        }
        "restrict_thread" => vec![text("changed who can read or reply in "), thread],
        "move_thread" => vec![
            text("moved "),
            thread,
            text(" from "),
            space_bit(row.from_space, row.from_space_slug),
            text(" to "),
            space_bit(row.to_space, row.to_space_slug),
        ],
        "move_posts" => {
            let count = row.details["count"].as_i64().unwrap_or_default();
            let new = row.details["new_thread"].as_bool().unwrap_or_default();
            vec![
                text(format!(
                    "moved {} from ",
                    crate::format::plural(count, "post", "posts")
                )),
                thread,
                text(if new { " to a new thread, " } else { " to " }),
                thread_bit(
                    row.to_thread_id,
                    row.to_title,
                    row.to_slug,
                    Some(false),
                    row.to_live,
                    row.to_readable,
                    staff,
                ),
            ]
        }
        "silence" => vec![text("silenced "), who(), text(for_days())],
        "suspend" => vec![text("suspended "), who(), text(for_days())],
        "unsilence" => vec![text("lifted the silence on "), who()],
        "unsuspend" => vec![text("lifted the suspension of "), who()],
        "suspend_branch" => {
            let others = row.details["user_ids"]
                .as_array()
                .map_or(0, |ids| ids.len().saturating_sub(1)) as i64;
            vec![
                text("suspended "),
                who(),
                text(format!(
                    " and {} they invited{}",
                    crate::format::plural(others, "other member", "other members"),
                    for_days()
                )),
            ]
        }
        "delete_spammer" => vec![text("deleted "), who(), text(" as a spammer")],
        "change_role" => {
            use crate::accounts::Role;
            let role = |key: &str| {
                row.details[key]
                    .as_str()
                    .and_then(Role::parse)
                    .unwrap_or(Role::User)
            };
            let (from, to) = (role("from"), role("to"));
            match to {
                Role::User => vec![
                    text(format!(
                        "took away the {} role of ",
                        from.label().to_lowercase()
                    )),
                    who(),
                ],
                Role::Moderator => vec![text("made "), who(), text(" a moderator")],
                Role::Admin => vec![text("made "), who(), text(" an admin")],
            }
        }
        "edit_space"
        | "archive_space"
        | "unarchive_space"
        | "delete_space"
        | "mark_space_essential"
        | "unmark_space_essential" => {
            // What was done to the space, before and after its name.
            let (before, after) = match row.action.as_str() {
                "edit_space" => ("changed the settings of ", ""),
                "archive_space" => ("archived ", ""),
                "unarchive_space" => ("opened again ", ""),
                "mark_space_essential" => ("made ", " essential"),
                "unmark_space_essential" => ("made ", " no longer essential"),
                _ => ("deleted the space ", ""),
            };
            let name = row.details["name"].as_str().unwrap_or("a space").to_owned();
            let space = match row.details["slug"].as_str() {
                Some(slug) if row.action != "delete_space" => Bit::Link {
                    href: format!("/c/{slug}"),
                    text: name,
                },
                _ => text(name),
            };
            let mut bits = vec![text(before), space];
            if !after.is_empty() {
                bits.push(text(after));
            }
            bits
        }
        "approve_post" => vec![
            text("approved a post by new member "),
            who(),
            text(" in "),
            thread,
        ],
        "reject_post" => vec![text("didn’t approve a post by new member "), who()],
        "dismiss_report" => match user {
            Some(_) => vec![text("dismissed a report about "), who()],
            None => vec![text("dismissed a message to the moderators")],
        },
        "add_note" => vec![text("added a staff note about "), who()],
        "remove_image" => match user {
            Some(_) => vec![text("removed an image uploaded by "), who()],
            None => vec![text("removed an image")],
        },
        "uphold_appeal" => vec![text("kept a decision "), who(), text(" contested")],
        "overturn_appeal" => vec![text("reversed a decision "), who(), text(" contested")],
        other => vec![text(other.replace('_', " "))],
    };
    LogEntry {
        id: row.id,
        actor: UserRef {
            id: row.actor_id,
            username: row.actor_username,
            display_name: row.actor_name,
        },
        action: row.action,
        user,
        bits,
        reason: row.reason,
        public: row.public,
        created_at: row.created_at,
    }
}

/// Who started a thread.
pub async fn thread_author(db: &Db, thread_id: i64) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar!("SELECT author_id FROM threads WHERE id = $1", thread_id)
        .fetch_optional(db.pool())
        .await
}

/// A deleted thread, as staff see it to restore it.
pub struct DeletedThread {
    pub id: i64,
    pub title: String,
    /// The space it was in, if the viewer can read it.
    pub space: Option<(String, String)>,
    /// If all its posts were moved, the thread they went to: its id, and
    /// its title if the viewer can read it.
    pub merged_into: Option<(i64, Option<String>)>,
}

/// A deleted thread in a space, for staff only.
pub async fn deleted_thread(
    db: &Db,
    viewer: &Viewer,
    thread_id: i64,
) -> sqlx::Result<Option<DeletedThread>> {
    if !viewer.is_staff() {
        return Ok(None);
    }
    let filter = viewer.filter();
    let row = sqlx::query!(
        r#"SELECT t.id, t.title, c.name AS "space_name?", c.slug AS "space_slug?",
                  d.id AS "merged_id?",
                  CASE WHEN thread_readable(d, $2, $3, $4, $5) THEN d.title END AS "merged_title?"
           FROM threads t
           JOIN spaces c ON c.id = t.space_id
           LEFT JOIN threads d ON d.id = t.merged_into_thread_id
           WHERE t.id = $1 AND t.deleted_at IS NOT NULL
             AND thread_readable(t, $2, $3, $4, $5)"#,
        thread_id,
        &filter.visibilities,
        &filter.restricted_space_ids,
        filter.dm_viewer,
        &filter.group_ids,
    )
    .fetch_optional(db.pool())
    .await?;
    Ok(row.map(|row| DeletedThread {
        id: row.id,
        title: row.title,
        space: row.space_name.zip(row.space_slug),
        merged_into: row.merged_id.map(|id| (id, row.merged_title)),
    }))
}

/// The thread a deleted one was merged into, if the viewer can read it:
/// its id and slug.
pub async fn merged_into(
    db: &Db,
    viewer: &Viewer,
    thread_id: i64,
) -> sqlx::Result<Option<(i64, String)>> {
    let filter = viewer.filter();
    let row = sqlx::query!(
        r#"SELECT d.id, d.slug FROM threads t JOIN threads d ON d.id = t.merged_into_thread_id
           WHERE t.id = $1 AND t.deleted_at IS NOT NULL AND d.deleted_at IS NULL
             AND thread_readable(t, $2, $3, $4, $5)
             AND thread_readable(d, $2, $3, $4, $5)"#,
        thread_id,
        &filter.visibilities,
        &filter.restricted_space_ids,
        filter.dm_viewer,
        &filter.group_ids,
    )
    .fetch_optional(db.pool())
    .await?;
    Ok(row.map(|row| (row.id, row.slug)))
}

/// A thread in a space a member of staff can't read, as they see it on its
/// moderation page (DESIGN.md §3.1, §8). They run every space, so they may
/// still lock, pin, rename, move, delete or restore it, but they read only
/// what their own memberships let them: nothing of what it says, its
/// title included, is given here.
#[derive(Clone, Debug)]
pub struct ClosedThread {
    pub id: i64,
    /// Its space, which staff run (and so may know the name of).
    pub space: SpaceTarget,
    pub author: UserRef,
    pub created_at: DateTime<Utc>,
    pub last_seq: i64,
    pub pinned: bool,
    pub locked: bool,
    pub deleted: bool,
    /// Whether all its posts were moved to another thread.
    pub merged: bool,
    pub restriction: ThreadRestriction,
}

impl ClosedThread {
    /// Who can read it.
    pub fn audience(&self) -> Audience {
        self.restriction.audience(&self.space.rules)
    }
}

/// A thread in a space, live or deleted, that the viewer runs as staff but
/// can't read, or `None` (for anyone who isn't staff, for a conversation,
/// and for a thread they can read, which they moderate from its own
/// pages).
pub async fn closed_thread(
    db: &Db,
    viewer: &Viewer,
    thread_id: i64,
) -> sqlx::Result<Option<ClosedThread>> {
    if !viewer.is_staff() {
        return Ok(None);
    }
    let filter = viewer.filter();
    let Some(row) = sqlx::query!(
        r#"SELECT t.id, t.space_id AS "space_id!", t.created_at, t.last_seq, t.pinned, t.locked,
                  t.deleted_at IS NOT NULL AS "deleted!", t.merged_into_thread_id IS NOT NULL AS "merged!",
                  t.read_group_ids, t.read_user_ids, t.reply_group_ids, t.reply_user_ids,
                  a.id AS author_id, a.username::text AS "author_username!", a.display_name AS author_name
           FROM threads t JOIN users a ON a.id = t.author_id
           WHERE t.id = $1 AND t.space_id IS NOT NULL
             AND NOT thread_readable(t, $2, $3, $4, $5)"#,
        thread_id,
        &filter.visibilities,
        &filter.restricted_space_ids,
        filter.dm_viewer,
        &filter.group_ids,
    )
    .fetch_optional(db.pool())
    .await?
    else {
        return Ok(None);
    };
    let Some(space) = super::administered_space_by_id(db, viewer, row.space_id).await? else {
        return Ok(None);
    };
    Ok(Some(ClosedThread {
        id: row.id,
        space,
        author: UserRef {
            id: row.author_id,
            username: row.author_username,
            display_name: row.author_name,
        },
        created_at: row.created_at,
        last_seq: i64::from(row.last_seq),
        pinned: row.pinned,
        locked: row.locked,
        deleted: row.deleted,
        merged: row.merged,
        restriction: ThreadRestriction::from_parts(
            row.read_group_ids.as_deref(),
            row.read_user_ids.as_deref(),
            row.reply_group_ids.as_deref(),
            row.reply_user_ids.as_deref(),
        ),
    }))
}

/// A post a member of staff can't read, as a report about it shows it in
/// the moderation queue (DESIGN.md §8: the content appears there only if
/// they can already read it): who wrote it and where, nothing it says.
#[derive(Clone, Debug)]
pub struct ClosedPost {
    pub post_id: i64,
    pub author: UserRef,
    /// The space it is in, or `None` for a conversation.
    pub space: Option<SpaceRef>,
}

/// A live post in a live thread or conversation that the viewer, as
/// staff, can't read; `None` for anyone else and for anything else.
pub async fn closed_post(
    db: &Db,
    viewer: &Viewer,
    post_id: i64,
) -> sqlx::Result<Option<ClosedPost>> {
    if !viewer.is_staff() {
        return Ok(None);
    }
    let filter = viewer.filter();
    let row = sqlx::query!(
        r#"SELECT p.id, a.id AS author_id, a.username::text AS "author_username!",
                  a.display_name AS author_name,
                  c.id AS "space_id?", c.slug::text AS "space_slug?", c.name AS "space_name?"
           FROM posts p
           JOIN threads t ON t.id = p.thread_id
           JOIN users a ON a.id = p.author_id
           LEFT JOIN spaces c ON c.id = t.space_id
           WHERE p.id = $1 AND p.deleted_at IS NULL AND t.deleted_at IS NULL
             AND NOT (thread_readable(t, $2, $3, $4, $5) OR dm_readable(t.id, t.visibility, $4))"#,
        post_id,
        &filter.visibilities,
        &filter.restricted_space_ids,
        filter.dm_viewer,
        &filter.group_ids,
    )
    .fetch_optional(db.pool())
    .await?;
    Ok(row.map(|row| ClosedPost {
        post_id: row.id,
        author: UserRef {
            id: row.author_id,
            username: row.author_username,
            display_name: row.author_name,
        },
        space: match (row.space_id, row.space_slug, row.space_name) {
            (Some(id), Some(slug), Some(name)) => Some(SpaceRef { id, slug, name }),
            _ => None,
        },
    }))
}
