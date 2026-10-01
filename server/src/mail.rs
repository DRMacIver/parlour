//! Outgoing email (DESIGN.md §9). Messages are queued as `email` jobs
//! (see [`crate::jobs`]) in the same transaction as whatever they're about,
//! and sent over SMTP with lettre by a job worker, which retries if the
//! server is down.
//!
//! The SMTP settings come from the admin settings if they have been saved
//! there (by `/setup` or `/admin/settings`), otherwise from the environment.

use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use lettre::message::Mailbox;
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool};

use crate::{AppState, jobs};

pub const JOB_KIND: &str = "email";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SmtpSecurity {
    /// Plain SMTP, for a relay on the same machine or Mailpit in development.
    None,
    /// Connect in plain text, then upgrade with STARTTLS (port 587).
    StartTls,
    /// TLS from the start (port 465).
    Tls,
}

impl SmtpSecurity {
    pub fn default_port(self) -> u16 {
        match self {
            SmtpSecurity::None => 25,
            SmtpSecurity::StartTls => 587,
            SmtpSecurity::Tls => 465,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SmtpSecurity::None => "none",
            SmtpSecurity::StartTls => "starttls",
            SmtpSecurity::Tls => "tls",
        }
    }
}

impl FromStr for SmtpSecurity {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "none" => Ok(SmtpSecurity::None),
            "starttls" => Ok(SmtpSecurity::StartTls),
            "tls" => Ok(SmtpSecurity::Tls),
            _ => Err(format!("expected none, starttls or tls, not {s:?}")),
        }
    }
}

impl fmt::Display for SmtpSecurity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmtpSettings {
    pub host: String,
    pub port: u16,
    pub security: SmtpSecurity,
    pub username: String,
    pub password: String,
    /// The From address, such as `The Tuesday Club <forum@example.com>`.
    pub from: String,
}

impl fmt::Debug for SmtpSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SmtpSettings")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("security", &self.security)
            .field("username", &self.username)
            .field("password", &"…")
            .field("from", &self.from)
            .finish()
    }
}

impl SmtpSettings {
    /// Checks the settings well enough to catch typing mistakes.
    pub fn validate(&self) -> Result<(), String> {
        if self.host.trim().is_empty() {
            return Err("Enter the SMTP server's host name.".into());
        }
        if self.port == 0 {
            return Err("Enter the SMTP server's port.".into());
        }
        self.from.parse::<Mailbox>().map_err(|_| {
            "The From address should look like Name <forum@example.com>.".to_owned()
        })?;
        Ok(())
    }
}

/// One plain-text email.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Email {
    pub to: String,
    pub subject: String,
    pub body: String,
}

/// Where email goes: an SMTP server, or (in tests) a list in memory.
#[derive(Clone, Default)]
pub enum Mailer {
    #[default]
    Smtp,
    Capture(Arc<Mutex<Vec<Email>>>),
}

impl Mailer {
    pub fn capture() -> Mailer {
        Mailer::Capture(Arc::default())
    }

    /// Everything sent so far, if capturing.
    pub fn sent(&self) -> Vec<Email> {
        match self {
            Mailer::Smtp => Vec::new(),
            Mailer::Capture(sent) => sent.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        }
    }
}

/// Queues an email to be sent by a job worker. Call
/// [`crate::jobs::Jobs::wake`] after committing.
pub async fn queue(db: &mut PgConnection, email: &Email) -> sqlx::Result<()> {
    let payload = serde_json::to_value(email).expect("emails serialise");
    jobs::enqueue(db, JOB_KIND, &payload).await?;
    Ok(())
}

/// The job handler for queued emails.
pub async fn run_job(state: AppState, payload: serde_json::Value) -> anyhow::Result<()> {
    let email: Email = serde_json::from_value(payload).context("malformed email job")?;
    send_now(&state, &email).await
}

/// The SMTP settings in effect: the admin's, or the environment's.
pub async fn smtp_settings(state: &AppState) -> sqlx::Result<Option<SmtpSettings>> {
    Ok(saved_smtp_settings(state.db())
        .await?
        .or_else(|| state.config().smtp.clone()))
}

pub async fn saved_smtp_settings(db: &PgPool) -> sqlx::Result<Option<SmtpSettings>> {
    let value: Option<serde_json::Value> =
        sqlx::query_scalar!("SELECT value FROM settings WHERE key = 'smtp'")
            .fetch_optional(db)
            .await?;
    Ok(value.and_then(|v| serde_json::from_value(v).ok()))
}

/// Sends an email straight away with the settings in effect.
pub async fn send_now(state: &AppState, email: &Email) -> anyhow::Result<()> {
    match state.mailer() {
        Mailer::Capture(sent) => {
            sent.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(email.clone());
            Ok(())
        }
        Mailer::Smtp => {
            let settings = smtp_settings(state)
                .await?
                .context("no SMTP server is configured; set one up in the admin settings")?;
            send_with(&settings, email).await
        }
    }
}

/// Sends an email over SMTP with the given settings (also used to test
/// settings before saving them).
pub async fn send_with(settings: &SmtpSettings, email: &Email) -> anyhow::Result<()> {
    let message = Message::builder()
        .from(settings.from.parse().context("invalid From address")?)
        .to(email.to.parse().context("invalid recipient address")?)
        .subject(&email.subject)
        .header(ContentType::TEXT_PLAIN)
        .body(email.body.clone())
        .context("could not build the message")?;
    let builder = match settings.security {
        SmtpSecurity::None => {
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&settings.host)
        }
        SmtpSecurity::StartTls => {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&settings.host)?
        }
        SmtpSecurity::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(&settings.host)?,
    };
    let mut builder = builder
        .port(settings.port)
        .timeout(Some(Duration::from_secs(30)));
    if !settings.username.is_empty() {
        builder = builder.credentials(Credentials::new(
            settings.username.clone(),
            settings.password.clone(),
        ));
    }
    builder.build().send(message).await.with_context(|| {
        format!(
            "SMTP server {}:{} refused the message",
            settings.host, settings.port
        )
    })?;
    Ok(())
}

/// Whether `address` looks like an email address we can send to.
pub fn valid_address(address: &str) -> bool {
    address.len() <= 254
        && !address.contains(['<', '>', ' ', ',', '"'])
        && address.parse::<lettre::Address>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security_round_trips() {
        for security in [
            SmtpSecurity::None,
            SmtpSecurity::StartTls,
            SmtpSecurity::Tls,
        ] {
            assert_eq!(security.as_str().parse::<SmtpSecurity>(), Ok(security));
        }
        assert!("ssl".parse::<SmtpSecurity>().is_err());
    }

    #[test]
    fn debug_hides_the_password() {
        let settings = SmtpSettings {
            host: "smtp.example.com".into(),
            port: 587,
            security: SmtpSecurity::StartTls,
            username: "forum".into(),
            password: "hunter2".into(),
            from: "forum@example.com".into(),
        };
        assert!(!format!("{settings:?}").contains("hunter2"));
        assert!(settings.validate().is_ok());
        assert!(
            SmtpSettings {
                from: "not an address".into(),
                ..settings.clone()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn addresses() {
        assert!(valid_address("ruth@example.com"));
        assert!(!valid_address("ruth"));
        assert!(!valid_address("Ruth <ruth@example.com>"));
        assert!(!valid_address("a@b.com, c@d.com"));
        assert!(!valid_address(""));
    }
}
