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
    NotFound(String),
    /// Well-formed, but not allowed in the current state — a second network
    /// apply while one is still waiting to be confirmed, say.
    Conflict(String),
    /// A rejected configuration. Carries every problem, each with a stable
    /// code, so the console can render them against the offending fields.
    Validation(Vec<lumen_net::ValidationError>),
    Internal(anyhow::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // Validation answers carry an extra `errors` array alongside the
        // standard envelope; `error` is still there, so a client that only
        // knows the envelope still shows something useful.
        if let ApiError::Validation(errors) = self {
            let message = errors
                .iter()
                .map(|e| e.message.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": message, "errors": errors })),
            )
                .into_response();
        }

        let (status, message) = match self {
            ApiError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "Invalid username or password.".to_string(),
            ),
            ApiError::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            ApiError::NotFound(message) => (StatusCode::NOT_FOUND, message),
            ApiError::Conflict(message) => (StatusCode::CONFLICT, message),
            ApiError::Validation(_) => unreachable!("handled above"),
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

/// The networking domain's errors, mapped onto the API's.
impl From<lumen_net::NetError> for ApiError {
    fn from(err: lumen_net::NetError) -> Self {
        match err {
            lumen_net::NetError::Invalid(errors) => ApiError::Validation(errors),
            lumen_net::NetError::NotFound(message) => ApiError::NotFound(message),
            lumen_net::NetError::Conflict(message) => ApiError::Conflict(message),
            lumen_net::NetError::Backend(err) => ApiError::Internal(err),
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        ApiError::Internal(err)
    }
}
