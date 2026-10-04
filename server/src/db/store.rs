//! Low-level writes for the content model: creating users, spaces,
//! threads and posts with their bookkeeping (post numbering, thread
//! activity, rendered HTML, each member's layout) kept consistent.
//!
//! Each takes a [`Handle`]: the database, or an open transaction.
//!
//! These functions don't check permissions or clear the page cache. Request
//! handlers that write (M4 onwards) check permissions first and then call
//! [`crate::AppState::changed`] once the write has committed. The `seed`
//! command uses them directly.

use crate::db::{Handle, KEY};
use chrono::{DateTime, Utc};

use crate::layouts;
use crate::markdown;
use crate::permissions::{Grant, Level};
use crate::social;

/// Turns a title into the readable part of a thread URL.
pub fn slugify(title: &str) -> String {
    let mut slug = String::new();
    let mut pending_dash = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            pending_dash = false;
            slug.push(c.to_ascii_lowercase());
            if slug.len() >= 60 {
                break;
            }
        } else if !matches!(c, '\'' | '’' | '‘') {
            pending_dash = true;
        }
    }
    if slug.is_empty() {
        slug.push_str("thread");
    }
    // `/t/{id}/unread`, `/t/{id}/reply`, `/t/{id}/subscribe` and
    // `/t/{id}/unsubscribe` are routes of their own.
    if RESERVED_SLUGS.contains(&slug.as_str()) {
        slug.push_str("-thread");
    }
    slug
}

const RESERVED_SLUGS: &[&str] = &["unread", "reply", "subscribe", "unsubscribe"];

#[derive(Clone, Debug, Default)]
pub struct NewUser {
    pub username: String,
    pub display_name: String,
    pub email: String,
    pub role: Option<String>,
    pub title: Option<String>,
    pub pronouns: Option<String>,
    pub status_line: Option<String>,
    pub location: Option<String>,
    pub invited_by_user_id: Option<i64>,
    pub created_at: Option<DateTime<Utc>>,
}

/// Creates an account, with its copy of the default front page
/// (DESIGN.md §3.5).
pub async fn create_user<'c>(db: impl Handle<'c>, user: &NewUser) -> sqlx::Result<i64> {
    let mut tx = db.open(KEY).begin().await?;
    let id = sqlx::query_scalar!(
        r#"INSERT INTO users (username, display_name, email, role, title, pronouns, status_line,
                              location, invited_by_user_id, created_at)
           VALUES ($1, $2, $3, coalesce($4, 'user'), $5, $6, $7, $8, $9, coalesce($10, now()))
           RETURNING id"#,
        user.username as _,
        user.display_name,
        user.email as _,
        user.role,
        user.title,
        user.pronouns,
        user.status_line,
        user.location,
        user.invited_by_user_id,
        user.created_at,
    )
    .fetch_one(&mut *tx)
    .await?;
    layouts::create_member_copy(&mut tx, id).await?;
    tx.commit().await?;
    Ok(id)
}

#[derive(Clone, Debug)]
pub struct NewSpace {
    pub slug: String,
    pub name: String,
    pub description: String,
    pub created_by: Option<i64>,
    pub read: Grant,
    pub reply: Grant,
    pub start: Grant,
    pub administer: Grant,
}

impl NewSpace {
    /// A space described the short way tests and the seed do: a level for
    /// reading (`public`, `members`, `staff` or `listed`) and one for both
    /// replying and starting threads (`members`, `staff` or `listed`),
    /// naming nobody yet. Staff alone administer it.
    pub fn with_levels(
        slug: &str,
        name: &str,
        description: &str,
        read_level: &str,
        write_level: &str,
    ) -> NewSpace {
        let level = |level: &str| match level {
            "groups" => Grant::listed([], []),
            other => Grant::from_parts(other, &[], &[]),
        };
        NewSpace {
            slug: slug.to_owned(),
            name: name.to_owned(),
            description: description.to_owned(),
            created_by: None,
            read: level(read_level),
            reply: level(write_level),
            start: level(write_level),
            administer: Grant::staff(),
        }
    }
}

/// Creates a space with its grants, without checking anything. It is on
/// nobody's front page until a layout lists it ([`crate::layouts`]).
pub async fn create_space<'c>(db: impl Handle<'c>, space: &NewSpace) -> sqlx::Result<i64> {
    let mut tx = db.open(KEY).begin().await?;
    let id = sqlx::query_scalar!(
        r#"INSERT INTO spaces (slug, name, description, created_by, visibility)
           VALUES ($1, $2, $3, $4, 'restricted') RETURNING id"#,
        space.slug,
        space.name,
        space.description,
        space.created_by,
    )
    .fetch_one(&mut *tx)
    .await?;
    for (verb, grant) in [
        ("read", &space.read),
        ("reply", &space.reply),
        ("start", &space.start),
        ("administer", &space.administer),
    ] {
        upsert_grant(&mut tx, id, verb, grant).await?;
    }
    tx.commit().await?;
    Ok(id)
}

