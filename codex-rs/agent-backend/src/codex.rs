//! [`CodexBackend`] — an [`AgentBackend`] implementation backed by Codex's
//! `app-server` JSON-RPC dialect.
//!
//! # Why this lives in `codex-agent-backend` and not its own crate
//!
//! The desktop UI's three-process model is symmetric across backends: it
//! always speaks NDJSON JSON-RPC over an `AsyncRead + AsyncWrite` pair. The
//! Codex case differs only in two places:
//!
//! 1. The translation between [`Submission`] / [`InitializeParams`] and the
//!    typed `codex-app-server-protocol` envelopes that the real app-server
//!    expects.
//! 2. The optional in-process startup path that wraps `codex-core`'s
//!    [`InProcessAppServerClient`].
//!
//! Both of those concerns are small enough to belong in this crate as a
//! sibling to [`crate::process::ProcessBackend`]. The heavy in-process path
//! is gated behind the `in-process` Cargo feature so default builds (and CI
//! environments without `libcap-dev` etc.) stay lean.
//!
//! # Constructors
//!
//! * [`CodexBackend::from_async_pipe`] — always available. Drives any
//!   `AsyncRead + AsyncWrite` pair using the same NDJSON framing the real
//!   `app-server` and `codex-agent` role both speak. This is the load-bearing
//!   constructor for the desktop UI when wiring to a child process or an
//!   in-memory duplex.
//! * `CodexBackend::start_in_process` — gated behind `feature = "in-process"`
//!   (deferred — see PR-E). Wraps a fully-fledged in-process app-server.
//!
//! # Design
//!
//! Internally the backend looks identical to [`ProcessBackend`]: an outbound
//! `Mutex<NdjsonWriter>`, an inbound reader task that drains the connection
//! and dispatches each frame into either a `pending` request map (response)
//! or a `broadcast` channel (notification). Calling
//! [`AgentBackend::events`] subscribes a fresh broadcast receiver and wraps
//! it in a [`BoxStream`]. Lagged subscribers (a UI consumer that fell behind
//! the broadcast capacity) silently drop missed notifications rather than
//! terminating the stream — matching `ProcessBackend`'s behaviour and the
//! "best-effort" classification of most server notifications in
//! `codex-app-server-client::server_notification_requires_delivery`.

use crate::{
    AgentBackend, BackendCapabilities, BackendError, IncomingServerNotification, InitializeParams,
    InitializeResponse, Submission, TurnId, envelope::KnownVariantRegistry,
};
use async_trait::async_trait;
use codex_jsonrpc_framing::{JsonRpcMessage, NdjsonReader, NdjsonWriter};
use futures::stream::{BoxStream, StreamExt};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tracing::{debug, error, warn};

/// Default capacity of the broadcast channel that fans inbound notifications
/// out to subscribers.
///
/// Sized to absorb a burst of stream tokens from a single in-flight turn
/// without lagging. Mirrors the constant used by [`crate::process`] so the
/// two backends behave identically under load.
const DEFAULT_BROADCAST_CAPACITY: usize = 128;

/// Type alias for the pending-request map.
///
/// Keyed by JSON-RPC request id (always a string in this crate's wire form);
/// the oneshot carries either the decoded `result` value or a
/// [`BackendError::Rejected`] for an error response.
type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, BackendError>>>>>;

/// Outbound write request sent to the writer actor.
///
/// The actor owns the [`NdjsonWriter`] outright so we never need to hold a
/// `tokio::sync::MutexGuard` across an `.await` (which the workspace's
/// `clippy::await-holding-invalid-type` lint forbids). The `ack` channel
/// carries the I/O result so the caller can surface
/// [`BackendError::Transport`] for write errors.
struct WriteJob {
    /// JSON value to frame and write.
    value: Value,
    /// One-shot reply channel for the I/O result.
    ack: oneshot::Sender<Result<(), std::io::Error>>,
}

