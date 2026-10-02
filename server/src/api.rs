//! JSON endpoints for the islands (DESIGN.md §11: `/api/…`). Each does
//! what a plain form or page already does, through the same access layer
//! and the same decisions in [`crate::writing`]; only the answer is JSON
//! instead of a page. Like every other state-changing request, writes are
//! checked by [`crate::csrf`].
//!
//! Errors are `{"error": "…"}` with a fitting status: 401 for visitors,
//! 404 for anything the viewer can't see, 403, 400 or 429 otherwise.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppState;
use crate::content::{self, ReactionCount};
use crate::error::AppError;
use crate::format;
use crate::notifications as stored;
use crate::notify;
use crate::pages::parse_id;
use crate::store;
use crate::viewer::Viewer;
use crate::writing::{self, Refused};

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

fn log_in() -> Response {
    error(StatusCode::UNAUTHORIZED, "Log in first.")
}

fn not_found() -> Response {
    error(
        StatusCode::NOT_FOUND,
        "That isn’t there, or you can’t see it.",
    )
}

fn refused(refusal: Refused) -> Response {
    match refusal {
        Refused::LogIn => log_in(),
        Refused::NotFound => not_found(),
        Refused::NotAllowed(why) => error(StatusCode::FORBIDDEN, why),
        Refused::NotAllowedBecause(why) => error(StatusCode::FORBIDDEN, &why),
        Refused::Invalid(problems) => error(StatusCode::BAD_REQUEST, &problems.join(" ")),
        Refused::RateLimited => error(
            StatusCode::TOO_MANY_REQUESTS,
            "You’re doing that faster than this forum allows. Wait a minute, then try again.",
        ),
    }
}

// ---------------------------------------------------------------------------
// The composer

#[derive(Deserialize)]
pub struct PreviewRequest {
    body: String,
}

/// `POST /api/preview`: a post's HTML, rendered as it would be saved.
pub async fn preview(
    State(state): State<AppState>,
    viewer: Viewer,
    Json(request): Json<PreviewRequest>,
) -> Result<Response, AppError> {
    if viewer.user().is_none() {
        return Ok(log_in());
    }
    let body = writing::clean_body(&request.body);
    let mut problems = Vec::new();
    writing::check_body(&body, &mut problems);
    if !problems.is_empty() {
        return Ok(refused(Refused::Invalid(problems)));
    }
    if !writing::may_preview(&state, &viewer) {
        return Ok(refused(Refused::RateLimited));
    }
    let html = store::render_post(state.db(), &body).await?.html;
    Ok(Json(json!({ "html": html })).into_response())
}

/// `GET /api/posts/{id}/source`: a live post's Markdown and author, for
/// quoting it into a reply.
pub async fn source(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    if viewer.user().is_none() {
        return Ok(log_in());
    }
    let Some(post_id) = parse_id(&id) else {
        return Ok(not_found());
    };
    let Some(post) = content::post_target(state.db(), &viewer, post_id)
        .await?
        .filter(content::PostTarget::is_live)
    else {
        return Ok(not_found());
    };
    let markdown = post.body.map(|b| b.markdown).unwrap_or_default();
    Ok(Json(json!({
        "post": post.post_id,
        "thread": post.thread.thread.id,
        "seq": post.seq,
        "username": post.author.username,
        "quote": crate::write_pages::quote_text(&post.author.username, &markdown),
    }))
    .into_response())
}

// ---------------------------------------------------------------------------
// Reactions

#[derive(Deserialize)]
pub struct ReactRequest {
    emoji: String,
    action: String,
}

#[derive(Serialize)]
struct Reactions {
    reactions: Vec<ReactionCount>,
}

