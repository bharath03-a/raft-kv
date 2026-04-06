/// Static cluster configuration shared across all nodes.
///
/// In a production system these values would be runtime-configurable;
/// for this portfolio project they are compile-time constants to keep
/// the scope manageable.
#[derive(Debug, Clone)]
pub struct RaftConfig {
    /// Minimum election timeout in milliseconds.
    pub election_timeout_min_ms: u64,
    /// Maximum election timeout in milliseconds.
    pub election_timeout_max_ms: u64,
    /// How often the leader sends heartbeat AppendEntries RPCs.
    pub heartbeat_interval_ms: u64,
    /// Total number of nodes in the cluster (fixed membership).
    pub cluster_size: usize,
}

impl RaftConfig {
    /// Conservative defaults that work well for a local cluster.
    pub fn default_local() -> Self {
        Self {
            election_timeout_min_ms: 500,
            election_timeout_max_ms: 1000,
            heartbeat_interval_ms: 100,
            cluster_size: 3,
        }
    }

    /// Majority quorum required to elect a leader or commit an entry.
    pub fn majority(&self) -> usize {
        self.cluster_size / 2 + 1
    }
}
