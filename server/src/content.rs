//! Every read of forum content goes through this module (DESIGN.md §6).
//! Each function takes the [`Viewer`] and filters with the shared
//! `thread_readable` / `category_readable` SQL functions, so private content
//! never reaches a template.
//!
//! Each function also records in a [`Deps`] what the result was built from,
//! which the page cache uses to know when to throw the page away. Ids are
//! recorded even for things the viewer can't see (such as a fork in a
//! private category), because a later change could make them visible.

use std::collections::{BTreeSet, HashMap};

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::cache::Tag;
use crate::viewer::Viewer;

pub const POSTS_PER_PAGE: i64 = 50;
pub const THREADS_PER_PAGE: i64 = 30;
pub const LATEST_ON_INDEX: i64 = 8;

/// What a page was built from.
#[derive(Debug, Default)]
pub struct Deps {
    tags: BTreeSet<Tag>,
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
    db: &PgPool,
    viewer: &Viewer,
    deps: &mut Deps,
) -> sqlx::Result<Vec<IndexSection>> {
    let filter = viewer.filter();
    let rows = sqlx::query!(
        r#"
        SELECT c.id, c.slug, c.name, c.description, c.read_level, s.name AS "section_name?",
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
    .fetch_all(db)
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
}

pub struct CategoryPage {
    pub category: CategoryRef,
    pub section_name: Option<String>,
    pub description: String,
    pub read_level: String,
    pub write_level: String,
    pub thread_count: i64,
    pub threads: Vec<ThreadRow>,
}

/// One page of a category's threads, or `None` if the category doesn't
/// exist or the viewer can't read it. A page past the end is empty.
pub async fn category_page(
    db: &PgPool,
    viewer: &Viewer,
    slug: &str,
    page: i64,
    deps: &mut Deps,
) -> sqlx::Result<Option<CategoryPage>> {
    let filter = viewer.filter();
    let Some(category) = sqlx::query!(
        r#"
        SELECT c.id, c.slug, c.name, c.description, c.read_level, c.write_level, s.name AS "section_name?",
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
    .fetch_optional(db)
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
    db: &PgPool,
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
    .fetch_all(db)
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
    db: &PgPool,
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
               src.visible AS "source_visible?"
        FROM threads t
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
    )
    .fetch_all(db)
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
    /// For a quote-fork, the post it quotes.
    pub embed: Option<Embed>,
    pub posts: Vec<PostView>,
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
}

pub enum PostBody {
    Live {
        author: Box<AuthorInfo>,
        body_html: String,
        edited_at: Option<DateTime<Utc>>,
    },
    Deleted,
    Hidden,
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
    db: &PgPool,
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
    )
    .fetch_optional(db)
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

    // Post numbers are 32-bit, so a page whose numbers don't fit is empty.
    let seq_range = page_offset(page, POSTS_PER_PAGE)
        .and_then(|offset| Some((offset.checked_add(1)?, offset.checked_add(POSTS_PER_PAGE)?)))
        .and_then(|(first, last)| Some((i32::try_from(first).ok()?, i32::try_from(last).ok()?)));
    let (first, last) = seq_range.unwrap_or((1, 0));
    let rows = sqlx::query!(
        r#"
        SELECT p.id, p.seq, p.created_at, p.edited_at,
               (p.deleted_at IS NOT NULL) AS "deleted!", (p.hidden_at IS NOT NULL) AS "hidden!",
               CASE WHEN p.deleted_at IS NULL AND p.hidden_at IS NULL THEN p.body_html END AS body_html,
               CASE WHEN p.deleted_at IS NULL AND p.hidden_at IS NULL THEN p.author_id END AS author_id,
               r.id AS "reply_id?", r.seq AS "reply_seq?",
               ru.id AS "reply_user_id?", ru.username AS "reply_username?", ru.display_name AS "reply_display_name?"
        FROM posts p
        LEFT JOIN posts r ON r.id = p.reply_to_post_id AND r.thread_id = p.thread_id
        LEFT JOIN users ru ON ru.id = r.author_id AND r.deleted_at IS NULL AND r.hidden_at IS NULL
        WHERE p.thread_id = $1 AND p.seq BETWEEN $2 AND $3
        ORDER BY p.seq
        "#,
        thread.id,
        first,
        last,
    )
    .fetch_all(db)
    .await?;

    let author_ids: Vec<i64> = {
        let mut ids: Vec<i64> = rows.iter().filter_map(|row| row.author_id).collect();
        ids.sort();
        ids.dedup();
        ids
    };
    let authors = author_infos(db, viewer, &author_ids, deps).await?;
    let post_ids: Vec<i64> = rows.iter().map(|row| row.id).collect();
    let mut forks = forks_of(db, viewer, &post_ids, deps).await?;

    let posts = rows
        .into_iter()
        .map(|row| {
            let body = if row.deleted {
                PostBody::Deleted
            } else if row.hidden {
                PostBody::Hidden
            } else {
                let author_id = row.author_id.expect("live posts have authors");
                PostBody::Live {
                    author: Box::new(authors.get(&author_id).cloned().expect("author loaded")),
                    body_html: row.body_html.unwrap_or_default(),
                    edited_at: row.edited_at,
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
                body,
                reply_to,
            }
        })
        .collect();

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
        embed,
        posts,
    }))
}

async fn author_infos(
    db: &PgPool,
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
    .fetch_all(db)
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

/// Visible quote-forks of each of these posts.
async fn forks_of(
    db: &PgPool,
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
    .fetch_all(db)
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
async fn embed(db: &PgPool, viewer: &Viewer, post_id: i64, deps: &mut Deps) -> sqlx::Result<Embed> {
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
    .fetch_optional(db)
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
    .fetch_one(db)
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
// Permalinks

pub struct PostLocation {
    pub thread_id: i64,
    pub thread_slug: String,
    pub seq: i64,
}

/// Where a post is, if the viewer can see its thread. Deleted and hidden
/// posts still have a location: their tombstone.
pub async fn post_location(
    db: &PgPool,
    viewer: &Viewer,
    post_id: i64,
) -> sqlx::Result<Option<PostLocation>> {
    let filter = viewer.filter();
    let row = sqlx::query!(
        r#"
        SELECT t.id, t.slug, p.seq
        FROM posts p JOIN threads t ON t.id = p.thread_id
        WHERE p.id = $1 AND t.deleted_at IS NULL AND thread_readable(t.visibility, t.category_id, $2, $3)
        "#,
        post_id,
        &filter.visibilities,
        &filter.group_category_ids,
    )
    .fetch_optional(db)
    .await?;
    Ok(row.map(|row| PostLocation {
        thread_id: row.id,
        thread_slug: row.slug,
        seq: i64::from(row.seq),
    }))
}

/// The thread's canonical slug, if the viewer can see it.
pub async fn thread_slug(
    db: &PgPool,
    viewer: &Viewer,
    thread_id: i64,
) -> sqlx::Result<Option<String>> {
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
    .fetch_optional(db)
    .await
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
