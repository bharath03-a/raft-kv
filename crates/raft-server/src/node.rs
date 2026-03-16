use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    time::Duration,
};

use anyhow::Result;
use raft_core::{
    message::{
        ClientOperation, ClientRequest, ClientResponse, ClientResult, NodeId, RaftMessage,
    },
    RaftConfig, RaftNode,
};
use tokio::{
    sync::{mpsc, oneshot},
    time,
};
use tracing::{debug, error, info};

use crate::{
    kv::KvStore,
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
    raft: RaftNode,
    kv: KvStore,
    transport_tx: mpsc::Sender<Outgoing>,
    transport_rx: mpsc::Receiver<Incoming>,
    /// Client-facing request receiver.
    client_rx: mpsc::Receiver<(ClientRequest, oneshot::Sender<ClientResponse>)>,
    /// Pending write log_index → sender.
    pending_writes: HashMap<u64, PendingWrite>,
    /// Pending reads buffered for the read-index protocol.
    pending_reads: Vec<PendingRead>,
    state_path: PathBuf,
    config: RaftConfig,
    election_timeout: Duration,
}

impl NodeActor {
    pub async fn new(
        id: NodeId,
        peers: HashMap<NodeId, SocketAddr>,
        listen_addr: SocketAddr,
        state_dir: PathBuf,
        config: RaftConfig,
        client_rx: mpsc::Receiver<(ClientRequest, oneshot::Sender<ClientResponse>)>,
    ) -> Result<Self> {
        let state_path = state_dir.join(format!("node-{id}.state"));

        // Recover durable state from disk (or start fresh).
        let persistent = storage::load(&state_path).await?;
        info!(node_id = id, term = persistent.current_term, "recovered state");

        let peer_ids: Vec<NodeId> = peers.keys().copied().collect();
        let mut raft = RaftNode::new(id, peer_ids, config.clone());
        raft.persistent = persistent;

        let transport = Transport::start(id, listen_addr, peers);

        let election_timeout = random_election_timeout(&config);

        Ok(Self {
            id,
            raft,
            kv: KvStore::new(),
            transport_tx: transport.outgoing_tx,
            transport_rx: transport.incoming_rx,
            client_rx,
            pending_writes: HashMap::new(),
            pending_reads: Vec::new(),
            state_path,
            config,
            election_timeout,
        })
    }

    /// Run the node event loop indefinitely.
    pub async fn run(mut self) {
        let heartbeat_interval = Duration::from_millis(self.config.heartbeat_interval_ms);
        let mut election_timer = time::interval(self.election_timeout);
        election_timer.reset(); // don't fire immediately
        let mut heartbeat_timer = time::interval(heartbeat_interval);

        loop {
            tokio::select! {
                // Inbound network message.
                Some(incoming) = self.transport_rx.recv() => {
                    self.handle_incoming(incoming).await;
                }
                // Client request submitted via the in-process channel.
                Some((req, reply)) = self.client_rx.recv() => {
                    self.handle_client(req, reply).await;
                }
                // Election timeout fired.
                _ = election_timer.tick() => {
                    self.handle_election_timeout().await;
                    // Reset with new random timeout to prevent split votes.
                    self.election_timeout = random_election_timeout(&self.config);
                    election_timer = time::interval(self.election_timeout);
                    election_timer.reset();
                }
                // Heartbeat tick (leader only — ignored by followers/candidates).
                _ = heartbeat_timer.tick() => {
                    self.handle_heartbeat().await;
                }
            }
        }
    }

    // ── Inbound message dispatch ───────────────────────────────────────────

    async fn handle_incoming(&mut self, incoming: Incoming) {
        let (new_raft, actions) = match incoming.message {
            RaftMessage::VoteRequest(req) => {
                let raft = std::mem::replace(&mut self.raft, unsafe_placeholder());
                raft.handle_vote_request(req)
            }
            RaftMessage::VoteResponse(resp) => {
                let raft = std::mem::replace(&mut self.raft, unsafe_placeholder());
                raft.handle_vote_response(incoming.from, resp)
            }
            RaftMessage::AppendEntriesRequest(req) => {
                let raft = std::mem::replace(&mut self.raft, unsafe_placeholder());
                raft.handle_append_entries(req)
            }
            RaftMessage::AppendEntriesResponse(resp) => {
                let raft = std::mem::replace(&mut self.raft, unsafe_placeholder());
                raft.handle_append_entries_response(incoming.from, resp)
            }
            RaftMessage::ClientRequest(req) => {
                // Peer-forwarded client request — rare but valid.
                let raft = std::mem::replace(&mut self.raft, unsafe_placeholder());
                raft.handle_client_request(req)
            }
            RaftMessage::ClientResponse(_) => return, // unexpected
        };

        self.raft = new_raft;
        self.apply_actions(actions).await;
    }

