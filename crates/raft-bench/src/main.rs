//! raft-bench — load tester for the Raft KV cluster.
//!
//! Runs N concurrent async clients for a configurable duration, each
//! repeatedly issuing PUT or GET operations. At the end it prints throughput
//! and latency percentiles in a format similar to `ghz`.
//!
//! # Quick start
//!
//! ```bash
//! # Terminal 1-3: start a 3-node cluster
//! cargo run --release --bin raft-server -- --id 1 --addr 127.0.0.1:7001 \
//!     --peers 2=127.0.0.1:7002,3=127.0.0.1:7003
//! # … nodes 2 and 3 similarly …
//!
//! # Terminal 4: run the benchmark
//! cargo run --release --bin raft-bench -- \
//!     --cluster 127.0.0.1:7001,127.0.0.1:7002,127.0.0.1:7003 \
//!     --concurrency 20 --duration 30 --workload mixed
//! ```

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use futures::{SinkExt, StreamExt};
use raft_core::message::{ClientOperation, ClientRequest, ClientResult, NodeId, RaftMessage};
use tokio::{net::TcpStream, sync::Mutex, time};
use tokio_util::codec::Framed;
use tracing::warn;

mod codec {
    // Identical to raft-server/raft-client codec — length-delimited bincode.
    use std::io;

    use bytes::{Buf, BufMut, BytesMut};
    use raft_core::message::RaftMessage;
    use tokio_util::codec::{Decoder, Encoder};

    const MAX_FRAME: usize = 8 * 1024 * 1024;

    pub struct RaftCodec;

    impl Encoder<RaftMessage> for RaftCodec {
        type Error = io::Error;
        fn encode(&mut self, msg: RaftMessage, dst: &mut BytesMut) -> io::Result<()> {
            let payload = bincode::serialize(&msg)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            if payload.len() > MAX_FRAME {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "frame too large",
                ));
            }
            dst.reserve(4 + payload.len());
            dst.put_u32(payload.len() as u32);
            dst.put_slice(&payload);
            Ok(())
        }
    }

    impl Decoder for RaftCodec {
        type Item = RaftMessage;
        type Error = io::Error;
        fn decode(&mut self, src: &mut BytesMut) -> io::Result<Option<RaftMessage>> {
            if src.len() < 4 {
                return Ok(None);
            }
            let len = u32::from_be_bytes([src[0], src[1], src[2], src[3]]) as usize;
            if len > MAX_FRAME {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "frame too large",
                ));
            }
            if src.len() < 4 + len {
                src.reserve(4 + len - src.len());
                return Ok(None);
            }
            src.advance(4);
            let payload = src.split_to(len);
            let msg = bincode::deserialize(&payload)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Ok(Some(msg))
        }
    }
}

// ── CLI ────────────────────────────────────────────────────────────────────

/// Raft KV load tester — measures throughput and latency.
#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    /// Comma-separated cluster node addresses.
    #[arg(long, default_value = "127.0.0.1:7001,127.0.0.1:7002,127.0.0.1:7003")]
    cluster: String,

    /// Number of concurrent clients.
    #[arg(long, short = 'c', default_value_t = 10)]
    concurrency: usize,

    /// How long to run the benchmark (seconds).
    #[arg(long, short = 'd', default_value_t = 10)]
    duration: u64,

    /// Operation mix.
    #[arg(long, value_enum, default_value_t = Workload::Write)]
    workload: Workload,
}

#[derive(ValueEnum, Clone, Debug)]
enum Workload {
    /// 100 % PUT operations (write-heavy).
    Write,
    /// 100 % GET operations (read-heavy, exercises read-index protocol).
    Read,
    /// 50 % PUT + 50 % GET.
    Mixed,
}

// ── Benchmark driver ───────────────────────────────────────────────────────

/// Shared state across all client tasks.
struct State {
    done: AtomicBool,
    total_ops: AtomicU64,
    errors: AtomicU64,
    /// Raw latencies in microseconds (μs), collected from all clients.
    latencies_us: Mutex<Vec<u64>>,
}

impl State {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            done: AtomicBool::new(false),
            total_ops: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            latencies_us: Mutex::new(Vec::with_capacity(100_000)),
        })
    }
}

