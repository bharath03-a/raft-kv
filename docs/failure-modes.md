# Failure Mode Analysis

This document describes how the raft-kv cluster behaves under each class of
failure. Each section states what happens, what the safety guarantees are, and
what a client or operator should expect.

---

## 1. Leader crash

**What happens**

The leader process terminates without sending a shutdown signal. Followers stop
receiving heartbeats. After each follower's election timeout fires
(150–300 ms, randomised), it converts to a candidate and broadcasts
`VoteRequest` messages. The first candidate to collect a majority wins the
election and becomes the new leader. The new leader then sends a no-op log
entry to commit any entries from previous terms that are safe to commit.

**Safety guarantee**

No committed entry is lost. An entry is committed only after a majority of
nodes have written it to durable storage. A new leader is elected only by a
node whose log is at least as up-to-date as any majority member (the
"election restriction", Raft §5.4.1), so it already holds all committed entries.

**In-flight writes**

Writes that the crashed leader accepted but did not yet commit are lost from
the client's perspective. The client's TCP connection drops immediately, so the
client receives a connection error. It should retry. If the write was not
committed before the crash, retrying is safe — at most one copy will ever be
committed.

**Client impact**

Approximately 300–700 ms of unavailability (one election round plus safety
margin). After re-election, the cluster is fully available. The
`three_node_leader_failover` integration test exercises this path.

**Configuration lever**

`election_timeout_min_ms` and `election_timeout_max_ms` in `RaftConfig`
control how quickly a new election begins. Lower values reduce recovery time
but increase the risk of spurious elections on a slow-but-live leader.

---

## 2. Follower crash

**What happens**

A follower process terminates. The leader continues replicating to the
remaining majority. Log entries keep committing normally — the crashed
follower's absence does not prevent commit as long as at least one other
follower is reachable.

When the follower restarts, it recovers its durable state (current_term,
voted_for, log) from disk. It begins as a Follower and waits for the leader's
next heartbeat or AppendEntries. The leader detects the follower's log is
behind (`match_index` was reset) and sends catch-up AppendEntries to bring it
up to the current commit index.

**Safety guarantee**

No data loss. The cluster continues to accept writes without interruption.

**Client impact**

None, provided the cluster is 3+ nodes. A 2-node cluster loses write
availability (cannot form a majority of 2 out of 2 remaining nodes).

**Replication lag**

After restart, the recovering follower may be seconds behind depending on how
many entries accumulated during the crash. All missed entries are replicated
before the follower's log is considered current.

---

## 3. Network partition — minority partitioned

**What happens**

A network split separates one node from the other two (e.g. node 3 loses
connectivity to nodes 1 and 2).

Node 3 will not receive heartbeats. Its election timer fires. It increments its
term and broadcasts `VoteRequest`. Because it cannot reach a majority, its
votes are never acknowledged. It increments its term again on each timeout
cycle. Terms can grow large during a long partition.

Nodes 1 and 2 form a majority and continue to elect a leader and serve clients
normally.

**When the partition heals**

Node 3 reconnects with a higher term than the current leader. The leader and
all followers see the higher term in the first message from node 3 and step
down to Follower. A new election begins. Once complete (one round, typically
<300 ms), the cluster is fully functional again. Node 3's log is brought
up-to-date via catch-up replication.

**Safety guarantee**

No committed entry is lost. The majority partition continued committing entries
while isolated; those entries remain committed. The previously-partitioned node
cannot have committed any entries while isolated (no majority).

**Client impact**

Clients connected to node 3 during the partition receive `NotLeader` responses
and are redirected to the leader on the majority side. After partition heals,
one brief re-election round occurs.

---

## 4. Network partition — even split (split-brain scenario)

**What happens for a 3-node cluster**

A true even split requires at least 4 nodes. For a 3-node cluster, any split
is either 2+1 (covered above) or 0+3 (no partition).

For a 4-node cluster, a 2+2 split means neither side has a majority (3/4 = 2).
Neither side can commit new entries. Both sides may hold elections but will
fail to win. The cluster stalls.

**Safety guarantee**

No data is corrupted. No phantom commits occur. Two leaders cannot exist in the
same term because neither can collect a majority of votes.

**Client impact**

