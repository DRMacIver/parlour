//! Who may fetch an uploaded image (DESIGN.md §9): its uploader; anyone,
//! if it is a member's avatar; and otherwise whoever may read a post that
//! shows it, or showed it in an earlier version that hasn't been redacted
//! (anyone who may read a post may read its edit history), as decided by
//! the shared visibility condition. A post that is deleted shows nothing,
//! and a hidden one shows its images only to staff, as with its text.
//! Staff may also fetch the images in posts held for approval, which they
//! review.

use crate::db::Db;
use crate::viewer::Viewer;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UploadAccess {
    /// Visitors may fetch it too, so shared caches may keep it.
    Public,
    /// This viewer may fetch it, but not everyone may.
    Private,
}

/// Whether the viewer may fetch the image with this hash and extension,
/// and whether anyone may. `None` if there is no such image or the viewer
/// may not see it.
pub async fn upload_access(
    db: &Db,
    viewer: &Viewer,
    sha256: &str,
    ext: &str,
) -> sqlx::Result<Option<UploadAccess>> {
    let filter = viewer.filter();
    let anyone = Viewer::Anonymous.filter();
    let row = sqlx::query!(
        r#"
        WITH shown_in AS (
            -- The posts that show the image now, or showed it in an
            -- earlier version that hasn't been redacted (redacting empties
            -- a version, and so its references).
            SELECT pu.post_id FROM post_uploads pu WHERE pu.sha256 = $1
            UNION
            SELECT r.post_id FROM revision_uploads ru JOIN post_revisions r ON r.id = ru.revision_id
            WHERE ru.sha256 = $1
        )
        SELECT
            EXISTS (SELECT 1 FROM uploads u WHERE u.sha256 = $1 AND u.ext = $2) AS "exists!",
            EXISTS (SELECT 1 FROM uploads u JOIN users a ON a.avatar_upload_id = u.id
                    WHERE u.sha256 = $1 AND a.deleted_at IS NULL) AS "avatar!",
            EXISTS (SELECT 1 FROM shown_in s
                    JOIN posts p ON p.id = s.post_id JOIN threads t ON t.id = p.thread_id
                    WHERE p.deleted_at IS NULL AND p.hidden_at IS NULL
                      AND t.deleted_at IS NULL
                      AND thread_readable(t.visibility, t.category_id, $3, $4)) AS "public!",
            EXISTS (SELECT 1 FROM shown_in s
                    JOIN posts p ON p.id = s.post_id JOIN threads t ON t.id = p.thread_id
                    WHERE p.deleted_at IS NULL AND (p.hidden_at IS NULL OR $8)
                      AND t.deleted_at IS NULL
                      AND (thread_readable(t.visibility, t.category_id, $5, $6)
                           OR dm_readable(t.id, t.visibility, $7))) AS "readable!",
            $8 AND EXISTS (SELECT 1 FROM held_post_uploads hu WHERE hu.sha256 = $1) AS "held!",
            EXISTS (SELECT 1 FROM uploads u WHERE u.sha256 = $1 AND u.user_id = $9) AS "mine!"
        "#,
        sha256,
        ext,
        &anyone.visibilities,
        &anyone.group_category_ids,
        &filter.visibilities,
        &filter.group_category_ids,
        filter.dm_viewer,
        viewer.is_staff(),
        viewer.user_id(),
    )
    .fetch_one(db.pool())
    .await?;
    Ok(if !row.exists {
        None
    } else if row.avatar || row.public {
        Some(UploadAccess::Public)
    } else if row.readable || row.held || row.mine {
        Some(UploadAccess::Private)
    } else {
        None
    })
}
