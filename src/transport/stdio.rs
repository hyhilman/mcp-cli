//! Newline-delimited JSON-RPC over stdio.
//!
//! One JSON-RPC message per line, both directions. This is the transport
//! a locally-spawned MCP client (an editor, the headless-mcp hub) talks:
//! stdin is implicitly trusted the way any process the user launched is.
//!
//! `tools/call` requests run concurrently (commands and logins can take a
//! while); everything else is handled in order.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use crate::mcp::jsonrpc::{decode_message, JsonRpcErrorResponse, JsonRpcMessage};
use crate::mcp::{McpSession, SessionReply};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Runs the stdio transport to completion: reads newline-delimited
/// JSON-RPC messages from stdin, dispatches each through `session`, and
/// writes replies to stdout, until stdin reaches EOF.
///
/// A single malformed line never stops the loop: a line that fails to
/// decode gets a JSON-RPC error reply (`id: null`) and reading continues.
///
/// `subscriptions/listen` streams share stdout with replies; each stream
/// runs until the client sends `notifications/cancelled` for its request id
/// or stdin closes.
pub async fn run_stdio(session: Arc<McpSession>) -> io::Result<()> {
    run(session, tokio::io::stdin(), tokio::io::stdout()).await
}

/// The testable core of [`run_stdio`], generic over the input/output streams.
async fn run<R, W>(session: Arc<McpSession>, reader: R, writer: W) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();
    let writer_task = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(message) = out_rx.recv().await {
            write_reply(&mut writer, &message).await?;
        }
        Ok::<(), io::Error>(())
    });

    let mut streams: HashMap<String, JoinHandle<()>> = HashMap::new();
    let mut lines = BufReader::new(reader).lines();

    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(%error, "failed to read a line from stdin; skipping it");
                continue;
            }
        };

        if line.trim().is_empty() {
            continue;
        }

        tracing::trace!(%line, "received stdio line");

        let message = match decode_message(line.as_bytes()) {
            Ok(message) => message,
            Err(error) => {
                let _ = out_tx.send(JsonRpcMessage::ErrorResponse(JsonRpcErrorResponse::new(
                    None, error,
                )));
                continue;
            }
        };

        if let JsonRpcMessage::Notification(n) = &message {
            if n.method == "notifications/cancelled" {
                if let Some(id) = n.params.as_ref().and_then(|p| p.get("requestId")) {
                    if let Some(handle) = streams.remove(&id.to_string()) {
                        handle.abort();
                    }
                }
            }
        }

        if matches!(&message, JsonRpcMessage::Request(r) if r.method == "tools/call") {
            let session = Arc::clone(&session);
            let tx = out_tx.clone();
            tokio::spawn(async move {
                if let Some(SessionReply::Message(reply)) = session.handle_streaming(message).await {
                    let _ = tx.send(reply);
                }
            });
            continue;
        }

        match session.handle_streaming(message).await {
            Some(SessionReply::Message(reply)) => {
                let _ = out_tx.send(reply);
            }
            Some(SessionReply::Stream { id, mut messages }) => {
                let tx = out_tx.clone();
                let handle = tokio::spawn(async move {
                    while let Some(message) = messages.recv().await {
                        if tx.send(message).is_err() {
                            break;
                        }
                    }
                });
                let key = serde_json::to_value(&id)
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                streams.insert(key, handle);
            }
            None => {}
        }
    }

    for handle in streams.values() {
        handle.abort();
    }
    for (_, handle) in streams {
        let _ = handle.await;
    }
    drop(out_tx);
    writer_task.await.unwrap_or_else(|e| Err(io::Error::other(e)))
}

