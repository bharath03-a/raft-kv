use std::{collections::HashMap, net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::Result;
use raft_core::{
    RaftConfig, RaftNode,
    message::{ClientOperation, ClientRequest, ClientResponse, ClientResult, NodeId, RaftMessage},
};
use tokio::{
    sync::{mpsc, oneshot},
    time,
};
use tracing::{debug, error, info};

use crate::{
    chaos::ChaosConfig,
    kv::KvStore,
    metrics::Metrics,
    storage,
    transport::{Incoming, Outgoing, Transport},
};

/// A pending write request from a client waiting for log commitment.
struct PendingWrite {
    request_id: u64,
    reply: oneshot::Sender<ClientResponse>,
}

/// A pending read request waiting for the read-index protocol to complete.
struct PendingRead {
    key: String,
    request_id: u64,
    read_index: u64,
    reply: oneshot::Sender<ClientResponse>,
}

/// The top-level node actor.
///
/// Runs the `tokio::select!` event loop that drives the pure `RaftNode`
/// core and handles all I/O side-effects returned via `Actions`.
pub struct NodeActor {
    id: NodeId,
    /// `Option` so we can `take()` for consuming transitions without
    /// `mem::replace` and a dummy placeholder.
    raft: Option<RaftNode>,
    kv: KvStore,
    transport_tx: mpsc::Sender<Outgoing>,
    transport_rx: mpsc::Receiver<Incoming>,
    /// Client-facing request receiver (fed by the transport's client handler).
    client_rx: mpsc::Receiver<(ClientRequest, oneshot::Sender<ClientResponse>)>,
    /// Pending write log_index → sender.
    pending_writes: HashMap<u64, PendingWrite>,
    /// Pending reads buffered for the read-index protocol.
    pending_reads: Vec<PendingRead>,
    state_path: PathBuf,
    config: RaftConfig,
    /// When true, skip fsync on persist (faster but not crash-safe).
    no_sync: bool,
    /// Shared metrics updated after every state transition.
    metrics: std::sync::Arc<Metrics>,
}

impl NodeActor {
    /// Create and initialise a node actor.
    ///
    /// The channel pair for client requests is created internally: the sender
    /// end is wired to the transport (which accepts client TCP connections on
    /// the same port as peer traffic) and the receiver end is kept here.
    /// Create and initialise a node actor with no chaos injection.
    pub async fn new(
        id: NodeId,
        peers: HashMap<NodeId, SocketAddr>,
        listen_addr: SocketAddr,
        state_dir: PathBuf,
        config: RaftConfig,
        no_sync: bool,
    ) -> Result<Self> {
        Self::new_with_chaos(
            id,
            peers,
            listen_addr,
            state_dir,
            config,
            no_sync,
            ChaosConfig::none(),
        )
        .await
    }

    /// Create a node actor with a custom chaos configuration.
    ///
    /// `ChaosConfig::none()` is the production default.  Pass
    /// `ChaosConfig::twenty_percent()` or a custom config to inject message
    /// drops for fault-tolerance testing.
    pub async fn new_with_chaos(
        id: NodeId,
        peers: HashMap<NodeId, SocketAddr>,
        listen_addr: SocketAddr,
        state_dir: PathBuf,
        config: RaftConfig,
        no_sync: bool,
        chaos: ChaosConfig,
    ) -> Result<Self> {
        let state_path = state_dir.join(format!("node-{id}.state"));

        // Recover durable state from disk (or start fresh).
        let persistent = storage::load(&state_path).await?;
        info!(
            node_id = id,
            term = persistent.current_term,
            "recovered state"
        );

        let peer_ids: Vec<NodeId> = peers.keys().copied().collect();
        // cluster_size must reflect the actual cluster, not the hardcoded default.
        let cluster_size = peer_ids.len() + 1;
        let config = RaftConfig {
            cluster_size,
            ..config
        };

        let mut raft = RaftNode::new(id, peer_ids, config.clone());
        raft.persistent = persistent;

        // Wire the client channel: transport writes to `client_tx`,
        // this actor reads from `client_rx`.
        let (client_tx, client_rx) = mpsc::channel(256);

        let transport = Transport::start(id, listen_addr, peers, client_tx, chaos);

        Ok(Self {
            id,
            raft: Some(raft),
            kv: KvStore::new(),
            transport_tx: transport.outgoing_tx,
            transport_rx: transport.incoming_rx,
            client_rx,
            pending_writes: HashMap::new(),
            pending_reads: Vec::new(),
            state_path,
            config,
            no_sync,
            metrics: Metrics::new(),
        })
    }

    /// Return a shared reference to the node's live metrics.
    ///
    /// Callers can pass this `Arc` to `metrics::serve` to expose the metrics
    /// over HTTP without any additional synchronisation.
    pub fn metrics(&self) -> std::sync::Arc<Metrics> {
        std::sync::Arc::clone(&self.metrics)
    }

    /// Run the node event loop indefinitely.
    pub async fn run(mut self) {
        let heartbeat_interval = Duration::from_millis(self.config.heartbeat_interval_ms);
        let mut election_timer = Self::new_election_timer(&self.config);
        let mut heartbeat_timer = time::interval(heartbeat_interval);

        loop {
            let reset_election_timer = tokio::select! {
                // Inbound peer message.
                Some(incoming) = self.transport_rx.recv() => {
                    self.handle_incoming(incoming).await
                }
                // Client request submitted via the TCP client handler.
                Some((req, reply)) = self.client_rx.recv() => {
                    self.handle_client(req, reply).await;
                    false
                }
                // Election timeout fired.
                _ = election_timer.tick() => {
                    debug!(node_id = self.id, "election timeout");
                    self.handle_election_timeout().await;
                    true // always reset after timeout fires
                }
                // Heartbeat tick (leader only — ignored by followers/candidates).
                _ = heartbeat_timer.tick() => {
                    self.handle_heartbeat().await;
                    false
                }
            };

            if reset_election_timer {
                election_timer = Self::new_election_timer(&self.config);
            }
        }
    }

    // ── Private helpers ────────────────────────────────────────────────────

    fn raft(&self) -> &RaftNode {
        self.raft.as_ref().expect("raft node must be set")
    }

    fn take_raft(&mut self) -> RaftNode {
        self.raft.take().expect("raft node must be set")
    }

    fn new_election_timer(config: &RaftConfig) -> time::Interval {
        use rand::RngExt;
        let ms = rand::rng()
            .random_range(config.election_timeout_min_ms..=config.election_timeout_max_ms);
        let start = time::Instant::now() + Duration::from_millis(ms);
        time::interval_at(start, Duration::from_millis(ms))
    }

    // ── Inbound message dispatch ───────────────────────────────────────────

    async fn handle_incoming(&mut self, incoming: Incoming) -> bool {
        self.metrics
            .messages_received
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let raft = self.take_raft();
        let (new_raft, actions) = match incoming.message {
            RaftMessage::VoteRequest(req) => raft.handle_vote_request(req),
            RaftMessage::VoteResponse(resp) => raft.handle_vote_response(incoming.from, resp),
            RaftMessage::AppendEntriesRequest(req) => raft.handle_append_entries(req),
            RaftMessage::AppendEntriesResponse(resp) => {
                raft.handle_append_entries_response(incoming.from, resp)
            }
            RaftMessage::ClientRequest(req) => raft.handle_client_request(req),
            RaftMessage::ClientResponse(_) => {
                self.raft = Some(raft);
                return false; // unexpected on server side
            }
        };
        self.raft = Some(new_raft);
        self.apply_actions(actions).await
    }

    async fn handle_client(&mut self, req: ClientRequest, reply: oneshot::Sender<ClientResponse>) {
        if !self.raft().is_leader() {
            let response = ClientResponse {
                id: req.id,
                result: ClientResult::NotLeader {
                    leader_hint: self.raft().current_leader,
                },
            };
            let _ = reply.send(response);
            return;
        }

        match &req.operation {
            ClientOperation::Get { key } => {
                // Read-index protocol: record read_index = current commit_index,
                // trigger a heartbeat to confirm we are still the leader, then
                // serve the read once last_applied >= read_index.
                let read_index = self.raft().volatile.commit_index;
                self.pending_reads.push(PendingRead {
                    key: key.clone(),
                    request_id: req.id,
                    read_index,
                    reply,
                });
                let raft = self.take_raft();
                let (new_raft, actions) = raft.tick();
                self.raft = Some(new_raft);
                self.apply_actions(actions).await;
            }
            ClientOperation::Put { .. } | ClientOperation::Delete { .. } => {
                // Record the pending write before entering the core so we can
                // associate the log index with the reply channel.
                let next_index = self.raft().log().last_index() + 1;
                self.pending_writes.insert(
                    next_index,
                    PendingWrite {
                        request_id: req.id,
                        reply,
                    },
                );
                let raft = self.take_raft();
                let (new_raft, actions) = raft.handle_client_request(req);
                self.raft = Some(new_raft);
                self.apply_actions(actions).await;
            }
        }
    }

    async fn handle_election_timeout(&mut self) {
        let raft = self.take_raft();
        let (new_raft, actions) = raft.election_timeout();
        self.raft = Some(new_raft);
        self.apply_actions(actions).await;
    }

    async fn handle_heartbeat(&mut self) {
        if !self.raft().is_leader() {
            return;
        }
        let raft = self.take_raft();
        let (new_raft, actions) = raft.tick();
        self.raft = Some(new_raft);
        self.apply_actions(actions).await;
    }

    // ── Action executor ────────────────────────────────────────────────────

    /// Execute all side-effects in `actions` and return whether the election
    /// timer should be reset.
    async fn apply_actions(&mut self, actions: raft_core::Actions) -> bool {
        // 1. Persist durable state BEFORE sending any messages (Raft §5.4.1).
        if let Some(ref state) = actions.persist
            && let Err(e) = storage::save(&self.state_path, state, !self.no_sync).await
        {
            error!("failed to persist state: {e}");
            // In production we would halt here to avoid violating durability.
        }

        // 2. Send outbound peer messages.
        let msg_count = actions.messages.len() as u64;
        for (to, msg) in actions.messages {
            let _ = self.transport_tx.send(Outgoing { to, message: msg }).await;
        }
        if msg_count > 0 {
            self.metrics
                .messages_sent
                .fetch_add(msg_count, std::sync::atomic::Ordering::Relaxed);
        }

        // 3. Apply committed entries to the KV state machine.
        for entry in &actions.entries_to_apply {
            let (new_kv, result) = self.kv.apply(&entry.command);
            self.kv = new_kv;
            if let Some(raft) = self.raft.as_mut() {
                raft.volatile.last_applied = entry.index;
            }

            // Resolve pending write for this log index (using actual KV result).
            if let Some(pending) = self.pending_writes.remove(&entry.index) {
                let response = ClientResponse {
                    id: pending.request_id,
                    result: ClientResult::Ok(result),
                };
                let _ = pending.reply.send(response);
            }
        }

        // 4. Serve pending reads if last_applied has caught up to read_index.
        self.drain_pending_reads();

        // 5. Update live metrics after every state transition.
        {
            use std::sync::atomic::Ordering::Relaxed;
            let r = self.raft();
            let role_u8 = match r.role {
                raft_core::state::Role::Follower => 0,
                raft_core::state::Role::Candidate => 1,
                raft_core::state::Role::Leader => 2,
            };
            self.metrics.term.store(r.persistent.current_term, Relaxed);
            self.metrics.role.store(role_u8, Relaxed);
            self.metrics
                .commit_index
                .store(r.volatile.commit_index, Relaxed);
            self.metrics
                .last_applied
                .store(r.volatile.last_applied, Relaxed);
            self.metrics.log_length.store(r.log().len() as u64, Relaxed);
            self.metrics
                .pending_writes
                .store(self.pending_writes.len() as u64, Relaxed);
            self.metrics
                .pending_reads
                .store(self.pending_reads.len() as u64, Relaxed);
        }

        actions.reset_election_timer
    }

    fn drain_pending_reads(&mut self) {
        let last_applied = self.raft().volatile.last_applied;

        let (ready, waiting): (Vec<PendingRead>, Vec<PendingRead>) = self
            .pending_reads
            .drain(..)
            .partition(|pr| last_applied >= pr.read_index);

        self.pending_reads = waiting;

        for pr in ready {
            let value = self.kv.get(&pr.key).map(str::to_owned);
            let response = ClientResponse {
                id: pr.request_id,
                result: ClientResult::Ok(value),
            };
            let _ = pr.reply.send(response);
        }
    }
}

// ── In-process client handle (used for testing) ────────────────────────────

/// A handle for submitting client requests to the node actor in-process.
#[derive(Clone)]
pub struct NodeHandle {
    tx: mpsc::Sender<(ClientRequest, oneshot::Sender<ClientResponse>)>,
}

impl NodeHandle {
    pub fn new(tx: mpsc::Sender<(ClientRequest, oneshot::Sender<ClientResponse>)>) -> Self {
        Self { tx }
    }

    pub async fn request(&self, req: ClientRequest) -> Result<ClientResponse> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send((req, reply_tx))
            .await
            .map_err(|_| anyhow::anyhow!("node shut down"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("node dropped reply"))
    }
}
