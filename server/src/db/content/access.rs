//! Break-glass grants (DESIGN.md §8) as people may see them: the line a
//! thread's, a space's or a conversation's page shows its readers for as
//! long as the grant's row exists (which is forever), the notice each of
//! them is sent, the log as admins see it at `/admin/access` and as each
//! member sees what touched their threads and spaces at `/settings/access`,
//! and what the confirmation page may say about what a member of staff is
//! about to open, which is where it is and who can read it, never what it
//! says.
//!
//! A grant is the business of everyone told about it: those who could
//! read what it opened when it was opened, or before the change that
//! opened it, each of whom has a notification tied to it. They see its
//! notice and its entry in their log for good, whether or not they can
//! still read what it opened; it is named by its title, and its space by
//! its name, only while they can read them, as `thread_readable`,
//! `dm_readable` and `space_readable` decide. Admins see every grant in
//! the log, each named by what it is and where, and by its title only if
//! they can read it themselves.

use super::*;
use crate::db::staff_access::Scope;

/// What a grant opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum What {
    Thread,
    Conversation,
    Space,
}

impl What {
    /// "this thread", as a page about it says.
    pub fn this(self) -> &'static str {
        match self {
            What::Thread => "this thread",
            What::Conversation => "this conversation",
            What::Space => "this space",
        }
    }
}

/// The line a page shows its readers about a grant on it.
#[derive(Clone, Debug)]
pub struct PageNotice {
    pub moderator: UserRef,
    pub what: What,
    pub created_at: DateTime<Utc>,
}

impl PageNotice {
    /// "A moderator (Sam) viewed this thread on 3 Oct 2026."
    pub fn sentence(&self) -> String {
        format!(
            "A moderator ({}) viewed {} on {}.",
            self.moderator.display_name,
            self.what.this(),
            crate::format::date(&self.created_at)
        )
    }
}

/// The lines for the page of a thread or conversation, oldest first: the
/// grants on it. Call only once the viewer is known to read it.
pub(super) async fn thread_notices(
    db: &Db,
    thread_id: i64,
    what: What,
) -> sqlx::Result<Vec<PageNotice>> {
    let rows = sqlx::query!(
        r#"SELECT a.created_at, u.id, u.username::text AS "username!", u.display_name
           FROM staff_access a JOIN users u ON u.id = a.actor_id
           WHERE a.thread_id = $1
           ORDER BY a.id"#,
        thread_id
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| PageNotice {
            moderator: UserRef {
                id: row.id,
                username: row.username,
                display_name: row.display_name,
            },
            what,
            created_at: row.created_at,
        })
        .collect())
}

/// The lines for a space's page, oldest first: the grants on the whole
/// space. Call only once the viewer is known to read it.
pub(super) async fn space_notices(db: &Db, space_id: i64) -> sqlx::Result<Vec<PageNotice>> {
    let rows = sqlx::query!(
        r#"SELECT a.created_at, u.id, u.username::text AS "username!", u.display_name
           FROM staff_access a JOIN users u ON u.id = a.actor_id
           WHERE a.space_id = $1
           ORDER BY a.id"#,
        space_id
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| PageNotice {
            moderator: UserRef {
                id: row.id,
                username: row.username,
                display_name: row.display_name,
            },
            what: What::Space,
            created_at: row.created_at,
        })
        .collect())
}

/// A grant as its notice names it to a member who was told about it.
#[derive(Clone, Debug)]
pub struct About {
    pub access_id: i64,
    pub moderator: UserRef,
    pub what: What,
    /// The thread's title, the conversation's (or "" for an untitled one),
    /// or the space's name, if the member can read it now; `None` if they
    /// can't any more.
    pub title: Option<String>,
    /// For a thread they can't read any more, the name of its space, if
    /// they read that.
    pub place: Option<String>,
    /// Where the notice leads: what was opened, if they can read it, and
    /// otherwise their log, which says why.
    pub href: String,
    pub created_at: DateTime<Utc>,
}

impl About {
    /// What the moderator viewed, as a notice says it: "the thread
    /// “Brass”", or, to someone who can't read it any more, "a thread you
    /// could read in General".
    pub fn object(&self) -> String {
        match (self.what, &self.title) {
            (What::Thread, Some(title)) => format!("the thread “{title}”"),
            (What::Thread, None) => match &self.place {
                Some(space) => format!("a thread you could read in {space}"),
                None => "a thread you could read".to_owned(),
            },
            (What::Conversation, Some(title)) if title.is_empty() => {
                "a conversation you’re in".to_owned()
            }
            (What::Conversation, Some(title)) => format!("your conversation “{title}”"),
            (What::Conversation, None) => "a conversation you were in".to_owned(),
            (What::Space, Some(name)) => format!("the space {name}"),
            (What::Space, None) => "a space you could read".to_owned(),
        }
    }