/// [`AgentBackend`] implementation that speaks NDJSON JSON-RPC against any
/// `AsyncRead + AsyncWrite` pair.
///
/// Construct with [`CodexBackend::from_async_pipe`]; the constructor spawns a
/// reader task and a writer task, returning immediately. The instance is
/// ready for [`AgentBackend::initialize`] once the constructor returns.
pub struct CodexBackend {
    /// Pending-request map (request id → oneshot sender).
    pending: PendingMap,
    /// Outbound write channel. A single writer actor task drains this and
    /// frames every value into the underlying NDJSON writer; using a channel
    /// instead of a `Mutex<NdjsonWriter>` avoids holding a guard across
    /// `.await` boundaries (forbidden by `clippy::await_holding_invalid_type`
    /// at the workspace level).
    writer_tx: mpsc::Sender<WriteJob>,
    /// Notification fan-out. Cloned once per [`AgentBackend::events`] caller.
    notif_tx: broadcast::Sender<IncomingServerNotification>,
    /// Capability snapshot populated by [`AgentBackend::initialize`].
    capabilities: BackendCapabilities,
    /// Monotonic id counter for outbound JSON-RPC requests.
    next_id: Arc<Mutex<u64>>,
    /// Static set of notification methods this client recognises. Anything
    /// outside this set is surfaced as
    /// [`IncomingServerNotification::Unknown`] so the desktop drift log
    /// (codex-drift-log) can record it for protocol-evolution diagnostics.
    registry: Arc<KnownVariantRegistry>,
}

impl CodexBackend {
    /// Returns the default registry of method names the agent role
    /// (`codex-rs/desktop/src/agent_role.rs`) is known to emit. Adding a new
    /// notification method here is the gate to surfacing it as a typed
    /// [`IncomingServerNotification::Known`] variant; anything else falls
    /// through to the [`IncomingServerNotification::Unknown`] preservation
    /// path and the desktop drift log.
    pub fn default_registry() -> KnownVariantRegistry {
        KnownVariantRegistry::new()
            .with_methods(["agent/message_delta", "agent/turn_completed"])
    }

    /// Construct a backend that speaks NDJSON JSON-RPC over the supplied
    /// reader/writer pair.
    ///
    /// Uses [`Self::default_registry`] as the recognised-methods set. Use
    /// [`Self::from_async_pipe_with_registry`] to override.
    ///
    /// Spawns a tokio task that drains `reader` until EOF, dispatching each
    /// frame to either a pending request waiter or the notification
    /// broadcast. The task terminates on EOF or transport error; subsequent
    /// `request()` calls fail with [`BackendError::Closed`].
    pub fn from_async_pipe<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        Self::from_async_pipe_with_registry(reader, writer, Self::default_registry())
    }

    /// Like [`Self::from_async_pipe`] but with an explicit
    /// [`KnownVariantRegistry`]. Tests pass an empty registry to get the
    /// "everything is Unknown" pre-PR-W behaviour; production callers can
    /// extend the default registry to cover backend-specific methods.
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

        // Spawn a writer actor that owns the NdjsonWriter outright.
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

    /// Borrow the active recognised-methods registry. Mostly useful for
    /// tests asserting that the default set covers the expected methods.
    pub fn registry(&self) -> &KnownVariantRegistry {
        &self.registry
    }

    /// Allocate a fresh request id for outbound JSON-RPC requests.
    ///
    /// Wire form is `r{n}` to match the convention used by
    /// [`crate::process::ProcessBackend`] — uniqueness within a single
    /// backend instance is sufficient because the pending map is keyed by
    /// string and each instance owns its own counter.
    async fn next_request_id(&self) -> String {
        let mut g = self.next_id.lock().await;
        *g += 1;
        format!("r{}", *g)
    }

    /// Send a JSON-RPC request and await its matching response.
    ///
    /// Returns [`BackendError::Closed`] if the reader task has gone away
    /// before the response arrives, [`BackendError::Transport`] for write
    /// errors, and [`BackendError::Rejected`] for explicit JSON-RPC error
    /// replies.
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

    /// Hand a JSON value to the writer actor and await the I/O ack.
    ///
    /// Splitting this out keeps [`Self::request`] short and lets future
    /// notification-style callers (e.g. client-to-server JSON-RPC
    /// notifications) reuse the same path.
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

/// Background writer task: drain the outbound write channel, framing each
/// value into the underlying NDJSON writer.
///
/// Owning the writer in a single task removes the need for a `Mutex` guard
/// across `.await` (forbidden by `clippy::await_holding_invalid_type`). The
/// task terminates when the corresponding [`mpsc::Sender`] is dropped.
async fn writer_loop<W>(mut writer: NdjsonWriter<W>, mut rx: mpsc::Receiver<WriteJob>)
where
    W: AsyncWrite + Unpin,
{
    while let Some(job) = rx.recv().await {
        let result = writer
            .write_message(&JsonRpcMessage::new(job.value))
            .await
            .map_err(|e| std::io::Error::other(e.to_string()));
        // The caller may have dropped the ack receiver (e.g. they were
        // cancelled by a timeout); silently swallow the send failure.
        let _ = job.ack.send(result);
    }
}