All writes time out. Reads return `NotLeader` (no leader can be elected).
Availability is lost until connectivity is restored.

**Resolution**

Once connectivity is restored, one side will win the election. All nodes
converge to the same log. Any client writes that timed out must be retried.

---

## 5. Slow disk — fsync latency

**What happens**

In durable mode, the leader fsyncs its log to disk before sending AppendEntries
to followers (Raft §5.4.1). Each follower also fsyncs before responding.
If the OS is under I/O pressure (`F_FULLFSYNC` on macOS, `fsync` on Linux),
this can take 10–50 ms per entry.

Because the leader's event loop awaits the fsync synchronously, subsequent
client requests queue behind the in-progress persist. Under high concurrency,
write latency scales linearly with queue depth.

**Safety guarantee**

Full durability. An acknowledged write has been written to stable storage on a
majority of nodes.

**Observable symptoms**

`raft_pending_writes` (visible in Prometheus metrics) grows. The leader's
`raft_commit_index` advances slowly relative to `raft_log_length`.

**Mitigation**

Use `--no-sync` for workloads that do not require crash durability (benchmarks,
caches). In production, use NVMe storage or a battery-backed write cache.
The long-term fix is write batching (group commit): buffer multiple proposals
into a single fsync, then commit them all atomically.

---

## 6. Message loss

**What happens**

UDP-style message loss is uncommon on TCP, but TCP connections can drop and
require reconnection. The `send_loop` in `transport.rs` maintains one
persistent TCP connection per peer and reconnects with a short back-off on
failure. Messages that cannot be delivered within one reconnect cycle are
dropped silently.

Raft is explicitly designed to tolerate message loss. The leader retries
AppendEntries on the next heartbeat tick (every 50 ms by default). Vote
requests are retried after each election timeout.

**Safety guarantee**

Progress (liveness) is only guaranteed eventually, not within a bounded time.
Safety (no incorrect commits) is always maintained regardless of how many
messages are dropped.

**Chaos test**

The `cluster_converges_with_20_percent_message_drops` integration test
injects 20 % inbound and outbound drop rates into all three nodes and asserts
that 20 writes commit correctly.

---

## 7. Stale read from a deposed leader

**What happens**

A node believes it is still the leader (no election has occurred from its
perspective) but a network partition has caused the majority to elect a new
leader. If the node serves a read directly from its local KV state, it may
return stale data.

**How this implementation prevents it**

GET requests use the read-index protocol: the leader records
`read_index = commit_index` and triggers a heartbeat round. The read is served
only after `last_applied >= read_index` and after the heartbeat confirms the
node is still the active leader.

If the heartbeat receives no majority acknowledgment (e.g. the node is
partitioned), `pending_reads` grows and reads are never served. The client
eventually times out. This is safe: a timeout is preferable to a stale read.

---

## 8. Data loss scenarios

**Under durable mode (default)**

Data loss requires a majority of nodes to crash simultaneously with unflushed
data, which is not possible if `sync_all()` completes before acknowledging each
write. Raft safety holds: at least one node in any majority has the committed
entry durably stored.

**Under --no-sync mode**

Any node crash before the OS flushes its page cache to disk can lose entries
that were acknowledged to the client. Simultaneous crashes of any nodes make
this worse. Use `--no-sync` only for workloads that can tolerate data loss
(benchmarks, ephemeral caches).

**Log growth (current limitation)**

This implementation does not yet implement log compaction (snapshots). A
long-running cluster accumulates unbounded log entries. Eventually, disk
space is exhausted and recovery from disk becomes slow. This is the highest-
priority missing feature for production use.

---

## Summary table

| Failure | Availability impact | Safety | Recovery time |
|---|---|---|---|
| Leader crash | ~300–700 ms | No data loss | One election round |
| Follower crash (n=3) | None | No data loss | None for clients |
| Minority partition | None on majority side | No data loss | One election after heal |
| Even split (n=4+) | Full stall | No corruption | One election after heal |
| Slow disk (durable) | Increased latency | Full durability | Clears when I/O clears |
| 20% message loss | Mild latency increase | No data loss | Continuous |
| Deposed-leader read | Reads time out | No stale reads | Client retries |
| Simultaneous majority crash (--no-sync) | Potential data loss | Log may be truncated | Manual intervention |
