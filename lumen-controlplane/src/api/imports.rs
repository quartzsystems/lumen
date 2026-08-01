//! Machine-import endpoints: a VMware archive in, a Lumen machine out.
//!
//! Thin like every other handler file — the spool, the descriptor reading,
//! and the job live in [`crate::vm_import`] and `lumen_virt::ovf`. What
//! belongs here is the HTTP: the streamed upload (the same discipline as the
//! installation-image route, and the only other body-limit-free route), the
//! 202-then-poll job shape the pool workflows established, and the session
//! every route demands.

use std::sync::Arc;

use axum::body::Body as AxumBody;
use axum::extract::{Path, State};
use axum::http::header;
use axum::Json;
use futures_util::StreamExt;

use crate::api::request::{required_body, Body};
use crate::error::ApiError;
use crate::security::Session;
use crate::vm_import::{ImportProgress, ImportView, VmImportCommit};
use crate::AppState;

/// GET /api/vms/import — every archive waiting in the spool, each with the
/// machine it describes.
pub async fn list(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let imports: Vec<ImportView> = state.import.list().await?;
    Ok(Json(serde_json::json!({ "imports": imports })))
}

/// PUT /api/vms/import/{name} — spool an uploaded archive, then answer with
/// the machine inside it.
///
/// The body is the archive itself, streamed to disk chunk by chunk — see
/// `storage::upload_iso` for why it is not multipart and carries no body
/// limit. Unlike an installation image the upload is not the whole story:
/// the answer is the parsed appliance, so the console can go straight from
/// "uploaded" to "here is the machine — where should its pieces land?".
/// An archive that cannot be read as an OVA is removed again in the same
/// breath: it can never be imported, and leaving it spooled would only
/// offer the same refusal again tomorrow.
pub async fn upload(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    headers: axum::http::HeaderMap,
    body: AxumBody,
) -> Result<Json<serde_json::Value>, ApiError> {
    let expected = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let mut upload = state.import.begin_upload(&name, expected).await?;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(err) => {
                upload.abort().await;
                return Err(ApiError::BadRequest(format!("The upload stopped: {err}")));
            }
        };
        if let Err(err) = upload.write(&chunk).await {
            upload.abort().await;
            return Err(err);
        }
    }
    let size = upload.finish().await?;

    let path = state.import.path(&name)?;
    let appliance = {
        let path = path.clone();
        tokio::task::spawn_blocking(move || lumen_virt::ovf::read_appliance(&path))
            .await
            .map_err(|err| ApiError::Internal(err.into()))?
    };
    let appliance = match appliance {
        Ok(appliance) => appliance,
        Err(err) => {
            let _ = tokio::fs::remove_file(&path).await;
            return Err(err.into());
        }
    };
    Ok(Json(serde_json::json!({
        "name": name,
        "size": size,
        "appliance": appliance,
    })))
}

/// POST /api/vms/import/{name} — start the import. 202, then the pending
/// feed.
pub async fn commit(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    raw: Body,
) -> Result<(axum::http::StatusCode, Json<ImportProgress>), ApiError> {
    let commit: VmImportCommit = required_body(raw)?;
    let by = format!("{}@{}", session.0.sub, session.0.realm);
    let progress = crate::vm_import::start_import(&state, name, commit, by).await?;
    Ok((axum::http::StatusCode::ACCEPTED, Json(progress)))
}

/// GET /api/vms/import/pending — the running (or last finished) import.
/// 404 when none has been started since the control plane came up.
pub async fn progress(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<ImportProgress>, ApiError> {
    state
        .import
        .snapshot()
        .map(Json)
        .ok_or_else(|| ApiError::NotFound("No import has been started.".to_string()))
}

/// DELETE /api/vms/import/{name} — remove one spooled archive.
pub async fn remove(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.import.delete(&name).await?;
    Ok(Json(serde_json::json!({ "name": name })))
}
