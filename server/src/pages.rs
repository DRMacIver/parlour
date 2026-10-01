use askama::Template;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

use crate::AppState;
use crate::error::AppError;
use crate::settings;

/// Fields every page needs for the shared layout in `base.html`.
pub struct Layout {
    pub site_name: String,
}

impl Layout {
    async fn load(state: &AppState) -> sqlx::Result<Layout> {
        Ok(Layout {
            site_name: settings::site_name(&state.db).await?,
        })
    }
}

fn render(template: &impl Template) -> Result<Html<String>, AppError> {
    Ok(Html(template.render()?))
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexPage {
    layout: Layout,
}

pub async fn index(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    render(&IndexPage {
        layout: Layout::load(&state).await?,
    })
}

#[derive(Template)]
#[template(path = "not_found.html")]
struct NotFoundPage {
    layout: Layout,
}

pub async fn not_found(State(state): State<AppState>) -> Result<Response, AppError> {
    let page = render(&NotFoundPage {
        layout: Layout::load(&state).await?,
    })?;
    Ok((StatusCode::NOT_FOUND, page).into_response())
}
