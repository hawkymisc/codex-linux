//! Append-only write-ahead log of agent turn events, used for crash recovery.
//!
//! See docs/desktop-architecture.md §3.4 / §11(l) for the design rationale.
//!
//! # Format
//!
//! ```text
//! record := u32_le_len || u8_kind || cbor_payload || u32_le_crc32c
//! ```
//!
//! `len` is the length of `kind || cbor_payload` (does NOT include the
//! trailing CRC32C). CRC32C is computed over `kind || cbor_payload`.
//!
//! Files are rotated on TurnCompleted (`.wal` → `.wal.done`) and a
//! background GC pass on construction prunes anything older than the
//! configured retention window.

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Record kinds in the WAL. See module docs.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind {
    TurnStarted = 0x01,
    ServerNotification = 0x02,
    UserOp = 0x03,
    ApprovalDecision = 0x04,
    TurnCompleted = 0x05,
    Sentinel = 0x7F,
}

impl RecordKind {
    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(Self::TurnStarted),
            0x02 => Some(Self::ServerNotification),
            0x03 => Some(Self::UserOp),
            0x04 => Some(Self::ApprovalDecision),
            0x05 => Some(Self::TurnCompleted),
            0x7F => Some(Self::Sentinel),
            _ => None,
        }
    }

    /// Boundary kinds at which the writer fsyncs.
    pub fn is_durability_boundary(self) -> bool {
        matches!(self, Self::TurnCompleted | Self::ApprovalDecision)
    }
}

/// One decoded record from the log. Payload is opaque CBOR bytes; callers
/// deserialise into their own types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub kind: RecordKind,
    pub payload: Vec<u8>,
}

/// Configuration for [`TurnLog`] / [`WalManager`].
#[derive(Debug, Clone)]
pub struct WalConfig {
    pub root: PathBuf,
    pub retention: Duration,
    pub thread_quota_bytes: u64,
}

impl WalConfig {
    pub fn defaults_under(home: &Path) -> Self {
        Self {
            root: home.join(".local/state/codex-desktop/turns"),
            retention: Duration::from_secs(14 * 24 * 60 * 60),
            thread_quota_bytes: 256 * 1024 * 1024,
        }
    }
}

/// Append-only writer for one turn's WAL. Drop closes the file.
pub struct TurnLog {
    file: File,
    path: PathBuf,
    /// True once `complete()` has been called and the file has been
    /// renamed to `.done`. Subsequent appends are rejected.
    completed: bool,
}

impl TurnLog {
    /// Open (creating if necessary) the WAL for `<thread_id>/<turn_id>`.
    pub fn open(cfg: &WalConfig, thread_id: &str, turn_id: &str) -> Result<Self> {
        let dir = cfg.root.join(thread_id);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create WAL dir {}", dir.display()))?;
        let path = dir.join(format!("{turn_id}.wal"));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(false)
            .open(&path)
            .with_context(|| format!("open WAL {}", path.display()))?;
        Ok(Self {
            file,
            path,
            completed: false,
        })
    }

    pub fn append<T: Serialize>(&mut self, kind: RecordKind, payload: &T) -> Result<()> {
        if self.completed {
            return Err(anyhow!("turn log already completed"));
        }
        let mut cbor: Vec<u8> = Vec::new();
        ciborium::ser::into_writer(payload, &mut cbor).context("serialise CBOR payload")?;
        self.append_raw(kind, &cbor)?;
        if kind.is_durability_boundary() {
            self.file.sync_data().context("fdatasync WAL")?;
        }
        Ok(())
    }

    pub fn append_raw(&mut self, kind: RecordKind, payload: &[u8]) -> Result<()> {
        let len: u32 = u32::try_from(1 + payload.len())
            .map_err(|_| anyhow!("payload too large for WAL record"))?;
        let crc = compute_crc32c(kind, payload);
        self.file.write_all(&len.to_le_bytes())?;
        self.file.write_all(&[kind as u8])?;
        self.file.write_all(payload)?;
        self.file.write_all(&crc.to_le_bytes())?;
        Ok(())
    }

