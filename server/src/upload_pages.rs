//! Uploading images and serving them (DESIGN.md §9).
//!
//! - `/uploads/new` is a plain form for adding an image to a post without
//!   JavaScript: it uploads the image and gives the Markdown to paste into
//!   the post. The composer island uploads pasted and dropped images to
//!   `/api/uploads` instead and inserts the Markdown itself.
//! - `/settings/avatar` sets or removes the member's avatar.
//! - `/uploads/{hash}.{ext}` serves an image to whoever may see it (see
//!   [`crate::content::uploads`]). The address never changes what it
//!   holds, but who may fetch it does, and staff may take an image down,
//!   so it is cached only briefly and then checked again (see
//!   [`PUBLIC_IMAGE`]): by shared caches too when every visitor may see
//!   the image, and otherwise only by the browser.

use askama::Template;
use axum::Json;
use axum::extract::multipart::{Multipart, MultipartError};
use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::AppState;
use crate::changes::Change;
use crate::content::uploads::{UploadAccess, upload_access};
use crate::error::AppError;
use crate::http_cache::{OwnCacheControl, etag_matches};
use crate::ratelimit::{Action, Scope};
use crate::upload_records;
use crate::uploads::{self, Processed, Purpose, Refusal};
use crate::viewer::Viewer;
use crate::web::{self, Layout, Nav, render, see_other};
use crate::writing;

/// Request bodies for uploads may be this big: the largest image, and the
/// form around it.
pub const MAX_BODY_BYTES: usize = uploads::MAX_UPLOAD_BYTES + 64 * 1024;

/// How long an image may be kept before it is checked again with its
/// `ETag` (which, being its hash, is cheap to check). Shared caches keep
/// an image every visitor may see for a few minutes, and browsers keep
/// any image for an hour: an image that is taken down, or whose post is
/// hidden, deleted or moved somewhere more private, stops being served
/// soon after.
const PUBLIC_IMAGE: &str = "public, max-age=3600, s-maxage=300";
const PRIVATE_IMAGE: &str = "private, max-age=3600";

/// Why an upload didn't happen.
enum Failed {
    LogIn,
    NotAllowed(&'static str),
    RateLimited,
    Refused(Refusal),
    /// No file in the form.
    NoFile,
    /// The body was too big or broken.
    Body(MultipartError),
}

impl Failed {
    fn status(&self) -> StatusCode {
        match self {
            Failed::LogIn => StatusCode::UNAUTHORIZED,
            Failed::NotAllowed(_) => StatusCode::FORBIDDEN,
            Failed::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Failed::Refused(Refusal::TooBig) => StatusCode::PAYLOAD_TOO_LARGE,
            Failed::Refused(_) | Failed::NoFile => StatusCode::BAD_REQUEST,
            Failed::Body(err) => err.status(),
        }
    }

