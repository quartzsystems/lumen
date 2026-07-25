use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use serde_json::json;

/// A rejected document, in the shape the console reads: a joined sentence for
/// anything that only knows the standard envelope, plus the machine-readable
/// detail for anything that can pin each problem to a field.
///
/// Each domain has its own `ValidationError` — networking's names a link,
/// virtualization's names a machine — and neither should have to know about
/// the other. They meet here, where the answer is JSON either way.
#[derive(Debug)]
pub struct Rejection {
    pub message: String,
    pub errors: serde_json::Value,
}

impl Rejection {
    fn of<T: Serialize>(errors: Vec<T>, message: impl Fn(&T) -> &str) -> Self {
        let joined = errors.iter().map(&message).collect::<Vec<_>>().join(" ");
        Self {
            message: joined,
            errors: serde_json::to_value(&errors).unwrap_or(serde_json::Value::Null),
        }
    }
}

impl From<Vec<lumen_net::ValidationError>> for Rejection {
    fn from(errors: Vec<lumen_net::ValidationError>) -> Self {
        Rejection::of(errors, |e| e.message.as_str())
    }
}

impl From<Vec<lumen_virt::ValidationError>> for Rejection {
    fn from(errors: Vec<lumen_virt::ValidationError>) -> Self {
        Rejection::of(errors, |e| e.message.as_str())
    }
}

impl From<Vec<lumen_zfs::ValidationError>> for Rejection {
    fn from(errors: Vec<lumen_zfs::ValidationError>) -> Self {
        Rejection::of(errors, |e| e.message.as_str())
    }
}

impl From<Vec<lumen_sys::ValidationError>> for Rejection {
    fn from(errors: Vec<lumen_sys::ValidationError>) -> Self {
        Rejection::of(errors, |e| e.message.as_str())
    }
}

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
    /// apply while one is still waiting to be confirmed, or starting a machine
    /// that is already running.
    Conflict(String),
    /// A rejected configuration. Carries every problem, each with a stable
    /// code, so the console can render them against the offending fields.
    Validation(Rejection),
    Internal(anyhow::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // Validation answers carry an extra `errors` array alongside the
        // standard envelope; `error` is still there, so a client that only
        // knows the envelope still shows something useful.
        if let ApiError::Validation(rejection) = self {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": rejection.message, "errors": rejection.errors })),
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
            lumen_net::NetError::Invalid(errors) => ApiError::Validation(errors.into()),
            lumen_net::NetError::NotFound(message) => ApiError::NotFound(message),
            lumen_net::NetError::Conflict(message) => ApiError::Conflict(message),
            lumen_net::NetError::Backend(err) => ApiError::Internal(err),
        }
    }
}

/// The virtualization domain's errors, mapped the same way.
impl From<lumen_virt::VirtError> for ApiError {
    fn from(err: lumen_virt::VirtError) -> Self {
        match err {
            lumen_virt::VirtError::Invalid(errors) => ApiError::Validation(errors.into()),
            lumen_virt::VirtError::NotFound(message) => ApiError::NotFound(message),
            lumen_virt::VirtError::Conflict(message) => ApiError::Conflict(message),
            lumen_virt::VirtError::Backend(err) => ApiError::Internal(err),
        }
    }
}

/// The storage domain's errors. Its validation arm is about pools — the rules
/// about a *volume* live where the disk is actually being asked for.
impl From<lumen_zfs::ZfsError> for ApiError {
    fn from(err: lumen_zfs::ZfsError) -> Self {
        match err {
            lumen_zfs::ZfsError::Invalid(errors) => ApiError::Validation(errors.into()),
            lumen_zfs::ZfsError::NotFound(message) => ApiError::NotFound(message),
            lumen_zfs::ZfsError::Conflict(message) => ApiError::Conflict(message),
            lumen_zfs::ZfsError::Backend(err) => ApiError::Internal(err),
        }
    }
}

/// The system domain's errors, mapped the same way.
impl From<lumen_sys::SysError> for ApiError {
    fn from(err: lumen_sys::SysError) -> Self {
        match err {
            lumen_sys::SysError::Invalid(errors) => ApiError::Validation(errors.into()),
            lumen_sys::SysError::NotFound(message) => ApiError::NotFound(message),
            lumen_sys::SysError::Conflict(message) => ApiError::Conflict(message),
            lumen_sys::SysError::Backend(err) => ApiError::Internal(err),
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        ApiError::Internal(err)
    }
}