/// A single benchmark client that reconnects and follows leader redirects.
struct BenchClient {
    addrs: Vec<SocketAddr>,
    node_ids: Vec<NodeId>,
    current_addr: SocketAddr,
    framed: Option<Framed<TcpStream, codec::RaftCodec>>,
    next_id: u64,
}

impl BenchClient {
    async fn new(addrs: Vec<SocketAddr>) -> Result<Self> {
        let node_ids: Vec<NodeId> = (1..=addrs.len() as u64).collect();
        let current_addr = addrs[0];
        let mut client = Self {
            addrs,
            node_ids,
            current_addr,
            framed: None,
            next_id: 1,
        };
        client.connect().await?;
        Ok(client)
    }

    async fn connect(&mut self) -> Result<()> {
        let stream = TcpStream::connect(self.current_addr)
            .await
            .with_context(|| format!("connect to {}", self.current_addr))?;
        self.framed = Some(Framed::new(stream, codec::RaftCodec));
        Ok(())
    }

    /// Send one operation, following leader redirects automatically.
    ///
    /// Retries on the hinted leader up to 5 times before giving up.
    /// Returns the total round-trip duration including any redirect latency.
    async fn send_once(&mut self, op: ClientOperation) -> Result<Duration> {
        let t0 = Instant::now();

        for _ in 0..5 {
            let req_id = self.next_id;
            self.next_id += 1;

            let framed = self.framed.as_mut().context("not connected")?;
            framed
                .send(RaftMessage::ClientRequest(ClientRequest {
                    id: req_id,
                    operation: op.clone(),
                }))
                .await
                .context("send")?;

            let msg = time::timeout(Duration::from_secs(5), framed.next())
                .await
                .context("response timeout")?
                .context("connection closed")?
                .context("decode error")?;

            if let RaftMessage::ClientResponse(resp) = msg {
                match resp.result {
                    ClientResult::NotLeader { leader_hint } => {
                        // Reconnect to the hinted leader and retry immediately
                        // without returning to the caller — this prevents the
                        // worker error handler from resetting to a non-leader.
                        self.redirect(leader_hint).await?;
                        continue;
                    }
                    ClientResult::Error(e) => bail!("server error: {e}"),
                    ClientResult::Ok(_) => return Ok(t0.elapsed()),
                }
            }

            bail!("unexpected response type");
        }

        bail!("too many leader redirects");
    }

    async fn redirect(&mut self, hint: Option<NodeId>) -> Result<()> {
        // Try the hinted leader first, then fall back to all other addresses.
        let mut candidates: Vec<SocketAddr> = if let Some(id) = hint {
            if let Some(idx) = self.node_ids.iter().position(|&n| n == id) {
                let mut v = self.addrs.clone();
                v.swap(0, idx);
                v
            } else {
                self.addrs.clone()
            }
        } else {
            self.addrs.clone()
        };

        // Don't retry the same node that rejected us.
        candidates.retain(|&a| a != self.current_addr);

        for addr in candidates {
            self.current_addr = addr;
            if self.connect().await.is_ok() {
                return Ok(());
            }
        }
        bail!("could not find leader after redirect");
    }
}

// ── Worker task ────────────────────────────────────────────────────────────

async fn worker(id: usize, addrs: Vec<SocketAddr>, workload: Workload, state: Arc<State>) {
    let mut client = match BenchClient::new(addrs.clone()).await {
        Ok(c) => c,
        Err(e) => {
            warn!("worker {id}: initial connect failed: {e}");
            return;
        }
    };

    let mut op_count: u64 = 0;
    let mut local_latencies: Vec<u64> = Vec::with_capacity(10_000);

    while !state.done.load(Ordering::Relaxed) {
        let op = match workload {
            Workload::Write => ClientOperation::Put {
                key: format!("bench-{id}-{op_count}"),
                value: op_count.to_string(),
            },
            Workload::Read => ClientOperation::Get {
                key: format!("bench-{id}-{op_count}"),
            },
            Workload::Mixed => {
                if op_count.is_multiple_of(2) {
                    ClientOperation::Put {
                        key: format!("bench-{id}-{}", op_count / 2),
                        value: (op_count / 2).to_string(),
                    }
                } else {
                    ClientOperation::Get {
                        key: format!("bench-{id}-{}", op_count / 2),
                    }
                }
            }
        };

        match client.send_once(op).await {
            Ok(lat) => {
                local_latencies.push(lat.as_micros() as u64);
                op_count += 1;
                state.total_ops.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                state.errors.fetch_add(1, Ordering::Relaxed);
                warn!("worker {id}: {e}");
                // Re-connect on errors.
                for addr in &addrs {
                    client.current_addr = *addr;
                    if client.connect().await.is_ok() {
                        break;
                    }
                }
                // Brief back-off to avoid tight error loops.
                time::sleep(Duration::from_millis(50)).await;
            }
        }
    }

    // Flush local latencies into shared state.
    state.latencies_us.lock().await.extend(local_latencies);
}

