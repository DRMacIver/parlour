//! Records of uploaded images and avatars (DESIGN.md §9). Processing and
//! storing the files is [`crate::uploads`]; who may fetch them is
//! [`crate::content::uploads`].

use crate::db::{Db, Handle, KEY};
use crate::uploads::Processed;

/// Records that a member uploaded an image (again, perhaps), returning
/// the upload's id.
pub async fn record<'c>(db: impl Handle<'c>, user_id: i64, image: &Processed) -> sqlx::Result<i64> {
    let mut conn = db.open(KEY).acquire().await?;
    sqlx::query_scalar!(
        r#"INSERT INTO uploads (user_id, sha256, ext, mime, bytes, width, height)
           VALUES ($1, $2, $3, $4, $5, $6, $7)
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
    .fetch_one(&mut *conn)
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
