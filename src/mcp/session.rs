use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use super::jsonrpc::{
    JsonRpcError, JsonRpcErrorResponse, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest,
    JsonRpcSuccessResponse,
};
use super::protocol::{
    add_cache_hints, mark_complete, RequestCtx, LEGACY_PROTOCOL_VERSION, META_SERVER_INFO,
    MODERN_PROTOCOL_VERSIONS,
};
use super::request_state::StateSigner;
use crate::config::Config;
use crate::events::EventHub;
use crate::state::StateManager;

/// Guidance for the model, sent in `initialize` / `server/discover`.
const INSTRUCTIONS: &str = "Each tool runs one fixed, pre-approved command; arguments are \
limited to the listed values. Some tools need an SSO login. If a tool returns LOGIN_REQUIRED, \
do not retry blindly: call the <cli>_login tool it names, show the returned link to the user, \
wait until <cli>_status reports 'ready', then retry. Never open, fetch, or forward login links \
yourself.";

/// A change that `subscriptions/listen` streams can be told about.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Change {
    ResourceUpdated(String),
}

/// Transport-agnostic MCP request/notification dispatch.
pub struct McpSession {
    pub(crate) config: Arc<Config>,
    pub(crate) states: Arc<StateManager>,
    initialized: AtomicBool,
    pub(crate) changes: tokio::sync::broadcast::Sender<Change>,
    pub(crate) state_signer: StateSigner,
    /// How long a retried call waits for a pending login to finish.
    pub(crate) login_wait: Duration,
    events: Option<Arc<EventHub>>,
}

impl McpSession {
    pub fn new(config: Arc<Config>, states: Arc<StateManager>) -> Self {
        let session = Self {
            login_wait: Duration::from_secs(config.login_wait_secs),
            config,
            states,
            initialized: AtomicBool::new(false),
            changes: tokio::sync::broadcast::channel(64).0,
            state_signer: StateSigner::new(),
            events: None,
        };
        session.forward_state_changes();
        session
    }

