//! Invites (DESIGN.md §3.6). An invite is a secret code in a link
//! (`/invite/{code}`); only a hash of the code is stored, so the link is
//! shown once, to whoever made it, or emailed to the person invited.
//!
//! - Members make single-use invites, optionally for one email address.
//!   Who may invite is a site setting ([`InvitePolicy`]).
//! - Staff can also make links with several uses that put everyone who
//!   joins with them into groups (the group ids are recorded now; groups
//!   arrive in M5).
//! - Registering records who invited the new member and with which invite.
//!   The database refuses to change either afterwards, so the invite tree is
//!   a permanent public record.

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;

use crate::accounts::{self, Role};
use crate::login_tokens::{self, Purpose};
use crate::settings::{InvitePolicy, RegistrationMode};
use crate::viewer::UserCtx;
use crate::{AppState, emails, format, mail, settings, tokens};

/// How many unused invites a member who isn't staff may have at once.
pub const MAX_PENDING: i64 = 10;
/// The most uses a staff link may have.
pub const MAX_USES: i32 = 1000;
/// The lifetimes offered, in days.
pub const EXPIRY_DAYS: &[i64] = &[1, 7, 30];
pub const STAFF_EXPIRY_DAYS: &[i64] = &[1, 7, 30, 90];

/// Why someone may not invite people.
pub fn may_invite(
    policy: InvitePolicy,
    mode: RegistrationMode,
    role: Role,
    account_age: Duration,
    email_verified: bool,
) -> Result<(), String> {
    if role.is_staff() {
        return Ok(());
    }
    if mode == RegistrationMode::Closed {
        return Err("Registration is closed at the moment, so invites can’t be used.".into());
    }
    if !email_verified {
        return Err(
            "Confirm your email address before inviting anyone: see your account settings.".into(),
        );
    }
    match policy {
        InvitePolicy::Everyone => Ok(()),
        InvitePolicy::Staff => {
            Err("At the moment only moderators and admins can invite people.".into())
        }
        InvitePolicy::MembersOlderThan { days } => {
            if account_age >= Duration::days(i64::from(days)) {
                Ok(())
            } else {
                Err(format!(
                    "Members can invite people once they have been here for {}.",
                    format::plural(i64::from(days), "day", "days")
                ))
            }
        }
    }
}

/// Whether this member may invite people now, and if not why not.
pub async fn check_may_invite(db: &PgPool, user: &UserCtx) -> sqlx::Result<Result<(), String>> {
    let policy = settings::invite_policy(db).await?;
    let mode = settings::registration_mode(db).await?;
    Ok(may_invite(
        policy,
        mode,
        user.role,
        Utc::now() - user.created_at,
        user.email_verified,
    ))
}

#[derive(Clone, Debug, Default)]
pub struct NewInvite {
    pub email: Option<String>,
    pub note: String,
    pub expires_in_days: i64,
    pub max_uses: i32,
    pub grant_group_ids: Vec<i64>,
}

#[derive(Debug)]
pub struct CreatedInvite {
    pub id: i64,
    pub code: String,
    pub url: String,
    pub emailed_to: Option<String>,
}