/// Background reader task: pull framed messages off `reader`, dispatch each
/// to either the pending-request map or the notification fan-out.
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
                debug!("CodexBackend: reader EOF");
                break;
            }
            Err(e) => {
                error!("CodexBackend: read error: {e}");
                break;
            }
        };
        dispatch_incoming(msg.into_value(), &pending, &notif_tx, &registry).await;
    }
}

/// Pure-ish dispatch of a single inbound JSON value.
///
/// Extracted from [`reader_loop`] so the interesting branch logic — "is
/// this a response, a notification, or malformed?" — can be unit tested
/// without spinning up a real transport.
///
/// Notifications get split into [`IncomingServerNotification::Known`] and
/// [`IncomingServerNotification::Unknown`] via
/// [`IncomingServerNotification::classify`] using the supplied `registry`.
/// Tests that want the legacy "everything is Unknown" behaviour can pass an
/// empty [`KnownVariantRegistry`].
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
        // Response (has id, no method) — route to pending oneshot.
        (Some(id), None) => deliver_response(id, &value, pending).await,
        // Some servers echo back `method` on responses (e.g. typed JSON-RPC
        // dialects). When the id is in our pending map, treat the message as
        // a response regardless of whether `method` is present.
        (Some(id), Some(_)) => {
            let is_pending = pending.lock().await.contains_key(&id);
            if is_pending {
                deliver_response(id, &value, pending).await;
            } else {
                // A request *from* the server with an id is the JSON-RPC
                // server-request shape. We don't dispatch those yet; route
                // it through the classifier so the desktop drift log can
                // surface the protocol drift rather than silently swallowing
                // the message.
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
            debug!("CodexBackend: ignored malformed message");
        }
    }
}

/// Resolve a pending-request oneshot for `id` with the result extracted from
/// `value`. A missing entry is logged but not fatal.
async fn deliver_response(id: String, value: &Value, pending: &PendingMap) {
    if let Some(tx) = pending.lock().await.remove(&id) {
        let outcome = if let Some(err) = value.get("error") {
            Err(BackendError::Rejected(err.to_string()))
        } else {
            Ok(value.get("result").cloned().unwrap_or(Value::Null))
        };
        let _ = tx.send(outcome);
    } else {
        warn!(?id, "CodexBackend: response for unknown id");
    }
}

#[async_trait]
impl AgentBackend for CodexBackend {
    async fn initialize(
        &mut self,
        p: InitializeParams,
    ) -> Result<InitializeResponse, BackendError> {
        let result = self.request("initialize", serde_json::to_value(p)?).await?;
        let resp: InitializeResponse = serde_json::from_value(result)?;
        // Mirror the negotiated capability lists onto our snapshot so the UI
        // can read them through `AgentBackend::capabilities` without a second
        // network round-trip.
        self.capabilities = BackendCapabilities {
            protocol_version: resp.protocol_version.clone(),
            supported_methods: resp.supported_methods.clone(),
            supported_notifications: resp.supported_notifications.clone(),
        };
        Ok(resp)
    }

    async fn submit(&self, sub: Submission) -> Result<(), BackendError> {
        // The agent role accepts the entire `Submission` envelope as the
        // `params` for `submit`; backends that need to translate to a typed
        // `userTurn` request can override this in a subclass crate.
        self.request("submit", serde_json::to_value(sub)?).await?;
        Ok(())
    }

    async fn interrupt(&self, turn_id: TurnId) -> Result<(), BackendError> {
        self.request("interrupt", json!({ "turn_id": turn_id }))
            .await?;
        Ok(())
    }

