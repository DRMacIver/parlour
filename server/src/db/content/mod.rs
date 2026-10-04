//! Every read of forum content goes through this module (DESIGN.md §6).
//! Each function takes the [`Viewer`] and filters with the shared
//! `thread_readable` / `space_readable` SQL functions, so private content
//! never reaches a template.
//!
//! Each function also records in a [`Deps`] what the result was built from,
//! which the page cache uses to know when to throw the page away. Ids are
//! recorded even for things the viewer can't see (such as a fork in a
//! private space), because a later change could make them visible.

pub mod arrangement;
pub mod messages;
pub mod moderation;
pub mod notifications;
pub mod search;
pub mod uploads;

use std::collections::{BTreeSet, HashMap};

use crate::db::Db;
use chrono::{DateTime, Utc};

use crate::cache::Tag;
use crate::permissions::{Audience, Grant, SpaceRules};
use crate::store::SpaceSettings;
use crate::viewer::Viewer;

pub use arrangement::{ArrangedSection, ArrangedSpace, Arrangement, arrangement};

pub const POSTS_PER_PAGE: i64 = 50;
pub const THREADS_PER_PAGE: i64 = 30;
pub const LATEST_ON_INDEX: i64 = 8;

/// What a page was built from.
#[derive(Debug, Default)]
pub struct Deps {
    tags: BTreeSet<Tag>,
    expires_at: Option<DateTime<Utc>>,
}

impl Deps {
    pub fn new() -> Deps {
        let mut deps = Deps::default();
        deps.add(Tag::Site);
        deps
    }

    pub fn add(&mut self, tag: Tag) {
        self.tags.insert(tag);
    }

    pub fn tags(&self) -> Vec<Tag> {
        self.tags.iter().copied().collect()
    }

    /// Says that the page stops being right at `at` by itself, with no
    /// change to clear it.
    pub fn expires_at(&mut self, at: DateTime<Utc>) {
        self.expires_at = Some(self.expires_at.map_or(at, |earlier| earlier.min(at)));
    }

    pub fn expiry(&self) -> Option<DateTime<Utc>> {
        self.expires_at
    }
}

/// The number of pages needed for `items`; always at least one.
pub fn page_count(items: i64, per_page: i64) -> i64 {
    ((items + per_page - 1) / per_page).max(1)
}

/// The offset of the first item on `page`, or `None` if the page is so far
/// past the end that the offset doesn't fit in an `i64`. Such a page is
/// necessarily empty.
pub fn page_offset(page: i64, per_page: i64) -> Option<i64> {
    page.checked_sub(1)?.checked_mul(per_page)
}

/// The page of a thread that post number `seq` is on.
pub fn page_of_seq(seq: i64) -> i64 {
    page_count(seq, POSTS_PER_PAGE)
}

#[derive(Clone, Debug)]
pub struct UserRef {
    pub id: i64,
    pub username: String,
    pub display_name: String,
}

#[derive(Clone, Debug)]
pub struct ThreadRef {
    pub id: i64,
    pub title: String,
    pub slug: String,
}

#[derive(Clone, Debug)]
pub struct SpaceRef {
    pub id: i64,
    pub slug: String,
    pub name: String,
}

// ---------------------------------------------------------------------------
// The front page

/// The front page as one viewer sees it (DESIGN.md §3.5): the essential
/// spaces, then the sections of their layout (the default layout for
/// visitors), each with the spaces in it they can read. Archived spaces,
/// spaces they can't read, spaces they have muted and empty sections are
/// left out.
#[derive(Debug)]
pub struct FrontPage {
    /// Essential spaces, in their order on the layout, then by name.
    pub essential: Vec<SpaceSummary>,
    pub sections: Vec<IndexSection>,
    /// Whether the viewer can read any live space at all, on the front
    /// page or not.
    pub any_spaces: bool,
}

impl FrontPage {
    pub fn is_empty(&self) -> bool {
        self.essential.is_empty() && self.sections.is_empty()
    }
}

#[derive(Debug)]
pub struct IndexSection {
    pub id: i64,
    pub name: String,
    pub spaces: Vec<SpaceSummary>,
}

#[derive(Debug)]
pub struct SpaceSummary {
    pub space: SpaceRef,
    pub description: String,
    /// The level of the read grant, for the label beside the name.
    pub read_level: String,
    /// For a listed space, the groups that may read it.
    pub group_names: Vec<String>,
    pub essential: bool,
    pub thread_count: i64,
    pub post_count: i64,
    /// For a logged-in viewer, how many of its threads have posts they
    /// haven't read (DESIGN.md §3.5).
    pub unread: Option<i64>,
    pub last: Option<LastActivity>,
}

#[derive(Debug)]
pub struct LastActivity {
    pub thread: ThreadRef,
    /// When the newest visible post was written.
    pub at: DateTime<Utc>,
    /// The author of the newest post that is still visible, if any.
    pub by: Option<UserRef>,
    pub post_id: Option<i64>,
}

/// The front page for the viewer, from their own layout if they have one
/// and the default otherwise.
pub async fn front_page(db: &Db, viewer: &Viewer, deps: &mut Deps) -> sqlx::Result<FrontPage> {
    let filter = viewer.filter();
    let user_id = viewer.user_id();
    let rows = sqlx::query!(
        r#"
        WITH chosen AS (
            SELECT id FROM layouts
            WHERE user_id IS NOT DISTINCT FROM $3::bigint
               OR (user_id IS NULL AND NOT EXISTS (SELECT 1 FROM layouts WHERE user_id = $3))
            ORDER BY user_id NULLS LAST
            LIMIT 1
        ), placed AS (
            SELECT x.space_id, s.id AS section_id, s.name AS section_name,
                   s.position AS section_position, x.position
            FROM chosen l
            JOIN layout_sections s ON s.layout_id = l.id
            JOIN layout_section_spaces x ON x.section_id = s.id
        )
        SELECT c.id, c.slug, c.name, c.description, c.essential, r.level AS read_level,
               p.section_id AS "section_id?", p.section_name AS "section_name?",
               array(SELECT g.name FROM groups g
                     WHERE r.level = 'listed' AND g.id = ANY (r.group_ids)
                     ORDER BY lower(g.name), g.id) AS "group_names!",
               counts.threads AS "thread_count!", counts.posts AS "post_count!", counts.unread AS "unread!",
               lt.id AS "last_thread_id?", lt.title AS "last_thread_title?", lt.slug AS "last_thread_slug?",
               coalesce(lp.created_at, lt.last_post_at) AS "last_at?", lp.id AS "last_post_id?",
               lu.id AS "last_user_id?", lu.username AS "last_username?", lu.display_name AS "last_display_name?"
        FROM spaces c
        JOIN space_grants r ON r.space_id = c.id AND r.verb = 'read'
        LEFT JOIN placed p ON p.space_id = c.id
        CROSS JOIN LATERAL (
            SELECT count(*) AS threads, coalesce(sum(t.last_seq), 0)::bigint AS posts,
                   count(*) FILTER (WHERE t.last_seq > coalesce(tr.last_read_seq, 0)) AS unread
            FROM threads t
            LEFT JOIN thread_reads tr ON tr.thread_id = t.id AND tr.user_id = $3
            WHERE t.space_id = c.id AND t.deleted_at IS NULL
              AND thread_readable(t.visibility, t.space_id, $1, $2)
        ) counts
        LEFT JOIN LATERAL (
            SELECT t.id, t.title, t.slug, t.last_post_at
            FROM threads t
            WHERE t.space_id = c.id AND t.deleted_at IS NULL
              AND thread_readable(t.visibility, t.space_id, $1, $2)
            ORDER BY t.last_post_at DESC, t.id DESC
            LIMIT 1
        ) lt ON true
        LEFT JOIN LATERAL (
            SELECT p.id, p.author_id, p.created_at FROM posts p
            WHERE p.thread_id = lt.id AND p.deleted_at IS NULL AND p.hidden_at IS NULL
            ORDER BY p.seq DESC
            LIMIT 1
        ) lp ON true
        LEFT JOIN users lu ON lu.id = lp.author_id
        WHERE (c.essential OR p.space_id IS NOT NULL)
          AND c.archived_at IS NULL
          AND space_readable(c.visibility, c.id, $1, $2)
          AND ($3::bigint IS NULL
               OR NOT EXISTS (SELECT 1 FROM space_mutes m WHERE m.user_id = $3 AND m.space_id = c.id))
        ORDER BY c.essential DESC, p.section_position NULLS LAST, p.section_id NULLS LAST,
                 p.position, lower(c.name), c.id
        "#,
        &filter.visibilities,
        &filter.restricted_space_ids,
        user_id,
    )
    .fetch_all(db.pool())
    .await?;
    // Only an empty front page needs to know whether that is because
    // there are no spaces at all.
    let any_spaces = !rows.is_empty()
        || sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM spaces c
                              WHERE c.archived_at IS NULL AND space_readable(c.visibility, c.id, $1, $2)) AS "e!""#,
            &filter.visibilities,
            &filter.restricted_space_ids,
        )
        .fetch_one(db.pool())
        .await?;

    let mut page = FrontPage {
        essential: Vec::new(),
        sections: Vec::new(),
        any_spaces,
    };
    for row in rows {
        deps.add(Tag::Space(row.id));
        let summary = SpaceSummary {
            space: SpaceRef {
                id: row.id,
                slug: row.slug,
                name: row.name,
            },
            description: row.description,
            read_level: row.read_level,
            group_names: row.group_names,
            essential: row.essential,
            thread_count: row.thread_count,
            post_count: row.post_count,
            unread: user_id.map(|_| row.unread),
            last: match (
                row.last_thread_id,
                row.last_thread_title,
                row.last_thread_slug,
                row.last_at,
            ) {
                (Some(id), Some(title), Some(slug), Some(at)) => Some(LastActivity {
                    thread: ThreadRef { id, title, slug },
                    at,
                    by: user_ref(row.last_user_id, row.last_username, row.last_display_name),
                    post_id: row.last_post_id,
                }),
                _ => None,
            },
        };
        if row.essential {
            page.essential.push(summary);
            continue;
        }
        let (Some(section_id), Some(section_name)) = (row.section_id, row.section_name) else {
            continue;
        };
        match page.sections.last_mut() {
            Some(section) if section.id == section_id => section.spaces.push(summary),
            _ => page.sections.push(IndexSection {
                id: section_id,
                name: section_name,
                spaces: vec![summary],
            }),
        }
    }
    Ok(page)
}

