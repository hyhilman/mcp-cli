//! `subscriptions/listen` streams and the `cli://<name>/state` resources
//! they can watch.

use serde_json::Value;
use tokio::sync::{broadcast, mpsc};

use super::jsonrpc::{JsonRpcError, JsonRpcErrorResponse, JsonRpcId, JsonRpcMessage, JsonRpcNotification};
use super::protocol::{RequestCtx, META_SUBSCRIPTION_ID};
use super::session::{Change, McpSession};
use crate::state::StateEvent;

/// What a transport should send back for one incoming message.
pub enum SessionReply {
    /// A single response.
    Message(JsonRpcMessage),
    /// A long-lived stream (`subscriptions/listen`): forward every message
    /// until the receiver ends, or drop the receiver to cancel.
    Stream {
        id: JsonRpcId,
        messages: mpsc::UnboundedReceiver<JsonRpcMessage>,
    },
}

pub fn state_uri(cli: &str) -> String {
    format!("cli://{cli}/state")
}

impl McpSession {
    /// Like [`McpSession::handle`], but `subscriptions/listen` returns a
    /// [`SessionReply::Stream`] instead of an error.
    pub async fn handle_streaming(&self, message: JsonRpcMessage) -> Option<SessionReply> {
        match message {
            JsonRpcMessage::Request(request) if request.method == "subscriptions/listen" => {
                let ctx = RequestCtx::from_params(request.params.as_ref());
                match self.listen(&request.id, request.params.as_ref(), &ctx) {
                    Ok(messages) => Some(SessionReply::Stream {
                        id: request.id,
                        messages,
                    }),
                    Err(error) => Some(SessionReply::Message(JsonRpcMessage::ErrorResponse(
                        JsonRpcErrorResponse::new(Some(request.id), error),
                    ))),
                }
            }
            JsonRpcMessage::Request(request) => {
                Some(SessionReply::Message(self.handle_request(request).await))
            }
            JsonRpcMessage::Notification(notification) => {
                self.handle_notification(notification);
                None
            }
            JsonRpcMessage::SuccessResponse(_) | JsonRpcMessage::ErrorResponse(_) => None,
        }
    }

    fn listen(
        &self,
        id: &JsonRpcId,
        params: Option<&Value>,
        ctx: &RequestCtx,
    ) -> Result<mpsc::UnboundedReceiver<JsonRpcMessage>, JsonRpcError> {
        ctx.check_version()?;
        if !ctx.is_modern() {
            return Err(JsonRpcError::invalid_request(
                "subscriptions/listen requires protocol version 2026-07-28",
            ));
        }
        let uris: Vec<String> = params
            .and_then(|p| p.get("notifications"))
            .and_then(|n| n.get("resourceSubscriptions"))
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .filter(|uri| self.state_resource(uri).is_some())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        // Acknowledge only what we will actually send. The tool list is
        // fixed by the config, so `toolsListChanged` is never agreed.
        let mut agreed = serde_json::Map::new();
        if !uris.is_empty() {
            agreed.insert("resourceSubscriptions".into(), serde_json::json!(uris));
        }

        let sub_id = serde_json::to_value(id).unwrap_or(Value::Null);
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(notification(
            "notifications/subscriptions/acknowledged",
            &sub_id,
            serde_json::json!({ "notifications": agreed }),
        ));

        let mut changes = self.changes.subscribe();
        tokio::spawn(async move {
            loop {
                let change = match changes.recv().await {
                    Ok(change) => change,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let Change::ResourceUpdated(uri) = change;
                if !uris.contains(&uri) {
                    continue;
                }
                let message = notification(
                    "notifications/resources/updated",
                    &sub_id,
                    serde_json::json!({ "uri": uri }),
                );
                if tx.send(message).is_err() {
                    break; // client went away
                }
            }
        });
        Ok(rx)
    }

    pub(crate) fn handle_resources_list(&self) -> Value {
        let resources: Vec<Value> = self
            .states
            .names()
            .iter()
            .map(|name| {
                serde_json::json!({
                    "uri": state_uri(name),
                    "name": format!("{name}-state"),
                    "title": format!("'{name}' state"),
                    "description": "Health state, login expiry, and any login waiting for the user.",
                    "mimeType": "application/json",
                })
            })
            .collect();
        serde_json::json!({ "resources": resources })
    }

    pub(crate) fn handle_resources_read(&self, params: Option<&Value>) -> Result<Value, JsonRpcError> {
        let uri = params
            .and_then(|p| p.get("uri"))
            .and_then(Value::as_str)
            .ok_or_else(|| JsonRpcError::invalid_params("resources/read requires 'uri'"))?;
        let status = self
            .state_resource(uri)
            .and_then(|name| self.states.status(&name))
            .ok_or_else(|| JsonRpcError::invalid_params(format!("resource not found: {uri}")))?;
        let text = serde_json::to_string_pretty(&status).unwrap_or_default();
        Ok(serde_json::json!({
            "contents": [{ "uri": uri, "mimeType": "application/json", "text": text }],
        }))
    }

    /// CLI name for a `cli://<name>/state` URI of a CLI with a health check.
    fn state_resource(&self, uri: &str) -> Option<String> {
        let name = uri.strip_prefix("cli://")?.strip_suffix("/state")?;
        self.states.status(name).map(|_| name.to_string())
    }

    /// Turn state changes into `cli://<name>/state` resource updates.
    pub(crate) fn forward_state_changes(&self) {
        if self.states.is_empty() {
            return;
        }
        let changes = self.changes.clone();
        let mut events = self.states.subscribe();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(StateEvent::StateChanged { name, .. }) => {
                        let _ = changes.send(Change::ResourceUpdated(state_uri(&name)));
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }
}

fn notification(method: &str, sub_id: &Value, params: Value) -> JsonRpcMessage {
    let mut params = params;
    params["_meta"] = serde_json::json!({ META_SUBSCRIPTION_ID: sub_id });
    JsonRpcMessage::Notification(JsonRpcNotification::new(method, Some(params)))
}
