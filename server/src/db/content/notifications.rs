//! A member's notifications, as they are allowed to see them now.
//!
//! A notification only records which post it is about. Whether the member
//! may still read that post is decided here, every time, with the same
//! visibility condition as everything else: a post that has since been
//! deleted, hidden or moved somewhere the member can't see takes its
//! notification with it (it isn't listed or counted).

use super::*;

pub const NOTIFICATIONS_PER_PAGE: i64 = 30;

/// What a notification is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Someone replied to one of the member's posts.
    Reply,
    /// Someone mentioned the member.
    Mention,
    /// A new message in one of the member's conversations.
    Message,
    /// A new post in a thread the member watches.
    WatchedThread,
    /// A new thread in a category the member watches.
    WatchedCategory,
    /// A note from the moderators.
    Moderation,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Reply => "reply",
            Kind::Mention => "mention",
            Kind::Message => "message",
            Kind::WatchedThread => "watched_thread",
            Kind::WatchedCategory => "watched_category",
            Kind::Moderation => "moderation",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "reply" => Kind::Reply,
            "mention" => Kind::Mention,
            "message" => Kind::Message,
            "watched_thread" => Kind::WatchedThread,
            "watched_category" => Kind::WatchedCategory,
            "moderation" => Kind::Moderation,
            _ => return None,
        })
    }

    /// How it reads after the actor's name, as in "Ruth replied to you in".
    pub fn verb(self) -> &'static str {
        match self {
            Kind::Reply => "replied to you in",
            Kind::Mention => "mentioned you in",
            Kind::Message => "sent you a message in",
            Kind::WatchedThread => "posted in",
            Kind::WatchedCategory => "started a new thread,",
            Kind::Moderation => "",
        }
    }
}

/// The post a notification is about.
pub struct About {
    pub post_id: i64,
    pub location: PostLocation,
    /// The thread's title, or for an untitled conversation a description.
    pub title: String,
}

pub struct NotificationRow {
    pub id: i64,
    pub kind: Kind,
    pub actor: Option<UserRef>,
    pub created_at: DateTime<Utc>,
    pub read: bool,
    pub about: Option<About>,
    /// For a moderation notice, what the moderators said.
    pub message: Option<String>,
    /// For a moderation notice about an action the member may contest,
    /// that action.
    pub mod_action_id: Option<i64>,
}

pub struct NotificationList {
    pub total: i64,
    pub rows: Vec<NotificationRow>,
}

