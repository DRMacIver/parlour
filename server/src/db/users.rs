//! Account records: finding accounts to log in to, passwords, email
//! addresses, roles and the first admin (DESIGN.md §7).
//!
//! What members show of themselves to others (profiles, the invite tree)
//! is in [`crate::members`]; what they post is in [`crate::content`].

use chrono::{DateTime, Utc};
use sqlx::Acquire;

use crate::accounts::Role;
use crate::db::{Db, Handle, KEY};
use crate::sessions;
use crate::tokens;

/// The account a login form names, by username or email address.
pub struct LoginAccount {
    pub id: i64,
    pub display_name: String,
    pub email: String,
    pub password_hash: Option<String>,
}

/// The account with this username or, failing that, this email address.
pub async fn find_login_account(db: &Db, identifier: &str) -> sqlx::Result<Option<LoginAccount>> {
    let identifier = identifier.trim();
    if identifier.is_empty() || identifier.len() > 254 {
        return Ok(None);
    }
    let row = sqlx::query!(
        r#"SELECT id, display_name, email AS "email: String", password_hash FROM users
           WHERE (username = $1::citext OR email = $1::citext) AND deleted_at IS NULL
           ORDER BY username = $1::citext DESC
           LIMIT 1"#,
        identifier as _
    )
    .fetch_optional(db.pool())
    .await?;
    Ok(row.map(|row| LoginAccount {
        id: row.id,
        display_name: row.display_name,
        email: row.email,
        password_hash: row.password_hash,
    }))
}

/// The id of the member with this username (any capitalisation).
pub async fn id_by_username(db: &Db, username: &str) -> sqlx::Result<Option<i64>> {
    if username.is_empty() || username.len() > 64 {
        return Ok(None);
    }
    sqlx::query_scalar!(
        "SELECT id FROM users WHERE username = $1::citext AND deleted_at IS NULL",
        username as _
    )
    .fetch_optional(db.pool())
    .await
}

/// A member, named.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamedUser {
    pub id: i64,
    pub username: String,
}

/// The members with these usernames (any capitalisation), skipping names
/// nobody has.
pub async fn by_usernames(db: &Db, names: &[String]) -> sqlx::Result<Vec<NamedUser>> {
    let names: Vec<String> = names.iter().map(|name| name.to_lowercase()).collect();
    sqlx::query_as!(
        NamedUser,
        r#"SELECT id, username::text AS "username!" FROM users
           WHERE lower(username::text) = ANY($1) AND deleted_at IS NULL"#,
        &names
    )
    .fetch_all(db.pool())
    .await
}

/// The usernames of these members.
pub async fn usernames_of(db: &Db, ids: &[i64]) -> sqlx::Result<Vec<NamedUser>> {
    sqlx::query_as!(
        NamedUser,
        r#"SELECT id, username::text AS "username!" FROM users WHERE id = ANY($1)"#,
        ids
    )
    .fetch_all(db.pool())
    .await
}

/// Records that the member can read their email (a magic link or reset
/// link reached them).
pub async fn mark_verified(db: &Db, user_id: i64) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE users SET email_verified_at = coalesce(email_verified_at, now()) WHERE id = $1",
        user_id
    )
    .execute(db.pool())
    .await?;
    Ok(())
}

/// The member's password hash, if they have a password.
pub async fn password_hash(db: &Db, user_id: i64) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar!("SELECT password_hash FROM users WHERE id = $1", user_id)
        .fetch_one(db.pool())
        .await
}