/// `POST /api/posts/{id}/reactions`: adds or takes back a reaction, and
/// gives the post's reactions as they now are.
pub async fn react(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Json(request): Json<ReactRequest>,
) -> Result<Response, AppError> {
    let Some(post_id) = parse_id(&id) else {
        return Ok(not_found());
    };
    let add = match request.action.as_str() {
        "add" => true,
        "remove" => false,
        _ => return Ok(refused(Refused::Invalid(vec!["Add or remove?".to_owned()]))),
    };
    if let Err(refusal) = writing::react(&state, &viewer, post_id, &request.emoji, add).await? {
        return Ok(refused(refusal));
    }
    let Some(reactions) = content::post_reaction_counts(state.db(), &viewer, post_id).await? else {
        return Ok(not_found());
    };
    Ok(Json(Reactions { reactions }).into_response())
}

// ---------------------------------------------------------------------------
// Quote-forks

/// `GET /api/posts/{id}/fork`: what the quote-fork dialog shows: the post,
/// and the categories it may be quoted into.
pub async fn fork(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    if viewer.user().is_none() {
        return Ok(log_in());
    }
    let Some(post_id) = parse_id(&id) else {
        return Ok(not_found());
    };
    let Some(source) = content::post_target(state.db(), &viewer, post_id)
        .await?
        .filter(content::PostTarget::is_live)
    else {
        return Ok(not_found());
    };
    let destinations = writing::fork_destinations(&state, &viewer, &source).await?;
    let current = source.thread.category().map(|c| c.category.slug.clone());
    Ok(Json(json!({
        "action": format!("/p/{}/fork", source.post_id),
        "source": {
            "seq": source.seq,
            "author": source.author.display_name,
            "thread": source.thread.name,
            "href": format!("/p/{}", source.post_id),
            "html": source.body.as_ref().map(|b| b.html.clone()).unwrap_or_default(),
        },
        "categories": destinations.iter().map(|c| json!({
            "slug": c.category.slug,
            "label": crate::write_pages::category_label(c),
            "selected": Some(&c.category.slug) == current.as_ref(),
        })).collect::<Vec<_>>(),
    }))
    .into_response())
}

// ---------------------------------------------------------------------------
// Notifications

/// How many notifications the menu shows.
const MENU_ITEMS: usize = 8;

/// `GET /api/notifications`: the latest notifications and the counts.
pub async fn notifications(
    State(state): State<AppState>,
    viewer: Viewer,
) -> Result<Response, AppError> {
    if viewer.user().is_none() {
        return Ok(log_in());
    }
    let list = content::notifications::list(state.db(), &viewer, 1).await?;
    let items: Vec<_> = list
        .rows
        .iter()
        .take(MENU_ITEMS)
        .map(|n| {
            let actor = n.actor.as_ref().map(|a| a.display_name.clone());
            let (text, title, href) = match &n.about {
                Some(about) => (
                    n.kind.verb().to_owned(),
                    Some(about.title.clone()),
                    format!("/notifications/{}", n.id),
                ),
                None => (
                    format!(
                        "From the moderators: {}",
                        n.message.as_deref().unwrap_or_default()
                    ),
                    None,
                    "/notifications".to_owned(),
                ),
            };
            json!({
                "id": n.id,
                "actor": actor,
                "text": text,
                "title": title,
                "href": href,
                "read": n.read,
                "at": format::datetime(&n.created_at),
                "iso": format::iso(&n.created_at),
            })
        })
        .collect();
    Ok(Json(json!({
        "unread": content::notifications::unread_count(state.db(), &viewer).await?,
        "messages": content::messages::unread_conversations(state.db(), &viewer).await?,
        "items": items,
    }))
    .into_response())
}

/// `POST /api/notifications/read`: marks them all read.
pub async fn mark_notifications_read(
    State(state): State<AppState>,
    viewer: Viewer,
) -> Result<Response, AppError> {
    let Some(user) = viewer.user() else {
        return Ok(log_in());
    };
    stored::mark_all_read(state.db(), user.id).await?;
    notify::counts_changed(&state, user.id);
    Ok(Json(json!({
        "unread": 0,
        "messages": content::messages::unread_conversations(state.db(), &viewer).await?,
    }))
    .into_response())
}