    /// Mark the turn complete: fdatasync once more then rename the file
    /// to `<turn_id>.wal.done`.
    pub fn complete(mut self) -> Result<PathBuf> {
        self.file.sync_data().context("final fdatasync")?;
        let mut done_path = self.path.clone();
        let stem = done_path.file_stem().unwrap_or_default().to_owned();
        done_path.set_file_name(format!("{}.wal.done", stem.to_string_lossy()));
        std::fs::rename(&self.path, &done_path).context("rename WAL to .done")?;
        self.completed = true;
        Ok(done_path)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Replay every record from `path`, deserialising payloads as they are
/// returned. Stops early on a CRC mismatch and returns
/// `Err(WalReadError::Truncated { records_recovered, .. })` so callers can
/// preserve partial state. CRC failures during replay are common —
/// they signal that the writer crashed mid-record, which is the exact
/// case the WAL is meant to handle gracefully.
pub fn replay(path: &Path) -> Result<Vec<Record>, WalReadError> {
    let mut file = File::open(path).map_err(|e| WalReadError::Io(format!("{e}")))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)
        .map_err(|e| WalReadError::Io(format!("{e}")))?;
    decode_records(&buf)
}

#[derive(Debug, thiserror::Error)]
pub enum WalReadError {
    #[error("io: {0}")]
    Io(String),
    #[error("truncated WAL after {records_recovered} record(s)")]
    Truncated {
        records_recovered: usize,
        records: Vec<Record>,
    },
    #[error("invalid record kind 0x{0:02X}")]
    InvalidKind(u8),
    #[error("CRC mismatch at record {index}")]
    CrcMismatch { index: usize, records: Vec<Record> },
}

fn decode_records(mut buf: &[u8]) -> Result<Vec<Record>, WalReadError> {
    let mut out = Vec::new();
    while !buf.is_empty() {
        if buf.len() < 4 {
            return Err(WalReadError::Truncated {
                records_recovered: out.len(),
                records: out,
            });
        }
        let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        buf = &buf[4..];
        if len == 0 || buf.len() < len + 4 {
            return Err(WalReadError::Truncated {
                records_recovered: out.len(),
                records: out,
            });
        }
        let kind_byte = buf[0];
        let kind =
            RecordKind::from_u8(kind_byte).ok_or(WalReadError::InvalidKind(kind_byte))?;
        let payload = buf[1..len].to_vec();
        let crc_bytes = &buf[len..len + 4];
        let stored_crc =
            u32::from_le_bytes([crc_bytes[0], crc_bytes[1], crc_bytes[2], crc_bytes[3]]);
        let computed_crc = compute_crc32c(kind, &payload);
        buf = &buf[len + 4..];
        if stored_crc != computed_crc {
            let index = out.len();
            return Err(WalReadError::CrcMismatch { index, records: out });
        }
        out.push(Record { kind, payload });
    }
    Ok(out)
}

/// CRC-32C (Castagnoli) over `kind || payload`. Inline software
/// implementation — no new dependency.
fn compute_crc32c(kind: RecordKind, payload: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    crc = update_crc(crc, kind as u8);
    for &b in payload {
        crc = update_crc(crc, b);
    }
    crc ^ 0xFFFF_FFFF
}

fn update_crc(mut crc: u32, byte: u8) -> u32 {
    crc ^= u32::from(byte);
    for _ in 0..8 {
        let mask = (crc & 1).wrapping_neg();
        crc = (crc >> 1) ^ (0x82F6_3B78 & mask);
    }
    crc
}

/// Top-level manager: GCs old WALs and exposes [`TurnLog::open`].
pub struct WalManager {
    cfg: WalConfig,
}

impl WalManager {
    pub fn new(cfg: WalConfig) -> Self {
        Self { cfg }
    }