/// Makes an invite, after checking the creator may, and queues the email
/// if it is for an address. Returns the problems if it can't.
pub async fn create(
    state: &AppState,
    creator: &UserCtx,
    new: &NewInvite,
) -> anyhow::Result<Result<CreatedInvite, Vec<String>>> {
    let db = state.db();
    if let Err(why) = check_may_invite(db, creator).await? {
        return Ok(Err(vec![why]));
    }
    let staff = creator.role.is_staff();
    let mut problems = Vec::new();
    let email = match new
        .email
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
    {
        None => None,
        Some(email) => match accounts::clean_email(email) {
            Ok(email) => Some(email),
            Err(why) => {
                problems.push(why.to_owned());
                None
            }
        },
    };
    let allowed_days = if staff {
        STAFF_EXPIRY_DAYS
    } else {
        EXPIRY_DAYS
    };
    if !allowed_days.contains(&new.expires_in_days) {
        problems.push("Choose how long the invite lasts.".to_owned());
    }
    if new.max_uses < 1 || new.max_uses > MAX_USES || (!staff && new.max_uses != 1) {
        problems.push(if staff {
            format!("An invite link can be used between 1 and {MAX_USES} times.")
        } else {
            "Each invite can be used once.".to_owned()
        });
    }
    if email.is_some() && new.max_uses != 1 {
        problems.push("An invite sent to an email address can only be used once.".to_owned());
    }
    if !staff && !new.grant_group_ids.is_empty() {
        problems.push("Only staff can make invites that add people to groups.".to_owned());
    }
    let note = new.note.trim();
    if note.chars().count() > 200 {
        problems.push("Keep the note to 200 characters.".to_owned());
    }
    if let Some(email) = &email {
        let member = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM users WHERE email = $1::citext) AS "e!""#,
            email as _
        )
        .fetch_one(db)
        .await?;
        if member {
            problems.push("Someone with that email address is already a member.".to_owned());
        }
    }
    if !staff {
        let pending = sqlx::query_scalar!(
            r#"SELECT count(*) AS "n!" FROM invites
               WHERE created_by = $1 AND revoked_at IS NULL AND uses < max_uses AND expires_at > now()"#,
            creator.id
        )
        .fetch_one(db)
        .await?;
        if pending >= MAX_PENDING {
            problems.push(format!(
                "You already have {MAX_PENDING} invites waiting to be used. Revoke one, or wait for them to be used or expire."
            ));
        }
    }
    if !problems.is_empty() {
        return Ok(Err(problems));
    }

    let code = tokens::random_token(16);
    let url = state.config().url(&format!("/invite/{code}"));
    let mut tx = db.begin().await?;
    let invite = sqlx::query!(
        r#"INSERT INTO invites (code_hash, created_by, email, max_uses, expires_at, grant_group_ids, note)
           VALUES ($1, $2, $3, $4, now() + make_interval(days => $5), $6, $7)
           RETURNING id, expires_at"#,
        tokens::hash(&code),
        creator.id,
        email.as_deref() as _,
        new.max_uses,
        new.expires_in_days as i32,
        &new.grant_group_ids,
        note,
    )
    .fetch_one(&mut *tx)
    .await?;
    if let Some(email) = &email {
        let site_name = settings::site_name(db).await?;
        mail::queue(
            &mut tx,
            &emails::invite(
                email,
                &site_name,
                &creator.display_name,
                &url,
                &format::datetime(&invite.expires_at),
            ),
        )
        .await?;
    }
    tx.commit().await?;
    state.jobs().wake();
    Ok(Ok(CreatedInvite {
        id: invite.id,
        code,
        url,
        emailed_to: email,
    }))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InviteStatus {
    Valid,
    Used,
    Expired,
    Revoked,
}

impl InviteStatus {
    fn parse(s: &str) -> InviteStatus {
        match s {
            "valid" => InviteStatus::Valid,
            "used" => InviteStatus::Used,
            "revoked" => InviteStatus::Revoked,
            _ => InviteStatus::Expired,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            InviteStatus::Valid => "Waiting",
            InviteStatus::Used => "Used",
            InviteStatus::Expired => "Expired",
            InviteStatus::Revoked => "Revoked",
        }
    }
}

/// One of a member's invites, for `/invites`.
#[derive(Debug)]
pub struct InviteRow {
    pub id: i64,
    pub email: Option<String>,
    pub note: String,
    pub max_uses: i64,
    pub uses: i64,
    pub grant_group_ids: Vec<i64>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub status: InviteStatus,
    /// Who joined with it: (username, display name, joined).
    pub used_by: Vec<(String, String, DateTime<Utc>)>,
}

pub async fn list_for(db: &PgPool, user_id: i64) -> sqlx::Result<Vec<InviteRow>> {
    let rows = sqlx::query!(
        r#"SELECT i.id, i.email AS "email: String", i.note, i.max_uses, i.uses, i.grant_group_ids,
                  i.created_at, i.expires_at, invite_status(i.revoked_at, i.uses, i.max_uses, i.expires_at) AS "status!",
                  coalesce((SELECT array_agg(u.username::text ORDER BY u.created_at) FROM users u WHERE u.invite_id = i.id), '{}') AS "usernames!",
                  coalesce((SELECT array_agg(u.display_name ORDER BY u.created_at) FROM users u WHERE u.invite_id = i.id), '{}') AS "names!",
                  coalesce((SELECT array_agg(u.created_at ORDER BY u.created_at) FROM users u WHERE u.invite_id = i.id), '{}') AS "joined!: Vec<DateTime<Utc>>"
           FROM invites i
           WHERE i.created_by = $1
           ORDER BY i.created_at DESC, i.id DESC
           LIMIT 200"#,
        user_id
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| InviteRow {
            id: row.id,
            email: row.email,
            note: row.note,
            max_uses: i64::from(row.max_uses),
            uses: i64::from(row.uses),
            grant_group_ids: row.grant_group_ids,
            created_at: row.created_at,
            expires_at: row.expires_at,
            status: InviteStatus::parse(&row.status),
            used_by: row
                .usernames
                .into_iter()
                .zip(row.names)
                .zip(row.joined)
                .map(|((u, n), j)| (u, n, j))
                .collect(),
        })
        .collect())
}

