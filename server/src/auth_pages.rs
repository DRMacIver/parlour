//! Logging in and out, magic links, password resets, email verification
//! and account settings (DESIGN.md §7).
//!
//! The login and reset-request pages carry nothing per-visitor (there are
//! no CSRF tokens; see [`crate::csrf`]), so anonymous visitors get them from
//! the page cache. Pages whose URL holds a token are never cached and tell
//! the browser not to send the URL on as a referrer.
//!
//! Requests that reveal whether an account exists (magic links, resets)
//! give the same answer either way.

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use serde::Deserialize;

use crate::AppState;
use crate::accounts;
use crate::client::ClientIp;
use crate::content::Deps;
use crate::emails;
use crate::error::AppError;
use crate::login_tokens::{self, CodeResult, Purpose};
use crate::mail;
use crate::ratelimit::{Action, Scope};
use crate::sessions;
use crate::settings;
use crate::tokens;
use crate::users::{self, LoginAccount};
use crate::viewer::{UserCtx, Viewer, session_cookie};
use crate::web::{
    self, Layout, Nav, cacheable, redirect_unless_canonical, render, secret_url, see_other,
};

/// Where to go after logging in.
fn destination(next: &str) -> &str {
    web::safe_next(next).unwrap_or("/")
}

async fn find_account(state: &AppState, identifier: &str) -> sqlx::Result<Option<LoginAccount>> {
    users::find_login_account(state.db(), identifier).await
}

/// Uses up one of an account's allowance for `action` (the account
/// [`Scope::User`] limits), returning whether there was any.
fn user_allows(state: &AppState, action: Action, user_id: i64) -> bool {
    state
        .limiter()
        .check(action, Scope::User, &user_id.to_string())
}

/// Starts a session and sends the browser on.
async fn log_in(
    state: &AppState,
    user_id: i64,
    headers: &HeaderMap,
    ip: &str,
    next: &str,
) -> Result<Response, AppError> {
    let token = sessions::create(state.db(), user_id, headers, ip).await?;
    Ok(web::start_session(
        see_other(destination(next)),
        state,
        &token,
    ))
}

/// A magic link or reset link proves the member can read their email.
async fn mark_verified(state: &AppState, user_id: i64) -> sqlx::Result<()> {
    users::mark_verified(state.db(), user_id).await
}

async fn expired_link(
    state: &AppState,
    viewer: &Viewer,
    what: &str,
    again: (&str, &str),
) -> Result<Response, AppError> {
    let text = format!("This {what} has expired or has already been used.");
    web::message(
        state,
        viewer,
        StatusCode::GONE,
        "Link expired",
        &[&text],
        Some(again),
    )
    .await
}

// ---------------------------------------------------------------------------
// Logging in

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    layout: Layout,
    next: String,
    identifier: String,
    email: String,
    error: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct NextQuery {
    next: String,
}

/// The page to come back to that a login page's URL names, if any: the
/// first `next` parameter, if it is a path on this site other than `/`.
fn next_in(uri: &Uri) -> Option<String> {
    let Query(params) = Query::<Vec<(String, String)>>::try_from_uri(uri).ok()?;
    let (_, next) = params.into_iter().find(|(key, _)| key == "next")?;
    web::safe_next(&next)
        .filter(|next| *next != "/")
        .map(str::to_owned)
}

