# raft-kv

A distributed key-value store built in Rust using the [Raft consensus protocol](https://raft.github.io/raft.pdf).

Built as a portfolio project demonstrating distributed systems fundamentals: leader election, log replication, linearizable reads, and fault tolerance.

## Architecture

```
                     +-----------+
                     | CLI Client|
                     +-----+-----+
                           | TCP
              +------------+------------+
              |            |            |
         +----v----+  +----v----+  +----v----+
         | Node 1  |  | Node 2  |  | Node 3  |
         | (Leader)|  |(Follower)| |(Follower)|
         +----+----+  +----+----+  +----+----+
              |            |            |
              +------Raft RPCs----------+
```

Each node runs three concurrent subsystems:
- **Raft Engine** — pure state machine for leader election and log replication
- **Network Transport** — TCP connections to peers with automatic reconnection
- **KV State Machine** — applies committed log entries to an in-memory store

## Workspace layout

```
crates/
  raft-core/     # Pure Raft logic — zero I/O, zero async
    src/
      config.rs      # RaftConfig (timeouts, cluster size)
      message.rs     # All RPC types + wire envelope
      log.rs         # Immutable RaftLog
      state.rs       # Role, PersistentState, Actions command object
      node.rs        # RaftNode state machine (all transitions → Actions)

  raft-server/   # Async TCP node binary
    src/
      codec.rs       # Length-delimited bincode framing
      transport.rs   # TCP acceptor + sender with reconnection
      kv.rs          # Immutable KvStore state machine
      storage.rs     # Crash-safe fsync persistence
      node.rs        # tokio::select! event loop
      main.rs        # CLI: --id, --addr, --peers

  raft-client/   # CLI client binary
    src/
      codec.rs       # Same wire format as server
      connection.rs  # Auto-reconnect + leader-redirect following
      main.rs        # get / put / delete subcommands
```

## Key design decisions

### Pure core, I/O-free
`raft-core` has no async runtime, no networking, and no file I/O. Every method on `RaftNode` takes `self` by value and returns `(RaftNode, Actions)`. The `Actions` struct is a command object listing side-effects (messages to send, entries to apply, state to persist) that the server layer executes.

This makes the entire Raft logic unit-testable without any mocking.

### Immutability throughout
`RaftLog::append` and `RaftLog::truncate_from` return new instances. `KvStore::apply` returns a new store. `RaftNode` transitions return new node state. No mutation in-place.

### Commitment rule (§5.4.2)
The leader only advances `commit_index` for entries whose `term == current_term`. This prevents the "Figure 8" problem where a leader could incorrectly commit entries from a previous term.

### Crash-safe persistence
Before responding to any RPC, the server writes durable state (`current_term`, `voted_for`, log) to disk using an atomic `write → fsync → rename` sequence.

### Read-index protocol
GET requests record `read_index = commit_index` and trigger a heartbeat round to confirm the node is still the active leader before serving the read. This prevents stale reads from a deposed leader.

## Raft protocol coverage

```mermaid
graph LR
    subgraph done["✅ Implemented"]
        A[Leader election §5.2]
        B[Log replication §5.3]
        C[Commitment rule §5.4.2]
        D[Log conflict resolution\nwith hints]
        E[Noop entry on\nnew leader §8]
        F[Crash recovery\npersistent state]
        G[Read-index\nlinearizable reads]
        H[Randomised\nelection timeouts]
        I[Heartbeat suppression\nof elections]
    end

    subgraph future["❌ Future work"]
        J[Log compaction\n/ snapshots]
        K[Dynamic membership\nchanges]
    end

    style done fill:#1a3a1a,stroke:#2d6a2d,color:#90ee90
    style future fill:#3a1a1a,stroke:#6a2d2d,color:#ffaaaa
```

## Current status

The Raft core and KV state machine are complete and tested. The server and client compile to release binaries.

**What works:**
- Pure Raft state machine (election, replication, commitment)
- Immutable KV state machine
- Length-delimited bincode codec
- Async TCP transport with reconnection
- Crash-safe fsync storage
- CLI argument parsing for server and client

**Known gaps (next milestone):**
- Server does not yet expose a client-facing TCP listener — the `NodeHandle` exists but is not wired to an accept loop
- Single-node clusters do not self-elect (no special case for zero peers)
- No end-to-end integration test across multiple live nodes
- `unsafe_placeholder()` in `node.rs` should be replaced with `Option<RaftNode>`

## Tests

```
cargo test --workspace
```

23 tests, all passing:
- 7 log unit tests (append, truncate, query, immutability)
- 10 Raft node unit tests (election, replication, step-down)
- 6 KV state machine unit tests

## Usage (once wiring is complete)

```bash
# Start a 3-node cluster
cargo run --bin raft-server -- --id 1 --addr 127.0.0.1:7001 --peers 2=127.0.0.1:7002,3=127.0.0.1:7003
cargo run --bin raft-server -- --id 2 --addr 127.0.0.1:7002 --peers 1=127.0.0.1:7001,3=127.0.0.1:7003
cargo run --bin raft-server -- --id 3 --addr 127.0.0.1:7003 --peers 1=127.0.0.1:7001,2=127.0.0.1:7002

# Use the CLI client
cargo run --bin raft-client -- put foo bar
cargo run --bin raft-client -- get foo
cargo run --bin raft-client -- delete foo
```

## References

- [In Search of an Understandable Consensus Algorithm (Raft paper)](https://raft.github.io/raft.pdf)
- [The Secret Lives of Data — Raft visualisation](http://thesecretlivesofdata.com/raft/)
