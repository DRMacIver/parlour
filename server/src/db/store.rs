//! Low-level writes for the content model: creating users, sections,
//! categories, threads and posts with their bookkeeping (post numbering,
//! thread activity, rendered HTML) kept consistent.
//!
//! Each takes a [`Handle`]: the database, or an open transaction.
//!
//! These functions don't check permissions or clear the page cache. Request
//! handlers that write (M4 onwards) check permissions first and then call
//! [`crate::AppState::changed`] once the write has committed. The `seed`
//! command uses them directly.

use crate::db::{Handle, KEY};
use chrono::{DateTime, Utc};

use crate::markdown;

/// Turns a title into the readable part of a thread URL.
pub fn slugify(title: &str) -> String {
    let mut slug = String::new();
    let mut pending_dash = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            pending_dash = false;
            slug.push(c.to_ascii_lowercase());
            if slug.len() >= 60 {
                break;
            }
        } else if !matches!(c, '\'' | '’' | '‘') {
            pending_dash = true;
        }
    }
    if slug.is_empty() {
        slug.push_str("thread");
    }
    // `/t/{id}/unread` and `/t/{id}/reply` are routes of their own.
    if RESERVED_SLUGS.contains(&slug.as_str()) {
        slug.push_str("-thread");
    }
    slug
}

const RESERVED_SLUGS: &[&str] = &["unread", "reply"];

#[derive(Clone, Debug, Default)]
pub struct NewUser {
    pub username: String,
    pub display_name: String,
    pub email: String,
    pub role: Option<String>,
    pub title: Option<String>,
    pub pronouns: Option<String>,
    pub status_line: Option<String>,
    pub location: Option<String>,
    pub invited_by_user_id: Option<i64>,
    pub created_at: Option<DateTime<Utc>>,
}

pub async fn create_user<'c>(db: impl Handle<'c>, user: &NewUser) -> sqlx::Result<i64> {
    let mut conn = db.open(KEY).acquire().await?;
    sqlx::query_scalar!(
        r#"INSERT INTO users (username, display_name, email, role, title, pronouns, status_line,
                              location, invited_by_user_id, created_at)
           VALUES ($1, $2, $3, coalesce($4, 'user'), $5, $6, $7, $8, $9, coalesce($10, now()))
           RETURNING id"#,
        user.username as _,
        user.display_name,
        user.email as _,
        user.role,
        user.title,
        user.pronouns,
        user.status_line,
        user.location,
        user.invited_by_user_id,
        user.created_at,
    )
    .fetch_one(&mut *conn)
    .await
}

pub async fn create_section<'c>(
    db: impl Handle<'c>,
    name: &str,
    position: i32,
) -> sqlx::Result<i64> {
    let mut conn = db.open(KEY).acquire().await?;
    sqlx::query_scalar!(
        "INSERT INTO sections (name, position) VALUES ($1, $2) RETURNING id",
        name,
        position
    )
    .fetch_one(&mut *conn)
    .await
}

#[derive(Clone, Debug)]
pub struct NewCategory {
    pub section_id: Option<i64>,
    pub slug: String,
    pub name: String,
    pub description: String,
    pub position: i32,
    /// `public`, `members` or `groups`.
    pub read_level: String,
    /// `members`, `groups` or `staff`.
    pub write_level: String,
}

pub async fn create_category<'c>(db: impl Handle<'c>, category: &NewCategory) -> sqlx::Result<i64> {
    let mut conn = db.open(KEY).acquire().await?;
    sqlx::query_scalar!(
        r#"INSERT INTO categories (section_id, slug, name, description, position, read_level, write_level)
           VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id"#,
        category.section_id,
        category.slug,
        category.name,
        category.description,
        category.position,
        category.read_level,
        category.write_level,
    )
    .fetch_one(&mut *conn)
    .await
}

// ---------------------------------------------------------------------------
// Sections and categories, as admins manage them

