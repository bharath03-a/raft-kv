//! Fault-injection integration tests.
//!
//! Each test starts a 3-node cluster where every node's transport randomly
//! drops a fraction of Raft peer messages. Tests assert that the cluster
//! still elects a leader and commits all writes correctly despite the drops.
//!
//! Client connections are never affected — only peer-to-peer Raft messages
//! (VoteRequest, VoteResponse, AppendEntriesRequest, AppendEntriesResponse)
//! are subject to the configured drop rate.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::atomic::{AtomicU16, Ordering},
    time::Duration,
};

use futures::{SinkExt, StreamExt};
use raft_core::{
    RaftConfig,
    message::{ClientOperation, ClientRequest, ClientResult, NodeId, RaftMessage},
};
use raft_server::{chaos::ChaosConfig, codec::RaftCodec, node::NodeActor};
use tempfile::TempDir;
use tokio::{net::TcpStream, task::JoinHandle, time};
use tokio_util::codec::Framed;

// ── Port allocation (separate range from e2e.rs to avoid conflicts) ────────

static NEXT_PORT: AtomicU16 = AtomicU16::new(20_000);

fn alloc_port() -> u16 {
    NEXT_PORT.fetch_add(1, Ordering::SeqCst)
}

// ── Chaos cluster helper ───────────────────────────────────────────────────

struct ChaosCluster {
    addrs: Vec<SocketAddr>,
    node_ids: Vec<NodeId>,
    handles: Vec<JoinHandle<()>>,
    _data_dir: TempDir,
}

impl ChaosCluster {
    /// Spin up `n` nodes where every node drops `drop_rate` fraction of
    /// inbound and outbound Raft peer messages.
    async fn start(n: usize, drop_rate: f64) -> Self {
        assert!(n >= 1);

        let data_dir = TempDir::new().expect("tmpdir");

        let ports: Vec<u16> = (0..n).map(|_| alloc_port()).collect();
        let addrs: Vec<SocketAddr> = ports
            .iter()
            .map(|&p| format!("127.0.0.1:{p}").parse().unwrap())
            .collect();
        let node_ids: Vec<NodeId> = (1..=n as u64).collect();

        let id_addr: HashMap<NodeId, SocketAddr> = node_ids
            .iter()
            .copied()
            .zip(addrs.iter().copied())
            .collect();

        let mut handles = Vec::with_capacity(n);

        let chaos = ChaosConfig {
            outbound_drop_rate: drop_rate,
            inbound_drop_rate: drop_rate,
        };

        for &id in &node_ids {
            let peers: HashMap<NodeId, SocketAddr> = id_addr
                .iter()
                .filter(|&(&pid, _)| pid != id)
                .map(|(&pid, &paddr)| (pid, paddr))
                .collect();

            tokio::fs::create_dir_all(data_dir.path().join(format!("node-{id}")))
                .await
                .expect("create node data dir");

            let actor = NodeActor::new_with_chaos(
                id,
                peers,
                id_addr[&id],
                data_dir.path().join(format!("node-{id}")),
                RaftConfig::default_local(),
                true, // no_sync: tests don't need crash durability
                chaos.clone(),
            )
            .await
            .expect("NodeActor::new_with_chaos");

            handles.push(tokio::spawn(actor.run()));
        }

        Self {
            addrs,
            node_ids,
            handles,
            _data_dir: data_dir,
        }
    }

    /// Send a single client operation, retrying through all nodes until a
    /// leader accepts it. Uses a longer timeout than the non-chaos tests to
    /// account for extra re-election rounds caused by dropped vote messages.
    async fn send(&self, op: ClientOperation) -> ClientResult {
        let mut req_id: u64 = 1;
        let mut addrs = self.addrs.clone();

        // 60 retries × 150ms = up to 9 seconds — generous enough for a cluster
        // that must hold a re-election under 20% message loss.
        for _ in 0..60 {
            for &addr in &addrs {
                let Ok(stream) = TcpStream::connect(addr).await else {
                    continue;
                };
                let mut framed = Framed::new(stream, RaftCodec);

                if framed
                    .send(RaftMessage::ClientRequest(ClientRequest {
                        id: req_id,
                        operation: op.clone(),
                    }))
                    .await
                    .is_err()
                {
                    continue;
                }

                let resp = time::timeout(Duration::from_secs(5), framed.next()).await;
                let Ok(Some(Ok(RaftMessage::ClientResponse(resp)))) = resp else {
                    continue;
                };

                match resp.result {
                    ClientResult::NotLeader { leader_hint } => {
                        if let Some(hint_id) = leader_hint
                            && let Some(idx) =
                                self.node_ids.iter().position(|&nid| nid == hint_id)
                        {
                            addrs = {
                                let mut v = self.addrs.clone();
                                v.swap(0, idx);
                                v
                            };
                        }
                        req_id += 1;
                        break;
                    }
                    result => return result,
                }
            }
            time::sleep(Duration::from_millis(150)).await;
        }

        panic!("chaos cluster: could not reach a leader after retries");
    }

    async fn put(&self, key: &str, value: &str) {
        match self
            .send(ClientOperation::Put {
                key: key.into(),
                value: value.into(),
            })
            .await
        {
            ClientResult::Ok(_) => {}
            other => panic!("put({key}) failed: {other:?}"),
        }
    }

    async fn get(&self, key: &str) -> Option<String> {
        match self.send(ClientOperation::Get { key: key.into() }).await {
            ClientResult::Ok(v) => v,
            other => panic!("get({key}) failed: {other:?}"),
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

/// A 3-node cluster with 20 % inbound + outbound message drops still elects a
/// leader and commits all writes correctly.
///
/// This verifies the fundamental Raft liveness property: the protocol makes
/// progress as long as a majority of nodes can communicate (eventually), even
/// if individual messages are lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cluster_converges_with_20_percent_message_drops() {
    let cluster = ChaosCluster::start(3, 0.20).await;

    // Allow extra time for election under message loss (2x normal election window).
    time::sleep(Duration::from_millis(1500)).await;

    // Write 20 keys sequentially.
    for i in 0u32..20 {
        cluster.put(&format!("chaos-key-{i}"), &i.to_string()).await;
    }

    // Verify all keys are readable from the cluster.
    for i in 0u32..20 {
        let got = cluster.get(&format!("chaos-key-{i}")).await;
        assert_eq!(
            got,
            Some(i.to_string()),
            "chaos-key-{i} missing or wrong after 20% drop"
        );
    }

    for h in cluster.handles {
        h.abort();
    }
}

/// A single-node cluster with 30 % message drops still functions correctly.
///
/// Single-node clusters don't use peer replication, so chaos has no effect
/// on Raft messages (there are none). This test confirms the single-node fast
/// path and that chaos config does not break the client path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_node_chaos_still_works() {
    let cluster = ChaosCluster::start(1, 0.30).await;

    time::sleep(Duration::from_millis(400)).await;

    cluster.put("foo", "bar").await;
    assert_eq!(cluster.get("foo").await, Some("bar".to_string()));

    for h in cluster.handles {
        h.abort();
    }
}