// ---------------------------------------------------------------------------
// Spaces, as their administrators manage them

pub const MAX_SPACE_NAME_CHARS: usize = 80;
pub const MAX_SPACE_DESCRIPTION_CHARS: usize = 500;
pub const MAX_SPACE_SLUG_CHARS: usize = 40;

/// Everything about a space that its administrators set. Where it sits
/// on the default front page, and whether it is essential, are the
/// admins' to say, through [`crate::layouts`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpaceSettings {
    pub slug: String,
    pub name: String,
    pub description: String,
    pub read: Grant,
    pub reply: Grant,
    pub start: Grant,
    pub administer: Grant,
}

impl Default for SpaceSettings {
    /// The defaults for a new space (DESIGN.md §3.1): members read, reply
    /// and start threads; nobody but staff administers until the maker is
    /// named.
    fn default() -> SpaceSettings {
        SpaceSettings {
            slug: String::new(),
            name: String::new(),
            description: String::new(),
            read: Grant::members(),
            reply: Grant::members(),
            start: Grant::members(),
            administer: Grant::listed([], []),
        }
    }
}

fn one_line(text: &str) -> String {
    text.replace(|c: char| c.is_control(), " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

impl SpaceSettings {
    /// Tidies the settings, returning the problems with them if any.
    pub fn clean(&self) -> Result<SpaceSettings, Vec<String>> {
        let clean = SpaceSettings {
            slug: self.slug.trim().to_ascii_lowercase(),
            name: one_line(&self.name),
            description: one_line(&self.description),
            read: self.read.clone(),
            reply: self.reply.clone(),
            start: self.start.clone(),
            administer: self.administer.clone(),
        };
        let mut problems = Vec::new();
        // A feed's address is its owner's username, which may hold
        // underscores; the store refuses one of those for a topic space.
        if !(topic_slug(&clean.slug) || feed_slug(&clean.slug)) {
            problems.push(SPACE_ADDRESS_FORM.to_owned());
        }
        if clean.name.is_empty() || clean.name.chars().count() > MAX_SPACE_NAME_CHARS {
            problems.push(format!(
                "The name should be between 1 and {MAX_SPACE_NAME_CHARS} characters."
            ));
        }
        if clean.description.chars().count() > MAX_SPACE_DESCRIPTION_CHARS {
            problems.push(format!(
                "Keep the description to {MAX_SPACE_DESCRIPTION_CHARS} characters."
            ));
        }
        if [&clean.reply, &clean.start, &clean.administer]
            .iter()
            .any(|grant| grant.level == Level::Public)
        {
            problems.push("Only reading can be open to everyone.".to_owned());
        }
        if problems.is_empty() {
            Ok(clean)
        } else {
            Err(problems)
        }
    }

    fn grants(&self) -> [(&'static str, &Grant); 4] {
        [
            ("read", &self.read),
            ("reply", &self.reply),
            ("start", &self.start),
            ("administer", &self.administer),
        ]
    }
}

pub const SPACE_ADDRESS_FORM: &str = "The address should be up to 40 lower-case letters and digits, with single hyphens between words.";

/// Whether a topic space may have this address.
fn topic_slug(slug: &str) -> bool {
    crate::groups::valid_slug(slug) && slug.len() <= MAX_SPACE_SLUG_CHARS
}

/// Whether a feed may have this address: a username, in lower case.
fn feed_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= MAX_SPACE_SLUG_CHARS
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

pub const SPACE_SLUG_TAKEN: &str = "Another space has that address.";
pub const FEED_ADDRESS: &str = "A feed’s address is its owner’s username.";
pub const FEED_RUN_BY_OWNER: &str = "The owner of a feed always runs it.";
pub const FEED_NOT_DELETED: &str = "A feed can’t be deleted. Archive it instead.";
pub const SPACE_SLUG_IS_USERNAME: &str =
    "That address is a member’s name. Space addresses and usernames can’t be the same.";

fn slug_clash(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::Database(err) if err.is_unique_violation())
}

/// Whether a space address is some member's name, now or before they
/// deleted their account (DESIGN.md §3.1: the two share one namespace).
async fn slug_is_username(conn: &mut sqlx::PgConnection, slug: &str) -> sqlx::Result<bool> {
    sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM users WHERE username = $1::citext)
               OR EXISTS (SELECT 1 FROM retired_usernames WHERE username_hash = username_hash($1)) AS "taken!""#,
        slug
    )
    .fetch_one(conn)
    .await
}

