//! The break-glass log (DESIGN.md §8, §11): `/admin/access`, every grant,
//! for admins, and `/settings/access`, the grants on threads, spaces and
//! conversations a member can read. Both depend on who is looking and are
//! never cached. What each may see is decided in
//! [`crate::content::access`].

use askama::Template;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;

use crate::AppState;
use crate::admin_pages::{admin, not_admin};
use crate::content::access::{self, LOG_LIMIT, Logged};
use crate::error::AppError;
use crate::viewer::Viewer;
use crate::web::{self, Layout, Nav, render};

#[derive(Template)]
#[template(path = "access_log.html")]
struct AccessLogTemplate {
    layout: Layout,
    /// The admins' log, rather than a member's own.
    admin: bool,
    entries: Vec<Logged>,
    limit: i64,
}

/// `/admin/access`: every grant, newest first.
pub async fn admin_log(
    State(state): State<AppState>,
    viewer: Viewer,
) -> Result<Response, AppError> {
    if admin(&viewer).is_none() {
        return not_admin(&state, &viewer, "/admin/access").await;
    }
    render(
        StatusCode::OK,
        &AccessLogTemplate {
            layout: Layout::load(&state, &viewer, Nav::Admin).await?,
            admin: true,
            entries: access::log(state.db(), &viewer).await?,
            limit: LOG_LIMIT,
        },
    )
}

/// `/settings/access`: the grants on what the member can read.
pub async fn member_log(
    State(state): State<AppState>,
    viewer: Viewer,
) -> Result<Response, AppError> {
    if viewer.user().is_none() {
        return Ok(web::login_first("/settings/access"));
    }
    render(
        StatusCode::OK,
        &AccessLogTemplate {
            layout: Layout::load(&state, &viewer, Nav::Account).await?,
            admin: false,
            entries: access::affecting(state.db(), &viewer).await?,
            limit: LOG_LIMIT,
        },
    )
}
