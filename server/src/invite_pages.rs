//! Invites (`/invites`, `/invite/{code}`), the public invite tree
//! (`/users/tree`) and member profiles (`/u/{username}`). See DESIGN.md
//! §3.6 and [`crate::invites`].

use askama::Template;
use axum::Form;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Redirect, Response};

use crate::AppState;
use crate::cache::Tag;
use crate::changes::Change;
use crate::client::ClientIp;
use crate::content::Deps;
use crate::error::AppError;
use crate::groups::{self, GroupSummary};
use crate::invites::{
    self, CreatedInvite, FinishError, InviteRow, InviteStatus, NewInvite, PendingRegistration,
    RedeemError, Redeemed, Registration,
};
use crate::members::{self, Profile, TreeItem};
use crate::ratelimit::{Action, Scope};
use crate::sessions;
use crate::settings::{self, RegistrationMode};
use crate::viewer::{UserCtx, Viewer};
use crate::web::{self, Layout, Nav, cacheable, render, secret_url, see_other};

// ---------------------------------------------------------------------------
// Making and managing invites

#[derive(Template)]
#[template(path = "invites.html")]
struct InvitesTemplate {
    layout: Layout,
    /// Why this member can't invite anyone at the moment, if they can't.
    refusal: Option<String>,
    staff: bool,
    closed: bool,
    expiry_days: &'static [i64],
    invites: Vec<InviteRow>,
    created: Option<CreatedInvite>,
    errors: Vec<String>,
    form: InviteForm,
    /// The groups staff can have an invite add people to.
    groups: Vec<GroupSummary>,
}

impl InvitesTemplate {
    fn group_chosen(&self, id: &i64) -> bool {
        self.form.groups.contains(id)
    }
}

#[derive(Clone, Debug)]
struct InviteForm {
    email: String,
    note: String,
    days: i64,
    uses: i64,
    groups: Vec<i64>,
}

impl Default for InviteForm {
    fn default() -> Self {
        InviteForm {
            email: String::new(),
            note: String::new(),
            days: 7,
            uses: 1,
            groups: Vec::new(),
        }
    }
}

async fn render_invites(
    state: &AppState,
    viewer: &Viewer,
    user: &UserCtx,
    status: StatusCode,
    created: Option<CreatedInvite>,
    errors: Vec<String>,
    form: InviteForm,
) -> Result<Response, AppError> {
    let db = state.db();
    let staff = user.role.is_staff();
    render(
        status,
        &InvitesTemplate {
            layout: Layout::load(state, viewer, Nav::Invites).await?,
            refusal: invites::check_may_invite(db, user).await?.err(),
            staff,
            closed: settings::registration_mode(db).await? == RegistrationMode::Closed,
            expiry_days: if staff {
                invites::STAFF_EXPIRY_DAYS
            } else {
                invites::EXPIRY_DAYS
            },
            invites: invites::list_for(db, user.id).await?,
            created,
            errors,
            form,
            groups: if staff {
                groups::list(db).await?
            } else {
                Vec::new()
            },
        },
    )
}

pub async fn invites_page(
    State(state): State<AppState>,
    viewer: Viewer,
) -> Result<Response, AppError> {
    let Some(user) = viewer.user() else {
        return Ok(web::login_first("/invites"));
    };
    render_invites(
        &state,
        &viewer,
        user,
        StatusCode::OK,
        None,
        Vec::new(),
        InviteForm::default(),
    )
    .await
}

/// Reads the invite form, which can name several groups.
fn parse_invite_form(fields: &[(String, String)]) -> (InviteForm, NewInvite, Vec<String>) {
    let mut form = InviteForm::default();
    let mut groups = Vec::new();
    let mut problems = Vec::new();
    for (key, value) in fields {
        match key.as_str() {
            "email" => form.email = value.clone(),
            "note" => form.note = value.clone(),
            "days" => form.days = value.trim().parse().unwrap_or(0),
            "uses" => form.uses = value.trim().parse().unwrap_or(0),
            "group" => match value.trim().parse::<i64>() {
                Ok(id) => groups.push(id),
                Err(_) => problems.push("Choose groups from the list.".to_owned()),
            },
            _ => {}
        }
    }
    let new = NewInvite {
        email: Some(form.email.clone()),
        note: form.note.clone(),
        expires_in_days: form.days,
        max_uses: i32::try_from(form.uses).unwrap_or(0),
        grant_group_ids: groups.clone(),
    };
    form.groups = groups;
    (form, new, problems)
}

