//! [`ProcessBackend`] — a generic [`AgentBackend`] that spawns a child
//! process and speaks NDJSON JSON-RPC over its stdio.
//!
//! This is the load-bearing reusable adapter for codex-desktop's three-
//! process model: the UI uses [`ProcessBackend`] to talk to a `codex-agent`
//! child, and concrete `CodexBackend` / `ClaudeBackend` impls compose with
//! [`ProcessBackend`] by overriding submission shapes and capability tables.
//!
//! # Architecture
//!
//! Internally the backend owns:
//! * an outgoing [`NdjsonWriter`] guarded by a [`Mutex`] so concurrent
//!   `request()` callers serialise their writes;
//! * a `pending` map keyed by request id that maps to a oneshot channel;
//! * a [`broadcast`] channel that fans out incoming notifications to every
//!   subscriber returned by [`AgentBackend::events`].
//!
//! A single background reader task reads framed messages off stdout and
//! dispatches each one — see [`dispatch_incoming`] for the pure logic that
//! decides whether a value is a response or a notification.

use crate::{
    AgentBackend, AgentBackendExtras, BackendCapabilities, BackendError,
    IncomingServerNotification, InitializeParams, InitializeResponse, Submission, TurnId,
    envelope::UnknownNotification,
};
use async_trait::async_trait;
use codex_jsonrpc_framing::{JsonRpcMessage, NdjsonReader, NdjsonWriter};
use futures::stream::{BoxStream, StreamExt};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, broadcast, oneshot};
use tracing::{debug, error, warn};

/// Configuration for spawning the child agent process.
///
/// `program`/`args`/`env` mirror the matching fields on
/// [`tokio::process::Command`]; `argv0` exposes the unix-only
/// `arg0` override that codex-desktop uses to multiplex roles via the same
/// ELF binary (a pattern borrowed from busybox-style multicall binaries).
#[derive(Debug, Clone)]
pub struct ProcessBackendConfig {
    /// Path to the executable to spawn.
    pub program: PathBuf,
    /// `argv[0]` override. When `Some`, spawned with
    /// [`tokio::process::Command::arg0`]. Ignored on non-unix platforms
    /// because the underlying API does not exist there.
    pub argv0: Option<String>,
    /// Arguments after `argv[0]`, in declaration order.
    pub args: Vec<String>,
    /// Extra environment variables layered on top of the inherited
    /// environment.
    pub env: Vec<(String, String)>,
}

/// Default capacity of the notification broadcast channel.
///
/// Sized to absorb a burst of stream tokens from a single in-flight turn
/// without lagging. Subscribers that fall too far behind will see
/// [`tokio::sync::broadcast::error::RecvError::Lagged`] which we silently
/// drop on the consumer side — see [`AgentBackend::events`].
const DEFAULT_BROADCAST_CAPACITY: usize = 128;

/// Type alias for the pending-request map.
///
/// Spelled out so signatures across [`ProcessBackend`] and the standalone
/// [`dispatch_incoming`] helper agree without repeating the long generic.
type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, BackendError>>>>>;

/// Generic stdio JSON-RPC adapter implementing [`AgentBackend`].
///
/// Holds the child process handle so [`AgentBackend::shutdown`] can SIGKILL
/// and reap it deterministically; the reader-loop task it spawns terminates
/// on its own when stdout reaches EOF.
pub struct ProcessBackend {
    /// Map of pending request id → oneshot sender for the response.
    pending: PendingMap,
    /// Outgoing-message lock: serialises stdin writes across concurrent
    /// `request()` calls.
    stdin: Arc<Mutex<NdjsonWriter<ChildStdin>>>,
    /// Notifications fan-out to subscribers. Cloning the sender is cheap;
    /// each call to [`AgentBackend::events`] subscribes a fresh receiver.
    notif_tx: broadcast::Sender<IncomingServerNotification>,
    /// Capability snapshot populated by [`AgentBackend::initialize`].
    capabilities: BackendCapabilities,
    /// Hold the child handle so [`AgentBackend::shutdown`] can SIGKILL and
    /// reap it. Wrapped so the trait's `&self` methods don't fight the
    /// borrow checker.
    child: Arc<Mutex<Option<Child>>>,
    /// Monotonic id counter for outgoing requests.
    next_id: Arc<Mutex<u64>>,
}