    /// "A moderator (Sam) viewed the thread “Brass” on 3 Oct 2026."
    pub fn sentence(&self) -> String {
        format!(
            "A moderator ({}) viewed {} on {}.",
            self.moderator.display_name,
            self.object(),
            crate::format::date(&self.created_at)
        )
    }
}

/// A grant, if the viewer was told about it: everyone who could read what
/// it opened when it was opened, or before the change that opened it,
/// whether or not they can read it now (DESIGN.md §8). It is named by its
/// title only while they can.
pub async fn about(db: &Db, viewer: &Viewer, access_id: i64) -> sqlx::Result<Option<About>> {
    let Some(user_id) = viewer.user_id() else {
        return Ok(None);
    };
    let filter = viewer.filter();
    let Some(row) = sqlx::query!(
        r#"SELECT a.id, a.created_at, u.id AS actor_id, u.username::text AS "actor_username!",
                  u.display_name AS actor_name,
                  t.id AS "thread_id?", t.title AS "title?", t.slug AS "slug?",
                  t.space_id IS NULL AS "conversation?",
                  (t.deleted_at IS NULL
                   AND (thread_readable(t, $2, $3, $4, $5) OR dm_readable(t.id, t.visibility, $4)))
                      AS "thread_readable?",
                  tc.name AS "thread_space?",
                  space_readable(tc.visibility, tc.id, $2, $3) AS "thread_space_readable?",
                  c.slug::text AS "space_slug?", c.name AS "space_name?",
                  space_readable(c.visibility, c.id, $2, $3) AS "space_readable?"
           FROM staff_access a
           JOIN users u ON u.id = a.actor_id
           LEFT JOIN threads t ON t.id = a.thread_id
           LEFT JOIN spaces tc ON tc.id = t.space_id
           LEFT JOIN spaces c ON c.id = a.space_id
           WHERE a.id = $1
             AND EXISTS (SELECT 1 FROM notifications n
                         WHERE n.staff_access_id = a.id AND n.user_id = $6)"#,
        access_id,
        &filter.visibilities,
        &filter.restricted_space_ids,
        filter.dm_viewer,
        &filter.group_ids,
        user_id,
    )
    .fetch_optional(db.pool())
    .await?
    else {
        return Ok(None);
    };
    let log = "/settings/access".to_owned();
    let thread_readable = row.thread_readable == Some(true);
    let (what, title, place, href) = match (row.thread_id, row.space_slug) {
        (Some(id), _) if row.conversation == Some(true) => (
            What::Conversation,
            thread_readable.then(|| row.title.unwrap_or_default()),
            None,
            if thread_readable {
                format!("/messages/{id}")
            } else {
                log
            },
        ),
        (Some(id), _) if thread_readable => (
            What::Thread,
            row.title,
            None,
            crate::format::thread_href(&id, &row.slug.unwrap_or_default()),
        ),
        (Some(_), _) => (
            What::Thread,
            None,
            row.thread_space
                .filter(|_| row.thread_space_readable == Some(true)),
            log,
        ),
        (None, Some(slug)) if row.space_readable == Some(true) => {
            (What::Space, row.space_name, None, format!("/c/{slug}"))
        }
        (None, Some(_)) => (What::Space, None, None, log),
        (None, None) => return Ok(None),
    };
    Ok(Some(About {
        access_id: row.id,
        moderator: UserRef {
            id: row.actor_id,
            username: row.actor_username,
            display_name: row.actor_name,
        },
        what,
        title,
        place,
        href,
        created_at: row.created_at,
    }))
}

/// One grant in the log.
#[derive(Clone, Debug)]
pub struct Logged {
    pub id: i64,
    pub moderator: UserRef,
    pub what: What,
    /// What it opened, as the log says it after "viewed": by its title or
    /// name where the viewer reads it ("the thread “Brass”"), and otherwise
    /// by what it is and where, as far as they may know that ("the thread
    /// № 12 in Circle", "the conversation between 2 people", "a space you
    /// could read").
    pub object: String,
    pub href: Option<String>,
    pub reason: String,
    /// Whether a report led to it.
    pub after_report: bool,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// How many pages were shown under it.
    pub views: i64,
    /// Whether a change of who may read what it opened opened it, so that
    /// its maker reads it as one of its readers for as long as the change
    /// stands, and no page is recorded under it (migration 0036).
    pub by_change: bool,
}

