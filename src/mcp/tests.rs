//! Session-level tests: protocol, tools, login gate, resources, events.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use super::jsonrpc::{JsonRpcError, JsonRpcId, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest};
use super::McpSession;
use crate::config::{CliDef, Config};
use crate::runner::{ArgSpec, ArgType, CommandDef};
use crate::state::{sh_def, StateManager};

fn command(tool: &str, script: &str, needs_login: bool) -> CommandDef {
    CommandDef {
        tool: tool.into(),
        description: format!("run {tool}"),
        run: vec!["-c".into(), script.into()],
        args: vec![],
        timeout_secs: 5,
        needs_login,
        read_only: true,
    }
}

/// A `netbird` CLI (sh scripts) with commands:
/// - `netbird_peers`: prints "peer-a", needs login
/// - `netbird_slow`: sleeps 1s
/// - `netbird_echo`: echoes its `word` argument (enum)
/// - `netbird_expired`: fails with "NeedsLogin" in its output
pub(crate) fn session_for(health: &str, login: &str) -> McpSession {
    session_with(sh_def("netbird", health, login))
}

fn session_with(state: crate::state::StateDef) -> McpSession {
    let mut echo = command("netbird_echo", "echo \"$0\"", false);
    echo.run.push("{word}".into());
    echo.args.push(ArgSpec {
        name: "word".into(),
        description: None,
        kind: ArgType::Enum(vec!["hello".into(), "bye".into()]),
        default: None,
    });
    let cli = CliDef {
        name: "netbird".into(),
        description: Some("NetBird".into()),
        program: state.program.clone(),
        state: Some(state.clone()),
        auto_login: false,
        commands: vec![
            command("netbird_peers", "echo peer-a", true),
            command("netbird_slow", "sleep 1; echo slow", false),
            echo,
            command(
                "netbird_expired",
                "echo 'Daemon status: NeedsLogin' >&2; exit 1",
                true,
            ),
        ],
    };
    let config = Config {
        server_name: "mcp-cli".into(),
        instructions: None,
        login_wait_secs: 5,
        clis: vec![cli],
        notify: vec![],
        events: None,
    };
    let states = StateManager::new(vec![state]);
    McpSession::new(Arc::new(config), states).with_login_wait(Duration::from_secs(5))
}

fn request(id: i64, method: &str, params: Option<Value>) -> JsonRpcMessage {
    JsonRpcMessage::Request(JsonRpcRequest::new(JsonRpcId::Number(id), method, params))
}

fn modern(extra: Value, url_elicitation: bool) -> Value {
    let caps = if url_elicitation {
        json!({ "elicitation": { "url": {} } })
    } else {
        json!({})
    };
    let mut params = json!({
        "_meta": {
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": caps,
        }
    });
    if let (Value::Object(p), Value::Object(e)) = (&mut params, extra) {
        p.extend(e);
    }
    params
}

async fn ok(session: &McpSession, message: JsonRpcMessage) -> Value {
    match session.handle(message).await {
        Some(JsonRpcMessage::SuccessResponse(r)) => r.result,
        other => panic!("expected success, got {other:?}"),
    }
}

async fn err(session: &McpSession, message: JsonRpcMessage) -> JsonRpcError {
    match session.handle(message).await {
        Some(JsonRpcMessage::ErrorResponse(r)) => r.error,
        other => panic!("expected error, got {other:?}"),
    }
}

async fn initialized(session: McpSession) -> McpSession {
    ok(&session, request(1, "initialize", None)).await;
    session
}

async fn call(session: &McpSession, name: &str, args: Value) -> Value {
    ok(
        session,
        request(9, "tools/call", Some(json!({ "name": name, "arguments": args }))),
    )
    .await
}

fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap_or_default()
}

// ── protocol ───────────────────────────────────────────────────────────

#[tokio::test]
async fn legacy_requests_need_initialize() {
    let session = session_for("true", "true");
    let e = err(&session, request(1, "tools/list", None)).await;
    assert_eq!(e.code, JsonRpcError::INVALID_REQUEST);
    let session = initialized(session).await;
    let e = err(&session, request(2, "nope/nope", None)).await;
    assert_eq!(e.code, JsonRpcError::METHOD_NOT_FOUND);
}

