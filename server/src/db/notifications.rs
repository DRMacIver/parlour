//! Storing notifications, and the facts deciding who gets one (DESIGN.md
//! §9). Which notifications a member is shown is decided in
//! [`crate::content::notifications`]; who is notified, after checking each
//! candidate can read the post, in [`crate::notify`].

use chrono::{DateTime, Utc};

use crate::content::notifications::Kind;
use crate::db::{Db, Handle, KEY};
use crate::social::WatchLevel;

/// What a new post means for notifications.
#[derive(Clone, Debug)]
pub struct PostFacts {
    pub post_id: i64,
    pub thread_id: i64,
    /// `None` for a conversation.
    pub category_id: Option<i64>,
    pub seq: i64,
    pub author_id: i64,
    pub author_is_staff: bool,
    /// The author of the post this one replies to.
    pub replied_to: Option<i64>,
    pub body_md: String,
    /// Whether the post is still there to notify about.
    pub live: bool,
}

pub async fn post_facts(db: &Db, post_id: i64) -> sqlx::Result<Option<PostFacts>> {
    let row = sqlx::query!(
        r#"SELECT p.id, p.thread_id, t.category_id, p.seq, p.author_id, a.role <> 'user' AS "staff!",
                  r.author_id AS "replied_to?", p.body_md,
                  p.deleted_at IS NULL AND p.hidden_at IS NULL AND t.deleted_at IS NULL AS "live!"
           FROM posts p
           JOIN threads t ON t.id = p.thread_id
           JOIN users a ON a.id = p.author_id
           LEFT JOIN posts r ON r.id = p.reply_to_post_id
           WHERE p.id = $1"#,
        post_id
    )
    .fetch_optional(db.pool())
    .await?;
    Ok(row.map(|row| PostFacts {
        post_id: row.id,
        thread_id: row.thread_id,
        category_id: row.category_id,
        seq: i64::from(row.seq),
        author_id: row.author_id,
        author_is_staff: row.staff,
        replied_to: row.replied_to,
        body_md: row.body_md,
        live: row.live,
    }))
}

/// The members watching a thread or category at some level other than
/// normal, with their levels.
pub async fn watchers(
    db: &Db,
    thread_id: i64,
    category_id: Option<i64>,
) -> sqlx::Result<Vec<(i64, Option<WatchLevel>, Option<WatchLevel>)>> {
    let rows = sqlx::query!(
        r#"SELECT u AS "user_id!",
                  (SELECT level FROM watches w WHERE w.user_id = u AND w.thread_id = $1) AS thread_level,
                  (SELECT level FROM watches w WHERE w.user_id = u AND w.category_id = $2) AS category_level
           FROM (SELECT user_id FROM watches WHERE thread_id = $1
                 UNION SELECT user_id FROM watches WHERE category_id = $2) AS w(u)"#,
        thread_id,
        category_id,
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.user_id,
                row.thread_level.and_then(|l| WatchLevel::parse(&l)),
                row.category_level.and_then(|l| WatchLevel::parse(&l)),
            )
        })
        .collect())
}

/// The current participants of a conversation.
pub async fn participants(db: &Db, thread_id: i64) -> sqlx::Result<Vec<i64>> {
    sqlx::query_scalar!(
        "SELECT user_id FROM private_thread_participants WHERE thread_id = $1 AND left_at IS NULL",
        thread_id
    )
    .fetch_all(db.pool())
    .await
}

/// The members with these usernames.
pub async fn ids_named(db: &Db, usernames: &[String]) -> sqlx::Result<Vec<i64>> {
    let lower: Vec<String> = usernames.iter().map(|n| n.to_lowercase()).collect();
    sqlx::query_scalar!(
        "SELECT id FROM users WHERE lower(username::text) = ANY($1) AND deleted_at IS NULL",
        &lower
    )
    .fetch_all(db.pool())
    .await
}

/// The members who have blocked `user_id`.
pub async fn blocked_by(db: &Db, user_id: i64) -> sqlx::Result<Vec<i64>> {
    sqlx::query_scalar!(
        "SELECT user_id FROM blocks WHERE blocked_user_id = $1",
        user_id
    )
    .fetch_all(db.pool())
    .await
}

/// Adds a notification about a post, unless the member already has one
/// about it. Returns its id if it is new.
pub async fn insert<'c>(
    db: impl Handle<'c>,
    user_id: i64,
    kind: Kind,
    actor_id: i64,
    thread_id: i64,
    post_id: i64,
) -> sqlx::Result<Option<i64>> {
    let mut conn = db.open(KEY).acquire().await?;
    sqlx::query_scalar!(
        r#"INSERT INTO notifications (user_id, kind, actor_id, thread_id, post_id)
           VALUES ($1, $2, $3, $4, $5)
           ON CONFLICT (user_id, post_id) WHERE post_id IS NOT NULL DO NOTHING
           RETURNING id"#,
        user_id,
        kind.as_str(),
        actor_id,
        thread_id,
        post_id,
    )
    .fetch_optional(&mut *conn)
    .await
}

/// Adds a note from the moderators.
pub async fn insert_moderation<'c>(
    db: impl Handle<'c>,
    user_id: i64,
    actor_id: Option<i64>,
    message: &str,
    action_id: Option<i64>,
) -> sqlx::Result<i64> {
    let mut conn = db.open(KEY).acquire().await?;
    sqlx::query_scalar!(
        r#"INSERT INTO notifications (user_id, kind, actor_id, message, mod_action_id)
           VALUES ($1, 'moderation', $2, $3, $4) RETURNING id"#,
        user_id,
        actor_id,
        message,
        action_id,
    )
    .fetch_one(&mut *conn)
    .await
}

