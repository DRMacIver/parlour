mod account_pages;
pub mod accounts;
mod admin_forum;
mod admin_pages;
mod api;
mod assets;
mod auth_pages;
pub mod cache;
pub mod client;
pub mod config;
pub mod csrf;
pub mod db;
pub mod diff;
pub mod emails;
mod error;
pub mod format;
pub mod http_cache;
mod invite_pages;
pub mod islands;
pub mod live;
pub mod mail;
pub mod markdown;
mod message_pages;
pub mod messages;
mod mod_pages;
pub mod moderation;
mod notification_pages;
pub mod notify;
mod pages;
pub mod pagination;
pub mod permissions;
pub mod ratelimit;
mod search_pages;
pub mod setup;
mod space_pages;
pub mod startup;
pub mod tokens;
mod upload_pages;
pub mod uploads;
pub mod viewer;
mod web;
mod write_pages;
pub mod writing;

pub use db::{
    changes, content, groups, invites, jobs, layouts, login_tokens, members,
    moderation as mod_records, notifications, seed, sessions, settings, social, store,
    uploads as upload_records, users,
};

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use sqlx::PgPool;

use crate::db::Db;
use sqlx::migrate::Migrator;

use crate::cache::PageCache;
use crate::changes::Change;
use crate::config::Config;
use crate::jobs::Jobs;
use crate::live::{Event, Live};
use crate::mail::Mailer;
use crate::ratelimit::RateLimiter;
use crate::viewer::Viewer;

/// Serves the app on `listener` until `shutdown` completes, then stops
/// taking new connections and waits for the requests in progress. Open
/// live streams end as soon as shutdown starts, so they don't hold it up.
pub async fn serve(
    listener: tokio::net::TcpListener,
    state: AppState,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let live = state.live().clone();
    axum::serve(
        listener,
        app(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown.await;
        live.close();
    })
    .await
}

pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// How much memory the anonymous page cache may use by default.
pub const DEFAULT_CACHE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct AppState {
    db: Db,
    cache: Arc<PageCache>,
    config: Arc<Config>,
    mailer: Mailer,
    limiter: Arc<RateLimiter>,
    jobs: Arc<Jobs>,
    live: Live,
    /// Island files to use instead of the built ones, for tests.
    islands: Option<Arc<islands::IslandAssets>>,
    /// The hash of the one-time `/setup` token, while there is no admin.
    setup_token: Arc<Mutex<Option<Vec<u8>>>>,
}

impl AppState {
    /// An app with development settings ([`Config::development`]) that
    /// sends email over SMTP.
    pub fn new(db: PgPool) -> Self {
        AppState::with_cache(db, PageCache::new(DEFAULT_CACHE_BYTES))
    }

    pub fn with_cache(db: PgPool, cache: PageCache) -> Self {
        let jobs = Jobs::default();
        jobs.register(mail::JOB_KIND, mail::run_job);
        jobs.scrub_on_failure(mail::JOB_KIND, mail::scrub);
        jobs.register(notify::POST_JOB, notify::run_post_job);
        jobs.register(notify::EMAIL_JOB, notify::run_email_job);
        jobs.register(moderation::FORGET_IPS_JOB, moderation::run_forget_ips);
        AppState {
            db: Db::new(db),
            cache: Arc::new(cache),
            config: Arc::new(Config::development()),
            mailer: Mailer::default(),
            limiter: Arc::new(RateLimiter::default()),
            jobs: Arc::new(jobs),
            live: Live::default(),
            islands: None,
            setup_token: Arc::default(),
        }
    }

    pub fn with_config(mut self, config: Config) -> Self {
        self.config = Arc::new(config);
        self
    }

    pub fn with_limiter(mut self, limiter: RateLimiter) -> Self {
        self.limiter = Arc::new(limiter);
        self
    }

    /// Uses these island files instead of those built into the binary.
    pub fn with_islands(mut self, islands: islands::IslandAssets) -> Self {
        self.islands = Some(Arc::new(islands));
        self
    }

    pub fn islands(&self) -> islands::IslandAssets {
        match &self.islands {
            Some(islands) => (**islands).clone(),
            None => islands::IslandAssets::current(),
        }
    }

