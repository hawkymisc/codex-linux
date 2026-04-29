//! End-to-end smoke test: spawn the in-tree binary in agent role and
//! drive it via raw NDJSON JSON-RPC, asserting that a submitted prompt
//! produces a `agent/message_delta` event back.
//!
//! This test exercises the *binary* — the agent role server and the
//! `CODEX_DESKTOP_FORCE_ROLE=agent` env override — not the bridge. The
//! bridge itself is exercised in PR-F via a real GUI session. We use raw
//! framing here to keep the test independent of `codex-agent-backend`'s
//! API surface and to give a clear failure surface if the wire-format
//! ever drifts.
//!
//! Cargo automatically sets `CARGO_BIN_EXE_codex-desktop` for tests in a
//! `bin`-shipping crate. If for some reason the env var is missing the
//! test bails out with a useful diagnostic rather than panicking.

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const READ_TIMEOUT: Duration = Duration::from_secs(15);

#[tokio::test(flavor = "multi_thread")]
async fn submit_round_trip_via_bridge() -> Result<()> {
    // Cargo sets CARGO_BIN_EXE_<name> for binaries in the same package as
    // the integration test. Anything else means the test is being run
    // from outside cargo or a misconfigured environment — surface that
    // clearly rather than panicking on a missing path.
    let bin = match option_env!("CARGO_BIN_EXE_codex-desktop") {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => {
            eprintln!(
                "skipping: CARGO_BIN_EXE_codex-desktop is not set; \
                 run via `cargo test`"
            );
            return Ok(());
        }
    };

    // Spawn the binary in agent role via the env override. We use the
    // synchronous `std::process::Command` so we can call
    // `pre_exec`/`arg0`-style helpers naturally; once the child is up we
    // wrap its pipes in `tokio` types for async IO.
    let mut std_cmd = Command::new(&bin);
    std_cmd
        .arg0("codex-desktop")
        .env("CODEX_DESKTOP_FORCE_ROLE", "agent")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = std_cmd.spawn().with_context(|| {
        format!("failed to spawn {bin} for agent_bridge_smoke test")
    })?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("child has no stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("child has no stdout"))?;

    // Convert the std pipes into tokio versions for async drain. The
    // child stays in std::process::Child so we can wait on it
    // synchronously at the end without needing tokio's process feature.
    let mut tokio_stdin = tokio::process::ChildStdin::from_std(stdin)
        .context("wrap stdin in tokio::process::ChildStdin")?;
    let tokio_stdout = tokio::process::ChildStdout::from_std(stdout)
        .context("wrap stdout in tokio::process::ChildStdout")?;
    let mut reader = BufReader::new(tokio_stdout);

    // 1. Initialize.
    write_line(
        &mut tokio_stdin,
        r#"{"jsonrpc":"2.0","id":"1","method":"initialize","params":{"client_info":{"name":"smoke","version":"0"}}}"#,
    )
    .await?;
    let init_resp = read_one(&mut reader).await?;
    assert_eq!(
        init_resp.get("id").and_then(Value::as_str),
        Some("1"),
        "initialize response missing id: {init_resp}"
    );
    let proto = init_resp
        .get("result")
        .and_then(|r| r.get("protocol_version"));
    assert!(
        proto.is_some(),
        "initialize result missing protocol_version: {init_resp}"
    );

    // 2. Submit. The agent role echoes the payload back as a
    //    message_delta and then sends turn_completed. We collect up to
    //    four lines (one ack + two notifications + slack) and assert the
    //    presence of both interesting lines.
    write_line(
        &mut tokio_stdin,
        r#"{"jsonrpc":"2.0","id":"2","method":"submit","params":{"payload":{"text":"hello bridge"}}}"#,
    )
    .await?;

    let mut saw_delta = false;
    let mut saw_turn_completed = false;
    for _ in 0..4 {
        let line = read_one(&mut reader).await?;
        if line
            .get("method")
            .and_then(Value::as_str)
            .is_some_and(|m| m == "agent/message_delta")
        {
            let delta = line
                .get("params")
                .and_then(|p| p.get("delta"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            assert_eq!(delta, "hello bridge", "unexpected delta payload: {line}");
            saw_delta = true;
        }
        if line
            .get("method")
            .and_then(Value::as_str)
            .is_some_and(|m| m == "agent/turn_completed")
        {
            saw_turn_completed = true;
        }
        if saw_delta && saw_turn_completed {
            break;
        }
    }
    assert!(saw_delta, "did not see agent/message_delta within 4 lines");
    assert!(
        saw_turn_completed,
        "did not see agent/turn_completed within 4 lines"
    );

    // 3. Shutdown gracefully.
    write_line(
        &mut tokio_stdin,
        r#"{"jsonrpc":"2.0","id":"3","method":"shutdown"}"#,
    )
    .await?;
    let shutdown_resp = read_one(&mut reader).await?;
    assert_eq!(
        shutdown_resp.get("id").and_then(Value::as_str),
        Some("3"),
        "shutdown response missing id: {shutdown_resp}"
    );

    // Drop stdin so the child sees EOF and exits.
    drop(tokio_stdin);

    // Wait for the process to exit cleanly. Use a synchronous wait with
    // a short polling loop so we can bound the test runtime.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait()? {
            Some(status) => {
                assert!(
                    status.success(),
                    "child exited with non-zero status: {status:?}"
                );
                break;
            }
            None => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    panic!("agent role did not exit within 5s after shutdown");
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }
    Ok(())
}

/// Write a single NDJSON line (JSON object, then `\n`) and flush.
async fn write_line(
    stdin: &mut tokio::process::ChildStdin,
    line: &str,
) -> Result<()> {
    stdin
        .write_all(line.as_bytes())
        .await
        .context("write ndjson body")?;
    stdin.write_all(b"\n").await.context("write ndjson newline")?;
    stdin.flush().await.context("flush stdin")?;
    Ok(())
}

/// Read one NDJSON line from `reader` with a wall-clock timeout, return
/// the parsed JSON value.
async fn read_one(
    reader: &mut BufReader<tokio::process::ChildStdout>,
) -> Result<Value> {
    let mut line = String::new();
    let read = tokio::time::timeout(READ_TIMEOUT, reader.read_line(&mut line))
        .await
        .context("timed out waiting for stdout line")?
        .context("reading stdout line")?;
    if read == 0 {
        return Err(anyhow!("child closed stdout before producing a line"));
    }
    let trimmed = line.trim_end_matches(['\r', '\n']);
    serde_json::from_str(trimmed).with_context(|| format!("parsing line: {trimmed}"))
}
