//! `/admin/settings`: the site name, who can join and invite, and the SMTP
//! server, with a test send. Admins only.

use askama::Template;
use axum::Form;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;

use crate::AppState;
use crate::changes::Change;
use crate::emails;
use crate::error::AppError;
use crate::mail::{self, SmtpSecurity, SmtpSettings};
use crate::settings::{self, InvitePolicy, RegistrationMode};
use crate::viewer::{UserCtx, Viewer};
use crate::web::{self, Layout, Nav, render, see_other};

/// The SMTP part of a settings form, as typed.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct SmtpFields {
    pub smtp_host: String,
    pub smtp_port: String,
    pub smtp_security: String,
    pub smtp_username: String,
    /// Blank keeps the saved password.
    pub smtp_password: String,
    pub smtp_from: String,
}

impl SmtpFields {
    pub fn from_settings(settings: Option<&SmtpSettings>) -> SmtpFields {
        match settings {
            None => SmtpFields {
                smtp_security: SmtpSecurity::StartTls.as_str().to_owned(),
                ..SmtpFields::default()
            },
            Some(s) => SmtpFields {
                smtp_host: s.host.clone(),
                smtp_port: s.port.to_string(),
                smtp_security: s.security.as_str().to_owned(),
                smtp_username: s.username.clone(),
                smtp_password: String::new(),
                smtp_from: s.from.clone(),
            },
        }
    }

    /// The settings these fields describe: `Ok(None)` if the host is blank
    /// (no SMTP server saved). `saved_password` fills in a blank password.
    pub fn parse(&self, saved_password: Option<&str>) -> Result<Option<SmtpSettings>, Vec<String>> {
        let host = self.smtp_host.trim();
        if host.is_empty() {
            return Ok(None);
        }
        let mut problems = Vec::new();
        let security = self
            .smtp_security
            .parse::<SmtpSecurity>()
            .unwrap_or(SmtpSecurity::StartTls);
        let port = match self.smtp_port.trim() {
            "" => security.default_port(),
            port => match port.parse::<u16>() {
                Ok(port) if port > 0 => port,
                _ => {
                    problems.push("The SMTP port should be a number such as 587.".to_owned());
                    0
                }
            },
        };
        let password = if self.smtp_password.is_empty() {
            saved_password.unwrap_or_default().to_owned()
        } else {
            self.smtp_password.clone()
        };
        let settings = SmtpSettings {
            host: host.to_owned(),
            port,
            security,
            username: self.smtp_username.trim().to_owned(),
            password,
            from: self.smtp_from.trim().to_owned(),
        };
        if problems.is_empty()
            && let Err(why) = settings.validate()
        {
            problems.push(why);
        }
        if problems.is_empty() {
            Ok(Some(settings))
        } else {
            Err(problems)
        }
    }
}

/// Saves SMTP settings, or forgets them (falling back to the environment's).
pub async fn save_smtp(
    db: &mut sqlx::PgConnection,
    smtp: Option<&SmtpSettings>,
) -> sqlx::Result<()> {
    match smtp {
        Some(smtp) => settings::set(db, "smtp", smtp).await,
        None => {
            sqlx::query!("DELETE FROM settings WHERE key = 'smtp'")
                .execute(db)
                .await?;
            Ok(())
        }
    }
}

/// Sends a test email to `to` and says how it went.
pub async fn send_test(
    state: &AppState,
    smtp: Option<&SmtpSettings>,
    to: &str,
) -> Result<(), String> {
    let site_name = settings::site_name(state.db())
        .await
        .map_err(|err| err.to_string())?;
    let email = emails::test(to, &site_name);
    let result = match (state.mailer(), smtp) {
        (mail::Mailer::Smtp, Some(smtp)) => mail::send_with(smtp, &email).await,
        _ => mail::send_now(state, &email).await,
    };
    result.map_err(|err| format!("{err:#}"))
}

