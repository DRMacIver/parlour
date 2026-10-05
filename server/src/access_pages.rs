//! The break-glass log (DESIGN.md §8, §11): `/admin/access`, every grant,
//! for admins, and `/settings/access`, the grants a member was told about,
//! whether or not they still read what each opened. Both list every grant,
//! newest first, a page at a time (`?page=n`), since the log keeps
//! everything. Both depend on who is looking and are never cached. What
//! each may see is decided in [`crate::content::access`].

use askama::Template;
use axum::extract::{Query, State};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Redirect, Response};

use crate::AppState;
use crate::admin_pages::{admin, not_admin};
use crate::content::access::{self, LOG_PER_PAGE, LogPage, Logged};
use crate::content::page_count;
use crate::error::AppError;
use crate::pagination::{PageItem, Pagination, page_href};
use crate::viewer::Viewer;
use crate::web::{self, Layout, Nav, redirect_unless_canonical, render};

#[derive(Template)]
#[template(path = "access_log.html")]
struct AccessLogTemplate {
    layout: Layout,
    /// The admins' log, rather than a member's own.
    admin: bool,
    entries: Vec<Logged>,
    /// How many grants the log has in all.
    total: i64,
    pagination: Pagination,
}

/// What a log's address asks for.
enum Requested {
    /// This page of it, asked for the one way it is written.
    Page(i64),
    /// The page, asked for another way: go to its address.
    Elsewhere(String),
    /// Nothing there.
    Nothing,
}

/// The page of the log at `base` that `uri` asks for: `?page=n`, and
/// nothing else.
fn requested(uri: &Uri, base: &str) -> Requested {
    let params = Query::<Vec<(String, String)>>::try_from_uri(uri)
        .map(|Query(params)| params)
        .unwrap_or_default();
    let mut page = 1;
    for (key, value) in &params {
        match (key.as_str(), value.parse::<i64>()) {
            ("page", Ok(n)) if n >= 1 && *value == n.to_string() => page = n,
            _ => return Requested::Nothing,
        }
    }
    let canonical = page_href(base, page);
    match redirect_unless_canonical(uri, &canonical) {
        None => Requested::Page(page),
        Some(_) => Requested::Elsewhere(canonical),
    }
}

async fn show(
    state: &AppState,
    viewer: &Viewer,
    uri: &Uri,
    base: &str,
    admin: bool,
) -> Result<Response, AppError> {
    let page = match requested(uri, base) {
        Requested::Page(page) => page,
        Requested::Elsewhere(canonical) => {
            return Ok(Redirect::permanent(&canonical).into_response());
        }
        Requested::Nothing => return web::not_found(state, viewer).await,
    };
    let LogPage { total, entries } = if admin {
        access::log(state.db(), viewer, page).await?
    } else {
        access::affecting(state.db(), viewer, page).await?
    };
    let pages = page_count(total, LOG_PER_PAGE).max(1);
    if page > pages {
        return web::not_found(state, viewer).await;
    }
    render(
        StatusCode::OK,
        &AccessLogTemplate {
            layout: Layout::load(state, viewer, if admin { Nav::Admin } else { Nav::Account })
                .await?,
            admin,
            entries,
            total,
            pagination: Pagination::new(base, page, pages),
        },
    )
}

/// `/admin/access`: every grant, newest first.
pub async fn admin_log(
    State(state): State<AppState>,
    viewer: Viewer,
    uri: Uri,
) -> Result<Response, AppError> {
    if admin(&viewer).is_none() {
        return not_admin(&state, &viewer, "/admin/access").await;
    }
    show(&state, &viewer, &uri, "/admin/access", true).await
}

/// `/settings/access`: the grants the member was told about.
pub async fn member_log(
    State(state): State<AppState>,
    viewer: Viewer,
    uri: Uri,
) -> Result<Response, AppError> {
    if viewer.user().is_none() {
        return Ok(web::login_first("/settings/access"));
    }
    show(&state, &viewer, &uri, "/settings/access", false).await
}