/// Marks a member's notifications about posts up to `seq` in a thread as
/// read, as when they have been shown that page. Returns how many were
/// unread.
pub async fn mark_read_in_thread(
    db: &Db,
    user_id: i64,
    thread_id: i64,
    seq: i64,
) -> sqlx::Result<u64> {
    let seq = i32::try_from(seq.max(0)).unwrap_or(i32::MAX);
    let done = sqlx::query!(
        r#"UPDATE notifications n SET read_at = now()
           FROM posts p
           WHERE n.user_id = $1 AND n.thread_id = $2 AND n.read_at IS NULL
             AND p.id = n.post_id AND p.seq <= $3"#,
        user_id,
        thread_id,
        seq,
    )
    .execute(db.pool())
    .await?;
    Ok(done.rows_affected())
}

/// Marks one notification read. Returns whether it was the member's and
/// unread.
pub async fn mark_read(db: &Db, user_id: i64, id: i64) -> sqlx::Result<bool> {
    let done = sqlx::query!(
        "UPDATE notifications SET read_at = now() WHERE id = $1 AND user_id = $2 AND read_at IS NULL",
        id,
        user_id
    )
    .execute(db.pool())
    .await?;
    Ok(done.rows_affected() == 1)
}

pub async fn mark_all_read(db: &Db, user_id: i64) -> sqlx::Result<u64> {
    let done = sqlx::query!(
        "UPDATE notifications SET read_at = now() WHERE user_id = $1 AND read_at IS NULL",
        user_id
    )
    .execute(db.pool())
    .await?;
    Ok(done.rows_affected())
}

// ---------------------------------------------------------------------------
// Email

/// What the email job needs to know about a member.
pub struct Addressee {
    pub email: String,
    pub display_name: String,
    pub verified: bool,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub deleted: bool,
}

pub async fn addressee(db: &Db, user_id: i64) -> sqlx::Result<Option<Addressee>> {
    let row = sqlx::query!(
        r#"SELECT email, display_name, email_verified_at IS NOT NULL AS "verified!", last_seen_at,
                  deleted_at IS NOT NULL AS "deleted!"
           FROM users WHERE id = $1"#,
        user_id
    )
    .fetch_optional(db.pool())
    .await?;
    Ok(row.map(|row| Addressee {
        email: row.email.to_string(),
        display_name: row.display_name,
        verified: row.verified,
        last_seen_at: row.last_seen_at,
        deleted: row.deleted,
    }))
}

/// A notification waiting to be emailed.
#[derive(Clone, Debug)]
pub struct Pending {
    pub id: i64,
    pub user_id: i64,
    pub kind: Kind,
    pub created_at: DateTime<Utc>,
    pub read: bool,
    pub emailed: bool,
}

pub async fn pending(db: &Db, id: i64) -> sqlx::Result<Option<Pending>> {
    let row = sqlx::query!(
        r#"SELECT id, user_id, kind, created_at, read_at IS NOT NULL AS "read!",
                  emailed_at IS NOT NULL AS "emailed!"
           FROM notifications WHERE id = $1"#,
        id
    )
    .fetch_optional(db.pool())
    .await?;
    Ok(row.map(|row| Pending {
        id: row.id,
        user_id: row.user_id,
        kind: Kind::parse(&row.kind).unwrap_or(Kind::Moderation),
        created_at: row.created_at,
        read: row.read,
        emailed: row.emailed,
    }))
}

/// A notification that could go in an email.
#[derive(Clone, Debug)]
pub struct Due {
    pub id: i64,
    pub kind: Kind,
    pub post_id: Option<i64>,
    pub message: Option<String>,
    pub created_at: DateTime<Utc>,
    /// The member has since blocked whoever it is from, or muted where it
    /// happened, so it isn't to be emailed (nor shown, for a block).
    pub quiet: bool,
}

/// Claims a member's unread notifications that haven't been emailed and
/// were made no later than `due`, oldest first: the ones one email can
/// cover. They are marked as dealt with by email, so call this in the
/// transaction that queues the email (or that decides not to send one):
/// a concurrent claim waits for that transaction and then finds nothing,
/// so no two emails cover the same notification.
pub async fn claim_for_email<'c>(
    db: impl Handle<'c>,
    user_id: i64,
    due: DateTime<Utc>,
) -> sqlx::Result<Vec<Due>> {
    let mut conn = db.open(KEY).acquire().await?;
    let rows = sqlx::query!(
        r#"
        WITH claimed AS (
            UPDATE notifications SET emailed_at = now()
            WHERE user_id = $1 AND read_at IS NULL AND emailed_at IS NULL AND created_at <= $2
            RETURNING id, kind, post_id, message, created_at, actor_id
        )
        SELECT c.id, c.kind, c.post_id, c.message, c.created_at,
               (EXISTS (SELECT 1 FROM blocks b JOIN users bu ON bu.id = b.blocked_user_id
                        WHERE b.user_id = $1 AND b.blocked_user_id = c.actor_id AND bu.role = 'user')
                OR coalesce((SELECT coalesce(tw.level, cw.level) = 'muted'
                             FROM posts p
                             JOIN threads t ON t.id = p.thread_id
                             LEFT JOIN watches tw ON tw.user_id = $1 AND tw.thread_id = t.id
                             LEFT JOIN watches cw ON cw.user_id = $1 AND cw.category_id = t.category_id
                             WHERE p.id = c.post_id), false)) AS "quiet!"
        FROM claimed c
        ORDER BY c.created_at, c.id
        "#,
        user_id,
        due
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| Due {
            id: row.id,
            kind: Kind::parse(&row.kind).unwrap_or(Kind::Moderation),
            post_id: row.post_id,
            message: row.message,
            created_at: row.created_at,
            quiet: row.quiet,
        })
        .collect())
}