fn user_ref(
    id: Option<i64>,
    username: Option<String>,
    display_name: Option<String>,
) -> Option<UserRef> {
    Some(UserRef {
        id: id?,
        username: username?,
        display_name: display_name?,
    })
}

// ---------------------------------------------------------------------------
// Thread lists: the space page, /latest and the index's "Latest" list

#[derive(Debug)]
pub struct ThreadRow {
    pub thread: ThreadRef,
    pub space: SpaceRef,
    pub author: UserRef,
    pub created_at: DateTime<Utc>,
    pub post_count: i64,
    /// When the newest post the viewer can read was written: the post
    /// `last_by` and `last_post_id` describe. Threads are still ordered by
    /// their last activity, which counts removed posts.
    pub last_post_at: DateTime<Utc>,
    pub last_by: Option<UserRef>,
    pub last_post_id: Option<i64>,
    pub pinned: bool,
    pub locked: bool,
    /// The thread this one was quote-forked from, if the viewer can see it.
    pub forked_from: Option<ThreadRef>,
    /// For a logged-in viewer, how many posts they haven't read yet: those
    /// after their read marker, or every post if they've never opened it.
    pub unread: Option<i64>,
}

pub struct SpacePage {
    pub space: SpaceRef,
    pub description: String,
    /// The level of the read grant, for the label beside the name.
    pub read_level: String,
    /// For a listed space, the groups that may read it.
    pub group_names: Vec<String>,
    pub archived: bool,
    pub thread_count: i64,
    pub threads: Vec<ThreadRow>,
}

/// One page of a space's threads, or `None` if the space doesn't
/// exist or the viewer can't read it. A page past the end is empty.
pub async fn space_page(
    db: &Db,
    viewer: &Viewer,
    slug: &str,
    page: i64,
    deps: &mut Deps,
) -> sqlx::Result<Option<SpacePage>> {
    let filter = viewer.filter();
    let Some(space) = sqlx::query!(
        r#"
        SELECT c.id, c.slug, c.name, c.description, r.level AS read_level,
               c.archived_at IS NOT NULL AS "archived!",
               array(SELECT g.name FROM groups g
                     WHERE r.level = 'listed' AND g.id = ANY (r.group_ids)
                     ORDER BY lower(g.name), g.id) AS "group_names!",
               (SELECT count(*) FROM threads t
                WHERE t.space_id = c.id AND t.deleted_at IS NULL
                  AND thread_readable(t.visibility, t.space_id, $2, $3)) AS "thread_count!"
        FROM spaces c
        JOIN space_grants r ON r.space_id = c.id AND r.verb = 'read'
        WHERE c.slug = $1 AND space_readable(c.visibility, c.id, $2, $3)
        "#,
        slug,
        &filter.visibilities,
        &filter.restricted_space_ids,
    )
    .fetch_optional(db.pool())
    .await?
    else {
        return Ok(None);
    };
    deps.add(Tag::Space(space.id));
    let threads = thread_rows(db, viewer, &[space.id], true, page, THREADS_PER_PAGE, deps).await?;
    Ok(Some(SpacePage {
        space: SpaceRef {
            id: space.id,
            slug: space.slug,
            name: space.name,
        },
        description: space.description,
        read_level: space.read_level,
        group_names: space.group_names,
        archived: space.archived,
        thread_count: space.thread_count,
        threads,
    }))
}

pub struct LatestPage {
    pub thread_count: i64,
    pub threads: Vec<ThreadRow>,
}

/// Which threads a `/latest` tab lists (DESIGN.md §3.5). Every tab leaves
/// out the spaces the viewer has muted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LatestScope {
    /// Every thread the viewer can read.
    All,
    /// Threads in the spaces on the viewer's front page: the essential
    /// spaces and those in the sections of their layout (the default
    /// layout for a visitor), archived ones left out as the front page
    /// leaves them out.
    Front,
    /// Threads in the spaces of one section of the viewer's layout. A
    /// section of anyone else's layout holds nothing for them.
    Section(i64),
}

impl LatestScope {
    fn as_str(self) -> &'static str {
        match self {
            LatestScope::All => "all",
            LatestScope::Front => "front",
            LatestScope::Section(_) => "section",
        }
    }

    fn section_id(self) -> Option<i64> {
        match self {
            LatestScope::Section(id) => Some(id),
            _ => None,
        }
    }
}

/// A section of the viewer's layout, as a tab of `/latest`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SectionTab {
    pub id: i64,
    pub name: String,
    /// How many of its spaces the viewer can see: readable, not archived
    /// and not muted. A tab with none has nothing to show.
    pub spaces: i64,
}

/// The viewer's layout as `/latest` shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LayoutSections {
    /// The sections, in order.
    pub sections: Vec<SectionTab>,
    /// Whether the viewer has muted any space, which every tab leaves out.
    pub has_mutes: bool,
}

/// The sections of the viewer's layout (the default layout's for a
/// visitor), in order, in one query with whether they have muted anything.
pub async fn layout_sections(
    db: &Db,
    viewer: &Viewer,
    deps: &mut Deps,
) -> sqlx::Result<LayoutSections> {
    let filter = viewer.filter();
    // The layout is joined on the left so that one without sections still
    // answers the question about mutes.
    let rows = sqlx::query!(
        r#"
        WITH chosen AS (
            SELECT id FROM layouts
            WHERE user_id IS NOT DISTINCT FROM $3::bigint
               OR (user_id IS NULL AND NOT EXISTS (SELECT 1 FROM layouts WHERE user_id = $3))
            ORDER BY user_id NULLS LAST
            LIMIT 1
        )
        SELECT s.id AS "id?", s.name AS "name?",
               (SELECT count(*) FROM layout_section_spaces x JOIN spaces c ON c.id = x.space_id
                WHERE x.section_id = s.id AND c.archived_at IS NULL
                  AND space_readable(c.visibility, c.id, $1, $2)
                  AND ($3::bigint IS NULL
                       OR NOT EXISTS (SELECT 1 FROM space_mutes m WHERE m.user_id = $3 AND m.space_id = c.id))
               ) AS "spaces!",
               EXISTS (SELECT 1 FROM space_mutes m WHERE m.user_id = $3) AS "has_mutes!"
        FROM chosen l
        LEFT JOIN layout_sections s ON s.layout_id = l.id
        ORDER BY s.position, s.id
        "#,
        &filter.visibilities,
        &filter.restricted_space_ids,
        viewer.user_id(),
    )
    .fetch_all(db.pool())
    .await?;
    // The default layout changes `site`; a member's own isn't cached.
    deps.add(Tag::Site);
    let mut out = LayoutSections::default();
    for row in rows {
        out.has_mutes = row.has_mutes;
        if let (Some(id), Some(name)) = (row.id, row.name) {
            out.sections.push(SectionTab {
                id,
                name,
                spaces: row.spaces,
            });
        }
    }
    Ok(out)
}

