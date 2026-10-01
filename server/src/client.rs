//! The client's network address, for rate limits and the session record.

use std::convert::Infallible;
use std::net::SocketAddr;

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::HeaderMap;
use axum::http::request::Parts;

use crate::AppState;

/// The address a request came from: the peer address, or with
/// `PARLOUR_TRUST_PROXY` the address the reverse proxy saw, which it
/// appends to `X-Forwarded-For`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientIp(pub String);

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<ClientIp, Infallible> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(addr)| *addr);
        Ok(ClientIp(client_ip(
            state.config().trust_proxy,
            &parts.headers,
            peer,
        )))
    }
}

pub fn client_ip(trust_proxy: bool, headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    if trust_proxy {
        let forwarded = headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .rfind(|v| !v.is_empty());
        if let Some(ip) = forwarded {
            return ip.chars().take(64).collect();
        }
    }
    match peer {
        Some(addr) => addr.ip().to_string(),
        None => "unknown".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn uses_the_proxys_record_only_when_trusted() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("6.6.6.6, 203.0.113.9"),
        );
        let peer: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        assert_eq!(client_ip(true, &headers, Some(peer)), "203.0.113.9");
        assert_eq!(client_ip(false, &headers, Some(peer)), "127.0.0.1");
        assert_eq!(client_ip(true, &HeaderMap::new(), Some(peer)), "127.0.0.1");
        assert_eq!(client_ip(false, &HeaderMap::new(), None), "unknown");
    }
}
