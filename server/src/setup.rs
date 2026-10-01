//! First-run setup (DESIGN.md §10). When the server starts and there is no
//! admin account, it makes a one-time token and prints a link with it to
//! the log. `/setup?token=…` then creates the admin account and names the
//! site; the next steps (email settings with a test send, then the first
//! invites) are ordinary admin pages.
//!
//! The token lives only in memory: restarting the server before setup is
//! finished prints a new one.

use askama::Template;
use axum::Form;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde::Deserialize;
use sqlx::PgPool;

use crate::AppState;
use crate::accounts;
use crate::admin_pages::{SmtpFields, save_smtp, send_test};
use crate::changes::Change;
use crate::client::ClientIp;
use crate::error::AppError;
use crate::invites::{self, CreatedInvite, NewInvite};
use crate::mail;
use crate::ratelimit::{Action, Scope};
use crate::sessions;
use crate::settings;
use crate::tokens;
use crate::viewer::{UserCtx, Viewer};
use crate::web::{self, Layout, Nav, render, secret_url, see_other};

/// Whether the site still needs its first admin.
pub async fn needed(db: &PgPool) -> sqlx::Result<bool> {
    Ok(!sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM users WHERE role = 'admin' AND deleted_at IS NULL) AS "e!""#
    )
    .fetch_one(db)
    .await?)
}

/// If the site has no admin, makes a new setup token and returns the link
/// to print. Each call replaces the previous token.
pub async fn prepare(state: &AppState) -> sqlx::Result<Option<String>> {
    if !needed(state.db()).await? {
        return Ok(None);
    }
    let token = tokens::new_token();
    *state
        .setup_token()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(tokens::hash(&token));
    Ok(Some(state.config().url(&format!("/setup?token={token}"))))
}

fn token_matches(state: &AppState, token: &str) -> bool {
    let expected = state
        .setup_token()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    expected.is_some_and(|expected| tokens::same(&expected, &tokens::hash(token)))
}

fn admin(viewer: &Viewer) -> Option<&UserCtx> {
    viewer.user().filter(|_| viewer.is_admin())
}

/// For a visitor who isn't an admin: logging in might help one who isn't
/// logged in; anyone else isn't allowed.
async fn not_admin(state: &AppState, viewer: &Viewer, path: &str) -> Result<Response, AppError> {
    match viewer.user() {
        None => Ok(web::login_first(path)),
        Some(_) => web::forbidden(state, viewer).await,
    }
}

async fn not_needed(state: &AppState, viewer: &Viewer) -> Result<Response, AppError> {
    web::message(
        state,
        viewer,
        StatusCode::NOT_FOUND,
        "Already set up",
        &["This forum has an admin account already, so there is nothing to set up."],
        Some(("/", "Go to the forum index")),
    )
    .await
}

async fn bad_token(state: &AppState, viewer: &Viewer) -> Result<Response, AppError> {
    web::message(
        state,
        viewer,
        StatusCode::FORBIDDEN,
        "Setup link not valid",
        &[
            "This setup link isn’t valid. Use the link printed in the server’s log when it started.",
            "Restarting the server prints a new link.",
        ],
        None,
    )
    .await
}

// ---------------------------------------------------------------------------
// Step 1: the admin account and the site's name