/// Revokes an invite, if `user` made it or is staff. Returns whether there
/// was such an invite to revoke.
pub async fn revoke(db: &PgPool, user: &UserCtx, invite_id: i64) -> sqlx::Result<bool> {
    let revoked = sqlx::query!(
        "UPDATE invites SET revoked_at = coalesce(revoked_at, now())
         WHERE id = $1 AND (created_by = $2 OR $3)",
        invite_id,
        user.id,
        user.role.is_staff(),
    )
    .execute(db)
    .await?;
    Ok(revoked.rows_affected() > 0)
}

/// What the registration page shows about an invite.
#[derive(Debug)]
pub struct InviteInfo {
    pub id: i64,
    pub inviter_username: String,
    pub inviter_name: String,
    pub email: Option<String>,
    pub status: InviteStatus,
    pub expires_at: DateTime<Utc>,
}

pub async fn find(db: &PgPool, code: &str) -> sqlx::Result<Option<InviteInfo>> {
    if !tokens::plausible(code) {
        return Ok(None);
    }
    let row = sqlx::query!(
        r#"SELECT i.id, u.username, u.display_name, i.email AS "email: String", i.expires_at,
                  invite_status(i.revoked_at, i.uses, i.max_uses, i.expires_at) AS "status!"
           FROM invites i JOIN users u ON u.id = i.created_by
           WHERE i.code_hash = $1"#,
        tokens::hash(code)
    )
    .fetch_optional(db)
    .await?;
    Ok(row.map(|row| InviteInfo {
        id: row.id,
        inviter_username: row.username.to_string(),
        inviter_name: row.display_name,
        email: row.email,
        status: InviteStatus::parse(&row.status),
        expires_at: row.expires_at,
    }))
}

#[derive(Clone, Debug, Default)]
pub struct Registration {
    pub username: String,
    pub display_name: String,
    pub email: String,
    /// Optional: without one, the new member logs in with magic links.
    pub password: Option<String>,
}

#[derive(Debug)]
pub enum RedeemError {
    NotFound,
    Closed,
    Unusable(InviteStatus),
    /// Problems with what was typed in, to show on the form.
    Invalid(Vec<String>),
}

#[derive(Debug)]
pub struct Joined {
    pub user_id: i64,
    pub invite_id: i64,
    pub username: String,
    pub email_verified: bool,
}

