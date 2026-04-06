use serde::{Deserialize, Serialize};

/// Unique node identifier within the cluster.
pub type NodeId = u64;

/// A command to be applied to the key-value state machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    Put {
        key: String,
        value: String,
    },
    Delete {
        key: String,
    },
    /// Committed by a new leader immediately after election to advance
    /// the commit index to the current term (Raft §8).
    Noop,
}

/// A single entry in the replicated log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub term: u64,
    pub index: u64,
    pub command: Command,
}

// ── Raft RPCs ──────────────────────────────────────────────────────────────

/// RequestVote RPC (Raft §5.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoteRequest {
    pub term: u64,
    pub candidate_id: NodeId,
    pub last_log_index: u64,
    pub last_log_term: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoteResponse {
    pub term: u64,
    pub vote_granted: bool,
    /// The node that is sending this response.
    /// Required so the receiver can identify the sender when this message
    /// arrives as the first frame on a new TCP connection.
    pub peer_id: NodeId,
}

/// AppendEntries RPC — also used as heartbeat when `entries` is empty (Raft §5.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppendEntriesRequest {
    pub term: u64,
    pub leader_id: NodeId,
    pub prev_log_index: u64,
    pub prev_log_term: u64,
    pub entries: Vec<LogEntry>,
    pub leader_commit: u64,
}

/// Response includes conflict hints for fast log backtracking (Raft §5.3 optimisation).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppendEntriesResponse {
    pub term: u64,
    pub success: bool,
    /// On failure: the index of the first conflicting entry.
    pub conflict_index: Option<u64>,
    /// On failure: the term of the conflicting entry (for term-based backtracking).
    pub conflict_term: Option<u64>,
    /// The node that is sending this response.
    /// Required so the receiver can identify the sender when this message
    /// arrives as the first frame on a new TCP connection.
    pub peer_id: NodeId,
}

// ── Client-facing protocol ─────────────────────────────────────────────────

/// A client request wrapped with a unique ID for deduplication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientRequest {
    /// Monotonically increasing per client; used to detect duplicate retries.
    pub id: u64,
    pub operation: ClientOperation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientOperation {
    Get { key: String },
    Put { key: String, value: String },
    Delete { key: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientResponse {
    pub id: u64,
    pub result: ClientResult,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientResult {
    Ok(Option<String>),
    NotLeader { leader_hint: Option<NodeId> },
    Error(String),
}

/// InstallSnapshot RPC — sent by the leader to a follower that is so far
/// behind that the required log entries have been compacted away (Raft §7).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallSnapshotRequest {
    pub term: u64,
    pub leader_id: NodeId,
    /// The last log index included in the snapshot.
    pub last_included_index: u64,
    /// The term of that entry.
    pub last_included_term: u64,
    /// Opaque serialised state-machine snapshot (KV store bytes).
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallSnapshotResponse {
    pub term: u64,
    /// The node sending this response (needed for peer identity on new connections).
    pub peer_id: NodeId,
}

// ── Wire envelope ──────────────────────────────────────────────────────────

/// Top-level message type multiplexed over a single TCP connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RaftMessage {
    VoteRequest(VoteRequest),
    VoteResponse(VoteResponse),
    AppendEntriesRequest(AppendEntriesRequest),
    AppendEntriesResponse(AppendEntriesResponse),
    InstallSnapshotRequest(InstallSnapshotRequest),
    InstallSnapshotResponse(InstallSnapshotResponse),
    ClientRequest(ClientRequest),
    ClientResponse(ClientResponse),
}
