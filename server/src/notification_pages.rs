//! Notifications and the settings around them (DESIGN.md §9):
//!
//! - `/notifications`: the member's notifications, newest first, each
//!   linking (through `/notifications/{id}`, which marks it read) to its
//!   post; "Mark all read".
//! - `POST /t/{id}/watch` and `POST /c/{slug}/watch`: watch, follow
//!   normally or mute a thread, conversation or space.
//! - `POST /settings/messages` and `POST /settings/notifications`: who may
//!   message the member, and which notifications they want emailed.
//! - `/unsubscribe/{token}`: the one-click unsubscribe link in notification
//!   emails (RFC 8058). The token, signed by the site, is all it needs, so
//!   it works without logging in, and mail providers may POST to it
//!   directly.
//! - `/admin/messages`: the site's rules for messages and email, with a
//!   test notification email.

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, Uri};
use axum::response::Response;
use serde::Deserialize;

use crate::AppState;
use crate::content::notifications::{Kind, NOTIFICATIONS_PER_PAGE, NotificationList};
use crate::content::{self, page_count};
use crate::emails;
use crate::error::AppError;
use crate::mail;
use crate::notifications as stored;
use crate::notify::{self, EmailItem};
use crate::pages::parse_id;
use crate::pagination::{PageItem, Pagination};
use crate::settings::{self, MessageSettings, NotificationSettings};
use crate::social::{self, DmPolicy, EmailPrefs, EmailTopic, WatchLevel, Watched};
use crate::viewer::Viewer;
use crate::web::{self, Layout, Nav, render, see_other};

type Fields = Vec<(String, String)>;

fn field<'a>(fields: &'a Fields, name: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

// ---------------------------------------------------------------------------
// The list

#[derive(Template)]
#[template(path = "notifications.html")]
struct NotificationsTemplate {
    layout: Layout,
    list: NotificationList,
    pagination: Pagination,
    unread: i64,
}

fn requested_page(uri: &Uri) -> Option<i64> {
    let Query(params) = Query::<Fields>::try_from_uri(uri).ok()?;
    match params.as_slice() {
        [] => Some(1),
        [(key, value)] if key == "page" => value
            .parse()
            .ok()
            .filter(|&n: &i64| n >= 1 && value == &n.to_string()),
        _ => None,
    }
}

pub async fn list(
    State(state): State<AppState>,
    viewer: Viewer,
    uri: Uri,
) -> Result<Response, AppError> {
    if viewer.user().is_none() {
        return Ok(web::login_first("/notifications"));
    }
    let Some(page) = requested_page(&uri) else {
        return web::not_found(&state, &viewer).await;
    };
    let list = content::notifications::list(state.db(), &viewer, page).await?;
    let total = page_count(list.total, NOTIFICATIONS_PER_PAGE);
    if page > total {
        return web::not_found(&state, &viewer).await;
    }
    render(
        StatusCode::OK,
        &NotificationsTemplate {
            layout: Layout::load(&state, &viewer, Nav::Notifications).await?,
            unread: content::notifications::unread_count(state.db(), &viewer).await?,
            list,
            pagination: Pagination::new("/notifications", page, total),
        },
    )
}

