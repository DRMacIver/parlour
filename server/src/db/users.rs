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
    Done,
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
    if current == Role::Admin.as_str() && role != Role::Admin && admins.len() <= 1 {
        return Ok(RoleChange::LastAdmin);
    }
    // M7: record this in mod_actions.
    sqlx::query!(
        "UPDATE users SET role = $2 WHERE id = $1",
        user_id,
        role.as_str()
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(RoleChange::Done)
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
            Ok(FirstAdmin::Created(id))
        }
        Err(sqlx::Error::Database(err)) if err.is_unique_violation() => Ok(FirstAdmin::Taken),
        Err(err) => Err(err),
    }
}
