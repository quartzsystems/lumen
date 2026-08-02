//! The console viewer's transport: a WebSocket over the machine's own console
//! socket.
//!
//! The split is the same one the rest of the API keeps. `lumen-virt` answers
//! *whether a machine has a screen and where it is* — that is a question about
//! machines — and this module carries the bytes, because a UNIX socket and a
//! WebSocket are transport and the virtualization domain has no HTTP in it.
//! There is no protocol handling here either: the hypervisor speaks RFB on one
//! end and the viewer speaks RFB on the other, and everything in between is a
//! pipe. Nothing in this file knows what a framebuffer update is.
//!
//! ## Why a pipe and not a VNC proxy
//!
//! The alternative would be terminating RFB here — authenticating, then
//! re-encoding for the browser. It would mean a second implementation of a
//! protocol the hypervisor already implements, a second place for it to be
//! wrong, and a second thing to keep current. The socket already carries
//! exactly the stream the viewer wants; the only thing missing was a way to
//! reach it from a browser, and that is what this is.
//!
//! ## Why there is no console ticket
//!
//! Other appliances mint a single-use ticket for the viewer because their
//! console is served by a second process on a second port, where the session
//! cookie does not reach. Lumen's console is the same origin as the console
//! that opened it — one daemon, one port — so the upgrade request carries the
//! same httpOnly cookie every other call does, and [`Session`] checks it the
//! same way. A second credential would be a second thing to expire, leak, and
//! explain.
//!
//! What that does buy is one thing a normal request gets for free and a
//! WebSocket does not: cross-origin protection. A WebSocket handshake is not
//! subject to the same-origin policy, so the `Origin` header is checked by
//! hand — see [`same_origin`].

use std::sync::Arc;

use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, Uri};
use axum::response::Response;
use axum::Json;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use lumen_virt::ConsoleTarget;

use crate::error::ApiError;
use crate::security::Session;
use crate::AppState;

/// How much of the hypervisor's stream becomes one WebSocket frame.
///
/// A framebuffer update after a screen change is far bigger than this and is
/// simply split across frames, which is what a stream protocol expects. Larger
/// frames would not make it arrive sooner; smaller ones would add headers to
/// every screen repaint.
const CHUNK: usize = 32 * 1024;

/// GET /api/vms/{vmid}/console — where the viewer should connect, before it
/// tries to.
///
/// The upgrade below can only fail with a close code once the handshake has
/// completed, and a close code is not a sentence anyone can act on. Asking
/// first means a machine that has stopped, or a socket that is not there, is
/// reported as an ordinary API error with the reason in it — which is the
/// difference between "This machine is not running" and a viewer that flickers
/// and goes grey.
#[derive(Debug, Serialize)]
pub struct ConsoleInfo {
    #[serde(flatten)]
    target: ConsoleTarget,
    /// The path the browser opens. Same origin as the page that opened it, so
    /// the session cookie is sent with the upgrade and there is no second
    /// credential to mint.
    websocket: String,
}

pub async fn info(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
) -> Result<Json<ConsoleInfo>, ApiError> {
    // The one thing about a machine that is not relayed to the member holding
    // it. A screen is a socket on that node, not a request and an answer, and
    // proxying the stream would mean a second console protocol carrying RFB
    // between two control planes. The refusal names the node instead, which is
    // what an operator needs to do the thing themselves.
    if let Some(owner) = crate::cluster_vms::owner_of(&state, vmid).await {
        return Err(ApiError::Conflict(format!(
            "Machine {vmid} is running on \"{}\", and its screen is served by that node. Open it \
             from \"{}\"'s console.",
            owner.name, owner.name
        )));
    }
    let target = state.virt.console(vmid).await?;
    Ok(Json(ConsoleInfo {
        websocket: format!("/api/vms/{vmid}/console/ws"),
        target,
    }))
}

