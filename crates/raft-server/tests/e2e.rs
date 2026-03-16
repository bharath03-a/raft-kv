//! End-to-end integration tests for the Raft KV cluster.
//!
//! Each test spins up real `NodeActor` instances as tokio tasks (no process
//! spawning), then connects via raw TCP using the same wire protocol as the
//! production client. This exercises the complete stack including:
//!
//!   - TCP accept loop
//!   - Client connection handling (read-request / write-response loop)
//!   - Raft leader election and log replication
//!   - KV state machine application
//!   - Leader-redirect following by the test client

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::atomic::{AtomicU16, Ordering},
    time::Duration,
};

use futures::{SinkExt, StreamExt};
use raft_core::message::{
    ClientOperation, ClientRequest, ClientResult, NodeId, RaftMessage,
};
use raft_core::RaftConfig;
use raft_server::{codec::RaftCodec, node::NodeActor};
use tokio::{net::TcpStream, time};
use tokio_util::codec::Framed;

// ── Port allocation ────────────────────────────────────────────────────────

/// Starting port for test clusters. Each `alloc_port()` call increments by 1.
static NEXT_PORT: AtomicU16 = AtomicU16::new(19_000);

fn alloc_port() -> u16 {
    NEXT_PORT.fetch_add(1, Ordering::SeqCst)
}

// ── In-test cluster helper ─────────────────────────────────────────────────

struct Cluster {
    addrs: Vec<SocketAddr>,
    // node_ids aligned with addrs
    node_ids: Vec<NodeId>,
}

impl Cluster {
    /// Spin up `n` `NodeActor` instances as background tasks in the current
    /// tokio runtime and wait for leader election to complete.
    async fn start(n: usize) -> Self {
        assert!(n >= 1);

        let data_dir = tempfile::TempDir::new().expect("tmpdir");

        let ports: Vec<u16> = (0..n).map(|_| alloc_port()).collect();
        let addrs: Vec<SocketAddr> = ports
            .iter()
            .map(|&p| format!("127.0.0.1:{p}").parse().unwrap())
            .collect();
        let node_ids: Vec<NodeId> = (1..=n as u64).collect();

        // Build the complete id→addr map.
        let id_addr: HashMap<NodeId, SocketAddr> =
            node_ids.iter().copied().zip(addrs.iter().copied()).collect();

        for &id in &node_ids {
            let peers: HashMap<NodeId, SocketAddr> = id_addr
                .iter()
                .filter(|&(&pid, _)| pid != id)
                .map(|(&pid, &paddr)| (pid, paddr))
                .collect();

            let actor = NodeActor::new(
                id,
                peers,
                id_addr[&id],
                data_dir.path().join(format!("node-{id}")),
                RaftConfig::default_local(),
            )
            .await
            .expect("NodeActor::new");

            // Leak the tempdir so it lives as long as the test.
            // The OS cleans it up when the process exits.
            let _ = std::mem::ManuallyDrop::new(tempfile::TempDir::new().unwrap());

            tokio::spawn(actor.run());
        }

        // Wait for election (election_timeout ∈ [150, 300] ms → allow 700 ms).
        time::sleep(Duration::from_millis(700)).await;

        Self { addrs, node_ids }
    }

    /// Send a single client operation, following leader redirects automatically.
    /// Returns the `ClientResult` from the leader.
    async fn send(&self, op: ClientOperation) -> ClientResult {
        let mut req_id: u64 = 1;
        let mut addrs = self.addrs.clone();

        for _ in 0..10 {
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

                let resp = time::timeout(Duration::from_secs(3), framed.next()).await;
                let Ok(Some(Ok(RaftMessage::ClientResponse(resp)))) = resp else {
                    continue;
                };

                match resp.result {
                    ClientResult::NotLeader { leader_hint } => {
                        // Reorder addrs to try the hinted leader first next iteration.
                        if let Some(hint_id) = leader_hint {
                            if let Some(idx) = self
                                .node_ids
                                .iter()
                                .position(|&nid| nid == hint_id)
                            {
                                addrs = {
                                    let mut v = self.addrs.clone();
                                    v.swap(0, idx);
                                    v
                                };
                            }
                        }
                        req_id += 1;
                        break; // try again with the new order
                    }
                    result => return result,
                }
            }
            time::sleep(Duration::from_millis(100)).await;
        }

