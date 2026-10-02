//! What a committed write means for the page cache (DESIGN.md §5).
//!
//! After each write commits, the handler calls [`crate::AppState::changed`]
//! with a [`Change`], which works out the tags from the current state of the
//! database and clears them.

use crate::db::Db;

use crate::cache::Tag;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// A new post, including the first post of a new thread.
    PostCreated { post_id: i64 },
    /// A post's text changed.
    PostEdited { post_id: i64 },
    /// Someone added or took back a reaction to a post.
    ReactionChanged { post_id: i64 },
    /// A post was deleted, hidden, or restored.
    PostRemoved { post_id: i64 },
    /// A thread was renamed, pinned, locked, deleted, restored or moved. For
    /// a move, `previous_category_id` is where it was before.
    ThreadChanged {
        thread_id: i64,
        previous_category_id: Option<i64>,
    },
    /// A category's name, description, levels, section or position changed,
    /// or it was created. Changes the index structure, so clears `site`.
    CategoryChanged { category_id: i64 },
    /// A user was renamed or changed their post-info column.
    UserChanged { user_id: i64 },
    /// Someone joined. Changes the invite tree and the inviter's profile.
    UserCreated { user_id: i64 },
    /// An invite was used or revoked.
    InviteChanged { invite_id: i64 },
    /// Site settings or sections changed.
    SiteChanged,
}

/// The tags to clear for a change.
pub async fn tags_for(db: &Db, change: Change) -> sqlx::Result<Vec<Tag>> {
    let mut tags = Vec::new();
    match change {
        Change::PostCreated { post_id } | Change::PostRemoved { post_id } => {
            if let Some(post) = post_context(db, post_id).await? {
                tags.push(Tag::Thread(post.thread_id));
                tags.extend(post.category_id.map(Tag::Category));
                tags.push(Tag::User(post.author_id));
                // A new thread that quote-forks a post adds a backlink to the
                // post's thread.
                tags.extend(post.forked_from_thread_id.map(Tag::Thread));
            }
            tags.extend(forks_of(db, post_id).await?);
        }
        Change::PostEdited { post_id } => {
            if let Some(post) = post_context(db, post_id).await? {
                tags.push(Tag::Thread(post.thread_id));
            }
            tags.extend(forks_of(db, post_id).await?);
        }
        Change::ReactionChanged { post_id } => {
            if let Some(post) = post_context(db, post_id).await? {
                tags.push(Tag::Thread(post.thread_id));
            }
        }
        Change::ThreadChanged {
            thread_id,
            previous_category_id,
        } => {
            tags.push(Tag::Thread(thread_id));
            tags.extend(previous_category_id.map(Tag::Category));
            let thread = sqlx::query!(
                r#"SELECT t.category_id,
                          (SELECT p.thread_id FROM posts p WHERE p.id = t.forked_from_post_id) AS forked_from_thread_id
                   FROM threads t WHERE t.id = $1"#,
                thread_id
            )
            .fetch_optional(db.pool())
            .await?;
            if let Some(thread) = thread {
                tags.extend(thread.category_id.map(Tag::Category));
                tags.extend(thread.forked_from_thread_id.map(Tag::Thread));
            }
            // Post counts next to posts only count visible threads.
            let authors = sqlx::query_scalar!(
                "SELECT DISTINCT author_id FROM posts WHERE thread_id = $1",
                thread_id
            )
            .fetch_all(db.pool())
            .await?;
            tags.extend(authors.into_iter().map(Tag::User));
        }
        Change::CategoryChanged { category_id } => {
            tags.push(Tag::Category(category_id));
            tags.push(Tag::Site);
        }
        Change::UserChanged { user_id } => {
            tags.push(Tag::User(user_id));
            tags.push(Tag::Site);
        }
        Change::UserCreated { user_id } => {
            tags.push(Tag::Members);
            tags.push(Tag::User(user_id));
            let inviter = sqlx::query_scalar!(
                "SELECT invited_by_user_id FROM users WHERE id = $1",
                user_id
            )
            .fetch_optional(db.pool())
            .await?
            .flatten();
            tags.extend(inviter.map(Tag::User));
        }
        Change::InviteChanged { invite_id } => tags.push(Tag::Invite(invite_id)),
        Change::SiteChanged => tags.push(Tag::Site),
    }
    Ok(tags)
}

/// The thread a post is in and its number there, for live updates.
pub async fn post_seq(db: &Db, post_id: i64) -> sqlx::Result<Option<(i64, i64)>> {
    let row = sqlx::query!("SELECT thread_id, seq FROM posts WHERE id = $1", post_id)
        .fetch_optional(db.pool())
        .await?;
    Ok(row.map(|row| (row.thread_id, i64::from(row.seq))))
}

struct PostContext {
    thread_id: i64,
    category_id: Option<i64>,
    author_id: i64,
    forked_from_thread_id: Option<i64>,
}

async fn post_context(db: &Db, post_id: i64) -> sqlx::Result<Option<PostContext>> {
    let row = sqlx::query!(
        r#"SELECT p.thread_id, p.author_id, t.category_id,
                  (SELECT sp.thread_id FROM posts sp WHERE sp.id = t.forked_from_post_id) AS forked_from_thread_id
           FROM posts p JOIN threads t ON t.id = p.thread_id
           WHERE p.id = $1"#,
        post_id
    )
    .fetch_optional(db.pool())
    .await?;
    Ok(row.map(|row| PostContext {
        thread_id: row.thread_id,
        category_id: row.category_id,
        author_id: row.author_id,
        forked_from_thread_id: row.forked_from_thread_id,
    }))
}

/// Threads that embed this post.
async fn forks_of(db: &Db, post_id: i64) -> sqlx::Result<Vec<Tag>> {
    let ids = sqlx::query_scalar!(
        "SELECT id FROM threads WHERE forked_from_post_id = $1",
        post_id
    )
    .fetch_all(db.pool())
    .await?;
    Ok(ids.into_iter().map(Tag::Thread).collect())
}
