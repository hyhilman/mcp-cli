//! Tools: one per configured command, plus `<cli>_status`, `<cli>_login`
//! and `<cli>_login_complete` for CLIs with a health check / login.

use std::time::{Duration, Instant};

use serde_json::Value;

use super::protocol::RequestCtx;
use super::session::{error_result, structured_result, text_result, McpSession, ToolCallParams};
use crate::config::CliDef;
use crate::runner::{CommandDef, Output};
use crate::state::{CliState, LoginOutcome};

/// `inputRequests` key for the login elicitation.
const LOGIN_INPUT_KEY: &str = "login";
/// How long a `requestState` for a login stays valid.
const LOGIN_STATE_TTL_SECS: u64 = 900;

enum Tool<'a> {
    Command(&'a CliDef, &'a CommandDef),
    Status(&'a CliDef),
    Login(&'a CliDef),
    LoginComplete(&'a CliDef),
}

impl McpSession {
    fn find_tool(&self, name: &str) -> Option<Tool<'_>> {
        for cli in &self.config.clis {
            if let Some(cmd) = cli.commands.iter().find(|c| c.tool == name) {
                return Some(Tool::Command(cli, cmd));
            }
            let Some(rest) = name.strip_prefix(&cli.name).and_then(|r| r.strip_prefix('_')) else {
                continue;
            };
            let Some(state) = &cli.state else { continue };
            let paste_back = state.login.as_ref().is_some_and(|l| !l.callback_ports.is_empty());
            match rest {
                "status" => return Some(Tool::Status(cli)),
                "login" if state.login.is_some() => return Some(Tool::Login(cli)),
                "login_complete" if paste_back => return Some(Tool::LoginComplete(cli)),
                _ => {}
            }
        }
        None
    }

    pub(crate) fn tool_descriptors(&self) -> Vec<Value> {
        let no_args =
            serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false });
        let mut tools = Vec::new();
        for cli in &self.config.clis {
            let label = cli.description.as_deref().unwrap_or(&cli.name);
            for cmd in &cli.commands {
                let mut tool = serde_json::json!({
                    "name": cmd.tool,
                    "description": format!("[{label}] {}", cmd.description),
                    "inputSchema": cmd.input_schema(),
                });
                if cmd.read_only {
                    tool["annotations"] = serde_json::json!({ "readOnlyHint": true });
                }
                tools.push(tool);
            }
            let Some(state) = &cli.state else { continue };
            let n = &cli.name;
            tools.push(serde_json::json!({
                "name": format!("{n}_status"),
                "description": format!(
                    "[{label}] Run the health check now and report the state (ready / needs_login / \
                     down), login expiry, and any login waiting for the user."
                ),
                "inputSchema": no_args,
                "annotations": { "readOnlyHint": true },
            }));
            let Some(login) = &state.login else { continue };
            tools.push(serde_json::json!({
                "name": format!("{n}_login"),
                "description": format!(
                    "[{label}] Start the SSO login and return the login link. Show the link to the \
                     user and ask them to sign in; do not open, fetch, or share it anywhere else. \
                     Then call {n}_status until the state is 'ready'."
                ),
                "inputSchema": no_args,
            }));
            if !login.callback_ports.is_empty() {
                tools.push(serde_json::json!({
                    "name": format!("{n}_login_complete"),
                    "description": format!(
                        "[{label}] Finish a login whose browser redirect ended on a \
                         http://localhost:<port>/... page that failed to load. Pass the full address \
                         the user copied from the browser's address bar. Only works while a login \
                         started by {n}_login is waiting."
                    ),
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "callback_url": {
                                "type": "string",
                                "description": "The full http://localhost:<port>/... address.",
                            },
                        },
                        "required": ["callback_url"],
                        "additionalProperties": false,
                    },
                }));
            }
        }
        tools
    }

    /// Call a tool outside any MCP request (the `mcp-cli call` command).
    pub(crate) async fn call_tool_direct(&self, params: &ToolCallParams) -> Value {
        self.call_tool(params, &RequestCtx::default()).await
    }

    /// Always returns a `CallToolResult` (or `input_required`); failures are
    /// reported with `isError: true`.
    pub(crate) async fn call_tool(&self, params: &ToolCallParams, ctx: &RequestCtx) -> Value {
        let Some(tool) = self.find_tool(&params.name) else {
            return error_result(format!("no such tool: {}", params.name));
        };
        let args = params.arguments.as_ref();
        match tool {
            Tool::Command(cli, cmd) => self.call_command(cli, cmd, params, ctx).await,
            Tool::Status(cli) => {
                if let Err(e) = no_arguments(args) {
                    return error_result(e);
                }
                match self.states.refresh(&cli.name).await {
                    Ok(status) => {
                        let text = serde_json::to_string_pretty(&status).unwrap_or_default();
                        structured_result(text, serde_json::to_value(&status).unwrap_or_default())
                    }
                    Err(e) => error_result(e.to_string()),
                }
            }
            Tool::Login(cli) => {
                if let Err(e) = no_arguments(args) {
                    return error_result(e);
                }
                self.login_tool(cli).await
            }
            Tool::LoginComplete(cli) => self.login_complete_tool(cli, args).await,
        }
    }

    async fn call_command(
        &self,
        cli: &CliDef,
        cmd: &CommandDef,
        params: &ToolCallParams,
        ctx: &RequestCtx,
    ) -> Value {
        let argv = match cmd.argv(params.arguments.as_ref()) {
            Ok(argv) => argv,
            Err(e) => return error_result(e),
        };

        if cmd.needs_login {
            if let Some(blocked) = self.login_gate(cli, params, ctx).await {
                return blocked;
            }
        }

        let output = match cli
            .program
            .run(&argv, Duration::from_secs(cmd.timeout_secs))
            .await
        {
            Ok(output) => output,
            Err(e) => {
                tracing::warn!(tool = %cmd.tool, error = %e, "command failed to run");
                return error_result(e);
            }
        };
        tracing::info!(
            tool = %cmd.tool,
            args = ?argv,
            exit = ?output.exit_code,
            ms = output.duration_ms,
            "command finished"
        );

        // The login may have expired since the last health check.
        if !output.success && params.request_state.is_none() {
            if let Some(state) = cli
                .state
                .as_ref()
                .filter(|s| s.output_needs_login(&output.combined()))
            {
                if let Ok(status) = self.states.refresh(&state.name).await {
                    if status.state == CliState::NeedsLogin {
                        if let Some(blocked) = self.login_gate(cli, params, ctx).await {
                            return blocked;
                        }
                    }
                }
            }
        }
        command_result(&output)
    }

    /// `None` when the call may run; otherwise the result to send back
    /// (a login prompt or an error).
    async fn login_gate(&self, cli: &CliDef, params: &ToolCallParams, ctx: &RequestCtx) -> Option<Value> {
        let status = self.states.status(&cli.name)?;
        // Unknown = not checked yet: let it through.
        if matches!(status.state, CliState::Ready | CliState::Unknown) {
            return None;
        }
        if !cli.has_login() {
            return Some(error_result(format!(
                "UNAVAILABLE: '{}' is {}{}. Ask the user to fix it on the server host, then retry.",
                cli.name,
                status.state,
                status.detail.map(|d| format!(" ({d})")).unwrap_or_default(),
            )));
        }
        if ctx.is_modern() && ctx.supports_url_elicitation() {
            return self.login_round_trip(cli, params).await;
        }
        Some(error_result(format!(
            "LOGIN_REQUIRED: '{}' is {}. {}",
            cli.name,
            status.state,
            login_instructions(&cli.name, status.login.is_some()),
        )))
    }

    /// Multi round-trip login (2026-07-28 + URL elicitation): instead of an
    /// error, ask the client to show the login link to the user, then let
    /// the retried call through once the CLI is ready.
    async fn login_round_trip(&self, cli: &CliDef, params: &ToolCallParams) -> Option<Value> {
        // A retry: the user has seen the link (or declined it).
        if let Some(token) = &params.request_state {
            if !self.state_signer.verify(token, &cli.name, &params.name) {
                return Some(error_result(
                    "requestState is invalid or expired; call the tool again without it",
                ));
            }
            let action = params
                .input_responses
                .as_ref()
                .and_then(|r| r.get(LOGIN_INPUT_KEY))
                .and_then(|r| r.get("action"))
                .and_then(Value::as_str);
            if matches!(action, Some("decline") | Some("cancel")) {
                return Some(error_result(format!(
                    "The user declined to sign in to '{}'; '{}' cannot run without it.",
                    cli.name, params.name
                )));
            }
            if self.wait_until_ready(&cli.name).await {
                return None;
            }
        }

        match self.states.start_login(&cli.name).await {
            Ok(LoginOutcome::AlreadyReady { .. }) => None,
            Ok(LoginOutcome::Pending { login }) => {
                let mut message = format!(
                    "Sign in to '{}' so '{}' can run. The login stays open for {} minutes.",
                    cli.name,
                    params.name,
                    login.timeout_at_unix.saturating_sub(login.started_at_unix) / 60,
                );
                // Keep device codes etc., but no other links in the message.
                let extra: Vec<&str> = login.message.lines().filter(|l| !l.contains("://")).collect();
                if !extra.is_empty() {
                    message.push('\n');
                    message.push_str(&extra.join("\n"));
                }
                Some(serde_json::json!({
                    "resultType": "input_required",
                    "inputRequests": {
                        LOGIN_INPUT_KEY: {
                            "method": "elicitation/create",
                            "params": { "mode": "url", "url": login.url, "message": message },
                        },
                    },
                    "requestState": self.state_signer.issue(&cli.name, &params.name, LOGIN_STATE_TTL_SECS),
                }))
            }
            Err(e) => Some(error_result(format!(
                "LOGIN_REQUIRED: '{}' needs a login, and starting it failed: {e}",
                cli.name
            ))),
        }
    }

    /// Wait (bounded) for the CLI to report ready.
    async fn wait_until_ready(&self, name: &str) -> bool {
        let deadline = Instant::now() + self.login_wait;
        loop {
            if self
                .states
                .status(name)
                .is_some_and(|s| s.state == CliState::Ready)
            {
                return true;
            }
            if Instant::now() >= deadline {
                return self
                    .states
                    .refresh(name)
                    .await
                    .is_ok_and(|s| s.state == CliState::Ready);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn login_tool(&self, cli: &CliDef) -> Value {
        let name = &cli.name;
        match self.states.start_login(name).await {
            Ok(LoginOutcome::AlreadyReady { status }) => structured_result(
                format!("'{name}' is already ready. Retry the original tool."),
                serde_json::json!({ "result": "already_ready", "status": status }),
            ),
            Ok(LoginOutcome::Pending { login }) => {
                // The link is shown once; keep the other lines (device codes etc.).
                let message: Vec<&str> = login.message.lines().filter(|l| !l.contains("://")).collect();
                let mut text = format!(
                    "Login link for '{name}': {url}\n\n{message}\n\n\
                     Show this link to the user and ask them to sign in. Do not open or share it \
                     anywhere else. The login stays open for {mins} minutes. Afterwards call \
                     {name}_status until the state is 'ready', then retry the original tool.",
                    url = login.url,
                    message = message.join("\n"),
                    mins = login.timeout_at_unix.saturating_sub(login.started_at_unix) / 60,
                );
                if self.states.supports_paste_back(name) {
                    text.push_str(&format!(
                        " If the browser ends on a http://localhost:... page that does not load, \
                         ask the user to copy that full address and pass it to {name}_login_complete."
                    ));
                }
                structured_result(text, serde_json::json!({ "result": "pending", "login": login }))
            }
            Err(e) => error_result(e.to_string()),
        }
    }

    async fn login_complete_tool(&self, cli: &CliDef, args: Option<&Value>) -> Value {
        let Some(Value::Object(obj)) = args else {
            return error_result("arguments must include 'callback_url'");
        };
        if let Some(extra) = obj.keys().find(|k| k.as_str() != "callback_url") {
            return error_result(format!("unexpected argument '{extra}'"));
        }
        let Some(callback_url) = obj.get("callback_url").and_then(Value::as_str) else {
            return error_result("argument 'callback_url' must be a string");
        };
        let name = &cli.name;
        match self.states.complete_login(name, callback_url).await {
            Ok(status) => structured_result(
                format!(
                    "Callback delivered to the '{name}' login ({status}). Call {name}_status until \
                     the state is 'ready', then retry the original tool."
                ),
                serde_json::json!({ "result": "delivered", "http_status": status }),
            ),
            Err(e) => error_result(e.to_string()),
        }
    }
}

fn no_arguments(args: Option<&Value>) -> Result<(), String> {
    match args {
        None | Some(Value::Null) => Ok(()),
        Some(Value::Object(obj)) if obj.is_empty() => Ok(()),
        Some(_) => Err("this tool takes no arguments".into()),
    }
}

fn login_instructions(cli: &str, pending: bool) -> String {
    if pending {
        format!(
            "A login is already waiting for the user: call {cli}_login to get the link again, \
             then call {cli}_status and retry once it is 'ready'."
        )
    } else {
        format!(
            "Call {cli}_login, show the returned link to the user, call {cli}_status until the \
             state is 'ready', then retry this tool."
        )
    }
}

fn command_result(output: &Output) -> Value {
    let mut text = output.stdout.trim_end().to_string();
    let stderr = output.stderr.trim_end();
    if !stderr.is_empty() {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str("[stderr]\n");
        text.push_str(stderr);
    }
    if output.truncated {
        text.push_str("\n\n[output truncated]");
    }
    if !output.success {
        let code = output.exit_code.map_or("signal".to_string(), |c| c.to_string());
        text = format!("exit code {code}\n\n{text}");
    }
    if text.is_empty() {
        text = "(no output)".into();
    }
    text_result(text, !output.success)
}