/// The spaces a scope covers that the viewer can read, each with how many
/// of its threads they can read.
async fn scoped_spaces(
    db: &Db,
    viewer: &Viewer,
    scope: LatestScope,
) -> sqlx::Result<Vec<(i64, i64)>> {
    let filter = viewer.filter();
    let rows = sqlx::query!(
        r#"
        WITH chosen AS (
            SELECT id FROM layouts
            WHERE user_id IS NOT DISTINCT FROM $3::bigint
               OR (user_id IS NULL AND NOT EXISTS (SELECT 1 FROM layouts WHERE user_id = $3))
            ORDER BY user_id NULLS LAST
            LIMIT 1
        ), placed AS (
            SELECT x.space_id, s.id AS section_id
            FROM chosen l
            JOIN layout_sections s ON s.layout_id = l.id
            JOIN layout_section_spaces x ON x.section_id = s.id
        )
        SELECT c.id,
               (SELECT count(*) FROM threads t
                WHERE t.space_id = c.id AND t.deleted_at IS NULL
                  AND thread_readable(t.visibility, t.space_id, $1, $2)) AS "thread_count!"
        FROM spaces c
        WHERE space_readable(c.visibility, c.id, $1, $2)
          AND ($3::bigint IS NULL
               OR NOT EXISTS (SELECT 1 FROM space_mutes m WHERE m.user_id = $3 AND m.space_id = c.id))
          AND CASE $4::text
                WHEN 'all' THEN true
                WHEN 'front' THEN c.archived_at IS NULL
                     AND (c.essential OR EXISTS (SELECT 1 FROM placed p WHERE p.space_id = c.id))
                ELSE c.archived_at IS NULL
                     AND EXISTS (SELECT 1 FROM placed p WHERE p.space_id = c.id AND p.section_id = $5)
              END
        ORDER BY c.id
        "#,
        &filter.visibilities,
        &filter.restricted_space_ids,
        viewer.user_id(),
        scope.as_str(),
        scope.section_id(),
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.id, row.thread_count))
        .collect())
}

/// The threads in a scope that the viewer can see, most recently active
/// first.
pub async fn latest(
    db: &Db,
    viewer: &Viewer,
    scope: LatestScope,
    page: i64,
    per_page: i64,
    deps: &mut Deps,
) -> sqlx::Result<LatestPage> {
    // Any space in the scope can gain the newest thread, so the list
    // depends on all of them.
    let spaces = scoped_spaces(db, viewer, scope).await?;
    let mut thread_count = 0;
    let mut ids = Vec::with_capacity(spaces.len());
    for (id, threads) in spaces {
        deps.add(Tag::Space(id));
        thread_count += threads;
        ids.push(id);
    }
    let threads = thread_rows(db, viewer, &ids, false, page, per_page, deps).await?;
    Ok(LatestPage {
        thread_count,
        threads,
    })
}

/// The ids of the threads a scope covers that the viewer can see, for
/// marking them all read.
pub async fn scope_thread_ids(
    db: &Db,
    viewer: &Viewer,
    scope: LatestScope,
) -> sqlx::Result<Vec<i64>> {
    let spaces: Vec<i64> = scoped_spaces(db, viewer, scope)
        .await?
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    visible_thread_ids(db, viewer, &spaces).await
}

/// One page of the threads in `spaces` that the viewer can read, most
/// recently active first, with pinned threads before the rest if
/// `pins_first` (the space page does; the `/latest` tabs don't).
async fn thread_rows(
    db: &Db,
    viewer: &Viewer,
    spaces: &[i64],
    pins_first: bool,
    page: i64,
    per_page: i64,
    deps: &mut Deps,
) -> sqlx::Result<Vec<ThreadRow>> {
    let Some(offset) = page_offset(page, per_page) else {
        return Ok(Vec::new());
    };
    if spaces.is_empty() {
        return Ok(Vec::new());
    }
    let filter = viewer.filter();
    let rows = sqlx::query!(
        r#"
        SELECT t.id, t.title, t.slug, t.created_at, t.last_seq,
               coalesce(lp.created_at, t.last_post_at) AS "last_post_at!", t.pinned, t.locked,
               c.id AS space_id, c.slug AS space_slug, c.name AS space_name,
               a.id AS author_id, a.username AS author_username, a.display_name AS author_display_name,
               lp.id AS "last_post_id?", lu.id AS "last_user_id?", lu.username AS "last_username?",
               lu.display_name AS "last_display_name?",
               src.id AS "source_thread_id?", src.title AS "source_title?", src.slug AS "source_slug?",
               src.visible AS "source_visible?",
               CASE WHEN $6::bigint IS NULL THEN NULL
                    ELSE t.last_seq - coalesce(tr.last_read_seq, 0) END AS unread
        FROM threads t
        LEFT JOIN thread_reads tr ON tr.thread_id = t.id AND tr.user_id = $6
        JOIN spaces c ON c.id = t.space_id
        JOIN users a ON a.id = t.author_id
        LEFT JOIN LATERAL (
            SELECT p.id, p.author_id, p.created_at FROM posts p
            WHERE p.thread_id = t.id AND p.deleted_at IS NULL AND p.hidden_at IS NULL
            ORDER BY p.seq DESC
            LIMIT 1
        ) lp ON true
        LEFT JOIN users lu ON lu.id = lp.author_id
        LEFT JOIN LATERAL (
            SELECT st.id, st.title, st.slug,
                   st.deleted_at IS NULL AND thread_readable(st.visibility, st.space_id, $1, $2) AS visible
            FROM posts sp JOIN threads st ON st.id = sp.thread_id
            WHERE sp.id = t.forked_from_post_id
        ) src ON true
        WHERE t.deleted_at IS NULL
          AND thread_readable(t.visibility, t.space_id, $1, $2)
          AND t.space_id = ANY($3)
        ORDER BY CASE WHEN $7 THEN t.pinned ELSE false END DESC,
                 t.last_post_at DESC, t.id DESC
        LIMIT $4 OFFSET $5
        "#,
        &filter.visibilities,
        &filter.restricted_space_ids,
        spaces,
        per_page,
        offset,
        viewer.user_id(),
        pins_first,
    )
    .fetch_all(db.pool())
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            deps.add(Tag::Thread(row.id));
            deps.add(Tag::Space(row.space_id));
            if let Some(source) = row.source_thread_id {
                deps.add(Tag::Thread(source));
            }
            let forked_from = match (
                row.source_thread_id,
                row.source_title,
                row.source_slug,
                row.source_visible,
            ) {
                (Some(id), Some(title), Some(slug), Some(true)) => {
                    Some(ThreadRef { id, title, slug })
                }
                _ => None,
            };
            ThreadRow {
                thread: ThreadRef {
                    id: row.id,
                    title: row.title,
                    slug: row.slug,
                },
                space: SpaceRef {
                    id: row.space_id,
                    slug: row.space_slug,
                    name: row.space_name,
                },
                author: UserRef {
                    id: row.author_id,
                    username: row.author_username.to_string(),
                    display_name: row.author_display_name,
                },
                created_at: row.created_at,
                post_count: i64::from(row.last_seq),
                last_post_at: row.last_post_at,
                last_by: user_ref(row.last_user_id, row.last_username, row.last_display_name),
                last_post_id: row.last_post_id,
                pinned: row.pinned,
                locked: row.locked,
                forked_from,
                unread: row.unread.map(i64::from),
            }
        })
        .collect())
}

// ---------------------------------------------------------------------------
// The thread page

pub struct ThreadPage {
    pub thread: ThreadRef,
    /// The thread's space, with what decides who may post in it.
    pub space: SpaceTarget,
    pub author: UserRef,
    pub created_at: DateTime<Utc>,
    /// The number of the last post; posts are numbered from 1 and deleted
    /// posts keep their numbers, so this is also the number of posts.
    pub last_seq: i64,
    pub pinned: bool,
    pub locked: bool,
    /// For a quote-fork, the post it quotes.
    pub embed: Option<Embed>,
    pub posts: Vec<PostView>,
    /// For a logged-in viewer, how far they had read before this page: the
    /// number of the last post they'd seen, or `None` if they had never
    /// opened the thread.
    pub read_marker: Option<i64>,
    /// Whether the viewer (if logged in) is subscribed to the thread.
    pub subscribed: bool,
}

impl ThreadPage {
    /// The thread as somewhere to reply, from what the page already loaded.
    pub fn target(&self) -> ThreadTarget {
        ThreadTarget {
            thread: self.thread.clone(),
            name: self.thread.title.clone(),
            place: Place::Space(Box::new(self.space.clone())),
            locked: self.locked,
            last_seq: self.last_seq,
        }
    }
}

pub enum Embed {
    Available(Box<EmbeddedPost>),
    /// The source post was deleted, hidden, or is somewhere the viewer
    /// can't see.
    Unavailable,
}

pub struct EmbeddedPost {
    pub post_id: i64,
    pub seq: i64,
    pub thread: ThreadRef,
    pub space: SpaceRef,
    pub author: UserRef,
    pub author_title: Option<String>,
    pub created_at: DateTime<Utc>,
    pub edited_at: Option<DateTime<Utc>>,
    pub body_html: String,
}