pub async fn create_invite(
    State(state): State<AppState>,
    viewer: Viewer,
    Form(fields): Form<Vec<(String, String)>>,
) -> Result<Response, AppError> {
    let Some(user) = viewer.user() else {
        return Ok(web::login_first("/invites"));
    };
    let (form, new, problems) = parse_invite_form(&fields);
    if !problems.is_empty() {
        return render_invites(
            &state,
            &viewer,
            user,
            StatusCode::BAD_REQUEST,
            None,
            problems,
            form,
        )
        .await;
    }
    match invites::create(&state, user, &new).await? {
        Ok(created) => {
            render_invites(
                &state,
                &viewer,
                user,
                StatusCode::OK,
                Some(created),
                Vec::new(),
                InviteForm::default(),
            )
            .await
        }
        Err(problems) => {
            render_invites(
                &state,
                &viewer,
                user,
                StatusCode::BAD_REQUEST,
                None,
                problems,
                form,
            )
            .await
        }
    }
}

pub async fn revoke_invite(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let Some(user) = viewer.user() else {
        return Ok(web::login_first("/invites"));
    };
    let Some(id) = id.parse::<i64>().ok() else {
        return web::not_found(&state, &viewer).await;
    };
    if !invites::revoke(state.db(), user, id).await? {
        return web::not_found(&state, &viewer).await;
    }
    state
        .changed(Change::InviteChanged { invite_id: id })
        .await?;
    Ok(see_other("/invites"))
}

// ---------------------------------------------------------------------------
// Joining with an invite

#[derive(Template)]
#[template(path = "redeem.html")]
struct RedeemTemplate {
    layout: Layout,
    code: String,
    inviter_username: String,
    inviter_name: String,
    /// For an invite sent to an address: that address, which can't change.
    fixed_email: Option<String>,
    expires_at: chrono::DateTime<chrono::Utc>,
    errors: Vec<String>,
    username: String,
    display_name: String,
    email: String,
}

async fn unusable(
    state: &AppState,
    viewer: &Viewer,
    status: InviteStatus,
) -> Result<Response, AppError> {
    let text = match status {
        InviteStatus::Used => "This invitation has already been used.",
        InviteStatus::Expired => "This invitation has expired.",
        InviteStatus::Revoked => "This invitation has been withdrawn.",
        InviteStatus::Valid => "This invitation can’t be used.",
    };
    web::message(
        state,
        viewer,
        StatusCode::GONE,
        "Invitation not available",
        &[text, "Ask whoever invited you for a new one."],
        None,
    )
    .await
}

async fn closed(state: &AppState, viewer: &Viewer) -> Result<Response, AppError> {
    web::message(
        state,
        viewer,
        StatusCode::FORBIDDEN,
        "Registration is closed",
        &["Nobody can join at the moment, even with an invitation. Try again later."],
        None,
    )
    .await
}

