pub mod accounts;
mod admin_pages;
mod assets;
mod auth_pages;
pub mod cache;
pub mod changes;
pub mod client;
pub mod config;
pub mod content;
pub mod csrf;
pub mod diff;
pub mod emails;
mod error;
pub mod format;
pub mod http_cache;
mod invite_pages;
pub mod invites;
pub mod jobs;
pub mod login_tokens;
pub mod mail;
pub mod markdown;
pub mod members;
mod pages;
pub mod pagination;
pub mod permissions;
pub mod ratelimit;
pub mod seed;
pub mod sessions;
pub mod settings;
pub mod setup;
pub mod store;
pub mod tokens;
pub mod viewer;
mod web;
mod write_pages;
pub mod writing;

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::http::StatusCode;
use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use sqlx::PgPool;
use sqlx::migrate::Migrator;

use crate::cache::PageCache;
use crate::changes::Change;
use crate::config::Config;
use crate::jobs::Jobs;
use crate::mail::Mailer;
use crate::ratelimit::RateLimiter;

pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// How much memory the anonymous page cache may use by default.
pub const DEFAULT_CACHE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct AppState {
    db: PgPool,
    cache: Arc<PageCache>,
    config: Arc<Config>,
    mailer: Mailer,
    limiter: Arc<RateLimiter>,
    jobs: Arc<Jobs>,
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
        AppState {
            db,
            cache: Arc::new(cache),
            config: Arc::new(Config::development()),
            mailer: Mailer::default(),
            limiter: Arc::new(RateLimiter::default()),
            jobs: Arc::new(jobs),
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

    pub fn with_mailer(mut self, mailer: Mailer) -> Self {
        self.mailer = mailer;
        self
    }

    pub fn db(&self) -> &PgPool {
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

    pub(crate) fn setup_token(&self) -> &Mutex<Option<Vec<u8>>> {
        &self.setup_token
    }

    /// Call after a write commits, to clear the cached pages it affects.
    pub async fn changed(&self, change: Change) -> sqlx::Result<()> {
        let tags = changes::tags_for(&self.db, change).await?;
        self.cache.invalidate(&tags);
        Ok(())
    }
}

pub fn app(state: AppState) -> Router {
    let pages = Router::new()
        .route("/", get(pages::index))
        .route("/latest", get(pages::latest))
        .route("/c/{slug}", get(pages::category))
        .route("/t/{id}", get(pages::thread_without_slug))
        .route("/t/{id}/unread", get(pages::thread_unread))
        .route("/t/{id}/{slug}", get(pages::thread))
        .route("/p/{id}", get(pages::permalink))
        // Writing
        .route(
            "/c/{slug}/new",
            get(write_pages::new_thread_page).post(write_pages::new_thread),
        )
        .route("/c/{slug}/read", post(write_pages::mark_category_read))
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
        .route("/static/{*path}", get(assets::serve))
        .merge(pages)
        .with_state(state)
}

async fn healthz(axum::extract::State(state): axum::extract::State<AppState>) -> impl IntoResponse {
    match sqlx::query("SELECT 1").execute(&state.db).await {
        Ok(_) => (StatusCode::OK, "ok"),
        Err(err) => {
            tracing::error!(%err, "health check failed to reach the database");
            (StatusCode::SERVICE_UNAVAILABLE, "database unavailable")
        }
    }
}
