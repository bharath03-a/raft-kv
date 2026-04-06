use std::collections::HashMap;

use crate::message::{ClientResponse, LogEntry, NodeId, RaftMessage};

/// The three roles a Raft node can occupy at any time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    Follower,
    Candidate,
    Leader,
}

/// Durable state — **must** be written to stable storage before responding
/// to any RPC so it survives crashes (Raft §5.4.1, Figure 2).
#[derive(Debug, Clone)]
pub struct PersistentState {
    /// Latest term seen (monotonically increasing).
    pub current_term: u64,
    /// The node this server voted for in `current_term`, if any.
    pub voted_for: Option<NodeId>,
    /// The replicated log.
    pub log: crate::log::RaftLog,
}

impl PersistentState {
    pub fn new() -> Self {
        Self {
            current_term: 0,
            voted_for: None,
            log: crate::log::RaftLog::new(),
        }
    }
}

impl Default for PersistentState {
    fn default() -> Self {
        Self::new()
    }
}

/// Volatile state kept on **all** servers (lost on restart — recomputed).
#[derive(Debug, Clone)]
pub struct VolatileState {
    /// Index of the highest log entry known to be committed.
    pub commit_index: u64,
    /// Index of the highest log entry applied to the state machine.
    pub last_applied: u64,
}

impl VolatileState {
    pub fn new() -> Self {
        Self {
            commit_index: 0,
            last_applied: 0,
        }
    }
}

impl Default for VolatileState {
    fn default() -> Self {
        Self::new()
    }
}

/// Volatile state kept only on the **leader** (reinitialized after each election).
///
/// We intentionally avoid storing async channels here to keep `raft-core`
/// free of I/O dependencies. The server layer maintains its own
/// `HashMap<log_index, oneshot::Sender>` mapping for pending writes.
#[derive(Debug)]
pub struct LeaderState {
    /// For each peer: index of the next log entry to send.
    pub next_index: HashMap<NodeId, u64>,
    /// For each peer: index of the highest log entry known to be replicated.
    pub match_index: HashMap<NodeId, u64>,
    /// IDs of client write requests that are in-flight (appended but not yet committed).
    /// The server layer uses this to correlate committed log indices with client channels.
    pub pending_write_ids: HashMap<u64, u64>, // log_index → client_request_id
    /// Pending read indices: (read_index, client_request_id).
    /// Served once `last_applied >= read_index` and leader confirms quorum.
    pub pending_reads: Vec<(u64, u64)>,
    /// Tracks which peers have acknowledged the most recent heartbeat round.
    pub heartbeat_acks: HashMap<NodeId, bool>,
}

impl LeaderState {
    pub fn new(peers: &[NodeId], last_log_index: u64) -> Self {
        let next_index = peers.iter().map(|&p| (p, last_log_index + 1)).collect();
        let match_index = peers.iter().map(|&p| (p, 0)).collect();
        let heartbeat_acks = peers.iter().map(|&p| (p, false)).collect();
        Self {
            next_index,
            match_index,
            pending_write_ids: HashMap::new(),
            pending_reads: Vec::new(),
            heartbeat_acks,
        }
    }
}

/// The set of side-effects the node orchestrator must execute after each
/// state transition in the pure Raft core.
///
/// Returning actions rather than performing them directly keeps `raft-core`
/// completely I/O-free and trivially testable.
#[derive(Debug, Default)]
pub struct Actions {
    /// Messages to be sent to specific peers (or back to the leader).
    pub messages: Vec<(NodeId, RaftMessage)>,
    /// Log entries that have just been committed and should be applied to
    /// the state machine.
    pub entries_to_apply: Vec<LogEntry>,
    /// If `Some`, the persistent state must be fsynced to disk before
    /// sending any of the `messages`.
    pub persist: Option<PersistentState>,
    /// Responses to unblock waiting client requests.
    pub client_responses: Vec<ClientResponse>,
    /// Whether the election timer should be reset.
    pub reset_election_timer: bool,
    /// Peers to which the leader must send an InstallSnapshot RPC.
    ///
    /// These peers have fallen so far behind that the required log entries
    /// have been compacted away. The server layer is responsible for
    /// serialising the current KV snapshot and sending the RPC.
    pub send_snapshot_to: Vec<NodeId>,
    /// An InstallSnapshot received from the leader that the server must
    /// apply to the state machine and persist to disk.
    pub install_snapshot: Option<crate::message::InstallSnapshotRequest>,
}