pub async fn redeem_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(code): Path<String>,
    uri: Uri,
) -> Result<Response, AppError> {
    let Some(invite) = invites::find(state.db(), &code).await? else {
        return web::not_found(&state, &viewer).await;
    };
    // The code is a plausible token, so this is the URL it was made with.
    if let Some(redirect) = web::redirect_unless_canonical(&uri, &format!("/invite/{code}")) {
        return Ok(redirect);
    }
    if let Some(user) = viewer.user() {
        let text = format!("You’re already a member, logged in as {}.", user.username);
        return web::message(
            &state,
            &viewer,
            StatusCode::OK,
            "Already a member",
            &[&text],
            Some(("/", "Go to the forum index")),
        )
        .await;
    }
    if invite.status != InviteStatus::Valid {
        return unusable(&state, &viewer, invite.status).await;
    }
    if settings::registration_mode(state.db()).await? == RegistrationMode::Closed {
        return closed(&state, &viewer).await;
    }
    let emailed = invite.email.is_some();
    let template = RedeemTemplate {
        layout: Layout::load(&state, &viewer, Nav::None).await?,
        code,
        inviter_username: invite.inviter_username,
        inviter_name: invite.inviter_name,
        fixed_email: invite.email,
        expires_at: invite.expires_at,
        errors: Vec::new(),
        username: String::new(),
        display_name: String::new(),
        email: String::new(),
    };
    // The page for an invite sent to an address shows the address, so it
    // isn't kept anywhere, here or in a cache in front; it only has one
    // visitor anyway. An invite link's page may be shared widely, so it is
    // cached until something changes the invite or it expires.
    if emailed {
        return web::ok(&template);
    }
    let mut deps = Deps::new();
    deps.add(Tag::Invite(invite.id));
    deps.expires_at(invite.expires_at);
    cacheable(&template, &deps)
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
pub struct RedeemForm {
    username: String,
    display_name: String,
    email: String,
    password: String,
}

pub async fn redeem(
    State(state): State<AppState>,
    viewer: Viewer,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    Path(code): Path<String>,
    Form(form): Form<RedeemForm>,
) -> Result<Response, AppError> {
    if !state.limiter().check(Action::Redeem, Scope::Ip, &ip) {
        return web::rate_limited(&state, &viewer).await;
    }
    if viewer.user().is_some() {
        return Ok(see_other("/"));
    }
    let registration = Registration {
        username: form.username.clone(),
        display_name: form.display_name.clone(),
        email: form.email.clone(),
        password: Some(form.password.clone()),
    };
    match invites::redeem(&state, &code, &registration).await? {
        Ok(Redeemed::Joined(joined)) => welcome(&state, joined, &headers, &ip).await,
        Ok(Redeemed::EmailSent { email }) => {
            let text = format!(
                "We’ve sent an email to {email}. Follow the link in it within {} to finish joining.",
                crate::format::plural(i64::from(invites::REGISTRATION_HOURS), "hour", "hours")
            );
            web::message(
                &state,
                &viewer,
                StatusCode::OK,
                "Check your email",
                &[
                    &text,
                    "If it doesn’t arrive, check that you typed your address correctly, and look in your spam folder.",
                ],
                None,
            )
            .await
        }
        Err(RedeemError::NotFound) => web::not_found(&state, &viewer).await,
        Err(RedeemError::Closed) => closed(&state, &viewer).await,
        Err(RedeemError::Unusable(status)) => unusable(&state, &viewer, status).await,
        Err(RedeemError::Invalid(problems)) => {
            let Some(invite) = invites::find(state.db(), &code).await? else {
                return web::not_found(&state, &viewer).await;
            };
            render(
                StatusCode::BAD_REQUEST,
                &RedeemTemplate {
                    layout: Layout::load(&state, &viewer, Nav::None).await?,
                    code,
                    inviter_username: invite.inviter_username,
                    inviter_name: invite.inviter_name,
                    fixed_email: invite.email,
                    expires_at: invite.expires_at,
                    errors: problems,
                    username: form.username,
                    display_name: form.display_name,
                    email: form.email,
                },
            )
        }
    }
}

/// Logs a new member in after they join.
async fn welcome(
    state: &AppState,
    joined: invites::Joined,
    headers: &HeaderMap,
    ip: &str,
) -> Result<Response, AppError> {
    state
        .changed(Change::UserCreated {
            user_id: joined.user_id,
        })
        .await?;
    state
        .changed(Change::InviteChanged {
            invite_id: joined.invite_id,
        })
        .await?;
    let token = sessions::create(state.db(), joined.user_id, headers, ip).await?;
    Ok(web::start_session(
        see_other("/settings?done=welcome"),
        state,
        &token,
    ))
}

// ---------------------------------------------------------------------------
// Finishing joining with an invite link, from the emailed link

#[derive(Template)]
#[template(path = "join.html")]
struct JoinTemplate {
    layout: Layout,
    token: String,
    pending: PendingRegistration,
    username: String,
    errors: Vec<String>,
}

async fn join_link_expired(state: &AppState, viewer: &Viewer) -> Result<Response, AppError> {
    web::message(
        state,
        viewer,
        StatusCode::GONE,
        "Link expired",
        &[
            "This link has expired or has already been used.",
            "If you haven’t joined yet, use your invitation again.",
        ],
        None,
    )
    .await
}

pub async fn join_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(token): Path<String>,
) -> Result<Response, AppError> {
    let Some(pending) = invites::pending_registration(state.db(), &token).await? else {
        return Ok(secret_url(join_link_expired(&state, &viewer).await?));
    };
    // Opening the link doesn't join by itself (mail scanners open links).
    Ok(secret_url(web::ok(&JoinTemplate {
        layout: Layout::load(&state, &viewer, Nav::None).await?,
        token,
        username: pending.username.clone(),
        pending,
        errors: Vec::new(),
    })?))
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
pub struct JoinForm {
    username: String,
}

pub async fn finish_joining(
    State(state): State<AppState>,
    viewer: Viewer,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    Path(token): Path<String>,
    Form(form): Form<JoinForm>,
) -> Result<Response, AppError> {
    // Not rate-limited: the token can't be guessed, and the expensive
    // parts (hashing the password, sending email) happened before.
    if viewer.user().is_some() {
        return Ok(see_other("/"));
    }
    match invites::finish(&state, &token, &form.username).await? {
        Ok(joined) => welcome(&state, joined, &headers, &ip).await,
        Err(FinishError::Expired) => join_link_expired(&state, &viewer).await,
        Err(FinishError::Closed) => closed(&state, &viewer).await,
        Err(FinishError::Unusable(status)) => unusable(&state, &viewer, status).await,
        Err(FinishError::EmailTaken) => {
            web::message(
                &state,
                &viewer,
                StatusCode::CONFLICT,
                "Already a member",
                &["There is already an account with this email address. Log in instead, or reset your password."],
                Some(("/login", "Log in")),
            )
            .await
        }
        Err(FinishError::Invalid(errors)) => {
            let Some(pending) = invites::pending_registration(state.db(), &token).await? else {
                return join_link_expired(&state, &viewer).await;
            };
            Ok(secret_url(render(
                StatusCode::BAD_REQUEST,
                &JoinTemplate {
                    layout: Layout::load(&state, &viewer, Nav::None).await?,
                    token,
                    pending,
                    username: form.username,
                    errors,
                },
            )?))
        }
    }
}

// ---------------------------------------------------------------------------
// The invite tree and profiles

#[derive(Template)]
#[template(path = "tree.html")]
struct TreeTemplate {
    layout: Layout,
    items: Vec<TreeItem>,
    count: usize,
    viewer_id: Option<i64>,
    may_invite: bool,
}

pub async fn tree(
    State(state): State<AppState>,
    viewer: Viewer,
    uri: Uri,
) -> Result<Response, AppError> {
    if uri.query().is_some() {
        return Ok(Redirect::permanent("/users/tree").into_response());
    }
    let mut deps = Deps::new();
    let (items, count) = members::tree(state.db(), &mut deps).await?;
    let may_invite = match viewer.user() {
        Some(user) => invites::check_may_invite(state.db(), user).await?.is_ok(),
        None => false,
    };
    cacheable(
        &TreeTemplate {
            layout: Layout::load(&state, &viewer, Nav::Members).await?,
            items,
            count,
            viewer_id: viewer.user_id(),
            may_invite,
        },
        &deps,
    )
}

#[derive(Template)]
#[template(path = "profile.html")]
struct ProfileTemplate {
    layout: Layout,
    profile: Profile,
    own: bool,
    /// What a logged-in member looking at someone else's profile can do.
    actions: Option<ProfileActions>,
}

struct ProfileActions {
    can_message: bool,
    blocked: bool,
    can_block: bool,
    /// Whether the viewer is staff, who can open the member's record.
    staff: bool,
}

pub async fn profile(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(username): Path<String>,
    uri: Uri,
) -> Result<Response, AppError> {
    if !username_shaped(&username) {
        return web::not_found(&state, &viewer).await;
    }
    let mut deps = Deps::new();
    let Some(profile) = members::profile(state.db(), &viewer, &username, &mut deps).await? else {
        return web::not_found(&state, &viewer).await;
    };
    let canonical = format!("/u/{}", profile.user.username);
    if uri.path_and_query().map_or(uri.path(), |pq| pq.as_str()) != canonical {
        return Ok(Redirect::permanent(&canonical).into_response());
    }
    let actions = match viewer.user() {
        Some(user) if user.id != profile.user.id => {
            match crate::social::members_named(
                state.db(),
                std::slice::from_ref(&profile.user.username),
            )
            .await?
            .into_iter()
            .next()
            {
                Some(member) => Some(ProfileActions {
                    can_message: crate::messages::may_message(&state, &viewer, &member).await?,
                    blocked: crate::social::has_blocked(state.db(), user.id, member.user.id)
                        .await?,
                    can_block: !member.role.is_staff(),
                    staff: viewer.is_staff(),
                }),
                None => None,
            }
        }
        _ => None,
    };
    cacheable(
        &ProfileTemplate {
            layout: Layout::load(&state, &viewer, Nav::Members).await?,
            own: viewer.user_id() == Some(profile.user.id),
            profile,
            actions,
        },
        &deps,
    )
}

/// Whether text has the form of a username, reserved or not (accounts
/// made before a name was reserved still have profiles).
fn username_shaped(text: &str) -> bool {
    (2..=24).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}