async fn upsert_grant(
    conn: &mut sqlx::PgConnection,
    space_id: i64,
    verb: &str,
    grant: &Grant,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO space_grants (space_id, verb, level, group_ids, user_ids)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (space_id, verb)
         DO UPDATE SET level = EXCLUDED.level, group_ids = EXCLUDED.group_ids, user_ids = EXCLUDED.user_ids",
        space_id,
        verb,
        grant.level.as_str(),
        &grant.group_ids(),
        &grant.user_ids(),
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Replaces one of a space's grants, without checking anything.
pub async fn set_grant<'c>(
    db: impl Handle<'c>,
    space_id: i64,
    verb: &str,
    grant: &Grant,
) -> sqlx::Result<()> {
    let mut conn = db.open(KEY).acquire().await?;
    upsert_grant(&mut conn, space_id, verb, grant).await
}

/// Makes a space from settings that [`SpaceSettings::clean`] accepted, by
/// `created_by` if a member made it. Returns the problem if its address is
/// taken or is a member's name.
pub async fn add_space<'c>(
    db: impl Handle<'c>,
    settings: &SpaceSettings,
    created_by: Option<i64>,
) -> sqlx::Result<Result<i64, String>> {
    if !topic_slug(&settings.slug) {
        return Ok(Err(SPACE_ADDRESS_FORM.to_owned()));
    }
    let mut tx = db.open(KEY).begin().await?;
    if slug_is_username(&mut tx, &settings.slug).await? {
        return Ok(Err(SPACE_SLUG_IS_USERNAME.to_owned()));
    }
    let inserted = sqlx::query_scalar!(
        r#"INSERT INTO spaces (slug, name, description, created_by, visibility)
           VALUES ($1, $2, $3, $4, 'restricted') RETURNING id"#,
        settings.slug,
        settings.name,
        settings.description,
        created_by,
    )
    .fetch_one(&mut *tx)
    .await;
    let id = match inserted {
        Ok(id) => id,
        Err(err) if slug_clash(&err) => return Ok(Err(SPACE_SLUG_TAKEN.to_owned())),
        Err(err) => return Err(err),
    };
    for (verb, grant) in settings.grants() {
        upsert_grant(&mut tx, id, verb, grant).await?;
    }
    tx.commit().await?;
    Ok(Ok(id))
}

/// The id of the space at this address, if there is one.
pub async fn space_id_by_slug<'c>(db: impl Handle<'c>, slug: &str) -> sqlx::Result<Option<i64>> {
    let mut conn = db.open(KEY).acquire().await?;
    sqlx::query_scalar!("SELECT id FROM spaces WHERE slug = $1", slug)
        .fetch_optional(&mut *conn)
        .await
}

