//! Who is looking at a page (DESIGN.md §6). Every content query takes a
//! `&Viewer` and filters with the shared SQL functions `thread_readable`
//! and `category_readable`, which take the parameters [`Viewer::filter`]
//! produces.
//!
//! Until accounts exist (M3) every visitor is anonymous. Groups, members-only
//! and staff access (M5) extend [`Viewer`] and [`ReadFilter`] without changing
//! the queries' shape.

use std::convert::Infallible;

use axum::extract::FromRequestParts;
use axum::http::HeaderMap;
use axum::http::header::COOKIE;
use axum::http::request::Parts;

/// The name of the session cookie. A request carrying it is never answered
/// from the anonymous cache.
pub const SESSION_COOKIE: &str = "parlour_session";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Viewer {
    Anonymous,
}

/// The bind parameters for the visibility condition in SQL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadFilter {
    /// Visibility levels readable everywhere (`$vis` in `thread_readable`).
    pub visibilities: Vec<String>,
    /// Group-restricted categories this viewer may read.
    pub group_category_ids: Vec<i64>,
}

impl Viewer {
    pub fn filter(&self) -> ReadFilter {
        match self {
            Viewer::Anonymous => ReadFilter {
                visibilities: vec!["public".to_owned()],
                group_category_ids: Vec::new(),
            },
        }
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Viewer {
    type Rejection = Infallible;

    async fn from_request_parts(_parts: &mut Parts, _state: &S) -> Result<Viewer, Infallible> {
        Ok(Viewer::Anonymous)
    }
}

/// Whether the request carries a session cookie, valid or not.
pub fn has_session_cookie(headers: &HeaderMap) -> bool {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .any(|pair| {
            pair.trim()
                .split_once('=')
                .is_some_and(|(name, _)| name.trim() == SESSION_COOKIE)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(cookies: &[&str]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for cookie in cookies {
            map.append(COOKIE, HeaderValue::from_str(cookie).unwrap());
        }
        map
    }

    #[test]
    fn detects_session_cookie() {
        assert!(!has_session_cookie(&headers(&[])));
        assert!(has_session_cookie(&headers(&["parlour_session=abc"])));
        assert!(has_session_cookie(&headers(&[
            "theme=dark; parlour_session=abc"
        ])));
        assert!(has_session_cookie(&headers(&[
            "theme=dark",
            "parlour_session="
        ])));
        assert!(!has_session_cookie(&headers(&["parlour_session_old=abc"])));
        assert!(!has_session_cookie(&headers(&["theme=parlour_session"])));
    }

    #[test]
    fn anonymous_reads_only_public_content() {
        assert_eq!(
            Viewer::Anonymous.filter(),
            ReadFilter {
                visibilities: vec!["public".into()],
                group_category_ids: vec![]
            }
        );
    }
}
