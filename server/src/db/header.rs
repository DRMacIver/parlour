//! What the header of every page shows, fetched in one query.

use crate::db::Db;
use crate::db::settings;
use crate::viewer::Viewer;

pub struct Header {
    pub site_name: String,
    /// For a member, their unread notifications they may still see.
    pub notifications: i64,
    /// For a member, conversations with messages they haven't read.
    pub messages: i64,
    /// For staff, what is waiting in the moderation queue.
    pub queue: Option<i64>,
    /// For a silenced or suspended member, the action to contest.
    pub restriction_action: Option<i64>,
}

/// The header for `viewer`. The counts use the same functions (migration
/// 0020) as the badge stream and the notifications pages.
pub async fn load(db: &Db, viewer: &Viewer) -> sqlx::Result<Header> {
    let filter = viewer.filter();
    let restricted = viewer.user().is_some_and(|user| user.is_silenced());
    let row = sqlx::query!(
        r#"SELECT (SELECT value FROM settings WHERE key = 'site_name') AS site_name,
                  CASE WHEN $1::bigint IS NULL THEN 0
                       ELSE unread_notification_count($1, $2, $3, $4) END AS "notifications!",
                  CASE WHEN $4::bigint IS NULL THEN 0
                       ELSE unread_conversation_count($4) END AS "messages!",
                  CASE WHEN $5 THEN moderation_queue_size() END AS queue,
                  CASE WHEN $6 THEN current_restriction_action($1) END AS restriction_action"#,
        viewer.user_id(),
        &filter.visibilities,
        &filter.group_category_ids,
        filter.dm_viewer,
        viewer.is_staff(),
        restricted,
    )
    .fetch_one(db.pool())
    .await?;
    Ok(Header {
        site_name: settings::site_name_from(
            row.site_name
                .and_then(|value| serde_json::from_value(value).ok()),
        ),
        notifications: row.notifications,
        messages: row.messages,
        queue: row.queue,
        restriction_action: row.restriction_action,
    })
}