#[tokio::test]
async fn discover_and_stateless_modern_requests() {
    let session = session_for("true", "true");
    let r = ok(
        &session,
        request(1, "server/discover", Some(modern(json!({}), false))),
    )
    .await;
    assert_eq!(r["supportedVersions"], json!(["2026-07-28"]));
    assert_eq!(r["resultType"], "complete");
    assert_eq!(
        r["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "mcp-cli"
    );
    assert!(r["capabilities"]["resources"]["subscribe"].as_bool().unwrap());

    assert_eq!(r["ttlMs"], 300_000);
    assert_eq!(r["cacheScope"], "public");

    let r = ok(&session, request(2, "tools/list", Some(modern(json!({}), false)))).await;
    assert_eq!(r["resultType"], "complete");
    assert_eq!(r["ttlMs"], 300_000);
    assert_eq!(r["cacheScope"], "public");

    let params = json!({ "_meta": { "io.modelcontextprotocol/protocolVersion": "1900-01-01" } });
    let e = err(&session, request(3, "tools/list", Some(params))).await;
    assert_eq!(e.code, super::protocol::UNSUPPORTED_PROTOCOL_VERSION);
    assert_eq!(e.data.unwrap()["supported"], json!(["2026-07-28"]));
}

#[tokio::test]
async fn notification_never_produces_a_reply() {
    let session = session_for("true", "true");
    let n = JsonRpcMessage::Notification(JsonRpcNotification::new("notifications/initialized", None));
    assert!(session.handle(n).await.is_none());
}

// ── tools ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn lists_commands_and_builtin_tools() {
    let session = initialized(session_for("true", "true")).await;
    let r = ok(&session, request(2, "tools/list", None)).await;
    let names: Vec<&str> = r["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "netbird_peers",
            "netbird_slow",
            "netbird_echo",
            "netbird_expired",
            "netbird_status",
            "netbird_login"
        ]
    );
    let echo = &r["tools"][2];
    assert_eq!(
        echo["inputSchema"]["properties"]["word"]["enum"],
        json!(["hello", "bye"])
    );
    assert_eq!(echo["annotations"]["readOnlyHint"], true);
    assert!(echo["description"].as_str().unwrap().starts_with("[NetBird]"));
}

#[tokio::test]
async fn command_runs_with_validated_arguments() {
    let session = initialized(session_for("echo 'Management: Connected'", "true")).await;
    let r = call(&session, "netbird_echo", json!({ "word": "hello" })).await;
    assert_eq!(r["isError"], false, "{r}");
    assert_eq!(text(&r), "hello");

    for bad in [
        json!({ "word": "rm -rf /" }),
        json!({}),
        json!({ "word": "hello", "x": 1 }),
    ] {
        let r = call(&session, "netbird_echo", bad).await;
        assert_eq!(r["isError"], true);
    }
    let r = call(&session, "netbird_status", json!({ "cli": "other" })).await;
    assert_eq!(r["isError"], true);
    let r = call(&session, "sh", json!({})).await;
    assert!(text(&r).contains("no such tool"));
}

#[tokio::test]
async fn status_tool_runs_health_check() {
    let session = initialized(session_for("echo 'Management: Connected'", "true")).await;
    let r = call(&session, "netbird_status", json!({})).await;
    assert_eq!(r["structuredContent"]["state"], "ready", "{r}");
}

// ── login gate ─────────────────────────────────────────────────────────

#[tokio::test]
async fn call_is_blocked_while_login_needed() {
    let session = initialized(session_for("echo NeedsLogin", "true")).await;
    session.states.refresh("netbird").await.unwrap();

    let r = call(&session, "netbird_peers", json!({})).await;
    assert_eq!(r["isError"], true);
    assert!(text(&r).starts_with("LOGIN_REQUIRED"), "{r}");
    assert!(text(&r).contains("netbird_login"), "{r}");

    // Commands with needs_login = false still run.
    let r = call(&session, "netbird_echo", json!({ "word": "bye" })).await;
    assert_eq!(r["isError"], false);
}