/// GET /api/vms/{vmid}/console/ws — the console stream itself.
///
/// Everything that can be said in words is said before the upgrade: the
/// session, the origin, whether the machine is running, whether the request
/// can become a WebSocket at all, and whether the socket answers. Only once
/// all five hold does the handshake complete, and from then on the only thing
/// left to report is the connection ending — a close code, which is not a
/// sentence anyone can act on.
///
/// That order is why the upgrade arrives as a `Result` rather than as a plain
/// extractor. An extractor rejects the request before the handler body runs,
/// which would answer "this is not a WebSocket" to somebody whose real problem
/// is that their machine is switched off.
pub async fn attach(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    headers: HeaderMap,
    uri: Uri,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Result<Response, ApiError> {
    if !state.config.no_tls {
        same_origin(&headers, &uri)?;
    }

    let target = state.virt.console(vmid).await?;

    let upgrade = upgrade.map_err(|rejection| {
        ApiError::BadRequest(format!(
            "The console of \"{}\" is a WebSocket, and this request could not become one: \
             {rejection}",
            target.name
        ))
    })?;

    // Connect before handing the connection over. A hypervisor that never
    // opened the socket, or a machine that stopped between the check above and
    // this line, is a sentence the console can show — and it can only be a
    // sentence while this is still an HTTP request.
    // Logged as well as returned, and that is not belt and braces: a browser
    // cannot read the body of a failed WebSocket handshake — it gets a close
    // and nothing else — so this sentence reaches the operator only through the
    // journal. Without it the console fails with "the connection ended" and no
    // node-side record of why.
    let stream = UnixStream::connect(&target.socket).await.map_err(|err| {
        tracing::warn!(
            vmid,
            name = %target.name,
            socket = %target.socket,
            error = %err,
            "could not open the console socket"
        );
        ApiError::Conflict(format!(
            "Could not open the console of \"{}\": {err} ({}).",
            target.name, target.socket
        ))
    })?;

    tracing::info!(vmid, socket = %target.socket, "console attached");
    let name = target.name.clone();
    Ok(upgrade.on_upgrade(move |socket| async move {
        pump(socket, stream).await;
        tracing::info!(vmid, name = %name, "console closed");
    }))
}

/// Refuse a handshake another site started.
///
/// A WebSocket handshake is exempt from the same-origin policy — no preflight,
/// no CORS — so a page on another origin can open one against this appliance
/// and the browser will attach the operator's cookie to it. The header is the
/// defence, and it is checked here rather than by a layer, because this is the
/// only route on the appliance where it matters.
///
/// A **missing** `Origin` is allowed, and that is not a hole: the header is
/// attached by browsers, so its absence means no browser sent this, and a
/// request no browser sent carries no cookie the sender did not attach on
/// purpose. Cross-site request forgery needs a victim's browser to be the one
/// making the request; there is none here to borrow.
pub(crate) fn same_origin(headers: &HeaderMap, uri: &Uri) -> Result<(), ApiError> {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return Ok(());
    };
    // The address the browser asked for. HTTP/1.1 puts it in a header;
    // HTTP/2 puts it in the request line, which axum surfaces on the URI.
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| uri.authority().map(|a| a.as_str()))
        .unwrap_or_default();
    let from = origin.split_once("://").map(|(_, rest)| rest).unwrap_or("");

    if !from.is_empty() && from == host {
        return Ok(());
    }
    tracing::warn!(%origin, %host, "refused a console connection from another origin");
    Err(ApiError::BadRequest(format!(
        "Refusing a console connection opened from \"{origin}\"."
    )))
}

/// Copy bytes both ways until either end stops.
///
/// The two halves are separate futures rather than one loop over both, because
/// a single loop would have to hold a mutable borrow of the WebSocket in two
/// places at once. `select!` is what ends the other half when one finishes:
/// dropping the sender closes the console socket, and dropping the receiver
/// closes the browser's, so neither side is left holding a connection whose
/// other end has gone.
async fn pump(socket: WebSocket, stream: UnixStream) {
    let (mut to_browser, mut from_browser) = socket.split();
    let (mut from_guest, mut to_guest) = stream.into_split();

    let browser_to_guest = async move {
        while let Some(Ok(message)) = from_browser.next().await {
            let bytes = match message {
                Message::Binary(bytes) => bytes,
                // A viewer that sends text is speaking the same protocol
                // through a different frame type; the bytes are the bytes.
                Message::Text(text) => text.as_bytes().to_vec().into(),
                Message::Close(_) => break,
                // Ping and pong are answered by the layer below.
                Message::Ping(_) | Message::Pong(_) => continue,
            };
            if to_guest.write_all(&bytes).await.is_err() {
                break;
            }
        }
    };

    let guest_to_browser = async move {
        let mut buffer = vec![0u8; CHUNK];
        loop {
            match from_guest.read(&mut buffer).await {
                // The hypervisor closed the socket: the guest is gone.
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    let frame = Message::Binary(buffer[..read].to_vec().into());
                    if to_browser.send(frame).await.is_err() {
                        break;
                    }
                }
            }
        }
        // Say why the viewer is about to go quiet, rather than letting the
        // connection simply stop answering.
        let _ = to_browser.send(Message::Close(None)).await;
    };

    tokio::select! {
        _ = browser_to_guest => {}
        _ = guest_to_browser => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(origin: Option<&str>, host: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, host.parse().unwrap());
        if let Some(origin) = origin {
            headers.insert(header::ORIGIN, origin.parse().unwrap());
        }
        headers
    }

    #[test]
    fn a_handshake_from_this_appliance_is_allowed() {
        let uri = Uri::from_static("/api/vms/100/console/ws");
        assert!(same_origin(
            &headers(Some("https://192.168.10.5:8443"), "192.168.10.5:8443"),
            &uri
        )
        .is_ok());
        // The scheme is not compared: an appliance reached over plain HTTP in
        // development is still the same origin as the page that opened it, and
        // the host and port are what identify it.
        assert!(same_origin(&headers(Some("http://lumen:8443"), "lumen:8443"), &uri).is_ok());
    }

    #[test]
    fn a_handshake_another_site_started_is_refused() {
        let uri = Uri::from_static("/api/vms/100/console/ws");
        let err = same_origin(
            &headers(Some("https://evil.example"), "192.168.10.5:8443"),
            &uri,
        )
        .unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)), "{err:?}");

        // A different port on the same host is a different origin, and the
        // browser treats it as one — so this does too.
        assert!(same_origin(
            &headers(Some("https://192.168.10.5:9000"), "192.168.10.5:8443"),
            &uri
        )
        .is_err());
    }

    /// No browser sent it, so there is no cookie to borrow and nothing to
    /// forge — `curl` and `websocat` have to keep working.
    #[test]
    fn a_handshake_with_no_origin_at_all_is_allowed() {
        let uri = Uri::from_static("/api/vms/100/console/ws");
        assert!(same_origin(&headers(None, "192.168.10.5:8443"), &uri).is_ok());
    }
}
