#![forbid(unsafe_code)]

//! Append-only JSONL log of "protocol drift" events plus per-method counters.
//!
//! # Why this crate exists
//!
//! `codex-agent-backend`'s [`IncomingServerNotification`] envelope splits
//! every server-to-client notification into [`Known`] (the local strict-mode
//! parser recognised the method) and [`Unknown`] (it did not). The
//! architecture plan in `docs/desktop-architecture.md` §3.3 / §11 calls for
//! an off-by-default "Protocol Drift" diagnostic pane that surfaces the
//! Unknown side, so users can see when an upstream backend has shipped a
//! method the local client doesn't yet decode — without ever silently
//! dropping the payload.
//!
//! This crate is the storage backend for that pane: it accepts each Unknown
//! notification, appends one JSONL record to a per-install file, and keeps
//! a small in-memory `BTreeMap<method, count>` so the GUI can render a live
//! "method × count" summary without re-reading the file.
//!
//! # Why not `tracing` / `slog`?
//!
//! Drift events are not log lines: they are protocol payloads we deliberately
//! preserve verbatim for later analysis (replay against an updated client
//! schema, automated PR generation, etc.). Mixing them into the application
//! logger would lose the ability to round-trip them through serde. We also
//! want a dedicated counters surface that doesn't depend on log-aggregator
//! tooling.
//!
//! # File format
//!
//! One JSON object per line:
//!
//! ```text
//! {"ts_unix_ms": 1714600000000, "method": "thread/whatever", "params": {...}}
//! ```
//!
//! `params` may be any JSON value (object, array, string, null, …). Lines are
//! flushed on every record; the file is opened with `append` so concurrent
//! writers from different processes will not corrupt each other (POSIX
//! guarantees `O_APPEND` writes are atomic up to `PIPE_BUF`).
//!
//! Counter snapshots are returned as `Vec<(String, u64)>` sorted by method
//! string — callers who need other orderings (descending by count, etc.) can
//! re-sort on the receiving side without re-reading the file.
//!
//! # Failure handling
//!
//! Log open failures are surfaced to the caller via [`anyhow::Result`].
//! Per-record I/O failures are surfaced *and* tracked as an internal counter
//! so a UI can show a "drift log degraded" banner without spamming the
//! application logger on every record.
//!
//! [`IncomingServerNotification`]: https://docs.rs/codex-agent-backend/latest/codex_agent_backend/envelope/enum.IncomingServerNotification.html
//! [`Known`]: https://docs.rs/codex-agent-backend/latest/codex_agent_backend/envelope/enum.IncomingServerNotification.html#variant.Known
//! [`Unknown`]: https://docs.rs/codex-agent-backend/latest/codex_agent_backend/envelope/enum.IncomingServerNotification.html#variant.Unknown

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;

/// One line of the JSONL drift log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriftRecord {
    /// Unix milliseconds since the epoch at the moment [`DriftLog::record`]
    /// was called. Cleanly sortable; not adjusted for clock skew.
    pub ts_unix_ms: u128,
    /// Wire `method` string of the unknown notification.
    pub method: String,
    /// Wire `params` payload, preserved verbatim. May be any JSON value.
    pub params: Value,
}

/// Snapshot of the in-memory state for diagnostic surfaces.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DriftSummary {
    /// Per-method counts, sorted lexicographically by method.
    pub by_method: Vec<(String, u64)>,
    /// Total number of records appended since [`DriftLog::open`].
    pub total: u64,
    /// Number of I/O failures observed since open. Non-zero means the JSONL
    /// file may be missing entries; the in-memory counters are still valid.
    pub io_errors: u64,
}

/// Append-only drift log. Cheap to clone via [`std::sync::Arc`].
pub struct DriftLog {
    path: PathBuf,
    inner: Mutex<Inner>,
}

struct Inner {
    writer: BufWriter<File>,
    counts: BTreeMap<String, u64>,
    total: u64,
    io_errors: u64,
}

impl DriftLog {
    /// Open or create the JSONL file at `path`. The file is opened with
    /// `append` mode so multiple processes (e.g. a desktop session and a
    /// concurrent CLI invocation) do not clobber each other's records.
    ///
    /// Counter state starts at zero — this constructor does **not** scan the
    /// existing file. Use [`DriftLog::replay`] separately if you need a
    /// full historical summary.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create drift-log parent {}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open drift-log {}", path.display()))?;
        Ok(Self {
            path,
            inner: Mutex::new(Inner {
                writer: BufWriter::new(file),
                counts: BTreeMap::new(),
                total: 0,
                io_errors: 0,
            }),
        })
    }

    /// Append one record. Returns `Ok(())` even when the underlying I/O
    /// failed — the failure increments the `io_errors` counter (visible in
    /// [`DriftLog::summary`]) but does not propagate, so callers in hot
    /// paths (e.g. the agent supervisor) can keep running. A `tracing::warn!`
    /// is emitted on each failure for debuggability.
    pub fn record(&self, method: &str, params: &Value) {
        let record = DriftRecord {
            ts_unix_ms: now_unix_ms(),
            method: method.to_owned(),
            params: params.clone(),
        };
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.total = guard.total.saturating_add(1);
        *guard.counts.entry(method.to_owned()).or_insert(0) += 1;
        if let Err(err) = write_record(&mut guard.writer, &record) {
            guard.io_errors = guard.io_errors.saturating_add(1);
            warn!(
                error = %err,
                path = %self.path.display(),
                "drift-log: write failed; in-memory counter still incremented"
            );
        }
    }

    /// Snapshot of the counters and totals. Cheap; locks briefly.
    pub fn summary(&self) -> DriftSummary {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        DriftSummary {
            by_method: guard.counts.iter().map(|(m, c)| (m.clone(), *c)).collect(),
            total: guard.total,
            io_errors: guard.io_errors,
        }
    }

    /// Path of the underlying JSONL file. Useful for tests and for surfacing
    /// the location in a "Reveal in file manager" UI affordance.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Flush the buffered writer. Call at quiescence (e.g. before the
    /// process exits) so no buffered records are lost.
    pub fn flush(&self) -> Result<()> {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard
            .writer
            .flush()
            .with_context(|| format!("flush drift-log {}", self.path.display()))?;
        Ok(())
    }

    /// Replay an existing drift-log file into a fresh summary. Reads the
    /// whole file into memory; intended for diagnostic tooling, not the
    /// hot path. Malformed lines are counted under `io_errors` and skipped.
    pub fn replay(path: impl AsRef<Path>) -> Result<DriftSummary> {
        let path = path.as_ref();
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(DriftSummary::default()),
            Err(e) => return Err(e).with_context(|| format!("read drift-log {}", path.display())),
        };
        let mut counts: BTreeMap<String, u64> = BTreeMap::new();
        let mut total: u64 = 0;
        let mut io_errors: u64 = 0;
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<DriftRecord>(trimmed) {
                Ok(rec) => {
                    total = total.saturating_add(1);
                    *counts.entry(rec.method).or_insert(0) += 1;
                }
                Err(_) => {
                    io_errors = io_errors.saturating_add(1);
                }
            }
        }
        Ok(DriftSummary {
            by_method: counts.into_iter().collect(),
            total,
            io_errors,
        })
    }
}

