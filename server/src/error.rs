use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// An unexpected failure while handling a request. The details are logged
/// and the visitor sees a generic 500 page.
#[derive(Debug)]
pub struct AppError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(err: E) -> Self {
        AppError(err.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::error!(err = ?self.0, "request failed");
        (StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong.").into_response()
    }
}