#[tokio::test]
async fn expired_login_detected_from_command_output() {
    // Health says ready at first; the command output says otherwise, and
    // the re-check agrees.
    let dir = std::env::temp_dir().join(format!("mcp-cli-expired-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let flag = dir.join("expired");
    let _ = std::fs::remove_file(&flag);
    let health = format!(
        "test -f {} && echo NeedsLogin || echo 'Management: Connected'",
        flag.display()
    );
    let session = initialized(session_for(&health, "true")).await;
    session.states.refresh("netbird").await.unwrap();
    std::fs::write(&flag, "").unwrap();

    let r = call(&session, "netbird_expired", json!({})).await;
    assert!(text(&r).starts_with("LOGIN_REQUIRED"), "{r}");
    assert_eq!(
        session.states.status("netbird").unwrap().state.to_string(),
        "needs_login"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

async fn call_modern(session: &McpSession, params: Value) -> Value {
    ok(session, request(20, "tools/call", Some(modern(params, true)))).await
}

#[tokio::test]
async fn blocked_call_round_trips_through_url_elicitation() {
    let dir = std::env::temp_dir().join(format!("mcp-cli-mrtr-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let flag = dir.join("ok");
    let _ = std::fs::remove_file(&flag);
    let flag = flag.display().to_string();
    let session = session_for(
        &format!("test -f {flag} && echo 'Management: Connected' || echo NeedsLogin"),
        &format!("echo 'login: https://idp.example/device'; echo 'Code: WXYZ'; sleep 1; touch {flag}"),
    );
    session.states.refresh("netbird").await.unwrap();

    let first = call_modern(&session, json!({ "name": "netbird_peers" })).await;
    assert_eq!(first["resultType"], "input_required", "{first}");
    let elicit = &first["inputRequests"]["login"];
    assert_eq!(elicit["method"], "elicitation/create");
    assert_eq!(elicit["params"]["mode"], "url");
    assert_eq!(elicit["params"]["url"], "https://idp.example/device");
    assert!(elicit["params"]["message"]
        .as_str()
        .unwrap()
        .contains("Code: WXYZ"));
    let state = first["requestState"].as_str().unwrap().to_string();

    // Tampered state, or state for another tool, is rejected.
    for (name, token) in [
        ("netbird_peers", format!("{state}x")),
        ("netbird_expired", state.clone()),
    ] {
        let bad = call_modern(
            &session,
            json!({
                "name": name, "requestState": token,
                "inputResponses": { "login": { "action": "accept" } }
            }),
        )
        .await;
        assert_eq!(bad["isError"], true, "{bad}");
    }

    // Accepted: the retry waits for the login, then runs the command.
    let done = call_modern(
        &session,
        json!({
            "name": "netbird_peers", "requestState": state,
            "inputResponses": { "login": { "action": "accept" } }
        }),
    )
    .await;
    assert_eq!(done["isError"], false, "{done}");
    assert_eq!(text(&done), "peer-a");
    assert_eq!(done["resultType"], "complete");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn declined_login_returns_error() {
    let session = session_for("echo NeedsLogin", "echo https://idp.example/x; sleep 5");
    session.states.refresh("netbird").await.unwrap();

    let first = call_modern(&session, json!({ "name": "netbird_peers" })).await;
    let state = first["requestState"].as_str().unwrap();
    let declined = call_modern(
        &session,
        json!({
            "name": "netbird_peers", "requestState": state,
            "inputResponses": { "login": { "action": "decline" } }
        }),
    )
    .await;
    assert_eq!(declined["isError"], true);
    assert!(text(&declined).contains("declined"));
}

#[tokio::test]
async fn login_tool_returns_link() {
    let session = initialized(session_for(
        "echo NeedsLogin",
        "echo 'open https://idp.example/d'; sleep 5",
    ))
    .await;
    let r = call(&session, "netbird_login", json!({})).await;
    assert_eq!(r["structuredContent"]["result"], "pending", "{r}");
    assert_eq!(r["structuredContent"]["login"]["url"], "https://idp.example/d");
    assert!(text(&r).contains("netbird_status"));
}

#[tokio::test]
async fn legacy_replies_carry_no_cache_hints() {
    let session = initialized(session_for("true", "true")).await;
    for method in ["tools/list", "resources/list"] {
        let r = ok(&session, request(2, method, None)).await;
        assert!(
            r.get("ttlMs").is_none() && r.get("cacheScope").is_none(),
            "{method}: {r}"
        );
        assert!(r.get("resultType").is_none(), "{method}: {r}");
    }
}

// ── resources & events ─────────────────────────────────────────────────

#[tokio::test]
async fn modern_resource_results_carry_cache_hints() {
    let session = session_for("echo NeedsLogin", "true");
    session.states.refresh("netbird").await.unwrap();

    let r = ok(
        &session,
        request(1, "resources/list", Some(modern(json!({}), false))),
    )
    .await;
    assert_eq!(r["resultType"], "complete");
    assert_eq!(r["ttlMs"], 300_000);
    assert_eq!(r["cacheScope"], "public");

    let params = modern(json!({ "uri": "cli://netbird/state" }), false);
    let r = ok(&session, request(2, "resources/read", Some(params))).await;
    assert_eq!(r["ttlMs"], 0);
    assert_eq!(r["cacheScope"], "private");
}

#[tokio::test]
async fn state_is_readable_as_resource() {
    let session = initialized(session_for("echo NeedsLogin", "true")).await;
    session.states.refresh("netbird").await.unwrap();

    let r = ok(&session, request(3, "resources/list", None)).await;
    assert_eq!(r["resources"][0]["uri"], "cli://netbird/state");

    let r = ok(
        &session,
        request(4, "resources/read", Some(json!({ "uri": "cli://netbird/state" }))),
    )
    .await;
    assert!(r["contents"][0]["text"].as_str().unwrap().contains("needs_login"));

    let e = err(
        &session,
        request(5, "resources/read", Some(json!({ "uri": "cli://other/state" }))),
    )
    .await;
    assert_eq!(e.code, JsonRpcError::INVALID_PARAMS);
}

#[tokio::test]
async fn events_methods_only_when_enabled() {
    let session = session_for("true", "true");
    let e = err(
        &session,
        request(1, "events/list", Some(modern(json!({}), false))),
    )
    .await;
    assert_eq!(e.code, JsonRpcError::METHOD_NOT_FOUND);

    let hub = crate::events::EventHub::new(Default::default(), vec!["netbird".into()]);
    let session = session.with_events(hub);
    let r = ok(
        &session,
        request(2, "events/list", Some(modern(json!({}), false))),
    )
    .await;
    assert_eq!(r["events"][0]["name"], "cli.state_changed");
    let r = ok(
        &session,
        request(3, "server/discover", Some(modern(json!({}), false))),
    )
    .await;
    assert!(r["capabilities"]["events"].is_object());
}