/// Registers a new member with an invite.
pub async fn redeem(
    state: &AppState,
    code: &str,
    registration: &Registration,
) -> anyhow::Result<Result<Joined, RedeemError>> {
    let db = state.db();
    if !tokens::plausible(code) {
        return Ok(Err(RedeemError::NotFound));
    }
    if settings::registration_mode(db).await? == RegistrationMode::Closed {
        return Ok(Err(RedeemError::Closed));
    }

    let mut problems = Vec::new();
    let username = registration.username.trim().to_owned();
    if let Err(why) = accounts::validate_username(&username) {
        problems.push(why.to_owned());
    }
    let display_name = match accounts::clean_display_name(&registration.display_name) {
        Ok(name) => name,
        Err(why) => {
            problems.push(why.to_owned());
            String::new()
        }
    };
    let password = registration.password.as_deref().filter(|p| !p.is_empty());
    if let Some(password) = password
        && let Err(why) = accounts::validate_password(password)
    {
        problems.push(why.to_owned());
    }

    let mut tx = db.begin().await?;
    let Some(invite) = sqlx::query!(
        r#"SELECT i.id, i.created_by, i.email AS "email: String", invite_status(i.revoked_at, i.uses, i.max_uses, i.expires_at) AS "status!"
           FROM invites i WHERE i.code_hash = $1 FOR UPDATE"#,
        tokens::hash(code)
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(Err(RedeemError::NotFound));
    };
    let status = InviteStatus::parse(&invite.status);
    if status != InviteStatus::Valid {
        return Ok(Err(RedeemError::Unusable(status)));
    }
    // An emailed invite can only be used for that address, which the link
    // has just proved the new member can read.
    let (email, verified) = match &invite.email {
        Some(email) => (email.clone(), true),
        None => match accounts::clean_email(&registration.email) {
            Ok(email) => (email, false),
            Err(why) => {
                problems.push(why.to_owned());
                (String::new(), false)
            }
        },
    };
    if problems.is_empty() {
        let taken = sqlx::query!(
            r#"SELECT EXISTS (SELECT 1 FROM users WHERE username = $1::citext) AS "username!",
                      EXISTS (SELECT 1 FROM users WHERE email = $2::citext) AS "email!""#,
            username.as_str() as _,
            email.as_str() as _,
        )
        .fetch_one(&mut *tx)
        .await?;
        if taken.username {
            problems.push("That username is taken.".to_owned());
        }
        if taken.email {
            problems.push(
                "There is already an account with that email address. Log in instead, or reset your password."
                    .to_owned(),
            );
        }
    }
    if !problems.is_empty() {
        return Ok(Err(RedeemError::Invalid(problems)));
    }

    let password_hash = match password {
        Some(password) => Some(accounts::hash_password(password).await),
        None => None,
    };
    let inserted = sqlx::query_scalar!(
        r#"INSERT INTO users (username, display_name, email, email_verified_at, password_hash, role,
                              invited_by_user_id, invite_id)
           VALUES ($1, $2, $3, CASE WHEN $4 THEN now() END, $5, 'user', $6, $7)
           RETURNING id"#,
        username.as_str() as _,
        display_name,
        email.as_str() as _,
        verified,
        password_hash,
        invite.created_by,
        invite.id,
    )
    .fetch_one(&mut *tx)
    .await;
    let user_id = match inserted {
        Ok(id) => id,
        // Someone else took the name or address in the meantime.
        Err(sqlx::Error::Database(err)) if err.is_unique_violation() => {
            return Ok(Err(RedeemError::Invalid(vec![
                "That username or email address has just been taken. Try another.".to_owned(),
            ])));
        }
        Err(err) => return Err(err.into()),
    };
    sqlx::query!(
        "UPDATE invites SET uses = uses + 1 WHERE id = $1",
        invite.id
    )
    .execute(&mut *tx)
    .await?;
    // M5: add the new member to the invite's grant_group_ids here.
    if !verified {
        let issued =
            login_tokens::issue(&mut tx, user_id, Purpose::VerifyEmail, Some(&email)).await?;
        let site_name = settings::site_name(db).await?;
        mail::queue(
            &mut tx,
            &emails::verify(
                &email,
                &site_name,
                &display_name,
                &state.config().url(&format!("/verify/{}", issued.token)),
            ),
        )
        .await?;
    }
    tx.commit().await?;
    state.jobs().wake();
    Ok(Ok(Joined {
        user_id,
        invite_id: invite.id,
        username,
        email_verified: verified,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator};

    fn policy(tc: &TestCase) -> InvitePolicy {
        match tc.draw(gs::integers::<u8>().max_value(2)) {
            0 => InvitePolicy::Everyone,
            1 => InvitePolicy::Staff,
            _ => InvitePolicy::MembersOlderThan {
                days: tc.draw(gs::integers::<u32>().max_value(400)),
            },
        }
    }

    /// The rule from DESIGN.md §3.6, written out as a table.
    #[hegel::test(test_cases = 500)]
    fn invite_policy_matches_the_design(tc: TestCase) {
        let policy = policy(&tc);
        let closed = tc.draw(gs::booleans());
        let mode = if closed {
            RegistrationMode::Closed
        } else {
            RegistrationMode::InviteOnly
        };
        let role = tc.draw(
            gs::sampled_from(vec![Role::User, Role::Moderator, Role::Admin]).print_as_debug(),
        );
        let age_days = tc.draw(gs::integers::<i64>().min_value(0).max_value(1000));
        let age = Duration::days(age_days)
            + Duration::seconds(tc.draw(gs::integers::<i64>().min_value(0).max_value(86_399)));
        let verified = tc.draw(gs::booleans());
        let expected = role != Role::User
            || (!closed
                && verified
                && match policy {
                    InvitePolicy::Everyone => true,
                    InvitePolicy::Staff => false,
                    InvitePolicy::MembersOlderThan { days } => age_days >= i64::from(days),
                });
        assert_eq!(
            may_invite(policy, mode, role, age, verified).is_ok(),
            expected
        );
    }
}