pub struct PostView {
    pub id: i64,
    pub seq: i64,
    pub created_at: DateTime<Utc>,
    pub body: PostBody,
    pub reply_to: Option<ReplyTo>,
    /// Quote-forks of this post that the viewer can see.
    pub forks: Vec<ForkLink>,
    /// Reactions to a live post, in the order the admin set.
    pub reactions: Vec<ReactionCount>,
    /// Whether the viewer blocked its author, so it is folded away.
    pub from_blocked: bool,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ReactionCount {
    pub emoji: String,
    pub count: i64,
    /// Whether the viewer is one of those who reacted.
    pub mine: bool,
    /// The display names of those who reacted, first to react first.
    pub names: Vec<String>,
}

pub enum PostBody {
    Live {
        author: Box<AuthorInfo>,
        body_html: String,
        edited_at: Option<DateTime<Utc>>,
        /// A moderator hid it: only staff are shown it, marked as hidden.
        hidden: bool,
    },
    /// Deleted by its author, or by a moderator.
    Deleted { by_moderator: bool },
    /// Hidden by a moderator, as everyone but staff sees it.
    Hidden,
    /// Moved to another thread, which is given if the viewer can read it.
    /// The post keeps its id, so its permalink leads there.
    Moved { to: Option<ThreadRef> },
}

pub struct ReplyTo {
    pub post_id: i64,
    pub seq: i64,
    /// `None` when the post replied to has been deleted or hidden.
    pub author: Option<UserRef>,
}

pub struct ForkLink {
    pub thread: ThreadRef,
    pub post_count: i64,
    pub author: UserRef,
}

/// The post-info column shown to the left of each post.
#[derive(Clone)]
pub struct AuthorInfo {
    pub user: UserRef,
    pub title: Option<String>,
    pub pronouns: Option<String>,
    pub status_line: Option<String>,
    pub location: Option<String>,
    /// The address of their picture, if they chose one.
    pub avatar: Option<String>,
    pub joined_at: DateTime<Utc>,
    /// Posts by this user that the viewer can see.
    pub post_count: i64,
}

/// One page of a thread, or `None` if it doesn't exist or the viewer can't
/// see it. A page past the end has no posts.
pub async fn thread_page(
    db: &Db,
    viewer: &Viewer,
    thread_id: i64,
    page: i64,
    deps: &mut Deps,
) -> sqlx::Result<Option<ThreadPage>> {
    let filter = viewer.filter();
    let Some(thread) = sqlx::query!(
        r#"
        SELECT t.id, t.title, t.slug, t.created_at, t.last_seq, t.pinned, t.locked, t.forked_from_post_id,
               c.id AS "space_id!", c.slug AS "space_slug!", c.name AS "space_name!",
               c.description AS "space_description!", c.essential AS "essential!",
               c.visibility AS "visibility!", c.created_by AS "created_by?",
               c.created_at AS "space_created_at!", c.archived_at AS "archived_at?",
               c.read_level AS "read_level!", c.read_group_ids AS "read_group_ids!", c.read_user_ids AS "read_user_ids!",
               c.reply_level AS "reply_level!", c.reply_group_ids AS "reply_group_ids!", c.reply_user_ids AS "reply_user_ids!",
               c.start_level AS "start_level!", c.start_group_ids AS "start_group_ids!", c.start_user_ids AS "start_user_ids!",
               c.administer_level AS "administer_level!", c.administer_group_ids AS "administer_group_ids!",
               c.administer_user_ids AS "administer_user_ids!",
               c.read_group_names AS "group_names!",
               (SELECT tr.last_read_seq FROM thread_reads tr
                WHERE tr.thread_id = t.id AND tr.user_id = $4) AS read_marker,
               EXISTS (SELECT 1 FROM thread_subscriptions s
                       WHERE s.user_id = $5 AND s.thread_id = t.id) AS "subscribed!",
               a.id AS author_id, a.username AS author_username, a.display_name AS author_display_name
        FROM threads t
        JOIN space_targets c ON c.id = t.space_id
        JOIN users a ON a.id = t.author_id
        WHERE t.id = $1 AND t.deleted_at IS NULL AND thread_readable(t.visibility, t.space_id, $2, $3)
        "#,
        thread_id,
        &filter.visibilities,
        &filter.restricted_space_ids,
        filter.dm_viewer,
        viewer.user_id(),
    )
    .fetch_optional(db.pool())
    .await?
    else {
        return Ok(None);
    };
    // The space's name appears in the breadcrumbs, but renaming a
    // space clears `site`, and moving the thread clears `thread:N`.
    deps.add(Tag::Thread(thread.id));

    let embed = match thread.forked_from_post_id {
        Some(post_id) => Some(embed(db, viewer, post_id, deps).await?),
        None => None,
    };

    let posts = posts_on_page(db, viewer, thread.id, page, deps).await?;

    Ok(Some(ThreadPage {
        thread: ThreadRef {
            id: thread.id,
            title: thread.title,
            slug: thread.slug,
        },
        space: SpaceTarget {
            space: SpaceRef {
                id: thread.space_id,
                slug: thread.space_slug,
                name: thread.space_name,
            },
            description: thread.space_description,
            essential: thread.essential,
            visibility: thread.visibility,
            group_names: thread.group_names,
            created_by: thread.created_by,
            created_at: thread.space_created_at,
            rules: SpaceRules {
                read: Grant::from_parts(
                    &thread.read_level,
                    &thread.read_group_ids,
                    &thread.read_user_ids,
                ),
                reply: Grant::from_parts(
                    &thread.reply_level,
                    &thread.reply_group_ids,
                    &thread.reply_user_ids,
                ),
                start: Grant::from_parts(
                    &thread.start_level,
                    &thread.start_group_ids,
                    &thread.start_user_ids,
                ),
                administer: Grant::from_parts(
                    &thread.administer_level,
                    &thread.administer_group_ids,
                    &thread.administer_user_ids,
                ),
                archived: thread.archived_at.is_some(),
            },
        },
        author: UserRef {
            id: thread.author_id,
            username: thread.author_username.to_string(),
            display_name: thread.author_display_name,
        },
        created_at: thread.created_at,
        last_seq: i64::from(thread.last_seq),
        pinned: thread.pinned,
        locked: thread.locked,
        embed,
        posts,
        read_marker: thread.read_marker.map(i64::from),
        subscribed: thread.subscribed,
    }))
}

/// The posts on one page of a thread the caller has checked the viewer can
/// read: a space thread or a conversation.
pub(super) async fn posts_on_page(
    db: &Db,
    viewer: &Viewer,
    thread_id: i64,
    page: i64,
    deps: &mut Deps,
) -> sqlx::Result<Vec<PostView>> {
    // Post numbers are 32-bit, so a page whose numbers don't fit is empty.
    let seq_range = page_offset(page, POSTS_PER_PAGE)
        .and_then(|offset| Some((offset.checked_add(1)?, offset.checked_add(POSTS_PER_PAGE)?)))
        .and_then(|(first, last)| Some((i32::try_from(first).ok()?, i32::try_from(last).ok()?)));
    let (first, last) = seq_range.unwrap_or((1, 0));
    // Staff read hidden posts, marked as hidden; nobody reads deleted ones.
    let staff = viewer.is_staff();
    let rows = sqlx::query!(
        r#"
        SELECT p.id, p.seq, p.created_at, p.edited_at,
               (p.deleted_at IS NOT NULL) AS "deleted!", (p.hidden_at IS NOT NULL) AS "hidden!",
               (p.deleted_by IS NOT NULL AND p.deleted_by <> p.author_id) AS "deleted_by_staff!",
               CASE WHEN p.deleted_at IS NULL AND (p.hidden_at IS NULL OR $4) THEN p.body_html END AS body_html,
               CASE WHEN p.deleted_at IS NULL AND (p.hidden_at IS NULL OR $4) THEN p.author_id END AS author_id,
               r.id AS "reply_id?", r.seq AS "reply_seq?",
               ru.id AS "reply_user_id?", ru.username AS "reply_username?", ru.display_name AS "reply_display_name?"
        FROM posts p
        LEFT JOIN posts r ON r.id = p.reply_to_post_id AND r.thread_id = p.thread_id
        LEFT JOIN users ru ON ru.id = r.author_id AND r.deleted_at IS NULL AND (r.hidden_at IS NULL OR $4)
        WHERE p.thread_id = $1 AND p.seq BETWEEN $2 AND $3
        ORDER BY p.seq
        "#,
        thread_id,
        first,
        last,
        staff,
    )
    .fetch_all(db.pool())
    .await?;

    let author_ids: Vec<i64> = {
        let mut ids: Vec<i64> = rows.iter().filter_map(|row| row.author_id).collect();
        ids.sort();
        ids.dedup();
        ids
    };
    let authors = author_infos(db, viewer, &author_ids, deps).await?;
    let blocked = blocked_authors(viewer);
    let post_ids: Vec<i64> = rows.iter().map(|row| row.id).collect();
    let mut forks = forks_of(db, viewer, &post_ids, deps).await?;
    let live_ids: Vec<i64> = rows
        .iter()
        .filter(|row| !row.deleted && !row.hidden)
        .map(|row| row.id)
        .collect();
    let mut reactions = reactions_for(db, viewer, &live_ids).await?;

    let mut posts: Vec<PostView> = rows
        .into_iter()
        .map(|row| {
            let body = if row.deleted {
                PostBody::Deleted {
                    by_moderator: row.deleted_by_staff,
                }
            } else if row.hidden && !staff {
                PostBody::Hidden
            } else {
                let author_id = row.author_id.expect("live posts have authors");
                PostBody::Live {
                    author: Box::new(authors.get(&author_id).cloned().expect("author loaded")),
                    body_html: row.body_html.unwrap_or_default(),
                    edited_at: row.edited_at,
                    hidden: row.hidden,
                }
            };
            let reply_to = match (row.reply_id, row.reply_seq) {
                (Some(post_id), Some(seq)) => Some(ReplyTo {
                    post_id,
                    seq: i64::from(seq),
                    author: user_ref(
                        row.reply_user_id,
                        row.reply_username.map(|u| u.to_string()),
                        row.reply_display_name,
                    ),
                }),
                _ => None,
            };
            PostView {
                id: row.id,
                seq: i64::from(row.seq),
                created_at: row.created_at,
                forks: if matches!(body, PostBody::Live { .. }) {
                    forks.remove(&row.id).unwrap_or_default()
                } else {
                    Vec::new()
                },
                reactions: reactions.remove(&row.id).unwrap_or_default(),
                from_blocked: row.author_id.is_some_and(|id| blocked.contains(&id)),
                body,
                reply_to,
            }
        })
        .collect();

    // The places of posts moved to other threads keep their numbers here.
    let filter = viewer.filter();
    let moves = sqlx::query!(
        r#"
        SELECT m.post_id, m.from_seq, m.moved_at, t.id AS thread_id, t.title, t.slug,
               t.deleted_at IS NULL AND thread_readable(t.visibility, t.space_id, $4, $5) AS "readable!"
        FROM post_moves m
        JOIN posts p ON p.id = m.post_id
        JOIN threads t ON t.id = p.thread_id
        WHERE m.from_thread_id = $1 AND m.from_seq BETWEEN $2 AND $3
        "#,
        thread_id,
        first,
        last,
        &filter.visibilities,
        &filter.restricted_space_ids,
    )
    .fetch_all(db.pool())
    .await?;
    for row in moves {
        // Where it is now may change without this thread changing.
        deps.add(Tag::Thread(row.thread_id));
        posts.push(PostView {
            id: row.post_id,
            seq: i64::from(row.from_seq),
            created_at: row.moved_at,
            body: PostBody::Moved {
                to: row.readable.then_some(ThreadRef {
                    id: row.thread_id,
                    title: row.title,
                    slug: row.slug,
                }),
            },
            reply_to: None,
            forks: Vec::new(),
            reactions: Vec::new(),
            from_blocked: false,
        });
    }
    posts.sort_by_key(|post| post.seq);
    Ok(posts)
}

/// The members whose posts the viewer has asked not to see: those they
/// blocked, unless they are staff now (staff can't be blocked). Loaded
/// with the viewer's session.
pub(super) fn blocked_authors(viewer: &Viewer) -> BTreeSet<i64> {
    viewer
        .user()
        .map(|user| user.blocked_user_ids.iter().copied().collect())
        .unwrap_or_default()
}

async fn author_infos(
    db: &Db,
    viewer: &Viewer,
    ids: &[i64],
    deps: &mut Deps,
) -> sqlx::Result<HashMap<i64, AuthorInfo>> {
    let filter = viewer.filter();
    let rows = sqlx::query!(
        r#"
        SELECT u.id AS "id!", u.username AS "username!", u.display_name AS "display_name!",
               u.title, u.pronouns, u.status_line, u.location, u.created_at AS "created_at!",
               av.sha256 AS "avatar_sha256?", av.ext AS "avatar_ext?",
               (SELECT coalesce(sum(pc.posts), 0)::bigint
                FROM user_space_post_counts pc JOIN spaces c ON c.id = pc.space_id
                WHERE pc.user_id = u.id
                  AND space_readable(c.visibility, c.id, $2, $3)) AS "post_count!"
        FROM users u
        LEFT JOIN uploads av ON av.id = u.avatar_upload_id
        WHERE u.id = ANY($1)
        "#,
        ids,
        &filter.visibilities,
        &filter.restricted_space_ids,
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            deps.add(Tag::User(row.id));
            (
                row.id,
                AuthorInfo {
                    user: UserRef {
                        id: row.id,
                        username: row.username.to_string(),
                        display_name: row.display_name,
                    },
                    title: row.title,
                    pronouns: row.pronouns,
                    status_line: row.status_line,
                    location: row.location,
                    avatar: match (row.avatar_sha256, row.avatar_ext) {
                        (Some(sha256), Some(ext)) => Some(crate::uploads::url(&sha256, &ext)),
                        _ => None,
                    },
                    joined_at: row.created_at,
                    post_count: row.post_count,
                },
            )
        })
        .collect())
}

