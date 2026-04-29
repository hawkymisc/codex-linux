//! Loopback integration test for [`codex_agent_backend::CodexBackend`].
//!
//! # What this test exercises
//!
//! A `tokio::io::duplex` pair gives us two halves of an in-memory pipe; we
//! attach [`CodexBackend::from_async_pipe`] to one end and a tiny NDJSON
//! JSON-RPC mock server to the other. The mock mirrors
//! `codex-desktop::agent_role::process_message` (initialize / submit /
//! interrupt / shutdown) closely enough to drive a full
//! `initialize → submit → events` round-trip.
//!
//! # Why a local mock instead of `codex-desktop::agent_role::run`
//!
//! `codex-desktop::agent_role::run` is hard-wired to real `tokio::io::stdin`
//! / `tokio::io::stdout` — there is no `run_with_io(reader, writer)` public
//! API on `codex-desktop` today. Adding one would touch a sibling crate (out
//! of scope per the PR description), and pulling `codex-desktop` in as a
//! dev-dependency to call its private process_message would create a
//! workspace dependency cycle (`codex-desktop` already depends on
//! `codex-agent-backend`).
//!
//! The mock here is intentionally small (~40 lines): it implements exactly
//! the methods this test drives. The full agent-role implementation lives in
//! `codex-desktop/src/agent_role.rs` and has its own unit tests for the
//! response shape — those are the source of truth for the wire format we
//! mimic here.
//!
//! TODO(PR-E): replace the local mock with a public
//! `codex_desktop::agent_role::run_with_io(reader, writer)` once that API is
//! exposed; the current shape is identical so the swap is mechanical.

use codex_agent_backend::{
    AgentBackend, ClientInfo, CodexBackend, InitializeParams, Submission, SubmissionId, ThreadId,
};
use codex_jsonrpc_framing::{JsonRpcMessage, NdjsonReader, NdjsonWriter};
use futures::StreamExt;
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, duplex};
use tokio::time::timeout;

/// Minimal JSON-RPC server mirroring the subset of
/// `codex-desktop::agent_role::process_message` that this test drives.
///
/// Returns when stdin EOFs or after a `shutdown` request.
async fn mock_agent_server<R, W>(reader: R, writer: W)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut reader = NdjsonReader::new(reader);
    let mut writer = NdjsonWriter::new(writer);
    while let Ok(Some(msg)) = reader.read_message().await {
        let value = msg.into_value();
        let id = value.get("id").cloned();
        let method = value
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .to_owned();
        match method.as_str() {
            "initialize" => {
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "server_info": { "name": "mock-codex", "version": "0.0.1" },
                        "protocol_version": "loopback",
                        "supported_methods": ["initialize", "submit", "interrupt", "shutdown"],
                        "supported_notifications": ["agent/message_delta", "agent/turn_completed"],
                    }
                });
                if writer
                    .write_message(&JsonRpcMessage::new(resp))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            "submit" => {
                let ack = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "accepted": true },
                });
                if writer
                    .write_message(&JsonRpcMessage::new(ack))
                    .await
                    .is_err()
                {
                    return;
                }
                let delta = json!({
                    "jsonrpc": "2.0",
                    "method": "agent/message_delta",
                    "params": { "delta": "hello" },
                });
                if writer
                    .write_message(&JsonRpcMessage::new(delta))
                    .await
                    .is_err()
                {
                    return;
                }
                let completed = json!({
                    "jsonrpc": "2.0",
                    "method": "agent/turn_completed",
                    "params": { "stop_reason": "end_turn" },
                });
                if writer
                    .write_message(&JsonRpcMessage::new(completed))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            "interrupt" => {
                let ack = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "ok": true },
                });
                if writer
                    .write_message(&JsonRpcMessage::new(ack))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            "shutdown" => {
                let ack = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "ok": true },
                });
                let _ = writer.write_message(&JsonRpcMessage::new(ack)).await;
                return;
            }
            _ => {
                // Unknown method — send back -32601 so the client can decide.
                let err = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "method not found" },
                });
                let _ = writer.write_message(&JsonRpcMessage::new(err)).await;
            }
        }
    }
}

#[tokio::test]
async fn loopback_initialize_submit_events() {
    // Two halves of a 64 KiB in-memory pipe:
    //   client_io <-> server_io
    let (client_io, server_io) = duplex(64 * 1024);
    let (client_read, client_write) = tokio::io::split(client_io);
    let (server_read, server_write) = tokio::io::split(server_io);

    // Mock server: drains client_read, writes to client_write.
    tokio::spawn(mock_agent_server(server_read, server_write));

    let mut backend = CodexBackend::from_async_pipe(client_read, client_write);

    // Subscribe BEFORE submit so we can't miss the streamed notifications.
    let mut events = backend.events();

    // 1. Initialize.
    let resp = timeout(
        Duration::from_secs(2),
        backend.initialize(InitializeParams {
            client_info: ClientInfo {
                name: "loopback".into(),
                version: "0.0.1".into(),
            },
            protocol_version: Some("loopback".into()),
            supported_methods: vec![],
        }),
    )
    .await
    .expect("initialize timed out")
    .expect("initialize failed");
    assert_eq!(resp.server_info.name, "mock-codex");
    assert_eq!(resp.protocol_version.as_deref(), Some("loopback"));
    assert!(
        backend.capabilities().supports_notification("agent/message_delta"),
        "capabilities mirrored from initialize response",
    );

    // 2. Submit. The mock acks then emits two notifications.
    timeout(
        Duration::from_secs(2),
        backend.submit(Submission {
            id: SubmissionId::from("s-1"),
            thread_id: ThreadId::from("t-1"),
            payload: json!({ "text": "hi" }),
        }),
    )
    .await
    .expect("submit timed out")
    .expect("submit failed");

    // 3. Drain at least one notification from the events stream.
    let first = timeout(Duration::from_secs(2), events.next())
        .await
        .expect("event stream timed out")
        .expect("event stream ended without yielding");
    // Both expected notifications use the `agent/...` namespace.
    let method = first.method();
    assert!(
        method == "agent/message_delta" || method == "agent/turn_completed",
        "unexpected method: {method}",
    );

    // Best-effort shutdown so the mock task exits cleanly. The boxed receiver
    // is consumed; the broadcast sender drops along with `backend`.
    let _ = Box::new(backend).shutdown().await;

    // Allow the spawned mock to settle so the test runtime can finish.
    let _ = timeout(Duration::from_millis(200), futures::future::pending::<()>()).await;
}

/// Sanity check: a backend constructed without a server still surfaces a
/// transport error (Closed) when the reader half EOFs before any response
/// arrives.
#[tokio::test]
async fn closed_reader_surfaces_as_closed_error() {
    use codex_agent_backend::BackendError;

    let (client_io, server_io) = duplex(1024);
    let (client_read, client_write) = tokio::io::split(client_io);
    // Drop the server half immediately so the reader task EOFs.
    drop(server_io);

    let mut backend = CodexBackend::from_async_pipe(client_read, client_write);

    let err = timeout(
        Duration::from_secs(2),
        backend.initialize(InitializeParams {
            client_info: ClientInfo {
                name: "loopback".into(),
                version: "0.0.1".into(),
            },
            protocol_version: None,
            supported_methods: vec![],
        }),
    )
    .await
    .expect("initialize timed out")
    .expect_err("initialize must fail when transport is closed");

    // Either Transport (write side broke) or Closed (reader EOFs first) is
    // acceptable; we care that we don't hang or panic.
    match err {
        BackendError::Closed | BackendError::Transport(_) => {}
        other => panic!("unexpected error: {other:?}"),
    }
}