pub const READ_LEVELS: [&str; 3] = ["public", "members", "groups"];
pub const WRITE_LEVELS: [&str; 3] = ["members", "groups", "staff"];
pub const MAX_SECTION_NAME_CHARS: usize = 80;
pub const MAX_CATEGORY_NAME_CHARS: usize = 80;
pub const MAX_CATEGORY_DESCRIPTION_CHARS: usize = 500;
pub const MAX_CATEGORY_SLUG_CHARS: usize = 40;

/// Everything about a category that an admin sets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CategorySettings {
    pub section_id: Option<i64>,
    pub slug: String,
    pub name: String,
    pub description: String,
    pub position: i32,
    /// `public`, `members` or `groups`.
    pub read_level: String,
    /// `members`, `groups` or `staff`.
    pub write_level: String,
    /// With the `groups` read level, the groups whose members may read.
    pub read_groups: Vec<i64>,
    /// With the `groups` write level, the groups whose members may post.
    pub write_groups: Vec<i64>,
}

fn one_line(text: &str) -> String {
    text.replace(|c: char| c.is_control(), " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

impl CategorySettings {
    /// Tidies the settings, returning the problems with them if any. Group
    /// lists are sorted and deduplicated, and dropped for levels that don't
    /// use them.
    pub fn clean(&self) -> Result<CategorySettings, Vec<String>> {
        let mut clean = CategorySettings {
            section_id: self.section_id,
            slug: self.slug.trim().to_ascii_lowercase(),
            name: one_line(&self.name),
            description: one_line(&self.description),
            position: self.position,
            read_level: self.read_level.clone(),
            write_level: self.write_level.clone(),
            read_groups: self.read_groups.clone(),
            write_groups: self.write_groups.clone(),
        };
        for groups in [&mut clean.read_groups, &mut clean.write_groups] {
            groups.sort();
            groups.dedup();
        }
        if clean.read_level != "groups" {
            clean.read_groups.clear();
        }
        if clean.write_level != "groups" {
            clean.write_groups.clear();
        }
        let mut problems = Vec::new();
        if !crate::groups::valid_slug(&clean.slug) || clean.slug.len() > MAX_CATEGORY_SLUG_CHARS {
            problems.push(format!(
                "The address should be up to {MAX_CATEGORY_SLUG_CHARS} lower-case letters and digits, with single hyphens between words."
            ));
        }
        if clean.name.is_empty() || clean.name.chars().count() > MAX_CATEGORY_NAME_CHARS {
            problems.push(format!(
                "The name should be between 1 and {MAX_CATEGORY_NAME_CHARS} characters."
            ));
        }
        if clean.description.chars().count() > MAX_CATEGORY_DESCRIPTION_CHARS {
            problems.push(format!(
                "Keep the description to {MAX_CATEGORY_DESCRIPTION_CHARS} characters."
            ));
        }
        if !READ_LEVELS.contains(&clean.read_level.as_str()) {
            problems.push("Choose who can read the category.".to_owned());
        }
        if !WRITE_LEVELS.contains(&clean.write_level.as_str()) {
            problems.push("Choose who can post in the category.".to_owned());
        }
        // Nobody can post where they can't read, so a group that may post
        // but not read would be a mistake.
        if clean.read_level == "groups"
            && clean.write_level == "groups"
            && !clean
                .write_groups
                .iter()
                .all(|g| clean.read_groups.contains(g))
        {
            problems.push("Groups that can post must also be able to read.".to_owned());
        }
        if problems.is_empty() {
            Ok(clean)
        } else {
            Err(problems)
        }
    }
}

const CATEGORY_SLUG_TAKEN: &str = "Another category has that address.";

fn slug_clash(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::Database(err) if err.is_unique_violation())
}

/// Replaces the groups named for a category.
async fn set_category_groups(
    conn: &mut sqlx::PgConnection,
    category_id: i64,
    settings: &CategorySettings,
) -> sqlx::Result<()> {
    sqlx::query!(
        "DELETE FROM category_groups WHERE category_id = $1",
        category_id
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "INSERT INTO category_groups (category_id, group_id, access)
         SELECT $1::bigint, g, 'read' FROM unnest($2::bigint[]) g
         UNION ALL
         SELECT $1::bigint, g, 'write' FROM unnest($3::bigint[]) g",
        category_id,
        &settings.read_groups,
        &settings.write_groups,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Makes a category from settings that [`CategorySettings::clean`]
/// accepted. Returns the problem if another category has its address.
pub async fn add_category<'c>(
    db: impl Handle<'c>,
    settings: &CategorySettings,
) -> sqlx::Result<Result<i64, String>> {
    let mut tx = db.open(KEY).begin().await?;
    let inserted = sqlx::query_scalar!(
        r#"INSERT INTO categories (section_id, slug, name, description, position, read_level, write_level)
           VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id"#,
        settings.section_id,
        settings.slug,
        settings.name,
        settings.description,
        settings.position,
        settings.read_level,
        settings.write_level,
    )
    .fetch_one(&mut *tx)
    .await;
    let id = match inserted {
        Ok(id) => id,
        Err(err) if slug_clash(&err) => return Ok(Err(CATEGORY_SLUG_TAKEN.to_owned())),
        Err(err) => return Err(err),
    };
    set_category_groups(&mut tx, id, settings).await?;
    tx.commit().await?;
    Ok(Ok(id))
}

/// Changes a category to settings that [`CategorySettings::clean`]
/// accepted. Its threads' visibility follows its new read level (a trigger
/// sees to that). `Ok(Ok(false))` if there is no such category.
pub async fn update_category<'c>(
    db: impl Handle<'c>,
    id: i64,
    settings: &CategorySettings,
) -> sqlx::Result<Result<bool, String>> {
    let mut tx = db.open(KEY).begin().await?;
    let updated = sqlx::query!(
        r#"UPDATE categories SET section_id = $2, slug = $3, name = $4, description = $5, position = $6,
                  read_level = $7, write_level = $8, version = version + 1
           WHERE id = $1"#,
        id,
        settings.section_id,
        settings.slug,
        settings.name,
        settings.description,
        settings.position,
        settings.read_level,
        settings.write_level,
    )
    .execute(&mut *tx)
    .await;
    match updated {
        Ok(done) if done.rows_affected() == 0 => return Ok(Ok(false)),
        Ok(_) => {}
        Err(err) if slug_clash(&err) => return Ok(Err(CATEGORY_SLUG_TAKEN.to_owned())),
        Err(err) => return Err(err),
    }
    set_category_groups(&mut tx, id, settings).await?;
    tx.commit().await?;
    Ok(Ok(true))
}

/// Deletes a category that has never had threads. Returns the problem if
/// it has some, and `Ok(Ok(false))` if there is no such category.
pub async fn delete_category(db: &crate::db::Db, id: i64) -> sqlx::Result<Result<bool, String>> {
    let mut tx = db.pool().begin().await?;
    let threads = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM threads WHERE category_id = $1"#,
        id
    )
    .fetch_one(&mut *tx)
    .await?;
    if threads > 0 {
        return Ok(Err(
            "Only an empty category can be deleted. Move its threads elsewhere first.".to_owned(),
        ));
    }
    let done = sqlx::query!("DELETE FROM categories WHERE id = $1", id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Ok(done.rows_affected() == 1))
}

