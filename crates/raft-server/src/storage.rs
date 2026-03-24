use std::path::Path;

use anyhow::{Context, Result};
use raft_core::state::PersistentState;
use serde::{Deserialize, Serialize};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
};

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
        log_entries: state
            .log
            .entries_after(0)
            .to_vec(),
    };

    let payload =
        bincode::serialize(&stored).context("failed to serialise persistent state")?;

    let tmp_path = path.with_extension("tmp");

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp_path)
        .await
        .with_context(|| format!("open {}", tmp_path.display()))?;

    file.write_all(&payload)
        .await
        .context("write persistent state")?;

    file.flush().await.context("flush")?;
    if sync {
        file.sync_all().await.context("fsync")?;
    }

    fs::rename(&tmp_path, path)
        .await
        .context("rename tmp → state file")?;

    Ok(())
}

/// Load `PersistentState` from `path`, or return a default if the file
/// does not exist (first boot).
pub async fn load(path: &Path) -> Result<PersistentState> {
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

    let stored: StoredState =
        bincode::deserialize(&buf).context("deserialise persistent state")?;

    let mut log = raft_core::log::RaftLog::new();
    for entry in stored.log_entries {
        log = log.append(entry);
    }

    Ok(PersistentState {
        current_term: stored.current_term,
        voted_for: stored.voted_for,
        log,
    })
}
