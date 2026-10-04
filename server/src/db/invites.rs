//! Invites (DESIGN.md §3.6). An invite is a secret code in a link
//! (`/invite/{code}`); only a hash of the code is stored, so the link is
//! shown once, to whoever made it, or emailed to the person invited.
//!
//! - Members make single-use invites, optionally for one email address.
//!   Who may invite is a site setting ([`InvitePolicy`]).
//! - An invite for an email address is sent only to that address, never
//!   shown to whoever made it, so using it proves the new member can read
//!   that address. If the address belongs to a member already, they are
//!   sent a note saying so instead, and the inviter is told the same as
//!   for anyone else, so invites don't reveal who is a member.
//! - An invite link can be passed around, so registering with one only
//!   sends an email to the address typed in; following the link in it
//!   ([`finish`]) makes the account. A member's address gets a note
//!   instead, and the page says the same either way.
//! - Staff can also make links with several uses that put everyone who
//!   joins with them into groups.
//! - Registering records who invited the new member and with which invite.
//!   The database refuses to change either afterwards, so the invite tree is
//!   a permanent public record.

use chrono::{DateTime, Duration, Utc};
use sqlx::PgConnection;

use crate::accounts::{self, Role};
use crate::db::Db;
use crate::groups;
use crate::settings::{InvitePolicy, RegistrationMode};
use crate::viewer::UserCtx;
use crate::{AppState, emails, format, mail, ratelimit, settings, tokens};

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
pub async fn check_may_invite(db: &Db, user: &UserCtx) -> sqlx::Result<Result<(), String>> {
    if !user.may_write() {
        return Ok(Err(
            "Your account is silenced or suspended for now, so you can’t invite anyone.".to_owned(),
        ));
    }
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
    /// The secret code. For an invite sent to an address, this goes only to
    /// that address and must not be shown to the inviter.
    pub code: String,
    /// The link to show the inviter: only for invites not sent by email.
    pub link: Option<String>,
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
    if email.is_some() && !mail::can_send(state).await? {
        problems.push(format!(
            "There is no mail server set up, so the invitation can’t be emailed. {}",
            if creator.role == Role::Admin {
                "Set one up in Admin → Settings, or leave the address blank to get a link to send yourself."
            } else {
                "Leave the address blank to get a link to send yourself."
            }
        ));
    }
    if email.is_some() && new.max_uses != 1 {
        problems.push("An invite sent to an email address can only be used once.".to_owned());
    }
    let mut grant_group_ids = new.grant_group_ids.clone();
    grant_group_ids.sort();
    grant_group_ids.dedup();
    if !staff && !grant_group_ids.is_empty() {
        problems.push("Only staff can make invites that add people to groups.".to_owned());
    } else if groups::existing(db, &grant_group_ids).await? != grant_group_ids {
        problems.push("Choose groups from the list.".to_owned());
    }
    let note = new.note.trim();
    if note.chars().count() > 200 {
        problems.push("Keep the note to 200 characters.".to_owned());
    }
    if !staff {
        let pending = sqlx::query_scalar!(
            r#"SELECT count(*) AS "n!" FROM invites
               WHERE created_by = $1 AND revoked_at IS NULL AND uses < max_uses AND expires_at > now()"#,
            creator.id
        )
        .fetch_one(db.pool())
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
    let mut tx = db.pool().begin().await?;
    let invite = sqlx::query!(
        r#"INSERT INTO invites (code_hash, created_by, email, max_uses, expires_at, grant_group_ids, note)
           VALUES ($1, $2, $3, $4, now() + make_interval(days => $5), $6, $7)
           RETURNING id, expires_at"#,
        tokens::hash(&code),
        creator.id,
        email.as_deref() as _,
        new.max_uses,
        new.expires_in_days as i32,
        &grant_group_ids,
        note,
    )
    .fetch_one(&mut *tx)
    .await?;
    if let Some(email) = &email {
        let site_name = settings::site_name(db).await?;
        // A member's address gets a note instead of the invite, which
        // nobody will see, so it just expires.
        let message = match member_with_email(&mut tx, email).await? {
            Some(member) => emails::already_member(
                email,
                &site_name,
                &member.display_name,
                &member.username,
                emails::AlreadyMember::InvitedBy(&creator.display_name),
                &state.config().url("/login"),
                &state.config().url("/reset"),
            ),
            None => emails::invite(
                email,
                &site_name,
                &creator.display_name,
                &url,
                &format::datetime(&invite.expires_at),
            ),
        };
        mail::queue(&mut tx, &message).await?;
    }
    tx.commit().await?;
    state.jobs().wake();
    Ok(Ok(CreatedInvite {
        id: invite.id,
        code,
        link: email.is_none().then_some(url),
        emailed_to: email,
    }))
}

struct Member {
    username: String,
    display_name: String,
}

async fn member_with_email(db: &mut PgConnection, email: &str) -> sqlx::Result<Option<Member>> {
    let row = sqlx::query!(
        r#"SELECT username AS "username: String", display_name FROM users WHERE email = $1::citext"#,
        email as _
    )
    .fetch_optional(db)
    .await?;
    Ok(row.map(|row| Member {
        username: row.username,
        display_name: row.display_name,
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
    /// The names of the groups it adds people to.
    pub grant_groups: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub status: InviteStatus,
    /// Who joined with it: (username, display name, joined).
    pub used_by: Vec<(String, String, DateTime<Utc>)>,
}

pub async fn list_for(db: &Db, user_id: i64) -> sqlx::Result<Vec<InviteRow>> {
    let rows = sqlx::query!(
        r#"SELECT i.id, i.email AS "email: String", i.note, i.max_uses, i.uses,
                  array(SELECT g.name FROM groups g WHERE g.id = ANY(i.grant_group_ids)
                        ORDER BY lower(g.name), g.id) AS "grant_groups!",
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
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| InviteRow {
            id: row.id,
            email: row.email,
            note: row.note,
            max_uses: i64::from(row.max_uses),
            uses: i64::from(row.uses),
            grant_groups: row.grant_groups,
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
pub async fn revoke(db: &Db, user: &UserCtx, invite_id: i64) -> sqlx::Result<bool> {
    let revoked = sqlx::query!(
        "UPDATE invites SET revoked_at = coalesce(revoked_at, now())
         WHERE id = $1 AND (created_by = $2 OR $3)",
        invite_id,
        user.id,
        user.role.is_staff(),
    )
    .execute(db.pool())
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

pub async fn find(db: &Db, code: &str) -> sqlx::Result<Option<InviteInfo>> {
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
    .fetch_optional(db.pool())
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
}

/// What happened when someone registered with an invite.
#[derive(Debug)]
pub enum Redeemed {
    /// An invite sent to an address: the account has been made, with that
    /// address confirmed.
    Joined(Joined),
    /// An invite link: an email has gone to the address typed in, with a
    /// link to finish joining or, if the address belongs to a member, a
    /// note saying so. Either way the page says the same.
    EmailSent { email: String },
}

/// How long the link to finish joining works.
pub const REGISTRATION_HOURS: i32 = 24;

/// Checks the parts of a registration that every invite needs.
fn check_registration(
    registration: &Registration,
    problems: &mut Vec<String>,
) -> (String, String, Option<String>) {
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
    let password = registration.password.clone().filter(|p| !p.is_empty());
    if let Some(password) = &password
        && let Err(why) = accounts::validate_password(password)
    {
        problems.push(why.to_owned());
    }
    (username, display_name, password)
}

/// Whether a username is someone's, now or before they deleted their
/// account, or a space's address: usernames and space addresses share one
/// namespace, and the new member's feed goes at theirs (DESIGN.md §3.1,
/// §3.2).
async fn username_taken(db: &mut PgConnection, username: &str) -> sqlx::Result<bool> {
    sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM users WHERE username = $1::citext)
                  OR EXISTS (SELECT 1 FROM retired_usernames
                             WHERE username_hash = username_hash($1))
                  OR EXISTS (SELECT 1 FROM spaces WHERE slug = lower($1)) AS "e!""#,
        username as _,
    )
    .fetch_one(db)
    .await
}

async fn email_taken(db: &mut PgConnection, email: &str) -> sqlx::Result<bool> {
    sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM users WHERE email = $1::citext) AS "e!""#,
        email as _,
    )
    .fetch_one(db)
    .await
}

const TAKEN_USERNAME: &str = "That username is taken.";

/// Registers a new member with an invite: straight away for an invite sent
/// to an address, or by email for an invite link (see [`Redeemed`]).
pub async fn redeem(
    state: &AppState,
    code: &str,
    registration: &Registration,
) -> anyhow::Result<Result<Redeemed, RedeemError>> {
    let db = state.db();
    if !tokens::plausible(code) {
        return Ok(Err(RedeemError::NotFound));
    }
    if settings::registration_mode(db).await? == RegistrationMode::Closed {
        return Ok(Err(RedeemError::Closed));
    }

    let mut problems = Vec::new();
    let (username, display_name, password) = check_registration(registration, &mut problems);

    let mut tx = db.pool().begin().await?;
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
    let email = match &invite.email {
        Some(email) => email.clone(),
        None => accounts::clean_email(&registration.email).unwrap_or_else(|why| {
            problems.push(why.to_owned());
            String::new()
        }),
    };
    // Joining with a link is finished from an email to the address typed
    // in, so it can't be done until the forum can send one.
    if invite.email.is_none() && !mail::can_send(state).await? {
        problems.push(
            "This forum can’t send email yet, so it can’t confirm your address. Let whoever invited you know."
                .to_owned(),
        );
    }
    // Usernames are public, so saying one is taken gives nothing away.
    if problems.is_empty() && username_taken(&mut tx, &username).await? {
        problems.push(TAKEN_USERNAME.to_owned());
    }
    // Only the address's owner can have an emailed invite's link, so they
    // can be told their address has an account.
    if problems.is_empty() && invite.email.is_some() && email_taken(&mut tx, &email).await? {
        problems.push(
            "There is already an account with that email address. Log in instead, or reset your password."
                .to_owned(),
        );
    }
    if !problems.is_empty() {
        return Ok(Err(RedeemError::Invalid(problems)));
    }

    // Hashed in every case, so that how long this takes doesn't say
    // whether the address belongs to a member.
    let password_hash = match &password {
        Some(password) => Some(accounts::hash_password(password).await),
        None => None,
    };

    if invite.email.is_none() {
        tx.rollback().await?;
        send_registration_email(
            state,
            invite.id,
            &username,
            &display_name,
            &email,
            password_hash,
        )
        .await?;
        return Ok(Ok(Redeemed::EmailSent { email }));
    }

    let inserted = insert_member(
        &mut tx,
        &username,
        &display_name,
        &email,
        password_hash,
        invite.created_by,
        invite.id,
    )
    .await?;
    let Some(user_id) = inserted else {
        return Ok(Err(RedeemError::Invalid(vec![
            "That username or email address has just been taken. Try another.".to_owned(),
        ])));
    };
    tx.commit().await?;
    Ok(Ok(Redeemed::Joined(Joined {
        user_id,
        invite_id: invite.id,
        username,
    })))
}

/// Makes the account and uses up one of the invite's uses. `None` if the
/// username or address was taken in the meantime.
async fn insert_member(
    tx: &mut PgConnection,
    username: &str,
    display_name: &str,
    email: &str,
    password_hash: Option<String>,
    invited_by: i64,
    invite_id: i64,
) -> sqlx::Result<Option<i64>> {
    let inserted = sqlx::query_scalar!(
        r#"INSERT INTO users (username, display_name, email, email_verified_at, password_hash, role,
                              invited_by_user_id, invite_id)
           VALUES ($1, $2, $3, now(), $4, 'user', $5, $6)
           RETURNING id"#,
        username as _,
        display_name,
        email as _,
        password_hash,
        invited_by,
        invite_id,
    )
    .fetch_one(&mut *tx)
    .await;
    let user_id = match inserted {
        Ok(id) => id,
        Err(sqlx::Error::Database(err)) if err.is_unique_violation() => return Ok(None),
        Err(err) => return Err(err),
    };
    let grant = sqlx::query_scalar!(
        "UPDATE invites SET uses = uses + 1 WHERE id = $1 RETURNING grant_group_ids",
        invite_id
    )
    .fetch_one(&mut *tx)
    .await?;
    // The invite's groups that still exist; the inviter added them.
    groups::grant(&mut *tx, user_id, &grant, Some(invited_by)).await?;
    crate::layouts::create_member_copy(&mut *tx, user_id).await?;
    Ok(Some(user_id))
}

/// Sends the link to finish joining with an invite link, or a note to a
/// member whose address it is. An address that has had a few of these
/// lately is quietly sent nothing, so a public link can't be used to flood
/// someone's inbox.
async fn send_registration_email(
    state: &AppState,
    invite_id: i64,
    username: &str,
    display_name: &str,
    email: &str,
    password_hash: Option<String>,
) -> anyhow::Result<()> {
    if !state.limiter().check(
        ratelimit::Action::JoinEmail,
        ratelimit::Scope::Account,
        email,
    ) {
        return Ok(());
    }
    let mut tx = state.db().pool().begin().await?;
    let site_name = settings::site_name(state.db()).await?;
    let message = match member_with_email(&mut tx, email).await? {
        Some(member) => emails::already_member(
            email,
            &site_name,
            &member.display_name,
            &member.username,
            emails::AlreadyMember::Joining,
            &state.config().url("/login"),
            &state.config().url("/reset"),
        ),
        None => {
            sqlx::query!("DELETE FROM registrations WHERE expires_at < now()")
                .execute(&mut *tx)
                .await?;
            let token = tokens::new_token();
            sqlx::query!(
                "INSERT INTO registrations (token_hash, invite_id, username, display_name, email, password_hash, expires_at)
                 VALUES ($1, $2, $3, $4, $5, $6, now() + make_interval(hours => $7))",
                tokens::hash(&token),
                invite_id,
                username as _,
                display_name,
                email as _,
                password_hash,
                REGISTRATION_HOURS,
            )
            .execute(&mut *tx)
            .await?;
            emails::join(
                email,
                &site_name,
                display_name,
                &state.config().url(&format!("/join/{token}")),
            )
        }
    };
    mail::queue(&mut tx, &message).await?;
    tx.commit().await?;
    state.jobs().wake();
    Ok(())
}

/// A registration waiting for its email link to be followed.
#[derive(Debug)]
pub struct PendingRegistration {
    pub username: String,
    pub email: String,
    pub inviter_username: String,
    pub inviter_name: String,
}

pub async fn pending_registration(
    db: &Db,
    token: &str,
) -> sqlx::Result<Option<PendingRegistration>> {
    if !tokens::plausible(token) {
        return Ok(None);
    }
    let row = sqlx::query!(
        r#"SELECT r.username AS "username: String", r.email AS "email: String",
                  u.username AS "inviter_username: String", u.display_name AS inviter_name
           FROM registrations r JOIN invites i ON i.id = r.invite_id JOIN users u ON u.id = i.created_by
           WHERE r.token_hash = $1 AND r.expires_at > now()"#,
        tokens::hash(token)
    )
    .fetch_optional(db.pool())
    .await?;
    Ok(row.map(|row| PendingRegistration {
        username: row.username,
        email: row.email,
        inviter_username: row.inviter_username,
        inviter_name: row.inviter_name,
    }))
}

#[derive(Debug)]
pub enum FinishError {
    /// No such registration, or its link has expired or been used.
    Expired,
    Closed,
    Unusable(InviteStatus),
    /// Problems with the username, to show on the form.
    Invalid(Vec<String>),
    /// Someone has joined with the address since the link was sent.
    EmailTaken,
}

/// Finishes joining with an invite link, from the link emailed by
/// [`redeem`]. `username` is the one chosen then, or another if that has
/// been taken since.
pub async fn finish(
    state: &AppState,
    token: &str,
    username: &str,
) -> anyhow::Result<Result<Joined, FinishError>> {
    let db = state.db();
    if !tokens::plausible(token) {
        return Ok(Err(FinishError::Expired));
    }
    let mut tx = db.pool().begin().await?;
    let Some(registration) = sqlx::query!(
        r#"SELECT invite_id, display_name, email AS "email: String", password_hash
           FROM registrations WHERE token_hash = $1 AND expires_at > now()
           FOR UPDATE"#,
        tokens::hash(token)
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(Err(FinishError::Expired));
    };
    if settings::registration_mode(db).await? == RegistrationMode::Closed {
        return Ok(Err(FinishError::Closed));
    }
    let invite = sqlx::query!(
        r#"SELECT created_by, invite_status(revoked_at, uses, max_uses, expires_at) AS "status!"
           FROM invites WHERE id = $1 FOR UPDATE"#,
        registration.invite_id
    )
    .fetch_one(&mut *tx)
    .await?;
    let status = InviteStatus::parse(&invite.status);
    if status != InviteStatus::Valid {
        return Ok(Err(FinishError::Unusable(status)));
    }
    let username = username.trim();
    if let Err(why) = accounts::validate_username(username) {
        return Ok(Err(FinishError::Invalid(vec![why.to_owned()])));
    }
    if username_taken(&mut tx, username).await? {
        return Ok(Err(FinishError::Invalid(vec![TAKEN_USERNAME.to_owned()])));
    }
    if email_taken(&mut tx, &registration.email).await? {
        return Ok(Err(FinishError::EmailTaken));
    }
    let inserted = insert_member(
        &mut tx,
        username,
        &registration.display_name,
        &registration.email,
        registration.password_hash,
        invite.created_by,
        registration.invite_id,
    )
    .await?;
    let Some(user_id) = inserted else {
        return Ok(Err(FinishError::Invalid(vec![
            "That username has just been taken. Try another.".to_owned(),
        ])));
    };
    sqlx::query!(
        "DELETE FROM registrations WHERE token_hash = $1",
        tokens::hash(token)
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Ok(Joined {
        user_id,
        invite_id: registration.invite_id,
        username: username.to_owned(),
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
