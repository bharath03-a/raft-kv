use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::warn;

/// Per-node Raft metrics exposed over HTTP in Prometheus text format.
///
/// All fields are atomics: the node event loop writes them after every state
/// transition while the HTTP handler reads them concurrently without a lock.
/// `Relaxed` ordering is used throughout — Prometheus scrapes are best-effort
/// and individual metrics are independently meaningful.
pub struct Metrics {
    /// Current Raft term.
    pub term: AtomicU64,
    /// Current role: 0 = Follower, 1 = Candidate, 2 = Leader.
    pub role: AtomicU8,
    /// Index of the highest log entry known to be committed.
    pub commit_index: AtomicU64,
    /// Index of the highest log entry applied to the state machine.
    pub last_applied: AtomicU64,
    /// Total number of entries in the Raft log.
    pub log_length: AtomicU64,
    /// Number of client write requests waiting for log commitment.
    pub pending_writes: AtomicU64,
    /// Number of client read requests waiting for the read-index protocol.
    pub pending_reads: AtomicU64,
    /// Total Raft peer messages sent (monotonically increasing).
    pub messages_sent: AtomicU64,
    /// Total Raft peer messages received (monotonically increasing).
    pub messages_received: AtomicU64,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            term: AtomicU64::new(0),
            role: AtomicU8::new(0),
            commit_index: AtomicU64::new(0),
            last_applied: AtomicU64::new(0),
            log_length: AtomicU64::new(0),
            pending_writes: AtomicU64::new(0),
            pending_reads: AtomicU64::new(0),
            messages_sent: AtomicU64::new(0),
            messages_received: AtomicU64::new(0),
        })
    }

    /// Render all fields in the Prometheus text exposition format (version 0.0.4).
    pub fn render(&self, node_id: u64) -> String {
        let r = Ordering::Relaxed;
        let role_val = self.role.load(r);
        let role_name = match role_val {
            1 => "candidate",
            2 => "leader",
            _ => "follower",
        };

        format!(
            concat!(
                "# HELP raft_term Current Raft term.\n",
                "# TYPE raft_term gauge\n",
                "raft_term{{node=\"{node}\",role=\"{role}\"}} {term}\n",
                "# HELP raft_role Current role encoded as integer (0=follower 1=candidate 2=leader).\n",
                "# TYPE raft_role gauge\n",
                "raft_role{{node=\"{node}\"}} {role_val}\n",
                "# HELP raft_commit_index Index of the highest committed log entry.\n",
                "# TYPE raft_commit_index gauge\n",
                "raft_commit_index{{node=\"{node}\"}} {commit}\n",
                "# HELP raft_last_applied Index of the highest applied log entry.\n",
                "# TYPE raft_last_applied gauge\n",
                "raft_last_applied{{node=\"{node}\"}} {applied}\n",
                "# HELP raft_log_length Total number of entries in the Raft log.\n",
                "# TYPE raft_log_length gauge\n",
                "raft_log_length{{node=\"{node}\"}} {log_len}\n",
                "# HELP raft_pending_writes Client writes waiting for log commitment.\n",
                "# TYPE raft_pending_writes gauge\n",
                "raft_pending_writes{{node=\"{node}\"}} {pw}\n",
                "# HELP raft_pending_reads Client reads waiting for the read-index protocol.\n",
                "# TYPE raft_pending_reads gauge\n",
                "raft_pending_reads{{node=\"{node}\"}} {pr}\n",
                "# HELP raft_messages_sent_total Total Raft peer messages sent.\n",
                "# TYPE raft_messages_sent_total counter\n",
                "raft_messages_sent_total{{node=\"{node}\"}} {sent}\n",
                "# HELP raft_messages_received_total Total Raft peer messages received.\n",
                "# TYPE raft_messages_received_total counter\n",
                "raft_messages_received_total{{node=\"{node}\"}} {recv}\n",
            ),
            node = node_id,
            role = role_name,
            term = self.term.load(r),
            role_val = role_val,
            commit = self.commit_index.load(r),
            applied = self.last_applied.load(r),
            log_len = self.log_length.load(r),
            pw = self.pending_writes.load(r),
            pr = self.pending_reads.load(r),
            sent = self.messages_sent.load(r),
            recv = self.messages_received.load(r),
        )
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Arc::try_unwrap(Self::new()).unwrap_or_else(|_| unreachable!())
    }
}

/// Spawn an HTTP server that serves Prometheus metrics on `listen_addr`.
///
/// The server is lightweight: it uses raw tokio TCP with no HTTP framework.
/// Each accepted connection reads the request headers (discarded) and replies
/// with a single HTTP/1.0 response containing the Prometheus text body.
pub async fn serve(listen_addr: SocketAddr, node_id: u64, metrics: Arc<Metrics>) {
    let listener = match TcpListener::bind(listen_addr).await {
        Ok(l) => {
            tracing::info!(addr = %listen_addr, "metrics HTTP server listening");
            l
        }
        Err(e) => {
            warn!("metrics: failed to bind {listen_addr}: {e}");
            return;
        }
    };

    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            continue;
        };
        let m = Arc::clone(&metrics);
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            // Consume the HTTP request (we only serve one route regardless of path).
            let _ = stream.read(&mut buf).await;
            let body = m.render(node_id);
            let response = format!(
                "HTTP/1.0 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
    }
}