impl Logged {
    /// Whether it is still running.
    pub fn running(&self) -> bool {
        self.expires_at > Utc::now()
    }
}

/// How many grants a page of either log lists.
pub const LOG_PER_PAGE: i64 = 50;

/// One page of a log, and how many grants it has in all.
pub struct LogPage {
    pub total: i64,
    pub entries: Vec<Logged>,
}

/// Every grant, newest first, a page at a time, for admins
/// (`/admin/access`); nothing for anyone else.
pub async fn log(db: &Db, viewer: &Viewer, page: i64) -> sqlx::Result<LogPage> {
    if !viewer.is_admin() {
        return Ok(LogPage {
            total: 0,
            entries: Vec::new(),
        });
    }
    logged(db, viewer, true, page).await
}

/// The grants the viewer was told about, newest first, a page at a time
/// (`/settings/access`): each time a moderator looked at what they could
/// read then, whether or not they can read it now (DESIGN.md §8).
pub async fn affecting(db: &Db, viewer: &Viewer, page: i64) -> sqlx::Result<LogPage> {
    if viewer.user().is_none() {
        return Ok(LogPage {
            total: 0,
            entries: Vec::new(),
        });
    }
    logged(db, viewer, false, page).await
}

async fn logged(db: &Db, viewer: &Viewer, everything: bool, page: i64) -> sqlx::Result<LogPage> {
    let Some(offset) = super::page_offset(page, LOG_PER_PAGE) else {
        return Ok(LogPage {
            total: 0,
            entries: Vec::new(),
        });
    };
    let total = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM staff_access a
           WHERE $1 OR EXISTS (SELECT 1 FROM notifications n
                               WHERE n.staff_access_id = a.id AND n.user_id = $2)"#,
        everything,
        viewer.user_id(),
    )
    .fetch_one(db.pool())
    .await?;
    let filter = viewer.filter();
    // Staff run every space, so know every space's name and the staff
    // pages about each thread; anyone else only those they read.
    let staff = viewer.is_staff();
    let rows = sqlx::query!(
        r#"SELECT a.id, a.reason, a.report_id IS NOT NULL AS "after_report!", a.created_at,
                  a.expires_at, a.by_change, u.id AS actor_id, u.username::text AS "actor_username!",
                  u.display_name AS actor_name,
                  t.id AS "thread_id?", t.title AS "title?", t.slug AS "slug?",
                  t.space_id IS NULL AS "conversation?",
                  (t.deleted_at IS NULL
                   AND (thread_readable(t, $2, $3, $4, $5) OR dm_readable(t.id, t.visibility, $4)))
                      AS "thread_readable?",
                  tc.name AS "thread_space?",
                  space_readable(tc.visibility, tc.id, $2, $3) AS "thread_space_readable?",
                  (SELECT count(*) FROM private_thread_participants p
                   WHERE p.thread_id = t.id AND p.left_at IS NULL) AS "people!",
                  c.slug::text AS "space_slug?", c.name AS "space_name?",
                  space_readable(c.visibility, c.id, $2, $3) AS "space_readable?",
                  (SELECT count(*) FROM staff_access_views v WHERE v.access_id = a.id) AS "views!"
           FROM staff_access a
           JOIN users u ON u.id = a.actor_id
           LEFT JOIN threads t ON t.id = a.thread_id
           LEFT JOIN spaces tc ON tc.id = t.space_id
           LEFT JOIN spaces c ON c.id = a.space_id
           WHERE $1 OR EXISTS (SELECT 1 FROM notifications n
                               WHERE n.staff_access_id = a.id AND n.user_id = $7)
           ORDER BY a.id DESC
           LIMIT $6 OFFSET $8"#,
        everything,
        &filter.visibilities,
        &filter.restricted_space_ids,
        filter.dm_viewer,
        &filter.group_ids,
        LOG_PER_PAGE,
        viewer.user_id(),
        offset,
    )
    .fetch_all(db.pool())
    .await?;
    let entries = rows
        .into_iter()
        .map(|row| {
            let thread_readable = row.thread_readable == Some(true);
            let (what, object, href) = match (row.thread_id, row.space_slug) {
                (Some(id), _) if row.conversation == Some(true) => {
                    if thread_readable {
                        let title = row.title.unwrap_or_default();
                        (
                            What::Conversation,
                            if title.is_empty() {
                                "the conversation (untitled)".to_owned()
                            } else {
                                format!("the conversation “{title}”")
                            },
                            Some(format!("/messages/{id}")),
                        )
                    } else {
                        (
                            What::Conversation,
                            format!(
                                "the conversation between {}",
                                crate::format::plural(row.people, "person", "people")
                            ),
                            None,
                        )
                    }
                }
                (Some(id), _) if thread_readable => (
                    What::Thread,
                    format!("the thread “{}”", row.title.unwrap_or_default()),
                    Some(crate::format::thread_href(
                        &id,
                        &row.slug.unwrap_or_default(),
                    )),
                ),
                (Some(id), _) => (
                    What::Thread,
                    match row
                        .thread_space
                        .filter(|_| staff || row.thread_space_readable == Some(true))
                    {
                        Some(space) => format!("the thread № {id} in {space}"),
                        None => format!("the thread № {id}"),
                    },
                    staff.then(|| format!("/mod/t/{id}")),
                ),
                (None, Some(slug)) if row.space_readable == Some(true) => (
                    What::Space,
                    format!("the space {}", row.space_name.unwrap_or_default()),
                    Some(format!("/c/{slug}")),
                ),
                (None, Some(slug)) if staff => (
                    What::Space,
                    format!("the space {}", row.space_name.unwrap_or_default()),
                    Some(format!("/c/{slug}/settings")),
                ),
                (None, Some(_)) => (What::Space, "a space you could read".to_owned(), None),
                (None, None) => (What::Space, String::new(), None),
            };
            Logged {
                id: row.id,
                moderator: UserRef {
                    id: row.actor_id,
                    username: row.actor_username,
                    display_name: row.actor_name,
                },
                what,
                object,
                href,
                reason: row.reason,
                after_report: row.after_report,
                created_at: row.created_at,
                expires_at: row.expires_at,
                views: row.views,
                by_change: row.by_change,
            }
        })
        .collect();
    Ok(LogPage { total, entries })
}

