//! Bridge between the GTK main thread and a tokio runtime that drives a
//! [`CodexBackend`] over a child `codex-agent` process.
//!
//! The bridge owns three actors:
//!
//! 1. A submission task that drains an mpsc channel and forwards each user
//!    prompt as an [`AgentBackend::submit`] call.
//! 2. A notification task that subscribes to [`CodexBackend::events`] and
//!    classifies each notification into a [`BridgeEvent`] for the UI.
//! 3. The child `codex-agent` process itself, spawned via `tokio::process`.
//!
//! The GTK side never touches `tokio` directly — it just calls
//! [`AgentBridge::submit`] (non-blocking) and drains the receiver returned
//! by [`AgentBridge::take_events_rx`] from `glib::MainContext::spawn_local`.
//!
//! # Why this lives in a separate module
//!
//! Wiring up an `Arc<dyn AgentBackend>` directly in the GUI module would
//! force every `gui::*` file to depend on tokio types that have nothing
//! to do with widget construction. Keeping all the runtime plumbing here
//! lets `gui::app` stay thin and lets future PRs replace the in-tree child
//! process with an in-process backend without touching widget code.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use codex_agent_backend::{
    AgentBackend, ClientInfo, CodexBackend, IncomingServerNotification, InitializeParams,
    Submission, SubmissionId, ThreadId,
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::process::{Child, Command};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, error, warn};

use crate::wal::{RecordKind, TurnLog, WalConfig, WalManager};

/// Default thread id used by [`AgentBridge`] until the protocol exposes a
/// real per-conversation thread id. Mirrors the value used in submissions.
const DEFAULT_THREAD_ID: &str = "default";

/// Events surfaced by the bridge to the GTK side.
///
/// The variants intentionally mirror the agent-role's notification
/// methods so the UI never has to peek at JSON. New variants will be added
/// as the agent role grows; consumers should treat unknown notifications
/// as a soft signal and continue running.
#[derive(Debug, Clone)]
pub enum BridgeEvent {
    /// A streamed `agent/message_delta` chunk.
    MessageDelta { text: String },
    /// The `agent/turn_completed` terminal notification.
    TurnCompleted { stop_reason: String },
    /// The agent process exited (or its event stream closed).
    AgentClosed,
}

/// CBOR payload appended for each user-submitted prompt.
///
/// The wal module treats record payloads as opaque bytes, so we wrap our
/// per-kind structures in tiny `Serialize`/`Deserialize` types that the
/// replay path (or future migration tooling) can decode directly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WalUserOp {
    prompt: String,
    ts_unix_ms: u128,
}

/// CBOR payload for one server notification we want to durably log.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WalNotif {
    method: String,
    /// JSON-encoded notification params. Stored as a string rather than a
    /// nested CBOR value so replay tooling can re-serialise to JSON without
    /// re-introducing a `serde_json::Value` schema dependency.
    params_json: String,
    ts_unix_ms: u128,
}

/// Single-owner wrapper around a [`WalManager`] and the currently-open
/// [`TurnLog`] (if any). Lives inside the supervisor task so all WAL writes
/// happen from one thread; sync `std::fs` is fine because the only fsync
/// call is on durability boundaries (TurnCompleted / ApprovalDecision).
struct WalSink {
    manager: WalManager,
    current: Option<TurnLog>,
    thread_id: String,
}

impl WalSink {
    /// Construct a sink rooted at `home` (which becomes
    /// `<home>/.local/state/codex-desktop/turns/`). Runs `WalManager::gc()`
    /// once on construction to lazily prune anything past retention or
    /// quota.
    fn new(home: PathBuf, thread_id: String) -> Result<Self> {
        let cfg = WalConfig::defaults_under(&home);
        // Ensure the root directory exists so the gc + first open_turn
        // succeed cleanly on a brand-new install. This also turns a
        // fundamentally bad `home` (e.g. a regular file) into an early
        // error rather than letting it surface on the first user submit.
        std::fs::create_dir_all(&cfg.root)
            .with_context(|| format!("create WAL root {}", cfg.root.display()))?;
        let manager = WalManager::new(cfg);
        if let Err(err) = manager.gc() {
            warn!(error = %err, "agent_bridge: WAL gc failed on startup");
        }
        Ok(Self {
            manager,
            current: None,
            thread_id,
        })
    }