/// Tidies a section name, or says what is wrong with it.
pub fn clean_section_name(name: &str) -> Result<String, String> {
    let name = one_line(name);
    if name.is_empty() || name.chars().count() > MAX_SECTION_NAME_CHARS {
        Err(format!(
            "A section name should be between 1 and {MAX_SECTION_NAME_CHARS} characters."
        ))
    } else {
        Ok(name)
    }
}

/// Renames or moves a section. Returns whether there was one.
pub async fn update_section(
    db: &crate::db::Db,
    id: i64,
    name: &str,
    position: i32,
) -> sqlx::Result<bool> {
    let done = sqlx::query!(
        "UPDATE sections SET name = $2, position = $3 WHERE id = $1",
        id,
        name,
        position
    )
    .execute(db.pool())
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Deletes a section. Its categories stay, outside any section.
pub async fn delete_section(db: &crate::db::Db, id: i64) -> sqlx::Result<bool> {
    let done = sqlx::query!("DELETE FROM sections WHERE id = $1", id)
        .execute(db.pool())
        .await?;
    Ok(done.rows_affected() == 1)
}

/// Moves a thread to another category, returning the category it was in,
/// or `None` if there is no such thread in a category. Its visibility
/// follows its new category (a trigger sees to that).
pub async fn move_thread<'c>(
    db: impl Handle<'c>,
    thread_id: i64,
    category_id: i64,
) -> sqlx::Result<Option<i64>> {
    let mut tx = db.open(KEY).begin().await?;
    let Some(previous) = sqlx::query_scalar!(
        r#"SELECT category_id AS "category_id!" FROM threads
           WHERE id = $1 AND category_id IS NOT NULL FOR UPDATE"#,
        thread_id
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(None);
    };
    sqlx::query!(
        "UPDATE threads SET category_id = $2, version = version + 1 WHERE id = $1",
        thread_id,
        category_id
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(previous))
}