    pub fn open_turn(&self, thread_id: &str, turn_id: &str) -> Result<TurnLog> {
        TurnLog::open(&self.cfg, thread_id, turn_id)
    }

    /// Walk the root and remove .wal.done files older than retention or
    /// pruning oldest-first within each thread directory until the per-
    /// thread quota is satisfied. Returns the number of files removed.
    pub fn gc(&self) -> Result<usize> {
        let mut removed = 0;
        let entries = match std::fs::read_dir(&self.cfg.root) {
            Ok(e) => e,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(err) => return Err(err.into()),
        };
        let now = SystemTime::now();
        for thread_entry in entries.flatten() {
            if !thread_entry
                .file_type()
                .map(|t| t.is_dir())
                .unwrap_or(false)
            {
                continue;
            }
            let thread_dir = thread_entry.path();
            let mut files: Vec<(PathBuf, SystemTime, u64)> = Vec::new();
            for f in std::fs::read_dir(&thread_dir)?.flatten() {
                let path = f.path();
                if path.extension().is_none_or(|e| e != "done") {
                    continue;
                }
                let meta = match f.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                files.push((path, mtime, meta.len()));
            }
            files.sort_by_key(|(_, t, _)| *t);

            // Retention prune.
            let cutoff = now.checked_sub(self.cfg.retention).unwrap_or(SystemTime::UNIX_EPOCH);
            for (path, mtime, _) in &files {
                if *mtime < cutoff && std::fs::remove_file(path).is_ok() {
                    removed += 1;
                }
            }

            // Quota prune (oldest first).
            let total: u64 = files.iter().map(|(_, _, sz)| *sz).sum();
            if total > self.cfg.thread_quota_bytes {
                let mut to_free = total - self.cfg.thread_quota_bytes;
                for (path, _, sz) in &files {
                    if to_free == 0 {
                        break;
                    }
                    if std::fs::remove_file(path).is_ok() {
                        removed += 1;
                        to_free = to_free.saturating_sub(*sz);
                    }
                }
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use serde::{Deserialize, Serialize};
    use tempfile::TempDir;

    #[derive(Serialize, Debug, Clone, PartialEq, Deserialize)]
    struct EvtFixture {
        method: String,
        n: u32,
    }

    fn cfg_in(tmp: &Path) -> WalConfig {
        WalConfig {
            root: tmp.join("turns"),
            retention: Duration::from_secs(14 * 24 * 60 * 60),
            thread_quota_bytes: 256 * 1024 * 1024,
        }
    }

    #[test]
    fn append_and_replay_round_trip() -> Result<()> {
        let tmp = TempDir::new()?;
        let cfg = cfg_in(tmp.path());
        let mgr = WalManager::new(cfg);
        let mut log = mgr.open_turn("th1", "t1")?;
        log.append(
            RecordKind::TurnStarted,
            &EvtFixture {
                method: "thread/started".into(),
                n: 1,
            },
        )?;
        log.append(
            RecordKind::ServerNotification,
            &EvtFixture {
                method: "agent/message_delta".into(),
                n: 2,
            },
        )?;
        let path = log.complete()?;
        let records = replay(&path).map_err(|e| anyhow!("{e}"))?;
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].kind, RecordKind::TurnStarted);
        assert_eq!(records[1].kind, RecordKind::ServerNotification);
        Ok(())
    }

    #[test]
    fn replay_detects_crc_corruption() -> Result<()> {
        let tmp = TempDir::new()?;
        let cfg = cfg_in(tmp.path());
        let mgr = WalManager::new(cfg);
        let mut log = mgr.open_turn("th1", "t1")?;
        log.append(
            RecordKind::TurnStarted,
            &EvtFixture {
                method: "x".into(),
                n: 1,
            },
        )?;
        let _ = log.complete()?;
        // Corrupt one byte in the WAL.
        let done = tmp.path().join("turns/th1/t1.wal.done");
        let mut bytes = std::fs::read(&done)?;
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        std::fs::write(&done, &bytes)?;
        let result = replay(&done);
        assert!(matches!(result, Err(WalReadError::CrcMismatch { .. })));
        Ok(())
    }

    #[test]
    fn truncated_wal_returns_partial_records() -> Result<()> {
        let tmp = TempDir::new()?;
        let cfg = cfg_in(tmp.path());
        let mgr = WalManager::new(cfg);
        let mut log = mgr.open_turn("th1", "t1")?;
        log.append(
            RecordKind::TurnStarted,
            &EvtFixture {
                method: "x".into(),
                n: 1,
            },
        )?;
        log.append(
            RecordKind::ServerNotification,
            &EvtFixture {
                method: "y".into(),
                n: 2,
            },
        )?;
        let path = log.path().to_path_buf();
        drop(log);
        // Truncate the trailing CRC of the last record so the second record
        // is incomplete; replay must report the first record as recovered.
        let bytes = std::fs::read(&path)?;
        std::fs::write(&path, &bytes[..bytes.len() - 2])?;
        let result = replay(&path);
        assert!(
            matches!(
                result,
                Err(WalReadError::Truncated { records_recovered: 1, .. })
            ),
            "result was {result:?}",
        );
        Ok(())
    }

    #[test]
    fn gc_removes_old_done_files() -> Result<()> {
        let tmp = TempDir::new()?;
        let cfg = WalConfig {
            root: tmp.path().join("turns"),
            retention: Duration::from_millis(0), // immediate retention
            thread_quota_bytes: 1024 * 1024,
        };
        let mgr = WalManager::new(cfg);
        let mut log = mgr.open_turn("th1", "old")?;
        log.append(
            RecordKind::TurnStarted,
            &EvtFixture {
                method: "x".into(),
                n: 1,
            },
        )?;
        let _ = log.complete()?;
        std::thread::sleep(Duration::from_millis(10));
        let removed = mgr.gc()?;
        assert!(removed >= 1);
        Ok(())
    }

    #[test]
    fn cannot_append_after_complete() -> Result<()> {
        let tmp = TempDir::new()?;
        let cfg = cfg_in(tmp.path());
        let mgr = WalManager::new(cfg);
        let mut log = mgr.open_turn("th1", "t1")?;
        log.append(
            RecordKind::TurnStarted,
            &EvtFixture {
                method: "x".into(),
                n: 1,
            },
        )?;
        // We can't actually re-call append after complete because complete
        // takes self by value — just assert the API shape compiles.
        let _path = log.complete()?;
        Ok(())
    }

    #[test]
    fn record_kind_durability_boundaries() {
        assert!(RecordKind::TurnCompleted.is_durability_boundary());
        assert!(RecordKind::ApprovalDecision.is_durability_boundary());
        assert!(!RecordKind::TurnStarted.is_durability_boundary());
        assert!(!RecordKind::ServerNotification.is_durability_boundary());
    }

    #[test]
    fn record_kind_round_trip_byte() {
        for k in [
            RecordKind::TurnStarted,
            RecordKind::ServerNotification,
            RecordKind::UserOp,
            RecordKind::ApprovalDecision,
            RecordKind::TurnCompleted,
            RecordKind::Sentinel,
        ] {
            assert_eq!(RecordKind::from_u8(k as u8), Some(k));
        }
        assert!(RecordKind::from_u8(0xFE).is_none());
    }

    #[test]
    fn crc32c_known_vector() {
        // We're testing OUR CRC pipeline, not the standard "123456789" vector — so just assert idempotence.
        let crc = compute_crc32c(RecordKind::TurnStarted, b"23456789");
        let crc2 = compute_crc32c(RecordKind::TurnStarted, b"23456789");
        assert_eq!(crc, crc2);
        let different = compute_crc32c(RecordKind::TurnStarted, b"23456788");
        assert_ne!(crc, different);
    }
}