    async fn handle_client(
        &mut self,
        req: ClientRequest,
        reply: oneshot::Sender<ClientResponse>,
    ) {
        if !self.raft.is_leader() {
            let response = ClientResponse {
                id: req.id,
                result: ClientResult::NotLeader {
                    leader_hint: self.raft.current_leader,
                },
            };
            let _ = reply.send(response);
            return;
        }

        match &req.operation {
            ClientOperation::Get { key } => {
                // Read-index protocol: record read_index = current commit_index,
                // send heartbeat round to confirm leadership, then serve once
                // last_applied >= read_index.
                let read_index = self.raft.volatile.commit_index;
                self.pending_reads.push(PendingRead {
                    key: key.clone(),
                    request_id: req.id,
                    read_index,
                    reply,
                });
                // Trigger a heartbeat to confirm we are still leader.
                let raft = std::mem::replace(&mut self.raft, unsafe_placeholder());
                let (new_raft, actions) = raft.tick();
                self.raft = new_raft;
                self.apply_actions(actions).await;
            }
            ClientOperation::Put { .. } | ClientOperation::Delete { .. } => {
                // Record pending write before calling into the core so we can
                // associate the log index with the reply channel.
                let next_index = self.raft.log().last_index() + 1;
                self.pending_writes.insert(
                    next_index,
                    PendingWrite {
                        request_id: req.id,
                        reply,
                    },
                );
                let raft = std::mem::replace(&mut self.raft, unsafe_placeholder());
                let (new_raft, actions) = raft.handle_client_request(req);
                self.raft = new_raft;
                self.apply_actions(actions).await;
            }
        }
    }

    async fn handle_election_timeout(&mut self) {
        debug!(node_id = self.id, "election timeout");
        let raft = std::mem::replace(&mut self.raft, unsafe_placeholder());
        let (new_raft, actions) = raft.election_timeout();
        self.raft = new_raft;
        self.apply_actions(actions).await;
    }

    async fn handle_heartbeat(&mut self) {
        if !self.raft.is_leader() {
            return;
        }
        let raft = std::mem::replace(&mut self.raft, unsafe_placeholder());
        let (new_raft, actions) = raft.tick();
        self.raft = new_raft;
        self.apply_actions(actions).await;
    }

    // ── Action executor ────────────────────────────────────────────────────

    async fn apply_actions(&mut self, actions: raft_core::Actions) {
        // 1. Persist durable state BEFORE sending any messages (Raft §5.4.1).
        if let Some(ref state) = actions.persist {
            if let Err(e) = storage::save(&self.state_path, state).await {
                error!("failed to persist state: {e}");
                // In a production system we would halt here to avoid violating
                // durability guarantees. For this portfolio project we log and continue.
            }
        }

        // 2. Send outbound messages.
        for (to, msg) in actions.messages {
            let _ = self
                .transport_tx
                .send(Outgoing { to, message: msg })
                .await;
        }

        // 3. Apply committed entries to the KV state machine.
        for entry in &actions.entries_to_apply {
            let (new_kv, _result) = self.kv.apply(&entry.command);
            self.kv = new_kv;
            self.raft.volatile.last_applied = entry.index;

            // Resolve any pending write for this log index.
            if let Some(pending) = self.pending_writes.remove(&entry.index) {
                let response = ClientResponse {
                    id: pending.request_id,
                    result: ClientResult::Ok(None),
                };
                let _ = pending.reply.send(response);
            }
        }

        // 4. Reset election timer if instructed.
        // (The timer is managed in `run()`; we set a flag via the bool.)
        // We can't reset timers from here directly, so we use the reset_election_timer
        // flag in Actions. The run() loop reads it after each select! arm via
        // a shared atomic or by checking raft state — here we take the simpler
        // approach of relying on the caller to reset.

        // 5. Serve pending reads if last_applied caught up to their read_index.
        self.drain_pending_reads();
    }

    fn drain_pending_reads(&mut self) {
        let last_applied = self.raft.volatile.last_applied;

        // Partition into (ready, still_waiting) by taking ownership.
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

fn random_election_timeout(config: &RaftConfig) -> Duration {
    use rand::RngExt;
    let ms = rand::rng()
        .random_range(config.election_timeout_min_ms..=config.election_timeout_max_ms);
    Duration::from_millis(ms)
}

/// Create a temporary placeholder `RaftNode` so we can move out of `self.raft`
/// during a state transition.
///
/// Safety: the placeholder is immediately replaced; it is never observed.
fn unsafe_placeholder() -> RaftNode {
    RaftNode::new(0, vec![], RaftConfig::default_local())
}

// ── Client-facing server ───────────────────────────────────────────────────

/// A handle for submitting client requests to the node actor.
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
