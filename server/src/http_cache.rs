//! The HTTP side of the anonymous page cache (DESIGN.md §5).
//!
//! A `GET` or `HEAD` request without a session cookie is answered from
//! [`PageCache`](crate::cache::PageCache) when possible. On a miss the page
//! is rendered as `Viewer::Anonymous`; if the handler marked the response
//! as cacheable by attaching [`CacheTags`], it is stored under its path and
//! query string.
//!
//! Cacheable responses carry `Cache-Control: public, …` and a strong `ETag`,
//! and conditional requests are answered with `304 Not Modified`. Everything
//! else gets `Cache-Control: private, no-store`.

use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, ETAG, IF_NONE_MATCH, SET_COOKIE,
};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use std::time::Instant;

use crate::AppState;
use crate::cache::{CachedPage, Tag};
use crate::viewer::has_session_cookie;

pub const PUBLIC_CACHE_CONTROL: &str = "public, max-age=0, s-maxage=30, stale-while-revalidate=60";
pub const PRIVATE_CACHE_CONTROL: &str = "private, no-store";

/// Attached by a handler to a response that may be cached for anonymous
/// visitors, listing what it was rendered from and, if it stops being
/// right by itself at some point, when.
#[derive(Clone, Debug)]
pub struct CacheTags(pub Vec<Tag>, pub Option<DateTime<Utc>>);

pub async fn middleware(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let cacheable_request =
        (method == Method::GET || method == Method::HEAD) && !has_session_cookie(request.headers());
    if !cacheable_request {
        return private(next.run(request).await);
    }

    let key = request.uri().path_and_query().map_or_else(
        || request.uri().path().to_owned(),
        |pq| pq.as_str().to_owned(),
    );
    let if_none_match = request.headers().get(IF_NONE_MATCH).cloned();
    let head = method == Method::HEAD;

    if let Some(page) = state.cache.get(&key) {
        return respond(&page, if_none_match.as_ref(), head);
    }

    let ticket = state.cache.ticket();
    let response = next.run(request).await;
    let (tags, expires) = match response.extensions().get::<CacheTags>() {
        Some(CacheTags(tags, expires_at))
            if response.status() == StatusCode::OK
                && !response.headers().contains_key(SET_COOKIE) =>
        {
            let expires = match expires_at {
                None => None,
                Some(at) => match (*at - Utc::now()).to_std() {
                    Ok(left) => Some(Instant::now() + left),
                    // Already out of date: don't keep it.
                    Err(_) => return private(response),
                },
            };
            (tags.clone(), expires)
        }
        _ => return private(response),
    };
    let (parts, body) = response.into_parts();
    let content_type = parts
        .headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("text/html; charset=utf-8")
        .to_owned();
    let body = match to_bytes(body, usize::MAX).await {
        Ok(body) => body,
        Err(err) => {
            tracing::error!(%err, "failed to read a rendered page");
            return private(StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
    };
    let page = CachedPage::new(content_type, body);
    let response = respond(&page, if_none_match.as_ref(), head);
    if !head {
        state.cache.insert_until(ticket, key, page, &tags, expires);
    }
    response
}

fn private(mut response: Response) -> Response {
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static(PRIVATE_CACHE_CONTROL),
    );
    response.headers_mut().remove(ETAG);
    response
}

fn respond(page: &CachedPage, if_none_match: Option<&HeaderValue>, head: bool) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static(PUBLIC_CACHE_CONTROL),
    );
    if let Ok(etag) = HeaderValue::from_str(&page.etag) {
        headers.insert(ETAG, etag);
    }
    if if_none_match.is_some_and(|value| etag_matches(value, &page.etag)) {
        return (StatusCode::NOT_MODIFIED, headers).into_response();
    }
    if let Ok(content_type) = HeaderValue::from_str(&page.content_type) {
        headers.insert(CONTENT_TYPE, content_type);
    }
    // A HEAD response says how long the GET response would be
    // (RFC 9110 §9.3.2), rather than the length of its own empty body.
    headers.insert(CONTENT_LENGTH, HeaderValue::from(page.body.len()));
    let body = if head {
        Body::empty()
    } else {
        Body::from(page.body.clone())
    };
    (StatusCode::OK, headers, body).into_response()
}

/// Whether an `If-None-Match` header matches `etag`, using the weak
/// comparison RFC 9110 §13.1.2 specifies for it.
pub fn etag_matches(if_none_match: &HeaderValue, etag: &str) -> bool {
    let Ok(value) = if_none_match.to_str() else {
        return false;
    };
    let etag = etag.trim_start_matches("W/");
    value.trim() == "*"
        || value
            .split(',')
            .any(|candidate| candidate.trim().trim_start_matches("W/") == etag)
}

#[cfg(test)]
mod tests {
    use super::etag_matches;
    use axum::http::HeaderValue;

    #[test]
    fn if_none_match() {
        let etag = "\"abc\"";
        for (header, matches) in [
            ("\"abc\"", true),
            ("W/\"abc\"", true),
            ("\"x\", \"abc\"", true),
            ("*", true),
            ("\"abd\"", false),
            ("abc", false),
            ("", false),
        ] {
            assert_eq!(
                etag_matches(&HeaderValue::from_static(header), etag),
                matches,
                "{header}"
            );
        }
    }
}