/// Follows a notification: marks it read and goes to its post.
pub async fn open(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let Some(user) = viewer.user() else {
        return Ok(web::login_first("/notifications"));
    };
    let Some(id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    let Some(target) = content::notifications::target(state.db(), &viewer, id).await? else {
        return web::not_found(&state, &viewer).await;
    };
    if stored::mark_read(state.db(), user.id, id).await? {
        notify::counts_changed(&state, user.id);
    }
    Ok(see_other(&match target {
        Some((post_id, location)) => location.href(post_id),
        None => "/notifications".to_owned(),
    }))
}

pub async fn mark_all_read(
    State(state): State<AppState>,
    viewer: Viewer,
) -> Result<Response, AppError> {
    let Some(user) = viewer.user() else {
        return Ok(web::login_first("/notifications"));
    };
    stored::mark_all_read(state.db(), user.id).await?;
    notify::counts_changed(&state, user.id);
    Ok(see_other("/notifications"))
}

// ---------------------------------------------------------------------------
// Watching

fn watch_level(fields: &Fields) -> Option<WatchLevel> {
    field(fields, "level").and_then(WatchLevel::parse)
}

pub async fn watch_thread(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    let Some(user) = viewer.user() else {
        return Ok(web::login_first(&format!("/t/{id}")));
    };
    let Some(thread_id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    let Some(thread) = content::thread_target(state.db(), &viewer, thread_id).await? else {
        return web::not_found(&state, &viewer).await;
    };
    let Some(level) = watch_level(&fields) else {
        return web::message(
            &state,
            &viewer,
            StatusCode::BAD_REQUEST,
            "That didn’t work",
            &["Choose watching, normal or muted."],
            Some((&thread.href(), "Go back")),
        )
        .await;
    };
    social::set_watch(state.db(), user.id, Watched::Thread(thread_id), level).await?;
    Ok(see_other(&thread.href()))
}

pub async fn watch_space(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(slug): Path<String>,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    let Some(user) = viewer.user() else {
        return Ok(web::login_first(&format!("/c/{slug}")));
    };
    let Some(space) = content::space_target(state.db(), &viewer, &slug).await? else {
        return web::not_found(&state, &viewer).await;
    };
    let back = format!("/c/{}", space.space.slug);
    let Some(level) = watch_level(&fields) else {
        return web::message(
            &state,
            &viewer,
            StatusCode::BAD_REQUEST,
            "That didn’t work",
            &["Choose watching, normal or muted."],
            Some((&back, "Go back")),
        )
        .await;
    };
    social::set_watch(state.db(), user.id, Watched::Space(space.space.id), level).await?;
    Ok(see_other(&back))
}

// ---------------------------------------------------------------------------
// Personal settings

pub async fn save_dm_policy(
    State(state): State<AppState>,
    viewer: Viewer,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    let Some(user) = viewer.user() else {
        return Ok(web::login_first("/settings"));
    };
    let Some(policy) = field(&fields, "dm_policy").and_then(DmPolicy::parse) else {
        return web::message(
            &state,
            &viewer,
            StatusCode::BAD_REQUEST,
            "That didn’t work",
            &["Choose who can message you."],
            Some(("/settings#messages", "Go back")),
        )
        .await;
    };
    social::set_dm_policy(state.db(), user.id, policy).await?;
    Ok(see_other("/settings?done=messages#messages"))
}

pub async fn save_email_prefs(
    State(state): State<AppState>,
    viewer: Viewer,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    let Some(user) = viewer.user() else {
        return Ok(web::login_first("/settings"));
    };
    let on = |name: &str| field(&fields, name).is_some();
    let prefs = EmailPrefs {
        replies: on("replies"),
        mentions: on("mentions"),
        messages: on("messages"),
        watching: on("watching"),
        moderation: on("moderation"),
    };
    social::set_email_prefs(state.db(), user.id, &prefs).await?;
    Ok(see_other("/settings?done=email-prefs#email"))
}

// ---------------------------------------------------------------------------
// One-click unsubscribe

#[derive(Template)]
#[template(path = "unsubscribe.html")]
struct UnsubscribeTemplate {
    layout: Layout,
    token: String,
    what: &'static str,
    done: bool,
}

async fn unsubscribe_target(
    state: &AppState,
    token: &str,
) -> Result<Option<(i64, EmailTopic)>, AppError> {
    let secret = settings::signing_secret(state.db()).await?;
    Ok(notify::check_unsubscribe_token(&secret, token))
}

/// The page the link in an email opens: a button, so that link checkers
/// following it don't unsubscribe anyone.
pub async fn unsubscribe_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(token): Path<String>,
) -> Result<Response, AppError> {
    let Some((_, topic)) = unsubscribe_target(&state, &token).await? else {
        return web::not_found(&state, &viewer).await;
    };
    Ok(web::secret_url(render(
        StatusCode::OK,
        &UnsubscribeTemplate {
            layout: Layout::load(&state, &viewer, Nav::None).await?,
            token,
            what: topic.describe(),
            done: false,
        },
    )?))
}

/// Unsubscribes: from the page's button, or straight from a mail
/// provider's one-click POST (`List-Unsubscribe=One-Click`).
pub async fn unsubscribe(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(token): Path<String>,
) -> Result<Response, AppError> {
    let Some((user_id, topic)) = unsubscribe_target(&state, &token).await? else {
        return web::not_found(&state, &viewer).await;
    };
    let prefs = social::email_prefs(state.db(), user_id).await?;
    social::set_email_prefs(state.db(), user_id, &prefs.without(topic)).await?;
    Ok(web::secret_url(render(
        StatusCode::OK,
        &UnsubscribeTemplate {
            layout: Layout::load(&state, &viewer, Nav::None).await?,
            token,
            what: topic.describe(),
            done: true,
        },
    )?))
}

// ---------------------------------------------------------------------------
// The admin's settings for messages and notification email

#[derive(Template)]
#[template(path = "admin_messages.html")]
struct AdminMessagesTemplate {
    layout: Layout,
    messages: MessageSettings,
    notifications: NotificationSettings,
    notice: Option<String>,
    errors: Vec<String>,
    admin_email: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct DoneQuery {
    done: String,
}

async fn render_admin(
    state: &AppState,
    viewer: &Viewer,
    status: StatusCode,
    notice: Option<String>,
    errors: Vec<String>,
) -> Result<Response, AppError> {
    let user = viewer.user().expect("admins are logged in");
    render(
        status,
        &AdminMessagesTemplate {
            layout: Layout::load(state, viewer, Nav::Admin).await?,
            messages: settings::message_settings(state.db()).await?,
            notifications: settings::notification_settings(state.db()).await?,
            notice,
            errors,
            admin_email: user.email.clone(),
        },
    )
}

async fn not_admin(state: &AppState, viewer: &Viewer) -> Result<Response, AppError> {
    if viewer.user().is_none() {
        Ok(web::login_first("/admin/messages"))
    } else {
        web::forbidden(state, viewer).await
    }
}

pub async fn admin_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Query(query): Query<DoneQuery>,
) -> Result<Response, AppError> {
    if !viewer.is_admin() {
        return not_admin(&state, &viewer).await;
    }
    let notice = match query.done.as_str() {
        "saved" => Some("Settings saved.".to_owned()),
        "test" => Some(format!(
            "A test notification email is on its way to {}.",
            viewer.user().map(|u| u.email.as_str()).unwrap_or_default()
        )),
        _ => None,
    };
    render_admin(&state, &viewer, StatusCode::OK, notice, Vec::new()).await
}

fn number(fields: &Fields, name: &str, max: u32, problems: &mut Vec<String>, label: &str) -> u32 {
    match field(fields, name)
        .unwrap_or_default()
        .trim()
        .parse::<u32>()
    {
        Ok(n) if n <= max => n,
        _ => {
            problems.push(format!("{label} should be a whole number from 0 to {max}."));
            0
        }
    }
}

pub async fn save_admin(
    State(state): State<AppState>,
    viewer: Viewer,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    if !viewer.is_admin() {
        return not_admin(&state, &viewer).await;
    }
    let mut problems = Vec::new();
    let messages = MessageSettings {
        enabled: field(&fields, "enabled").is_some(),
        max_recipients: number(
            &fields,
            "max_recipients",
            100,
            &mut problems,
            "The most people in a conversation",
        )
        .max(1),
        new_user_days: number(&fields, "new_user_days", 365, &mut problems, "The days"),
        new_user_posts: number(&fields, "new_user_posts", 1000, &mut problems, "The posts"),
    };
    let notifications = NotificationSettings {
        email_delay_minutes: number(
            &fields,
            "email_delay_minutes",
            7 * 24 * 60,
            &mut problems,
            "The email delay",
        ),
    };
    if !problems.is_empty() {
        return render_admin(&state, &viewer, StatusCode::BAD_REQUEST, None, problems).await;
    }
    settings::set(state.db(), "messages", &messages).await?;
    settings::set(state.db(), "notifications", &notifications).await?;
    Ok(see_other("/admin/messages?done=saved"))
}

/// Sends the admin a sample notification email, with the unsubscribe
/// headers, so they can see what members get.
pub async fn test_email(
    State(state): State<AppState>,
    viewer: Viewer,
) -> Result<Response, AppError> {
    if !viewer.is_admin() {
        return not_admin(&state, &viewer).await;
    }
    let user = viewer.user().expect("admins are logged in");
    let site_name = settings::site_name(state.db()).await?;
    let secret = settings::signing_secret(state.db()).await?;
    let unsubscribe = state.config().url(&format!(
        "/unsubscribe/{}",
        notify::unsubscribe_token(&secret, user.id, EmailTopic::All)
    ));
    let email = emails::notifications(
        &user.email,
        &site_name,
        &user.display_name,
        &[EmailItem {
            headline: format!("This is a test from {site_name}"),
            text: "Members get emails like this one about notifications they haven’t seen, once they have been away for a while.".to_owned(),
            url: state.config().url("/notifications"),
        }],
        &state.config().url("/settings#email"),
        &unsubscribe,
        EmailTopic::All.describe(),
    );
    if let Err(err) = mail::send_now(&state, &email).await {
        let problem = format!("The test email couldn’t be sent: {err:#}");
        return render_admin(
            &state,
            &viewer,
            StatusCode::BAD_GATEWAY,
            None,
            vec![problem],
        )
        .await;
    }
    Ok(see_other("/admin/messages?done=test"))
}

impl Kind {
    /// The phrase for a notification row, after the actor's name.
    fn phrase(&self) -> &'static str {
        self.verb()
    }
}
