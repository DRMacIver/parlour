//! A member's notifications, as they are allowed to see them now.
//!
//! A notification only records which post it is about. Whether the member
//! may still read that post is decided here, every time, with the same
//! visibility condition as everything else: a post that has since been
//! deleted, hidden or moved somewhere the member can't see takes its
//! notification with it (it isn't listed or counted).
//!
//! A notice that a moderator broke glass is the exception: it is for the
//! member it was sent to for good, whether or not they can still read what
//! it is about, as a change of who may read something tells those it
//! shuts out (DESIGN.md §8). What it may name is decided in
//! [`super::access::about`].

use super::*;

pub const NOTIFICATIONS_PER_PAGE: i64 = 30;

/// What a notification is about: the only kinds there are (DESIGN.md §9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Someone mentioned the member in a post they can read.
    Mention,
    /// A new post in a thread the member is subscribed to.
    Subscription,
    /// A new message in one of the member's conversations.
    Message,
    /// A notice about the member or their posts.
    Moderation,
    /// A moderator broke glass on a thread, space or conversation the
    /// member could read then (DESIGN.md §8).
    StaffAccess,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Mention => "mention",
            Kind::Subscription => "subscription",
            Kind::Message => "message",
            Kind::Moderation => "moderation",
            Kind::StaffAccess => "staff_access",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "mention" => Kind::Mention,
            "subscription" => Kind::Subscription,
            "message" => Kind::Message,
            "moderation" => Kind::Moderation,
            "staff_access" => Kind::StaffAccess,
            _ => return None,
        })
    }

    /// How it reads after the actor's name, as in "Ruth posted in".
    pub fn verb(self) -> &'static str {
        match self {
            Kind::Mention => "mentioned you in",
            Kind::Subscription => "posted in",
            Kind::Message => "sent you a message in",
            Kind::Moderation | Kind::StaffAccess => "",
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
    /// For a notice that a moderator broke glass, what on.
    pub access: Option<super::access::About>,
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
          AND CASE
              WHEN n.post_id IS NULL THEN true
              ELSE p.deleted_at IS NULL AND p.hidden_at IS NULL AND t.deleted_at IS NULL
                   AND (thread_readable(t, $2, $3, $4, $5) OR dm_readable(t.id, t.visibility, $4))
              END
          AND NOT EXISTS (SELECT 1 FROM blocks b JOIN users bu ON bu.id = b.blocked_user_id
                          WHERE b.user_id = $1 AND b.blocked_user_id = n.actor_id AND bu.role = 'user')
        "#,
        user_id,
        &filter.visibilities,
        &filter.restricted_space_ids,
        filter.dm_viewer,
        &filter.group_ids,
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
               n.staff_access_id,
               a.id AS "actor_id?", a.username AS "actor_username?", a.display_name AS "actor_name?",
               p.id AS "post_id?", p.seq AS "seq?", t.id AS "thread_id?", t.title AS "title?",
               t.slug AS "slug?", t.visibility = 'private' AS "private?"
        FROM notifications n
        LEFT JOIN users a ON a.id = n.actor_id
        LEFT JOIN posts p ON p.id = n.post_id
        LEFT JOIN threads t ON t.id = p.thread_id
        WHERE n.user_id = $1
          AND CASE
              WHEN n.post_id IS NULL THEN true
              ELSE p.deleted_at IS NULL AND p.hidden_at IS NULL AND t.deleted_at IS NULL
                   AND (thread_readable(t, $2, $3, $6, $7) OR dm_readable(t.id, t.visibility, $6))
              END
          AND NOT EXISTS (SELECT 1 FROM blocks b JOIN users bu ON bu.id = b.blocked_user_id
                          WHERE b.user_id = $1 AND b.blocked_user_id = n.actor_id AND bu.role = 'user')
        ORDER BY n.created_at DESC, n.id DESC
        LIMIT $4 OFFSET $5
        "#,
        user_id,
        &filter.visibilities,
        &filter.restricted_space_ids,
        NOTIFICATIONS_PER_PAGE,
        offset,
        filter.dm_viewer,
        &filter.group_ids,
    )
    .fetch_all(db.pool())
    .await?;
    let mut abouts = HashMap::new();
    for id in rows.iter().filter_map(|row| row.staff_access_id) {
        if let Some(about) = super::access::about(db, viewer, id).await? {
            abouts.insert(id, about);
        }
    }
    Ok(NotificationList {
        total,
        rows: rows
            .into_iter()
            .map(|row| NotificationRow {
                access: row.staff_access_id.and_then(|id| abouts.remove(&id)),
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
        r#"SELECT unread_notification_count($1, $2, $3, $4, $5) AS "n!""#,
        user_id,
        &filter.visibilities,
        &filter.restricted_space_ids,
        filter.dm_viewer,
        &filter.group_ids,
    )
    .fetch_one(db.pool())
    .await
}

/// Where a notification leads.
pub enum Leads {
    /// To a post.
    Post(i64, PostLocation),
    /// To the page of what a moderator broke glass on.
    Page(String),
    /// Nowhere: a notice from the moderators is read where it is listed.
    Nowhere,
}

/// Where one of the viewer's notifications leads, if they may still see
/// it.
pub async fn target(db: &Db, viewer: &Viewer, notification_id: i64) -> sqlx::Result<Option<Leads>> {
    let Some(user_id) = viewer.user_id() else {
        return Ok(None);
    };
    let Some(row) = sqlx::query!(
        r#"SELECT post_id, staff_access_id FROM notifications WHERE id = $1 AND user_id = $2"#,
        notification_id,
        user_id
    )
    .fetch_optional(db.pool())
    .await?
    else {
        return Ok(None);
    };
    match (row.post_id, row.staff_access_id) {
        (_, Some(access_id)) => Ok(super::access::about(db, viewer, access_id)
            .await?
            .map(|about| Leads::Page(about.href))),
        (Some(post_id), None) => Ok(post_location(db, viewer, post_id)
            .await?
            .map(|location| Leads::Post(post_id, location))),
        (None, None) => Ok(Some(Leads::Nowhere)),
    }
}