#[derive(Template)]
#[template(path = "setup.html")]
struct SetupTemplate {
    layout: Layout,
    token: String,
    errors: Vec<String>,
    site_name: String,
    username: String,
    display_name: String,
    email: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct TokenQuery {
    token: String,
}

pub async fn setup_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Query(query): Query<TokenQuery>,
) -> Result<Response, AppError> {
    if !needed(state.db()).await? {
        return Ok(secret_url(not_needed(&state, &viewer).await?));
    }
    if !token_matches(&state, &query.token) {
        return Ok(secret_url(bad_token(&state, &viewer).await?));
    }
    let site_name = settings::site_name(state.db()).await?;
    Ok(secret_url(web::ok(&SetupTemplate {
        layout: Layout::load(&state, &viewer, Nav::None).await?,
        token: query.token,
        errors: Vec::new(),
        site_name,
        username: String::new(),
        display_name: String::new(),
        email: String::new(),
    })?))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct SetupForm {
    token: String,
    site_name: String,
    username: String,
    display_name: String,
    email: String,
    password: String,
    confirm: String,
}

pub async fn create_admin(
    State(state): State<AppState>,
    viewer: Viewer,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    Form(form): Form<SetupForm>,
) -> Result<Response, AppError> {
    if !state.limiter().check(Action::Setup, Scope::Ip, &ip) {
        return web::rate_limited(&state, &viewer).await;
    }
    if !needed(state.db()).await? {
        return not_needed(&state, &viewer).await;
    }
    if !token_matches(&state, &form.token) {
        return bad_token(&state, &viewer).await;
    }
    let mut problems = Vec::new();
    let site_name = form
        .site_name
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if site_name.is_empty() || site_name.chars().count() > 80 {
        problems.push("The site name should be between 1 and 80 characters.".to_owned());
    }
    let username = form.username.trim();
    if let Err(why) = accounts::validate_username(username) {
        problems.push(why.to_owned());
    }
    let display_name = accounts::clean_display_name(&form.display_name).unwrap_or_else(|why| {
        problems.push(why.to_owned());
        String::new()
    });
    let email = accounts::clean_email(&form.email).unwrap_or_else(|why| {
        problems.push(why.to_owned());
        String::new()
    });
    // The admin needs a password: email may not work until step 2.
    if let Err(why) = accounts::validate_password(&form.password) {
        problems.push(why.to_owned());
    } else if form.password != form.confirm {
        problems.push("The two passwords don’t match.".to_owned());
    }
    if !problems.is_empty() {
        return Ok(secret_url(render(
            StatusCode::BAD_REQUEST,
            &SetupTemplate {
                layout: Layout::load(&state, &viewer, Nav::None).await?,
                token: form.token,
                errors: problems,
                site_name: form.site_name,
                username: form.username,
                display_name: form.display_name,
                email: form.email,
            },
        )?));
    }
    let hash = accounts::hash_password(&form.password).await;
    let mut tx = state.db().begin().await?;
    // Two people racing through setup can't both make an admin.
    sqlx::query!("SELECT pg_advisory_xact_lock(hashtext('parlour setup'))")
        .execute(&mut *tx)
        .await?;
    let exists = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM users WHERE role = 'admin' AND deleted_at IS NULL) AS "e!""#
    )
    .fetch_one(&mut *tx)
    .await?;
    if exists {
        return not_needed(&state, &viewer).await;
    }
    let inserted = sqlx::query_scalar!(
        "INSERT INTO users (username, display_name, email, email_verified_at, password_hash, role)
         VALUES ($1, $2, $3, now(), $4, 'admin')
         RETURNING id",
        username as _,
        display_name,
        email.as_str() as _,
        hash,
    )
    .fetch_one(&mut *tx)
    .await;
    let user_id = match inserted {
        Ok(id) => id,
        Err(sqlx::Error::Database(err)) if err.is_unique_violation() => {
            return Ok(secret_url(render(
                StatusCode::BAD_REQUEST,
                &SetupTemplate {
                    layout: Layout::load(&state, &viewer, Nav::None).await?,
                    token: form.token,
                    errors: vec![
                        "An account with that username or email address already exists.".to_owned(),
                    ],
                    site_name: form.site_name,
                    username: form.username,
                    display_name: form.display_name,
                    email: form.email,
                },
            )?));
        }
        Err(err) => return Err(err.into()),
    };
    settings::set(&mut tx, "site_name", &site_name).await?;
    tx.commit().await?;
    *state
        .setup_token()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
    state.changed(Change::SiteChanged).await?;
    state.changed(Change::UserCreated { user_id }).await?;
    tracing::info!(username, "setup created the admin account");
    let token = sessions::create(state.db(), user_id, &headers, &ip).await?;
    Ok(web::start_session(
        see_other("/setup/email"),
        &state,
        &token,
    ))
}

// ---------------------------------------------------------------------------
// Step 2: email

#[derive(Template)]
#[template(path = "setup_email.html")]
struct SetupEmailTemplate {
    layout: Layout,
    smtp: SmtpFields,
    smtp_has_password: bool,
    smtp_from_env: bool,
    admin_email: String,
    errors: Vec<String>,
    tested: Option<String>,
}