// ── Report ─────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn print_report(
    addrs: &[SocketAddr],
    concurrency: usize,
    duration_secs: u64,
    workload: &Workload,
    wall: Duration,
    total_ops: u64,
    errors: u64,
    mut latencies: Vec<u64>,
) {
    latencies.sort_unstable();

    let n = latencies.len();
    let ops_per_sec = total_ops as f64 / wall.as_secs_f64();

    let pct = |p: f64| -> f64 {
        if n == 0 {
            return 0.0;
        }
        let idx = ((p / 100.0) * n as f64) as usize;
        latencies[idx.min(n - 1)] as f64 / 1000.0 // μs → ms
    };

    let avg_ms = if n == 0 {
        0.0
    } else {
        latencies.iter().sum::<u64>() as f64 / n as f64 / 1000.0
    };

    let cluster_str = addrs
        .iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join(", ");

    println!();
    println!("╔══════════════════════════════════════════╗");
    println!("║          Raft KV  —  Benchmark           ║");
    println!("╚══════════════════════════════════════════╝");
    println!();
    println!("  Cluster      {cluster_str}");
    println!("  Concurrency  {concurrency} clients");
    println!("  Duration     {duration_secs}s  (actual: {wall:.2?})");
    println!("  Workload     {workload:?}");
    println!();
    println!("  ── Results ────────────────────────────");
    println!("  Total ops    {total_ops}");
    println!("  Errors       {errors}");
    println!("  Throughput   {ops_per_sec:.1} ops/s");
    println!();
    println!("  ── Latency (end-to-end round trip) ────");
    println!("  avg          {avg_ms:.2} ms");
    println!("  p50          {:.2} ms", pct(50.0));
    println!("  p90          {:.2} ms", pct(90.0));
    println!("  p95          {:.2} ms", pct(95.0));
    println!("  p99          {:.2} ms", pct(99.0));
    println!("  max          {:.2} ms", pct(100.0));
    println!();
}

// ── Entry point ────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let args = Args::parse();

    let addrs: Vec<SocketAddr> = args
        .cluster
        .split(',')
        .map(|s| {
            s.trim()
                .parse()
                .with_context(|| format!("invalid addr: {s}"))
        })
        .collect::<Result<_>>()?;

    println!("Connecting to cluster ({} nodes) …", addrs.len());
    // Verify at least one node is reachable.
    TcpStream::connect(addrs[0])
        .await
        .context("cannot reach first cluster node — is the cluster running?")?;
    println!(
        "OK. Starting {}-client benchmark for {}s …\n",
        args.concurrency, args.duration
    );

    let state = State::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(args.duration);

    // Spawn worker tasks.
    let mut tasks = Vec::with_capacity(args.concurrency);
    for id in 0..args.concurrency {
        let addrs = addrs.clone();
        let workload = args.workload.clone();
        let state = Arc::clone(&state);
        tasks.push(tokio::spawn(worker(id, addrs, workload, state)));
    }

    // Run until deadline, then signal workers to stop.
    let wall_start = Instant::now();
    time::sleep_until(deadline).await;
    state.done.store(true, Ordering::Relaxed);

    // Wait for all workers.
    for t in tasks {
        let _ = t.await;
    }
    let wall = wall_start.elapsed();

    let total_ops = state.total_ops.load(Ordering::Relaxed);
    let errors = state.errors.load(Ordering::Relaxed);
    let latencies = state.latencies_us.lock().await.clone();

    print_report(
        &addrs,
        args.concurrency,
        args.duration,
        &args.workload,
        wall,
        total_ops,
        errors,
        latencies,
    );

    Ok(())
}
