//! A member's own data, for them to download (DESIGN.md §2): their
//! account and profile, everything they wrote (with earlier versions),
//! their posts held for approval, reactions, conversations,
//! notifications, invites, site groups, their lists (with who is on them)
//! and the lists they are on (with whose they are), blocks, thread
//! subscriptions, how they have
//! arranged their front page and what they have muted, uploads, reports,
//! sessions and the addresses recorded for them.
//!
//! Other people's words are not included, not even in conversations the
//! member is in. The title of a thread is included only if the member may
//! read the thread now, judged by the shared visibility condition. A
//! conversation lists the people in it now, or, if the member has left
//! it, the people in it when they left: who joined later is none of their
//! business.
//!
//! Posts held for approval are included whatever became of them, and so
//! are the member's notifications (without who caused them, which may be
//! in a thread the member can no longer read).

use serde_json::{Value, json};

use crate::db::{Db, layouts, lists};
use crate::viewer::Viewer;

pub async fn export(db: &Db, viewer: &Viewer) -> sqlx::Result<Option<Value>> {
    let Some(user_id) = viewer.user_id() else {
        return Ok(None);
    };
    let filter = viewer.filter();
    let pool = db.pool();
    let account = sqlx::query!(
        r#"SELECT u.username AS "username!: String", u.display_name AS "display_name!",
                  u.email AS "email!: String", u.email_verified_at, u.role AS "role!",
                  u.title, u.pronouns, u.status_line, u.location,
                  u.created_at AS "created_at!", u.last_seen_at,
                  u.dm_policy AS "dm_policy!", u.email_prefs AS "email_prefs!",
                  i.username AS "invited_by?: String", av.sha256 AS "avatar_sha256?", av.ext AS "avatar_ext?"
           FROM users u
           LEFT JOIN users i ON i.id = u.invited_by_user_id
           LEFT JOIN uploads av ON av.id = u.avatar_upload_id
           WHERE u.id = $1"#,
        user_id
    )
    .fetch_one(pool)
    .await?;

    let posts = sqlx::query!(
        r#"SELECT p.id, p.thread_id, p.seq, p.created_at, p.edited_at, p.body_md,
                  p.deleted_at IS NOT NULL AS "deleted!", p.hidden_at IS NOT NULL AS "hidden!",
                  t.visibility = 'private' AS "conversation!",
                  CASE WHEN t.deleted_at IS NULL
                        AND (thread_readable(t.visibility, t.space_id, $2, $3)
                             OR dm_readable(t.id, t.visibility, $4))
                       THEN t.title END AS thread_title
           FROM posts p JOIN threads t ON t.id = p.thread_id
           WHERE p.author_id = $1
           ORDER BY p.created_at, p.id"#,
        user_id,
        &filter.visibilities,
        &filter.restricted_space_ids,
        filter.dm_viewer,
    )
    .fetch_all(pool)
    .await?;
    let revisions = sqlx::query!(
        r#"SELECT r.post_id, r.rev, r.created_at, r.body_md
           FROM post_revisions r JOIN posts p ON p.id = r.post_id
           WHERE p.author_id = $1 AND r.redacted_at IS NULL
           ORDER BY r.post_id, r.rev"#,
        user_id
    )
    .fetch_all(pool)
    .await?;
    let conversations = sqlx::query!(
        r#"SELECT t.id, t.title, me.joined_at, me.left_at,
                  array(SELECT u.username::text FROM private_thread_participants o
                        JOIN users u ON u.id = o.user_id
                        WHERE o.thread_id = t.id
                          AND CASE WHEN me.left_at IS NULL THEN o.left_at IS NULL
                                   ELSE o.joined_at <= me.left_at
                                        AND (o.left_at IS NULL OR o.left_at >= me.left_at) END
                        ORDER BY o.joined_at, u.id) AS "people!"
           FROM private_thread_participants me JOIN threads t ON t.id = me.thread_id
           WHERE me.user_id = $1
           ORDER BY me.joined_at, t.id"#,
        user_id
    )
    .fetch_all(pool)
    .await?;
    let held = sqlx::query!(
        r#"SELECT h.created_at, h.title, h.body_md, h.status, h.decided_at, h.post_id, h.thread_id,
                  CASE WHEN t.deleted_at IS NULL
                        AND (thread_readable(t.visibility, t.space_id, $2, $3)
                             OR dm_readable(t.id, t.visibility, $4))
                       THEN t.title END AS thread_title
           FROM held_posts h LEFT JOIN threads t ON t.id = h.thread_id
           WHERE h.author_id = $1
           ORDER BY h.created_at, h.id"#,
        user_id,
        &filter.visibilities,
        &filter.restricted_space_ids,
        filter.dm_viewer,
    )
    .fetch_all(pool)
    .await?;
    let notifications = sqlx::query!(
        "SELECT kind, thread_id, post_id, message, created_at, read_at, emailed_at
         FROM notifications WHERE user_id = $1 ORDER BY created_at, id",
        user_id
    )
    .fetch_all(pool)
    .await?;
    let reactions = sqlx::query!(
        "SELECT post_id, emoji, created_at FROM reactions WHERE user_id = $1 ORDER BY created_at",
        user_id
    )
    .fetch_all(pool)
    .await?;
    let invites = sqlx::query!(
        r#"SELECT created_at, email AS "email: String", max_uses, uses, expires_at, revoked_at, note
           FROM invites WHERE created_by = $1 ORDER BY created_at"#,
        user_id
    )
    .fetch_all(pool)
    .await?;
    let invited = sqlx::query_scalar!(
        r#"SELECT username::text AS "username!" FROM users
           WHERE invited_by_user_id = $1 AND deleted_at IS NULL ORDER BY created_at"#,
        user_id
    )
    .fetch_all(pool)
    .await?;
    let groups = sqlx::query_scalar!(
        "SELECT g.name FROM group_members m JOIN groups g ON g.id = m.group_id
         WHERE m.user_id = $1 AND g.owner_user_id IS NULL ORDER BY g.name",
        user_id
    )
    .fetch_all(pool)
    .await?;
    let (own_lists, other_lists): (Vec<_>, Vec<_>) = lists::known_to(db, user_id)
        .await?
        .into_iter()
        .partition(|list| list.yours);
    let blocks = sqlx::query_scalar!(
        r#"SELECT u.username::text AS "username!" FROM blocks b JOIN users u ON u.id = b.blocked_user_id
           WHERE b.user_id = $1 ORDER BY b.created_at"#,
        user_id
    )
    .fetch_all(pool)
    .await?;
    // The threads and conversations they are subscribed to, named only
    // if they may read them now.
    let subscriptions = sqlx::query!(
        r#"SELECT s.thread_id,
                  CASE WHEN t.deleted_at IS NULL
                        AND (thread_readable(t.visibility, t.space_id, $2, $3)
                             OR dm_readable(t.id, t.visibility, $4))
                       THEN t.title END AS title
           FROM thread_subscriptions s JOIN threads t ON t.id = s.thread_id
           WHERE s.user_id = $1 ORDER BY s.thread_id"#,
        user_id,
        &filter.visibilities,
        &filter.restricted_space_ids,
        filter.dm_viewer,
    )
    .fetch_all(pool)
    .await?;
    // Their front page as they have arranged it (DESIGN.md §3.5): every
    // section and the spaces in it, named by slug, whether or not they can
    // read them now, since the arrangement is theirs.
    let layout = match layouts::member_id(db, user_id).await? {
        Some(id) => layouts::read(db, id).await?,
        None => None,
    };
    let muted = sqlx::query_scalar!(
        "SELECT c.slug FROM space_mutes m JOIN spaces c ON c.id = m.space_id
         WHERE m.user_id = $1 ORDER BY c.slug",
        user_id
    )
    .fetch_all(pool)
    .await?;
    let uploads = sqlx::query!(
        "SELECT sha256, ext, width, height, created_at FROM uploads WHERE user_id = $1 ORDER BY created_at",
        user_id
    )
    .fetch_all(pool)
    .await?;
    let reports = sqlx::query!(
        "SELECT kind, reason, details, status, created_at FROM reports
         WHERE reporter_id = $1 ORDER BY created_at",
        user_id
    )
    .fetch_all(pool)
    .await?;
    let sessions = sqlx::query!(
        "SELECT created_at, last_seen_at, ip, user_agent FROM sessions
         WHERE user_id = $1 ORDER BY created_at",
        user_id
    )
    .fetch_all(pool)
    .await?;
    let addresses = sqlx::query!(
        "SELECT ip, first_seen_at, last_seen_at FROM user_ips WHERE user_id = $1 ORDER BY first_seen_at",
        user_id
    )
    .fetch_all(pool)
    .await?;

    Ok(Some(json!({
        "exported_at": chrono::Utc::now(),
        "account": {
            "username": account.username,
            "display_name": account.display_name,
            "email": account.email,
            "email_verified_at": account.email_verified_at,
            "role": account.role,
            "title": account.title,
            "pronouns": account.pronouns,
            "status_line": account.status_line,
            "location": account.location,
            "avatar": match (account.avatar_sha256, account.avatar_ext) {
                (Some(sha), Some(ext)) => Some(crate::uploads::url(&sha, &ext)),
                _ => None,
            },
            "joined_at": account.created_at,
            "last_seen_at": account.last_seen_at,
            "invited_by": account.invited_by,
            "who_can_message": account.dm_policy,
            "email_preferences": account.email_prefs,
        },
        "posts": posts.iter().map(|p| json!({
            "id": p.id,
            "thread_id": p.thread_id,
            "thread_title": p.thread_title,
            "in_conversation": p.conversation,
            "number": p.seq,
            "created_at": p.created_at,
            "edited_at": p.edited_at,
            "deleted": p.deleted,
            "hidden_by_moderator": p.hidden,
            "markdown": p.body_md,
        })).collect::<Vec<_>>(),
        "earlier_versions": revisions.iter().map(|r| json!({
            "post_id": r.post_id,
            "version": r.rev,
            "created_at": r.created_at,
            "markdown": r.body_md,
        })).collect::<Vec<_>>(),
        "posts_held_for_approval": held.iter().map(|h| json!({
            "created_at": h.created_at,
            "title": h.title,
            "thread_id": h.thread_id,
            "thread_title": h.thread_title,
            "markdown": h.body_md,
            "status": h.status,
            "decided_at": h.decided_at,
            "became_post_id": h.post_id,
        })).collect::<Vec<_>>(),
        "notifications": notifications.iter().map(|n| json!({
            "kind": n.kind,
            "thread_id": n.thread_id,
            "post_id": n.post_id,
            "message": n.message,
            "created_at": n.created_at,
            "read_at": n.read_at,
            "emailed_at": n.emailed_at,
        })).collect::<Vec<_>>(),
        "conversations": conversations.iter().map(|c| json!({
            "id": c.id,
            "title": c.title,
            "people": c.people,
            "joined_at": c.joined_at,
            "left_at": c.left_at,
        })).collect::<Vec<_>>(),
        "reactions": reactions.iter().map(|r| json!({
            "post_id": r.post_id,
            "emoji": r.emoji,
            "created_at": r.created_at,
        })).collect::<Vec<_>>(),
        "invites": invites.iter().map(|i| json!({
            "created_at": i.created_at,
            "email": i.email,
            "max_uses": i.max_uses,
            "uses": i.uses,
            "expires_at": i.expires_at,
            "revoked_at": i.revoked_at,
            "note": i.note,
        })).collect::<Vec<_>>(),
        "invited_members": invited,
        "groups": groups,
        "lists": own_lists.iter().map(|l| json!({
            "name": l.name,
            "members": l.members.iter().map(|m| &m.user.username).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "lists_you_are_on": other_lists.iter().map(|l| json!({
            "name": l.name,
            "owner": l.owner.username,
        })).collect::<Vec<_>>(),
        "blocked_members": blocks,
        "thread_subscriptions": subscriptions.iter().map(|s| json!({
            "thread_id": s.thread_id,
            "title": s.title,
        })).collect::<Vec<_>>(),
        "front_page": layout.map(|l| json!({
            "edited_at": l.edited_at,
            "sections": l.sections.iter().map(|s| json!({
                "name": s.name,
                "spaces": s.spaces.iter().map(|c| &c.slug).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        })),
        "muted_spaces": muted,
        "uploads": uploads.iter().map(|u| json!({
            "url": crate::uploads::url(&u.sha256, &u.ext),
            "width": u.width,
            "height": u.height,
            "created_at": u.created_at,
        })).collect::<Vec<_>>(),
        "reports_sent": reports.iter().map(|r| json!({
            "kind": r.kind,
            "reason": r.reason,
            "details": r.details,
            "status": r.status,
            "created_at": r.created_at,
        })).collect::<Vec<_>>(),
        "sessions": sessions.iter().map(|s| json!({
            "created_at": s.created_at,
            "last_seen_at": s.last_seen_at,
            "ip": s.ip,
            "user_agent": s.user_agent,
        })).collect::<Vec<_>>(),
        "addresses": addresses.iter().map(|a| json!({
            "ip": a.ip,
            "first_seen_at": a.first_seen_at,
            "last_seen_at": a.last_seen_at,
        })).collect::<Vec<_>>(),
    })))
}