    fn message(&self) -> String {
        match self {
            Failed::LogIn => "Log in to upload images.".to_owned(),
            Failed::NotAllowed(why) => (*why).to_owned(),
            Failed::RateLimited => {
                "You’re uploading faster than this forum allows. Wait a minute, then try again."
                    .to_owned()
            }
            Failed::Refused(refusal) => refusal.message(),
            Failed::NoFile => "Choose an image to upload.".to_owned(),
            Failed::Body(err) if err.status() == StatusCode::PAYLOAD_TOO_LARGE => {
                Refusal::TooBig.message()
            }
            Failed::Body(_) => "The upload didn’t arrive whole. Try again.".to_owned(),
        }
    }
}

/// Reads the `file` field of an upload form.
async fn read_file(multipart: &mut Multipart) -> Result<Vec<u8>, Failed> {
    while let Some(field) = multipart.next_field().await.map_err(Failed::Body)? {
        if field.name() == Some("file") {
            let bytes = field.bytes().await.map_err(Failed::Body)?;
            if bytes.is_empty() {
                return Err(Failed::NoFile);
            }
            return Ok(bytes.to_vec());
        }
    }
    Err(Failed::NoFile)
}

/// Checks, processes, stores and records an uploaded image for the viewer.
async fn accept(
    state: &AppState,
    viewer: &Viewer,
    multipart: &mut Multipart,
    purpose: Purpose,
) -> Result<Result<(i64, Processed), Failed>, AppError> {
    let Some(user) = viewer.user() else {
        return Ok(Err(Failed::LogIn));
    };
    if let Some(why) = writing::restriction(user) {
        return Ok(Err(Failed::NotAllowed(why)));
    }
    if !state
        .limiter()
        .check(Action::Upload, Scope::User, &user.id.to_string())
    {
        return Ok(Err(Failed::RateLimited));
    }
    let data = match read_file(multipart).await {
        Ok(data) => data,
        Err(failed) => return Ok(Err(failed)),
    };
    let image = match uploads::process_blocking(data, purpose).await {
        Ok(image) => image,
        Err(refusal) => return Ok(Err(Failed::Refused(refusal))),
    };
    if upload_records::is_removed(state.db(), &image.sha256).await? {
        return Ok(Err(Failed::Refused(Refusal::Removed)));
    }
    uploads::store(&state.config().uploads_dir, &image).await?;
    let Some(id) = upload_records::record(state.db(), user.id, &image).await? else {
        // Removed just now: the file may be the one just stored.
        uploads::remove(&state.config().uploads_dir, &image.sha256, image.ext).await?;
        return Ok(Err(Failed::Refused(Refusal::Removed)));
    };
    Ok(Ok((id, image)))
}

#[derive(Template)]
#[template(path = "upload.html")]
struct UploadTemplate {
    layout: Layout,
    uploaded: Option<Processed>,
    error: Option<String>,
}

impl UploadTemplate {
    fn markdown(&self) -> String {
        self.uploaded
            .as_ref()
            .map(|image| format!("![]({})", image.url()))
            .unwrap_or_default()
    }
}

pub async fn upload_page(
    State(state): State<AppState>,
    viewer: Viewer,
) -> Result<Response, AppError> {
    if viewer.user().is_none() {
        return Ok(web::login_first("/uploads/new"));
    }
    render(
        StatusCode::OK,
        &UploadTemplate {
            layout: Layout::load(&state, &viewer, Nav::None).await?,
            uploaded: None,
            error: None,
        },
    )
}

pub async fn upload(
    State(state): State<AppState>,
    viewer: Viewer,
    mut multipart: Multipart,
) -> Result<Response, AppError> {
    let (status, uploaded, error) =
        match accept(&state, &viewer, &mut multipart, Purpose::Post).await? {
            Ok((_, image)) => (StatusCode::OK, Some(image), None),
            Err(Failed::LogIn) => return Ok(web::login_first("/uploads/new")),
            Err(failed) => (failed.status(), None, Some(failed.message())),
        };
    render(
        status,
        &UploadTemplate {
            layout: Layout::load(&state, &viewer, Nav::None).await?,
            uploaded,
            error,
        },
    )
}

/// `POST /api/uploads`, for the composer island: the image's address and
/// the Markdown to insert, or an error, as JSON.
pub async fn api_upload(
    State(state): State<AppState>,
    viewer: Viewer,
    mut multipart: Multipart,
) -> Result<Response, AppError> {
    Ok(
        match accept(&state, &viewer, &mut multipart, Purpose::Post).await? {
            Ok((_, image)) => Json(json!({
                "url": image.url(),
                "markdown": format!("![]({})", image.url()),
                "width": image.width,
                "height": image.height,
            }))
            .into_response(),
            Err(failed) => {
                (failed.status(), Json(json!({ "error": failed.message() }))).into_response()
            }
        },
    )
}

pub async fn set_avatar(
    State(state): State<AppState>,
    viewer: Viewer,
    mut multipart: Multipart,
) -> Result<Response, AppError> {
    match accept(&state, &viewer, &mut multipart, Purpose::Avatar).await? {
        Ok((id, _)) => {
            let user_id = viewer.user_id().expect("only members upload");
            upload_records::set_avatar(state.db(), user_id, Some(id)).await?;
            state.changed(Change::UserChanged { user_id }).await?;
            Ok(see_other("/settings?done=avatar"))
        }
        Err(Failed::LogIn) => Ok(web::login_first("/settings")),
        Err(failed) => {
            web::message(
                &state,
                &viewer,
                failed.status(),
                "That didn’t work",
                &[&failed.message()],
                Some(("/settings#profile", "Back to your settings")),
            )
            .await
        }
    }
}

pub async fn remove_avatar(
    State(state): State<AppState>,
    viewer: Viewer,
) -> Result<Response, AppError> {
    let Some(user) = viewer.user() else {
        return Ok(web::login_first("/settings"));
    };
    if !user.may_act() {
        return web::forbidden(&state, &viewer).await;
    }
    upload_records::set_avatar(state.db(), user.id, None).await?;
    state
        .changed(Change::UserChanged { user_id: user.id })
        .await?;
    Ok(see_other("/settings?done=avatar-removed"))
}

/// `GET /uploads/{hash}.{ext}`.
pub async fn serve(
    State(state): State<AppState>,
    viewer: Viewer,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Response, AppError> {
    let Some((sha256, ext)) = uploads::parse_name(&name) else {
        return web::not_found(&state, &viewer).await;
    };
    let Some(access) = upload_access(state.db(), &viewer, sha256, ext).await? else {
        return web::not_found(&state, &viewer).await;
    };
    let cache_control = match access {
        UploadAccess::Public => PUBLIC_IMAGE,
        UploadAccess::Private => PRIVATE_IMAGE,
    };
    let etag = format!("\"{sha256}\"");
    let mut response = if headers
        .get(IF_NONE_MATCH)
        .is_some_and(|value| etag_matches(value, &etag))
    {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let path = uploads::path(&state.config().uploads_dir, sha256, ext);
        match tokio::fs::read(&path).await {
            Ok(bytes) => ([(CONTENT_TYPE, uploads::mime_of(ext))], bytes).into_response(),
            Err(err) => {
                tracing::error!(%err, path = %path.display(), "an uploaded image is missing");
                return web::not_found(&state, &viewer).await;
            }
        }
    };
    let response_headers = response.headers_mut();
    response_headers.insert(CACHE_CONTROL, HeaderValue::from_static(cache_control));
    response_headers.insert(
        ETAG,
        HeaderValue::from_str(&etag).expect("hex is valid header text"),
    );
    response_headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response.extensions_mut().insert(OwnCacheControl);
    Ok(response)
}
