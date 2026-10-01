//! Configuration from the environment (DESIGN.md §10). Everything else is
//! set in the admin interface and stored in the `settings` table.

use anyhow::{Context, bail};

use crate::mail::{SmtpSecurity, SmtpSettings};

#[derive(Clone, Debug)]
pub struct Config {
    /// The site's public origin, such as `https://forum.example.com`, with
    /// no trailing slash. Used for links in emails and to check the
    /// `Origin` of form submissions.
    pub base_url: String,
    /// Whether the session cookie is marked `Secure`. On by default when
    /// `base_url` is https; turn it off for local development over http.
    pub cookie_secure: bool,
    /// Whether to take the client's address from `X-Forwarded-For` (set
    /// this when running behind Caddy or another reverse proxy).
    pub trust_proxy: bool,
    /// SMTP settings from the environment. Settings saved in the admin
    /// interface take precedence.
    pub smtp: Option<SmtpSettings>,
}

impl Config {
    /// Settings for local development and tests: http://localhost:8080,
    /// no `Secure` cookies, no proxy, no SMTP server.
    pub fn development() -> Config {
        Config {
            base_url: "http://localhost:8080".to_owned(),
            cookie_secure: false,
            trust_proxy: false,
            smtp: None,
        }
    }

    /// Reads `PARLOUR_BASE_URL` (or `PARLOUR_DOMAIN`), `PARLOUR_COOKIE_SECURE`,
    /// `PARLOUR_TRUST_PROXY` and the `PARLOUR_SMTP_*` variables.
    pub fn from_env() -> anyhow::Result<Config> {
        Config::from_lookup(|name| std::env::var(name).ok().filter(|v| !v.is_empty()))
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Config> {
        let base_url = match (get("PARLOUR_BASE_URL"), get("PARLOUR_DOMAIN")) {
            (Some(url), _) => url,
            (None, Some(domain)) => format!("https://{domain}"),
            (None, None) => "http://localhost:8080".to_owned(),
        };
        let base_url = base_url.trim_end_matches('/').to_owned();
        let host = base_url
            .strip_prefix("https://")
            .or_else(|| base_url.strip_prefix("http://"));
        if !host.is_some_and(|host| !host.is_empty() && !host.contains(['/', '?', '#'])) {
            bail!(
                "PARLOUR_BASE_URL must be an origin like https://forum.example.com, not {base_url:?}"
            );
        }
        let cookie_secure = match get("PARLOUR_COOKIE_SECURE") {
            Some(value) => parse_bool(&value).context("PARLOUR_COOKIE_SECURE")?,
            None => base_url.starts_with("https://"),
        };
        let trust_proxy = match get("PARLOUR_TRUST_PROXY") {
            Some(value) => parse_bool(&value).context("PARLOUR_TRUST_PROXY")?,
            None => false,
        };
        let smtp = match get("PARLOUR_SMTP_HOST") {
            None => None,
            Some(host) => {
                let security = match get("PARLOUR_SMTP_SECURITY") {
                    Some(value) => value
                        .parse::<SmtpSecurity>()
                        .map_err(anyhow::Error::msg)
                        .context("PARLOUR_SMTP_SECURITY")?,
                    None => SmtpSecurity::StartTls,
                };
                let port = match get("PARLOUR_SMTP_PORT") {
                    Some(port) => port.parse().context("PARLOUR_SMTP_PORT must be a number")?,
                    None => security.default_port(),
                };
                Some(SmtpSettings {
                    host,
                    port,
                    security,
                    username: get("PARLOUR_SMTP_USERNAME").unwrap_or_default(),
                    password: get("PARLOUR_SMTP_PASSWORD").unwrap_or_default(),
                    from: get("PARLOUR_MAIL_FROM")
                        .context("PARLOUR_MAIL_FROM must be set when PARLOUR_SMTP_HOST is")?,
                })
            }
        };
        Ok(Config {
            base_url,
            cookie_secure,
            trust_proxy,
            smtp,
        })
    }

    /// An absolute URL for a path on this site.
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }
}

fn parse_bool(value: &str) -> anyhow::Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => bail!("expected true or false, not {value:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn config(vars: &[(&str, &str)]) -> anyhow::Result<Config> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(|name| vars.get(name).cloned())
    }

    #[test]
    fn defaults_suit_local_development() {
        let c = config(&[]).unwrap();
        assert_eq!(c.base_url, "http://localhost:8080");
        assert!(!c.cookie_secure);
        assert!(!c.trust_proxy);
        assert!(c.smtp.is_none());
    }

    #[test]
    fn a_domain_means_https_and_secure_cookies() {
        let c = config(&[("PARLOUR_DOMAIN", "forum.example.com")]).unwrap();
        assert_eq!(c.base_url, "https://forum.example.com");
        assert!(c.cookie_secure);
        assert_eq!(c.url("/login"), "https://forum.example.com/login");
    }

    #[test]
    fn secure_cookies_can_be_turned_off() {
        let c = config(&[
            ("PARLOUR_BASE_URL", "https://forum.example.com/"),
            ("PARLOUR_COOKIE_SECURE", "false"),
        ])
        .unwrap();
        assert_eq!(c.base_url, "https://forum.example.com");
        assert!(!c.cookie_secure);
    }

    #[test]
    fn base_url_must_be_an_origin() {
        assert!(config(&[("PARLOUR_BASE_URL", "forum.example.com")]).is_err());
        assert!(config(&[("PARLOUR_BASE_URL", "https://example.com/forum")]).is_err());
        assert!(config(&[("PARLOUR_BASE_URL", "http://localhost:3000")]).is_ok());
    }

    #[test]
    fn smtp_from_environment() {
        let c = config(&[
            ("PARLOUR_SMTP_HOST", "localhost"),
            ("PARLOUR_SMTP_SECURITY", "none"),
            ("PARLOUR_SMTP_PORT", "1025"),
            ("PARLOUR_MAIL_FROM", "Parlour <forum@example.com>"),
        ])
        .unwrap();
        let smtp = c.smtp.unwrap();
        assert_eq!(smtp.host, "localhost");
        assert_eq!(smtp.port, 1025);
        assert_eq!(smtp.security, SmtpSecurity::None);
        assert_eq!(smtp.from, "Parlour <forum@example.com>");
        assert!(config(&[("PARLOUR_SMTP_HOST", "localhost")]).is_err());
        let tls = config(&[
            ("PARLOUR_SMTP_HOST", "smtp.example.com"),
            ("PARLOUR_SMTP_SECURITY", "tls"),
            ("PARLOUR_MAIL_FROM", "forum@example.com"),
        ])
        .unwrap()
        .smtp
        .unwrap();
        assert_eq!(tls.port, 465);
    }
}
