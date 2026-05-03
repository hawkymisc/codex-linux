#![forbid(unsafe_code)]

//! [`ClaudeBackend`] — an [`AgentBackend`] adapter for Claude Code's
//! Anthropic Agent SDK.
//!
//! # Where this fits in the stack
//!
//! `codex-desktop`'s `AgentBackend` abstraction (in `codex-agent-backend`)
//! is deliberately backend-agnostic: the GUI talks to a single `Arc<dyn
//! AgentBackend>` regardless of which agent runtime sits underneath.
//! [`crate::ClaudeBackend`] is the "Claude Code" implementation —
//! a sibling of `codex_agent_backend::CodexBackend` that swaps the wire-
//! method namespace from `agent/*` to `claude/*` and ships its own
//! recognised-notification registry.
//!
//! # Why a separate crate
//!
//! The architecture plan in `docs/desktop-architecture.md` §11 calls for
//! `codex-claude-backend` as a sibling of `codex-agent-backend`. Keeping
//! Claude-specific logic out of the abstraction crate means
//! `codex-agent-backend` stays dependency-light (no Anthropic SDK creep)
//! and the desktop binary can opt in to the Claude backend through cargo
//! features once a real upstream transport lands.
//!
//! # Wire protocol (this skeleton)
//!
//! NDJSON JSON-RPC 2.0 over stdio — same framing as [`CodexBackend`] —
//! with the following wire methods:
//!
//! | High-level (`AgentBackend`) | Wire method     | Direction |
//! |-----------------------------|-----------------|-----------|
//! | `initialize`                | `claude/initialize` | C → S |
//! | `submit`                    | `claude/submit`     | C → S |
//! | `interrupt`                 | `claude/interrupt`  | C → S |
//! | `shutdown`                  | `claude/shutdown`   | C → S |
//! | (notification)              | `claude/message_delta`     | S → C |
//! | (notification)              | `claude/turn_completed`    | S → C |
//! | (notification)              | `claude/content_block_delta` | S → C |
//!
//! When a real `claude-code` host process is wired up, this module
//! becomes the pure translation layer between [`Submission`] /
//! [`InitializeParams`] and the host's typed envelopes — it does not
//! shell out to or reimplement the Anthropic API itself.
//!
//! # Construction
//!
//! * [`ClaudeBackend::from_async_pipe`] — drives any
//!   `AsyncRead + AsyncWrite` pair using the same NDJSON framing as
//!   [`CodexBackend::from_async_pipe`]. This is the load-bearing
//!   constructor for tests via `tokio::io::duplex`, and for future
//!   `claude-code` subprocess integration.
//! * [`ClaudeBackend::default_registry`] — recognised-notification
//!   set the desktop's drift log uses to decide what is "expected"
//!   protocol traffic from a Claude Code host.

use async_trait::async_trait;
use codex_agent_backend::{
    AgentBackend, BackendCapabilities, BackendError, IncomingServerNotification, InitializeParams,
    InitializeResponse, KnownVariantRegistry, Submission, TurnId,
    registry::{BackendDescriptor, BackendFactoryFuture, BackendId},
};
use codex_jsonrpc_framing::{JsonRpcMessage, NdjsonReader, NdjsonWriter};
use futures::stream::{BoxStream, StreamExt};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tracing::{debug, error, warn};

/// Capacity of the per-instance notification fan-out channel. Sized to
/// absorb a burst of message_delta / content_block_delta tokens from a
/// single in-flight turn without lagging slow subscribers.
const DEFAULT_BROADCAST_CAPACITY: usize = 128;

/// Pending-request map keyed by JSON-RPC request id.
type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, BackendError>>>>>;

/// Outbound write request sent to the writer actor. The actor owns the
/// [`NdjsonWriter`] outright so the `.await` in the writer never holds a
/// `tokio::sync::MutexGuard` (forbidden by the workspace's
/// `clippy::await-holding-invalid-type` lint).
struct WriteJob {
    value: Value,
    ack: oneshot::Sender<Result<(), std::io::Error>>,
}

/// [`AgentBackend`] implementation that speaks the Claude Code dialect of
/// NDJSON JSON-RPC against any `AsyncRead + AsyncWrite` pair.
pub struct ClaudeBackend {
    pending: PendingMap,
    writer_tx: mpsc::Sender<WriteJob>,
    notif_tx: broadcast::Sender<IncomingServerNotification>,
    capabilities: BackendCapabilities,
    next_id: Arc<Mutex<u64>>,
    registry: Arc<KnownVariantRegistry>,
}

