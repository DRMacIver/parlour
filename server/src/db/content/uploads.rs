//! Who may fetch an uploaded image (DESIGN.md §9): its uploader; anyone,
//! if it is a member's avatar; and otherwise whoever may read a post that
//! shows it, or showed it in an earlier version that hasn't been redacted
//! (anyone who may read a post may read its edit history), as decided by
//! the shared visibility condition. A post that is deleted shows nothing,
//! and a hidden one shows its images only to those who moderate where it
//! is, staff and its space's administrators, as with its text.
//! Staff may also fetch the images in posts held for approval, which they
//! review, where they could read the post once it is up (DESIGN.md §8),
//! and the images in what a break-glass grant of theirs opens, each fetch
//! recorded as a page shown under it.

use crate::content::UserRef;
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
                      AND thread_readable(t, $3, $4, NULL, NULL)) AS "public!",
            EXISTS (SELECT 1 FROM shown_in s
                    JOIN posts p ON p.id = s.post_id JOIN threads t ON t.id = p.thread_id
                    WHERE p.deleted_at IS NULL
                      AND (p.hidden_at IS NULL OR $8 OR space_run_by(t.space_id, $7, $10))
                      AND t.deleted_at IS NULL
                      AND (thread_readable(t, $5, $6, $7, $10)
                           OR dm_readable(t.id, t.visibility, $7))) AS "readable!",
            $8 AND EXISTS (SELECT 1 FROM held_post_uploads hu
                           JOIN held_posts h ON h.id = hu.held_post_id
                           WHERE hu.sha256 = $1
                             AND held_readable(h, $5, $6, $7, $10)) AS "held!",
            EXISTS (SELECT 1 FROM uploads u WHERE u.sha256 = $1 AND u.user_id = $9) AS "mine!",
            (SELECT t.id FROM shown_in s
             JOIN posts p ON p.id = s.post_id JOIN threads t ON t.id = p.thread_id
             WHERE p.deleted_at IS NULL AND t.deleted_at IS NULL
               AND thread_granted(t, $11, $12)
             ORDER BY t.id LIMIT 1) AS "granted_thread?"
        "#,
        sha256,
        ext,
        &anyone.visibilities,
        &anyone.restricted_space_ids,
        &filter.visibilities,
        &filter.restricted_space_ids,
        filter.dm_viewer,
        viewer.is_staff(),
        viewer.user_id(),
        &filter.group_ids,
        &filter.granted_thread_ids,
        &filter.granted_space_ids,
    )
    .fetch_one(db.pool())
    .await?;
    Ok(if !row.exists {
        None
    } else if row.avatar || row.public {
        Some(UploadAccess::Public)
    } else if row.readable || row.held || row.mine {
        Some(UploadAccess::Private)
    } else if let Some(thread_id) = row.granted_thread {
        // Under a grant: the image is a page shown, recorded before it is.
        let space_id = sqlx::query_scalar!("SELECT space_id FROM threads WHERE id = $1", thread_id)
            .fetch_one(db.pool())
            .await?;
        super::record_granted_view(
            db,
            viewer,
            super::Granted::Thread {
                thread_id,
                space_id,
            },
        )
        .await?;
        Some(UploadAccess::Private)
    } else {
        None
    })
}

/// A post that shows an image, now or in an earlier version, in a thread
/// the viewer can read.
pub struct ImageInPost {
    pub post_id: i64,
    pub seq: i32,
    pub thread_title: String,
    pub deleted: bool,
}

/// Everything about an image, for staff deciding whether to remove it.
/// Staff read what their memberships let them (DESIGN.md §1 principle 3):
/// the posts that show it are named only in threads they can read, and
/// the rest are only counted.
pub struct ImageUse {
    pub width: i32,
    pub height: i32,
    pub bytes: i32,
    pub uploaders: Vec<UserRef>,
    /// Posts in space threads the viewer can read.
    pub posts: Vec<ImageInPost>,
    /// How many posts in space threads the viewer can't read show it.
    pub closed_posts: i64,
    /// How many posts in conversations show it. Staff aren't told which.
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

/// Where an image is used, as the viewer may know it. The caller checks
/// the viewer is staff.
pub async fn image_use(
    db: &Db,
    viewer: &Viewer,
    sha256: &str,
    ext: &str,
) -> sqlx::Result<ImageLookup> {
    let pool = db.pool();
    let Some(file) = sqlx::query!(
        "SELECT width, height, bytes FROM uploads WHERE sha256 = $1 AND ext = $2 LIMIT 1",
        sha256,
        ext
    )
    .fetch_optional(pool)
    .await?
    else {
        return Ok(if crate::upload_records::is_removed(db, sha256).await? {
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
    let filter = viewer.filter();
    let posts = sqlx::query!(
        r#"WITH shown_in AS (
               SELECT post_id FROM post_uploads WHERE sha256 = $1
               UNION
               SELECT r.post_id FROM revision_uploads ru JOIN post_revisions r ON r.id = ru.revision_id
               WHERE ru.sha256 = $1
           )
           SELECT p.id, p.seq, t.space_id IS NULL AS "private!",
                  CASE WHEN thread_readable(t, $2, $3, $4, $5) THEN t.title END AS "title?",
                  p.deleted_at IS NOT NULL OR t.deleted_at IS NOT NULL AS "deleted!"
           FROM shown_in s JOIN posts p ON p.id = s.post_id JOIN threads t ON t.id = p.thread_id
           ORDER BY p.id"#,
        sha256,
        &filter.visibilities,
        &filter.restricted_space_ids,
        filter.dm_viewer,
        &filter.group_ids,
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
    let (conversation_posts, posts): (Vec<_>, Vec<_>) = posts.into_iter().partition(|p| p.private);
    let (posts, closed_posts): (Vec<_>, Vec<_>) =
        posts.into_iter().partition(|p| p.title.is_some());
    Ok(ImageLookup::Found(ImageUse {
        width: file.width,
        height: file.height,
        bytes: file.bytes,
        uploaders: uploaders
            .into_iter()
            .map(|r| user_ref(r.id, r.username, r.display_name))
            .collect(),
        conversation_posts: conversation_posts.len() as i64,
        closed_posts: closed_posts.len() as i64,
        posts: posts
            .into_iter()
            .filter_map(|p| {
                Some(ImageInPost {
                    post_id: p.id,
                    seq: p.seq,
                    thread_title: p.title?,
                    deleted: p.deleted,
                })
            })
            .collect(),
        held_posts,
        avatars: avatars
            .into_iter()
            .map(|r| user_ref(r.id, r.username, r.display_name))
            .collect(),
    }))
}