/// Changes a space to settings that [`SpaceSettings::clean`] accepted.
/// Its threads' visibility follows its new read grant (triggers see to
/// that). `Ok(Ok(false))` if there is no such space.
pub async fn update_space<'c>(
    db: impl Handle<'c>,
    id: i64,
    settings: &SpaceSettings,
) -> sqlx::Result<Result<bool, String>> {
    let mut tx = db.open(KEY).begin().await?;
    let current = sqlx::query!(
        "SELECT slug, owner_user_id FROM spaces WHERE id = $1 FOR UPDATE",
        id
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(current) = current else {
        return Ok(Ok(false));
    };
    let mut settings = settings.clone();
    // A feed is at its owner's username, and they always run it
    // (DESIGN.md §3.2): a grant that lists who runs it lists them too.
    if let Some(owner) = current.owner_user_id {
        if settings.slug != current.slug {
            return Ok(Err(FEED_ADDRESS.to_owned()));
        }
        match settings.administer.level {
            Level::Members => {}
            Level::Listed => {
                settings.administer.user_ids.insert(owner);
            }
            Level::Public | Level::Staff => return Ok(Err(FEED_RUN_BY_OWNER.to_owned())),
        }
    } else if !topic_slug(&settings.slug) {
        return Ok(Err(SPACE_ADDRESS_FORM.to_owned()));
    } else if current.slug != settings.slug && slug_is_username(&mut tx, &settings.slug).await? {
        return Ok(Err(SPACE_SLUG_IS_USERNAME.to_owned()));
    }
    let updated = sqlx::query!(
        r#"UPDATE spaces SET slug = $2, name = $3, description = $4, version = version + 1
           WHERE id = $1"#,
        id,
        settings.slug,
        settings.name,
        settings.description,
    )
    .execute(&mut *tx)
    .await;
    match updated {
        Ok(done) if done.rows_affected() == 0 => return Ok(Ok(false)),
        Ok(_) => {}
        Err(err) if slug_clash(&err) => return Ok(Err(SPACE_SLUG_TAKEN.to_owned())),
        Err(err) => return Err(err),
    }
    for (verb, grant) in settings.grants() {
        upsert_grant(&mut tx, id, verb, grant).await?;
    }
    tx.commit().await?;
    Ok(Ok(true))
}

/// Archives a space (read-only, out of the index) or opens it again.
/// Returns whether anything changed. The feed of a member who deleted
/// their account stays archived: nobody owns it now (DESIGN.md §3.2).
pub async fn set_archived(db: &crate::db::Db, id: i64, archived: bool) -> sqlx::Result<bool> {
    let done = sqlx::query!(
        "UPDATE spaces c SET archived_at = CASE WHEN $2 THEN now() END, version = version + 1
         WHERE c.id = $1 AND (c.archived_at IS NULL) = $2
           AND ($2 OR NOT EXISTS (SELECT 1 FROM users u
                                  WHERE u.id = c.owner_user_id AND u.deleted_at IS NOT NULL))",
        id,
        archived
    )
    .execute(db.pool())
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Deletes a space that has never had threads. Returns the problem if
/// it has some or is a feed, which every member keeps (DESIGN.md §3.2),
/// and `Ok(Ok(false))` if there is no such space.
pub async fn delete_space(db: &crate::db::Db, id: i64) -> sqlx::Result<Result<bool, String>> {
    let mut tx = db.pool().begin().await?;
    let feed = sqlx::query_scalar!(
        r#"SELECT kind = 'feed' AS "feed!" FROM spaces WHERE id = $1"#,
        id
    )
    .fetch_optional(&mut *tx)
    .await?;
    if feed == Some(true) {
        return Ok(Err(FEED_NOT_DELETED.to_owned()));
    }
    let threads = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM threads WHERE space_id = $1"#,
        id
    )
    .fetch_one(&mut *tx)
    .await?;
    if threads > 0 {
        return Ok(Err(
            "Only an empty space can be deleted. Move its threads elsewhere first.".to_owned(),
        ));
    }
    let done = sqlx::query!("DELETE FROM spaces WHERE id = $1", id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Ok(done.rows_affected() == 1))
}
/// Moves a thread to another space, returning the space it was in,
/// or `None` if there is no such thread in a space. Its visibility
/// follows its new space (a trigger sees to that).
pub async fn move_thread<'c>(
    db: impl Handle<'c>,
    thread_id: i64,
    space_id: i64,
) -> sqlx::Result<Option<i64>> {
    let mut tx = db.open(KEY).begin().await?;
    let Some(previous) = sqlx::query_scalar!(
        r#"SELECT space_id AS "space_id!" FROM threads
           WHERE id = $1 AND space_id IS NOT NULL FOR UPDATE"#,
        thread_id
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(None);
    };
    sqlx::query!(
        "UPDATE threads SET space_id = $2, version = version + 1 WHERE id = $1",
        thread_id,
        space_id
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(previous))
}

#[derive(Clone, Debug)]
pub struct NewThread {
    pub space_id: i64,
    pub title: String,
    pub author_id: i64,
    pub body_md: String,
    pub forked_from_post_id: Option<i64>,
    pub pinned: bool,
    pub locked: bool,
    pub created_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CreatedThread {
    pub thread_id: i64,
    pub first_post_id: i64,
}

/// Creates a thread together with its first post. The author has read
/// that post, having written it, so the thread isn't new to them, and is
/// subscribed to the thread (DESIGN.md §9).
pub async fn create_thread<'c>(
    db: impl Handle<'c>,
    thread: &NewThread,
) -> sqlx::Result<CreatedThread> {
    let mut tx = db.open(KEY).begin().await?;
    let created_at = thread.created_at.unwrap_or_else(Utc::now);
    let thread_id = sqlx::query_scalar!(
        r#"INSERT INTO threads (space_id, visibility, title, slug, author_id, forked_from_post_id,
                                pinned, locked, created_at, last_post_at)
           VALUES ($1, 'public', $2, $3, $4, $5, $6, $7, $8, $8)
           RETURNING id"#,
        thread.space_id,
        thread.title,
        slugify(&thread.title),
        thread.author_id,
        thread.forked_from_post_id,
        thread.pinned,
        thread.locked,
        created_at,
    )
    .fetch_one(&mut *tx)
    .await?;
    let first_post_id = insert_post(
        &mut tx,
        &NewPost {
            thread_id,
            author_id: thread.author_id,
            body_md: thread.body_md.clone(),
            reply_to_post_id: None,
            created_at: Some(created_at),
        },
    )
    .await?;
    mark_read(&mut *tx, thread.author_id, thread_id, 1).await?;
    social::set_subscribed(&mut *tx, thread.author_id, thread_id, true).await?;
    tx.commit().await?;
    Ok(CreatedThread {
        thread_id,
        first_post_id,
    })
}