fn write_record(writer: &mut BufWriter<File>, record: &DriftRecord) -> Result<()> {
    let line = serde_json::to_string(record).context("serialise DriftRecord")?;
    writer.write_all(line.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::sync::Arc;
    use std::thread;
    use tempfile::TempDir;

    #[test]
    fn record_and_summary_round_trip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("drift.jsonl");
        let log = DriftLog::open(&path).unwrap();
        log.record("thread/whatever", &json!({"thread_id": "t-1"}));
        log.record("thread/whatever", &json!({"thread_id": "t-2"}));
        log.record("agent/oddTool", &json!({"name": "calc"}));
        log.flush().unwrap();

        let summary = log.summary();
        assert_eq!(summary.total, 3);
        assert_eq!(summary.io_errors, 0);
        assert_eq!(
            summary.by_method,
            vec![
                ("agent/oddTool".to_string(), 1),
                ("thread/whatever".to_string(), 2),
            ]
        );
    }

    #[test]
    fn record_writes_one_jsonl_line_per_call() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("drift.jsonl");
        let log = DriftLog::open(&path).unwrap();
        log.record("a/b", &json!({}));
        log.record("c/d", &json!([1, 2]));
        drop(log);

        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "expected 2 lines, got {}", lines.len());

        let r1: DriftRecord = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(r1.method, "a/b");
        assert_eq!(r1.params, json!({}));

        let r2: DriftRecord = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(r2.method, "c/d");
        assert_eq!(r2.params, json!([1, 2]));
    }

    #[test]
    fn replay_aggregates_existing_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("drift.jsonl");
        let log = DriftLog::open(&path).unwrap();
        for _ in 0..5 {
            log.record("thread/x", &json!(null));
        }
        for _ in 0..3 {
            log.record("agent/y", &json!({}));
        }
        log.flush().unwrap();
        drop(log);

        let summary = DriftLog::replay(&path).unwrap();
        assert_eq!(summary.total, 8);
        assert_eq!(summary.io_errors, 0);
        assert_eq!(
            summary.by_method,
            vec![("agent/y".to_string(), 3), ("thread/x".to_string(), 5)]
        );
    }

    #[test]
    fn replay_missing_file_returns_empty_summary() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("never-created.jsonl");
        let summary = DriftLog::replay(&path).unwrap();
        assert_eq!(summary, DriftSummary::default());
    }

    #[test]
    fn replay_counts_malformed_lines_as_io_errors() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("drift.jsonl");
        // Mix one valid record with two garbage lines.
        std::fs::write(
            &path,
            b"{\"ts_unix_ms\":1,\"method\":\"a/b\",\"params\":null}\nthis is not json\n{also not json}\n",
        )
        .unwrap();

        let summary = DriftLog::replay(&path).unwrap();
        assert_eq!(summary.total, 1);
        assert_eq!(summary.io_errors, 2);
        assert_eq!(summary.by_method, vec![("a/b".to_string(), 1)]);
    }

    #[test]
    fn concurrent_records_are_atomic_under_mutex() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("drift.jsonl");
        let log = Arc::new(DriftLog::open(&path).unwrap());

        let mut handles = Vec::new();
        for t in 0..4 {
            let log = Arc::clone(&log);
            handles.push(thread::spawn(move || {
                for i in 0..50 {
                    log.record(
                        &format!("worker/{t}"),
                        &json!({"i": i}),
                    );
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        log.flush().unwrap();

        let summary = log.summary();
        assert_eq!(summary.total, 200);
        for t in 0..4 {
            let key = format!("worker/{t}");
            let count = summary
                .by_method
                .iter()
                .find(|(k, _)| k == &key)
                .map(|(_, c)| *c)
                .unwrap_or(0);
            assert_eq!(count, 50, "worker/{t} count off");
        }

        // Cross-check: file has 200 well-formed lines.
        let replayed = DriftLog::replay(&path).unwrap();
        assert_eq!(replayed.total, 200);
        assert_eq!(replayed.io_errors, 0);
    }

    #[test]
    fn open_creates_parent_directory() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nested/under/here/drift.jsonl");
        let log = DriftLog::open(&path).unwrap();
        log.record("hello/world", &json!(1));
        drop(log);
        assert!(path.exists());
    }
}
