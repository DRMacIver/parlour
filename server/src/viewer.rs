//! Who is looking at a page (DESIGN.md §6). Every content query takes a
//! `&Viewer` and filters with the shared SQL functions `thread_readable`
//! and `category_readable`, which take the parameters [`Viewer::filter`]
//! produces.
//!
//! The session middleware ([`crate::sessions::middleware`]) works out the
//! viewer from the session cookie and leaves it in the request's
//! extensions, where the [`Viewer`] extractor finds it.
//!
//! Logged-in members can read public and members-only categories; staff can
//! read every category (DESIGN.md §3.1). Group membership (M5) adds the
//! group-restricted categories a member can read to [`ReadFilter`] without
//! changing the queries' shape.

use std::convert::Infallible;

use axum::extract::FromRequestParts;
use axum::http::HeaderMap;
use axum::http::header::COOKIE;
use axum::http::request::Parts;
use chrono::{DateTime, Utc};

use crate::accounts::Role;

/// The name of the session cookie. A request carrying it is never answered
/// from the anonymous cache.
pub const SESSION_COOKIE: &str = "parlour_session";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Viewer {
    Anonymous,
    User(UserCtx),
}

/// A logged-in member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserCtx {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub email: String,
    pub email_verified: bool,
    pub role: Role,
    pub created_at: DateTime<Utc>,
    /// The groups this member belongs to. Groups arrive in M5; until then
    /// this is always empty.
    pub group_ids: Vec<i64>,
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
        let levels: &[&str] = match self {
            Viewer::Anonymous => &["public"],
            Viewer::User(user) if user.role.is_staff() => &["public", "members", "groups"],
            Viewer::User(_) => &["public", "members"],
        };
        ReadFilter {
            visibilities: levels.iter().map(|level| (*level).to_owned()).collect(),
            group_category_ids: Vec::new(),
        }
    }

    pub fn user(&self) -> Option<&UserCtx> {
        match self {
            Viewer::Anonymous => None,
            Viewer::User(user) => Some(user),
        }
    }

    pub fn user_id(&self) -> Option<i64> {
        self.user().map(|user| user.id)
    }

    pub fn is_staff(&self) -> bool {
        self.user().is_some_and(|user| user.role.is_staff())
    }

    pub fn is_admin(&self) -> bool {
        self.user().is_some_and(|user| user.role == Role::Admin)
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Viewer {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Viewer, Infallible> {
        Ok(parts
            .extensions
            .get::<Viewer>()
            .cloned()
            .unwrap_or(Viewer::Anonymous))
    }
}

/// Whether the request carries a session cookie, valid or not.
pub fn has_session_cookie(headers: &HeaderMap) -> bool {
    session_cookie(headers).is_some()
}

/// The value of the session cookie, if the request has one.
pub fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            (name.trim() == SESSION_COOKIE).then(|| value.trim())
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

    fn user(role: Role) -> Viewer {
        Viewer::User(UserCtx {
            id: 1,
            username: "ruth".into(),
            display_name: "Ruth".into(),
            email: "ruth@example.com".into(),
            email_verified: true,
            role,
            created_at: Utc::now(),
            group_ids: Vec::new(),
        })
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
        assert_eq!(
            session_cookie(&headers(&["a=b; parlour_session=tok ; c=d"])),
            Some("tok")
        );
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

    #[test]
    fn members_read_members_only_content_and_staff_read_everything() {
        assert_eq!(
            user(Role::User).filter().visibilities,
            vec!["public", "members"]
        );
        for role in [Role::Moderator, Role::Admin] {
            assert_eq!(
                user(role).filter().visibilities,
                vec!["public", "members", "groups"]
            );
        }
        assert!(!user(Role::User).is_staff());
        assert!(user(Role::Moderator).is_staff());
        assert!(!user(Role::Moderator).is_admin());
        assert!(user(Role::Admin).is_admin());
    }
}
