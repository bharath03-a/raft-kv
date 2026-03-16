use std::{collections::HashMap, net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use raft_core::{message::NodeId, RaftConfig};
use raft_server::node::NodeActor;
use tracing::info;

/// Raft KV — a distributed key-value store node.
#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    /// This node's ID (must be unique within the cluster).
    #[arg(long)]
    id: NodeId,

    /// Address this node listens on for both peer and client connections.
    ///
    /// Example: `--addr 127.0.0.1:7001`
    #[arg(long)]
    addr: SocketAddr,

    /// Comma-separated list of `id=addr` pairs for all **other** nodes.
    ///
    /// Example: `--peers 2=127.0.0.1:7002,3=127.0.0.1:7003`
    #[arg(long, default_value = "")]
    peers: String,

    /// Directory to store durable state.
    #[arg(long, default_value = "data")]
    data_dir: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "raft_server=info".into()),
        )
        .init();

    let args = Args::parse();

    // Parse peer list: "2=127.0.0.1:7002,3=127.0.0.1:7003"
    let peers: HashMap<NodeId, SocketAddr> = args
        .peers
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| {
            let (id_str, addr_str) = s
                .split_once('=')
                .with_context(|| format!("invalid peer spec: {s}"))?;
            let id: NodeId = id_str.parse().context("peer id")?;
            let addr: SocketAddr = addr_str.parse().context("peer addr")?;
            Ok((id, addr))
        })
        .collect::<Result<_>>()?;

    tokio::fs::create_dir_all(&args.data_dir)
        .await
        .context("create data dir")?;

    let config = RaftConfig::default_local();

    let actor = NodeActor::new(
        args.id,
        peers,
        args.addr,
        args.data_dir,
        config,
    )
    .await?;

    info!(id = args.id, addr = %args.addr, "starting node");

    actor.run().await;

    Ok(())
}