/// The reactions to each of these posts, which the caller has checked are
/// live posts the viewer can see. Only reactions of enabled types count.
async fn reactions_for(
    db: &Db,
    viewer: &Viewer,
    post_ids: &[i64],
) -> sqlx::Result<HashMap<i64, Vec<ReactionCount>>> {
    if post_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query!(
        r#"
        SELECT r.post_id, r.emoji, count(*) AS "count!",
               coalesce(bool_or(r.user_id = $2), false) AS "mine!",
               array_agg(u.display_name ORDER BY r.created_at, u.id) AS "names!"
        FROM reactions r
        JOIN reaction_types rt ON rt.emoji = r.emoji AND rt.enabled
        JOIN users u ON u.id = r.user_id
        WHERE r.post_id = ANY($1)
        GROUP BY r.post_id, r.emoji, rt.position
        ORDER BY r.post_id, rt.position, r.emoji
        "#,
        post_ids,
        viewer.user_id(),
    )
    .fetch_all(db.pool())
    .await?;
    let mut out: HashMap<i64, Vec<ReactionCount>> = HashMap::new();
    for row in rows {
        out.entry(row.post_id).or_default().push(ReactionCount {
            emoji: row.emoji,
            count: row.count,
            mine: row.mine,
            names: row.names,
        });
    }
    Ok(out)
}

/// Visible quote-forks of each of these posts.
async fn forks_of(
    db: &Db,
    viewer: &Viewer,
    post_ids: &[i64],
    deps: &mut Deps,
) -> sqlx::Result<HashMap<i64, Vec<ForkLink>>> {
    let filter = viewer.filter();
    let rows = sqlx::query!(
        r#"
        SELECT f.id, f.title, f.slug, f.last_seq, f.forked_from_post_id AS "source_post_id!",
               f.deleted_at IS NULL AND thread_readable(f.visibility, f.space_id, $2, $3) AS "visible!",
               a.id AS author_id, a.username AS author_username, a.display_name AS author_display_name
        FROM threads f
        JOIN users a ON a.id = f.author_id
        WHERE f.forked_from_post_id = ANY($1)
        ORDER BY f.created_at, f.id
        "#,
        post_ids,
        &filter.visibilities,
        &filter.restricted_space_ids,
    )
    .fetch_all(db.pool())
    .await?;
    let mut forks: HashMap<i64, Vec<ForkLink>> = HashMap::new();
    for row in rows {
        // Even invisible forks matter: if one becomes visible, the backlink
        // must appear.
        deps.add(Tag::Thread(row.id));
        if row.visible {
            forks.entry(row.source_post_id).or_default().push(ForkLink {
                thread: ThreadRef {
                    id: row.id,
                    title: row.title,
                    slug: row.slug,
                },
                post_count: i64::from(row.last_seq),
                author: UserRef {
                    id: row.author_id,
                    username: row.author_username.to_string(),
                    display_name: row.author_display_name,
                },
            });
        }
    }
    Ok(forks)
}

/// The quoted post at the top of a quote-fork. It is checked against the
/// viewer every time, because embeds are references, not copies.
async fn embed(db: &Db, viewer: &Viewer, post_id: i64, deps: &mut Deps) -> sqlx::Result<Embed> {
    let filter = viewer.filter();
    let Some(row) = sqlx::query!(
        r#"
        SELECT p.id, p.seq, p.created_at, p.edited_at, p.thread_id,
               (p.deleted_at IS NULL AND p.hidden_at IS NULL AND t.deleted_at IS NULL
                AND thread_readable(t.visibility, t.space_id, $2, $3)) AS "available!"
        FROM posts p
        JOIN threads t ON t.id = p.thread_id
        WHERE p.id = $1
        "#,
        post_id,
        &filter.visibilities,
        &filter.restricted_space_ids,
    )
    .fetch_optional(db.pool())
    .await?
    else {
        return Ok(Embed::Unavailable);
    };
    deps.add(Tag::Thread(row.thread_id));
    if !row.available {
        return Ok(Embed::Unavailable);
    }
    // Only now that the post is known to be visible is its content read.
    let post = sqlx::query!(
        r#"
        SELECT p.body_html, t.title, t.slug, c.id AS space_id, c.slug AS space_slug, c.name AS space_name,
               u.id AS author_id, u.username, u.display_name, u.title AS author_title
        FROM posts p
        JOIN threads t ON t.id = p.thread_id
        JOIN spaces c ON c.id = t.space_id
        JOIN users u ON u.id = p.author_id
        WHERE p.id = $1
        "#,
        post_id,
    )
    .fetch_one(db.pool())
    .await?;
    deps.add(Tag::User(post.author_id));
    Ok(Embed::Available(Box::new(EmbeddedPost {
        post_id: row.id,
        seq: i64::from(row.seq),
        thread: ThreadRef {
            id: row.thread_id,
            title: post.title,
            slug: post.slug,
        },
        space: SpaceRef {
            id: post.space_id,
            slug: post.space_slug,
            name: post.space_name,
        },
        author: UserRef {
            id: post.author_id,
            username: post.username.to_string(),
            display_name: post.display_name,
        },
        author_title: post.author_title,
        created_at: row.created_at,
        edited_at: row.edited_at,
        body_html: post.body_html,
    })))
}

