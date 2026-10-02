//! Who may fetch an uploaded image (DESIGN.md §9): its uploader; anyone,
//! if it is a member's avatar; and otherwise whoever may read a post that
//! shows it, as decided by the shared visibility condition. A post that
//! is deleted shows nothing, and a hidden one shows its images only to
//! staff, as with its text.

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
        SELECT
            EXISTS (SELECT 1 FROM uploads u WHERE u.sha256 = $1 AND u.ext = $2) AS "exists!",
            EXISTS (SELECT 1 FROM uploads u JOIN users a ON a.avatar_upload_id = u.id
                    WHERE u.sha256 = $1 AND a.deleted_at IS NULL) AS "avatar!",
            EXISTS (SELECT 1 FROM post_uploads pu
                    JOIN posts p ON p.id = pu.post_id JOIN threads t ON t.id = p.thread_id
                    WHERE pu.sha256 = $1 AND p.deleted_at IS NULL AND p.hidden_at IS NULL
                      AND t.deleted_at IS NULL
                      AND thread_readable(t.visibility, t.category_id, $3, $4)) AS "public!",
            EXISTS (SELECT 1 FROM post_uploads pu
                    JOIN posts p ON p.id = pu.post_id JOIN threads t ON t.id = p.thread_id
                    WHERE pu.sha256 = $1 AND p.deleted_at IS NULL AND (p.hidden_at IS NULL OR $8)
                      AND t.deleted_at IS NULL
                      AND (thread_readable(t.visibility, t.category_id, $5, $6)
                           OR dm_readable(t.id, t.visibility, $7))) AS "readable!",
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
    } else if row.readable || row.mine {
        Some(UploadAccess::Private)
    } else {
        None
    })
}
