//! Integration test for [`AgentBridge::restart`] (PR-S2).
//!
//! Spawns the in-tree binary in agent role via [`AgentBridge`], drives one
//! submit/event round-trip, calls `restart()`, then drives a second
//! round-trip — asserting that the new supervisor is alive, accepts a
//! prompt, and produces an `agent/message_delta` echo.
//!
//! The test exercises the bridge's public API end-to-end, including the
//! `events_tx`-clone-on-restart path (subscribers stay subscribed across
//! restarts) and the `kill_on_drop` teardown of the old grandchild.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use codex_desktop::agent_bridge::{AgentBridge, AgentCommand, BridgeEvent};
use tokio::runtime::Handle;
use tokio::time::timeout;

const RECV_TIMEOUT: Duration = Duration::from_secs(15);

fn test_command() -> Result<AgentCommand> {
    let bin = option_env!("CARGO_BIN_EXE_codex-desktop")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "CARGO_BIN_EXE_codex-desktop is not set; this test must be run via `cargo test`"
            )
        })?;
    Ok(AgentCommand {
        program: PathBuf::from(bin),
        // The test binary doesn't go through main.rs's argv[0] dispatch
        // unless we force the role via the env var. Mirrors the pattern in
        // tests/agent_bridge_smoke.rs.
        arg0: "codex-desktop".to_string(),
        envs: vec![("CODEX_DESKTOP_FORCE_ROLE".into(), "agent".into())],
    })
}

/// Drain `events_rx` until we see one of:
///   - a `MessageDelta` whose text contains `expect_substr`, OR
///   - a `TurnCompleted`, OR
///   - the channel closes / the wall-clock budget expires.
///
/// Returns the most-recent `MessageDelta` text seen along the way (or empty
/// string if none).
async fn drain_until_turn_completes(
    events_rx: &mut tokio::sync::mpsc::UnboundedReceiver<BridgeEvent>,
    expect_substr: &str,
) -> Result<String> {
    let mut last_delta = String::new();
    loop {
        let event = timeout(RECV_TIMEOUT, events_rx.recv())
            .await
            .context("timed out waiting for BridgeEvent")?
            .ok_or_else(|| anyhow!("events channel closed before TurnCompleted"))?;
        match event {
            BridgeEvent::MessageDelta { text } => {
                if text.contains(expect_substr) {
                    last_delta = text.clone();
                }
            }
            BridgeEvent::TurnCompleted { stop_reason: _ } => {
                return Ok(last_delta);
            }
            BridgeEvent::AgentClosed => {
                // The first restart-side AgentClosed legitimately races
                // against new-supervisor events. Keep draining: if the
                // new supervisor is healthy it'll deliver the expected
                // delta + TurnCompleted within RECV_TIMEOUT.
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_respawns_child_and_continues_streaming() -> Result<()> {
    let cmd = match test_command() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("skipping: {e}");
            return Ok(());
        }
    };

    let bridge = AgentBridge::spawn_with(Handle::current(), cmd)?;
    let mut events_rx = bridge
        .take_events_rx()
        .ok_or_else(|| anyhow!("take_events_rx must succeed once"))?;

    // First turn against the original supervisor.
    bridge.submit("alpha-one".to_string());
    let first_text = drain_until_turn_completes(&mut events_rx, "alpha-one").await?;
    assert!(
        first_text.contains("alpha-one"),
        "first turn delta missing payload: {first_text:?}"
    );

    // Restart: spawn a new agent child. The old child gets killed via
    // `kill_on_drop` when its supervisor's submit_rx closes.
    bridge.restart()?;

    // Second turn against the brand-new supervisor. The drain helper
    // tolerates a trailing AgentClosed from the dying old supervisor
    // before the new one starts producing events.
    bridge.submit("beta-two".to_string());
    let second_text = drain_until_turn_completes(&mut events_rx, "beta-two").await?;
    assert!(
        second_text.contains("beta-two"),
        "post-restart turn delta missing payload: {second_text:?}"
    );

    Ok(())
}