#[derive(Clone, Debug)]
pub struct NewThread {
    pub category_id: i64,
    pub title: String,
    pub author_id: i64,
    pub body_md: String,
    pub forked_from_post_id: Option<i64>,
    pub pinned: bool,
    pub locked: bool,
    pub created_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CreatedThread {
    pub thread_id: i64,
    pub first_post_id: i64,
}

/// Creates a thread together with its first post.
pub async fn create_thread<'c>(
    db: impl Handle<'c>,
    thread: &NewThread,
) -> sqlx::Result<CreatedThread> {
    let mut tx = db.open(KEY).begin().await?;
    let created_at = thread.created_at.unwrap_or_else(Utc::now);
    let thread_id = sqlx::query_scalar!(
        r#"INSERT INTO threads (category_id, visibility, title, slug, author_id, forked_from_post_id,
                                pinned, locked, created_at, last_post_at)
           VALUES ($1, 'public', $2, $3, $4, $5, $6, $7, $8, $8)
           RETURNING id"#,
        thread.category_id,
        thread.title,
        slugify(&thread.title),
        thread.author_id,
        thread.forked_from_post_id,
        thread.pinned,
        thread.locked,
        created_at,
    )
    .fetch_one(&mut *tx)
    .await?;
    let first_post_id = insert_post(
        &mut tx,
        &NewPost {
            thread_id,
            author_id: thread.author_id,
            body_md: thread.body_md.clone(),
            reply_to_post_id: None,
            created_at: Some(created_at),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(CreatedThread {
        thread_id,
        first_post_id,
    })
}

#[derive(Clone, Debug)]
pub struct NewPost {
    pub thread_id: i64,
    pub author_id: i64,
    pub body_md: String,
    pub reply_to_post_id: Option<i64>,
    pub created_at: Option<DateTime<Utc>>,
}

/// Adds a post to the end of a thread.
pub async fn create_post<'c>(db: impl Handle<'c>, post: &NewPost) -> sqlx::Result<i64> {
    let mut tx = db.open(KEY).begin().await?;
    let id = insert_post(&mut tx, post).await?;
    tx.commit().await?;
    Ok(id)
}

async fn insert_post(tx: &mut sqlx::PgConnection, post: &NewPost) -> sqlx::Result<i64> {
    let created_at = post.created_at.unwrap_or_else(Utc::now);
    // Taking the next number locks the thread row, so concurrent posts to
    // one thread are numbered one after another.
    let seq = sqlx::query_scalar!(
        r#"UPDATE threads
           SET last_seq = last_seq + 1, last_post_at = greatest(last_post_at, $2), version = version + 1
           WHERE id = $1
           RETURNING last_seq"#,
        post.thread_id,
        created_at,
    )
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query_scalar!(
        r#"INSERT INTO posts (thread_id, seq, author_id, reply_to_post_id, body_md, body_html, created_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7)
           RETURNING id"#,
        post.thread_id,
        seq,
        post.author_id,
        post.reply_to_post_id,
        post.body_md,
        render_post(&mut *tx, &post.body_md).await?.html,
        created_at,
    )
    .fetch_one(&mut *tx)
    .await
}