    pub fn with_mailer(mut self, mailer: Mailer) -> Self {
        self.mailer = mailer;
        self
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn cache(&self) -> &PageCache {
        &self.cache
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn mailer(&self) -> &Mailer {
        &self.mailer
    }

    pub fn limiter(&self) -> &RateLimiter {
        &self.limiter
    }

    pub fn jobs(&self) -> &Jobs {
        &self.jobs
    }

    pub fn live(&self) -> &Live {
        &self.live
    }

    pub(crate) fn setup_token(&self) -> &Mutex<Option<Vec<u8>>> {
        &self.setup_token
    }

    /// Call after a write commits, to clear the cached pages it affects.
    pub async fn changed(&self, change: Change) -> sqlx::Result<()> {
        let tags = changes::tags_for(&self.db, change).await?;
        self.cache.invalidate(&tags);
        if let Change::PostCreated { post_id } = change
            && self.live.has_listeners()
            && let Some((thread_id, seq)) = changes::post_seq(&self.db, post_id).await?
        {
            // Whether visitors may read it is checked once here, for every
            // visitor's stream at once.
            let public = content::thread_target(&self.db, &Viewer::Anonymous, thread_id)
                .await?
                .is_some();
            self.live.publish(Event::Post {
                thread_id,
                post_id,
                seq,
                public,
            });
        }
        Ok(())
    }
}

pub fn app(state: AppState) -> Router {
    let pages = Router::new()
        .route("/", get(pages::index))
        .route("/latest", get(pages::latest))
        .route("/latest/front", get(pages::latest_front))
        .route("/latest/s/{id}", get(pages::latest_section))
        .route("/latest/s/{id}/read", post(write_pages::mark_section_read))
        .route("/c/{slug}", get(pages::space))
        .route("/spaces", get(space_pages::index))
        .route("/t/{id}", get(pages::thread_without_slug))
        .route("/t/{id}/unread", get(pages::thread_unread))
        .route("/t/{id}/{slug}", get(pages::thread))
        .route("/p/{id}", get(pages::permalink))
        .route("/search", get(search_pages::search_page))
        .route("/uploads/{name}", get(upload_pages::serve))
        .route(
            "/uploads/new",
            get(upload_pages::upload_page)
                .post(upload_pages::upload)
                .layer(DefaultBodyLimit::max(upload_pages::MAX_BODY_BYTES)),
        )
        .route(
            "/api/uploads",
            post(upload_pages::api_upload)
                .layer(DefaultBodyLimit::max(upload_pages::MAX_BODY_BYTES)),
        )
        .route(
            "/settings/avatar",
            post(upload_pages::set_avatar)
                .layer(DefaultBodyLimit::max(upload_pages::MAX_BODY_BYTES)),
        )
        .route("/settings/avatar/remove", post(upload_pages::remove_avatar))
        .route("/api/preview", post(api::preview))
        .route("/api/posts/{id}/source", get(api::source))
        .route("/api/posts/{id}/reactions", post(api::react))
        .route("/api/posts/{id}/fork", get(api::fork))
        .route("/api/notifications", get(api::notifications))
        .route(
            "/api/notifications/read",
            post(api::mark_notifications_read),
        )
        .route("/settings/export", post(account_pages::export))
        .route("/settings/delete", post(account_pages::delete_account))
        .route("/terms", get(account_pages::terms))
        .route("/privacy", get(account_pages::privacy))
        .route(
            "/admin/pages",
            get(account_pages::admin_pages_page).post(account_pages::save_admin_pages),
        )
        // Writing
        .route(
            "/c/{slug}/new",
            get(write_pages::new_thread_page).post(write_pages::new_thread),
        )
        .route("/c/{slug}/read", post(write_pages::mark_space_read))
        .route(
            "/spaces/new",
            get(space_pages::new_page).post(space_pages::create),
        )
        .route(
            "/c/{slug}/settings",
            get(space_pages::settings_page).post(space_pages::save_settings),
        )
        .route("/c/{slug}/archive", post(space_pages::archive))
        .route("/c/{slug}/unarchive", post(space_pages::unarchive))
        .route("/c/{slug}/delete", post(space_pages::delete))
        .route("/c/{slug}/watch", post(notification_pages::watch_space))
        .route("/c/{slug}/follow", post(space_pages::follow))
        .route("/c/{slug}/unfollow", post(space_pages::unfollow))
        .route("/c/{slug}/mute", post(space_pages::mute))
        .route("/c/{slug}/unmute", post(space_pages::unmute))
        .route("/t/{id}/watch", post(notification_pages::watch_thread))
        .route(
            "/t/{id}/reply",
            get(write_pages::reply_page).post(write_pages::reply),
        )
        .route(
            "/p/{id}/edit",
            get(write_pages::edit_page).post(write_pages::edit),
        )
        .route(
            "/p/{id}/delete",
            get(write_pages::delete_page).post(write_pages::delete),
        )
        .route("/p/{id}/revisions", get(write_pages::revisions))
        .route(
            "/p/{id}/reactions",
            get(write_pages::reactions_page).post(write_pages::react),
        )
        .route(
            "/p/{id}/fork",
            get(write_pages::fork_page).post(write_pages::fork),
        )
        // Messages and notifications
        .route("/messages", get(message_pages::list))
        .route(
            "/messages/new",
            get(message_pages::new_page).post(message_pages::start),
        )
        .route(
            "/messages/{id}",
            get(message_pages::conversation).post(write_pages::reply),
        )
        .route("/messages/{id}/unread", get(message_pages::unread))
        .route("/messages/{id}/leave", post(message_pages::leave))
        .route("/messages/{id}/people", post(message_pages::add_person))
        .route(
            "/u/{username}/block",
            get(message_pages::block_page).post(message_pages::block),
        )
        .route("/u/{username}/unblock", post(message_pages::unblock))
        .route("/notifications", get(notification_pages::list))
        .route(
            "/notifications/read",
            post(notification_pages::mark_all_read),
        )
        .route("/notifications/{id}", get(notification_pages::open))
        .route(
            "/settings/messages",
            post(notification_pages::save_dm_policy),
        )
        .route(
            "/settings/notifications",
            post(notification_pages::save_email_prefs),
        )
        .route(
            "/unsubscribe/{token}",
            get(notification_pages::unsubscribe_page).post(notification_pages::unsubscribe),
        )
        .route(
            "/admin/messages",
            get(notification_pages::admin_page).post(notification_pages::save_admin),
        )
        .route(
            "/admin/messages/test-email",
            post(notification_pages::test_email),
        )
        // Moderation
        .route(
            "/p/{id}/report",
            get(mod_pages::report_post_page).post(mod_pages::report_post),
        )
        .route(
            "/u/{username}/report",
            get(mod_pages::report_member_page).post(mod_pages::report_member),
        )
        .route(
            "/contact",
            get(mod_pages::contact_page).post(mod_pages::contact),
        )
        .route("/held/{id}", get(mod_pages::held_page))
        .route(
            "/appeal/{id}",
            get(mod_pages::appeal_page).post(mod_pages::appeal),
        )
        .route("/moderation", get(mod_pages::public_log))
        .route("/mod", get(mod_pages::mod_index))
        .route("/mod/queue", get(mod_pages::queue))
        .route("/mod/log", get(mod_pages::staff_log))
        .route("/mod/users", get(mod_pages::users_page))
        .route("/mod/users/{username}", get(mod_pages::user_page))
        .route(
            "/mod/users/{username}/{action}",
            post(mod_pages::user_action),
        )
        .route(
            "/mod/p/{id}",
            get(mod_pages::post_page).post(mod_pages::moderate_post),
        )
        .route(
            "/mod/p/{id}/revisions/{rev}/redact",
            post(mod_pages::redact),
        )
        .route(
            "/mod/t/{id}",
            get(mod_pages::thread_page).post(mod_pages::moderate_thread),
        )
        .route("/mod/t/{id}/move-posts", post(mod_pages::move_posts))
        .route("/mod/held/{id}/approve", post(mod_pages::approve))
        .route("/mod/held/{id}/reject", post(mod_pages::reject))
        .route("/mod/reports/{id}/dismiss", post(mod_pages::dismiss))
        .route("/mod/appeals/{id}", post(mod_pages::decide_appeal))
        .route("/mod/uploads", get(mod_pages::images_page))
        .route("/mod/uploads/{name}", get(mod_pages::image_page))
        .route("/mod/uploads/{name}/remove", post(mod_pages::remove_image))
        .route(
            "/admin/moderation",
            get(mod_pages::admin_page).post(mod_pages::save_admin),
        )
        .route("/events/t/{id}", get(live::thread_events))
        .route("/events/notifications", get(live::notification_events))
        // Accounts
        .route(
            "/login",
            get(auth_pages::login_page).post(auth_pages::login),
        )
        .route("/login/email", post(auth_pages::request_magic_link))
        .route("/login/code", post(auth_pages::enter_code))
        .route(
            "/login/{token}",
            get(auth_pages::magic_link_page).post(auth_pages::use_magic_link),
        )
        .route(
            "/logout",
            get(auth_pages::logout_page).post(auth_pages::logout),
        )
        .route(
            "/reset",
            get(auth_pages::reset_request_page).post(auth_pages::request_reset),
        )
        .route(
            "/reset/{token}",
            get(auth_pages::reset_page).post(auth_pages::reset_password),
        )
        .route(
            "/verify/{token}",
            get(auth_pages::verify_page).post(auth_pages::verify_email),
        )
        .route("/settings", get(auth_pages::settings_page))
        .route("/settings/password", post(auth_pages::change_password))
        .route("/settings/profile", post(auth_pages::save_profile))
        .route("/settings/email", post(auth_pages::change_email))
        .route("/settings/verify", post(auth_pages::resend_verification))
        .route("/settings/sessions", post(auth_pages::log_out_elsewhere))
        // Invites and members
        .route(
            "/invites",
            get(invite_pages::invites_page).post(invite_pages::create_invite),
        )
        .route("/invites/{id}/revoke", post(invite_pages::revoke_invite))
        .route(
            "/invite/{code}",
            get(invite_pages::redeem_page).post(invite_pages::redeem),
        )
        .route(
            "/join/{token}",
            get(invite_pages::join_page).post(invite_pages::finish_joining),
        )
        .route("/users/tree", get(invite_pages::tree))
        .route("/u/{username}", get(invite_pages::profile))
        // Staff
        .route(
            "/admin/settings",
            get(admin_pages::settings_page).post(admin_pages::save_settings),
        )
        .route("/admin/settings/test-email", post(admin_pages::test_email))
        .route("/admin/users", get(admin_pages::users_page))
        .route(
            "/admin/reactions",
            get(write_pages::admin_reactions_page).post(write_pages::admin_reactions),
        )
        .route("/admin/users/{id}/role", post(admin_pages::change_role))
        .route("/admin/spaces", get(admin_forum::spaces_page))
        .route("/admin/spaces/new", get(admin_forum::new_space_page))
        .route("/admin/spaces/{id}", get(admin_forum::space_page))
        .route("/admin/sections", post(admin_forum::create_section))
        .route("/admin/sections/{id}", post(admin_forum::update_section))
        .route(
            "/admin/sections/{id}/delete",
            get(admin_forum::delete_section_page).post(admin_forum::delete_section),
        )
        .route(
            "/admin/groups",
            get(admin_forum::groups_page).post(admin_forum::create_group),
        )
        .route(
            "/admin/groups/{id}",
            get(admin_forum::group_page).post(admin_forum::update_group),
        )
        .route(
            "/admin/groups/{id}/delete",
            get(admin_forum::delete_group_page).post(admin_forum::delete_group),
        )
        .route("/admin/groups/{id}/members", post(admin_forum::add_member))
        .route(
            "/admin/groups/{id}/members/{user_id}/remove",
            post(admin_forum::remove_member),
        )
        .route("/setup", get(setup::setup_page).post(setup::create_admin))
        .route(
            "/setup/email",
            get(setup::email_page).post(setup::save_email),
        )
        .route(
            "/setup/invites",
            get(setup::invites_page).post(setup::create_invites),
        )
        .route("/healthz", get(healthz))
        .fallback(pages::not_found)
        // Outermost last: the cache answers anonymous GETs first, then
        // form posts from other sites are refused, then the session cookie
        // is looked up.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            sessions::middleware,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            csrf::middleware,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            http_cache::middleware,
        ));
    Router::new()
        .route("/static/islands/{*path}", get(islands::serve))
        .route("/static/{*path}", get(assets::serve))
        .merge(pages)
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

/// Headers every response carries: no other site may show a page of this
/// one in a frame (so nobody can be tricked into clicking a form here
/// through an invisible frame on their site), and browsers are to believe
/// the content type given rather than sniff one.
async fn security_headers(
    request: axum::extract::Request,
    next: middleware::Next,
) -> axum::response::Response {
    use axum::http::HeaderValue;
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static("frame-ancestors 'none'"),
    );
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
}

async fn healthz(axum::extract::State(state): axum::extract::State<AppState>) -> impl IntoResponse {
    match state.db.ping().await {
        Ok(_) => (StatusCode::OK, "ok"),
        Err(err) => {
            tracing::error!(%err, "health check failed to reach the database");
            (StatusCode::SERVICE_UNAVAILABLE, "database unavailable")
        }
    }
}
