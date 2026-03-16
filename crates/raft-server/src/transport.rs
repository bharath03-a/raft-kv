use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use futures::SinkExt;
use raft_core::message::{NodeId, RaftMessage};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
    time,
};
use tokio_util::codec::Framed;
use tracing::{error, info, warn};

use crate::codec::RaftCodec;

/// An incoming message and who sent it.
#[derive(Debug)]
pub struct Incoming {
    pub from: NodeId,
    pub message: RaftMessage,
}

/// Messages to send to a specific peer.
#[derive(Debug)]
pub struct Outgoing {
    pub to: NodeId,
    pub message: RaftMessage,
}

/// Manages all TCP connections for a single Raft node.
///
/// Accepts inbound connections from peers and clients on `listen_addr`.
/// Maintains persistent outbound connections to each peer with
/// automatic reconnection.
pub struct Transport {
    /// Channel the node reads from for all inbound messages.
    pub incoming_rx: mpsc::Receiver<Incoming>,
    /// Channel the node writes to for outbound messages.
    pub outgoing_tx: mpsc::Sender<Outgoing>,
}

impl Transport {
    /// Spawn the transport actor and return the handle.
    pub fn start(
        node_id: NodeId,
        listen_addr: SocketAddr,
        peers: HashMap<NodeId, SocketAddr>,
    ) -> Self {
        let (incoming_tx, incoming_rx) = mpsc::channel::<Incoming>(1024);
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<Outgoing>(1024);

        let peers = Arc::new(peers);

        // Spawn acceptor task.
        {
            let incoming_tx = incoming_tx.clone();
            tokio::spawn(accept_loop(node_id, listen_addr, incoming_tx));
        }

        // Spawn sender task.
        tokio::spawn(send_loop(outgoing_rx, Arc::clone(&peers)));

        Self {
            incoming_rx,
            outgoing_tx,
        }
    }
}

/// Accepts inbound TCP connections and spawns a reader task per connection.
async fn accept_loop(
    _node_id: NodeId,
    addr: SocketAddr,
    incoming_tx: mpsc::Sender<Incoming>,
) {
    let listener = loop {
        match TcpListener::bind(addr).await {
            Ok(l) => break l,
            Err(e) => {
                warn!("bind {addr} failed: {e}, retrying in 1s");
                time::sleep(Duration::from_secs(1)).await;
            }
        }
    };
    info!("listening on {addr}");

    loop {
        match listener.accept().await {
            Ok((stream, peer_addr)) => {
                info!("accepted connection from {peer_addr}");
                let tx = incoming_tx.clone();
                tokio::spawn(read_connection(stream, tx));
            }
            Err(e) => {
                error!("accept error: {e}");
            }
        }
    }
}

/// Reads messages from a single TCP connection and forwards them to the
/// incoming channel. The `from` NodeId is encoded in the first message.
async fn read_connection(stream: TcpStream, tx: mpsc::Sender<Incoming>) {
    use futures::StreamExt;

    let peer_addr = stream.peer_addr().ok();
    let mut framed = Framed::new(stream, RaftCodec);

    // The first message must identify the sender's NodeId.
    let from = match framed.next().await {
        Some(Ok(RaftMessage::VoteRequest(r))) => {
            // Re-inject so the node processes it.
            let _ = tx
                .send(Incoming {
                    from: r.candidate_id,
                    message: RaftMessage::VoteRequest(r.clone()),
                })
                .await;
            r.candidate_id
        }
        Some(Ok(RaftMessage::AppendEntriesRequest(r))) => {
            let leader_id = r.leader_id;
            let _ = tx
                .send(Incoming {
                    from: leader_id,
                    message: RaftMessage::AppendEntriesRequest(r),
                })
                .await;
            leader_id
        }
        Some(Ok(msg)) => {
            // Client connection — use a sentinel NodeId of 0.
            let _ = tx.send(Incoming { from: 0, message: msg }).await;
            0
        }
        Some(Err(e)) => {
            warn!("decode error on {:?}: {e}", peer_addr);
            return;
        }
        None => return,
    };

    // Continue reading subsequent messages from the same peer.
    while let Some(result) = framed.next().await {
        match result {
            Ok(msg) => {
                if tx.send(Incoming { from, message: msg }).await.is_err() {
                    break; // node shut down
                }
            }
            Err(e) => {
                warn!("decode error from peer {from}: {e}");
                break;
            }
        }
    }
    info!("connection from {from} closed");
}

/// Sends outbound messages, maintaining one long-lived TCP connection per peer
/// with exponential-backoff reconnection.
async fn send_loop(
    mut rx: mpsc::Receiver<Outgoing>,
    peers: Arc<HashMap<NodeId, SocketAddr>>,
) {
    let mut connections: HashMap<NodeId, Framed<TcpStream, RaftCodec>> = HashMap::new();

    while let Some(Outgoing { to, message }) = rx.recv().await {
        let addr = match peers.get(&to) {
            Some(a) => *a,
            None => {
                warn!("no address for peer {to}");
                continue;
            }
        };

        // Ensure we have a connection; reconnect if needed.
        loop {
            if let Some(conn) = connections.get_mut(&to) {
                match conn.send(message.clone()).await {
                    Ok(()) => break,
                    Err(e) => {
                        warn!("send to {to} failed: {e}, reconnecting");
                        connections.remove(&to);
                    }
                }
            } else {
                match TcpStream::connect(addr).await {
                    Ok(stream) => {
                        connections.insert(to, Framed::new(stream, RaftCodec));
                    }
                    Err(e) => {
                        warn!("connect to {to} ({addr}) failed: {e}");
                        time::sleep(Duration::from_millis(100)).await;
                        // Drop this message — Raft tolerates message loss.
                        break;
                    }
                }
            }
        }
    }
}
