//! Every read of forum content goes through this module (DESIGN.md §6).
//! Each function takes the [`Viewer`] and filters with the shared
//! `thread_readable` / `category_readable` SQL functions, so private content
//! never reaches a template.
//!
//! Each function also records in a [`Deps`] what the result was built from,
//! which the page cache uses to know when to throw the page away. Ids are
//! recorded even for things the viewer can't see (such as a fork in a
//! private category), because a later change could make them visible.

pub mod messages;
pub mod moderation;
pub mod notifications;
pub mod search;

use std::collections::{BTreeSet, HashMap};

use crate::db::Db;
use chrono::{DateTime, Utc};

use crate::cache::Tag;
use crate::permissions::{Audience, WriteLevel};
use crate::store::CategorySettings;
use crate::viewer::Viewer;

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
pub struct CategoryRef {
    pub id: i64,
    pub slug: String,
    pub name: String,
}

// ---------------------------------------------------------------------------
// The index page

#[derive(Debug)]
pub struct IndexSection {
    /// `None` for categories that aren't in a section; they come first.
    pub name: Option<String>,
    pub categories: Vec<CategorySummary>,
}

#[derive(Debug)]
pub struct CategorySummary {
    pub category: CategoryRef,
    pub description: String,
    pub read_level: String,
    /// For a group-restricted category, the groups that may read it.
    pub group_names: Vec<String>,
    pub thread_count: i64,
    pub post_count: i64,
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

pub async fn index_sections(
    db: &Db,
    viewer: &Viewer,
    deps: &mut Deps,
) -> sqlx::Result<Vec<IndexSection>> {
    let filter = viewer.filter();
    let rows = sqlx::query!(
        r#"
        SELECT c.id, c.slug, c.name, c.description, c.read_level, s.name AS "section_name?",
               array(SELECT g.name FROM category_groups cg JOIN groups g ON g.id = cg.group_id
                     WHERE cg.category_id = c.id AND cg.access = 'read' AND c.read_level = 'groups'
                     ORDER BY lower(g.name), g.id) AS "group_names!",
               counts.threads AS "thread_count!", counts.posts AS "post_count!",
               lt.id AS "last_thread_id?", lt.title AS "last_thread_title?", lt.slug AS "last_thread_slug?",
               coalesce(lp.created_at, lt.last_post_at) AS "last_at?", lp.id AS "last_post_id?",
               lu.id AS "last_user_id?", lu.username AS "last_username?", lu.display_name AS "last_display_name?"
        FROM categories c
        LEFT JOIN sections s ON s.id = c.section_id
        CROSS JOIN LATERAL (
            SELECT count(*) AS threads, coalesce(sum(t.last_seq), 0)::bigint AS posts
            FROM threads t
            WHERE t.category_id = c.id AND t.deleted_at IS NULL
              AND thread_readable(t.visibility, t.category_id, $1, $2)
        ) counts
        LEFT JOIN LATERAL (
            SELECT t.id, t.title, t.slug, t.last_post_at
            FROM threads t
            WHERE t.category_id = c.id AND t.deleted_at IS NULL
              AND thread_readable(t.visibility, t.category_id, $1, $2)
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
        WHERE category_readable(c.read_level, c.id, $1, $2)
        ORDER BY s.position NULLS FIRST, s.id NULLS FIRST, c.position, c.id
        "#,
        &filter.visibilities,
        &filter.group_category_ids,
    )
    .fetch_all(db.pool())
    .await?;

    let mut sections: Vec<IndexSection> = Vec::new();
    for row in rows {
        deps.add(Tag::Category(row.id));
        let summary = CategorySummary {
            category: CategoryRef {
                id: row.id,
                slug: row.slug,
                name: row.name,
            },
            description: row.description,
            read_level: row.read_level,
            group_names: row.group_names,
            thread_count: row.thread_count,
            post_count: row.post_count,
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
        match sections.last_mut() {
            Some(section) if section.name == row.section_name => section.categories.push(summary),
            _ => sections.push(IndexSection {
                name: row.section_name,
                categories: vec![summary],
            }),
        }
    }
    Ok(sections)
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
// Thread lists: the category page, /latest and the index's "Latest" list

#[derive(Debug)]
pub struct ThreadRow {
    pub thread: ThreadRef,
    pub category: CategoryRef,
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

pub struct CategoryPage {
    pub category: CategoryRef,
    pub section_name: Option<String>,
    pub description: String,
    pub read_level: String,
    /// For a group-restricted category, the groups that may read it.
    pub group_names: Vec<String>,
    pub write_level: String,
    pub thread_count: i64,
    pub threads: Vec<ThreadRow>,
}

/// One page of a category's threads, or `None` if the category doesn't
/// exist or the viewer can't read it. A page past the end is empty.
pub async fn category_page(
    db: &Db,
    viewer: &Viewer,
    slug: &str,
    page: i64,
    deps: &mut Deps,
) -> sqlx::Result<Option<CategoryPage>> {
    let filter = viewer.filter();
    let Some(category) = sqlx::query!(
        r#"
        SELECT c.id, c.slug, c.name, c.description, c.read_level, c.write_level, s.name AS "section_name?",
               array(SELECT g.name FROM category_groups cg JOIN groups g ON g.id = cg.group_id
                     WHERE cg.category_id = c.id AND cg.access = 'read' AND c.read_level = 'groups'
                     ORDER BY lower(g.name), g.id) AS "group_names!",
               (SELECT count(*) FROM threads t
                WHERE t.category_id = c.id AND t.deleted_at IS NULL
                  AND thread_readable(t.visibility, t.category_id, $2, $3)) AS "thread_count!"
        FROM categories c
        LEFT JOIN sections s ON s.id = c.section_id
        WHERE c.slug = $1 AND category_readable(c.read_level, c.id, $2, $3)
        "#,
        slug,
        &filter.visibilities,
        &filter.group_category_ids,
    )
    .fetch_optional(db.pool())
    .await?
    else {
        return Ok(None);
    };
    deps.add(Tag::Category(category.id));
    let threads = thread_rows(db, viewer, Some(category.id), page, THREADS_PER_PAGE, deps).await?;
    Ok(Some(CategoryPage {
        category: CategoryRef {
            id: category.id,
            slug: category.slug,
            name: category.name,
        },
        section_name: category.section_name,
        description: category.description,
        read_level: category.read_level,
        group_names: category.group_names,
        write_level: category.write_level,
        thread_count: category.thread_count,
        threads,
    }))
}

pub struct LatestPage {
    pub thread_count: i64,
    pub threads: Vec<ThreadRow>,
}

/// All threads the viewer can see, most recently active first.
pub async fn latest(
    db: &Db,
    viewer: &Viewer,
    page: i64,
    per_page: i64,
    deps: &mut Deps,
) -> sqlx::Result<LatestPage> {
    let filter = viewer.filter();
    // Any visible category can gain the newest thread, so the list depends
    // on all of them.
    let categories = sqlx::query!(
        r#"
        SELECT c.id,
               (SELECT count(*) FROM threads t
                WHERE t.category_id = c.id AND t.deleted_at IS NULL
                  AND thread_readable(t.visibility, t.category_id, $1, $2)) AS "thread_count!"
        FROM categories c
        WHERE category_readable(c.read_level, c.id, $1, $2)
        "#,
        &filter.visibilities,
        &filter.group_category_ids,
    )
    .fetch_all(db.pool())
    .await?;
    let mut thread_count = 0;
    for category in categories {
        deps.add(Tag::Category(category.id));
        thread_count += category.thread_count;
    }
    let threads = thread_rows(db, viewer, None, page, per_page, deps).await?;
    Ok(LatestPage {
        thread_count,
        threads,
    })
}

async fn thread_rows(
    db: &Db,
    viewer: &Viewer,
    category_id: Option<i64>,
    page: i64,
    per_page: i64,
    deps: &mut Deps,
) -> sqlx::Result<Vec<ThreadRow>> {
    let Some(offset) = page_offset(page, per_page) else {
        return Ok(Vec::new());
    };
    let filter = viewer.filter();
    // With a category, pinned threads come first; /latest ignores pins.
    let rows = sqlx::query!(
        r#"
        SELECT t.id, t.title, t.slug, t.created_at, t.last_seq,
               coalesce(lp.created_at, t.last_post_at) AS "last_post_at!", t.pinned, t.locked,
               c.id AS category_id, c.slug AS category_slug, c.name AS category_name,
               a.id AS author_id, a.username AS author_username, a.display_name AS author_display_name,
               lp.id AS "last_post_id?", lu.id AS "last_user_id?", lu.username AS "last_username?",
               lu.display_name AS "last_display_name?",
               src.id AS "source_thread_id?", src.title AS "source_title?", src.slug AS "source_slug?",
               src.visible AS "source_visible?",
               CASE WHEN $6::bigint IS NULL THEN NULL
                    ELSE t.last_seq - coalesce(tr.last_read_seq, 0) END AS unread
        FROM threads t
        LEFT JOIN thread_reads tr ON tr.thread_id = t.id AND tr.user_id = $6
        JOIN categories c ON c.id = t.category_id
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
                   st.deleted_at IS NULL AND thread_readable(st.visibility, st.category_id, $1, $2) AS visible
            FROM posts sp JOIN threads st ON st.id = sp.thread_id
            WHERE sp.id = t.forked_from_post_id
        ) src ON true
        WHERE t.deleted_at IS NULL
          AND thread_readable(t.visibility, t.category_id, $1, $2)
          AND ($3::bigint IS NULL OR t.category_id = $3)
        ORDER BY CASE WHEN $3::bigint IS NULL THEN false ELSE t.pinned END DESC,
                 t.last_post_at DESC, t.id DESC
        LIMIT $4 OFFSET $5
        "#,
        &filter.visibilities,
        &filter.group_category_ids,
        category_id,
        per_page,
        offset,
        viewer.user_id(),
    )
    .fetch_all(db.pool())
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            deps.add(Tag::Thread(row.id));
            deps.add(Tag::Category(row.category_id));
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
                category: CategoryRef {
                    id: row.category_id,
                    slug: row.category_slug,
                    name: row.category_name,
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
    pub category: CategoryRef,
    pub section_name: Option<String>,
    pub author: UserRef,
    pub created_at: DateTime<Utc>,
    /// The number of the last post; posts are numbered from 1 and deleted
    /// posts keep their numbers, so this is also the number of posts.
    pub last_seq: i64,
    pub pinned: bool,
    pub locked: bool,
    /// The category's read and write levels, for deciding who may reply.
    pub read_level: String,
    pub write_level: String,
    /// For a quote-fork, the post it quotes.
    pub embed: Option<Embed>,
    pub posts: Vec<PostView>,
    /// For a logged-in viewer, how far they had read before this page: the
    /// number of the last post they'd seen, or `None` if they had never
    /// opened the thread.
    pub read_marker: Option<i64>,
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
    pub category: CategoryRef,
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

#[derive(Clone, Debug)]
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
               c.id AS category_id, c.slug AS category_slug, c.name AS category_name, s.name AS "section_name?",
               c.read_level, c.write_level,
               (SELECT tr.last_read_seq FROM thread_reads tr
                WHERE tr.thread_id = t.id AND tr.user_id = $4) AS read_marker,
               a.id AS author_id, a.username AS author_username, a.display_name AS author_display_name
        FROM threads t
        JOIN categories c ON c.id = t.category_id
        LEFT JOIN sections s ON s.id = c.section_id
        JOIN users a ON a.id = t.author_id
        WHERE t.id = $1 AND t.deleted_at IS NULL AND thread_readable(t.visibility, t.category_id, $2, $3)
        "#,
        thread_id,
        &filter.visibilities,
        &filter.group_category_ids,
        filter.dm_viewer,
    )
    .fetch_optional(db.pool())
    .await?
    else {
        return Ok(None);
    };
    // The category's name appears in the breadcrumbs, but renaming a
    // category clears `site`, and moving the thread clears `thread:N`.
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
        category: CategoryRef {
            id: thread.category_id,
            slug: thread.category_slug,
            name: thread.category_name,
        },
        section_name: thread.section_name,
        author: UserRef {
            id: thread.author_id,
            username: thread.author_username.to_string(),
            display_name: thread.author_display_name,
        },
        created_at: thread.created_at,
        last_seq: i64::from(thread.last_seq),
        pinned: thread.pinned,
        locked: thread.locked,
        read_level: thread.read_level,
        write_level: thread.write_level,
        embed,
        posts,
        read_marker: thread.read_marker.map(i64::from),
    }))
}

/// The posts on one page of a thread the caller has checked the viewer can
/// read: a category thread or a conversation.
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
    let blocked = blocked_authors(db, viewer).await?;
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
               t.deleted_at IS NULL AND thread_readable(t.visibility, t.category_id, $4, $5) AS "readable!"
        FROM post_moves m
        JOIN posts p ON p.id = m.post_id
        JOIN threads t ON t.id = p.thread_id
        WHERE m.from_thread_id = $1 AND m.from_seq BETWEEN $2 AND $3
        "#,
        thread_id,
        first,
        last,
        &filter.visibilities,
        &filter.group_category_ids,
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
/// blocked, unless they are staff now (staff can't be blocked).
pub(super) async fn blocked_authors(db: &Db, viewer: &Viewer) -> sqlx::Result<BTreeSet<i64>> {
    let Some(user_id) = viewer.user_id() else {
        return Ok(BTreeSet::new());
    };
    Ok(sqlx::query_scalar!(
        r#"SELECT b.blocked_user_id FROM blocks b JOIN users u ON u.id = b.blocked_user_id
           WHERE b.user_id = $1 AND u.role = 'user'"#,
        user_id
    )
    .fetch_all(db.pool())
    .await?
    .into_iter()
    .collect())
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
        SELECT u.id, u.username, u.display_name, u.title, u.pronouns, u.status_line, u.location, u.created_at,
               (SELECT count(*) FROM posts p JOIN threads t ON t.id = p.thread_id
                WHERE p.author_id = u.id AND p.deleted_at IS NULL AND p.hidden_at IS NULL
                  AND t.deleted_at IS NULL
                  AND thread_readable(t.visibility, t.category_id, $2, $3)) AS "post_count!"
        FROM users u
        WHERE u.id = ANY($1)
        "#,
        ids,
        &filter.visibilities,
        &filter.group_category_ids,
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
               f.deleted_at IS NULL AND thread_readable(f.visibility, f.category_id, $2, $3) AS "visible!",
               a.id AS author_id, a.username AS author_username, a.display_name AS author_display_name
        FROM threads f
        JOIN users a ON a.id = f.author_id
        WHERE f.forked_from_post_id = ANY($1)
        ORDER BY f.created_at, f.id
        "#,
        post_ids,
        &filter.visibilities,
        &filter.group_category_ids,
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
                AND thread_readable(t.visibility, t.category_id, $2, $3)) AS "available!"
        FROM posts p
        JOIN threads t ON t.id = p.thread_id
        WHERE p.id = $1
        "#,
        post_id,
        &filter.visibilities,
        &filter.group_category_ids,
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
        SELECT p.body_html, t.title, t.slug, c.id AS category_id, c.slug AS category_slug, c.name AS category_name,
               u.id AS author_id, u.username, u.display_name, u.title AS author_title
        FROM posts p
        JOIN threads t ON t.id = p.thread_id
        JOIN categories c ON c.id = t.category_id
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
        category: CategoryRef {
            id: post.category_id,
            slug: post.category_slug,
            name: post.category_name,
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

/// How many of a member's posts the viewer can see. Pages showing it
/// depend on `user:N`, which new posts and thread moves clear.
pub async fn visible_post_count(db: &Db, viewer: &Viewer, user_id: i64) -> sqlx::Result<i64> {
    let filter = viewer.filter();
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM posts p JOIN threads t ON t.id = p.thread_id
           WHERE p.author_id = $1 AND p.deleted_at IS NULL AND p.hidden_at IS NULL
             AND t.deleted_at IS NULL AND thread_readable(t.visibility, t.category_id, $2, $3)"#,
        user_id,
        &filter.visibilities,
        &filter.group_category_ids,
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
          AND (thread_readable(t.visibility, t.category_id, $2, $3) OR dm_readable(t.id, t.visibility, $4))
        "#,
        post_id,
        &filter.visibilities,
        &filter.group_category_ids,
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
        WHERE t.id = $1 AND t.deleted_at IS NULL AND thread_readable(t.visibility, t.category_id, $2, $3)
        "#,
        thread_id,
        &filter.visibilities,
        &filter.group_category_ids,
    )
    .fetch_optional(db.pool())
    .await
}

// ---------------------------------------------------------------------------
// Writing: what a form is about, checked against the viewer
//
// These say where something is and who may see it; whether the viewer may
// write there is for [`crate::permissions`] to decide.

/// A category the viewer can read, as somewhere to post.
#[derive(Clone, Debug)]
pub struct CategoryTarget {
    pub category: CategoryRef,
    pub section_id: Option<i64>,
    pub section_name: Option<String>,
    pub description: String,
    pub position: i32,
    pub read_level: String,
    pub write_level: String,
    /// The groups named for reading and posting. They matter only with the
    /// `groups` read and write levels.
    pub read_groups: Vec<i64>,
    pub write_groups: Vec<i64>,
    /// The names of the groups that may read a group-restricted category.
    pub group_names: Vec<String>,
    /// The names of the groups that may post, with the `groups` write level.
    pub write_group_names: Vec<String>,
}

impl CategoryTarget {
    pub fn audience(&self) -> Audience {
        Audience::from_level(&self.read_level, self.read_groups.iter().copied())
    }