/// Sets a member's password. With `verified`, it also records that their
/// email works, as after a reset link.
pub async fn set_password<'c>(
    db: impl Handle<'c>,
    user_id: i64,
    hash: &str,
    verified: bool,
) -> sqlx::Result<()> {
    let mut conn = db.open(KEY).acquire().await?;
    sqlx::query!(
        "UPDATE users SET password_hash = $2,
                email_verified_at = CASE WHEN $3 THEN coalesce(email_verified_at, now()) ELSE email_verified_at END
         WHERE id = $1",
        user_id,
        hash,
        verified,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Moves a member to a confirmed address. Returns false, changing nothing,
/// if another account has the address.
pub async fn confirm_email<'c>(
    db: impl Handle<'c>,
    user_id: i64,
    email: Option<&str>,
) -> sqlx::Result<bool> {
    let mut conn = db.open(KEY).acquire().await?;
    let mut tx = conn.begin().await?;
    let updated = sqlx::query!(
        "UPDATE users SET email = coalesce($2, email), email_verified_at = now() WHERE id = $1",
        user_id,
        email as _,
    )
    .execute(&mut *tx)
    .await;
    match updated {
        Ok(_) => {
            tx.commit().await?;
            Ok(true)
        }
        Err(sqlx::Error::Database(err)) if err.is_unique_violation() => Ok(false),
        Err(err) => Err(err),
    }
}

/// Whether any account has this email address.
pub async fn email_taken(db: &Db, email: &str) -> sqlx::Result<bool> {
    sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM users WHERE email = $1::citext) AS "e!""#,
        email as _
    )
    .fetch_one(db.pool())
    .await
}

/// What the account settings page shows about the account.
pub struct AccountSummary {
    pub has_password: bool,
    /// Current sessions other than the one with `token`.
    pub other_sessions: i64,
}

pub async fn account_summary(
    db: &Db,
    user_id: i64,
    token: Option<&str>,
) -> sqlx::Result<AccountSummary> {
    let row = sqlx::query!(
        r#"SELECT password_hash IS NOT NULL AS "has_password!",
                  (SELECT count(*) FROM sessions s WHERE s.user_id = u.id AND s.token_hash IS DISTINCT FROM $2
                     AND s.last_seen_at > now() - make_interval(days => $3)) AS "sessions!"
           FROM users u WHERE u.id = $1"#,
        user_id,
        token.map(tokens::hash),
        sessions::IDLE_DAYS,
    )
    .fetch_one(db.pool())
    .await?;
    Ok(AccountSummary {
        has_password: row.has_password,
        other_sessions: row.sessions,
    })
}

// ---------------------------------------------------------------------------
// Members and roles, for admins

/// One member in the admin list.
pub struct AdminMember {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub role: Role,
    pub joined_at: DateTime<Utc>,
    pub inviter: Option<String>,
}

pub async fn member_count(db: &Db) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM users WHERE deleted_at IS NULL"#)
        .fetch_one(db.pool())
        .await
}

/// Members in username order.
pub async fn members(db: &Db, limit: i64, offset: i64) -> sqlx::Result<Vec<AdminMember>> {
    let rows = sqlx::query!(
        r#"SELECT u.id, u.username AS "username: String", u.display_name, u.role, u.created_at,
                  i.username AS "inviter?: String"
           FROM users u LEFT JOIN users i ON i.id = u.invited_by_user_id
           WHERE u.deleted_at IS NULL
           ORDER BY u.username
           LIMIT $1 OFFSET $2"#,
        limit,
        offset,
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| AdminMember {
            id: row.id,
            username: row.username,
            display_name: row.display_name,
            role: Role::parse(&row.role).unwrap_or(Role::User),
            joined_at: row.created_at,
            inviter: row.inviter,
        })
        .collect())
}

/// What became of a role change.
#[derive(Debug, PartialEq, Eq)]
pub enum RoleChange {
    /// Done; the member had this role before.
    Done {
        from: Role,
    },
    NoSuchMember,
    /// It would leave the site without an admin.
    LastAdmin,
}

pub async fn change_role(db: &Db, user_id: i64, role: Role) -> sqlx::Result<RoleChange> {
    let mut tx = db.pool().begin().await?;
    // Lock the admins first, so that two admins demoting each other at
    // once can't leave none.
    let admins = sqlx::query_scalar!(
        "SELECT id FROM users WHERE role = 'admin' AND deleted_at IS NULL ORDER BY id FOR UPDATE"
    )
    .fetch_all(&mut *tx)
    .await?;
    let Some(current) = sqlx::query_scalar!(
        "SELECT role FROM users WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
        user_id
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(RoleChange::NoSuchMember);
    };
    let from = Role::parse(&current).unwrap_or(Role::User);
    if from == Role::Admin && role != Role::Admin && admins.len() <= 1 {
        return Ok(RoleChange::LastAdmin);
    }
    sqlx::query!(
        "UPDATE users SET role = $2 WHERE id = $1",
        user_id,
        role.as_str()
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(RoleChange::Done { from })
}

// ---------------------------------------------------------------------------
// The first admin

/// Whether there is an admin yet.
pub async fn has_admin<'c>(db: impl Handle<'c>) -> sqlx::Result<bool> {
    let mut conn = db.open(KEY).acquire().await?;
    sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM users WHERE role = 'admin' AND deleted_at IS NULL) AS "e!""#
    )
    .fetch_one(&mut *conn)
    .await
}