async fn write_reply<W>(writer: &mut W, reply: &JsonRpcMessage) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut serialized = match serde_json::to_vec(reply) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(%error, "failed to serialize a JSON-RPC reply; dropping it");
            return Ok(());
        }
    };
    serialized.push(b'\n');

    writer.write_all(&serialized).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::{json, Value};
    use tokio::io::AsyncReadExt;

    use super::*;
    use crate::mcp::tests::session_for;

    fn initialize_line(id: i64) -> String {
        json!({"jsonrpc": "2.0", "id": id, "method": "initialize"}).to_string()
    }

    async fn drive(session: Arc<McpSession>, input: &str) -> String {
        let (mut input_tx, input_rx) = tokio::io::duplex(64 * 1024);
        let (output_tx, mut output_rx) = tokio::io::duplex(64 * 1024);
        let handle = tokio::spawn(async move { run(session, input_rx, output_tx).await });
        input_tx.write_all(input.as_bytes()).await.unwrap();
        drop(input_tx);
        handle.await.unwrap().unwrap();
        let mut collected = String::new();
        output_rx.read_to_string(&mut collected).await.unwrap();
        collected
    }

    fn parse_lines(output: &str) -> Vec<Value> {
        output
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("each output line is valid JSON"))
            .collect()
    }

    #[tokio::test]
    async fn garbage_line_gets_a_null_id_error_and_the_transport_keeps_running() {
        let session = Arc::new(session_for("echo 'Management: Connected'", "true"));
        let output = drive(session, &format!("not json {{{{\n{}\n", initialize_line(1))).await;
        let replies = parse_lines(&output);
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0]["id"], Value::Null);
        assert!(replies[0]["error"].is_object());
        assert_eq!(replies[1]["result"]["serverInfo"]["name"], "mcp-cli");
    }

    #[tokio::test]
    async fn tool_calls_run_concurrently() {
        let session = Arc::new(session_for("echo 'Management: Connected'", "true"));
        let call = |id: i64, name: &str| {
            json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name}}).to_string()
        };
        let (mut input_tx, input_rx) = tokio::io::duplex(64 * 1024);
        let (output_tx, mut output_rx) = tokio::io::duplex(64 * 1024);
        let handle = tokio::spawn(async move { run(session, input_rx, output_tx).await });
        let input = format!(
            "{}\n{}\n{}\n",
            initialize_line(1),
            call(2, "netbird_slow"),
            call(3, "netbird_peers")
        );
        input_tx.write_all(input.as_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        drop(input_tx);
        handle.await.unwrap().unwrap();
        let mut collected = String::new();
        output_rx.read_to_string(&mut collected).await.unwrap();
        let ids: Vec<i64> = parse_lines(&collected)
            .iter()
            .map(|r| r["id"].as_i64().unwrap())
            .collect();
        // The fast call (3) finishes before the slow one (2).
        assert_eq!(ids, [1, 3, 2], "{collected}");
    }

    #[tokio::test]
    async fn listen_stream_sends_state_updates_until_cancelled() {
        let session = Arc::new(session_for("echo NeedsLogin", "true"));
        let (mut input_tx, input_rx) = tokio::io::duplex(64 * 1024);
        let (output_tx, mut output_rx) = tokio::io::duplex(64 * 1024);
        let handle = tokio::spawn({
            let session = session.clone();
            async move { run(session, input_rx, output_tx).await }
        });

        let listen = json!({
            "jsonrpc": "2.0", "id": 7, "method": "subscriptions/listen",
            "params": {
                "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"},
                "notifications": {"toolsListChanged": true, "resourceSubscriptions": ["cli://netbird/state", "cli://other/state"]}
            }
        });
        input_tx
            .write_all(format!("{listen}\n").as_bytes())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        session.states.refresh("netbird").await.unwrap(); // unknown -> needs_login
        tokio::time::sleep(Duration::from_millis(50)).await;

        let cancel =
            json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 7}});
        input_tx
            .write_all(format!("{cancel}\n").as_bytes())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(input_tx);
        handle.await.unwrap().unwrap();

        let mut collected = String::new();
        output_rx.read_to_string(&mut collected).await.unwrap();
        let lines = parse_lines(&collected);
        assert_eq!(lines.len(), 2, "{collected}");
        assert_eq!(lines[0]["method"], "notifications/subscriptions/acknowledged");
        assert_eq!(
            lines[0]["params"]["notifications"],
            json!({"resourceSubscriptions": ["cli://netbird/state"]})
        );
        assert_eq!(lines[1]["method"], "notifications/resources/updated");
        assert_eq!(lines[1]["params"]["uri"], "cli://netbird/state");
        assert_eq!(
            lines[1]["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"],
            7
        );
    }
}