#[derive(Template)]
#[template(path = "admin_settings.html")]
struct AdminSettingsTemplate {
    layout: Layout,
    notice: Option<String>,
    errors: Vec<String>,
    site_name: String,
    mode: RegistrationMode,
    invite_who: String,
    invite_days: u32,
    smtp: SmtpFields,
    smtp_has_password: bool,
    smtp_from_env: bool,
    admin_email: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct DoneQuery {
    done: String,
}

fn admin(viewer: &Viewer) -> Option<&UserCtx> {
    viewer.user().filter(|_| viewer.is_admin())
}

async fn render_settings(
    state: &AppState,
    viewer: &Viewer,
    user: &UserCtx,
    status: StatusCode,
    notice: Option<String>,
    errors: Vec<String>,
    form: Option<&SettingsForm>,
) -> Result<Response, AppError> {
    let db = state.db();
    let saved_smtp = mail::saved_smtp_settings(db).await?;
    let policy = settings::invite_policy(db).await?;
    let (invite_who, invite_days) = match policy {
        InvitePolicy::Everyone => ("everyone", 7),
        InvitePolicy::MembersOlderThan { days } => ("members_older_than", days),
        InvitePolicy::Staff => ("staff", 7),
    };
    let template = match form {
        Some(form) => AdminSettingsTemplate {
            layout: Layout::load(state, viewer, Nav::Admin).await?,
            notice,
            errors,
            site_name: form.site_name.clone(),
            mode: RegistrationMode::parse(&form.registration_mode).unwrap_or_default(),
            invite_who: form.invite_who.clone(),
            invite_days: form.invite_days.trim().parse().unwrap_or(invite_days),
            smtp: SmtpFields {
                smtp_password: String::new(),
                ..form.smtp.clone()
            },
            smtp_has_password: saved_smtp.as_ref().is_some_and(|s| !s.password.is_empty()),
            smtp_from_env: saved_smtp.is_none() && state.config().smtp.is_some(),
            admin_email: user.email.clone(),
        },
        None => AdminSettingsTemplate {
            layout: Layout::load(state, viewer, Nav::Admin).await?,
            notice,
            errors,
            site_name: settings::site_name(db).await?,
            mode: settings::registration_mode(db).await?,
            invite_who: invite_who.to_owned(),
            invite_days,
            smtp: SmtpFields::from_settings(saved_smtp.as_ref()),
            smtp_has_password: saved_smtp.as_ref().is_some_and(|s| !s.password.is_empty()),
            smtp_from_env: saved_smtp.is_none() && state.config().smtp.is_some(),
            admin_email: user.email.clone(),
        },
    };
    render(status, &template)
}

pub async fn settings_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Query(query): Query<DoneQuery>,
) -> Result<Response, AppError> {
    let Some(user) = admin(&viewer) else {
        return match viewer.user() {
            None => Ok(web::login_first("/admin/settings")),
            Some(_) => web::forbidden(&state, &viewer).await,
        };
    };
    let notice = match query.done.as_str() {
        "saved" => Some("Settings saved.".to_owned()),
        "test" => Some(format!("A test email is on its way to {}.", user.email)),
        _ => None,
    };
    render_settings(
        &state,
        &viewer,
        user,
        StatusCode::OK,
        notice,
        Vec::new(),
        None,
    )
    .await
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct SettingsForm {
    site_name: String,
    registration_mode: String,
    invite_who: String,
    invite_days: String,
    #[serde(flatten)]
    smtp: SmtpFields,
}

pub async fn save_settings(
    State(state): State<AppState>,
    viewer: Viewer,
    Form(form): Form<SettingsForm>,
) -> Result<Response, AppError> {
    let Some(user) = admin(&viewer) else {
        return match viewer.user() {
            None => Ok(web::login_first("/admin/settings")),
            Some(_) => web::forbidden(&state, &viewer).await,
        };
    };
    let mut problems = Vec::new();
    let site_name = form
        .site_name
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if site_name.is_empty() || site_name.chars().count() > 80 {
        problems.push("The site name should be between 1 and 80 characters.".to_owned());
    }
    let mode = RegistrationMode::parse(&form.registration_mode);
    if mode.is_none() {
        problems.push("Choose who can join.".to_owned());
    }
    let policy = match form.invite_who.as_str() {
        "everyone" => Some(InvitePolicy::Everyone),
        "staff" => Some(InvitePolicy::Staff),
        "members_older_than" => match form.invite_days.trim().parse::<u32>() {
            Ok(days) if days <= 3650 => Some(InvitePolicy::MembersOlderThan { days }),
            _ => {
                problems.push("Give the number of days as a whole number up to 3650.".to_owned());
                None
            }
        },
        _ => {
            problems.push("Choose who can invite people.".to_owned());
            None
        }
    };
    let saved = mail::saved_smtp_settings(state.db()).await?;
    let smtp = match form.smtp.parse(saved.as_ref().map(|s| s.password.as_str())) {
        Ok(smtp) => smtp,
        Err(more) => {
            problems.extend(more);
            None
        }
    };
    let (Some(mode), Some(policy)) = (mode, policy) else {
        return render_settings(
            &state,
            &viewer,
            user,
            StatusCode::BAD_REQUEST,
            None,
            problems,
            Some(&form),
        )
        .await;
    };
    if !problems.is_empty() {
        return render_settings(
            &state,
            &viewer,
            user,
            StatusCode::BAD_REQUEST,
            None,
            problems,
            Some(&form),
        )
        .await;
    }
    let mut tx = state.db().begin().await?;
    settings::set(&mut tx, "site_name", &site_name).await?;
    settings::set(&mut tx, "registration_mode", &mode).await?;
    settings::set(&mut tx, "invite_policy", &policy).await?;
    save_smtp(&mut tx, smtp.as_ref()).await?;
    tx.commit().await?;
    state.changed(Change::SiteChanged).await?;
    Ok(see_other("/admin/settings?done=saved"))
}

pub async fn test_email(
    State(state): State<AppState>,
    viewer: Viewer,
) -> Result<Response, AppError> {
    let Some(user) = admin(&viewer) else {
        return match viewer.user() {
            None => Ok(web::login_first("/admin/settings")),
            Some(_) => web::forbidden(&state, &viewer).await,
        };
    };
    let smtp = mail::smtp_settings(&state).await?;
    match send_test(&state, smtp.as_ref(), &user.email).await {
        Ok(()) => Ok(see_other("/admin/settings?done=test")),
        Err(why) => {
            render_settings(
                &state,
                &viewer,
                user,
                StatusCode::BAD_GATEWAY,
                None,
                vec![format!("The test email couldn’t be sent: {why}")],
                None,
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smtp_fields_parse() {
        let fields = SmtpFields {
            smtp_host: " smtp.example.com ".into(),
            smtp_port: "".into(),
            smtp_security: "tls".into(),
            smtp_username: "forum".into(),
            smtp_password: "".into(),
            smtp_from: "Club <forum@example.com>".into(),
        };
        let parsed = fields.parse(Some("saved")).unwrap().unwrap();
        assert_eq!(parsed.host, "smtp.example.com");
        assert_eq!(parsed.port, 465);
        assert_eq!(parsed.password, "saved");
        assert_eq!(
            SmtpFields {
                smtp_host: "".into(),
                ..fields.clone()
            }
            .parse(None),
            Ok(None)
        );
        assert!(
            SmtpFields {
                smtp_port: "seven".into(),
                ..fields.clone()
            }
            .parse(None)
            .is_err()
        );
        assert!(
            SmtpFields {
                smtp_from: "nobody".into(),
                ..fields
            }
            .parse(None)
            .is_err()
        );
    }
}