impl ProcessBackend {
    /// Spawn the child process and start the reader task.
    ///
    /// On success the returned [`ProcessBackend`] is ready to accept
    /// [`AgentBackend::initialize`]; the reader task is already draining
    /// stdout in the background.
    pub async fn spawn(cfg: ProcessBackendConfig) -> Result<Self, BackendError> {
        let mut cmd = Command::new(&cfg.program);
        #[cfg(unix)]
        if let Some(ref a0) = cfg.argv0 {
            cmd.arg0(a0);
        }
        #[cfg(not(unix))]
        let _ = &cfg.argv0;
        cmd.args(&cfg.args);
        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        let mut child = cmd.spawn().map_err(BackendError::Io)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| BackendError::Transport("child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| BackendError::Transport("child has no stdout".into()))?;

        let writer = NdjsonWriter::new(stdin);
        let reader = NdjsonReader::new(stdout);

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (notif_tx, _rx) = broadcast::channel(DEFAULT_BROADCAST_CAPACITY);
        let next_id = Arc::new(Mutex::new(0u64));

        let pending_clone = Arc::clone(&pending);
        let notif_tx_clone = notif_tx.clone();
        tokio::spawn(reader_loop(reader, pending_clone, notif_tx_clone));

        Ok(Self {
            pending,
            stdin: Arc::new(Mutex::new(writer)),
            notif_tx,
            capabilities: BackendCapabilities::default(),
            child: Arc::new(Mutex::new(Some(child))),
            next_id,
        })
    }

    /// Allocate a fresh request id for outbound JSON-RPC requests.
    ///
    /// The wire form is `r{n}` with `n` a monotonically increasing counter;
    /// uniqueness within a single backend instance is sufficient because
    /// the pending map is keyed by string and each instance owns its own
    /// counter.
    async fn next_request_id(&self) -> String {
        let mut g = self.next_id.lock().await;
        *g += 1;
        format!("r{}", *g)
    }

    /// Send a JSON-RPC request and await its matching response.
    async fn request(&self, method: &str, params: Value) -> Result<Value, BackendError> {
        let id = self.next_request_id().await;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);

        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.stdin
            .lock()
            .await
            .write_message(&JsonRpcMessage::new(msg))
            .await
            .map_err(|e| BackendError::Transport(e.to_string()))?;

        rx.await.map_err(|_| BackendError::Closed)?
    }
}

/// Background reader task: pull framed messages off `reader`, dispatch each
/// to either the pending-request map or the notification fan-out.
async fn reader_loop(
    mut reader: NdjsonReader<ChildStdout>,
    pending: PendingMap,
    notif_tx: broadcast::Sender<IncomingServerNotification>,
) {
    loop {
        let msg = match reader.read_message().await {
            Ok(Some(m)) => m,
            Ok(None) => {
                debug!("ProcessBackend: child stdout EOF");
                break;
            }
            Err(e) => {
                error!("ProcessBackend: read error: {e}");
                break;
            }
        };
        dispatch_incoming(msg.into_value(), &pending, &notif_tx).await;
    }
}

/// Pure-ish dispatch of a single inbound JSON value.
///
/// Extracted from [`reader_loop`] so the interesting branch logic — "is
/// this a response, a notification, or malformed?" — can be unit tested
/// without spawning a real process. Returns nothing; both branches are
/// best-effort sends (a closed oneshot or unsubscribed broadcast both fail
/// silently because the caller has already moved on).
pub(crate) async fn dispatch_incoming(
    value: Value,
    pending: &PendingMap,
    notif_tx: &broadcast::Sender<IncomingServerNotification>,
) {
    // Determine whether this is a response (has `id` and either
    // `result` or `error`) or a notification (has `method` but no `id`).
    let id = value.get("id").and_then(|v| v.as_str()).map(str::to_owned);
    let method = value
        .get("method")
        .and_then(|v| v.as_str())
        .map(str::to_owned);

    match (id, method) {
        (Some(id), _) => {
            if let Some(tx) = pending.lock().await.remove(&id) {
                let outcome = if let Some(err) = value.get("error") {
                    Err(BackendError::Rejected(err.to_string()))
                } else {
                    Ok(value.get("result").cloned().unwrap_or(Value::Null))
                };
                let _ = tx.send(outcome);
            } else {
                warn!(?id, "ProcessBackend: response for unknown id");
            }
        }
        (None, Some(method)) => {
            let params = value.get("params").cloned().unwrap_or(Value::Null);
            let notif = IncomingServerNotification::Unknown(UnknownNotification {
                method,
                params,
            });
            let _ = notif_tx.send(notif);
        }
        _ => {
            debug!("ProcessBackend: ignored malformed message");
        }
    }
}

#[async_trait]
impl AgentBackend for ProcessBackend {
    async fn initialize(
        &mut self,
        p: InitializeParams,
    ) -> Result<InitializeResponse, BackendError> {
        let result = self.request("initialize", serde_json::to_value(p)?).await?;
        let resp: InitializeResponse = serde_json::from_value(result)?;
        // Cache capabilities for downstream queries.
        self.capabilities = BackendCapabilities {
            protocol_version: resp.protocol_version.clone(),
            supported_methods: resp.supported_methods.clone(),
            supported_notifications: resp.supported_notifications.clone(),
        };
        Ok(resp)
    }

