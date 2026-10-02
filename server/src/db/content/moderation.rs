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
    from_category: Option<String>,
    from_category_slug: Option<String>,
    to_category: Option<String>,
    to_category_slug: Option<String>,
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
        SELECT a.id, a.action, a.reason, a.public, a.details, a.created_at, a.post_id,
               actor.id AS actor_id, actor.username AS "actor_username!: String",
               actor.display_name AS actor_name,
               u.id AS "user_id?", u.username AS "username?: String", u.display_name AS "user_name?",
               t.id AS "thread_id?", t.title AS "thread_title?", t.slug AS "thread_slug?",
               t.category_id IS NULL AS "thread_private?", t.deleted_at IS NULL AS "thread_live?",
               (thread_readable(t.visibility, t.category_id, $4, $5)
                OR dm_readable(t.id, t.visibility, $6)) AS "thread_readable?",
               t2.id AS "to_thread_id?", t2.title AS "to_title?", t2.slug AS "to_slug?",
               t2.deleted_at IS NULL AS "to_live?",
               thread_readable(t2.visibility, t2.category_id, $4, $5) AS "to_readable?",
               CASE WHEN category_readable(c1.read_level, c1.id, $4, $5) THEN c1.name END AS "from_category?",
               CASE WHEN category_readable(c1.read_level, c1.id, $4, $5) THEN c1.slug END AS "from_category_slug?",
               CASE WHEN category_readable(c2.read_level, c2.id, $4, $5) THEN c2.name END AS "to_category?",
               CASE WHEN category_readable(c2.read_level, c2.id, $4, $5) THEN c2.slug END AS "to_category_slug?"
        FROM mod_actions a
        JOIN users actor ON actor.id = a.actor_id
        LEFT JOIN users u ON u.id = a.user_id
        LEFT JOIN threads t ON t.id = a.thread_id
        LEFT JOIN threads t2 ON t2.id = (a.details->>'to_thread_id')::bigint
        LEFT JOIN categories c1 ON c1.id = (a.details->>'from_category_id')::bigint
        LEFT JOIN categories c2 ON c2.id = (a.details->>'to_category_id')::bigint
        WHERE ($1 OR a.public) AND ($2::bigint IS NULL OR a.user_id = $2)
          AND ($3::bigint IS NULL OR a.id = $3)
        ORDER BY a.created_at DESC, a.id DESC
        LIMIT $7 OFFSET $8
        "#,
        everything,
        member,
        only,
        &filter.visibilities,
        &filter.group_category_ids,
        filter.dm_viewer,
        limit,
        offset,
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
        SELECT a.id, a.action, a.reason, a.public, a.details, a.created_at, a.post_id,
               actor.id AS actor_id, actor.username AS "actor_username!: String",
               actor.display_name AS actor_name,
               u.id AS "user_id?", u.username AS "username?: String", u.display_name AS "user_name?",
               t.id AS "thread_id?", t.title AS "thread_title?", t.slug AS "thread_slug?",
               t.category_id IS NULL AS "thread_private?", t.deleted_at IS NULL AS "thread_live?",
               (thread_readable(t.visibility, t.category_id, $2, $3)
                OR dm_readable(t.id, t.visibility, $4)) AS "thread_readable?",
               NULL::bigint AS "to_thread_id?", NULL::text AS "to_title?", NULL::text AS "to_slug?",
               NULL::boolean AS "to_live?", NULL::boolean AS "to_readable?",
               NULL::text AS "from_category?", NULL::text AS "from_category_slug?",
               NULL::text AS "to_category?", NULL::text AS "to_category_slug?"
        FROM mod_actions a
        JOIN users actor ON actor.id = a.actor_id
        LEFT JOIN users u ON u.id = a.user_id
        LEFT JOIN threads t ON t.id = a.thread_id
        WHERE a.id = $1
        "#,
        id,
        &filter.visibilities,
        &filter.group_category_ids,
        filter.dm_viewer,
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

fn category_bit(name: Option<String>, slug: Option<String>) -> Bit {
    match (name, slug) {
        (Some(name), Some(slug)) => Bit::Link {
            href: format!("/c/{slug}"),
            text: name,
        },
        _ => text("a category you can’t see"),
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
        "move_thread" => vec![
            text("moved "),
            thread,
            text(" from "),
            category_bit(row.from_category, row.from_category_slug),
            text(" to "),
            category_bit(row.to_category, row.to_category_slug),
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
    /// The category it was in, if the viewer can read it.
    pub category: Option<(String, String)>,
    /// If all its posts were moved, the thread they went to: its id and
    /// title.
    pub merged_into: Option<(i64, String)>,
}

/// A deleted thread in a category, for staff only.
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
        r#"SELECT t.id, t.title, c.name AS "category_name?", c.slug AS "category_slug?",
                  d.id AS "merged_id?", d.title AS "merged_title?"
           FROM threads t
           JOIN categories c ON c.id = t.category_id
           LEFT JOIN threads d ON d.id = t.merged_into_thread_id
           WHERE t.id = $1 AND t.deleted_at IS NOT NULL
             AND thread_readable(t.visibility, t.category_id, $2, $3)"#,
        thread_id,
        &filter.visibilities,
        &filter.group_category_ids,
    )
    .fetch_optional(db.pool())
    .await?;
    Ok(row.map(|row| DeletedThread {
        id: row.id,
        title: row.title,
        category: row.category_name.zip(row.category_slug),
        merged_into: row.merged_id.zip(row.merged_title),
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
             AND thread_readable(t.visibility, t.category_id, $2, $3)
             AND thread_readable(d.visibility, d.category_id, $2, $3)"#,
        thread_id,
        &filter.visibilities,
        &filter.group_category_ids,
    )
    .fetch_optional(db.pool())
    .await?;
    Ok(row.map(|row| (row.id, row.slug)))
}