    /// Append a `UserOp` record. Opens a fresh [`TurnLog`] if none is open.
    fn record_user_op(&mut self, turn_id: &str, prompt: &str) -> Result<()> {
        let log = self.ensure_open(turn_id)?;
        let payload = WalUserOp {
            prompt: prompt.to_owned(),
            ts_unix_ms: now_unix_ms(),
        };
        log.append(RecordKind::UserOp, &payload)
    }

    /// Append a `ServerNotification` record. Opens a fresh [`TurnLog`] if
    /// none is open.
    fn record_notification(
        &mut self,
        turn_id: &str,
        method: &str,
        params_json: &str,
    ) -> Result<()> {
        let log = self.ensure_open(turn_id)?;
        let payload = WalNotif {
            method: method.to_owned(),
            params_json: params_json.to_owned(),
            ts_unix_ms: now_unix_ms(),
        };
        log.append(RecordKind::ServerNotification, &payload)
    }

    /// Mark the current turn complete: rename `<turn>.wal` to
    /// `<turn>.wal.done`. Idempotent — returns Ok if no turn is open.
    fn complete_turn(&mut self) -> Result<()> {
        match self.current.take() {
            None => Ok(()),
            Some(log) => log.complete().map(|_| ()),
        }
    }

    fn ensure_open(&mut self, turn_id: &str) -> Result<&mut TurnLog> {
        // `Option::get_or_insert_with` doesn't compose with a fallible
        // factory, so we open up-front and assign. `Option::insert` returns
        // a `&mut T` directly, side-stepping the `expect()` lint.
        match self.current {
            Some(ref mut log) => Ok(log),
            None => {
                let log = self.manager.open_turn(&self.thread_id, turn_id)?;
                Ok(self.current.insert(log))
            }
        }
    }
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Resolve the user's home directory for WAL storage. Honours `$HOME`
/// when set, falling back to the current dir if not — the supervisor
/// will surface any write errors as warnings without crashing the bridge.
fn resolve_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Handle held by the GUI side. Cheap to clone via [`std::rc::Rc`].
pub struct AgentBridge {
    /// Outbound channel for user prompts. Produces a [`Submission`] which
    /// the submission task forwards to [`AgentBackend::submit`].
    submit_tx: mpsc::UnboundedSender<String>,
    /// One-shot extraction slot for the inbound event receiver. Wrapped in
    /// a [`std::sync::Mutex`] so [`Self::take_events_rx`] only takes the
    /// receiver once even if the bridge is shared.
    events_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<BridgeEvent>>>,
    /// Join handle for the notification-forwarding task.
    ///
    /// The leading underscore signals "owned for drop semantics" — when the
    /// bridge is dropped the handle is dropped, which lets tokio reclaim
    /// the task slot. The submission task is kept alive by the channel
    /// receiver and shuts down on its own when [`Self::submit_tx`] drops.
    _shutdown_handle: JoinHandle<()>,
}

impl AgentBridge {
    /// Spawn the in-tree `codex-agent` child and start listening for events.
    ///
    /// All async work is spawned onto `rt`, so this constructor is safe to
    /// call from the GTK main thread (which has no tokio runtime of its
    /// own). The returned bridge is ready for [`Self::submit`] immediately;
    /// the initialize handshake runs concurrently in the background.
    pub fn spawn(rt: Handle) -> Result<Self> {
        let exe = std::env::current_exe()
            .context("agent_bridge: cannot resolve current executable path")?;

        let mut cmd = Command::new(&exe);
        // Set argv[0] to "codex-agent" so the role-detection logic in
        // `main.rs` selects the agent role. `current_exe()` gives an
        // absolute path which we keep as the `program` so the child
        // continues to load the same binary; `arg0` only renames argv[0].
        cmd.arg0("codex-agent")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);

