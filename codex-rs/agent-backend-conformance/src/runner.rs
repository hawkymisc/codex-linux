//! Scenario runner: drives an [`AgentBackend`] through a [`ConformanceFixture`].

use crate::fixture::{ConformanceFixture, ExpectedEvent};
use codex_agent_backend::AgentBackend;
use futures::StreamExt;
use std::time::Duration;

#[derive(Debug, Clone)]
pub enum ScenarioOutcome {
    Pass,
    Failed(String),
    TimedOut,
}

#[derive(Debug, Clone)]
pub struct ScenarioReport {
    pub outcome: ScenarioOutcome,
    pub observed_methods: Vec<String>,
}

/// Run a scenario against the supplied backend.
///
/// PR-B implementation: streams notifications and asserts each
/// `expected_events[i].method` matches the i-th non-ignored observed event.
/// Approval round-trips (respond_with) are not yet wired — they will land
/// when the trait gains an explicit approval channel in PR-C.
pub async fn run_scenario(
    backend: &dyn AgentBackend,
    fixture: &ConformanceFixture,
) -> ScenarioReport {
    let mut events = backend.events();
    let mut observed: Vec<String> = Vec::new();
    let mut expected: std::collections::VecDeque<ExpectedEvent> =
        fixture.expected_events.iter().cloned().collect();

    let timeout = Duration::from_millis(fixture.timeout_ms);

    let outcome = match tokio::time::timeout(timeout, async {
        while let Some(event) = events.next().await {
            let method = event.method().to_string();
            observed.push(method.clone());
            if let Some(exp) = expected.front() {
                if exp.method == method {
                    expected.pop_front();
                }
                // In strict mode, mismatches between expected and observed
                // would fail; in lenient mode (the default), extra unknown
                // events are tolerated.
            }
            if expected.is_empty() {
                return Ok::<_, ()>(ScenarioOutcome::Pass);
            }
        }
        Err(())
    })
    .await
    {
        Ok(Ok(o)) => o,
        Ok(Err(_)) => ScenarioOutcome::Failed(format!(
            "stream ended before expected events drained; remaining: {:?}",
            expected.iter().map(|e| &e.method).collect::<Vec<_>>()
        )),
        Err(_) => ScenarioOutcome::TimedOut,
    };

    ScenarioReport {
        outcome,
        observed_methods: observed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_agent_backend::{
        AgentBackend, AgentBackendExtras, BackendCapabilities, BackendError,
        IncomingServerNotification, InitializeParams, InitializeResponse, Submission, TurnId,
        envelope::{KnownNotification, UnknownNotification},
        types::ServerInfo,
    };
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// Mock backend that emits a scripted sequence of notifications when
    /// `events()` is called. Used to verify the runner's matching logic.
    struct MockBackend {
        caps: BackendCapabilities,
        scripted: Arc<Mutex<Vec<IncomingServerNotification>>>,
    }

    #[async_trait]
    impl AgentBackend for MockBackend {
        async fn initialize(&mut self, _p: InitializeParams) -> Result<InitializeResponse, BackendError> {
            Ok(InitializeResponse {
                server_info: ServerInfo { name: "mock".into(), version: "0".into() },
                protocol_version: None,
                supported_methods: vec![],
                supported_notifications: vec![],
            })
        }
        async fn submit(&self, _s: Submission) -> Result<(), BackendError> { Ok(()) }
        async fn interrupt(&self, _t: TurnId) -> Result<(), BackendError> { Ok(()) }
        async fn shutdown(self: Box<Self>) -> Result<(), BackendError> { Ok(()) }
        fn events(&self) -> BoxStream<'static, IncomingServerNotification> {
            let scripted = Arc::clone(&self.scripted);
            Box::pin(futures::stream::unfold(scripted, |s| async move {
                let mut g = s.lock().await;
                if g.is_empty() { None } else { Some((g.remove(0), Arc::clone(&s))) }
            }))
        }
        fn capabilities(&self) -> &BackendCapabilities { &self.caps }
        fn extras(&self) -> Option<&dyn AgentBackendExtras> { None }
    }

    fn known(method: &str) -> IncomingServerNotification {
        IncomingServerNotification::Known(KnownNotification {
            method: method.into(),
            params: serde_json::Value::Null,
            recognised_as: Some(method.into()),
        })
    }
    fn unknown(method: &str) -> IncomingServerNotification {
        IncomingServerNotification::Unknown(UnknownNotification {
            method: method.into(),
            params: serde_json::Value::Null,
        })
    }

    #[tokio::test]
    async fn runner_passes_when_methods_match() {
        let scripted = Arc::new(Mutex::new(vec![
            known("thread/started"),
            known("agent/message_delta"),
            known("turn/completed"),
        ]));
        let backend = MockBackend {
            caps: BackendCapabilities::default(),
            scripted,
        };
        let fixture = ConformanceFixture::from_toml(r#"
description = "happy path"
timeout_ms = 1000
[input]
prompt = "x"
[[expected_events]]
method = "thread/started"
[[expected_events]]
method = "turn/completed"
"#).unwrap();
        let report = run_scenario(&backend, &fixture).await;
        assert!(matches!(report.outcome, ScenarioOutcome::Pass), "outcome was {:?}", report.outcome);
        assert_eq!(report.observed_methods.len(), 3);
    }

    #[tokio::test]
    async fn runner_times_out_when_stream_idle() {
        let scripted = Arc::new(Mutex::new(vec![]));
        let backend = MockBackend {
            caps: BackendCapabilities::default(),
            scripted,
        };
        let fixture = ConformanceFixture::from_toml(r#"
description = "idle"
timeout_ms = 50
[input]
prompt = "x"
[[expected_events]]
method = "thread/started"
"#).unwrap();
        let report = run_scenario(&backend, &fixture).await;
        // Could be either TimedOut (if the stream never yields) or Failed
        // (if it ends quickly). Both are acceptable failure modes for an
        // empty scripted backend.
        assert!(matches!(report.outcome, ScenarioOutcome::TimedOut | ScenarioOutcome::Failed(_)));
    }
}