// ---------------------------------------------------------------------------
// Profiles

/// How many of a member's posts the viewer can see, from the counts per
/// space that triggers keep (migration 0019), so it costs the same
/// however much they have posted. A post in a thread counts in the
/// thread's space, whose read level the thread's visibility always
/// copies. Pages showing it depend on `user:N`, which new posts and thread
/// moves clear.
pub async fn visible_post_count(db: &Db, viewer: &Viewer, user_id: i64) -> sqlx::Result<i64> {
    let filter = viewer.filter();
    sqlx::query_scalar!(
        r#"SELECT coalesce(sum(pc.posts), 0)::bigint AS "n!"
           FROM user_space_post_counts pc JOIN spaces c ON c.id = pc.space_id
           WHERE pc.user_id = $1 AND space_readable(c.visibility, c.id, $2, $3)"#,
        user_id,
        &filter.visibilities,
        &filter.restricted_space_ids,
    )
    .fetch_one(db.pool())
    .await
}

// ---------------------------------------------------------------------------
// Permalinks

pub struct PostLocation {
    pub thread_id: i64,
    pub thread_slug: String,
    pub seq: i64,
    /// Whether the thread is a conversation (a DM), at `/messages/{id}`.
    pub private: bool,
}

impl PostLocation {
    /// The page of the thread or conversation the post is on.
    pub fn page_href(&self) -> String {
        let base = if self.private {
            format!("/messages/{}", self.thread_id)
        } else {
            crate::format::thread_href(&self.thread_id, &self.thread_slug)
        };
        crate::pagination::page_href(&base, page_of_seq(self.seq))
    }

    /// The post itself, on its page.
    pub fn href(&self, post_id: i64) -> String {
        format!("{}#p{post_id}", self.page_href())
    }
}

/// Where a post is, if the viewer can see its thread or conversation.
/// Deleted and hidden posts still have a location: their tombstone.
pub async fn post_location(
    db: &Db,
    viewer: &Viewer,
    post_id: i64,
) -> sqlx::Result<Option<PostLocation>> {
    let filter = viewer.filter();
    let row = sqlx::query!(
        r#"
        SELECT t.id, t.slug, p.seq, t.visibility = 'private' AS "private!"
        FROM posts p JOIN threads t ON t.id = p.thread_id
        WHERE p.id = $1 AND t.deleted_at IS NULL
          AND (thread_readable(t.visibility, t.space_id, $2, $3) OR dm_readable(t.id, t.visibility, $4))
        "#,
        post_id,
        &filter.visibilities,
        &filter.restricted_space_ids,
        filter.dm_viewer,
    )
    .fetch_optional(db.pool())
    .await?;
    Ok(row.map(|row| PostLocation {
        thread_id: row.id,
        thread_slug: row.slug,
        seq: i64::from(row.seq),
        private: row.private,
    }))
}

/// The thread's canonical slug, if the viewer can see it.
pub async fn thread_slug(db: &Db, viewer: &Viewer, thread_id: i64) -> sqlx::Result<Option<String>> {
    let filter = viewer.filter();
    sqlx::query_scalar!(
        r#"
        SELECT t.slug FROM threads t
        WHERE t.id = $1 AND t.deleted_at IS NULL AND thread_readable(t.visibility, t.space_id, $2, $3)
        "#,
        thread_id,
        &filter.visibilities,
        &filter.restricted_space_ids,
    )
    .fetch_optional(db.pool())
    .await
}

// ---------------------------------------------------------------------------
// Writing: what a form is about, checked against the viewer
//
// These say where something is and who may see it; whether the viewer may
// write there is for [`crate::permissions`] to decide.

/// A space the viewer can read, as somewhere to post or to administer.
#[derive(Clone, Debug)]
pub struct SpaceTarget {
    pub space: SpaceRef,
    pub description: String,
    /// Shown at the top of every front page (DESIGN.md §3.1).
    pub essential: bool,
    /// `public`, `members` or `restricted`, as `spaces.visibility` has it.
    pub visibility: String,
    /// The names of the groups the read grant lists.
    pub group_names: Vec<String>,
    pub created_by: Option<i64>,
    pub created_at: DateTime<Utc>,
    /// Who may do what here.
    pub rules: SpaceRules,
}

impl SpaceTarget {
    pub fn audience(&self) -> Audience {
        self.rules.read.audience()
    }

    /// The level of the read grant, for the label beside the name.
    pub fn read_level(&self) -> &'static str {
        self.rules.read.level.as_str()
    }

    pub fn archived(&self) -> bool {
        self.rules.archived
    }

    /// The space's settings, as its administrators edit them.
    pub fn settings(&self) -> SpaceSettings {
        SpaceSettings {
            slug: self.space.slug.clone(),
            name: self.space.name.clone(),
            description: self.description.clone(),
            read: self.rules.read.clone(),
            reply: self.rules.reply.clone(),
            start: self.rules.start.clone(),
            administer: self.rules.administer.clone(),
        }
    }
}

/// A space the viewer can read, by id.
pub async fn space_by_id(db: &Db, viewer: &Viewer, id: i64) -> sqlx::Result<Option<SpaceTarget>> {
    Ok(space_targets(db, viewer, Some(id), None).await?.pop())
}

/// How many threads a space the viewer can read has ever had,
/// including deleted ones: only a space with none can be deleted.
pub async fn space_thread_count(db: &Db, viewer: &Viewer, id: i64) -> sqlx::Result<i64> {
    let filter = viewer.filter();
    sqlx::query_scalar!(
        r#"SELECT count(t.id) AS "n!" FROM spaces c LEFT JOIN threads t ON t.space_id = c.id
           WHERE c.id = $1 AND space_readable(c.visibility, c.id, $2, $3)"#,
        id,
        &filter.visibilities,
        &filter.restricted_space_ids,
    )
    .fetch_one(db.pool())
    .await
}

/// Every space the viewer can read, archived ones included, by name.
pub async fn readable_spaces(db: &Db, viewer: &Viewer) -> sqlx::Result<Vec<SpaceTarget>> {
    space_targets(db, viewer, None, None).await
}

/// The spaces the viewer can read, by name: all of them, or (with `id` or
/// `slug`) just the one so named.
async fn space_targets(
    db: &Db,
    viewer: &Viewer,
    id: Option<i64>,
    slug: Option<&str>,
) -> sqlx::Result<Vec<SpaceTarget>> {
    let filter = viewer.filter();
    let rows = sqlx::query!(
        r#"
        SELECT c.id AS "id!", c.slug AS "slug!", c.name AS "name!", c.description AS "description!",
               c.essential AS "essential!",
               c.visibility AS "visibility!", c.created_by AS "created_by?", c.created_at AS "created_at!",
               c.archived_at AS "archived_at?",
               c.read_level AS "read_level!", c.read_group_ids AS "read_group_ids!", c.read_user_ids AS "read_user_ids!",
               c.reply_level AS "reply_level!", c.reply_group_ids AS "reply_group_ids!", c.reply_user_ids AS "reply_user_ids!",
               c.start_level AS "start_level!", c.start_group_ids AS "start_group_ids!", c.start_user_ids AS "start_user_ids!",
               c.administer_level AS "administer_level!", c.administer_group_ids AS "administer_group_ids!",
               c.administer_user_ids AS "administer_user_ids!",
               c.read_group_names AS "group_names!"
        FROM space_targets c
        WHERE space_readable(c.visibility, c.id, $1, $2)
          AND ($3::bigint IS NULL OR c.id = $3)
          AND ($4::text IS NULL OR c.slug = $4)
        ORDER BY lower(c.name), c.id
        "#,
        &filter.visibilities,
        &filter.restricted_space_ids,
        id,
        slug,
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| SpaceTarget {
            space: SpaceRef {
                id: row.id,
                slug: row.slug,
                name: row.name,
            },
            description: row.description,
            essential: row.essential,
            visibility: row.visibility,
            group_names: row.group_names,
            created_by: row.created_by,
            created_at: row.created_at,
            rules: SpaceRules {
                read: Grant::from_parts(&row.read_level, &row.read_group_ids, &row.read_user_ids),
                reply: Grant::from_parts(
                    &row.reply_level,
                    &row.reply_group_ids,
                    &row.reply_user_ids,
                ),
                start: Grant::from_parts(
                    &row.start_level,
                    &row.start_group_ids,
                    &row.start_user_ids,
                ),
                administer: Grant::from_parts(
                    &row.administer_level,
                    &row.administer_group_ids,
                    &row.administer_user_ids,
                ),
                archived: row.archived_at.is_some(),
            },
        })
        .collect())
}

/// A space the viewer can read, by slug.
pub async fn space_target(
    db: &Db,
    viewer: &Viewer,
    slug: &str,
) -> sqlx::Result<Option<SpaceTarget>> {
    Ok(space_targets(db, viewer, None, Some(slug)).await?.pop())
}

// ---------------------------------------------------------------------------
// The space index

/// How `/spaces` is ordered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpaceSort {
    /// Newest first: the default.
    Created,
    /// Most recently posted in first.
    Activity,
    Name,
}

