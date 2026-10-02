//! A member's own data (DESIGN.md §2): downloading it, and deleting the
//! account; and the site's terms and privacy pages, which admins write.

use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::AppState;
use crate::accounts;
use crate::changes::Change;
use crate::content::Deps;
use crate::error::AppError;
use crate::markdown;
use crate::ratelimit::{Action, Scope};
use crate::settings::{self, SitePages};
use crate::users::{self, Deletion};
use crate::viewer::Viewer;
use crate::web::{self, Layout, Nav, cacheable, render, see_other};

/// `POST /settings/export`: everything held about the member, as a JSON
/// file to save.
pub async fn export(State(state): State<AppState>, viewer: Viewer) -> Result<Response, AppError> {
    let Some(user) = viewer.user() else {
        return Ok(web::login_first("/settings"));
    };
    if !state
        .limiter()
        .check(Action::Export, Scope::User, &user.id.to_string())
    {
        return web::rate_limited(&state, &viewer).await;
    }
    let Some(data) = crate::db::export::export(state.db(), &viewer).await? else {
        return Ok(web::login_first("/settings"));
    };
    let filename = format!(
        "{}-{}.json",
        user.username,
        chrono::Utc::now().format("%Y-%m-%d")
    );
    Ok((
        [
            (CONTENT_TYPE, "application/json".to_owned()),
            (
                CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        serde_json::to_string_pretty(&data)?,
    )
        .into_response())
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct DeleteForm {
    password: String,
    confirm: String,
}

/// `POST /settings/delete`: deletes the account, once the member has typed
/// their username and, if they have one, their password.
pub async fn delete_account(
    State(state): State<AppState>,
    viewer: Viewer,
    Form(form): Form<DeleteForm>,
) -> Result<Response, AppError> {
    let Some(user) = viewer.user() else {
        return Ok(web::login_first("/settings"));
    };
    if !state
        .limiter()
        .check(Action::Login, Scope::User, &user.id.to_string())
    {
        return web::rate_limited(&state, &viewer).await;
    }
    let refuse = |message: &'static str| {
        let state = state.clone();
        let viewer = viewer.clone();
        async move {
            web::message(
                &state,
                &viewer,
                StatusCode::BAD_REQUEST,
                "Your account was not deleted",
                &[message],
                Some(("/settings#delete", "Back to your settings")),
            )
            .await
        }
    };
    if !form.confirm.trim().eq_ignore_ascii_case(&user.username) {
        return refuse("Type your username to confirm that you want to delete your account.").await;
    }
    let hash = users::password_hash(state.db(), user.id).await?;
    if hash.is_some() && !accounts::verify_password(hash, &form.password).await {
        return refuse("Your password isn’t right.").await;
    }
    match users::delete_account(state.db(), user.id).await? {
        Deletion::Deleted { unused_picture } => {
            if let Some((sha256, ext)) = unused_picture {
                crate::uploads::remove(&state.config().uploads_dir, &sha256, &ext).await?;
            }
        }
        Deletion::NoSuchMember => return Ok(web::login_first("/settings")),
        Deletion::LastAdmin => {
            return refuse(
                "You are the only admin. Make someone else an admin first, so that the forum still has one.",
            )
            .await;
        }
    }
    state
        .changed(Change::UserDeleted { user_id: user.id })
        .await?;
    let response = web::message(
        &state,
        &Viewer::Anonymous,
        StatusCode::OK,
        "Your account has been deleted",
        &[
            "Your profile, picture, email address and password are gone, and you have been logged out everywhere.",
            "What you posted stays where it is, shown as by a deleted member.",
        ],
        Some(("/", "Back to the forum index")),
    )
    .await?;
    Ok(web::end_session(response, &state))
}

// ---------------------------------------------------------------------------
// Terms and privacy

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SitePage {
    Terms,
    Privacy,
}

#[derive(Template)]
#[template(path = "site_page.html")]
struct SitePageTemplate {
    layout: Layout,
    title: &'static str,
    html: String,
}

/// A page that is the same for everyone, so it is cached: like the reading
/// pages it has one URL, and other spellings of it redirect there rather
/// than each taking a place in the cache.
async fn site_page(
    state: &AppState,
    viewer: &Viewer,
    uri: &axum::http::Uri,
    which: SitePage,
) -> Result<Response, AppError> {
    let (path, title) = match which {
        SitePage::Terms => ("/terms", "Terms of use"),
        SitePage::Privacy => ("/privacy", "Privacy"),
    };
    if let Some(redirect) = web::redirect_unless_canonical(uri, path) {
        return Ok(redirect);
    }
    let pages = settings::site_pages(state.db()).await?;
    let html = match which {
        SitePage::Terms => pages.terms_html,
        SitePage::Privacy => pages.privacy_html,
    };
    cacheable(
        &SitePageTemplate {
            layout: Layout::load(state, viewer, Nav::None).await?,
            title,
            html,
        },
        &Deps::new(),
    )
}

pub async fn terms(
    State(state): State<AppState>,
    viewer: Viewer,
    uri: axum::http::Uri,
) -> Result<Response, AppError> {
    site_page(&state, &viewer, &uri, SitePage::Terms).await
}

pub async fn privacy(
    State(state): State<AppState>,
    viewer: Viewer,
    uri: axum::http::Uri,
) -> Result<Response, AppError> {
    site_page(&state, &viewer, &uri, SitePage::Privacy).await
}

#[derive(Template)]
#[template(path = "admin_pages.html")]
struct AdminPagesTemplate {
    layout: Layout,
    pages: SitePages,
    saved: bool,
}

pub async fn admin_pages_page(
    State(state): State<AppState>,
    viewer: Viewer,
    uri: axum::http::Uri,
) -> Result<Response, AppError> {
    if viewer.user().is_none() {
        return Ok(web::login_first("/admin/pages"));
    }
    if !viewer.is_admin() {
        return web::forbidden(&state, &viewer).await;
    }
    render(
        StatusCode::OK,
        &AdminPagesTemplate {
            layout: Layout::load(&state, &viewer, Nav::Admin).await?,
            pages: settings::site_pages(state.db()).await?,
            saved: uri.query() == Some("saved"),
        },
    )
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct PagesForm {
    terms: String,
    privacy: String,
}

/// The longest page, in characters.
const MAX_PAGE_CHARS: usize = 100_000;

pub async fn save_admin_pages(
    State(state): State<AppState>,
    viewer: Viewer,
    Form(form): Form<PagesForm>,
) -> Result<Response, AppError> {
    if viewer.user().is_none() {
        return Ok(web::login_first("/admin/pages"));
    }
    if !viewer.is_admin() {
        return web::forbidden(&state, &viewer).await;
    }
    let terms = crate::writing::clean_body(&form.terms);
    let privacy = crate::writing::clean_body(&form.privacy);
    for text in [&terms, &privacy] {
        if text.chars().count() > MAX_PAGE_CHARS
            || markdown::nesting_depth(text) > markdown::MAX_NESTING
        {
            return web::message(
                &state,
                &viewer,
                StatusCode::BAD_REQUEST,
                "That didn’t work",
                &["Each page can be up to 100,000 characters, without deeply nested quotes or lists."],
                Some(("/admin/pages", "Back")),
            )
            .await;
        }
    }
    let pages = SitePages {
        terms_html: markdown::render(&terms),
        privacy_html: markdown::render(&privacy),
        terms_md: terms,
        privacy_md: privacy,
    };
    settings::set(state.db(), "pages", &pages).await?;
    state.changed(Change::SiteChanged).await?;
    Ok(see_other("/admin/pages?saved"))
}