// ---------------------------------------------------------------------------
// What the confirmation page may say

/// Something a member of staff can't read, as the confirmation page for
/// breaking glass on it names it (DESIGN.md §8): what it is and where,
/// never its title or what it says.
#[derive(Clone, Debug)]
pub struct Closed {
    pub scope: Scope,
    pub what: What,
    /// "thread № 12 in Circle, started by Kit on 3 Oct 2026", "a
    /// conversation between 2 people", "the whole space Circle".
    pub label: String,
    /// The space a thread is in, which a grant on the space would cover.
    pub space_id: Option<i64>,
    /// Where reading it starts.
    pub href: String,
}

/// Where the viewer reads something they can already read, or `None` if
/// it doesn't exist (or is deleted) or they can't.
pub async fn open_href(db: &Db, viewer: &Viewer, scope: Scope) -> sqlx::Result<Option<String>> {
    let filter = viewer.filter();
    Ok(match scope {
        Scope::Thread(id) => sqlx::query!(
            r#"SELECT t.id, t.space_id IS NULL AS "conversation!"
               FROM threads t
               WHERE t.id = $1 AND t.deleted_at IS NULL
                 AND (thread_readable(t, $2, $3, $4, $5) OR dm_readable(t.id, t.visibility, $4))"#,
            id,
            &filter.visibilities,
            &filter.restricted_space_ids,
            filter.dm_viewer,
            &filter.group_ids,
        )
        .fetch_optional(db.pool())
        .await?
        .map(|row| {
            if row.conversation {
                format!("/messages/{}", row.id)
            } else {
                format!("/t/{}", row.id)
            }
        }),
        Scope::Space(id) => sqlx::query_scalar!(
            r#"SELECT c.slug::text AS "slug!" FROM spaces c
               WHERE c.id = $1 AND space_readable(c.visibility, c.id, $2, $3)"#,
            id,
            &filter.visibilities,
            &filter.restricted_space_ids,
        )
        .fetch_optional(db.pool())
        .await?
        .map(|slug| format!("/c/{slug}")),
    })
}

