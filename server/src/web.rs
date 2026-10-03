//! Things every page handler needs: the shared layout, responses for common
//! outcomes (not found, forbidden, rate-limited, redirects), and setting the
//! session cookie.

use askama::Template;
use axum::http::header::{LOCATION, REFERRER_POLICY, SET_COOKIE};
use axum::http::{HeaderValue, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Redirect, Response};

use crate::AppState;
use crate::content::Deps;
use crate::error::AppError;
use crate::http_cache::CacheTags;
use crate::sessions;
use crate::viewer::Viewer;

/// Which item in the top navigation is the current page.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Index,
    Latest,
    Spaces,
    Members,
    Messages,
    Notifications,
    Invites,
    Account,
    Admin,
    Moderation,
    Login,
    Search,
    None,
}

/// The logged-in member shown in the navigation bar.
pub struct NavUser {
    pub username: String,
    pub is_admin: bool,
    /// Unread notifications the member may see, for the badge.
    pub notifications: i64,
    /// Conversations with messages they haven't read.
    pub messages: i64,
    /// For staff, how many things are waiting in the moderation queue.
    pub queue: Option<i64>,
}

/// What every page tells a silenced or suspended member.
pub struct RestrictionNotice {
    pub suspended: bool,
    pub until: chrono::DateTime<chrono::Utc>,
    /// The action to contest, if there is one.
    pub action_id: Option<i64>,
}

/// Fields every page needs for the shared layout in `base.html`.
pub struct Layout {
    pub site_name: String,
    pub nav: Nav,
    pub user: Option<NavUser>,
    pub restriction: Option<RestrictionNotice>,
    /// The islands' files, for logged-in members only (DESIGN.md §5:
    /// anonymous pages load no island JavaScript).
    pub islands: Option<crate::islands::IslandAssets>,
}

impl Layout {
    /// Everything the header needs, in one query: the site's name and, for
    /// a member, their counts and any restriction on them.
    pub async fn load(state: &AppState, viewer: &Viewer, nav: Nav) -> sqlx::Result<Layout> {
        let header = crate::db::header::load(state.db(), viewer).await?;
        let user = viewer.user().map(|user| NavUser {
            username: user.username.clone(),
            is_admin: viewer.is_admin(),
            notifications: header.notifications,
            messages: header.messages,
            queue: header.queue,
        });
        let restriction = match viewer.user() {
            Some(user) if user.is_silenced() => {
                let suspended = user.is_suspended();
                let until = if suspended {
                    user.suspended_until
                } else {
                    user.silenced_until
                };
                Some(RestrictionNotice {
                    suspended,
                    until: until.unwrap_or_default(),
                    action_id: header.restriction_action,
                })
            }
            _ => None,
        };
        let islands = match user {
            Some(_) => Some(state.islands()).filter(|islands| islands.entry.is_some()),
            None => None,
        };
        Ok(Layout {
            islands,
            site_name: header.site_name,
            nav,
            user,
            restriction,
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
    response
        .extensions_mut()
        .insert(CacheTags(deps.tags(), deps.expiry()));
    Ok(response)
}

/// A page whose URL holds a secret (a login, reset or verification
/// token): the browser is told not to send the URL to other sites.
///
/// Not `no-referrer`: with that policy browsers send `Origin: null` with
/// the page's own form, which the CSRF check rightly refuses.
pub fn secret_url(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(REFERRER_POLICY, HeaderValue::from_static("same-origin"));
    response
}

/// Every page has exactly one URL. A request for any other spelling of it
/// (unknown query parameters, `?page=1`, `?page=02`, `/t/049/…`,
/// percent-encoded letters) is redirected there before any rendering, so
/// such URLs never cost a render or take up room in the anonymous cache,
/// which is keyed by the raw path and query.
pub fn redirect_unless_canonical(uri: &Uri, canonical: &str) -> Option<Response> {
    let requested = uri.path_and_query().map_or(uri.path(), |pq| pq.as_str());
    (requested != canonical).then(|| Redirect::permanent(canonical).into_response())
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