        panic!("could not reach a leader after retries");
    }

    async fn put(&self, key: &str, value: &str) -> Option<String> {
        match self
            .send(ClientOperation::Put {
                key: key.into(),
                value: value.into(),
            })
            .await
        {
            ClientResult::Ok(prev) => prev,
            r => panic!("PUT unexpected result: {r:?}"),
        }
    }

    async fn get(&self, key: &str) -> Option<String> {
        match self.send(ClientOperation::Get { key: key.into() }).await {
            ClientResult::Ok(val) => val,
            r => panic!("GET unexpected result: {r:?}"),
        }
    }

    async fn delete(&self, key: &str) -> Option<String> {
        match self
            .send(ClientOperation::Delete { key: key.into() })
            .await
        {
            ClientResult::Ok(removed) => removed,
            r => panic!("DELETE unexpected result: {r:?}"),
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn single_node_put_get_delete() {
    let c = Cluster::start(1).await;

    assert_eq!(c.put("x", "hello").await, None, "first PUT returns no prev");
    assert_eq!(c.get("x").await, Some("hello".into()));
    assert_eq!(c.put("x", "world").await, Some("hello".into()), "overwrite returns prev");
    assert_eq!(c.get("x").await, Some("world".into()));
    assert_eq!(c.delete("x").await, Some("world".into()));
    assert_eq!(c.get("x").await, None, "key absent after delete");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_node_cluster_basic_ops() {
    let c = Cluster::start(3).await;

    // Write multiple keys.
    c.put("a", "1").await;
    c.put("b", "2").await;
    c.put("c", "3").await;

    // Read them back.
    assert_eq!(c.get("a").await, Some("1".into()));
    assert_eq!(c.get("b").await, Some("2".into()));
    assert_eq!(c.get("c").await, Some("3".into()));

    // Delete one; others untouched.
    assert_eq!(c.delete("b").await, Some("2".into()));
    assert_eq!(c.get("b").await, None);
    assert_eq!(c.get("a").await, Some("1".into()));
    assert_eq!(c.get("c").await, Some("3".into()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_node_cluster_concurrent_writes() {
    let c = Cluster::start(3).await;

    // Fire multiple writes concurrently.
    let puts: Vec<_> = (0u64..20)
        .map(|i| {
            let c = &c;
            async move { c.put(&format!("k{i}"), &i.to_string()).await }
        })
        .collect();

    futures::future::join_all(puts).await;

    // All keys should be readable.
    for i in 0u64..20 {
        assert_eq!(
            c.get(&format!("k{i}")).await,
            Some(i.to_string()),
            "key k{i} missing after concurrent writes"
        );
    }
}

/// Throughput: sequential PUTs; prints ops/s — not a pass/fail assertion.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn throughput_sequential_puts() {
    const N: u64 = 50;
    let c = Cluster::start(3).await;

    let start = std::time::Instant::now();
    for i in 0..N {
        c.put(&format!("bench-{i}"), &i.to_string()).await;
    }
    let elapsed = start.elapsed();
    let ops_per_sec = N as f64 / elapsed.as_secs_f64();

    // Print throughput for the user to inspect (not a hard assertion).
    println!();
    println!(
        "Throughput: {N} sequential PUTs in {elapsed:.2?}  →  {ops_per_sec:.0} ops/s"
    );
    println!();

    // Sanity-check a few entries were actually committed.
    assert_eq!(c.get("bench-0").await, Some("0".into()));
    assert_eq!(c.get(&format!("bench-{}", N - 1)).await, Some((N - 1).to_string()));
}