/// A live thread, conversation or space that the viewer, as staff, can't
/// read; `None` for anyone else, and for anything else.
pub async fn closed(db: &Db, viewer: &Viewer, scope: Scope) -> sqlx::Result<Option<Closed>> {
    if !viewer.is_staff() {
        return Ok(None);
    }
    let filter = viewer.filter();
    match scope {
        Scope::Thread(id) => {
            let Some(row) = sqlx::query!(
                r#"SELECT t.id, t.space_id, t.created_at, c.name AS "space_name?",
                          a.display_name AS author_name,
                          (SELECT count(*) FROM private_thread_participants p
                           WHERE p.thread_id = t.id AND p.left_at IS NULL) AS "people!"
                   FROM threads t
                   JOIN users a ON a.id = t.author_id
                   LEFT JOIN spaces c ON c.id = t.space_id
                   WHERE t.id = $1 AND t.deleted_at IS NULL
                     AND NOT (thread_readable(t, $2, $3, $4, $5)
                              OR dm_readable(t.id, t.visibility, $4))"#,
                id,
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
            Ok(Some(match (row.space_id, row.space_name) {
                (Some(space_id), Some(space)) => Closed {
                    scope,
                    what: What::Thread,
                    label: format!(
                        "thread № {id} in {space}, started by {} on {}",
                        row.author_name,
                        crate::format::date(&row.created_at)
                    ),
                    space_id: Some(space_id),
                    href: format!("/t/{id}"),
                },
                _ => Closed {
                    scope,
                    what: What::Conversation,
                    label: format!(
                        "a conversation between {}",
                        crate::format::plural(row.people, "person", "people")
                    ),
                    space_id: None,
                    href: format!("/messages/{id}"),
                },
            }))
        }
        Scope::Space(id) => Ok(sqlx::query!(
            r#"SELECT c.slug::text AS "slug!", c.name FROM spaces c
               WHERE c.id = $1 AND NOT space_readable(c.visibility, c.id, $2, $3)"#,
            id,
            &filter.visibilities,
            &filter.restricted_space_ids,
        )
        .fetch_optional(db.pool())
        .await?
        .map(|row| Closed {
            scope,
            what: What::Space,
            label: format!("the whole space {}", row.name),
            space_id: Some(id),
            href: format!("/c/{}", row.slug),
        })),
    }
}

/// The thread (or conversation) a report on a post is about, and the post,
/// for staff: where opening the report would break glass.
pub async fn reported(
    db: &Db,
    viewer: &Viewer,
    report_id: i64,
) -> sqlx::Result<Option<(i64, i64)>> {
    if !viewer.is_staff() {
        return Ok(None);
    }
    Ok(sqlx::query!(
        r#"SELECT p.thread_id, p.id FROM reports r JOIN posts p ON p.id = r.post_id
           WHERE r.id = $1 AND r.kind = 'post'"#,
        report_id
    )
    .fetch_optional(db.pool())
    .await?
    .map(|row| (row.thread_id, row.id)))
}

/// What joining a group would let the viewer read that they can't now:
/// each space whose read grant names it, and each thread whose own
/// restriction names it in a space they would read (and, with the space,
/// read already). Joining one as staff is breaking glass on each of them
/// (DESIGN.md §8).
pub async fn opened_by_joining(
    db: &Db,
    viewer: &Viewer,
    group_id: i64,
) -> sqlx::Result<Vec<Closed>> {
    if !viewer.is_staff() {
        return Ok(Vec::new());
    }
    let filter = viewer.filter();
    let mut groups = filter.group_ids.clone();
    if !groups.contains(&group_id) {
        groups.push(group_id);
    }
    let spaces = sqlx::query!(
        r#"SELECT c.id FROM spaces c JOIN space_grants g ON g.space_id = c.id AND g.verb = 'read'
           WHERE g.level = 'listed' AND $1 = ANY (g.group_ids)
             AND NOT space_readable(c.visibility, c.id, $2, $3)
           ORDER BY c.id"#,
        group_id,
        &filter.visibilities,
        &filter.restricted_space_ids,
    )
    .fetch_all(db.pool())
    .await?;
    let mut restricted = filter.restricted_space_ids.clone();
    restricted.extend(spaces.iter().map(|row| row.id));
    let threads = sqlx::query_scalar!(
        r#"SELECT t.id FROM threads t
           WHERE t.deleted_at IS NULL AND t.space_id IS NOT NULL
             AND NOT (t.space_id = ANY ($6))
             AND NOT thread_readable(t, $1, $2, $3, $4)
             AND thread_readable(t, $1, $5, $3, $7)
           ORDER BY t.id"#,
        &filter.visibilities,
        &filter.restricted_space_ids,
        filter.dm_viewer,
        &filter.group_ids,
        &restricted,
        &spaces.iter().map(|row| row.id).collect::<Vec<i64>>(),
        &groups,
    )
    .fetch_all(db.pool())
    .await?;
    let mut opened = Vec::new();
    for row in spaces {
        opened.extend(closed(db, viewer, Scope::Space(row.id)).await?);
    }
    for id in threads {
        opened.extend(closed(db, viewer, Scope::Thread(id)).await?);
    }
    Ok(opened)
}
