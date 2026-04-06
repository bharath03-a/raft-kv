use std::path::Path;

use anyhow::{Context, Result};
use raft_core::state::PersistentState;
use serde::{Deserialize, Serialize};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
};

// ── Raft persistent state ──────────────────────────────────────────────────

/// Serialisable snapshot of `PersistentState`.
///
/// We keep this separate from the core type so the storage format can
/// evolve independently.
#[derive(Serialize, Deserialize)]
struct StoredState {
    current_term: u64,
    voted_for: Option<u64>,
    log_entries: Vec<raft_core::message::LogEntry>,
}

/// Persist `state` to `path`.
///
/// When `sync` is true we fsync before the rename — the Raft §5.4.1
/// durability guarantee.  Pass `false` only for benchmarking; the
/// cluster is not crash-safe in that mode.
pub async fn save(path: &Path, state: &PersistentState, sync: bool) -> Result<()> {
    let stored = StoredState {
        current_term: state.current_term,
        voted_for: state.voted_for,
        log_entries: state.log.entries_after(0).to_vec(),
    };

    let payload = bincode::serialize(&stored).context("failed to serialise persistent state")?;

    atomic_write(path, &payload, sync).await
}

/// Load `PersistentState` from `path`, or return a default if the file
/// does not exist (first boot).
///
/// `snapshot_base` sets the log's compaction baseline so that log entries
/// loaded from disk (which may start above index 1 after compaction) are
/// appended to the correct base.
pub async fn load(path: &Path, snapshot_base: Option<(u64, u64)>) -> Result<PersistentState> {
    if !path.exists() {
        return Ok(PersistentState::new());
    }

    let mut file = fs::File::open(path)
        .await
        .with_context(|| format!("open {}", path.display()))?;

    let mut buf = Vec::new();
    file.read_to_end(&mut buf)
        .await
        .context("read persistent state")?;

    let stored: StoredState = bincode::deserialize(&buf).context("deserialise persistent state")?;

    let (snap_idx, snap_term) = snapshot_base.unwrap_or((0, 0));
    let mut log = raft_core::log::RaftLog::new_with_snapshot(snap_idx, snap_term);
    for entry in stored.log_entries {
        log = log.append(entry);
    }

    Ok(PersistentState {
        current_term: stored.current_term,
        voted_for: stored.voted_for,
        log,
    })
}

// ── Snapshot file ──────────────────────────────────────────────────────────

/// On-disk representation of a KV state machine snapshot (Raft §7).
///
/// Stored separately from the Raft state file so that snapshot data
/// (potentially large) does not slow down the hot-path fsync for log entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotFile {
    /// Last log index covered by this snapshot.
    pub last_included_index: u64,
    /// Term of that entry (needed for log consistency after restart).
    pub last_included_term: u64,
    /// Key-value pairs at the time of the snapshot.
    pub kv_data: Vec<(String, String)>,
}

/// Write a snapshot to `path` atomically.
pub async fn save_snapshot(path: &Path, snapshot: &SnapshotFile, sync: bool) -> Result<()> {
    let payload = bincode::serialize(snapshot).context("failed to serialise snapshot")?;
    atomic_write(path, &payload, sync).await
}

/// Load a snapshot from `path`. Returns `None` if the file does not exist
/// (no snapshot has been taken yet).
pub async fn load_snapshot(path: &Path) -> Result<Option<SnapshotFile>> {
    if !path.exists() {
        return Ok(None);
    }

    let mut file = fs::File::open(path)
        .await
        .with_context(|| format!("open snapshot {}", path.display()))?;

    let mut buf = Vec::new();
    file.read_to_end(&mut buf).await.context("read snapshot")?;

    let snapshot: SnapshotFile = bincode::deserialize(&buf).context("deserialise snapshot")?;

    Ok(Some(snapshot))
}

// ── Shared helper ──────────────────────────────────────────────────────────

/// Write `payload` to `path` atomically: write to a `.tmp` file, optionally
/// fsync, then rename into place.
async fn atomic_write(path: &Path, payload: &[u8], sync: bool) -> Result<()> {
    let tmp_path = path.with_extension("tmp");

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp_path)
        .await
        .with_context(|| format!("open {}", tmp_path.display()))?;

    file.write_all(payload).await.context("write")?;
    file.flush().await.context("flush")?;
    if sync {
        file.sync_all().await.context("fsync")?;
    }

    fs::rename(&tmp_path, path)
        .await
        .context("rename tmp → target")?;

    Ok(())
}
