//! Things every page handler needs: the shared layout, responses for common
//! outcomes (not found, forbidden, rate-limited, redirects), and setting the
//! session cookie.

use askama::Template;
use axum::http::header::{LOCATION, REFERRER_POLICY, SET_COOKIE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};

use crate::AppState;
use crate::content::Deps;
use crate::error::AppError;
use crate::http_cache::CacheTags;
use crate::sessions;
use crate::settings;
use crate::viewer::Viewer;

/// Which item in the top navigation is the current page.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Index,
    Latest,
    Members,
    Invites,
    Account,
    Admin,
    Login,
    None,
}

/// The logged-in member shown in the navigation bar.
pub struct NavUser {
    pub username: String,
    pub is_admin: bool,
}

/// Fields every page needs for the shared layout in `base.html`.
pub struct Layout {
    pub site_name: String,
    pub nav: Nav,
    pub user: Option<NavUser>,
}

impl Layout {
    pub async fn load(state: &AppState, viewer: &Viewer, nav: Nav) -> sqlx::Result<Layout> {
        Ok(Layout {
            site_name: settings::site_name(state.db()).await?,
            nav,
            user: viewer.user().map(|user| NavUser {
                username: user.username.clone(),
                is_admin: viewer.is_admin(),
            }),
        })
    }
}

/// A page with a status code.
pub fn render(status: StatusCode, template: &impl Template) -> Result<Response, AppError> {
    Ok((status, Html(template.render()?)).into_response())
}

pub fn ok(template: &impl Template) -> Result<Response, AppError> {
    render(StatusCode::OK, template)
}

/// A rendered page that anonymous visitors may be served from the cache.
pub fn cacheable(template: &impl Template, deps: &Deps) -> Result<Response, AppError> {
    let mut response = Html(template.render()?).into_response();
    response.extensions_mut().insert(CacheTags(deps.tags()));
    Ok(response)
}

/// A page whose URL holds a secret (a login, reset or verification
/// token): the browser is told not to send the URL to anything the page
/// links to.
pub fn secret_url(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    response
}

/// A 303 redirect, for after a form has done its work.
pub fn see_other(location: &str) -> Response {
    let mut response = StatusCode::SEE_OTHER.into_response();
    if let Ok(value) = HeaderValue::from_str(location) {
        response.headers_mut().insert(LOCATION, value);
    }
    response
}

/// Sends a visitor who must log in first to the login page, to come back
/// to `back_to` afterwards.
pub fn login_first(back_to: &str) -> Response {
    see_other(&format!("/login?next={}", query_encode(back_to)))
}

/// Adds the cookie for a new session to a response.
pub fn start_session(mut response: Response, state: &AppState, token: &str) -> Response {
    response.headers_mut().append(
        SET_COOKIE,
        sessions::cookie(token, state.config().cookie_secure),
    );
    response
}

pub fn end_session(mut response: Response, state: &AppState) -> Response {
    response.headers_mut().append(
        SET_COOKIE,
        sessions::clear_cookie(state.config().cookie_secure),
    );
    response
}

#[derive(Template)]
#[template(path = "message.html")]
struct MessageTemplate<'a> {
    layout: Layout,
    title: &'a str,
    paragraphs: &'a [&'a str],
    link: Option<(&'a str, &'a str)>,
}

/// A page that just says something, with an optional link onwards.
pub async fn message(
    state: &AppState,
    viewer: &Viewer,
    status: StatusCode,
    title: &str,
    paragraphs: &[&str],
    link: Option<(&str, &str)>,
) -> Result<Response, AppError> {
    render(
        status,
        &MessageTemplate {
            layout: Layout::load(state, viewer, Nav::None).await?,
            title,
            paragraphs,
            link,
        },
    )
}

pub async fn rate_limited(state: &AppState, viewer: &Viewer) -> Result<Response, AppError> {
    message(
        state,
        viewer,
        StatusCode::TOO_MANY_REQUESTS,
        "Slow down",
        &["There have been too many attempts at this recently. Wait a few minutes and try again."],
        None,
    )
    .await
}

pub async fn forbidden(state: &AppState, viewer: &Viewer) -> Result<Response, AppError> {
    message(
        state,
        viewer,
        StatusCode::FORBIDDEN,
        "Not allowed",
        &["You don’t have permission to do that."],
        Some(("/", "Back to the forum index")),
    )
    .await
}

#[derive(Template)]
#[template(path = "not_found.html")]
struct NotFoundTemplate {
    layout: Layout,
}

pub async fn not_found(state: &AppState, viewer: &Viewer) -> Result<Response, AppError> {
    render(
        StatusCode::NOT_FOUND,
        &NotFoundTemplate {
            layout: Layout::load(state, viewer, Nav::None).await?,
        },
    )
}

/// The page to return to after logging in, if `next` is a path on this
/// site. Anything else (another site, `//host`, `/\host`, odd characters)
/// is ignored, so a link can't send someone elsewhere after they log in.
pub fn safe_next(next: &str) -> Option<&str> {
    let ok = next.starts_with('/')
        && !next.starts_with("//")
        && next.len() <= 1000
        && next.bytes().all(|b| b.is_ascii_graphic() && b != b'\\');
    ok.then_some(next)
}

/// Percent-encodes text for a query-string value.
pub fn query_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;

    #[test]
    fn next_must_be_a_local_path() {
        assert_eq!(safe_next("/invites"), Some("/invites"));
        assert_eq!(safe_next("/t/1/x?page=2#p5"), Some("/t/1/x?page=2#p5"));
        for bad in [
            "",
            "invites",
            "//evil.com",
            "/\\evil.com",
            "https://evil.com",
            "/a b",
            "/\tx",
            "/é",
        ] {
            assert_eq!(safe_next(bad), None, "{bad:?}");
        }
    }

    #[hegel::test(test_cases = 500)]
    fn next_never_leaves_the_site(tc: TestCase) {
        let next = tc.draw(gs::text().max_size(40));
        if let Some(next) = safe_next(&next) {
            let base = url::Url::parse("https://forum.example.com/login").unwrap();
            let target = base
                .join(next)
                .expect("a safe path is a valid relative URL");
            assert_eq!(target.host_str(), Some("forum.example.com"), "{next:?}");
            assert_eq!(target.scheme(), "https");
        }
    }

    #[hegel::test(test_cases = 500)]
    fn query_encoding_round_trips(tc: TestCase) {
        let text = tc.draw(gs::text().max_size(40));
        let encoded = query_encode(&text);
        assert!(!encoded.contains(['&', '=', '#', '?', '+', ' ']));
        let decoded: Vec<(String, String)> =
            url::form_urlencoded::parse(format!("next={encoded}").as_bytes())
                .into_owned()
                .collect();
        assert_eq!(decoded, vec![("next".to_owned(), text)]);
    }
}
