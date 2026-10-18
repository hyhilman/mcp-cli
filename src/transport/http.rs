//! Streamable HTTP transport: `POST /mcp`, JSON replies, SSE for
//! `subscriptions/listen`.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use serde::Serialize;
use subtle::ConstantTimeEq;

use crate::mcp::jsonrpc::{decode_message, JsonRpcError, JsonRpcErrorResponse, JsonRpcMessage};
use crate::mcp::protocol::{request_protocol_version, HEADER_MISMATCH, UNSUPPORTED_PROTOCOL_VERSION};
use crate::mcp::{McpSession, SessionReply};

const PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";

#[derive(Clone)]
struct AppState {
    session: Arc<McpSession>,
    /// `None` = no auth (loopback only, enforced by `run_http`).
    token: Option<Arc<str>>,
}

/// Serve until the listener fails. A token is required unless `addr` is
/// a loopback address.
pub async fn run_http(
    session: Arc<McpSession>,
    addr: SocketAddr,
    token: Option<String>,
) -> std::io::Result<()> {
    if token.is_none() && !addr.ip().is_loopback() {
        return Err(std::io::Error::other(
            "refusing to listen on a non-loopback address without MCP_CLI_TOKEN",
        ));
    }
    let app = router(session, token);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "http transport listening on /mcp");
    axum::serve(listener, app).await
}

fn router(session: Arc<McpSession>, token: Option<String>) -> Router {
    let state = AppState {
        session,
        token: token.map(Arc::from),
    };
    Router::new().route("/mcp", post(handle_mcp)).with_state(state)
}

fn authorized(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(expected) = &state.token else { return true };
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|given| given.as_bytes().ct_eq(expected.as_bytes()).into())
}

async fn handle_mcp(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if !authorized(&state, &headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let message = match decode_message(&body) {
        Ok(message) => message,
        Err(error) => return json_response(StatusCode::OK, &JsonRpcErrorResponse::new(None, error)),
    };

    // Modern (2026-07-28) requests declare their version in `_meta`; the
    // header must agree so intermediaries and the server see the same thing.
    let modern = match &message {
        JsonRpcMessage::Request(request) => {
            request_protocol_version(request.params.as_ref()).map(|v| (v, request.id.clone()))
        }
        _ => None,
    };
    if let Some((version, id)) = &modern {
        let header = headers.get(PROTOCOL_VERSION_HEADER).and_then(|v| v.to_str().ok());
        if header != Some(version.as_str()) {
            let error = JsonRpcError {
                code: HEADER_MISMATCH,
                message: format!(
                    "Header mismatch: MCP-Protocol-Version {:?} does not match body version '{version}'",
                    header.unwrap_or("<missing>")
                ),
                data: None,
            };
            return json_response(
                StatusCode::BAD_REQUEST,
                &JsonRpcErrorResponse::new(Some(id.clone()), error),
            );
        }
    }

    match state.session.handle_streaming(message).await {
        Some(SessionReply::Message(reply)) => {
            let status = if modern.is_some() {
                modern_status(&reply)
            } else {
                StatusCode::OK
            };
            json_response(status, &reply)
        }
        Some(SessionReply::Stream { messages, .. }) => sse_response(messages),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

/// Long-lived SSE response for `subscriptions/listen`. Closing the HTTP
/// connection drops the receiver, which ends the subscription.
fn sse_response(messages: tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>) -> Response {
    let stream = futures_util::stream::unfold(messages, |mut rx| async move {
        let message = rx.recv().await?;
        let data = serde_json::to_string(&message).unwrap_or_default();
        Some((Ok::<_, std::convert::Infallible>(Event::default().data(data)), rx))
    });
    Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
}

/// HTTP status for a modern reply, per the 2026-07-28 Streamable HTTP rules.
fn modern_status(reply: &JsonRpcMessage) -> StatusCode {
    match reply {
        JsonRpcMessage::ErrorResponse(err) => match err.error.code {
            UNSUPPORTED_PROTOCOL_VERSION => StatusCode::BAD_REQUEST,
            JsonRpcError::METHOD_NOT_FOUND => StatusCode::NOT_FOUND,
            _ => StatusCode::OK,
        },
        _ => StatusCode::OK,
    }
}

fn json_response<T: Serialize>(status: StatusCode, body: &T) -> Response {
    match serde_json::to_vec(body) {
        Ok(bytes) => (status, [(header::CONTENT_TYPE, "application/json")], bytes).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
