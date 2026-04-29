//! `codex-agent` role: a JSON-RPC over NDJSON server speaking from stdin to
//! stdout. PR-B implementation responds to a small set of methods so the
//! desktop UI can be developed end-to-end before real backends (Codex,
//! Claude Code) are wired in PR-C.
//!
//! Methods accepted:
//!  * `initialize` (request) → `InitializeResponse`
//!  * `submit`     (request) → ack; followed by streamed
//!    `agent/message_delta` notifications echoing
//!    the submission's payload back as text.
//!  * `interrupt`  (request) → ack
//!  * `shutdown`   (request) → ack, then the loop exits.
//!
//! Anything else gets a JSON-RPC `MethodNotFound` (-32601) error response.
//! Streamed notifications use the `agent/<event>` method namespace.

use anyhow::Result;
use codex_jsonrpc_framing::{JsonRpcError, JsonRpcMessage, NdjsonReader, NdjsonWriter};
use serde_json::json;
use tokio::io;
use tokio::io::AsyncWrite;
use tracing::{debug, info, warn};

const PROTOCOL_VERSION: &str = "0.0.0-pr-b";

/// Run the agent role: read NDJSON JSON-RPC messages from stdin and write
/// responses/notifications to stdout until EOF or `shutdown`.
pub async fn run() -> Result<()> {
    info!("codex-agent: starting NDJSON JSON-RPC server on stdio");

    let stdin = io::stdin();
    let stdout = io::stdout();

    let mut reader = NdjsonReader::new(stdin);
    let mut writer = NdjsonWriter::new(stdout);

    loop {
        let msg = match reader.read_message().await? {
            Some(m) => m,
            None => {
                info!("codex-agent: stdin EOF, exiting");
                break;
            }
        };

        if !process_message(&msg, &mut writer).await? {
            info!("codex-agent: shutdown requested, exiting loop");
            break;
        }
    }

    Ok(())
}

/// Dispatch a single JSON-RPC message and write any responses/notifications
/// to `writer`.
///
/// Returns `Ok(true)` when the server should keep running and `Ok(false)`
/// when the loop should break (e.g. after a `shutdown` request). Pulled out
/// of [`run`] so unit tests can drive the server with a `Vec<u8>` capture
/// writer rather than real stdio.
pub async fn process_message<W>(msg: &JsonRpcMessage, writer: &mut NdjsonWriter<W>) -> Result<bool>
where
    W: AsyncWrite + Unpin,
{
    let value = msg.as_value();
    let method = value.get("method").and_then(|v| v.as_str());
    let id = value.get("id").cloned();

    match method {
        Some("initialize") => {
            handle_initialize(id, writer).await?;
            Ok(true)
        }
        Some("submit") => {
            handle_submit(id, value.get("params").cloned(), writer).await?;
            Ok(true)
        }
        Some("interrupt") => {
            handle_ack(id, writer, "interrupted").await?;
            Ok(true)
        }
        Some("shutdown") => {
            handle_ack(id, writer, "goodbye").await?;
            Ok(false)
        }
        Some(other) => {
            warn!(method = other, "codex-agent: unrecognised method");
            send_error(id, writer, -32601, &format!("method not found: {other}")).await?;
            Ok(true)
        }
        None => {
            debug!(?value, "codex-agent: ignored non-method message");
            Ok(true)
        }
    }
}

async fn handle_initialize<W>(
    id: Option<serde_json::Value>,
    writer: &mut NdjsonWriter<W>,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let response = json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "server_info": { "name": "codex-agent-stub", "version": "0.1.0" },
            "protocol_version": PROTOCOL_VERSION,
            "supported_methods": ["initialize", "submit", "interrupt", "shutdown"],
            "supported_notifications": ["agent/message_delta", "agent/turn_completed"],
        }
    });
    writer.write_message(&JsonRpcMessage::new(response)).await?;
    Ok(())
}

async fn handle_submit<W>(
    id: Option<serde_json::Value>,
    params: Option<serde_json::Value>,
    writer: &mut NdjsonWriter<W>,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    // 1. Ack the submission.
    let ack = json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "accepted": true },
    });
    writer.write_message(&JsonRpcMessage::new(ack)).await?;

    // 2. Echo the payload back as a streamed message_delta notification, then
    //    a turn_completed notification. The UI can use this to verify the
    //    streaming path end-to-end before a real backend lands.
    let echo_text = params
        .as_ref()
        .and_then(|p| p.get("payload"))
        .and_then(|p| p.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or("(no payload.text)")
        .to_string();

    let delta = json!({
        "jsonrpc": "2.0",
        "method": "agent/message_delta",
        "params": { "delta": echo_text }
    });
    writer.write_message(&JsonRpcMessage::new(delta)).await?;

    let completed = json!({
        "jsonrpc": "2.0",
        "method": "agent/turn_completed",
        "params": { "stop_reason": "end_turn" }
    });
    writer.write_message(&JsonRpcMessage::new(completed)).await?;

    Ok(())
}

async fn handle_ack<W>(
    id: Option<serde_json::Value>,
    writer: &mut NdjsonWriter<W>,
    note: &str,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let ack = json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "ok": true, "note": note }
    });
    writer.write_message(&JsonRpcMessage::new(ack)).await?;
    Ok(())
}