/// One page of the viewer's notifications, newest first.
pub async fn list(db: &Db, viewer: &Viewer, page: i64) -> sqlx::Result<NotificationList> {
    let Some(user_id) = viewer.user_id() else {
        return Ok(NotificationList {
            total: 0,
            rows: Vec::new(),
        });
    };
    let filter = viewer.filter();
    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) AS "n!"
        FROM notifications n
        LEFT JOIN posts p ON p.id = n.post_id
        LEFT JOIN threads t ON t.id = p.thread_id
        WHERE n.user_id = $1
          AND (n.post_id IS NULL
               OR (p.deleted_at IS NULL AND p.hidden_at IS NULL AND t.deleted_at IS NULL
                   AND (thread_readable(t.visibility, t.category_id, $2, $3) OR dm_readable(t.id, t.visibility, $4))))
          AND NOT EXISTS (SELECT 1 FROM blocks b JOIN users bu ON bu.id = b.blocked_user_id
                          WHERE b.user_id = $1 AND b.blocked_user_id = n.actor_id AND bu.role = 'user')
        "#,
        user_id,
        &filter.visibilities,
        &filter.group_category_ids,
        filter.dm_viewer,
    )
    .fetch_one(db.pool())
    .await?;
    let Some(offset) = page_offset(page, NOTIFICATIONS_PER_PAGE) else {
        return Ok(NotificationList {
            total,
            rows: Vec::new(),
        });
    };
    let rows = sqlx::query!(
        r#"
        SELECT n.id, n.kind, n.created_at, n.read_at IS NOT NULL AS "read!", n.message, n.mod_action_id,
               a.id AS "actor_id?", a.username AS "actor_username?", a.display_name AS "actor_name?",
               p.id AS "post_id?", p.seq AS "seq?", t.id AS "thread_id?", t.title AS "title?",
               t.slug AS "slug?", t.visibility = 'private' AS "private?"
        FROM notifications n
        LEFT JOIN users a ON a.id = n.actor_id
        LEFT JOIN posts p ON p.id = n.post_id
        LEFT JOIN threads t ON t.id = p.thread_id
        WHERE n.user_id = $1
          AND (n.post_id IS NULL
               OR (p.deleted_at IS NULL AND p.hidden_at IS NULL AND t.deleted_at IS NULL
                   AND (thread_readable(t.visibility, t.category_id, $2, $3) OR dm_readable(t.id, t.visibility, $6))))
          AND NOT EXISTS (SELECT 1 FROM blocks b JOIN users bu ON bu.id = b.blocked_user_id
                          WHERE b.user_id = $1 AND b.blocked_user_id = n.actor_id AND bu.role = 'user')
        ORDER BY n.created_at DESC, n.id DESC
        LIMIT $4 OFFSET $5
        "#,
        user_id,
        &filter.visibilities,
        &filter.group_category_ids,
        NOTIFICATIONS_PER_PAGE,
        offset,
        filter.dm_viewer,
    )
    .fetch_all(db.pool())
    .await?;
    Ok(NotificationList {
        total,
        rows: rows
            .into_iter()
            .map(|row| NotificationRow {
                id: row.id,
                kind: Kind::parse(&row.kind).unwrap_or(Kind::Moderation),
                actor: user_ref(
                    row.actor_id,
                    row.actor_username.map(|u| u.to_string()),
                    row.actor_name,
                ),
                created_at: row.created_at,
                read: row.read,
                about: match (
                    row.post_id,
                    row.seq,
                    row.thread_id,
                    row.title,
                    row.slug,
                    row.private,
                ) {
                    (
                        Some(post_id),
                        Some(seq),
                        Some(thread_id),
                        Some(title),
                        Some(slug),
                        Some(private),
                    ) => Some(About {
                        post_id,
                        title: if title.is_empty() {
                            "a private conversation".to_owned()
                        } else {
                            title
                        },
                        location: PostLocation {
                            thread_id,
                            thread_slug: slug,
                            seq: i64::from(seq),
                            private,
                        },
                    }),
                    _ => None,
                },
                message: row.message,
                mod_action_id: row.mod_action_id,
            })
            .collect(),
    })
}

/// How many unread notifications the viewer has that they may see: the
/// number on the badge in the page header.
pub async fn unread_count(db: &Db, viewer: &Viewer) -> sqlx::Result<i64> {
    let Some(user_id) = viewer.user_id() else {
        return Ok(0);
    };
    let filter = viewer.filter();
    sqlx::query_scalar!(
        r#"
        SELECT count(*) AS "n!"
        FROM notifications n
        LEFT JOIN posts p ON p.id = n.post_id
        LEFT JOIN threads t ON t.id = p.thread_id
        WHERE n.user_id = $1 AND n.read_at IS NULL
          AND (n.post_id IS NULL
               OR (p.deleted_at IS NULL AND p.hidden_at IS NULL AND t.deleted_at IS NULL
                   AND (thread_readable(t.visibility, t.category_id, $2, $3) OR dm_readable(t.id, t.visibility, $4))))
          AND NOT EXISTS (SELECT 1 FROM blocks b JOIN users bu ON bu.id = b.blocked_user_id
                          WHERE b.user_id = $1 AND b.blocked_user_id = n.actor_id AND bu.role = 'user')
        "#,
        user_id,
        &filter.visibilities,
        &filter.group_category_ids,
        filter.dm_viewer,
    )
    .fetch_one(db.pool())
    .await
}

/// Where one of the viewer's notifications leads, if they may still see
/// it: the post, or for a moderation notice `None` inside `Some`.
pub async fn target(
    db: &Db,
    viewer: &Viewer,
    notification_id: i64,
) -> sqlx::Result<Option<Option<(i64, PostLocation)>>> {
    let Some(user_id) = viewer.user_id() else {
        return Ok(None);
    };
    let Some(row) = sqlx::query!(
        r#"SELECT post_id FROM notifications WHERE id = $1 AND user_id = $2"#,
        notification_id,
        user_id
    )
    .fetch_optional(db.pool())
    .await?
    else {
        return Ok(None);
    };
    match row.post_id {
        None => Ok(Some(None)),
        Some(post_id) => Ok(post_location(db, viewer, post_id)
            .await?
            .map(|location| Some((post_id, location)))),
    }
}
