use std::{net::SocketAddr, time::Duration};

use anyhow::{Context, Result, bail};
use futures::{SinkExt, StreamExt};
use raft_core::message::{
    ClientOperation, ClientRequest, ClientResponse, ClientResult, NodeId, RaftMessage,
};
use tokio::net::TcpStream;
use tokio_util::codec::Framed;
use tracing::{info, warn};

// Share the same codec as the server.
use crate::codec::RaftCodec;

/// A connection to a Raft cluster that automatically follows leader redirects.
pub struct ClusterConnection {
    addrs: Vec<SocketAddr>,
    current: SocketAddr,
    framed: Framed<TcpStream, RaftCodec>,
    next_request_id: u64,
}

impl ClusterConnection {
    /// Connect to the first responsive node in `addrs`.
    pub async fn connect(addrs: Vec<SocketAddr>) -> Result<Self> {
        for &addr in &addrs {
            match Self::try_connect(addr).await {
                Ok(framed) => {
                    info!("connected to {addr}");
                    return Ok(Self {
                        addrs,
                        current: addr,
                        framed,
                        next_request_id: 1,
                    });
                }
                Err(e) => warn!("could not connect to {addr}: {e}"),
            }
        }
        bail!("could not connect to any cluster node");
    }

    /// Send a GET and return the value (or `None` if the key doesn't exist).
    pub async fn get(&mut self, key: impl Into<String>) -> Result<Option<String>> {
        let resp = self.send(ClientOperation::Get { key: key.into() }).await?;
        match resp.result {
            ClientResult::Ok(value) => Ok(value),
            ClientResult::Error(e) => bail!("server error: {e}"),
            ClientResult::NotLeader { .. } => unreachable!("handled in send()"),
        }
    }

    /// Send a PUT and return the previous value if any.
    pub async fn put(
        &mut self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<Option<String>> {
        let resp = self
            .send(ClientOperation::Put {
                key: key.into(),
                value: value.into(),
            })
            .await?;
        match resp.result {
            ClientResult::Ok(prev) => Ok(prev),
            ClientResult::Error(e) => bail!("server error: {e}"),
            ClientResult::NotLeader { .. } => unreachable!("handled in send()"),
        }
    }

    /// Send a DELETE and return the removed value if any.
    pub async fn delete(&mut self, key: impl Into<String>) -> Result<Option<String>> {
        let resp = self
            .send(ClientOperation::Delete { key: key.into() })
            .await?;
        match resp.result {
            ClientResult::Ok(removed) => Ok(removed),
            ClientResult::Error(e) => bail!("server error: {e}"),
            ClientResult::NotLeader { .. } => unreachable!("handled in send()"),
        }
    }

    // ── Private ────────────────────────────────────────────────────────────

    /// Send an operation, following leader redirects up to 5 times.
    async fn send(&mut self, op: ClientOperation) -> Result<ClientResponse> {
        const MAX_REDIRECTS: usize = 5;
        let id = self.next_request_id;
        self.next_request_id += 1;

        for attempt in 0..MAX_REDIRECTS {
            let req = ClientRequest {
                id,
                operation: op.clone(),
            };
            self.framed
                .send(RaftMessage::ClientRequest(req))
                .await
                .context("send request")?;

            let msg = tokio::time::timeout(Duration::from_secs(5), self.framed.next())
                .await
                .context("response timeout")?
                .context("connection closed")?
                .context("decode error")?;

            if let RaftMessage::ClientResponse(resp) = msg {
                match &resp.result {
                    ClientResult::NotLeader { leader_hint } => {
                        warn!(attempt, leader_hint, "not leader — redirecting");
                        self.redirect(*leader_hint).await?;
                    }
                    _ => return Ok(resp),
                }
            }
        }

        bail!("too many leader redirects");
    }

    /// Reconnect to the hinted leader, or try all known addresses.
    async fn redirect(&mut self, hint: Option<NodeId>) -> Result<()> {
        // We don't have a NodeId→addr map at the client level, so we try
        // all addresses. In a production client we would maintain the map.
        let _ = hint;
        let addrs = self.addrs.clone();
        for &addr in &addrs {
            if addr == self.current {
                continue; // don't retry the node that just redirected us
            }
            match Self::try_connect(addr).await {
                Ok(framed) => {
                    info!("redirected to {addr}");
                    self.current = addr;
                    self.framed = framed;
                    return Ok(());
                }
                Err(e) => warn!("redirect to {addr} failed: {e}"),
            }
        }
        bail!("could not find the leader after redirect");
    }

    async fn try_connect(addr: SocketAddr) -> Result<Framed<TcpStream, RaftCodec>> {
        let stream = TcpStream::connect(addr)
            .await
            .with_context(|| format!("connect to {addr}"))?;
        Ok(Framed::new(stream, RaftCodec))
    }
}
