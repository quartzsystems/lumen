use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// API error → `{ "error": "..." }` with the matching status. The web UI's
/// client surfaces the message verbatim, so texts are user-facing.
#[derive(Debug)]
pub enum ApiError {
    /// Wrong credentials or no/expired session. Body text is deliberately
    /// uniform so responses don't leak whether the account exists.
    Unauthorized,
    BadRequest(String),
    Internal(anyhow::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "Invalid username or password.".to_string(),
            ),
            ApiError::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            ApiError::Internal(err) => {
                tracing::error!("internal error: {err:#}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal server error.".to_string(),
                )
            }
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        ApiError::Internal(err)
    }
}
