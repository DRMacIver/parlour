mod assets;
pub mod cache;
pub mod changes;
pub mod content;
mod error;
pub mod format;
pub mod http_cache;
pub mod markdown;
mod pages;
pub mod pagination;
pub mod seed;
mod settings;
pub mod store;
pub mod viewer;

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::get;
use sqlx::PgPool;
use sqlx::migrate::Migrator;

use crate::cache::PageCache;
use crate::changes::Change;

pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// How much memory the anonymous page cache may use by default.
pub const DEFAULT_CACHE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct AppState {
    db: PgPool,
    cache: Arc<PageCache>,
}

impl AppState {
    pub fn new(db: PgPool) -> Self {
        AppState::with_cache(db, PageCache::new(DEFAULT_CACHE_BYTES))
    }

    pub fn with_cache(db: PgPool, cache: PageCache) -> Self {
        AppState {
            db,
            cache: Arc::new(cache),
        }
    }

    pub fn cache(&self) -> &PageCache {
        &self.cache
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
        .route("/healthz", get(healthz))
        .fallback(pages::not_found)
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