impl SpaceSort {
    pub fn parse(s: &str) -> Option<SpaceSort> {
        match s {
            "created" => Some(SpaceSort::Created),
            "activity" => Some(SpaceSort::Activity),
            "name" => Some(SpaceSort::Name),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SpaceSort::Created => "created",
            SpaceSort::Activity => "activity",
            SpaceSort::Name => "name",
        }
    }
}

#[derive(Debug)]
pub struct SpaceListing {
    pub space: SpaceRef,
    pub description: String,
    pub read_level: String,
    pub group_names: Vec<String>,
    pub archived: bool,
    /// Whether the viewer has muted it (shown only when asked for).
    pub muted: bool,
    pub created_at: DateTime<Utc>,
    pub thread_count: i64,
    pub last_post_at: Option<DateTime<Utc>>,
}

/// Every space the viewer can read, for the space index: archived ones,
/// and ones the viewer has muted, only when asked for.
pub async fn space_index(
    db: &Db,
    viewer: &Viewer,
    sort: SpaceSort,
    include_archived: bool,
    include_muted: bool,
    deps: &mut Deps,
) -> sqlx::Result<Vec<SpaceListing>> {
    let filter = viewer.filter();
    let rows = sqlx::query!(
        r#"
        SELECT c.id, c.slug, c.name, c.description, r.level AS read_level,
               c.archived_at IS NOT NULL AS "archived!", c.created_at,
               EXISTS (SELECT 1 FROM space_mutes m WHERE m.user_id = $5 AND m.space_id = c.id) AS "muted!",
               array(SELECT g.name FROM groups g
                     WHERE r.level = 'listed' AND g.id = ANY (r.group_ids)
                     ORDER BY lower(g.name), g.id) AS "group_names!",
               counts.threads AS "thread_count!", counts.last_post_at AS "last_post_at?"
        FROM spaces c
        JOIN space_grants r ON r.space_id = c.id AND r.verb = 'read'
        CROSS JOIN LATERAL (
            SELECT count(*) AS threads, max(t.last_post_at) AS last_post_at
            FROM threads t
            WHERE t.space_id = c.id AND t.deleted_at IS NULL
              AND thread_readable(t.visibility, t.space_id, $1, $2)
        ) counts
        WHERE space_readable(c.visibility, c.id, $1, $2)
          AND ($3 OR c.archived_at IS NULL)
          AND ($6 OR $5::bigint IS NULL
               OR NOT EXISTS (SELECT 1 FROM space_mutes m WHERE m.user_id = $5 AND m.space_id = c.id))
        ORDER BY CASE WHEN $4 = 'name' THEN lower(c.name) END,
                 CASE WHEN $4 = 'activity' THEN counts.last_post_at END DESC NULLS LAST,
                 c.created_at DESC, c.id DESC
        "#,
        &filter.visibilities,
        &filter.restricted_space_ids,
        include_archived,
        sort.as_str(),
        viewer.user_id(),
        include_muted,
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            deps.add(Tag::Space(row.id));
            SpaceListing {
                space: SpaceRef {
                    id: row.id,
                    slug: row.slug,
                    name: row.name,
                },
                description: row.description,
                read_level: row.read_level,
                group_names: row.group_names,
                archived: row.archived,
                muted: row.muted,
                created_at: row.created_at,
                thread_count: row.thread_count,
                last_post_at: row.last_post_at,
            }
        })
        .collect())
}

/// A thread or conversation the viewer can read, as somewhere to reply.
#[derive(Clone, Debug)]
pub struct ThreadTarget {
    pub thread: ThreadRef,
    /// What to call it: a conversation without a title is named after the
    /// people in it, other than the viewer.
    pub name: String,
    pub place: Place,
    pub locked: bool,
    pub last_seq: i64,
}

/// Where a thread is: in a space, or a conversation between members.
#[derive(Clone, Debug)]
pub enum Place {
    /// Boxed: a space carries its four grants, a conversation little.
    Space(Box<SpaceTarget>),
    Conversation(messages::ConversationTarget),
}

impl ThreadTarget {
    pub fn space(&self) -> Option<&SpaceTarget> {
        match &self.place {
            Place::Space(space) => Some(space),
            Place::Conversation(_) => None,
        }
    }

    pub fn conversation(&self) -> Option<&messages::ConversationTarget> {
        match &self.place {
            Place::Space(_) => None,
            Place::Conversation(conversation) => Some(conversation),
        }
    }

    /// Who can read it.
    pub fn audience(&self) -> Audience {
        match &self.place {
            Place::Space(space) => space.audience(),
            Place::Conversation(_) => Audience::Private,
        }
    }

    /// Its page: the thread, or the conversation.
    pub fn href(&self) -> String {
        match &self.place {
            Place::Space(_) => crate::format::thread_href(&self.thread.id, &self.thread.slug),
            Place::Conversation(_) => format!("/messages/{}", self.thread.id),
        }
    }
}

pub async fn thread_target(
    db: &Db,
    viewer: &Viewer,
    thread_id: i64,
) -> sqlx::Result<Option<ThreadTarget>> {
    let filter = viewer.filter();
    let Some(row) = sqlx::query!(
        r#"
        SELECT t.id, t.title, t.slug, t.locked, t.last_seq, t.space_id
        FROM threads t
        WHERE t.id = $1 AND t.deleted_at IS NULL
          AND (thread_readable(t.visibility, t.space_id, $2, $3) OR dm_readable(t.id, t.visibility, $4))
        "#,
        thread_id,
        &filter.visibilities,
        &filter.restricted_space_ids,
        filter.dm_viewer,
    )
    .fetch_optional(db.pool())
    .await?
    else {
        return Ok(None);
    };
    let place = match row.space_id {
        Some(space_id) => {
            let Some(space) = space_by_id(db, viewer, space_id).await? else {
                return Ok(None);
            };
            Place::Space(Box::new(space))
        }
        None => Place::Conversation(messages::conversation_target(db, row.id).await?),
    };
    let name = match &place {
        Place::Space(_) => row.title.clone(),
        Place::Conversation(c) => c.name(&row.title, viewer.user_id()),
    };
    Ok(Some(ThreadTarget {
        thread: ThreadRef {
            id: row.id,
            title: row.title,
            slug: row.slug,
        },
        name,
        place,
        locked: row.locked,
        last_seq: i64::from(row.last_seq),
    }))
}

/// A post in a thread the viewer can read.
#[derive(Clone, Debug)]
pub struct PostTarget {
    pub post_id: i64,
    pub seq: i64,
    pub thread: ThreadTarget,
    pub author: UserRef,
    pub created_at: DateTime<Utc>,
    pub edited_at: Option<DateTime<Utc>>,
    /// Whether a moderator hid it.
    pub hidden: bool,
    /// The text, as Markdown and as HTML, unless the post was deleted, or
    /// hidden and the viewer isn't staff.
    pub body: Option<PostText>,
}

#[derive(Clone, Debug)]
pub struct PostText {
    pub markdown: String,
    pub html: String,
}

impl PostTarget {
    /// Whether it is there for everyone who can read its thread: neither
    /// deleted nor hidden.
    pub fn is_live(&self) -> bool {
        self.body.is_some() && !self.hidden
    }
}

pub async fn post_target(
    db: &Db,
    viewer: &Viewer,
    post_id: i64,
) -> sqlx::Result<Option<PostTarget>> {
    let filter = viewer.filter();
    let Some(row) = sqlx::query!(
        r#"
        SELECT p.id, p.seq, p.thread_id, p.created_at, p.edited_at, p.hidden_at IS NOT NULL AS "hidden!",
               CASE WHEN p.deleted_at IS NULL AND (p.hidden_at IS NULL OR $5) THEN p.body_md END AS body_md,
               CASE WHEN p.deleted_at IS NULL AND (p.hidden_at IS NULL OR $5) THEN p.body_html END AS body_html,
               u.id AS author_id, u.username, u.display_name
        FROM posts p
        JOIN threads t ON t.id = p.thread_id
        JOIN users u ON u.id = p.author_id
        WHERE p.id = $1 AND t.deleted_at IS NULL
          AND (thread_readable(t.visibility, t.space_id, $2, $3) OR dm_readable(t.id, t.visibility, $4))
        "#,
        post_id,
        &filter.visibilities,
        &filter.restricted_space_ids,
        filter.dm_viewer,
        viewer.is_staff(),
    )
    .fetch_optional(db.pool())
    .await?
    else {
        return Ok(None);
    };
    let Some(thread) = thread_target(db, viewer, row.thread_id).await? else {
        return Ok(None);
    };
    Ok(Some(PostTarget {
        post_id: row.id,
        seq: i64::from(row.seq),
        thread,
        author: UserRef {
            id: row.author_id,
            username: row.username.to_string(),
            display_name: row.display_name,
        },
        created_at: row.created_at,
        edited_at: row.edited_at,
        hidden: row.hidden,
        body: match (row.body_md, row.body_html) {
            (Some(markdown), Some(html)) => Some(PostText { markdown, html }),
            _ => None,
        },
    }))
}

