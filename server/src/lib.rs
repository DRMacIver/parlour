mod assets;
mod error;
mod pages;
mod settings;

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use sqlx::PgPool;
use sqlx::migrate::Migrator;

pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

#[derive(Clone)]
pub struct AppState {
    db: PgPool,
}

impl AppState {
    pub fn new(db: PgPool) -> Self {
        AppState { db }
    }
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/", get(pages::index))
        .route("/healthz", get(healthz))
        .route("/static/{*path}", get(assets::serve))
        .fallback(pages::not_found)
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