pub async fn login_page(
    State(state): State<AppState>,
    viewer: Viewer,
    uri: Uri,
) -> Result<Response, AppError> {
    // One URL per page that can be shown, as for the reading pages: other
    // query strings are redirected before rendering, so they can't fill
    // the anonymous cache.
    let next = next_in(&uri);
    let canonical = match &next {
        None => "/login".to_owned(),
        Some(next) => format!("/login?next={}", web::query_encode(next)),
    };
    if let Some(redirect) = redirect_unless_canonical(&uri, &canonical) {
        return Ok(redirect);
    }
    if let Some(user) = viewer.user() {
        let text = format!("You’re logged in as {}.", user.username);
        return web::message(
            &state,
            &viewer,
            StatusCode::OK,
            "Logged in",
            &[&text],
            Some(("/", "Go to the front page")),
        )
        .await;
    }
    let template = LoginTemplate {
        layout: Layout::load(&state, &viewer, Nav::Login).await?,
        next: next.clone().unwrap_or_default(),
        identifier: String::new(),
        email: String::new(),
        error: None,
    };
    match next {
        // Only the plain login page is cached: any page on the site can be
        // somewhere to come back to, so there is no limit to the others.
        Some(_) => web::ok(&template),
        None => cacheable(&template, &Deps::new()),
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct LoginForm {
    identifier: String,
    password: String,
    next: String,
}

pub async fn login(
    State(state): State<AppState>,
    viewer: Viewer,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Result<Response, AppError> {
    let account_key = form.identifier.trim();
    if !state.limiter().check_all(&[
        (Action::Login, Scope::Ip, &ip),
        (Action::Login, Scope::Account, account_key),
    ]) {
        return web::rate_limited(&state, &viewer).await;
    }
    let account = find_account(&state, &form.identifier).await?;
    // The account's own limit, however it was named. When it refuses, the
    // password is checked against nothing (taking as long as a real check),
    // so the refusal looks like a wrong password.
    let (user_id, hash) = match account {
        Some(account) if user_allows(&state, Action::Login, account.id) => {
            (Some(account.id), account.password_hash)
        }
        _ => (None, None),
    };
    if accounts::verify_password(hash, &form.password).await
        && let Some(user_id) = user_id
    {
        return log_in(&state, user_id, &headers, &ip, &form.next).await;
    }
    render(
        StatusCode::UNAUTHORIZED,
        &LoginTemplate {
            layout: Layout::load(&state, &viewer, Nav::Login).await?,
            next: web::safe_next(&form.next).unwrap_or_default().to_owned(),
            identifier: form.identifier,
            email: String::new(),
            error: Some(
                "That username or email and password don’t match an account. \
                 If you joined without a password, use a login link instead."
                    .into(),
            ),
        },
    )
}

// ---------------------------------------------------------------------------
// Magic links

#[derive(Template)]
#[template(path = "login_code.html")]
struct LoginCodeTemplate {
    layout: Layout,
    request: String,
    next: String,
    error: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct MagicLinkForm {
    email: String,
    next: String,
}

pub async fn request_magic_link(
    State(state): State<AppState>,
    viewer: Viewer,
    ClientIp(ip): ClientIp,
    Form(form): Form<MagicLinkForm>,
) -> Result<Response, AppError> {
    let account_key = form.email.trim();
    if !state.limiter().check_all(&[
        (Action::MagicLink, Scope::Ip, &ip),
        (Action::MagicLink, Scope::Account, account_key),
    ]) {
        return web::rate_limited(&state, &viewer).await;
    }
    let next = web::safe_next(&form.next).unwrap_or_default().to_owned();
    // Without an account there is nothing to send, but the page looks the
    // same, with a request id that matches nothing.
    // An account that has had enough links lately is quietly sent nothing.
    let request = match find_account(&state, &form.email).await? {
        Some(account) if user_allows(&state, Action::MagicLink, account.id) => {
            let mut tx = state.db().begin().await?;
            let issued = login_tokens::issue(&mut tx, account.id, Purpose::MagicLink, None).await?;
            let mut url = state.config().url(&format!("/login/{}", issued.token));
            if !next.is_empty() && next != "/" {
                url.push_str(&format!("?next={}", web::query_encode(&next)));
            }
            let site_name = settings::site_name(state.db()).await?;
            mail::queue(
                &mut tx,
                &emails::magic_link(
                    &account.email,
                    &site_name,
                    &account.display_name,
                    &url,
                    issued.code.as_deref().unwrap_or_default(),
                ),
            )
            .await?;
            tx.commit().await?;
            state.jobs().wake();
            issued.request.unwrap_or_default()
        }
        _ => tokens::new_token(),
    };
    web::ok(&LoginCodeTemplate {
        layout: Layout::load(&state, &viewer, Nav::Login).await?,
        request,
        next,
        error: None,
    })
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct CodeForm {
    request: String,
    code: String,
    next: String,
}

pub async fn enter_code(
    State(state): State<AppState>,
    viewer: Viewer,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    Form(form): Form<CodeForm>,
) -> Result<Response, AppError> {
    if !state.limiter().check(Action::MagicCode, Scope::Ip, &ip) {
        return web::rate_limited(&state, &viewer).await;
    }
    let allow = |user_id| user_allows(&state, Action::MagicCode, user_id);
    match login_tokens::try_code(state.db(), &form.request, &form.code, allow).await? {
        CodeResult::LoggedIn { user_id } => {
            mark_verified(&state, user_id).await?;
            log_in(&state, user_id, &headers, &ip, &form.next).await
        }
        CodeResult::Wrong { attempts_left } => render(
            StatusCode::UNAUTHORIZED,
            &LoginCodeTemplate {
                layout: Layout::load(&state, &viewer, Nav::Login).await?,
                request: form.request,
                next: web::safe_next(&form.next).unwrap_or_default().to_owned(),
                error: Some(format!(
                    "That code isn’t right. You have {} left.",
                    crate::format::plural(i64::from(attempts_left), "try", "tries")
                )),
            },
        ),
        CodeResult::Expired => {
            expired_link(
                &state,
                &viewer,
                "code",
                ("/login", "Ask for a new login link"),
            )
            .await
        }
        CodeResult::TooManyGuesses => {
            web::message(
                &state,
                &viewer,
                StatusCode::TOO_MANY_REQUESTS,
                "Too many wrong codes",
                &["There have been too many wrong codes for this account recently, so codes won’t work for it for a while. Follow the link in the email instead: that still works."],
                None,
            )
            .await
        }
    }
}

#[derive(Template)]
#[template(path = "login_link.html")]
struct LoginLinkTemplate {
    layout: Layout,
    token: String,
    next: String,
}

pub async fn magic_link_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(token): Path<String>,
    Query(query): Query<NextQuery>,
) -> Result<Response, AppError> {
    if !login_tokens::is_current(state.db(), &token, Purpose::MagicLink).await? {
        return Ok(secret_url(
            expired_link(
                &state,
                &viewer,
                "login link",
                ("/login", "Ask for a new one"),
            )
            .await?,
        ));
    }
    Ok(secret_url(web::ok(&LoginLinkTemplate {
        layout: Layout::load(&state, &viewer, Nav::Login).await?,
        token,
        next: web::safe_next(&query.next).unwrap_or_default().to_owned(),
    })?))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct NextForm {
    next: String,
}

pub async fn use_magic_link(
    State(state): State<AppState>,
    viewer: Viewer,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    Path(token): Path<String>,
    Form(form): Form<NextForm>,
) -> Result<Response, AppError> {
    match login_tokens::consume(state.db(), &token, Purpose::MagicLink).await? {
        Some(used) => {
            mark_verified(&state, used.user_id).await?;
            log_in(&state, used.user_id, &headers, &ip, &form.next).await
        }
        None => {
            expired_link(
                &state,
                &viewer,
                "login link",
                ("/login", "Ask for a new one"),
            )
            .await
        }
    }
}

// ---------------------------------------------------------------------------
// Logging out

#[derive(Template)]
#[template(path = "logout.html")]
struct LogoutTemplate {
    layout: Layout,
}

pub async fn logout_page(
    State(state): State<AppState>,
    viewer: Viewer,
) -> Result<Response, AppError> {
    if viewer.user().is_none() {
        return Ok(see_other("/"));
    }
    web::ok(&LogoutTemplate {
        layout: Layout::load(&state, &viewer, Nav::None).await?,
    })
}

pub async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    // Without a session cookie there is nothing to clear, and anonymous
    // visitors are never sent cookies.
    let Some(token) = session_cookie(&headers) else {
        return Ok(see_other("/"));
    };
    sessions::delete(state.db(), token).await?;
    Ok(web::end_session(see_other("/"), &state))
}

// ---------------------------------------------------------------------------
// Password resets

#[derive(Template)]
#[template(path = "reset_request.html")]
struct ResetRequestTemplate {
    layout: Layout,
}

pub async fn reset_request_page(
    State(state): State<AppState>,
    viewer: Viewer,
    uri: Uri,
) -> Result<Response, AppError> {
    if let Some(redirect) = redirect_unless_canonical(&uri, "/reset") {
        return Ok(redirect);
    }
    cacheable(
        &ResetRequestTemplate {
            layout: Layout::load(&state, &viewer, Nav::Login).await?,
        },
        &Deps::new(),
    )
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct EmailForm {
    email: String,
}

pub async fn request_reset(
    State(state): State<AppState>,
    viewer: Viewer,
    ClientIp(ip): ClientIp,
    Form(form): Form<EmailForm>,
) -> Result<Response, AppError> {
    let account_key = form.email.trim();
    if !state.limiter().check_all(&[
        (Action::PasswordReset, Scope::Ip, &ip),
        (Action::PasswordReset, Scope::Account, account_key),
    ]) {
        return web::rate_limited(&state, &viewer).await;
    }
    // An account that has had enough reset emails lately is quietly sent
    // nothing.
    if let Some(account) = find_account(&state, &form.email).await?
        && user_allows(&state, Action::PasswordReset, account.id)
    {
        let mut tx = state.db().begin().await?;
        let issued = login_tokens::issue(&mut tx, account.id, Purpose::Reset, None).await?;
        let site_name = settings::site_name(state.db()).await?;
        mail::queue(
            &mut tx,
            &emails::reset(
                &account.email,
                &site_name,
                &account.display_name,
                &state.config().url(&format!("/reset/{}", issued.token)),
            ),
        )
        .await?;
        tx.commit().await?;
        state.jobs().wake();
    }
    web::message(
        &state,
        &viewer,
        StatusCode::OK,
        "Check your email",
        &["If there is an account with that username or email address, we’ve sent it a link to choose a new password. The link works for an hour."],
        Some(("/login", "Back to logging in")),
    )
    .await
}

#[derive(Template)]
#[template(path = "reset_form.html")]
struct ResetFormTemplate {
    layout: Layout,
    token: String,
    errors: Vec<String>,
}

pub async fn reset_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(token): Path<String>,
) -> Result<Response, AppError> {
    if !login_tokens::is_current(state.db(), &token, Purpose::Reset).await? {
        return Ok(secret_url(
            expired_link(
                &state,
                &viewer,
                "reset link",
                ("/reset", "Ask for a new one"),
            )
            .await?,
        ));
    }
    Ok(secret_url(web::ok(&ResetFormTemplate {
        layout: Layout::load(&state, &viewer, Nav::Login).await?,
        token,
        errors: Vec::new(),
    })?))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct NewPasswordForm {
    password: String,
    confirm: String,
}

/// The problems with a new password and its confirmation.
fn new_password_problems(password: &str, confirm: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if let Err(why) = accounts::validate_password(password) {
        problems.push(why.to_owned());
    } else if password != confirm {
        problems.push("The two passwords don’t match.".to_owned());
    }
    problems
}

pub async fn reset_password(
    State(state): State<AppState>,
    viewer: Viewer,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    Path(token): Path<String>,
    Form(form): Form<NewPasswordForm>,
) -> Result<Response, AppError> {
    let problems = new_password_problems(&form.password, &form.confirm);
    if !problems.is_empty() {
        return Ok(secret_url(render(
            StatusCode::BAD_REQUEST,
            &ResetFormTemplate {
                layout: Layout::load(&state, &viewer, Nav::Login).await?,
                token,
                errors: problems,
            },
        )?));
    }
    // Hashing is deliberately slow, so anyone can't make the server do it
    // by posting made-up tokens: the token is checked first (and once
    // more, using it up, below).
    if !login_tokens::is_current(state.db(), &token, Purpose::Reset).await? {
        return expired_link(
            &state,
            &viewer,
            "reset link",
            ("/reset", "Ask for a new one"),
        )
        .await;
    }
    let hash = accounts::hash_password(&form.password).await;
    let mut tx = state.db().begin().await?;
    let Some(used) = login_tokens::consume(&mut tx, &token, Purpose::Reset).await? else {
        return expired_link(
            &state,
            &viewer,
            "reset link",
            ("/reset", "Ask for a new one"),
        )
        .await;
    };
    users::set_password(&mut tx, used.user_id, &hash, true).await?;
    // Whoever knew the old password is logged out everywhere.
    sessions::delete_all(&mut tx, used.user_id, None).await?;
    tx.commit().await?;
    log_in(
        &state,
        used.user_id,
        &headers,
        &ip,
        "/settings?done=password",
    )
    .await
}

// ---------------------------------------------------------------------------
// Email verification

#[derive(Template)]
#[template(path = "verify.html")]
struct VerifyTemplate {
    layout: Layout,
    token: String,
}

pub async fn verify_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(token): Path<String>,
) -> Result<Response, AppError> {
    if !login_tokens::is_current(state.db(), &token, Purpose::VerifyEmail).await? {
        return Ok(secret_url(
            expired_link(
                &state,
                &viewer,
                "confirmation link",
                ("/settings", "Send a new one from your settings"),
            )
            .await?,
        ));
    }
    Ok(secret_url(web::ok(&VerifyTemplate {
        layout: Layout::load(&state, &viewer, Nav::None).await?,
        token,
    })?))
}

pub async fn verify_email(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(token): Path<String>,
) -> Result<Response, AppError> {
    let mut tx = state.db().begin().await?;
    let Some(used) = login_tokens::consume(&mut tx, &token, Purpose::VerifyEmail).await? else {
        return expired_link(
            &state,
            &viewer,
            "confirmation link",
            ("/settings", "Send a new one from your settings"),
        )
        .await;
    };
    if !users::confirm_email(&mut tx, used.user_id, used.email.as_deref()).await? {
        return web::message(
            &state,
            &viewer,
            StatusCode::CONFLICT,
            "Address already in use",
            &["Another account has started using that email address since the link was sent."],
            Some(("/settings", "Back to your settings")),
        )
        .await;
    }
    tx.commit().await?;
    if viewer.user_id() == Some(used.user_id) {
        return Ok(see_other("/settings?done=verified"));
    }
    web::message(
        &state,
        &viewer,
        StatusCode::OK,
        "Email address confirmed",
        &["Thank you: your email address is confirmed."],
        Some(("/login", "Log in")),
    )
    .await
}

// ---------------------------------------------------------------------------
// Account settings

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate<'a> {
    layout: Layout,
    user: &'a UserCtx,
    has_password: bool,
    other_sessions: i64,
    notice: Option<&'static str>,
    errors: Vec<String>,
    new_email: String,
    dm_policy: crate::social::DmPolicy,
    email_prefs: crate::social::EmailPrefs,
    blocked: Vec<crate::content::UserRef>,
    profile: accounts::ProfileFields,
    avatar: Option<String>,
    me: crate::content::UserRef,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct DoneQuery {
    done: String,
}

fn notice_for(done: &str) -> Option<&'static str> {
    match done {
        "password" => {
            Some("Your password has been changed, and you’ve been logged out everywhere else.")
        }
        "verify" => Some("We’ve sent you an email with a link to confirm your address."),
        "email" => {
            Some("We’ve sent a link to your new address. Your address changes when you follow it.")
        }
        "verified" => Some("Your email address is confirmed."),
        "sessions" => Some("You’ve been logged out everywhere else."),
        "messages" => Some("Saved who can message you."),
        "email-prefs" => Some("Saved which emails you get."),
        "unblocked" => Some("Unblocked."),
        "profile" => Some("Your profile has been saved."),
        "avatar" => Some("Your new picture is in place."),
        "avatar-removed" => Some("Your picture has been removed."),
        "welcome" => {
            Some("Welcome! You’re a member now. This page is where you manage your account.")
        }
        _ => None,
    }
}

/// What the settings page says on top of the account's details.
struct SettingsOutcome {
    status: StatusCode,
    notice: Option<&'static str>,
    errors: Vec<String>,
    /// The new address typed into the change-email form.
    new_email: String,
    /// What was typed into the profile form, when it is shown again.
    profile: Option<accounts::ProfileFields>,
}

impl SettingsOutcome {
    fn notice(notice: Option<&'static str>) -> SettingsOutcome {
        SettingsOutcome {
            status: StatusCode::OK,
            notice,
            errors: Vec::new(),
            new_email: String::new(),
            profile: None,
        }
    }

    fn problems(errors: Vec<String>, new_email: String) -> SettingsOutcome {
        SettingsOutcome {
            status: StatusCode::BAD_REQUEST,
            notice: None,
            errors,
            new_email,
            profile: None,
        }
    }
}

async fn render_settings(
    state: &AppState,
    viewer: &Viewer,
    user: &UserCtx,
    token: Option<&str>,
    outcome: SettingsOutcome,
) -> Result<Response, AppError> {
    let row = users::account_summary(state.db(), user.id, token).await?;
    render(
        outcome.status,
        &SettingsTemplate {
            layout: Layout::load(state, viewer, Nav::Account).await?,
            user,
            has_password: row.has_password,
            other_sessions: row.other_sessions,
            notice: outcome.notice,
            errors: outcome.errors,
            new_email: outcome.new_email,
            dm_policy: crate::social::dm_policy(state.db(), user.id).await?,
            email_prefs: crate::social::email_prefs(state.db(), user.id).await?,
            blocked: crate::social::blocked_members(state.db(), user.id).await?,
            profile: match outcome.profile {
                Some(typed) => typed,
                None => users::profile_fields(state.db(), user.id).await?,
            },
            avatar: crate::upload_records::avatar_url(state.db(), user.id).await?,
            me: crate::content::UserRef {
                id: user.id,
                username: user.username.clone(),
                display_name: user.display_name.clone(),
            },
        },
    )
}

pub async fn settings_page(
    State(state): State<AppState>,
    viewer: Viewer,
    headers: HeaderMap,
    Query(query): Query<DoneQuery>,
) -> Result<Response, AppError> {
    let Some(user) = viewer.user() else {
        return Ok(web::login_first("/settings"));
    };
    render_settings(
        &state,
        &viewer,
        user,
        session_cookie(&headers),
        SettingsOutcome::notice(notice_for(&query.done)),
    )
    .await
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct ProfileForm {
    display_name: String,
    title: String,
    pronouns: String,
    status_line: String,
    location: String,
}

/// Saves the member's name and post-info column (DESIGN.md §2).
pub async fn save_profile(
    State(state): State<AppState>,
    viewer: Viewer,
    headers: HeaderMap,
    Form(form): Form<ProfileForm>,
) -> Result<Response, AppError> {
    let Some(user) = viewer.user() else {
        return Ok(web::login_first("/settings"));
    };
    // Others read it beside every post, so it is writing like a post is.
    if let Some(why) = crate::writing::restriction(user) {
        return crate::write_pages::not_allowed(&state, &viewer, why).await;
    }
    if !user_allows(&state, Action::Edit, user.id) {
        return web::rate_limited(&state, &viewer).await;
    }
    let (fields, problems) = accounts::ProfileFields::clean(
        &form.display_name,
        &form.title,
        &form.pronouns,
        &form.status_line,
        &form.location,
    );
    if !problems.is_empty() {
        let mut outcome = SettingsOutcome::problems(problems, String::new());
        outcome.profile = Some(fields);
        return render_settings(&state, &viewer, user, session_cookie(&headers), outcome).await;
    }
    users::update_profile(state.db(), user.id, &fields).await?;
    state
        .changed(crate::changes::Change::UserChanged { user_id: user.id })
        .await?;
    Ok(see_other("/settings?done=profile"))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct ChangePasswordForm {
    current: String,
    password: String,
    confirm: String,
}

pub async fn change_password(
    State(state): State<AppState>,
    viewer: Viewer,
    headers: HeaderMap,
    Form(form): Form<ChangePasswordForm>,
) -> Result<Response, AppError> {
    let Some(user) = viewer.user() else {
        return Ok(web::login_first("/settings"));
    };
    if !user_allows(&state, Action::Login, user.id) {
        return web::rate_limited(&state, &viewer).await;
    }
    let current_hash = users::password_hash(state.db(), user.id).await?;
    let mut problems = Vec::new();
    if current_hash.is_some() && !accounts::verify_password(current_hash, &form.current).await {
        problems.push("Your current password isn’t right.".to_owned());
    }
    problems.extend(new_password_problems(&form.password, &form.confirm));
    let token = session_cookie(&headers);
    if !problems.is_empty() {
        return render_settings(
            &state,
            &viewer,
            user,
            token,
            SettingsOutcome::problems(problems, String::new()),
        )
        .await;
    }
    let hash = accounts::hash_password(&form.password).await;
    let mut tx = state.db().begin().await?;
    users::set_password(&mut tx, user.id, &hash, false).await?;
    sessions::delete_all(&mut tx, user.id, token).await?;
    tx.commit().await?;
    Ok(see_other("/settings?done=password"))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct ChangeEmailForm {
    email: String,
    password: String,
}

pub async fn change_email(
    State(state): State<AppState>,
    viewer: Viewer,
    headers: HeaderMap,
    Form(form): Form<ChangeEmailForm>,
) -> Result<Response, AppError> {
    let Some(user) = viewer.user() else {
        return Ok(web::login_first("/settings"));
    };
    if !user_allows(&state, Action::VerifyEmail, user.id) {
        return web::rate_limited(&state, &viewer).await;
    }
    let token = session_cookie(&headers);
    let current_hash = users::password_hash(state.db(), user.id).await?;
    let mut problems = Vec::new();
    // Someone who has borrowed a logged-in browser mustn't be able to take
    // the account over by moving it to their own address.
    if current_hash.is_some() && !accounts::verify_password(current_hash, &form.password).await {
        problems.push("Your password isn’t right.".to_owned());
    }
    let email = match accounts::clean_email(&form.email) {
        Ok(email) => Some(email),
        Err(why) => {
            problems.push(why.to_owned());
            None
        }
    };
    if let Some(email) = &email
        && users::email_taken(state.db(), email).await?
    {
        problems.push("That address is already in use.".to_owned());
    }
    let Some(email) = email.filter(|_| problems.is_empty()) else {
        return render_settings(
            &state,
            &viewer,
            user,
            token,
            SettingsOutcome::problems(problems, form.email),
        )
        .await;
    };
    send_verification(&state, user, &email).await?;
    Ok(see_other("/settings?done=email"))
}

async fn send_verification(state: &AppState, user: &UserCtx, email: &str) -> Result<(), AppError> {
    let mut tx = state.db().begin().await?;
    let issued = login_tokens::issue(&mut tx, user.id, Purpose::VerifyEmail, Some(email)).await?;
    let site_name = settings::site_name(state.db()).await?;
    mail::queue(
        &mut tx,
        &emails::verify(
            email,
            &site_name,
            &user.display_name,
            &state.config().url(&format!("/verify/{}", issued.token)),
        ),
    )
    .await?;
    tx.commit().await?;
    state.jobs().wake();
    Ok(())
}

pub async fn resend_verification(
    State(state): State<AppState>,
    viewer: Viewer,
) -> Result<Response, AppError> {
    let Some(user) = viewer.user() else {
        return Ok(web::login_first("/settings"));
    };
    if user.email_verified {
        return Ok(see_other("/settings"));
    }
    if !user_allows(&state, Action::VerifyEmail, user.id) {
        return web::rate_limited(&state, &viewer).await;
    }
    send_verification(&state, user, &user.email).await?;
    Ok(see_other("/settings?done=verify"))
}

pub async fn log_out_elsewhere(
    State(state): State<AppState>,
    viewer: Viewer,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let Some(user) = viewer.user() else {
        return Ok(web::login_first("/settings"));
    };
    sessions::delete_all(state.db(), user.id, session_cookie(&headers)).await?;
    Ok(see_other("/settings?done=sessions"))
}
