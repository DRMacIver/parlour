//! Cross-site request forgery protection (DESIGN.md §5, §7).
//!
//! The session cookie is `SameSite=Lax`, so browsers don't send it with
//! cross-site form posts. As a second, independent check, every request
//! that can change something (anything but GET, HEAD and OPTIONS) must come
//! from one of this site's own pages: its `Origin` header, or failing that
//! its `Referer`, must name this site. Requests with neither are refused.
//!
//! There are no CSRF tokens, so the login and invite forms carry nothing
//! per-visitor and can be served from the anonymous cache.

use axum::extract::{Request, State};
use axum::http::header::{HOST, ORIGIN, REFERER};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::AppState;

pub async fn middleware(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if safe_method(request.method()) || same_origin(request.headers(), &state.config().base_url) {
        next.run(request).await
    } else {
        (
            StatusCode::FORBIDDEN,
            "This form was sent from another site, so it was ignored. \
             If you sent it from this site, go back, reload the page and try again.",
        )
            .into_response()
    }
}

fn safe_method(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// Whether a request's `Origin` (or `Referer`) is this site: either the
/// configured base URL, or the host the request was sent to.
pub fn same_origin(headers: &HeaderMap, base_url: &str) -> bool {
    let host = headers.get(HOST).and_then(|v| v.to_str().ok());
    let allowed = |origin: &str| {
        origin.eq_ignore_ascii_case(base_url)
            || host.is_some_and(|host| {
                origin
                    .strip_prefix("https://")
                    .or_else(|| origin.strip_prefix("http://"))
                    .is_some_and(|rest| rest.eq_ignore_ascii_case(host))
            })
    };
    if let Some(origin) = headers.get(ORIGIN) {
        // "null" (from sandboxed frames, file: pages and some redirects)
        // never matches.
        return origin.to_str().is_ok_and(allowed);
    }
    match headers.get(REFERER).and_then(|v| v.to_str().ok()) {
        Some(referer) => referer_origin(referer).is_some_and(allowed),
        None => false,
    }
}

/// The origin part of a URL: `https://example.com:8080` from
/// `https://example.com:8080/some/page?x`.
fn referer_origin(url: &str) -> Option<&str> {
    let after_scheme = url.find("://")? + 3;
    let end = url[after_scheme..]
        .find(['/', '?', '#'])
        .map_or(url.len(), |i| after_scheme + i);
    Some(&url[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use hegel::TestCase;
    use hegel::generators as gs;

    const BASE: &str = "https://forum.example.com";

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn accepts_this_sites_origin() {
        assert!(same_origin(&headers(&[("origin", BASE)]), BASE));
        assert!(same_origin(
            &headers(&[
                ("origin", "http://localhost:8080"),
                ("host", "localhost:8080")
            ]),
            BASE
        ));
        assert!(same_origin(
            &headers(&[("referer", "https://forum.example.com/login?next=/")]),
            BASE
        ));
    }

    #[test]
    fn refuses_other_origins() {
        for origin in [
            "https://evil.example.com",
            "https://forum.example.com.evil.com",
            "https://forum.example.com:444",
            "null",
            "",
        ] {
            assert!(
                !same_origin(&headers(&[("origin", origin)]), BASE),
                "{origin}"
            );
        }
        assert!(!same_origin(&headers(&[]), BASE));
        assert!(!same_origin(
            &headers(&[("referer", "https://forum.example.com.evil.com/")]),
            BASE
        ));
        assert!(!same_origin(
            &headers(&[("referer", "https://evil.com/https://forum.example.com")]),
            BASE
        ));
        // An Origin header wins over a Referer that would have matched.
        assert!(!same_origin(
            &headers(&[("origin", "https://evil.com"), ("referer", BASE)]),
            BASE
        ));
    }

    #[hegel::test(test_cases = 300)]
    fn only_this_site_is_accepted(tc: TestCase) {
        let host = tc.draw(gs::domains());
        let other = tc.draw(gs::domains());
        let path = tc.draw(gs::from_regex("(/[a-z?=#.:/]*)?").fullmatch(true));
        let scheme = tc.draw(gs::sampled_from(vec!["http", "https"]));
        let base = format!("https://{host}");
        let use_referer = tc.draw(gs::booleans());
        let check = |origin_host: &str| {
            let origin = format!("{scheme}://{origin_host}");
            let h = if use_referer {
                headers(&[("referer", &format!("{origin}{path}")), ("host", &host)])
            } else {
                headers(&[("origin", &origin), ("host", &host)])
            };
            same_origin(&h, &base)
        };
        assert!(check(&host));
        assert_eq!(check(&other), other.eq_ignore_ascii_case(&host));
    }
}
