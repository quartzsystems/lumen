//! Request-body handling shared by every domain's handlers.
//!
//! Two things live here because all three domains need them and none of them
//! should own them:
//!
//! - the optional JSON body every mutating route accepts, so they can all
//!   carry the same optional `node` field;
//! - the node check itself, which is in the API from day one even though there
//!   is exactly one node — the request shape must not change when clustering
//!   lands, and quietly applying a request meant for another node to this one
//!   is exactly the wrong failure.

use axum::body::Bytes;
use axum::extract::{FromRequest, Request};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::ApiError;

/// An optional JSON body. The console sends one only when it has something to
/// say, and every mutating route accepts one either way.
///
/// An extractor of its own rather than `Option<Json<Value>>`, which answers
/// `None` only when the request carries no `Content-Type` at all: a request
/// that declares JSON and then sends nothing — which is what a browser `fetch`
/// with no body does — is rejected as a syntax error before the handler is
/// reached. "Optional" was true only for clients that also remembered to drop
/// the header, and no body is no body whatever the request calls it.
pub struct Body(Option<Value>);

impl<S: Send + Sync> FromRequest<S> for Body {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let bytes = Bytes::from_request(req, state)
            .await
            .map_err(|err| ApiError::BadRequest(err.to_string()))?;
        if bytes.is_empty() {
            return Ok(Self(None));
        }
        serde_json::from_slice(&bytes)
            .map(|value| Self(Some(value)))
            .map_err(|err| {
                ApiError::BadRequest(format!("The request body is not valid JSON: {err}"))
            })
    }
}

impl Body {
    /// The body as it arrived, with no cross-node guard applied. For the one
    /// route whose `node` field names a *different* node on purpose — adding a
    /// member to a cluster — where [`check_node`] would strip exactly the field
    /// the request is about.
    pub fn into_value(self) -> Option<Value> {
        self.0
    }
}

/// Split the optional `node` field off a request body and deserialize the rest
/// into a domain type. A missing or empty body is that type's default.
pub fn body<T: DeserializeOwned + Default>(raw: Body) -> Result<T, ApiError> {
    let Body(Some(mut value)) = raw else {
        return Ok(T::default());
    };
    check_node(&mut value)?;
    if value.as_object().is_some_and(|map| map.is_empty()) {
        return Ok(T::default());
    }
    serde_json::from_value(value).map_err(|err| ApiError::BadRequest(err.to_string()))
}

/// Same as [`body`], for bodies that must be present.
pub fn required_body<T: DeserializeOwned>(raw: Body) -> Result<T, ApiError> {
    let Body(Some(mut value)) = raw else {
        return Err(ApiError::BadRequest("A request body is required.".into()));
    };
    check_node(&mut value)?;
    serde_json::from_value(value).map_err(|err| ApiError::BadRequest(err.to_string()))
}

/// For routes whose body carries nothing but the optional `node` field. The
/// node still has to be checked — that is the whole point.
pub fn node_only(raw: Body) -> Result<(), ApiError> {
    if let Body(Some(mut value)) = raw {
        check_node(&mut value)?;
    }
    Ok(())
}

/// Remove `node` and reject anything that is not this appliance.
///
/// Removing it keeps the domain types `deny_unknown_fields`, which is what
/// turns a typo in a request into a 400 instead of an ignored setting.
pub fn check_node(value: &mut Value) -> Result<(), ApiError> {
    let Some(map) = value.as_object_mut() else {
        return Ok(());
    };
    let Some(node) = map.remove("node") else {
        return Ok(());
    };
    let Some(node) = node.as_str() else {
        return Err(ApiError::BadRequest("\"node\" must be a hostname.".into()));
    };
    let local = lumen_net::backend::nm::hostname();
    if node.is_empty() || node == local {
        return Ok(());
    }
    Err(ApiError::BadRequest(format!(
        "This appliance is not in a cluster, so changes cannot be sent to \"{node}\" — it only \
         manages \"{local}\"."
    )))
}