        let (submit_tx, submit_rx) = mpsc::unbounded_channel::<String>();
        let (events_tx, events_rx) = mpsc::unbounded_channel::<BridgeEvent>();

        // Spawn the supervisor task on the tokio runtime; it owns the
        // child process and the backend instance for the lifetime of the
        // bridge.
        let supervisor = rt.spawn(supervisor(cmd, submit_rx, events_tx));

        Ok(Self {
            submit_tx,
            events_rx: std::sync::Mutex::new(Some(events_rx)),
            _shutdown_handle: supervisor,
        })
    }

    /// Send a user-typed prompt. Non-blocking; returns immediately.
    ///
    /// If the bridge has already shut down (the supervisor task dropped
    /// the receiver) the prompt is silently discarded after a `tracing`
    /// warning. The UI will see the failure as an [`BridgeEvent::AgentClosed`]
    /// event on the inbound stream.
    pub fn submit(&self, prompt: String) {
        if let Err(err) = self.submit_tx.send(prompt) {
            warn!(
                error = %err,
                "agent_bridge: submit dropped — supervisor task is gone"
            );
        }
    }

    /// Take the inbound event receiver. Returns [`None`] on subsequent
    /// calls.
    ///
    /// The caller owns the receiver and is expected to drain it from the
    /// GTK main loop via `glib::MainContext::spawn_local`.
    pub fn take_events_rx(&self) -> Option<mpsc::UnboundedReceiver<BridgeEvent>> {
        match self.events_rx.lock() {
            Ok(mut g) => g.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        }
    }
}