async fn send_error<W>(
    id: Option<serde_json::Value>,
    writer: &mut NdjsonWriter<W>,
    code: i32,
    message: &str,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let err = JsonRpcError {
        code,
        message: message.to_string(),
        data: None,
    };
    let response = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": err,
    });
    writer.write_message(&JsonRpcMessage::new(response)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::Value;

    /// Drive `process_message` with a synthetic request and return the parsed
    /// JSON values written by the server, plus the boolean continuation flag.
    async fn run_one(request: Value) -> Result<(Vec<Value>, bool)> {
        let buf: Vec<u8> = Vec::new();
        let mut writer = NdjsonWriter::new(buf);
        let msg = JsonRpcMessage::new(request);
        let cont = process_message(&msg, &mut writer).await?;
        let buf = writer.into_inner();
        let parsed = parse_ndjson(&buf)?;
        Ok((parsed, cont))
    }

    fn parse_ndjson(bytes: &[u8]) -> Result<Vec<Value>> {
        let text = std::str::from_utf8(bytes)?;
        let mut out = Vec::new();
        for line in text.split('\n') {
            if line.trim().is_empty() {
                continue;
            }
            out.push(serde_json::from_str(line)?);
        }
        Ok(out)
    }

    #[tokio::test]
    async fn initialize_returns_server_info() -> Result<()> {
        let (msgs, cont) = run_one(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
        }))
        .await?;

        assert!(cont, "initialize should not break the loop");
        assert_eq!(msgs.len(), 1);
        let resp = &msgs[0];
        assert_eq!(resp.get("id"), Some(&json!(1)));
        let name = resp
            .get("result")
            .and_then(|r| r.get("server_info"))
            .and_then(|s| s.get("name"))
            .and_then(|n| n.as_str());
        assert_eq!(name, Some("codex-agent-stub"));
        let proto = resp
            .get("result")
            .and_then(|r| r.get("protocol_version"))
            .and_then(|v| v.as_str());
        assert_eq!(proto, Some(PROTOCOL_VERSION));
        Ok(())
    }

    #[tokio::test]
    async fn submit_acks_and_streams_two_notifications() -> Result<()> {
        let (msgs, cont) = run_one(json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": "submit",
            "params": { "payload": { "text": "hello world" } },
        }))
        .await?;

        assert!(cont, "submit should not break the loop");
        assert_eq!(msgs.len(), 3, "ack + 2 notifications expected");

        // 1. Ack with id passthrough.
        assert_eq!(msgs[0].get("id"), Some(&json!(42)));
        assert_eq!(
            msgs[0].get("result").and_then(|r| r.get("accepted")),
            Some(&json!(true))
        );

        // 2. message_delta notification with echoed text.
        assert_eq!(
            msgs[1].get("method").and_then(|m| m.as_str()),
            Some("agent/message_delta")
        );
        assert_eq!(
            msgs[1]
                .get("params")
                .and_then(|p| p.get("delta"))
                .and_then(|d| d.as_str()),
            Some("hello world")
        );
        // Notification has no id.
        assert!(msgs[1].get("id").is_none());

        // 3. turn_completed notification.
        assert_eq!(
            msgs[2].get("method").and_then(|m| m.as_str()),
            Some("agent/turn_completed")
        );
        assert_eq!(
            msgs[2]
                .get("params")
                .and_then(|p| p.get("stop_reason"))
                .and_then(|s| s.as_str()),
            Some("end_turn")
        );
        Ok(())
    }

    #[tokio::test]
    async fn submit_without_payload_text_uses_placeholder() -> Result<()> {
        let (msgs, cont) = run_one(json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "submit",
            "params": {},
        }))
        .await?;

        assert!(cont);
        assert_eq!(msgs.len(), 3);
        assert_eq!(
            msgs[1]
                .get("params")
                .and_then(|p| p.get("delta"))
                .and_then(|d| d.as_str()),
            Some("(no payload.text)")
        );
        Ok(())
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found_error() -> Result<()> {
        let (msgs, cont) = run_one(json!({
            "jsonrpc": "2.0",
            "id": "abc",
            "method": "definitely_not_a_method",
        }))
        .await?;

        assert!(cont, "unknown methods do not break the loop");
        assert_eq!(msgs.len(), 1);
        let err = &msgs[0];
        assert_eq!(err.get("id"), Some(&json!("abc")));
        let code = err
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(serde_json::Value::as_i64);
        assert_eq!(code, Some(-32601));
        let message = err
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("");
        assert!(
            message.contains("definitely_not_a_method"),
            "message should name the bad method, got {message:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_writes_response_and_breaks_loop() -> Result<()> {
        let (msgs, cont) = run_one(json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "shutdown",
        }))
        .await?;

        assert!(!cont, "shutdown should break the loop");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].get("id"), Some(&json!(99)));
        assert_eq!(
            msgs[0]
                .get("result")
                .and_then(|r| r.get("ok"))
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            msgs[0]
                .get("result")
                .and_then(|r| r.get("note"))
                .and_then(|n| n.as_str()),
            Some("goodbye")
        );
        Ok(())
    }

    #[tokio::test]
    async fn interrupt_returns_ack() -> Result<()> {
        let (msgs, cont) = run_one(json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "interrupt",
        }))
        .await?;

        assert!(cont);
        assert_eq!(msgs.len(), 1);
        assert_eq!(
            msgs[0]
                .get("result")
                .and_then(|r| r.get("note"))
                .and_then(|n| n.as_str()),
            Some("interrupted")
        );
        Ok(())
    }

    #[tokio::test]
    async fn non_method_message_is_ignored_silently() -> Result<()> {
        // Pure response-shaped envelope (no `method`) — should be ignored.
        let (msgs, cont) = run_one(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": { "ok": true },
        }))
        .await?;
        assert!(cont);
        assert!(msgs.is_empty(), "no output expected, got {msgs:?}");
        Ok(())
    }
}