/// Renders a post body as it will be saved, linking mentions of members
/// who exist now.
pub async fn render_post<'c>(
    db: impl Handle<'c>,
    body_md: &str,
) -> sqlx::Result<markdown::Rendered> {
    let candidates = markdown::mention_candidates(body_md);
    let known: Vec<String> = if candidates.is_empty() {
        Vec::new()
    } else {
        let mut conn = db.open(KEY).acquire().await?;
        sqlx::query_scalar!(
            r#"SELECT username::text AS "username!" FROM users
               WHERE lower(username::text) = ANY($1) AND deleted_at IS NULL"#,
            &candidates
        )
        .fetch_all(&mut *conn)
        .await?
    };
    // Rendering takes long enough that it shouldn't hold up other requests.
    let body_md = body_md.to_owned();
    let rendered = tokio::task::spawn_blocking(move || {
        markdown::render_with_mentions(&body_md, |name| {
            known
                .iter()
                .find(|known| known.eq_ignore_ascii_case(name))
                .cloned()
        })
    })
    .await
    .expect("rendering a post panicked");
    Ok(rendered)
}

/// Edits made this soon after posting replace the original rather than
/// adding to the post's history (DESIGN.md §3.3).
pub const EDIT_GRACE_MINUTES: i32 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edited {
    /// The text was the same, so nothing changed.
    Unchanged,
    /// The original was replaced, within the grace period.
    Replaced,
    /// The previous version was kept as revision `rev`.
    Revised { rev: i32 },
}

/// Replaces a post's text, keeping the version it replaces in
/// `post_revisions` unless the post is still in its grace period and has
/// never been edited since. Doesn't check permissions.
pub async fn edit_post<'c>(
    db: impl Handle<'c>,
    post_id: i64,
    editor_id: i64,
    body_md: &str,
) -> sqlx::Result<Edited> {
    let mut tx = db.open(KEY).begin().await?;
    let post = sqlx::query!(
        r#"SELECT body_md, body_html, author_id, created_at, edited_at, edited_by,
                  created_at > now() - make_interval(mins => $2) AS "in_grace!"
           FROM posts WHERE id = $1 FOR UPDATE"#,
        post_id,
        EDIT_GRACE_MINUTES,
    )
    .fetch_one(&mut *tx)
    .await?;
    if post.body_md == body_md {
        return Ok(Edited::Unchanged);
    }
    let html = render_post(&mut tx, body_md).await?.html;
    let outcome = if post.in_grace && post.edited_at.is_none() && editor_id == post.author_id {
        sqlx::query!(
            "UPDATE posts SET body_md = $2, body_html = $3 WHERE id = $1",
            post_id,
            body_md,
            html
        )
        .execute(&mut *tx)
        .await?;
        Edited::Replaced
    } else {
        let rev = sqlx::query_scalar!(
            r#"INSERT INTO post_revisions (post_id, rev, body_md, body_html, editor_id, created_at)
               SELECT $1, coalesce(max(rev), 0) + 1, $2, $3, $4, $5 FROM post_revisions WHERE post_id = $1
               RETURNING rev"#,
            post_id,
            post.body_md,
            post.body_html,
            post.edited_by.unwrap_or(post.author_id),
            post.edited_at.unwrap_or(post.created_at),
        )
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query!(
            r#"UPDATE posts SET body_md = $2, body_html = $3, edited_at = now(),
                      edited_by = CASE WHEN author_id = $4 THEN NULL ELSE $4 END
               WHERE id = $1"#,
            post_id,
            body_md,
            html,
            editor_id,
        )
        .execute(&mut *tx)
        .await?;
        Edited::Revised { rev }
    };
    tx.commit().await?;
    Ok(outcome)
}