/// Run the bridge supervisor: spawn the child, drive the backend, and pump
/// notifications into the UI channel until either side closes.
///
/// This function is the bridge's hot path. It owns the `CodexBackend` for
/// its entire lifetime and exits cleanly when the user-prompt channel is
/// closed (e.g. the GUI dropped the bridge) or when the child stdout
/// reaches EOF (the agent process exited).
async fn supervisor(
    mut cmd: Command,
    submit_rx: mpsc::UnboundedReceiver<String>,
    events_tx: mpsc::UnboundedSender<BridgeEvent>,
) {
    let mut child: Child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) => {
            error!(error = %err, "agent_bridge: failed to spawn codex-agent child");
            let _ = events_tx.send(BridgeEvent::AgentClosed);
            return;
        }
    };

    let stdin = match child.stdin.take() {
        Some(s) => s,
        None => {
            error!("agent_bridge: child has no stdin pipe");
            let _ = events_tx.send(BridgeEvent::AgentClosed);
            return;
        }
    };
    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            error!("agent_bridge: child has no stdout pipe");
            let _ = events_tx.send(BridgeEvent::AgentClosed);
            return;
        }
    };

    let mut backend = CodexBackend::from_async_pipe(stdout, stdin);

    // Send `initialize` immediately. Failure here is non-fatal for the UI;
    // we surface it as a warning and let the user discover the broken
    // backend through the AgentClosed event we send below if the child
    // crashes outright.
    let init = InitializeParams {
        client_info: ClientInfo {
            name: "codex-desktop".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
        protocol_version: None,
        supported_methods: Vec::new(),
    };
    if let Err(err) = backend.initialize(init).await {
        warn!(error = %err, "agent_bridge: initialize failed");
    }

    // Subscribe to backend notifications BEFORE we start pumping submits
    // so we don't race the first message_delta. The notification stream
    // outlives the submission stream by design — even after the user
    // stops sending, the agent may still emit trailing notifications.
    let mut events = backend.events();

    let backend = std::sync::Arc::new(backend);

    // Construct the WAL sink in the supervisor task. Failure to set up
    // the WAL is non-fatal: we log a warning and run without durable
    // logging rather than refusing to start the bridge.
    let mut wal_sink: Option<WalSink> = match WalSink::new(resolve_home(), DEFAULT_THREAD_ID.into()) {
        Ok(s) => Some(s),
        Err(err) => {
            warn!(
                error = %err,
                "agent_bridge: WAL sink unavailable; turns will not be durably logged"
            );
            None
        }
    };

    // Track the most-recent submit id so we can route notifications to
    // the right per-turn WAL file. Until the protocol exposes a real turn
    // id we synthesise one from the submit counter (`s{n}`), which is the
    // same id we sent on the wire.
    let mut current_turn_id: Option<String> = None;
    let mut submit_counter: u64 = 0;

    // Drive submissions and notifications from a single task — this keeps
    // `WalSink` single-owner (sync `std::fs` is fine) and avoids any
    // cross-task locking on the hot path.
    let mut submit_rx = submit_rx;
    loop {
        tokio::select! {
            // User prompt -> WAL UserOp record + backend submission.
            maybe_prompt = submit_rx.recv() => {
                let Some(prompt) = maybe_prompt else {
                    // GUI dropped the bridge; stop pumping. We still want
                    // to drain any pending notifications so we exit the
                    // outer loop and let the agent_closed branch fire on
                    // child EOF below.
                    break;
                };
                submit_counter += 1;
                let submission_id = format!("s{submit_counter}");
                if let Some(sink) = wal_sink.as_mut()
                    && let Err(err) = sink.record_user_op(&submission_id, &prompt)
                {
                    warn!(error = %err, "agent_bridge: WAL record_user_op failed");
                }
                current_turn_id = Some(submission_id.clone());
                let submission = Submission {
                    id: SubmissionId::from(submission_id),
                    thread_id: ThreadId::from(DEFAULT_THREAD_ID),
                    payload: json!({ "text": prompt }),
                };
                if let Err(err) = backend.submit(submission).await {
                    warn!(error = %err, "agent_bridge: submit failed");
                    let _ = events_tx.send(BridgeEvent::AgentClosed);
                    break;
                }
            }
            // Server notification -> WAL ServerNotification + UI event.
            maybe_notif = events.next() => {
                let Some(notification) = maybe_notif else {
                    // Backend stream closed (child EOF or receiver dropped).
                    break;
                };
                let method = notification.method().to_string();
                if method.starts_with("agent/")
                    && let Some(sink) = wal_sink.as_mut()
                {
                    // The latest user submission's id is the closest
                    // approximation of a turn id. If we haven't seen
                    // a submit yet, fall back to a sentinel so the
                    // notification still lands on disk somewhere
                    // recoverable.
                    let turn_id = current_turn_id.as_deref().unwrap_or("preboot");
                    let params_json = serde_json::to_string(notification.params())
                        .unwrap_or_else(|_| "null".to_string());
                    if let Err(err) = sink.record_notification(turn_id, &method, &params_json) {
                        warn!(error = %err, "agent_bridge: WAL record_notification failed");
                    }
                }
                let is_turn_complete = method == "agent/turn_completed";
                if let Some(event) = classify_notification(&notification) {
                    if events_tx.send(event).is_err() {
                        debug!("agent_bridge: UI receiver dropped, stopping pump");
                        break;
                    }
                } else {
                    debug!(
                        method = %notification.method(),
                        "agent_bridge: ignoring unmapped notification"
                    );
                }
                if is_turn_complete {
                    if let Some(sink) = wal_sink.as_mut()
                        && let Err(err) = sink.complete_turn()
                    {
                        warn!(error = %err, "agent_bridge: WAL complete_turn failed");
                    }
                    current_turn_id = None;
                }
            }
        }
    }

    // Stream ended — either EOF on the child or the receiver was dropped.
    // Tell the UI and let everything wind down. Any in-flight turn is
    // left as `<turn>.wal` (no `.done` rename); replay tooling treats
    // that as crash recovery.
    let _ = events_tx.send(BridgeEvent::AgentClosed);
    let _ = child.wait().await;
}