pub struct NewAdmin<'a> {
    pub username: &'a str,
    pub display_name: &'a str,
    pub email: &'a str,
    pub password_hash: &'a str,
}

/// What became of creating the first admin.
#[derive(Debug, PartialEq, Eq)]
pub enum FirstAdmin {
    Created(i64),
    /// There is an admin already.
    NotNeeded,
    /// The username or email address is taken.
    Taken,
}

/// Creates the first admin in `tx`, unless someone else got there first.
pub async fn create_first_admin(
    tx: &mut crate::db::Tx,
    admin: &NewAdmin<'_>,
) -> sqlx::Result<FirstAdmin> {
    let conn = tx.conn();
    // Two people racing through setup can't both make an admin.
    sqlx::query!("SELECT pg_advisory_xact_lock(hashtext('parlour setup'))")
        .execute(&mut *conn)
        .await?;
    if has_admin(&mut *conn).await? {
        return Ok(FirstAdmin::NotNeeded);
    }
    // A savepoint, so that a clash leaves the transaction usable.
    let mut savepoint = conn.begin().await?;
    let inserted = sqlx::query_scalar!(
        "INSERT INTO users (username, display_name, email, email_verified_at, password_hash, role)
         VALUES ($1, $2, $3, now(), $4, 'admin')
         RETURNING id",
        admin.username as _,
        admin.display_name,
        admin.email as _,
        admin.password_hash,
    )
    .fetch_one(&mut *savepoint)
    .await;
    match inserted {
        Ok(id) => {
            savepoint.commit().await?;
            crate::layouts::create_member_copy(&mut *conn, id).await?;
            Ok(FirstAdmin::Created(id))
        }
        Err(sqlx::Error::Database(err)) if err.is_unique_violation() => Ok(FirstAdmin::Taken),
        Err(err) => Err(err),
    }
}

// ---------------------------------------------------------------------------
// The post-info column