#[derive(Clone, Debug)]
pub struct NewPost {
    pub thread_id: i64,
    pub author_id: i64,
    pub body_md: String,
    pub reply_to_post_id: Option<i64>,
    pub created_at: Option<DateTime<Utc>>,
}

/// Adds a post to the end of a thread.
pub async fn create_post<'c>(db: impl Handle<'c>, post: &NewPost) -> sqlx::Result<i64> {
    let mut tx = db.open(KEY).begin().await?;
    let id = insert_post(&mut tx, post).await?;
    tx.commit().await?;
    Ok(id)
}

async fn insert_post(tx: &mut sqlx::PgConnection, post: &NewPost) -> sqlx::Result<i64> {
    let created_at = post.created_at.unwrap_or_else(Utc::now);
    // Taking the next number locks the thread row, so concurrent posts to
    // one thread are numbered one after another.
    let seq = sqlx::query_scalar!(
        r#"UPDATE threads
           SET last_seq = last_seq + 1, last_post_at = greatest(last_post_at, $2), version = version + 1
           WHERE id = $1
           RETURNING last_seq"#,
        post.thread_id,
        created_at,
    )
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query_scalar!(
        r#"INSERT INTO posts (thread_id, seq, author_id, reply_to_post_id, body_md, body_html, created_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7)
           RETURNING id"#,
        post.thread_id,
        seq,
        post.author_id,
        post.reply_to_post_id,
        post.body_md,
        render_post(&mut *tx, &post.body_md).await?.html,
        created_at,
    )
    .fetch_one(&mut *tx)
    .await
}

/// Renders a post body as it will be saved, linking mentions of members
/// who exist now.
pub async fn render_post<'c>(
    db: impl Handle<'c>,
    body_md: &str,
) -> sqlx::Result<markdown::Rendered> {
    let candidates = markdown::mention_candidates(body_md);
    let known: Vec<String> = if candidates.is_empty() {
        Vec::new()
    } else {
        let mut conn = db.open(KEY).acquire().await?;
        sqlx::query_scalar!(
            r#"SELECT username::text AS "username!" FROM users
               WHERE lower(username::text) = ANY($1) AND deleted_at IS NULL"#,
            &candidates
        )
        .fetch_all(&mut *conn)
        .await?
    };
    // Rendering takes long enough that it shouldn't hold up other requests.
    let body_md = body_md.to_owned();
    let rendered = tokio::task::spawn_blocking(move || {
        markdown::render_with_mentions(&body_md, |name| {
            known
                .iter()
                .find(|known| known.eq_ignore_ascii_case(name))
                .cloned()
        })
    })
    .await
    .expect("rendering a post panicked");
    Ok(rendered)
}

// ---------------------------------------------------------------------------
// Conversations (DESIGN.md §3.7)

#[derive(Clone, Debug)]
pub struct NewConversation {
    pub author_id: i64,
    /// May be empty: the conversation is then named after its people.
    pub title: String,
    /// Everyone else in it.
    pub recipient_ids: Vec<i64>,
    pub body_md: String,
    pub created_at: Option<DateTime<Utc>>,
}