/// Deletes a post, leaving a tombstone. Its number, history and reactions
/// stay in the database but are no longer shown.
pub async fn delete_post<'c>(db: impl Handle<'c>, post_id: i64) -> sqlx::Result<bool> {
    let mut conn = db.open(KEY).acquire().await?;
    let done = sqlx::query!(
        "UPDATE posts SET deleted_at = now() WHERE id = $1 AND deleted_at IS NULL",
        post_id
    )
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Adds a reaction; adding one twice does nothing. Returns whether it was
/// added.
pub async fn add_reaction<'c>(
    db: impl Handle<'c>,
    post_id: i64,
    user_id: i64,
    emoji: &str,
) -> sqlx::Result<bool> {
    let mut conn = db.open(KEY).acquire().await?;
    let done = sqlx::query!(
        "INSERT INTO reactions (post_id, user_id, emoji) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        post_id,
        user_id,
        emoji
    )
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Removes a reaction, returning whether there was one.
pub async fn remove_reaction<'c>(
    db: impl Handle<'c>,
    post_id: i64,
    user_id: i64,
    emoji: &str,
) -> sqlx::Result<bool> {
    let mut conn = db.open(KEY).acquire().await?;
    let done = sqlx::query!(
        "DELETE FROM reactions WHERE post_id = $1 AND user_id = $2 AND emoji = $3",
        post_id,
        user_id,
        emoji
    )
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Moves a member's read marker in a thread forward to `seq`. It never
/// moves back.
pub async fn mark_read<'c>(
    db: impl Handle<'c>,
    user_id: i64,
    thread_id: i64,
    seq: i64,
) -> sqlx::Result<()> {
    let mut conn = db.open(KEY).acquire().await?;
    let seq = i32::try_from(seq.max(0)).unwrap_or(i32::MAX);
    sqlx::query!(
        r#"INSERT INTO thread_reads (user_id, thread_id, last_read_seq) VALUES ($1, $2, $3)
           ON CONFLICT (user_id, thread_id)
           DO UPDATE SET last_read_seq = greatest(thread_reads.last_read_seq, excluded.last_read_seq)
           WHERE thread_reads.last_read_seq < excluded.last_read_seq"#,
        user_id,
        thread_id,
        seq
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Marks every thread in a category, as listed in `thread_ids`, as read up
/// to its last post.
pub async fn mark_threads_read<'c>(
    db: impl Handle<'c>,
    user_id: i64,
    thread_ids: &[i64],
) -> sqlx::Result<()> {
    let mut conn = db.open(KEY).acquire().await?;
    sqlx::query!(
        r#"INSERT INTO thread_reads (user_id, thread_id, last_read_seq)
           SELECT $1, t.id, t.last_seq FROM threads t WHERE t.id = ANY($2)
           ON CONFLICT (user_id, thread_id)
           DO UPDATE SET last_read_seq = greatest(thread_reads.last_read_seq, excluded.last_read_seq)
           WHERE thread_reads.last_read_seq < excluded.last_read_seq"#,
        user_id,
        thread_ids
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Whether text can be offered as a reaction: a short run of symbols,
/// such as an emoji, with no letters, digits, spaces or markup.
pub fn valid_reaction(emoji: &str) -> bool {
    !emoji.is_empty()
        && emoji.chars().count() <= 16
        && emoji.chars().all(|c| {
            !c.is_alphanumeric() && !c.is_whitespace() && !c.is_control() && !"<>&\"'`".contains(c)
        })
}

/// Changes the reaction types, as the admin asked: `add` a new one, `enable`
/// or `disable` one, move it `up` or `down`, or `delete` one nobody has
/// used. At most `max` can be enabled at once. Returns the problem, if any,
/// to show the admin.
pub async fn change_reaction_type<'c>(
    db: impl Handle<'c>,
    action: &str,
    emoji: &str,
    max: usize,
) -> sqlx::Result<Result<(), String>> {
    let mut tx = db.open(KEY).begin().await?;
    // One change at a time, so that the limit and positions hold.
    sqlx::query!("LOCK TABLE reaction_types IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;
    let types = sqlx::query!("SELECT emoji, enabled FROM reaction_types ORDER BY position, emoji")
        .fetch_all(&mut *tx)
        .await?;
    let enabled = types.iter().filter(|t| t.enabled).count();
    let index = types.iter().position(|t| t.emoji == emoji);
    let full = format!("At most {max} reactions can be in use at once. Turn one off first.");
    let problem = match (action, index) {
        ("add", Some(_)) => Some("That reaction is already in the list.".to_owned()),
        ("add", None) if !valid_reaction(emoji) => {
            Some("A reaction should be an emoji or a symbol of a few characters, with no letters or spaces.".to_owned())
        }
        ("add", None) if enabled >= max => Some(full),
        ("add", None) => {
            sqlx::query!(
                "INSERT INTO reaction_types (emoji, position)
                 SELECT $1, coalesce(max(position), 0) + 1 FROM reaction_types",
                emoji
            )
            .execute(&mut *tx)
            .await?;
            None
        }
        ("enable", Some(i)) if !types[i].enabled && enabled >= max => Some(full),
        ("enable" | "disable", Some(_)) => {
            sqlx::query!(
                "UPDATE reaction_types SET enabled = $2 WHERE emoji = $1",
                emoji,
                action == "enable"
            )
            .execute(&mut *tx)
            .await?;
            None
        }
        ("up" | "down", Some(i)) => {
            let other = if action == "up" { i.checked_sub(1) } else { Some(i + 1) };
            if let Some(other) = other.filter(|&o| o < types.len()) {
                let mut order: Vec<&str> = types.iter().map(|t| t.emoji.as_str()).collect();
                order.swap(i, other);
                for (position, emoji) in order.iter().enumerate() {
                    sqlx::query!(
                        "UPDATE reaction_types SET position = $2 WHERE emoji = $1",
                        emoji,
                        position as i32 + 1
                    )
                    .execute(&mut *tx)
                    .await?;
                }
            }
            None
        }
        ("delete", Some(_)) => {
            let used = sqlx::query_scalar!(
                r#"SELECT EXISTS (SELECT 1 FROM reactions WHERE emoji = $1) AS "used!""#,
                emoji
            )
            .fetch_one(&mut *tx)
            .await?;
            if used {
                Some("Members have used that reaction, so it can only be turned off.".to_owned())
            } else {
                sqlx::query!("DELETE FROM reaction_types WHERE emoji = $1", emoji)
                    .execute(&mut *tx)
                    .await?;
                None
            }
        }
        _ => Some("Choose a reaction from the list.".to_owned()),
    };
    if problem.is_none() {
        tx.commit().await?;
    }
    Ok(problem.map_or(Ok(()), Err))
}

#[cfg(test)]
mod tests {
    use super::slugify;
    use hegel::TestCase;
    use hegel::generators as gs;

    #[test]
    fn slugs_are_readable() {
        assert_eq!(
            slugify("Do solo modes count as ‘playing’ a game?"),
            "do-solo-modes-count-as-playing-a-game"
        );
        assert_eq!(
            slugify("What did you play this week? (September)"),
            "what-did-you-play-this-week-september"
        );
        assert_eq!(slugify("  ?? "), "thread");
        assert_eq!(slugify("Ünïcødé"), "n-c-d");
        assert_eq!(slugify("Unread!"), "unread-thread");
        assert_eq!(slugify("Reply"), "reply-thread");
    }

    #[hegel::test(test_cases = 500)]
    fn slugs_are_url_safe_and_stable(tc: TestCase) {
        let title = tc.draw(gs::text());
        let slug = slugify(&title);
        assert!(!slug.is_empty() && slug.len() <= 67, "{slug:?}");
        assert_ne!(slug, "unread");
        assert_ne!(slug, "reply");
        assert!(
            !slug.starts_with('-') && !slug.ends_with('-') && !slug.contains("--"),
            "{slug:?}"
        );
        assert!(
            slug.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "{slug:?}"
        );
        assert_eq!(slugify(&slug), slug);
    }
}