pub async fn email_page(
    State(state): State<AppState>,
    viewer: Viewer,
) -> Result<Response, AppError> {
    let Some(user) = admin(&viewer) else {
        return not_admin(&state, &viewer, "/setup/email").await;
    };
    let saved = mail::saved_smtp_settings(state.db()).await?;
    let effective = saved.clone().or_else(|| state.config().smtp.clone());
    web::ok(&SetupEmailTemplate {
        layout: Layout::load(&state, &viewer, Nav::Admin).await?,
        smtp: SmtpFields::from_settings(effective.as_ref()),
        smtp_has_password: effective.as_ref().is_some_and(|s| !s.password.is_empty()),
        smtp_from_env: saved.is_none() && state.config().smtp.is_some(),
        admin_email: user.email.clone(),
        errors: Vec::new(),
        tested: None,
    })
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct SetupEmailForm {
    /// "test" or "save".
    action: String,
    #[serde(flatten)]
    smtp: SmtpFields,
}

pub async fn save_email(
    State(state): State<AppState>,
    viewer: Viewer,
    Form(form): Form<SetupEmailForm>,
) -> Result<Response, AppError> {
    let Some(user) = admin(&viewer) else {
        return not_admin(&state, &viewer, "/setup/email").await;
    };
    let saved = mail::saved_smtp_settings(state.db()).await?;
    let effective = saved.clone().or_else(|| state.config().smtp.clone());
    let parsed = form
        .smtp
        .parse(effective.as_ref().map(|s| s.password.as_str()));
    let mut errors = Vec::new();
    let mut tested = None;
    match (&parsed, form.action.as_str()) {
        (Err(problems), _) => errors.extend(problems.iter().cloned()),
        (Ok(smtp), "test") => {
            match send_test(&state, smtp.as_ref().or(effective.as_ref()), &user.email).await {
                Ok(()) => {
                    tested = Some(format!(
                        "A test email is on its way to {}. Check that it arrived, then save.",
                        user.email
                    ))
                }
                Err(why) => errors.push(format!("The test email couldn’t be sent: {why}")),
            }
        }
        (Ok(smtp), _) => {
            save_smtp(&mut *state.db().acquire().await?, smtp.as_ref()).await?;
            return Ok(see_other("/setup/invites"));
        }
    }
    let status = if errors.is_empty() {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    };
    render(
        status,
        &SetupEmailTemplate {
            layout: Layout::load(&state, &viewer, Nav::Admin).await?,
            smtp: SmtpFields {
                smtp_password: String::new(),
                ..form.smtp.clone()
            },
            smtp_has_password: effective.as_ref().is_some_and(|s| !s.password.is_empty())
                || !form.smtp.smtp_password.is_empty(),
            smtp_from_env: saved.is_none() && state.config().smtp.is_some(),
            admin_email: user.email.clone(),
            errors,
            tested,
        },
    )
}

// ---------------------------------------------------------------------------
// Step 3: the first invites

#[derive(Template)]
#[template(path = "setup_invites.html")]
struct SetupInvitesTemplate {
    layout: Layout,
    errors: Vec<String>,
    created: Vec<CreatedInvite>,
    emails: String,
    uses: String,
}

impl SetupInvitesTemplate {
    fn shows_link(&self) -> bool {
        self.created.iter().any(|invite| invite.link.is_some())
    }
}

pub async fn invites_page(
    State(state): State<AppState>,
    viewer: Viewer,
) -> Result<Response, AppError> {
    if admin(&viewer).is_none() {
        return not_admin(&state, &viewer, "/setup/invites").await;
    }
    web::ok(&SetupInvitesTemplate {
        layout: Layout::load(&state, &viewer, Nav::Admin).await?,
        errors: Vec::new(),
        created: Vec::new(),
        emails: String::new(),
        uses: String::new(),
    })
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct SetupInvitesForm {
    /// Addresses to invite, one per line (or separated by commas).
    emails: String,
    /// If set, also make a link with this many uses.
    uses: String,
}

pub async fn create_invites(
    State(state): State<AppState>,
    viewer: Viewer,
    Form(form): Form<SetupInvitesForm>,
) -> Result<Response, AppError> {
    let Some(user) = admin(&viewer) else {
        return not_admin(&state, &viewer, "/setup/invites").await;
    };
    let mut errors = Vec::new();
    let mut created = Vec::new();
    let addresses: Vec<&str> = form
        .emails
        .split(['\n', ',', ';'])
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .collect();
    if addresses.len() > 100 {
        errors.push("Invite at most 100 addresses at a time.".to_owned());
    }
    let uses = match form.uses.trim() {
        "" | "0" => None,
        n => match n.parse::<i32>() {
            Ok(n) if (1..=invites::MAX_USES).contains(&n) => Some(n),
            _ => {
                errors.push(format!(
                    "A link can be used between 1 and {} times.",
                    invites::MAX_USES
                ));
                None
            }
        },
    };
    if errors.is_empty() {
        let mut failed = Vec::new();
        for address in &addresses {
            let new = NewInvite {
                email: Some((*address).to_owned()),
                note: "From setup".to_owned(),
                expires_in_days: 30,
                max_uses: 1,
                grant_group_ids: Vec::new(),
            };
            match invites::create(&state, user, &new).await? {
                Ok(invite) => created.push(invite),
                Err(problems) => {
                    failed.push(address.to_string());
                    errors.extend(problems.into_iter().map(|p| format!("{address}: {p}")));
                }
            }
        }
        if let Some(uses) = uses {
            let new = NewInvite {
                email: None,
                note: "Link from setup".to_owned(),
                expires_in_days: 30,
                max_uses: uses,
                grant_group_ids: Vec::new(),
            };
            match invites::create(&state, user, &new).await? {
                Ok(invite) => created.push(invite),
                Err(problems) => errors.extend(problems),
            }
        }
        let status = if errors.is_empty() {
            StatusCode::OK
        } else {
            StatusCode::BAD_REQUEST
        };
        return render(
            status,
            &SetupInvitesTemplate {
                layout: Layout::load(&state, &viewer, Nav::Admin).await?,
                errors,
                created,
                emails: failed.join("\n"),
                uses: String::new(),
            },
        );
    }
    render(
        StatusCode::BAD_REQUEST,
        &SetupInvitesTemplate {
            layout: Layout::load(&state, &viewer, Nav::Admin).await?,
            errors,
            created,
            emails: form.emails,
            uses: form.uses,
        },
    )
}