impl ClaudeBackend {
    /// Returns the default registry of method names a Claude Code host is
    /// known to emit. Adding a new method here is the gate to surfacing it
    /// as [`IncomingServerNotification::Known`]; everything else falls
    /// through to [`IncomingServerNotification::Unknown`] and the desktop
    /// drift log.
    pub fn default_registry() -> KnownVariantRegistry {
        KnownVariantRegistry::new().with_methods([
            "claude/message_delta",
            "claude/turn_completed",
            "claude/content_block_delta",
        ])
    }

    /// Construct a backend that speaks the Claude dialect over the supplied
    /// reader/writer pair. Uses [`Self::default_registry`] as the
    /// recognised-methods set.
    pub fn from_async_pipe<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        Self::from_async_pipe_with_registry(reader, writer, Self::default_registry())
    }

    /// Like [`Self::from_async_pipe`] but with an explicit registry. Tests
    /// pass an empty registry to force every notification through the
    /// `Unknown` fallback path.
    pub fn from_async_pipe_with_registry<R, W>(
        reader: R,
        writer: W,
        registry: KnownVariantRegistry,
    ) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (notif_tx, _rx) = broadcast::channel(DEFAULT_BROADCAST_CAPACITY);
        let registry = Arc::new(registry);

        let (writer_tx, writer_rx) = mpsc::channel::<WriteJob>(DEFAULT_BROADCAST_CAPACITY);
        let writer_typed = NdjsonWriter::new(writer);
        tokio::spawn(writer_loop(writer_typed, writer_rx));

        let reader_typed = NdjsonReader::new(reader);
        let pending_clone = Arc::clone(&pending);
        let notif_tx_clone = notif_tx.clone();
        let registry_clone = Arc::clone(&registry);
        tokio::spawn(reader_loop(
            reader_typed,
            pending_clone,
            notif_tx_clone,
            registry_clone,
        ));

        Self {
            pending,
            writer_tx,
            notif_tx,
            capabilities: BackendCapabilities::default(),
            next_id: Arc::new(Mutex::new(0u64)),
            registry,
        }
    }

    /// Borrow the active recognised-methods registry. Useful for tests
    /// asserting the default set covers the expected methods.
    pub fn registry(&self) -> &KnownVariantRegistry {
        &self.registry
    }

    async fn next_request_id(&self) -> String {
        let mut g = self.next_id.lock().await;
        *g += 1;
        format!("c{}", *g)
    }

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
        self.send_value(msg).await?;
        rx.await.map_err(|_| BackendError::Closed)?
    }

    async fn send_value(&self, value: Value) -> Result<(), BackendError> {
        let (ack, ack_rx) = oneshot::channel();
        self.writer_tx
            .send(WriteJob { value, ack })
            .await
            .map_err(|_| BackendError::Closed)?;
        match ack_rx.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(io)) => Err(BackendError::Transport(io.to_string())),
            Err(_) => Err(BackendError::Closed),
        }
    }
}

async fn writer_loop<W>(mut writer: NdjsonWriter<W>, mut rx: mpsc::Receiver<WriteJob>)
where
    W: AsyncWrite + Unpin,
{
    while let Some(job) = rx.recv().await {
        let result = writer
            .write_message(&JsonRpcMessage::new(job.value))
            .await
            .map_err(|e| std::io::Error::other(e.to_string()));
        let _ = job.ack.send(result);
    }
}

async fn reader_loop<R>(
    mut reader: NdjsonReader<R>,
    pending: PendingMap,
    notif_tx: broadcast::Sender<IncomingServerNotification>,
    registry: Arc<KnownVariantRegistry>,
) where
    R: AsyncRead + Unpin,
{
    loop {
        let msg = match reader.read_message().await {
            Ok(Some(m)) => m,
            Ok(None) => {
                debug!("ClaudeBackend: reader EOF");
                break;
            }
            Err(e) => {
                error!("ClaudeBackend: read error: {e}");
                break;
            }
        };
        dispatch_incoming(msg.into_value(), &pending, &notif_tx, &registry).await;
    }
}