    pub fn write(&self) -> WriteLevel {
        WriteLevel::from_level(&self.write_level, self.write_groups.iter().copied())
    }

    /// The category's settings, as an admin edits them.
    pub fn settings(&self) -> CategorySettings {
        CategorySettings {
            section_id: self.section_id,
            slug: self.category.slug.clone(),
            name: self.category.name.clone(),
            description: self.description.clone(),
            position: self.position,
            read_level: self.read_level.clone(),
            write_level: self.write_level.clone(),
            read_groups: self.read_groups.clone(),
            write_groups: self.write_groups.clone(),
        }
    }
}

/// A category the viewer can read, by id.
pub async fn category_by_id(
    db: &Db,
    viewer: &Viewer,
    id: i64,
) -> sqlx::Result<Option<CategoryTarget>> {
    Ok(readable_categories(db, viewer)
        .await?
        .into_iter()
        .find(|c| c.category.id == id))
}

/// How many threads a category the viewer can read has ever had,
/// including deleted ones: only a category with none can be deleted.
pub async fn category_thread_count(db: &Db, viewer: &Viewer, id: i64) -> sqlx::Result<i64> {
    let filter = viewer.filter();
    sqlx::query_scalar!(
        r#"SELECT count(t.id) AS "n!" FROM categories c LEFT JOIN threads t ON t.category_id = c.id
           WHERE c.id = $1 AND category_readable(c.read_level, c.id, $2, $3)"#,
        id,
        &filter.visibilities,
        &filter.group_category_ids,
    )
    .fetch_one(db.pool())
    .await
}

/// A display-only heading on the index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub id: i64,
    pub name: String,
    pub position: i32,
}