    async fn shutdown(self: Box<Self>) -> Result<(), BackendError> {
        // Best-effort RPC shutdown: the server may already have torn down,
        // in which case we just drop the broadcast sender below.
        let _ = self.request("shutdown", Value::Null).await;
        // Dropping `self` releases the broadcast sender, which terminates
        // every outstanding `events()` stream.
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

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tokio::sync::oneshot;

    fn make_pending() -> PendingMap {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn empty_registry() -> KnownVariantRegistry {
        KnownVariantRegistry::new()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_routes_response_to_pending_oneshot() {
        let pending = make_pending();
        let (tx, rx) = oneshot::channel();
        pending.lock().await.insert("r1".to_string(), tx);

        let (notif_tx, _notif_rx) = broadcast::channel(8);
        let value = json!({"jsonrpc": "2.0", "id": "r1", "result": {"ok": true}});
        dispatch_incoming(value, &pending, &notif_tx, &empty_registry()).await;

        let got = rx.await.unwrap_or_else(|_| panic!("oneshot dropped"));
        let v = got.unwrap_or_else(|_| panic!("expected Ok outcome"));
        assert_eq!(v, json!({"ok": true}));
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
        dispatch_incoming(value, &pending, &notif_tx, &empty_registry()).await;

        let got = rx.await.unwrap_or_else(|_| panic!("oneshot dropped"));
        match got {
            Err(BackendError::Rejected(msg)) => assert!(msg.contains("bad"), "msg = {msg}"),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_routes_unknown_notification_to_broadcast() {
        let pending = make_pending();
        let (notif_tx, mut notif_rx) = broadcast::channel(8);
        let value = json!({
            "jsonrpc": "2.0",
            "method": "agent/foo",
            "params": {"hello": "world"},
        });
        dispatch_incoming(value, &pending, &notif_tx, &empty_registry()).await;

        let got = notif_rx
            .recv()
            .await
            .unwrap_or_else(|_| panic!("recv failed"));
        assert_eq!(got.method(), "agent/foo");
        assert_eq!(got.params(), &json!({"hello": "world"}));
        assert!(got.is_unknown());
    }

    /// PR-W: notifications whose `method` is in the registry get classified
    /// as `Known` rather than falling through to `Unknown`. The default
    /// registry covers the agent role's published methods.
    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_classifies_known_notification_against_registry() {
        let pending = make_pending();
        let (notif_tx, mut notif_rx) = broadcast::channel(8);
        let value = json!({
            "jsonrpc": "2.0",
            "method": "agent/message_delta",
            "params": {"delta": "hi"},
        });
        let registry = CodexBackend::default_registry();
        dispatch_incoming(value, &pending, &notif_tx, &registry).await;

        let got = notif_rx
            .recv()
            .await
            .unwrap_or_else(|_| panic!("recv failed"));
        assert_eq!(got.method(), "agent/message_delta");
        assert!(!got.is_unknown(), "agent/message_delta must classify as Known");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn default_registry_covers_agent_role_published_methods() {
        let r = CodexBackend::default_registry();
        // These two are the methods agent_role.rs (PR-B) emits today.
        assert!(r.contains("agent/message_delta"));
        assert!(r.contains("agent/turn_completed"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_handles_response_with_method_field() {
        // Some dialects echo `method` on responses. Our pending map is the
        // authoritative routing signal, so this should resolve the oneshot
        // rather than fall through to the broadcast channel.
        let pending = make_pending();
        let (tx, rx) = oneshot::channel();
        pending.lock().await.insert("r1".to_string(), tx);

        let (notif_tx, mut notif_rx) = broadcast::channel(8);
        let value = json!({
            "jsonrpc": "2.0",
            "id": "r1",
            "method": "submit",
            "result": {"accepted": true},
        });
        dispatch_incoming(value, &pending, &notif_tx, &empty_registry()).await;

        let got = rx.await.unwrap_or_else(|_| panic!("oneshot dropped"));
        let v = got.unwrap_or_else(|_| panic!("expected Ok outcome"));
        assert_eq!(v, json!({"accepted": true}));
        // The broadcast channel must remain empty.
        match notif_rx.try_recv() {
            Err(broadcast::error::TryRecvError::Empty) => {}
            other => panic!("expected empty, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_ignores_malformed_message() {
        let pending = make_pending();
        let (notif_tx, mut notif_rx) = broadcast::channel(8);
        dispatch_incoming(json!({"junk": true}), &pending, &notif_tx, &empty_registry()).await;
        match notif_rx.try_recv() {
            Err(broadcast::error::TryRecvError::Empty) => {}
            other => panic!("expected empty, got {other:?}"),
        }
    }
}
