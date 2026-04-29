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

use std::process::Stdio;

use anyhow::{Context, Result};
use codex_agent_backend::{
    AgentBackend, ClientInfo, CodexBackend, IncomingServerNotification, InitializeParams,
    Submission, SubmissionId, ThreadId,
};
use futures::StreamExt;
use serde_json::json;
use tokio::process::{Child, Command};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, error, warn};

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

    // Subscribe to backend notifications BEFORE we spawn the submit task
    // so we don't race the first message_delta. The notification stream
    // outlives the submission task by design — even after the user stops
    // sending, the agent may still emit trailing notifications.
    let mut events = backend.events();

    let backend = std::sync::Arc::new(backend);

    // Pump user submissions onto the backend.
    let submit_backend = backend.clone();
    let submit_tx_done = events_tx.clone();
    let submit_task = tokio::spawn(async move {
        let mut rx = submit_rx;
        let mut counter: u64 = 0;
        while let Some(prompt) = rx.recv().await {
            counter += 1;
            let submission = Submission {
                id: SubmissionId::from(format!("s{counter}")),
                thread_id: ThreadId::from("default"),
                payload: json!({ "text": prompt }),
            };
            if let Err(err) = submit_backend.submit(submission).await {
                warn!(error = %err, "agent_bridge: submit failed");
                let _ = submit_tx_done.send(BridgeEvent::AgentClosed);
                break;
            }
        }
    });

    // Pump notifications into the UI channel until the stream ends.
    while let Some(notification) = events.next().await {
        if let Some(event) = classify_notification(&notification) {
            if events_tx.send(event).is_err() {
                debug!("agent_bridge: UI receiver dropped, stopping pump");
                break;
            }
        } else {
            debug!(method = %notification.method(), "agent_bridge: ignoring unmapped notification");
        }
    }

    // Stream ended — either EOF on the child or the receiver was dropped.
    // Tell the UI and let everything wind down.
    let _ = events_tx.send(BridgeEvent::AgentClosed);
    submit_task.abort();
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
    use super::*;
    use codex_agent_backend::UnknownNotification;
    use serde_json::json;

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
}
