mod codec;
mod connection;

use std::net::SocketAddr;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use connection::ClusterConnection;

/// Raft KV — command-line client.
///
/// Connects to a Raft cluster and issues GET / PUT / DELETE commands.
///
/// Example:
///   raft-client --cluster 127.0.0.1:7001,127.0.0.1:7002,127.0.0.1:7003 put foo bar
#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    /// Comma-separated list of cluster node addresses.
    #[arg(long, default_value = "127.0.0.1:7001,127.0.0.1:7002,127.0.0.1:7003")]
    cluster: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Get the value for a key.
    Get { key: String },
    /// Set a key to a value.
    Put { key: String, value: String },
    /// Delete a key.
    Delete { key: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let args = Args::parse();

    let addrs: Vec<SocketAddr> = args
        .cluster
        .split(',')
        .map(|s| s.parse().with_context(|| format!("invalid address: {s}")))
        .collect::<Result<_>>()?;

    let mut conn = ClusterConnection::connect(addrs).await?;

    match args.command {
        Command::Get { key } => match conn.get(&key).await? {
            Some(value) => println!("{value}"),
            None => {
                eprintln!("(nil)");
                std::process::exit(1);
            }
        },
        Command::Put { key, value } => {
            conn.put(&key, &value).await?;
            println!("OK");
        }
        Command::Delete { key } => match conn.delete(&key).await? {
            Some(old) => println!("(deleted: {old})"),
            None => println!("(nil — key did not exist)"),
        },
    }

    Ok(())
}