/// Starts a conversation: a private thread, its participants (the author
/// and the recipients, each subscribed to it) and its first message.
pub async fn create_conversation<'c>(
    db: impl Handle<'c>,
    conversation: &NewConversation,
) -> sqlx::Result<CreatedThread> {
    let mut tx = db.open(KEY).begin().await?;
    let created_at = conversation.created_at.unwrap_or_else(Utc::now);
    let thread_id = sqlx::query_scalar!(
        r#"INSERT INTO threads (space_id, visibility, title, slug, author_id, created_at, last_post_at)
           VALUES (NULL, 'private', $1, 'message', $2, $3, $3)
           RETURNING id"#,
        conversation.title,
        conversation.author_id,
        created_at,
    )
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query!(
        r#"INSERT INTO private_thread_participants (thread_id, user_id, joined_at, added_by)
           SELECT $1, u, $3, CASE WHEN u = $2 THEN NULL ELSE $2 END
           FROM unnest(array_append($4::bigint[], $2)) AS u
           ON CONFLICT DO NOTHING"#,
        thread_id,
        conversation.author_id,
        created_at,
        &conversation.recipient_ids,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "INSERT INTO thread_subscriptions (user_id, thread_id)
         SELECT user_id, thread_id FROM private_thread_participants WHERE thread_id = $1",
        thread_id,
    )
    .execute(&mut *tx)
    .await?;
    let first_post_id = insert_post(
        &mut tx,
        &NewPost {
            thread_id,
            author_id: conversation.author_id,
            body_md: conversation.body_md.clone(),
            reply_to_post_id: None,
            created_at: Some(created_at),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(CreatedThread {
        thread_id,
        first_post_id,
    })
}

/// Adds someone to a conversation, or back to one they left, subscribed
/// to it. Returns whether they weren't in it already.
pub async fn add_participant<'c>(
    db: impl Handle<'c>,
    thread_id: i64,
    user_id: i64,
    added_by: i64,
) -> sqlx::Result<bool> {
    let mut tx = db.open(KEY).begin().await?;
    let done = sqlx::query!(
        r#"INSERT INTO private_thread_participants (thread_id, user_id, added_by) VALUES ($1, $2, $3)
           ON CONFLICT (thread_id, user_id)
           DO UPDATE SET left_at = NULL, joined_at = now(), added_by = excluded.added_by
           WHERE private_thread_participants.left_at IS NOT NULL"#,
        thread_id,
        user_id,
        added_by,
    )
    .execute(&mut *tx)
    .await?;
    let added = done.rows_affected() == 1;
    if added {
        social::set_subscribed(&mut *tx, user_id, thread_id, true).await?;
    }
    tx.commit().await?;
    Ok(added)
}

/// Takes someone out of a conversation, and so unsubscribes them from it.
/// Returns whether they were in it.
pub async fn leave_conversation<'c>(
    db: impl Handle<'c>,
    thread_id: i64,
    user_id: i64,
) -> sqlx::Result<bool> {
    let mut tx = db.open(KEY).begin().await?;
    let done = sqlx::query!(
        "UPDATE private_thread_participants SET left_at = now()
         WHERE thread_id = $1 AND user_id = $2 AND left_at IS NULL",
        thread_id,
        user_id,
    )
    .execute(&mut *tx)
    .await?;
    social::set_subscribed(&mut *tx, user_id, thread_id, false).await?;
    tx.commit().await?;
    Ok(done.rows_affected() == 1)
}

/// Edits made this soon after posting replace the original rather than
/// adding to the post's history (DESIGN.md §3.3).
pub const EDIT_GRACE_MINUTES: i32 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edited {
    /// The text was the same, so nothing changed.
    Unchanged,
    /// The original was replaced, within the grace period.
    Replaced,
    /// The previous version was kept as revision `rev`.
    Revised { rev: i32 },
}

/// Replaces a post's text, keeping the version it replaces in
/// `post_revisions` unless the post is still in its grace period and has
/// never been edited since. Doesn't check permissions.
pub async fn edit_post<'c>(
    db: impl Handle<'c>,
    post_id: i64,
    editor_id: i64,
    body_md: &str,
) -> sqlx::Result<Edited> {
    let mut tx = db.open(KEY).begin().await?;
    let post = sqlx::query!(
        r#"SELECT body_md, body_html, author_id, created_at, edited_at, edited_by,
                  created_at > now() - make_interval(mins => $2) AS "in_grace!"
           FROM posts WHERE id = $1 FOR UPDATE"#,
        post_id,
        EDIT_GRACE_MINUTES,
    )
    .fetch_one(&mut *tx)
    .await?;
    if post.body_md == body_md {
        return Ok(Edited::Unchanged);
    }
    let html = render_post(&mut tx, body_md).await?.html;
    let outcome = if post.in_grace && post.edited_at.is_none() && editor_id == post.author_id {
        sqlx::query!(
            "UPDATE posts SET body_md = $2, body_html = $3 WHERE id = $1",
            post_id,
            body_md,
            html
        )
        .execute(&mut *tx)
        .await?;
        Edited::Replaced
    } else {
        let rev = sqlx::query_scalar!(
            r#"INSERT INTO post_revisions (post_id, rev, body_md, body_html, editor_id, created_at)
               SELECT $1, coalesce(max(rev), 0) + 1, $2, $3, $4, $5 FROM post_revisions WHERE post_id = $1
               RETURNING rev"#,
            post_id,
            post.body_md,
            post.body_html,
            post.edited_by.unwrap_or(post.author_id),
            post.edited_at.unwrap_or(post.created_at),
        )
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query!(
            r#"UPDATE posts SET body_md = $2, body_html = $3, edited_at = now(),
                      edited_by = CASE WHEN author_id = $4 THEN NULL ELSE $4 END
               WHERE id = $1"#,
            post_id,
            body_md,
            html,
            editor_id,
        )
        .execute(&mut *tx)
        .await?;
        Edited::Revised { rev }
    };
    tx.commit().await?;
    Ok(outcome)
}