/// Pure-ish dispatch of one inbound JSON value into either the pending
/// request map or the notification fan-out. Mirrors `CodexBackend::dispatch_incoming`.
pub(crate) async fn dispatch_incoming(
    value: Value,
    pending: &PendingMap,
    notif_tx: &broadcast::Sender<IncomingServerNotification>,
    registry: &KnownVariantRegistry,
) {
    let id = value.get("id").and_then(|v| v.as_str()).map(str::to_owned);
    let method = value
        .get("method")
        .and_then(|v| v.as_str())
        .map(str::to_owned);

    match (id, method) {
        (Some(id), None) => deliver_response(id, &value, pending).await,
        (Some(id), Some(_)) => {
            let is_pending = pending.lock().await.contains_key(&id);
            if is_pending {
                deliver_response(id, &value, pending).await;
            } else {
                let params = value.get("params").cloned().unwrap_or(Value::Null);
                let method = value
                    .get("method")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_owned();
                let notif = IncomingServerNotification::classify(&method, params, registry);
                let _ = notif_tx.send(notif);
            }
        }
        (None, Some(method)) => {
            let params = value.get("params").cloned().unwrap_or(Value::Null);
            let notif = IncomingServerNotification::classify(&method, params, registry);
            let _ = notif_tx.send(notif);
        }
        _ => {
            debug!("ClaudeBackend: ignored malformed message");
        }
    }
}

async fn deliver_response(id: String, value: &Value, pending: &PendingMap) {
    if let Some(tx) = pending.lock().await.remove(&id) {
        let outcome = if let Some(err) = value.get("error") {
            Err(BackendError::Rejected(err.to_string()))
        } else {
            Ok(value.get("result").cloned().unwrap_or(Value::Null))
        };
        let _ = tx.send(outcome);
    } else {
        warn!(?id, "ClaudeBackend: response for unknown id");
    }
}

#[async_trait]
impl AgentBackend for ClaudeBackend {
    async fn initialize(
        &mut self,
        p: InitializeParams,
    ) -> Result<InitializeResponse, BackendError> {
        let result = self
            .request("claude/initialize", serde_json::to_value(p)?)
            .await?;
        let resp: InitializeResponse = serde_json::from_value(result)?;
        self.capabilities = BackendCapabilities {
            protocol_version: resp.protocol_version.clone(),
            supported_methods: resp.supported_methods.clone(),
            supported_notifications: resp.supported_notifications.clone(),
        };
        Ok(resp)
    }

    async fn submit(&self, sub: Submission) -> Result<(), BackendError> {
        self.request("claude/submit", serde_json::to_value(sub)?)
            .await?;
        Ok(())
    }

    async fn interrupt(&self, turn_id: TurnId) -> Result<(), BackendError> {
        self.request("claude/interrupt", json!({ "turn_id": turn_id }))
            .await?;
        Ok(())
    }

    async fn shutdown(self: Box<Self>) -> Result<(), BackendError> {
        let _ = self.request("claude/shutdown", Value::Null).await;
        Ok(())
    }

    fn events(&self) -> BoxStream<'static, IncomingServerNotification> {
        let rx = self.notif_tx.subscribe();
        let stream =
            tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|r| async move { r.ok() });
        stream.boxed()
    }

    fn capabilities(&self) -> &BackendCapabilities {
        &self.capabilities
    }
}

/// Stable id surfaced to the desktop's backend picker UI.
pub const BACKEND_ID: BackendId = BackendId("claude-code");

/// Default factory: registered via `inventory::submit!` below. It returns
/// [`BackendError::Closed`] until a real `claude-code` host transport is
/// plumbed in (out of scope for this scaffold) — the desktop UI is expected
/// to fall back to the Codex backend when this fails.
fn factory() -> BackendFactoryFuture {
    Box::pin(async move {
        Err::<Arc<dyn AgentBackend>, _>(BackendError::Closed)
    })
}

