use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use futures::SinkExt;
use raft_core::message::{ClientRequest, ClientResponse, NodeId, RaftMessage};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
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
/// A single listen address handles both peer-to-peer Raft traffic and
/// client requests. The first message on a connection determines its type:
/// - `VoteRequest` / `AppendEntriesRequest` → peer connection (forwarded to
///   the `Incoming` channel with the sender's `NodeId`)
/// - `ClientRequest` → client connection (read/write loop using the
///   `client_tx` channel with an oneshot reply)
pub struct Transport {
    /// Channel the node reads from for all inbound peer messages.
    pub incoming_rx: mpsc::Receiver<Incoming>,
    /// Channel the node writes to for outbound peer messages.
    pub outgoing_tx: mpsc::Sender<Outgoing>,
}

impl Transport {
    /// Spawn the transport actor and return the handle.
    pub fn start(
        node_id: NodeId,
        listen_addr: SocketAddr,
        peers: HashMap<NodeId, SocketAddr>,
        client_tx: mpsc::Sender<(ClientRequest, oneshot::Sender<ClientResponse>)>,
    ) -> Self {
        let (incoming_tx, incoming_rx) = mpsc::channel::<Incoming>(1024);
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<Outgoing>(1024);

        let peers = Arc::new(peers);

        // Spawn acceptor task.
        {
            let incoming_tx = incoming_tx.clone();
            tokio::spawn(accept_loop(node_id, listen_addr, incoming_tx, client_tx));
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
    client_tx: mpsc::Sender<(ClientRequest, oneshot::Sender<ClientResponse>)>,
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
                tokio::spawn(read_connection(
                    stream,
                    incoming_tx.clone(),
                    client_tx.clone(),
                ));
            }
            Err(e) => {
                error!("accept error: {e}");
            }
        }
    }
}

/// Reads the first message to classify the connection, then either:
/// - forwards all subsequent messages to `incoming_tx` (peer connection), or
/// - drives a request/response loop via `client_tx` (client connection).
async fn read_connection(
    stream: TcpStream,
    tx: mpsc::Sender<Incoming>,
    client_tx: mpsc::Sender<(ClientRequest, oneshot::Sender<ClientResponse>)>,
) {
    use futures::StreamExt;

    let peer_addr = stream.peer_addr().ok();
    let mut framed = Framed::new(stream, RaftCodec);

    // The first message determines whether this is a peer or a client.
    // Request messages carry the sender's NodeId in their body; response
    // messages carry it in the `peer_id` field added for exactly this purpose.
    let from = match framed.next().await {
        Some(Ok(RaftMessage::VoteRequest(r))) => {
            let from = r.candidate_id;
            let _ = tx.send(Incoming { from, message: RaftMessage::VoteRequest(r) }).await;
            from
        }
        Some(Ok(RaftMessage::AppendEntriesRequest(r))) => {
            let from = r.leader_id;
            let _ = tx
                .send(Incoming { from, message: RaftMessage::AppendEntriesRequest(r) })
                .await;
            from
        }
        Some(Ok(RaftMessage::VoteResponse(r))) => {
            let from = r.peer_id;
            let _ = tx.send(Incoming { from, message: RaftMessage::VoteResponse(r) }).await;
            from
        }
        Some(Ok(RaftMessage::AppendEntriesResponse(r))) => {
            let from = r.peer_id;
            let _ = tx
                .send(Incoming { from, message: RaftMessage::AppendEntriesResponse(r) })
                .await;
            from
        }
        Some(Ok(RaftMessage::ClientRequest(req))) => {
            // Client connection — switch to bidirectional request/response mode.
            handle_client_conn(framed, req, client_tx).await;
            return;
        }
        Some(Ok(msg)) => {
            warn!("unexpected first message from {:?}: {:?}", peer_addr, msg);
            return;
        }
        Some(Err(e)) => {
            warn!("decode error on {:?}: {e}", peer_addr);
            return;
        }
        None => return,
    };

    // Peer connection: continue reading subsequent messages.
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
    info!("peer connection from {from} closed");
}

/// Drives a client TCP connection: reads `ClientRequest` messages, submits
/// each one to the node via `client_tx`, and writes the `ClientResponse`
/// back to the same TCP connection.
async fn handle_client_conn(
    mut framed: Framed<TcpStream, RaftCodec>,
    first_req: ClientRequest,
    client_tx: mpsc::Sender<(ClientRequest, oneshot::Sender<ClientResponse>)>,
) {
    use futures::StreamExt;

    let mut next_req: Option<ClientRequest> = Some(first_req);

    loop {
        // Pick up either the pre-read first request or read the next one.
        let req = match next_req.take() {
            Some(r) => r,
            None => match framed.next().await {
                Some(Ok(RaftMessage::ClientRequest(r))) => r,
                Some(Ok(_)) => break, // unexpected message type
                Some(Err(e)) => {
                    warn!("client decode error: {e}");
                    break;
                }
                None => break, // client disconnected
            },
        };

        let (reply_tx, reply_rx) = oneshot::channel();
        if client_tx.send((req, reply_tx)).await.is_err() {
            break; // node shut down
        }

        match reply_rx.await {
            Ok(resp) => {
                if framed.send(RaftMessage::ClientResponse(resp)).await.is_err() {
                    break; // client disconnected mid-response
                }
            }
            Err(_) => break, // node dropped reply channel
        }
    }
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