/// Every section, in index order. Sections hold no content of their own,
/// so every viewer may know them.
pub async fn sections(db: &Db) -> sqlx::Result<Vec<Section>> {
    sqlx::query_as!(
        Section,
        "SELECT id, name, position FROM sections ORDER BY position, id"
    )
    .fetch_all(db.pool())
    .await
}

/// Every category the viewer can read, in index order.
pub async fn readable_categories(db: &Db, viewer: &Viewer) -> sqlx::Result<Vec<CategoryTarget>> {
    let filter = viewer.filter();
    let rows = sqlx::query!(
        r#"
        SELECT c.id, c.slug, c.name, c.description, c.position, c.section_id, c.read_level, c.write_level,
               s.name AS "section_name?",
               array(SELECT cg.group_id FROM category_groups cg
                     WHERE cg.category_id = c.id AND cg.access = 'read' ORDER BY 1) AS "read_groups!",
               array(SELECT cg.group_id FROM category_groups cg
                     WHERE cg.category_id = c.id AND cg.access = 'write' ORDER BY 1) AS "write_groups!",
               array(SELECT g.name FROM category_groups cg JOIN groups g ON g.id = cg.group_id
                     WHERE cg.category_id = c.id AND cg.access = 'write' AND c.write_level = 'groups'
                     ORDER BY lower(g.name), g.id) AS "write_group_names!",
               array(SELECT g.name FROM category_groups cg JOIN groups g ON g.id = cg.group_id
                     WHERE cg.category_id = c.id AND cg.access = 'read' AND c.read_level = 'groups'
                     ORDER BY lower(g.name), g.id) AS "group_names!"
        FROM categories c LEFT JOIN sections s ON s.id = c.section_id
        WHERE category_readable(c.read_level, c.id, $1, $2)
        ORDER BY s.position NULLS FIRST, s.id NULLS FIRST, c.position, c.id
        "#,
        &filter.visibilities,
        &filter.group_category_ids,
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| CategoryTarget {
            category: CategoryRef {
                id: row.id,
                slug: row.slug,
                name: row.name,
            },
            section_id: row.section_id,
            section_name: row.section_name,
            description: row.description,
            position: row.position,
            read_level: row.read_level,
            write_level: row.write_level,
            read_groups: row.read_groups,
            write_groups: row.write_groups,
            group_names: row.group_names,
            write_group_names: row.write_group_names,
        })
        .collect())
}

