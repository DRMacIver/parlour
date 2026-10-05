//! Records of uploaded images and avatars (DESIGN.md §9). Processing and
//! storing the files is [`crate::uploads`]; who may fetch them is
//! [`crate::content::uploads`].

use crate::db::{Db, Handle, KEY};
use crate::uploads::Processed;

/// Records that a member uploaded an image (again, perhaps), returning
/// the upload's id. `None` if staff have removed this image, which can't
/// be uploaded again.
pub async fn record<'c>(
    db: impl Handle<'c>,
    user_id: i64,
    image: &Processed,
) -> sqlx::Result<Option<i64>> {
    let mut conn = db.open(KEY).acquire().await?;
    sqlx::query_scalar!(
        r#"INSERT INTO uploads (user_id, sha256, ext, mime, bytes, width, height)
           SELECT $1, $2, $3, $4, $5, $6, $7
           WHERE NOT EXISTS (SELECT 1 FROM removed_uploads WHERE sha256 = $2)
           ON CONFLICT (user_id, sha256) DO UPDATE SET created_at = uploads.created_at
           RETURNING id"#,
        user_id,
        image.sha256,
        image.ext,
        image.mime,
        image.bytes.len() as i32,
        image.width as i32,
        image.height as i32,
    )
    .fetch_optional(&mut *conn)
    .await
}

/// Whether staff have removed the image with this hash.
pub async fn is_removed(db: &Db, sha256: &str) -> sqlx::Result<bool> {
    sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM removed_uploads WHERE sha256 = $1) AS "removed!""#,
        sha256
    )
    .fetch_one(db.pool())
    .await
}

/// Makes an upload a member's avatar, or with `None` removes it.
pub async fn set_avatar(db: &Db, user_id: i64, upload_id: Option<i64>) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE users SET avatar_upload_id = $2 WHERE id = $1",
        user_id,
        upload_id
    )
    .execute(db.pool())
    .await?;
    Ok(())
}

/// The address of a member's avatar, if they have one.
pub async fn avatar_url(db: &Db, user_id: i64) -> sqlx::Result<Option<String>> {
    let row = sqlx::query!(
        "SELECT up.sha256, up.ext FROM users u JOIN uploads up ON up.id = u.avatar_upload_id
         WHERE u.id = $1",
        user_id
    )
    .fetch_optional(db.pool())
    .await?;
    Ok(row.map(|row| crate::uploads::url(&row.sha256, &row.ext)))
}

/// The addresses of the images a post shows, in no particular order.
pub async fn in_post(db: &Db, post_id: i64) -> sqlx::Result<Vec<String>> {
    let rows = sqlx::query!(
        r#"SELECT DISTINCT pu.sha256, u.ext FROM post_uploads pu JOIN uploads u ON u.sha256 = pu.sha256
           WHERE pu.post_id = $1 ORDER BY pu.sha256"#,
        post_id
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| crate::uploads::url(&row.sha256, &row.ext))
        .collect())
}

/// What removing an image took away.
pub struct RemovedImage {
    /// The id of one of its records, for the moderation log.
    pub upload_id: i64,
    /// Who uploaded it, if just one member did.
    pub uploader_id: Option<i64>,
    /// The members whose picture it was.
    pub avatar_user_ids: Vec<i64>,
}

/// Removes an image for everyone: its records go (and with them anyone's
/// picture that it was), and it can't be uploaded again. The caller
/// deletes the file once this is committed. `None` if there was no such
/// image.
pub async fn remove<'c>(
    db: impl Handle<'c>,
    sha256: &str,
    ext: &str,
    by: i64,
) -> sqlx::Result<Option<RemovedImage>> {
    let mut tx = db.open(KEY).begin().await?;
    let avatar_user_ids = sqlx::query_scalar!(
        "SELECT a.id FROM uploads u JOIN users a ON a.avatar_upload_id = u.id
         WHERE u.sha256 = $1 ORDER BY a.id",
        sha256
    )
    .fetch_all(&mut *tx)
    .await?;
    let removed = sqlx::query!(
        "DELETE FROM uploads WHERE sha256 = $1 AND ext = $2 RETURNING id, user_id",
        sha256,
        ext
    )
    .fetch_all(&mut *tx)
    .await?;
    let Some(first) = removed.iter().min_by_key(|r| r.id) else {
        return Ok(None);
    };
    // Each member has one record of an image.
    let uploaders: Vec<i64> = removed.iter().filter_map(|r| r.user_id).collect();
    sqlx::query!(
        "INSERT INTO removed_uploads (sha256, removed_by) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        sha256,
        by
    )
    .execute(&mut *tx)
    .await?;
    let result = RemovedImage {
        upload_id: first.id,
        uploader_id: (uploaders.len() == 1).then(|| uploaders[0]),
        avatar_user_ids,
    };
    tx.commit().await?;
    Ok(Some(result))
}
