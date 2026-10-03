//! Records of uploaded images and avatars (DESIGN.md §9). Processing and
//! storing the files is [`crate::uploads`]; who may fetch them is
//! [`crate::content::uploads`].

use crate::content::UserRef;
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

/// A post in a space thread that shows an image, now or in an earlier
/// version.
pub struct ImageInPost {
    pub post_id: i64,
    pub seq: i32,
    pub thread_title: String,
    pub deleted: bool,
}

/// Everything about an image, for staff deciding whether to remove it.
pub struct ImageUse {
    pub width: i32,
    pub height: i32,
    pub bytes: i32,
    pub uploaders: Vec<UserRef>,
    /// Posts in space threads, which staff may read.
    pub posts: Vec<ImageInPost>,
    /// How many posts in conversations show it. Staff aren't told which,
    /// as they may not read the conversations.
    pub conversation_posts: i64,
    /// Posts held for approval that show it.
    pub held_posts: Vec<i64>,
    /// The members whose picture it is.
    pub avatars: Vec<UserRef>,
}

pub enum ImageLookup {
    NotFound,
    /// Staff removed it.
    Removed,
    Found(ImageUse),
}

/// Where an image is used, for staff. The caller checks the viewer is
/// staff.
pub async fn image_use(db: &Db, sha256: &str, ext: &str) -> sqlx::Result<ImageLookup> {
    let pool = db.pool();
    let Some(file) = sqlx::query!(
        "SELECT width, height, bytes FROM uploads WHERE sha256 = $1 AND ext = $2 LIMIT 1",
        sha256,
        ext
    )
    .fetch_optional(pool)
    .await?
    else {
        return Ok(if is_removed(db, sha256).await? {
            ImageLookup::Removed
        } else {
            ImageLookup::NotFound
        });
    };
    let uploaders = sqlx::query!(
        r#"SELECT a.id, a.username AS "username!: String", a.display_name
           FROM uploads u JOIN users a ON a.id = u.user_id
           WHERE u.sha256 = $1 ORDER BY u.created_at, a.id"#,
        sha256
    )
    .fetch_all(pool)
    .await?;
    let avatars = sqlx::query!(
        r#"SELECT a.id, a.username AS "username!: String", a.display_name
           FROM uploads u JOIN users a ON a.avatar_upload_id = u.id
           WHERE u.sha256 = $1 ORDER BY a.id"#,
        sha256
    )
    .fetch_all(pool)
    .await?;
    let posts = sqlx::query!(
        r#"WITH shown_in AS (
               SELECT post_id FROM post_uploads WHERE sha256 = $1
               UNION
               SELECT r.post_id FROM revision_uploads ru JOIN post_revisions r ON r.id = ru.revision_id
               WHERE ru.sha256 = $1
           )
           SELECT p.id, p.seq, t.title, t.space_id IS NULL AS "private!",
                  p.deleted_at IS NOT NULL OR t.deleted_at IS NOT NULL AS "deleted!"
           FROM shown_in s JOIN posts p ON p.id = s.post_id JOIN threads t ON t.id = p.thread_id
           ORDER BY p.id"#,
        sha256
    )
    .fetch_all(pool)
    .await?;
    let held_posts = sqlx::query_scalar!(
        "SELECT held_post_id FROM held_post_uploads WHERE sha256 = $1 ORDER BY held_post_id",
        sha256
    )
    .fetch_all(pool)
    .await?;
    let user_ref = |id: i64, username: String, display_name: String| UserRef {
        id,
        username,
        display_name,
    };
    Ok(ImageLookup::Found(ImageUse {
        width: file.width,
        height: file.height,
        bytes: file.bytes,
        uploaders: uploaders
            .into_iter()
            .map(|r| user_ref(r.id, r.username, r.display_name))
            .collect(),
        conversation_posts: posts.iter().filter(|p| p.private).count() as i64,
        posts: posts
            .into_iter()
            .filter(|p| !p.private)
            .map(|p| ImageInPost {
                post_id: p.id,
                seq: p.seq,
                thread_title: p.title,
                deleted: p.deleted,
            })
            .collect(),
        held_posts,
        avatars: avatars
            .into_iter()
            .map(|r| user_ref(r.id, r.username, r.display_name))
            .collect(),
    }))
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