/// What a member shows beside their posts, as they last saved it.
pub async fn profile_fields<'c>(
    db: impl Handle<'c>,
    user_id: i64,
) -> sqlx::Result<crate::accounts::ProfileFields> {
    let mut conn = db.open(KEY).acquire().await?;
    let row = sqlx::query!(
        "SELECT display_name, title, pronouns, status_line, location FROM users WHERE id = $1",
        user_id
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok(crate::accounts::ProfileFields {
        display_name: row.display_name,
        title: row.title,
        pronouns: row.pronouns,
        status_line: row.status_line,
        location: row.location,
    })
}

/// Saves a member's post-info column, which the caller has checked with
/// [`crate::accounts::ProfileFields::clean`].
pub async fn update_profile<'c>(
    db: impl Handle<'c>,
    user_id: i64,
    fields: &crate::accounts::ProfileFields,
) -> sqlx::Result<()> {
    let mut conn = db.open(KEY).acquire().await?;
    sqlx::query!(
        "UPDATE users SET display_name = $2, title = $3, pronouns = $4, status_line = $5, location = $6
         WHERE id = $1",
        user_id,
        fields.display_name,
        fields.title,
        fields.pronouns,
        fields.status_line,
        fields.location,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Deleting an account

/// What became of deleting an account.
#[derive(Debug, PartialEq, Eq)]
pub enum Deletion {
    /// With the file of their picture, as its hash and extension, if
    /// nothing else shows it: the caller deletes it once this is
    /// committed.
    Deleted {
        unused_picture: Option<(String, String)>,
    },
    NoSuchMember,
    /// It would leave the site without an admin.
    LastAdmin,
}

/// Deletes a member's account at their request (DESIGN.md §2). What they
/// wrote stays where it is, under "Deleted member": the account's row is
/// kept, with everything that identified them (names, email address,
/// password, profile, picture) replaced or removed, and so are its posts.
/// The old username is retired, so that nobody new can take it and pass as
/// them in the posts that mention or quote them.
/// Their picture is deleted too, unless a post shows the same image.
/// Everything else that is theirs alone goes: sessions, tokens, recorded
/// addresses, notifications, reactions, thread subscriptions, blocks, read markers,
/// group memberships, their front page and mutes, and staff notes about
/// them. They leave their conversations, unused invites are revoked, and
/// posts waiting for approval are rejected.
pub async fn delete_account(db: &Db, user_id: i64) -> sqlx::Result<Deletion> {
    let mut tx = db.pool().begin().await?;
    let admins = sqlx::query_scalar!(
        "SELECT id FROM users WHERE role = 'admin' AND deleted_at IS NULL ORDER BY id FOR UPDATE"
    )
    .fetch_all(&mut *tx)
    .await?;
    let Some(role) = sqlx::query_scalar!(
        "SELECT role FROM users WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
        user_id
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(Deletion::NoSuchMember);
    };
    if role == Role::Admin.as_str() && admins.len() <= 1 {
        return Ok(Deletion::LastAdmin);
    }
    // Their picture goes, unless a post shows the same image: then it
    // stays, as part of the post.
    let picture = sqlx::query!(
        r#"DELETE FROM uploads up USING users u
           WHERE u.id = $1 AND up.id = u.avatar_upload_id
             AND NOT EXISTS (SELECT 1 FROM post_uploads x WHERE x.sha256 = up.sha256)
             AND NOT EXISTS (SELECT 1 FROM revision_uploads x WHERE x.sha256 = up.sha256)
             AND NOT EXISTS (SELECT 1 FROM held_post_uploads x WHERE x.sha256 = up.sha256)
           RETURNING up.sha256, up.ext,
                     NOT EXISTS (SELECT 1 FROM uploads o WHERE o.sha256 = up.sha256 AND o.id <> up.id)
                         AS "unused!""#,
        user_id
    )
    .fetch_optional(&mut *tx)
    .await?;
    sqlx::query!(
        "INSERT INTO retired_usernames (username_hash)
         SELECT username_hash(username::text) FROM users WHERE id = $1
         ON CONFLICT DO NOTHING",
        user_id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE users SET username = 'deleted_' || id, display_name = 'Deleted member',
                email = 'deleted-' || id || '@deleted.invalid', email_verified_at = NULL,
                password_hash = NULL, role = 'user', title = NULL, pronouns = NULL,
                status_line = NULL, location = NULL, avatar_upload_id = NULL,
                dm_policy = 'nobody', email_prefs = '{}', last_seen_at = NULL, deleted_at = now()
         WHERE id = $1",
        user_id
    )
    .execute(&mut *tx)
    .await?;
    macro_rules! remove {
        ($sql:literal) => {
            sqlx::query!($sql, user_id).execute(&mut *tx).await?;
        };
    }
    remove!("DELETE FROM sessions WHERE user_id = $1");
    remove!("DELETE FROM login_tokens WHERE user_id = $1");
    remove!("DELETE FROM user_ips WHERE user_id = $1");
    remove!("DELETE FROM notifications WHERE user_id = $1");
    remove!("DELETE FROM reactions WHERE user_id = $1");
    remove!("DELETE FROM thread_subscriptions WHERE user_id = $1");
    remove!("DELETE FROM blocks WHERE user_id = $1 OR blocked_user_id = $1");
    remove!("DELETE FROM thread_reads WHERE user_id = $1");
    remove!("DELETE FROM group_members WHERE user_id = $1");
    remove!("DELETE FROM layouts WHERE user_id = $1");
    remove!("DELETE FROM space_mutes WHERE user_id = $1");
    remove!("DELETE FROM user_notes WHERE user_id = $1");
    remove!("UPDATE uploads SET user_id = NULL WHERE user_id = $1");
    remove!(
        "UPDATE private_thread_participants SET left_at = now() WHERE user_id = $1 AND left_at IS NULL"
    );
    remove!("UPDATE invites SET revoked_at = now() WHERE created_by = $1 AND revoked_at IS NULL");
    remove!(
        "UPDATE held_posts SET status = 'rejected', decided_at = now() WHERE author_id = $1 AND status = 'held'"
    );
    tx.commit().await?;
    Ok(Deletion::Deleted {
        unused_picture: picture.filter(|p| p.unused).map(|p| (p.sha256, p.ext)),
    })
}
