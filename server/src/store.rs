//! Low-level writes for the content model: creating users, sections,
//! categories, threads and posts with their bookkeeping (post numbering,
//! thread activity, rendered HTML) kept consistent.
//!
//! These functions don't check permissions or clear the page cache. Request
//! handlers that write (M4 onwards) check permissions first and then call
//! [`crate::AppState::changed`] once the write has committed. The `seed`
//! command uses them directly.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

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
    // `/t/{id}/unread` is a route of its own.
    if RESERVED_SLUGS.contains(&slug.as_str()) {
        slug.push_str("-thread");
    }
    slug
}

const RESERVED_SLUGS: &[&str] = &["unread"];

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

pub async fn create_user(db: &PgPool, user: &NewUser) -> sqlx::Result<i64> {
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
    .fetch_one(db)
    .await
}

pub async fn create_section(db: &PgPool, name: &str, position: i32) -> sqlx::Result<i64> {
    sqlx::query_scalar!(
        "INSERT INTO sections (name, position) VALUES ($1, $2) RETURNING id",
        name,
        position
    )
    .fetch_one(db)
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

pub async fn create_category(db: &PgPool, category: &NewCategory) -> sqlx::Result<i64> {
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
    .fetch_one(db)
    .await
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
pub async fn create_thread(db: &PgPool, thread: &NewThread) -> sqlx::Result<CreatedThread> {
    let mut tx = db.begin().await?;
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
pub async fn create_post(db: &PgPool, post: &NewPost) -> sqlx::Result<i64> {
    let mut tx = db.begin().await?;
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
        markdown::render(&post.body_md),
        created_at,
    )
    .fetch_one(&mut *tx)
    .await
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
    }

    #[hegel::test(test_cases = 500)]
    fn slugs_are_url_safe_and_stable(tc: TestCase) {
        let title = tc.draw(gs::text());
        let slug = slugify(&title);
        assert!(!slug.is_empty() && slug.len() <= 67, "{slug:?}");
        assert_ne!(slug, "unread");
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