/// The ids of the threads in these spaces that the viewer can see.
pub async fn visible_thread_ids(
    db: &Db,
    viewer: &Viewer,
    space_ids: &[i64],
) -> sqlx::Result<Vec<i64>> {
    let filter = viewer.filter();
    sqlx::query_scalar!(
        r#"SELECT t.id FROM threads t
           WHERE t.space_id = ANY($1) AND t.deleted_at IS NULL
             AND thread_readable(t.visibility, t.space_id, $2, $3)"#,
        space_ids,
        &filter.visibilities,
        &filter.restricted_space_ids,
    )
    .fetch_all(db.pool())
    .await
}

/// The first post a logged-in viewer hasn't read in a thread or
/// conversation they can see:
/// the one after their read marker, or the last post if they've read them
/// all. For anyone else, and for threads never opened, the first post.
pub async fn first_unread(
    db: &Db,
    viewer: &Viewer,
    thread_id: i64,
) -> sqlx::Result<Option<(i64, PostLocation)>> {
    let filter = viewer.filter();
    let row = sqlx::query!(
        r#"
        SELECT p.id AS "id!", p.seq AS "seq!", t.slug, t.visibility = 'private' AS "private!"
        FROM threads t
        CROSS JOIN LATERAL (
            SELECT coalesce((SELECT tr.last_read_seq FROM thread_reads tr
                             WHERE tr.thread_id = t.id AND tr.user_id = $4), 0) + 1 AS seq
        ) wanted
        -- The first post from there on, or the last post if there is none:
        -- the places of posts moved elsewhere have no post. Each is one
        -- probe of the (thread, number) index, however long the thread.
        JOIN LATERAL (
            (SELECT p.id, p.seq FROM posts p
             WHERE p.thread_id = t.id AND p.seq >= wanted.seq
             ORDER BY p.seq LIMIT 1)
            UNION ALL
            (SELECT p.id, p.seq FROM posts p
             WHERE p.thread_id = t.id
             ORDER BY p.seq DESC LIMIT 1)
            LIMIT 1
        ) p ON true
        WHERE t.id = $1 AND t.deleted_at IS NULL
          AND (thread_readable(t.visibility, t.space_id, $2, $3) OR dm_readable(t.id, t.visibility, $4))
        "#,
        thread_id,
        &filter.visibilities,
        &filter.restricted_space_ids,
        filter.dm_viewer,
    )
    .fetch_optional(db.pool())
    .await?;
    Ok(row.map(|row| {
        (
            row.id,
            PostLocation {
                thread_id,
                thread_slug: row.slug,
                seq: i64::from(row.seq),
                private: row.private,
            },
        )
    }))
}

// ---------------------------------------------------------------------------
// Edit history

/// One version of a post.
pub struct Version {
    /// 1 for the original.
    pub number: i64,
    pub at: DateTime<Utc>,
    pub editor: UserRef,
    /// `None` if a moderator redacted it.
    pub markdown: Option<String>,
}

pub struct PostHistory {
    pub post: PostTarget,
    /// Oldest first; the last is the current version.
    pub versions: Vec<Version>,
}

/// The versions of a live post the viewer can see. A deleted or hidden
/// post's history is hidden with it.
pub async fn post_history(
    db: &Db,
    viewer: &Viewer,
    post_id: i64,
    deps: &mut Deps,
) -> sqlx::Result<Option<PostHistory>> {
    let Some(post) = post_target(db, viewer, post_id).await? else {
        return Ok(None);
    };
    deps.add(Tag::Thread(post.thread.thread.id));
    let Some(text) = &post.body else {
        return Ok(None);
    };
    let rows = sqlx::query!(
        r#"
        SELECT r.rev, r.created_at, CASE WHEN r.redacted_at IS NULL THEN r.body_md END AS body_md,
               u.id AS editor_id, u.username, u.display_name
        FROM post_revisions r JOIN users u ON u.id = r.editor_id
        WHERE r.post_id = $1
        ORDER BY r.rev
        "#,
        post_id
    )
    .fetch_all(db.pool())
    .await?;
    let current_editor = sqlx::query!(
        r#"SELECT u.id, u.username, u.display_name FROM posts p
           JOIN users u ON u.id = coalesce(p.edited_by, p.author_id) WHERE p.id = $1"#,
        post_id
    )
    .fetch_one(db.pool())
    .await?;
    let mut versions: Vec<Version> = rows
        .into_iter()
        .map(|row| Version {
            number: i64::from(row.rev),
            at: row.created_at,
            editor: UserRef {
                id: row.editor_id,
                username: row.username.to_string(),
                display_name: row.display_name,
            },
            markdown: row.body_md,
        })
        .collect();
    for version in &versions {
        deps.add(Tag::User(version.editor.id));
    }
    deps.add(Tag::User(current_editor.id));
    versions.push(Version {
        number: versions.len() as i64 + 1,
        at: post.edited_at.unwrap_or(post.created_at),
        editor: UserRef {
            id: current_editor.id,
            username: current_editor.username.to_string(),
            display_name: current_editor.display_name,
        },
        markdown: Some(text.markdown.clone()),
    });
    Ok(Some(PostHistory { post, versions }))
}

// ---------------------------------------------------------------------------
// Reactions

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReactionType {
    pub emoji: String,
    pub position: i32,
    pub enabled: bool,
}

/// The reaction types, in order; with `enabled_only`, just those members
/// can use.
pub async fn reaction_types(db: &Db, enabled_only: bool) -> sqlx::Result<Vec<ReactionType>> {
    sqlx::query_as!(
        ReactionType,
        "SELECT emoji, position, enabled FROM reaction_types
         WHERE enabled OR NOT $1 ORDER BY position, emoji",
        enabled_only
    )
    .fetch_all(db.pool())
    .await
}

/// The reaction counts on a live post the viewer can see, as its thread
/// page shows them.
pub async fn post_reaction_counts(
    db: &Db,
    viewer: &Viewer,
    post_id: i64,
) -> sqlx::Result<Option<Vec<ReactionCount>>> {
    let Some(post) = post_target(db, viewer, post_id).await? else {
        return Ok(None);
    };
    if !post.is_live() {
        return Ok(None);
    }
    Ok(Some(
        reactions_for(db, viewer, &[post_id])
            .await?
            .remove(&post_id)
            .unwrap_or_default(),
    ))
}

/// Who reacted to a post with one emoji.
pub struct ReactionDetail {
    pub emoji: String,
    pub users: Vec<UserRef>,
    pub mine: bool,
}

pub struct PostReactions {
    pub post: PostTarget,
    /// Every enabled reaction type, whether or not anyone used it.
    pub reactions: Vec<ReactionDetail>,
}

/// The reactions to a live post the viewer can see.
pub async fn post_reactions(
    db: &Db,
    viewer: &Viewer,
    post_id: i64,
    deps: &mut Deps,
) -> sqlx::Result<Option<PostReactions>> {
    let Some(post) = post_target(db, viewer, post_id).await? else {
        return Ok(None);
    };
    deps.add(Tag::Thread(post.thread.thread.id));
    if !post.is_live() {
        return Ok(None);
    }
    let rows = sqlx::query!(
        r#"
        SELECT r.emoji, u.id, u.username, u.display_name
        FROM reactions r JOIN users u ON u.id = r.user_id
        WHERE r.post_id = $1
        ORDER BY r.created_at, u.id
        "#,
        post_id
    )
    .fetch_all(db.pool())
    .await?;
    let mut reactions: Vec<ReactionDetail> = reaction_types(db, true)
        .await?
        .into_iter()
        .map(|t| ReactionDetail {
            emoji: t.emoji,
            users: Vec::new(),
            mine: false,
        })
        .collect();
    for row in rows {
        if let Some(detail) = reactions.iter_mut().find(|d| d.emoji == row.emoji) {
            detail.mine |= viewer.user_id() == Some(row.id);
            detail.users.push(UserRef {
                id: row.id,
                username: row.username.to_string(),
                display_name: row.display_name,
            });
        }
    }
    Ok(Some(PostReactions { post, reactions }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;

    #[test]
    fn page_counts() {
        assert_eq!(page_count(0, 50), 1);
        assert_eq!(page_count(1, 50), 1);
        assert_eq!(page_count(50, 50), 1);
        assert_eq!(page_count(51, 50), 2);
        assert_eq!(page_of_seq(1), 1);
        assert_eq!(page_of_seq(50), 1);
        assert_eq!(page_of_seq(51), 2);
        assert_eq!(page_of_seq(107), 3);
    }

    #[hegel::test]
    fn every_post_is_on_the_page_its_number_says(tc: TestCase) {
        let seq = tc.draw(
            gs::integers::<i64>()
                .min_value(1)
                .max_value(i64::from(i32::MAX)),
        );
        let page = page_of_seq(seq);
        assert!((page - 1) * POSTS_PER_PAGE < seq && seq <= page * POSTS_PER_PAGE);
    }

    #[hegel::test]
    fn page_offsets_never_overflow(tc: TestCase) {
        let page = tc.draw(gs::integers::<i64>().min_value(1));
        let per_page = tc.draw(gs::integers::<i64>().min_value(1).max_value(1000));
        match page_offset(page, per_page) {
            Some(offset) => assert_eq!(
                i128::from(offset),
                (i128::from(page) - 1) * i128::from(per_page)
            ),
            None => assert!((i128::from(page) - 1) * i128::from(per_page) > i128::from(i64::MAX)),
        }
    }
}