inventory::submit! {
    BackendDescriptor {
        id: BACKEND_ID,
        display_name: "Claude Code",
        factory,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use codex_agent_backend::{
        ClientInfo, ServerInfo, SubmissionId, ThreadId, registered_backends,
    };
    use codex_jsonrpc_framing::{NdjsonReader, NdjsonWriter};
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::time::Duration;
    use tokio::io::duplex;

    fn empty_registry() -> KnownVariantRegistry {
        KnownVariantRegistry::new()
    }

    fn make_pending() -> PendingMap {
        Arc::new(Mutex::new(HashMap::new()))
    }

    #[test]
    fn default_registry_covers_claude_published_methods() {
        let r = ClaudeBackend::default_registry();
        for m in [
            "claude/message_delta",
            "claude/turn_completed",
            "claude/content_block_delta",
        ] {
            assert!(r.contains(m), "default registry missing {m}");
        }
        // Codex-namespaced methods must NOT be in the Claude registry —
        // the drift log relies on per-backend registries to avoid
        // false-negative "expected" classifications.
        assert!(!r.contains("agent/message_delta"));
        assert!(!r.contains("agent/turn_completed"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_classifies_known_notification_against_registry() {
        let pending = make_pending();
        let (notif_tx, mut notif_rx) = broadcast::channel(8);
        let registry = ClaudeBackend::default_registry();
        let value = json!({
            "jsonrpc": "2.0",
            "method": "claude/message_delta",
            "params": {"delta": "hello"},
        });
        dispatch_incoming(value, &pending, &notif_tx, &registry).await;

        let got = notif_rx.recv().await.unwrap();
        assert_eq!(got.method(), "claude/message_delta");
        assert!(!got.is_unknown(), "claude/message_delta must classify as Known");
        assert_eq!(got.params(), &json!({"delta": "hello"}));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_falls_back_to_unknown_when_method_not_in_registry() {
        let pending = make_pending();
        let (notif_tx, mut notif_rx) = broadcast::channel(8);
        let value = json!({
            "jsonrpc": "2.0",
            "method": "claude/some_future_event",
            "params": {"x": 1},
        });
        dispatch_incoming(value, &pending, &notif_tx, &empty_registry()).await;

        let got = notif_rx.recv().await.unwrap();
        assert_eq!(got.method(), "claude/some_future_event");
        assert!(got.is_unknown());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_routes_response_to_pending_oneshot() {
        let pending = make_pending();
        let (tx, rx) = oneshot::channel::<Result<Value, BackendError>>();
        pending.lock().await.insert("c1".to_string(), tx);

        let (notif_tx, _notif_rx) = broadcast::channel(8);
        let value = json!({"jsonrpc": "2.0", "id": "c1", "result": {"ok": true}});
        dispatch_incoming(value, &pending, &notif_tx, &empty_registry()).await;

        let v = rx.await.unwrap().unwrap();
        assert_eq!(v, json!({"ok": true}));
        assert!(pending.lock().await.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_routes_error_response_as_rejected() {
        let pending = make_pending();
        let (tx, rx) = oneshot::channel::<Result<Value, BackendError>>();
        pending.lock().await.insert("c1".to_string(), tx);

        let (notif_tx, _notif_rx) = broadcast::channel(8);
        let value = json!({
            "jsonrpc": "2.0",
            "id": "c1",
            "error": {"code": -32600, "message": "bad"},
        });
        dispatch_incoming(value, &pending, &notif_tx, &empty_registry()).await;

        match rx.await.unwrap() {
            Err(BackendError::Rejected(msg)) => assert!(msg.contains("bad")),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn inventory_submission_includes_claude_code() {
        let found = registered_backends().any(|d| d.id == BACKEND_ID);
        assert!(found, "claude-code backend should be registered via inventory");

        let descriptor = registered_backends()
            .find(|d| d.id == BACKEND_ID)
            .expect("claude-code descriptor present");
        assert_eq!(descriptor.display_name, "Claude Code");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn factory_returns_closed_until_real_transport_is_plumbed() {
        let descriptor = registered_backends()
            .find(|d| d.id == BACKEND_ID)
            .expect("claude-code descriptor present");
        // `Result<Arc<dyn AgentBackend>, _>` doesn't implement Debug
        // (dyn AgentBackend doesn't derive Debug), so unwrap the variants
        // by pattern instead of `{:?}`.
        match (descriptor.factory)().await {
            Err(BackendError::Closed) => {}
            Err(other) => panic!("expected BackendError::Closed, got {other}"),
            Ok(_) => panic!("expected BackendError::Closed, got an Ok backend"),
        }
    }

    /// End-to-end via duplex pipes: drive `submit` against a mock Claude
    /// host that reads the wire request, asserts the method is
    /// `claude/submit`, and replies with an empty result.
    #[tokio::test(flavor = "multi_thread")]
    async fn submit_sends_claude_namespaced_method_over_the_wire() {
        // backend writes -> server reads
        let (backend_writer, mut server_reader) = duplex(64 * 1024);
        // server writes -> backend reads
        let (mut server_writer, backend_reader) = duplex(64 * 1024);

        let backend = ClaudeBackend::from_async_pipe(backend_reader, backend_writer);

        // Mock server: pull one frame, assert shape, reply, drain shutdown.
        let mock = tokio::spawn(async move {
            let mut reader = NdjsonReader::new(&mut server_reader);
            let req = reader
                .read_message()
                .await
                .expect("read")
                .expect("frame")
                .into_value();
            assert_eq!(req["method"], json!("claude/submit"));
            assert_eq!(req["params"]["thread_id"], json!("default"));
            assert_eq!(req["params"]["payload"]["text"], json!("hi"));
            let id = req["id"].clone();

            let mut writer = NdjsonWriter::new(&mut server_writer);
            writer
                .write_message(&JsonRpcMessage::new(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {},
                })))
                .await
                .unwrap();
        });

        let sub = Submission {
            id: SubmissionId::from("s1"),
            thread_id: ThreadId::from("default"),
            payload: json!({"text": "hi"}),
        };
        tokio::time::timeout(Duration::from_secs(5), backend.submit(sub))
            .await
            .expect("submit completes")
            .expect("submit ok");

        tokio::time::timeout(Duration::from_secs(5), mock)
            .await
            .expect("mock completes")
            .expect("mock ok");
    }

    /// End-to-end via duplex: drive `initialize` and verify the response
    /// populates `capabilities()`.
    #[tokio::test(flavor = "multi_thread")]
    async fn initialize_round_trip_populates_capabilities() {
        let (backend_writer, mut server_reader) = duplex(64 * 1024);
        let (mut server_writer, backend_reader) = duplex(64 * 1024);

        let mut backend = ClaudeBackend::from_async_pipe(backend_reader, backend_writer);

        let mock = tokio::spawn(async move {
            let mut reader = NdjsonReader::new(&mut server_reader);
            let req = reader
                .read_message()
                .await
                .expect("read")
                .expect("frame")
                .into_value();
            assert_eq!(req["method"], json!("claude/initialize"));
            let id = req["id"].clone();

            let mut writer = NdjsonWriter::new(&mut server_writer);
            writer
                .write_message(&JsonRpcMessage::new(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "server_info": {"name": "claude-code-mock", "version": "0.0.0"},
                        "protocol_version": "claude-pr-x",
                        "supported_methods": ["claude/initialize", "claude/submit"],
                        "supported_notifications": ["claude/message_delta"],
                    },
                })))
                .await
                .unwrap();
        });

        let resp = tokio::time::timeout(
            Duration::from_secs(5),
            backend.initialize(InitializeParams {
                client_info: ClientInfo {
                    name: "test".into(),
                    version: "0".into(),
                },
                protocol_version: None,
                supported_methods: Vec::new(),
            }),
        )
        .await
        .expect("init completes")
        .expect("init ok");

        assert_eq!(
            resp.server_info,
            ServerInfo {
                name: "claude-code-mock".into(),
                version: "0.0.0".into(),
            }
        );
        assert_eq!(resp.protocol_version.as_deref(), Some("claude-pr-x"));
        assert_eq!(
            backend.capabilities().protocol_version.as_deref(),
            Some("claude-pr-x")
        );
        assert_eq!(
            backend.capabilities().supported_methods,
            vec!["claude/initialize".to_string(), "claude/submit".to_string()],
        );

        tokio::time::timeout(Duration::from_secs(5), mock)
            .await
            .expect("mock completes")
            .expect("mock ok");
    }

    /// Server-pushed notification arrives on `events()` and is classified
    /// against the default registry.
    #[tokio::test(flavor = "multi_thread")]
    async fn server_notification_pumps_through_events_stream() {
        let (backend_writer, _server_reader) = duplex(64 * 1024);
        let (mut server_writer, backend_reader) = duplex(64 * 1024);

        let backend = ClaudeBackend::from_async_pipe(backend_reader, backend_writer);
        let mut events = backend.events();

        // Push a notification from the server side.
        let mock = tokio::spawn(async move {
            let mut writer = NdjsonWriter::new(&mut server_writer);
            writer
                .write_message(&JsonRpcMessage::new(json!({
                    "jsonrpc": "2.0",
                    "method": "claude/message_delta",
                    "params": {"delta": "world"},
                })))
                .await
                .unwrap();
            // Also push an unknown method to verify the fallback path.
            writer
                .write_message(&JsonRpcMessage::new(json!({
                    "jsonrpc": "2.0",
                    "method": "claude/totally_new",
                    "params": {"x": 1},
                })))
                .await
                .unwrap();
        });

        let n1 = tokio::time::timeout(Duration::from_secs(5), events.next())
            .await
            .expect("first notification arrives")
            .expect("non-None");
        assert_eq!(n1.method(), "claude/message_delta");
        assert!(!n1.is_unknown(), "default registry must recognise message_delta");

        let n2 = tokio::time::timeout(Duration::from_secs(5), events.next())
            .await
            .expect("second notification arrives")
            .expect("non-None");
        assert_eq!(n2.method(), "claude/totally_new");
        assert!(n2.is_unknown(), "unknown method must classify as Unknown");

        tokio::time::timeout(Duration::from_secs(5), mock)
            .await
            .expect("mock completes")
            .expect("mock ok");
    }
}