    /// Serve MCP Events (`events/list`, `events/subscribe`, `events/unsubscribe`).
    pub fn with_events(mut self, hub: Arc<EventHub>) -> Self {
        self.events = Some(hub);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_login_wait(mut self, wait: Duration) -> Self {
        self.login_wait = wait;
        self
    }

    /// Handles one incoming message. Returns `Some` for a request (always
    /// reply, success or error) and `None` for a notification or response.
    ///
    /// `subscriptions/listen` needs a streaming transport; use
    /// [`McpSession::handle_streaming`] for that.
    #[cfg(test)]
    pub async fn handle(&self, message: JsonRpcMessage) -> Option<JsonRpcMessage> {
        match message {
            JsonRpcMessage::Request(request) => Some(self.handle_request(request).await),
            JsonRpcMessage::Notification(notification) => {
                self.handle_notification(notification);
                None
            }
            JsonRpcMessage::SuccessResponse(_) | JsonRpcMessage::ErrorResponse(_) => None,
        }
    }

    pub(crate) async fn handle_request(&self, request: JsonRpcRequest) -> JsonRpcMessage {
        match self.dispatch_request(&request).await {
            Ok(value) => JsonRpcMessage::SuccessResponse(JsonRpcSuccessResponse::new(request.id, value)),
            Err(error) => {
                tracing::debug!(method = %request.method, code = error.code, "request failed");
                JsonRpcMessage::ErrorResponse(JsonRpcErrorResponse::new(Some(request.id), error))
            }
        }
    }

    async fn dispatch_request(&self, request: &JsonRpcRequest) -> Result<Value, JsonRpcError> {
        let ctx = RequestCtx::from_params(request.params.as_ref());
        ctx.check_version()?;
        let params = request.params.as_ref();

        let mut result = match request.method.as_str() {
            "server/discover" => Ok(self.handle_discover()),
            "initialize" if !ctx.is_modern() => Ok(self.handle_initialize()),
            "ping" => Ok(serde_json::json!({})),
            method => {
                self.require_initialized(&ctx)?;
                match method {
                    "tools/list" => Ok(serde_json::json!({ "tools": self.tool_descriptors() })),
                    "tools/call" => self.handle_tools_call(params, &ctx).await,
                    "resources/list" => Ok(self.handle_resources_list()),
                    "resources/read" => self.handle_resources_read(params),
                    "events/list" | "events/subscribe" | "events/unsubscribe" if self.events.is_some() => {
                        self.handle_events(method, params).await
                    }
                    "subscriptions/listen" => Err(JsonRpcError::invalid_request(
                        "subscriptions/listen needs a streaming transport",
                    )),
                    other => Err(JsonRpcError::method_not_found(format!("unknown method: {other}"))),
                }
            }
        };
        if ctx.is_modern() {
            if let Ok(value) = &mut result {
                mark_complete(value);
                add_cache_hints(&request.method, value);
            }
        }
        result
    }

    fn capabilities(&self) -> Value {
        let mut caps = serde_json::json!({ "tools": { "listChanged": false } });
        if !self.states.is_empty() {
            caps["resources"] = serde_json::json!({ "subscribe": true, "listChanged": false });
        }
        if self.events.is_some() {
            caps["events"] = serde_json::json!({});
        }
        caps
    }

    fn instructions(&self) -> String {
        match &self.config.instructions {
            Some(extra) => format!("{INSTRUCTIONS}\n\n{extra}"),
            None => INSTRUCTIONS.to_string(),
        }
    }

    fn server_info(&self) -> Value {
        serde_json::json!({ "name": self.config.server_name, "version": env!("CARGO_PKG_VERSION") })
    }

    async fn handle_events(&self, method: &str, params: Option<&Value>) -> Result<Value, JsonRpcError> {
        let hub = self
            .events
            .as_ref()
            .ok_or_else(|| JsonRpcError::method_not_found(method))?;
        match method {
            "events/list" => Ok(hub.list()),
            "events/subscribe" => hub.subscribe(params).await,
            _ => hub.unsubscribe(params),
        }
    }

    fn handle_discover(&self) -> Value {
        serde_json::json!({
            "supportedVersions": MODERN_PROTOCOL_VERSIONS,
            "capabilities": self.capabilities(),
            "_meta": { META_SERVER_INFO: self.server_info() },
            "instructions": self.instructions(),
        })
    }

    fn handle_initialize(&self) -> Value {
        self.initialized.store(true, Ordering::SeqCst);
        serde_json::json!({
            "protocolVersion": LEGACY_PROTOCOL_VERSION,
            "serverInfo": self.server_info(),
            "capabilities": self.capabilities(),
            "instructions": self.instructions(),
        })
    }

    /// Legacy requests need `initialize` first; modern requests are stateless.
    fn require_initialized(&self, ctx: &RequestCtx) -> Result<(), JsonRpcError> {
        if ctx.is_modern() || self.initialized.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(JsonRpcError::invalid_request(
                "session is not initialized; call \"initialize\" first",
            ))
        }
    }

    async fn handle_tools_call(
        &self,
        params: Option<&Value>,
        ctx: &RequestCtx,
    ) -> Result<Value, JsonRpcError> {
        let params: ToolCallParams = match params {
            Some(p) => serde_json::from_value(p.clone())
                .map_err(|e| JsonRpcError::invalid_params(format!("invalid \"tools/call\" params: {e}")))?,
            None => return Err(JsonRpcError::invalid_params("\"tools/call\" requires params")),
        };
        Ok(self.call_tool(&params, ctx).await)
    }

    pub(crate) fn handle_notification(&self, notification: JsonRpcNotification) {
        tracing::debug!(method = %notification.method, "notification");
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ToolCallParams {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) arguments: Option<Value>,
    /// Multi round-trip retry: echoed `requestState`.
    #[serde(default, rename = "requestState")]
    pub(crate) request_state: Option<String>,
    /// Multi round-trip retry: answers to our `inputRequests`.
    #[serde(default, rename = "inputResponses")]
    pub(crate) input_responses: Option<Value>,
}

pub(crate) fn text_result(text: String, is_error: bool) -> Value {
    serde_json::json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

pub(crate) fn structured_result(text: String, structured: Value) -> Value {
    serde_json::json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false,
        "structuredContent": structured,
    })
}

pub(crate) fn error_result(message: impl Into<String>) -> Value {
    text_result(message.into(), true)
}