/// Deletes a post, leaving a tombstone. Its number, history and reactions
/// stay in the database but are no longer shown.
pub async fn delete_post<'c>(db: impl Handle<'c>, post_id: i64) -> sqlx::Result<bool> {
    let mut conn = db.open(KEY).acquire().await?;
    let done = sqlx::query!(
        "UPDATE posts SET deleted_at = now() WHERE id = $1 AND deleted_at IS NULL",
        post_id
    )
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Adds a reaction; adding one twice does nothing. Returns whether it was
/// added.
pub async fn add_reaction<'c>(
    db: impl Handle<'c>,
    post_id: i64,
    user_id: i64,
    emoji: &str,
) -> sqlx::Result<bool> {
    let mut conn = db.open(KEY).acquire().await?;
    let done = sqlx::query!(
        "INSERT INTO reactions (post_id, user_id, emoji) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        post_id,
        user_id,
        emoji
    )
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Removes a reaction, returning whether there was one.
pub async fn remove_reaction<'c>(
    db: impl Handle<'c>,
    post_id: i64,
    user_id: i64,
    emoji: &str,
) -> sqlx::Result<bool> {
    let mut conn = db.open(KEY).acquire().await?;
    let done = sqlx::query!(
        "DELETE FROM reactions WHERE post_id = $1 AND user_id = $2 AND emoji = $3",
        post_id,
        user_id,
        emoji
    )
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Counts a new post as read by its author if they had read everything
/// before it, so that their own post isn't new to them. If others posted
/// in between that they haven't seen, those (and so this) stay new.
pub async fn read_own_post<'c>(db: impl Handle<'c>, post_id: i64) -> sqlx::Result<()> {
    let mut conn = db.open(KEY).acquire().await?;
    sqlx::query!(
        r#"UPDATE thread_reads tr SET last_read_seq = p.seq
           FROM posts p
           WHERE p.id = $1 AND tr.user_id = p.author_id AND tr.thread_id = p.thread_id
             AND tr.last_read_seq = p.seq - 1"#,
        post_id
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Moves a member's read marker in a thread forward to `seq`. It never
/// moves back.
pub async fn mark_read<'c>(
    db: impl Handle<'c>,
    user_id: i64,
    thread_id: i64,
    seq: i64,
) -> sqlx::Result<()> {
    let mut conn = db.open(KEY).acquire().await?;
    let seq = i32::try_from(seq.max(0)).unwrap_or(i32::MAX);
    sqlx::query!(
        r#"INSERT INTO thread_reads (user_id, thread_id, last_read_seq) VALUES ($1, $2, $3)
           ON CONFLICT (user_id, thread_id)
           DO UPDATE SET last_read_seq = greatest(thread_reads.last_read_seq, excluded.last_read_seq)
           WHERE thread_reads.last_read_seq < excluded.last_read_seq"#,
        user_id,
        thread_id,
        seq
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Marks every thread in a space, as listed in `thread_ids`, as read up
/// to its last post.
pub async fn mark_threads_read<'c>(
    db: impl Handle<'c>,
    user_id: i64,
    thread_ids: &[i64],
) -> sqlx::Result<()> {
    let mut conn = db.open(KEY).acquire().await?;
    sqlx::query!(
        r#"INSERT INTO thread_reads (user_id, thread_id, last_read_seq)
           SELECT $1, t.id, t.last_seq FROM threads t WHERE t.id = ANY($2)
           ON CONFLICT (user_id, thread_id)
           DO UPDATE SET last_read_seq = greatest(thread_reads.last_read_seq, excluded.last_read_seq)
           WHERE thread_reads.last_read_seq < excluded.last_read_seq"#,
        user_id,
        thread_ids
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Whether text can be offered as a reaction: a short run of symbols,
/// such as an emoji, with no letters, digits, spaces or markup.
pub fn valid_reaction(emoji: &str) -> bool {
    !emoji.is_empty()
        && emoji.chars().count() <= 16
        && emoji.chars().all(|c| {
            !c.is_alphanumeric() && !c.is_whitespace() && !c.is_control() && !"<>&\"'`".contains(c)
        })
}

/// Changes the reaction types, as the admin asked: `add` a new one, `enable`
/// or `disable` one, move it `up` or `down`, or `delete` one nobody has
/// used. At most `max` can be enabled at once. Returns the problem, if any,
/// to show the admin.
pub async fn change_reaction_type<'c>(
    db: impl Handle<'c>,
    action: &str,
    emoji: &str,
    max: usize,
) -> sqlx::Result<Result<(), String>> {
    let mut tx = db.open(KEY).begin().await?;
    // One change at a time, so that the limit and positions hold.
    sqlx::query!("LOCK TABLE reaction_types IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;
    let types = sqlx::query!("SELECT emoji, enabled FROM reaction_types ORDER BY position, emoji")
        .fetch_all(&mut *tx)
        .await?;
    let enabled = types.iter().filter(|t| t.enabled).count();
    let index = types.iter().position(|t| t.emoji == emoji);
    let full = format!("At most {max} reactions can be in use at once. Turn one off first.");
    let problem = match (action, index) {
        ("add", Some(_)) => Some("That reaction is already in the list.".to_owned()),
        ("add", None) if !valid_reaction(emoji) => {
            Some("A reaction should be an emoji or a symbol of a few characters, with no letters or spaces.".to_owned())
        }
        ("add", None) if enabled >= max => Some(full),
        ("add", None) => {
            sqlx::query!(
                "INSERT INTO reaction_types (emoji, position)
                 SELECT $1, coalesce(max(position), 0) + 1 FROM reaction_types",
                emoji
            )
            .execute(&mut *tx)
            .await?;
            None
        }
        ("enable", Some(i)) if !types[i].enabled && enabled >= max => Some(full),
        ("enable" | "disable", Some(_)) => {
            sqlx::query!(
                "UPDATE reaction_types SET enabled = $2 WHERE emoji = $1",
                emoji,
                action == "enable"
            )
            .execute(&mut *tx)
            .await?;
            None
        }
        ("up" | "down", Some(i)) => {
            let other = if action == "up" { i.checked_sub(1) } else { Some(i + 1) };
            if let Some(other) = other.filter(|&o| o < types.len()) {
                let mut order: Vec<&str> = types.iter().map(|t| t.emoji.as_str()).collect();
                order.swap(i, other);
                for (position, emoji) in order.iter().enumerate() {
                    sqlx::query!(
                        "UPDATE reaction_types SET position = $2 WHERE emoji = $1",
                        emoji,
                        position as i32 + 1
                    )
                    .execute(&mut *tx)
                    .await?;
                }
            }
            None
        }
        ("delete", Some(_)) => {
            let used = sqlx::query_scalar!(
                r#"SELECT EXISTS (SELECT 1 FROM reactions WHERE emoji = $1) AS "used!""#,
                emoji
            )
            .fetch_one(&mut *tx)
            .await?;
            if used {
                Some("Members have used that reaction, so it can only be turned off.".to_owned())
            } else {
                sqlx::query!("DELETE FROM reaction_types WHERE emoji = $1", emoji)
                    .execute(&mut *tx)
                    .await?;
                None
            }
        }
        _ => Some("Choose a reaction from the list.".to_owned()),
    };
    if problem.is_none() {
        tx.commit().await?;
    }
    Ok(problem.map_or(Ok(()), Err))
}

#[cfg(test)]
mod tests {
    use super::{RESERVED_SLUGS, slugify};
    use hegel::TestCase;
    use hegel::generators as gs;

    #[test]
    fn slugs_are_readable() {
        assert_eq!(
            slugify("Do solo modes count as ‘playing’ a game?"),
            "do-solo-modes-count-as-playing-a-game"
        );
        assert_eq!(
            slugify("What did you play this week? (September)"),
            "what-did-you-play-this-week-september"
        );
        assert_eq!(slugify("  ?? "), "thread");
        assert_eq!(slugify("Ünïcødé"), "n-c-d");
        assert_eq!(slugify("Unread!"), "unread-thread");
        assert_eq!(slugify("Reply"), "reply-thread");
        assert_eq!(slugify("Subscribe"), "subscribe-thread");
        assert_eq!(slugify("Unsubscribe?"), "unsubscribe-thread");
    }

    #[hegel::test(test_cases = 500)]
    fn slugs_are_url_safe_and_stable(tc: TestCase) {
        let title = tc.draw(gs::text());
        let slug = slugify(&title);
        assert!(!slug.is_empty() && slug.len() <= 67, "{slug:?}");
        for route in RESERVED_SLUGS {
            assert_ne!(slug, *route);
        }
        assert!(
            !slug.starts_with('-') && !slug.ends_with('-') && !slug.contains("--"),
            "{slug:?}"
        );
        assert!(
            slug.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "{slug:?}"
        );
        assert_eq!(slugify(&slug), slug);
    }
}