/// Classify a backend notification into a [`BridgeEvent`].
///
/// Returns [`None`] for notifications the UI does not currently surface;
/// the supervisor logs those at debug level rather than dropping them
/// silently so protocol drift is at least observable.
fn classify_notification(notification: &IncomingServerNotification) -> Option<BridgeEvent> {
    match notification.method() {
        "agent/message_delta" => {
            let text = notification
                .params()
                .get("delta")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            Some(BridgeEvent::MessageDelta { text })
        }
        "agent/turn_completed" => {
            let stop_reason = notification
                .params()
                .get("stop_reason")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            Some(BridgeEvent::TurnCompleted { stop_reason })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use codex_agent_backend::UnknownNotification;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn classify_message_delta() {
        let n = IncomingServerNotification::Unknown(UnknownNotification {
            method: "agent/message_delta".into(),
            params: json!({"delta": "hello"}),
        });
        match classify_notification(&n) {
            Some(BridgeEvent::MessageDelta { text }) => assert_eq!(text, "hello"),
            other => panic!("expected MessageDelta, got {other:?}"),
        }
    }

    #[test]
    fn classify_turn_completed() {
        let n = IncomingServerNotification::Unknown(UnknownNotification {
            method: "agent/turn_completed".into(),
            params: json!({"stop_reason": "end_turn"}),
        });
        match classify_notification(&n) {
            Some(BridgeEvent::TurnCompleted { stop_reason }) => {
                assert_eq!(stop_reason, "end_turn");
            }
            other => panic!("expected TurnCompleted, got {other:?}"),
        }
    }

    #[test]
    fn classify_unknown_method_returns_none() {
        let n = IncomingServerNotification::Unknown(UnknownNotification {
            method: "agent/something_new".into(),
            params: json!({}),
        });
        assert!(classify_notification(&n).is_none());
    }

    #[test]
    fn message_delta_missing_field_yields_empty_string() {
        let n = IncomingServerNotification::Unknown(UnknownNotification {
            method: "agent/message_delta".into(),
            params: json!({}),
        });
        match classify_notification(&n) {
            Some(BridgeEvent::MessageDelta { text }) => assert_eq!(text, ""),
            other => panic!("expected MessageDelta, got {other:?}"),
        }
    }

    #[test]
    fn wal_sink_records_user_op_and_notification() {
        use crate::wal::replay;

        let tmp = TempDir::new().unwrap();
        let home = tmp.path().to_path_buf();
        let mut sink = WalSink::new(home.clone(), "th-test".into())
            .expect("WalSink::new succeeds in writable tempdir");

        sink.record_user_op("s1", "hello world")
            .expect("record_user_op succeeds");
        sink.record_notification("s1", "agent/message_delta", "{\"delta\":\"hi\"}")
            .expect("record_notification succeeds");
        sink.complete_turn().expect("complete_turn succeeds");

        let done_path = home
            .join(".local/state/codex-desktop/turns")
            .join("th-test")
            .join("s1.wal.done");
        assert!(done_path.exists(), "expected {} to exist", done_path.display());

        let records = replay(&done_path).expect("replay succeeds");
        assert_eq!(records.len(), 2, "expected 2 records, got {}", records.len());
        assert_eq!(records[0].kind, RecordKind::UserOp);
        assert_eq!(records[1].kind, RecordKind::ServerNotification);

        // Verify the payloads round-trip via CBOR.
        let user_op: WalUserOp = ciborium::de::from_reader(records[0].payload.as_slice())
            .expect("decode WalUserOp");
        assert_eq!(user_op.prompt, "hello world");
        let notif: WalNotif = ciborium::de::from_reader(records[1].payload.as_slice())
            .expect("decode WalNotif");
        assert_eq!(notif.method, "agent/message_delta");
        assert_eq!(notif.params_json, "{\"delta\":\"hi\"}");

        // complete_turn is idempotent — calling again is a no-op.
        sink.complete_turn()
            .expect("second complete_turn returns Ok with no open log");
    }

    #[test]
    fn wal_sink_handles_failed_directory_gracefully() {
        // Pass a regular file as `home`. WalSink::new tries to create
        // `<home>/.local/state/codex-desktop/turns/`, which must fail since
        // the parent path component is a non-directory.
        let tmp = TempDir::new().unwrap();
        let bogus_home = tmp.path().join("not-a-directory");
        std::fs::write(&bogus_home, b"i am a file, not a dir").unwrap();

        let result = WalSink::new(bogus_home, "th-test".into());
        assert!(
            result.is_err(),
            "WalSink::new must fail when home is a regular file"
        );
    }
}