    async fn submit(&self, sub: Submission) -> Result<(), BackendError> {
        self.request("submit", serde_json::to_value(sub)?).await?;
        Ok(())
    }

    async fn interrupt(&self, turn_id: TurnId) -> Result<(), BackendError> {
        self.request("interrupt", json!({ "turn_id": turn_id }))
            .await?;
        Ok(())
    }

    async fn shutdown(self: Box<Self>) -> Result<(), BackendError> {
        // Best-effort RPC shutdown. Then ensure the child is reaped.
        let _ = self.request("shutdown", Value::Null).await;
        if let Some(mut child) = self.child.lock().await.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        Ok(())
    }

    fn events(&self) -> BoxStream<'static, IncomingServerNotification> {
        let rx = self.notif_tx.subscribe();
        // Convert broadcast::Receiver into a Stream. Lagged receivers (a
        // subscriber that fell behind the broadcast capacity) surface as
        // `Err`; we drop those silently rather than terminating the stream.
        let stream =
            tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|r| async move { r.ok() });
        stream.boxed()
    }

    fn capabilities(&self) -> &BackendCapabilities {
        &self.capabilities
    }

    fn extras(&self) -> Option<&dyn AgentBackendExtras> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::time::Duration;
    use tokio::time::timeout;

    // -------------------------------------------------------------------
    // Pure dispatch tests — no process spawning, no flakiness.
    // -------------------------------------------------------------------

    fn make_pending() -> PendingMap {
        Arc::new(Mutex::new(HashMap::new()))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_routes_response_to_pending_oneshot() {
        let pending = make_pending();
        let (tx, rx) = oneshot::channel();
        pending.lock().await.insert("r1".to_string(), tx);

        let (notif_tx, _notif_rx) = broadcast::channel(8);
        let value = json!({"jsonrpc": "2.0", "id": "r1", "result": {"ok": true}});
        dispatch_incoming(value, &pending, &notif_tx).await;

        let got = rx.await.unwrap_or_else(|_| panic!("oneshot dropped"));
        let v = got.unwrap_or_else(|_| panic!("expected Ok outcome"));
        assert_eq!(v, json!({"ok": true}));
        // The pending entry was consumed.
        assert!(pending.lock().await.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_routes_error_response_as_rejected() {
        let pending = make_pending();
        let (tx, rx) = oneshot::channel();
        pending.lock().await.insert("r1".to_string(), tx);

        let (notif_tx, _notif_rx) = broadcast::channel(8);
        let value = json!({
            "jsonrpc": "2.0",
            "id": "r1",
            "error": {"code": -32600, "message": "bad"},
        });
        dispatch_incoming(value, &pending, &notif_tx).await;

        let got = rx.await.unwrap_or_else(|_| panic!("oneshot dropped"));
        match got {
            Err(BackendError::Rejected(msg)) => {
                assert!(msg.contains("bad"), "msg = {msg}");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_unknown_id_is_logged_not_fatal() {
        // Demonstrates the warning path: a response arrives for an id we
        // never registered. Nothing breaks; the broadcast channel stays
        // empty.
        let pending = make_pending();
        let (notif_tx, mut notif_rx) = broadcast::channel(8);
        let value = json!({"jsonrpc": "2.0", "id": "r-unknown", "result": 42});
        dispatch_incoming(value, &pending, &notif_tx).await;

        // No notification was emitted on the spurious-response path.
        match notif_rx.try_recv() {
            Err(broadcast::error::TryRecvError::Empty) => {}
            other => panic!("expected empty broadcast, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_routes_notification_to_broadcast() {
        let pending = make_pending();
        let (notif_tx, mut notif_rx) = broadcast::channel(8);
        let value = json!({
            "jsonrpc": "2.0",
            "method": "agent/foo",
            "params": {"hello": "world"},
        });
        dispatch_incoming(value, &pending, &notif_tx).await;

        let got = notif_rx.recv().await.unwrap_or_else(|_| panic!("recv"));
        assert_eq!(got.method(), "agent/foo");
        assert_eq!(got.params(), &json!({"hello": "world"}));
        assert!(got.is_unknown());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_handles_notification_with_no_params() {
        let pending = make_pending();
        let (notif_tx, mut notif_rx) = broadcast::channel(8);
        let value = json!({"jsonrpc": "2.0", "method": "agent/ping"});
        dispatch_incoming(value, &pending, &notif_tx).await;

        let got = notif_rx.recv().await.unwrap_or_else(|_| panic!("recv"));
        assert_eq!(got.method(), "agent/ping");
        assert_eq!(got.params(), &Value::Null);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_ignores_malformed_message() {
        // Neither `id` nor `method` — must not panic.
        let pending = make_pending();
        let (notif_tx, mut notif_rx) = broadcast::channel(8);
        dispatch_incoming(json!({"junk": true}), &pending, &notif_tx).await;
        match notif_rx.try_recv() {
            Err(broadcast::error::TryRecvError::Empty) => {}
            other => panic!("expected empty, got {other:?}"),
        }
    }

    // -------------------------------------------------------------------
    // End-to-end process tests.
    //
    // These shell out to system binaries (`cat`, `bash`) and so are
    // marked `#[ignore]` for default runs because some CI environments
    // restrict process spawning or lack a POSIX shell. Run them locally
    // with `cargo nextest run -p codex-agent-backend --run-ignored all`
    // (or `cargo test -p codex-agent-backend -- --ignored`) to exercise
    // the full process plumbing.
    // -------------------------------------------------------------------

    #[tokio::test(flavor = "current_thread")]
    #[ignore = "spawns a real subprocess; run with --ignored"]
    async fn cat_echoes_response_with_unknown_id_warns_but_does_not_break() {
        // `cat` simply replays what we write. We send something that
        // looks like a JSON-RPC response — there is no pending request
        // for that id, which exercises the warn() path in
        // dispatch_incoming without breaking the reader loop.
        let cfg = ProcessBackendConfig {
            program: PathBuf::from("/bin/cat"),
            argv0: None,
            args: vec![],
            env: vec![],
        };
        let backend = ProcessBackend::spawn(cfg)
            .await
            .unwrap_or_else(|e| panic!("spawn cat: {e}"));

        // Manually shove a line into stdin via the mutex-guarded writer.
        let line = json!({"jsonrpc": "2.0", "id": "r1", "method": "echo", "result": 42});
        backend
            .stdin
            .lock()
            .await
            .write_message(&JsonRpcMessage::new(line))
            .await
            .unwrap_or_else(|e| panic!("write: {e}"));

        // Give the reader task a moment to consume the echoed bytes.
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Nothing observable to assert — the absence of panic is the test.
        // Tear down deterministically.
        let _ = Box::new(backend).shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    #[ignore = "spawns a real subprocess; run with --ignored"]
    async fn bash_echo_server_round_trips_request() {
        // True echo server: read a line, write it back. Because the
        // backend writes `{"jsonrpc":..., "id":"r1", "method":"foo",
        // "params":...}` and bash echoes it verbatim, the dispatcher
        // sees an inbound message with the same id and treats it as a
        // response. The "method" field is also present but our match
        // arms route on the id first, so this functions as a synthetic
        // response.
        let cfg = ProcessBackendConfig {
            program: PathBuf::from("/bin/bash"),
            argv0: None,
            args: vec![
                "-c".into(),
                "while IFS= read -r line; do echo \"$line\"; done".into(),
            ],
            env: vec![],
        };
        let backend = ProcessBackend::spawn(cfg)
            .await
            .unwrap_or_else(|e| panic!("spawn bash: {e}"));

        let result = timeout(
            Duration::from_secs(2),
            backend.request("ping", json!({"x": 1})),
        )
        .await
        .unwrap_or_else(|_| panic!("request timed out"))
        .unwrap_or_else(|e| panic!("request error: {e}"));

        // The echoed message has no `result` field, so dispatch_incoming
        // returns `Value::Null` for the result on the success branch.
        assert_eq!(result, Value::Null);

        let _ = Box::new(backend).shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    #[ignore = "spawns a real subprocess; run with --ignored"]
    async fn events_stream_yields_notification_lines() {
        // `bash` prints a single notification line, then exits.
        let cfg = ProcessBackendConfig {
            program: PathBuf::from("/bin/bash"),
            argv0: None,
            args: vec![
                "-c".into(),
                "echo '{\"jsonrpc\":\"2.0\",\"method\":\"agent/foo\",\"params\":{\"k\":1}}'"
                    .into(),
            ],
            env: vec![],
        };
        let backend = ProcessBackend::spawn(cfg)
            .await
            .unwrap_or_else(|e| panic!("spawn bash: {e}"));

        let mut events = backend.events();
        let got = timeout(Duration::from_secs(2), events.next())
            .await
            .unwrap_or_else(|_| panic!("event timed out"))
            .unwrap_or_else(|| panic!("stream ended without yielding"));
        assert_eq!(got.method(), "agent/foo");
        assert_eq!(got.params(), &json!({"k": 1}));

        let _ = Box::new(backend).shutdown().await;
    }
}
