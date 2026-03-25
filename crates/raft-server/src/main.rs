use std::{collections::HashMap, net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use raft_core::{message::NodeId, RaftConfig};
use raft_server::{metrics, node::NodeActor};
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

    /// Skip fsync on each write (higher throughput, NOT crash-safe).
    ///
    /// Use only for benchmarking. Data may be lost on power failure.
    #[arg(long, default_value_t = false)]
    no_sync: bool,

    /// Address to serve Prometheus metrics over HTTP.
    ///
    /// Example: `--metrics-addr 0.0.0.0:9001`
    /// Metrics are served at any path (e.g. `curl http://127.0.0.1:9000/metrics`).
    #[arg(long, default_value = "127.0.0.1:9000")]
    metrics_addr: SocketAddr,
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

    if args.no_sync {
        tracing::warn!("--no-sync: fsync disabled — cluster is NOT crash-safe");
    }

    let actor = NodeActor::new(
        args.id,
        peers,
        args.addr,
        args.data_dir,
        config,
        args.no_sync,
    )
    .await?;

    // Spawn the Prometheus metrics HTTP server as a background task.
    let node_metrics = actor.metrics();
    tokio::spawn(metrics::serve(args.metrics_addr, args.id, node_metrics));

    info!(id = args.id, addr = %args.addr, metrics = %args.metrics_addr, "starting node");

    actor.run().await;

    Ok(())
}