/// A category the viewer can read, by slug.
pub async fn category_target(
    db: &Db,
    viewer: &Viewer,
    slug: &str,
) -> sqlx::Result<Option<CategoryTarget>> {
    Ok(readable_categories(db, viewer)
        .await?
        .into_iter()
        .find(|c| c.category.slug == slug))
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

/// Where a thread is: in a category, or a conversation between members.
#[derive(Clone, Debug)]
pub enum Place {
    Category(CategoryTarget),
    Conversation(messages::ConversationTarget),
}

impl ThreadTarget {
    pub fn category(&self) -> Option<&CategoryTarget> {
        match &self.place {
            Place::Category(category) => Some(category),
            Place::Conversation(_) => None,
        }
    }

    pub fn conversation(&self) -> Option<&messages::ConversationTarget> {
        match &self.place {
            Place::Category(_) => None,
            Place::Conversation(conversation) => Some(conversation),
        }
    }

    /// Who can read it.
    pub fn audience(&self) -> Audience {
        match &self.place {
            Place::Category(category) => category.audience(),
            Place::Conversation(_) => Audience::Private,
        }
    }

    /// Its page: the thread, or the conversation.
    pub fn href(&self) -> String {
        match &self.place {
            Place::Category(_) => crate::format::thread_href(&self.thread.id, &self.thread.slug),
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
        SELECT t.id, t.title, t.slug, t.locked, t.last_seq, t.category_id
        FROM threads t
        WHERE t.id = $1 AND t.deleted_at IS NULL
          AND (thread_readable(t.visibility, t.category_id, $2, $3) OR dm_readable(t.id, t.visibility, $4))
        "#,
        thread_id,
        &filter.visibilities,
        &filter.group_category_ids,
        filter.dm_viewer,
    )
    .fetch_optional(db.pool())
    .await?
    else {
        return Ok(None);
    };
    let place = match row.category_id {
        Some(category_id) => {
            let Some(category) = readable_categories(db, viewer)
                .await?
                .into_iter()
                .find(|c| c.category.id == category_id)
            else {
                return Ok(None);
            };
            Place::Category(category)
        }
        None => Place::Conversation(messages::conversation_target(db, row.id).await?),
    };
    let name = match &place {
        Place::Category(_) => row.title.clone(),
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
          AND (thread_readable(t.visibility, t.category_id, $2, $3) OR dm_readable(t.id, t.visibility, $4))
        "#,
        post_id,
        &filter.visibilities,
        &filter.group_category_ids,
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

/// The ids of the threads in a category that the viewer can see.
pub async fn visible_thread_ids(
    db: &Db,
    viewer: &Viewer,
    category_id: i64,
) -> sqlx::Result<Vec<i64>> {
    let filter = viewer.filter();
    sqlx::query_scalar!(
        r#"SELECT t.id FROM threads t
           WHERE t.category_id = $1 AND t.deleted_at IS NULL
             AND thread_readable(t.visibility, t.category_id, $2, $3)"#,
        category_id,
        &filter.visibilities,
        &filter.group_category_ids,
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
        SELECT p.id, p.seq, t.slug, t.visibility = 'private' AS "private!"
        FROM threads t
        CROSS JOIN LATERAL (
            SELECT coalesce((SELECT tr.last_read_seq FROM thread_reads tr
                             WHERE tr.thread_id = t.id AND tr.user_id = $4), 0) + 1 AS seq
        ) wanted
        -- The first post from there on, or the last post if there is none:
        -- the places of posts moved elsewhere have no post.
        JOIN LATERAL (
            SELECT p.id, p.seq FROM posts p WHERE p.thread_id = t.id
            ORDER BY p.seq >= wanted.seq DESC, CASE WHEN p.seq >= wanted.seq THEN p.seq END, p.seq DESC
            LIMIT 1
        ) p ON true
        WHERE t.id = $1 AND t.deleted_at IS NULL
          AND (thread_readable(t.visibility, t.category_id, $2, $3) OR dm_readable(t.id, t.visibility, $4))
        "#,
        thread_id,
        &filter.visibilities,
        &filter.group_category_ids,
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
