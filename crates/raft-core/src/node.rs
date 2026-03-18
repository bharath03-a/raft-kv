use crate::{
    config::RaftConfig,
    log::RaftLog,
    message::{
        AppendEntriesRequest, AppendEntriesResponse, ClientRequest, ClientResponse, ClientResult,
        Command, LogEntry, NodeId, RaftMessage, VoteRequest, VoteResponse,
    },
    state::{Actions, LeaderState, PersistentState, Role, VolatileState},
};

/// The central Raft state machine.
///
/// All methods are **pure**: they take `self` by value and return
/// `(RaftNode, Actions)`. The caller (the async network layer in
/// `raft-server`) is responsible for executing the `Actions`
/// (sending messages, persisting state, resetting timers, etc.).
pub struct RaftNode {
    pub id: NodeId,
    pub peers: Vec<NodeId>,
    pub role: Role,
    pub persistent: PersistentState,
    pub volatile: VolatileState,
    pub leader_state: Option<LeaderState>,
    pub config: RaftConfig,
    /// Track current leader for client redirects.
    pub current_leader: Option<NodeId>,
}

impl RaftNode {
    pub fn new(id: NodeId, peers: Vec<NodeId>, config: RaftConfig) -> Self {
        Self {
            id,
            peers,
            role: Role::Follower,
            persistent: PersistentState::new(),
            volatile: VolatileState::new(),
            leader_state: None,
            config,
            current_leader: None,
        }
    }

    // ── Accessors ──────────────────────────────────────────────────────────

    pub fn current_term(&self) -> u64 {
        self.persistent.current_term
    }

    pub fn log(&self) -> &RaftLog {
        &self.persistent.log
    }

    pub fn is_leader(&self) -> bool {
        self.role == Role::Leader
    }

    // ── Election ───────────────────────────────────────────────────────────

    /// Called when the election timeout fires.
    ///
    /// Transitions: Follower → Candidate (or Candidate → new Candidate on
    /// split-vote timeout).
    pub fn election_timeout(self) -> (Self, Actions) {
        // Leaders never time out for elections.
        if self.role == Role::Leader {
            return (self, Actions::default());
        }

        let new_term = self.persistent.current_term + 1;
        let last_log_index = self.persistent.log.last_index();
        let last_log_term = self.persistent.log.last_term();

        let persistent = PersistentState {
            current_term: new_term,
            voted_for: Some(self.id), // vote for self
            log: self.persistent.log.clone(),
        };

        let candidate = Self {
            role: Role::Candidate,
            persistent: persistent.clone(),
            current_leader: None,
            ..self
        };

        // Single-node fast path: no peers → already have majority (just self).
        if candidate.peers.is_empty() {
            let leader_state = LeaderState::new(&[], last_log_index);
            return candidate.become_leader(leader_state);
        }

        let vote_requests: Vec<(NodeId, RaftMessage)> = candidate
            .peers
            .iter()
            .map(|&peer| {
                (
                    peer,
                    RaftMessage::VoteRequest(VoteRequest {
                        term: new_term,
                        candidate_id: candidate.id,
                        last_log_index,
                        last_log_term,
                    }),
                )
            })
            .collect();

        let actions = Actions {
            messages: vote_requests,
            persist: Some(persistent),
            reset_election_timer: true,
            ..Actions::default()
        };

        (candidate, actions)
    }

    /// Handle an incoming RequestVote RPC.
    pub fn handle_vote_request(self, req: VoteRequest) -> (Self, Actions) {
        // §5.1: if we see a higher term, revert to follower first.
        let (mut node, mut actions) = if req.term > self.persistent.current_term {
            self.step_down(req.term)
        } else {
            (self, Actions::default())
        };

        let grant = node.can_grant_vote(&req);

        if grant {
            let persistent = PersistentState {
                current_term: req.term,
                voted_for: Some(req.candidate_id),
                log: node.persistent.log.clone(),
            };
            actions.persist = Some(persistent.clone());
            actions.reset_election_timer = true;
            node.persistent = persistent;
        }

        let response = RaftMessage::VoteResponse(VoteResponse {
            term: node.persistent.current_term,
            vote_granted: grant,
            peer_id: node.id,
        });
        actions.messages.push((req.candidate_id, response));

        (node, actions)
    }

    /// Handle a VoteResponse from a peer.
    pub fn handle_vote_response(
        self,
        from: NodeId,
        resp: VoteResponse,
    ) -> (Self, Actions) {
        // Stale term — step down and ignore.
        if resp.term > self.persistent.current_term {
            return self.step_down(resp.term);
        }

        // Only a Candidate processes vote responses.
        if self.role != Role::Candidate {
            return (self, Actions::default());
        }

        // Stale response for an old term.
        if resp.term < self.persistent.current_term {
            return (self, Actions::default());
        }

        if !resp.vote_granted {
            return (self, Actions::default());
        }

        // Count votes: self + all granted peers we've seen.
        // We track this by inspecting voted_for across peers — for simplicity
        // we store granted votes as a running total in leader_state.
        // Here we collect granted_votes by examining what we know.
        // Since we transition states atomically, we keep a vote counter
        // embedded in the candidate's leader_state (reused as vote tally).
        let votes_needed = self.config.majority();
        let mut vote_count = 1; // always vote for self

        // Accumulate: we use a simple heuristic — each unique "from" that
        // sent a granted response contributes one vote. We track this by
        // reusing LeaderState::match_index as a "voted" set.
        let mut leader_state = self.leader_state.unwrap_or_else(|| {
            LeaderState::new(&self.peers, self.persistent.log.last_index())
        });

        // Use match_index == u64::MAX as a sentinel for "granted".
        leader_state.match_index.insert(from, u64::MAX);
        vote_count += leader_state
            .match_index
            .values()
            .filter(|&&v| v == u64::MAX)
            .count();

        if vote_count >= votes_needed {
            // We won the election — become leader.
            let last_log_index = self.persistent.log.last_index();
            let new_leader_state =
                LeaderState::new(&self.peers, last_log_index);
            let (node, actions) = Self {
                role: Role::Candidate,
                leader_state: Some(leader_state),
                ..self
            }
            .become_leader(new_leader_state);
            return (node, actions);
        }

        let node = Self {
            leader_state: Some(leader_state),
            ..self
        };
        (node, Actions::default())
    }

    // ── Log replication ────────────────────────────────────────────────────

    /// Called by the leader on every heartbeat tick.
    ///
    /// Sends an AppendEntries to every peer. When `entries` is empty this
    /// acts as a heartbeat.
    pub fn tick(self) -> (Self, Actions) {
        if self.role != Role::Leader {
            return (self, Actions::default());
        }

        let messages = self.build_append_entries_for_all_peers();

        // Single-node: no peers will ever respond, so advance commit on every tick.
        if self.peers.is_empty() {
            let (node, commit) = self.try_advance_commit();
            return (node, Actions {
                messages,
                entries_to_apply: commit.entries_to_apply,
                ..Actions::default()
            });
        }

        (self, Actions { messages, ..Actions::default() })
    }

    /// Handle an incoming AppendEntries RPC (follower/candidate side).
    pub fn handle_append_entries(
        self,
        req: AppendEntriesRequest,
    ) -> (Self, Actions) {
        // §5.1: higher term → step down.
        let (mut node, mut actions) = if req.term > self.persistent.current_term {
            self.step_down(req.term)
        } else {
            (self, Actions::default())
        };

        // Reject if term < current_term.
        if req.term < node.persistent.current_term {
            let resp = RaftMessage::AppendEntriesResponse(AppendEntriesResponse {
                term: node.persistent.current_term,
                success: false,
                conflict_index: None,
                conflict_term: None,
                peer_id: node.id,
            });
            actions.messages.push((req.leader_id, resp));
            return (node, actions);
        }

        // Valid AppendEntries from the current leader — reset election timer.
        actions.reset_election_timer = true;
        node.current_leader = Some(req.leader_id);

        // Log consistency check (§5.3): prev entry must match.
        if !node.log_matches(req.prev_log_index, req.prev_log_term) {
            let (conflict_index, conflict_term) =
                node.find_conflict_hint(req.prev_log_index);
            let resp = RaftMessage::AppendEntriesResponse(AppendEntriesResponse {
                term: node.persistent.current_term,
                success: false,
                conflict_index: Some(conflict_index),
                conflict_term,
                peer_id: node.id,
            });
            actions.messages.push((req.leader_id, resp));
            return (node, actions);
        }

        // Append new entries, truncating any conflicting tail (§5.3).
        let mut log = node.persistent.log.clone();
        for entry in &req.entries {
            if let Some(existing) = log.get(entry.index)
                && existing.term != entry.term
            {
                log = log.truncate_from(entry.index);
            }
            if log.get(entry.index).is_none() {
                log = log.append(entry.clone());
            }
        }

        // Advance commit index.
        let new_commit = req.leader_commit.min(log.last_index());
        let old_commit = node.volatile.commit_index;

        let entries_to_apply: Vec<LogEntry> = if new_commit > old_commit {
            (old_commit + 1..=new_commit)
                .filter_map(|i| log.get(i).cloned())
                .collect()
        } else {
            vec![]
        };

        let persistent = PersistentState {
            current_term: node.persistent.current_term,
            voted_for: node.persistent.voted_for,
            log,
        };

        let resp = RaftMessage::AppendEntriesResponse(AppendEntriesResponse {
            term: node.persistent.current_term,
            success: true,
            conflict_index: None,
            conflict_term: None,
            peer_id: node.id,
        });

        actions.messages.push((req.leader_id, resp));
        actions.entries_to_apply = entries_to_apply;
        actions.persist = Some(persistent.clone());

        node.persistent = persistent;
        node.volatile.commit_index = new_commit;

        (node, actions)
    }

    /// Handle an AppendEntries response (leader side).
    pub fn handle_append_entries_response(
        mut self,
        from: NodeId,
        resp: AppendEntriesResponse,
    ) -> (Self, Actions) {
        if resp.term > self.persistent.current_term {
            return self.step_down(resp.term);
        }

        if self.role != Role::Leader {
            return (self, Actions::default());
        }

        let leader_state = match self.leader_state.as_mut() {
            Some(ls) => ls,
            None => return (self, Actions::default()),
        };

        if resp.success {
            // Update match_index to the highest entry we just sent to this peer.
            let sent_up_to = leader_state.next_index[&from].saturating_sub(1);
            let match_entry = leader_state.match_index.entry(from).or_insert(0);
            if sent_up_to > *match_entry {
                *match_entry = sent_up_to;
            }
            leader_state.next_index.insert(from, sent_up_to + 1);
        } else {
            // Fast backtracking using conflict hints (Raft leader §5.3 optimisation).
            let next = if let (Some(ci), Some(ct)) =
                (resp.conflict_index, resp.conflict_term)
            {
                // Find the last entry in our log with conflict_term.
                let our_last_in_term = (1..=self.persistent.log.last_index())
                    .rev()
                    .find(|&i| self.persistent.log.term_at(i) == ct);
                match our_last_in_term {
                    Some(idx) => idx + 1,
                    None => ci, // we don't have that term at all
                }
            } else {
                // No hint — fall back to decrement by one.
                leader_state.next_index[&from].saturating_sub(1).max(1)
            };

            leader_state.next_index.insert(from, next);
        }

        // Try to advance the commit index (§5.3, §5.4).
        let (node, actions) = self.try_advance_commit();
        (node, actions)
    }

    /// Handle a client request.
    ///
    /// If this node is not the leader, returns a redirect immediately.
    /// If it is the leader, appends the command to the log and replicates.
    pub fn handle_client_request(self, req: ClientRequest) -> (Self, Actions) {
        if self.role != Role::Leader {
            let response = ClientResponse {
                id: req.id,
                result: ClientResult::NotLeader {
                    leader_hint: self.current_leader,
                },
            };
            let actions = Actions {
                client_responses: vec![response],
                ..Actions::default()
            };
            return (self, actions);
        }

        match req.operation {
            crate::message::ClientOperation::Get { .. } => {
                // Read-only path: the server layer handles the read-index protocol.
                // We send a heartbeat round to confirm we are still the active leader,
                // then the server applies the read once last_applied >= read_index.
                let actions = Actions {
                    messages: self.build_append_entries_for_all_peers(),
                    ..Actions::default()
                };
                (self, actions)
            }
            crate::message::ClientOperation::Put { key, value } => {
                let command = Command::Put { key, value };
                self.append_and_replicate(req.id, command)
            }
            crate::message::ClientOperation::Delete { key } => {
                let command = Command::Delete { key };
                self.append_and_replicate(req.id, command)
            }
        }
    }

    // ── Private helpers ────────────────────────────────────────────────────

    fn can_grant_vote(&self, req: &VoteRequest) -> bool {
        // Wrong term — already handled by step_down above, but double-check.
        if req.term < self.persistent.current_term {
            return false;
        }
        // Already voted for someone else this term.
        match self.persistent.voted_for {
            Some(id) if id != req.candidate_id => return false,
            _ => {}
        }
        // Candidate's log must be at least as up-to-date (§5.4.1).
        let our_last_term = self.persistent.log.last_term();
        let our_last_index = self.persistent.log.last_index();
        if req.last_log_term > our_last_term {
            return true;
        }
        if req.last_log_term == our_last_term && req.last_log_index >= our_last_index {
            return true;
        }
        false
    }

    fn log_matches(&self, prev_index: u64, prev_term: u64) -> bool {
        if prev_index == 0 {
            return true; // beginning of log always matches
        }
        self.persistent.log.term_at(prev_index) == prev_term
    }

    fn find_conflict_hint(&self, prev_index: u64) -> (u64, Option<u64>) {
        // Walk back to find the first entry of the conflicting term.
        let conflict_term = self.persistent.log.term_at(prev_index);
        if conflict_term == 0 {
            // prev_index beyond our log end.
            return (self.persistent.log.last_index() + 1, None);
        }
        let conflict_index = (1..=prev_index)
            .rev()
            .find(|&i| self.persistent.log.term_at(i) != conflict_term)
            .map_or(1, |i| i + 1);
        (conflict_index, Some(conflict_term))
    }

    fn build_append_entries_for_peer(&self, peer: NodeId) -> RaftMessage {
        let leader_state = self.leader_state.as_ref().expect("must be leader");
        let next_idx = leader_state.next_index[&peer];
        let prev_log_index = next_idx.saturating_sub(1);
        let prev_log_term = self.persistent.log.term_at(prev_log_index);
        let entries = self
            .persistent
            .log
            .entries_after(prev_log_index)
            .to_vec();

        RaftMessage::AppendEntriesRequest(AppendEntriesRequest {
            term: self.persistent.current_term,
            leader_id: self.id,
            prev_log_index,
            prev_log_term,
            entries,
            leader_commit: self.volatile.commit_index,
        })
    }

    fn build_append_entries_for_all_peers(&self) -> Vec<(NodeId, RaftMessage)> {
        self.peers
            .iter()
            .map(|&peer| (peer, self.build_append_entries_for_peer(peer)))
            .collect()
    }

    fn become_leader(mut self, leader_state: LeaderState) -> (Self, Actions) {
        self.role = Role::Leader;
        self.current_leader = Some(self.id);
        self.leader_state = Some(leader_state);

        // Append a Noop entry to commit any outstanding entries from previous
        // terms and establish the new leader's commit index (Raft §8).
        let noop_index = self.persistent.log.last_index() + 1;
        let noop = LogEntry {
            term: self.persistent.current_term,
            index: noop_index,
            command: Command::Noop,
        };
        let log = self.persistent.log.append(noop);
        let persistent = PersistentState {
            current_term: self.persistent.current_term,
            voted_for: self.persistent.voted_for,
            log,
        };

        let messages = self.build_append_entries_for_all_peers();

        self.persistent = persistent.clone();

        let actions = Actions {
            messages,
            persist: Some(persistent),
            ..Actions::default()
        };

        (self, actions)
    }

    /// Attempt to advance the leader's commit index.
    ///
    /// The new commit index is the highest index `n` such that:
    ///   1. n > commit_index
    ///   2. A majority of nodes have match_index >= n
    ///   3. log[n].term == current_term   (§5.4.2 safety — critical!)
    fn try_advance_commit(mut self) -> (Self, Actions) {
        let leader_state = match self.leader_state.as_ref() {
            Some(ls) => ls,
            None => return (self, Actions::default()),
        };

        let last_index = self.persistent.log.last_index();
        let commit_index = self.volatile.commit_index;
        let current_term = self.persistent.current_term;
        let majority = self.config.majority();

        // Find the highest n we can commit.
        let mut new_commit = commit_index;
        for n in (commit_index + 1)..=last_index {
            if self.persistent.log.term_at(n) != current_term {
                continue; // §5.4.2: only commit entries from current term
            }
            let replicated = 1 + leader_state // 1 = the leader itself
                .match_index
                .values()
                .filter(|&&m| m >= n)
                .count();
            if replicated >= majority {
                new_commit = n;
            }
        }

        if new_commit <= commit_index {
            return (self, Actions::default());
        }

        let entries_to_apply: Vec<LogEntry> = (commit_index + 1..=new_commit)
            .filter_map(|i| self.persistent.log.get(i).cloned())
            .collect();

        self.volatile.commit_index = new_commit;

        let actions = Actions {
            entries_to_apply,
            ..Actions::default()
        };

        (self, actions)
    }

    fn append_and_replicate(mut self, req_id: u64, command: Command) -> (Self, Actions) {
        let new_index = self.persistent.log.last_index() + 1;
        let entry = LogEntry {
            term: self.persistent.current_term,
            index: new_index,
            command,
        };

        let log = self.persistent.log.append(entry);
        let persistent = PersistentState {
            current_term: self.persistent.current_term,
            voted_for: self.persistent.voted_for,
            log,
        };

        // Update leader's own match_index.
        if let Some(ls) = self.leader_state.as_mut() {
            ls.match_index.insert(self.id, new_index);
        }

        self.persistent = persistent.clone();

        // Note: we do NOT immediately respond to the client. The response
        // is sent by the server layer once the entry commits (tracked via
        // leader_state.pending keyed by log index).
        let messages = self.build_append_entries_for_all_peers();
        let persist = Some(persistent);
        let _ = req_id; // server layer owns the channel mapping

        // Single-node: no peers will ever ack, so commit the entry immediately.
        if self.peers.is_empty() {
            let (node, commit) = self.try_advance_commit();
            return (node, Actions {
                messages,
                persist,
                entries_to_apply: commit.entries_to_apply,
                ..Actions::default()
            });
        }

        (self, Actions { messages, persist, ..Actions::default() })
    }

    /// Step down to follower when a higher term is observed (§5.1).
    pub fn step_down(mut self, new_term: u64) -> (Self, Actions) {
        let persistent = PersistentState {
            current_term: new_term,
            voted_for: None,
            log: self.persistent.log.clone(),
        };
        self.role = Role::Follower;
        self.leader_state = None;
        self.current_leader = None;
        self.persistent = persistent.clone();

        let actions = Actions {
            persist: Some(persistent),
            reset_election_timer: true,
            ..Actions::default()
        };

        (self, actions)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::RaftConfig, message::Command};

    fn make_node(id: NodeId, peers: Vec<NodeId>) -> RaftNode {
        RaftNode::new(id, peers, RaftConfig::default_local())
    }

    fn single_node() -> RaftNode {
        make_node(1, vec![])
    }

    fn three_node_cluster() -> (RaftNode, RaftNode, RaftNode) {
        (
            make_node(1, vec![2, 3]),
            make_node(2, vec![1, 3]),
            make_node(3, vec![1, 2]),
        )
    }

    // ── Election tests ─────────────────────────────────────────────────────

    #[test]
    fn single_node_election_becomes_leader_immediately() {
        let node = single_node();
        let (leader, actions) = node.election_timeout();
        // Single-node clusters skip the vote round and become leader directly.
        assert_eq!(leader.role, Role::Leader);
        assert_eq!(leader.current_term(), 1);
        // Must persist the Noop entry appended on election.
        assert!(actions.persist.is_some());
    }

    #[test]
    fn election_timeout_increments_term() {
        let (n1, _, _) = three_node_cluster();
        let (candidate, actions) = n1.election_timeout();
        assert_eq!(candidate.current_term(), 1);
        assert_eq!(candidate.role, Role::Candidate);
        assert!(actions.persist.is_some());
        assert!(actions.reset_election_timer);
        // Should send vote requests to both peers.
        assert_eq!(actions.messages.len(), 2);
    }

    #[test]
    fn vote_granted_when_log_is_up_to_date() {
        let (_, n2, _) = three_node_cluster();
        let req = VoteRequest {
            term: 1,
            candidate_id: 1,
            last_log_index: 0,
            last_log_term: 0,
        };
        let (n2_after, actions) = n2.handle_vote_request(req);
        assert_eq!(n2_after.persistent.voted_for, Some(1));
        let granted = actions.messages.iter().any(|(_, m)| {
            matches!(m, RaftMessage::VoteResponse(r) if r.vote_granted)
        });
        assert!(granted);
    }

    #[test]
    fn vote_denied_when_candidate_term_is_stale() {
        let mut node = make_node(2, vec![1, 3]);
        node.persistent.current_term = 5;
        let req = VoteRequest {
            term: 3,
            candidate_id: 1,
            last_log_index: 0,
            last_log_term: 0,
        };
        let (_, actions) = node.handle_vote_request(req);
        let denied = actions.messages.iter().any(|(_, m)| {
            matches!(m, RaftMessage::VoteResponse(r) if !r.vote_granted)
        });
        assert!(denied);
    }

    #[test]
    fn vote_denied_for_second_candidate_same_term() {
        let (_, n2, _) = three_node_cluster();
        let req1 = VoteRequest { term: 1, candidate_id: 1, last_log_index: 0, last_log_term: 0 };
        let (n2, _) = n2.handle_vote_request(req1);
        let req2 = VoteRequest { term: 1, candidate_id: 3, last_log_index: 0, last_log_term: 0 };
        let (_, actions) = n2.handle_vote_request(req2);
        let denied = actions.messages.iter().any(|(_, m)| {
            matches!(m, RaftMessage::VoteResponse(r) if !r.vote_granted)
        });
        assert!(denied);
    }

    #[test]
    fn candidate_becomes_leader_with_majority() {
        let (n1, n2, _n3) = three_node_cluster();
        let (candidate, _) = n1.election_timeout();
        assert_eq!(candidate.role, Role::Candidate);

        let vote_req = VoteRequest {
            term: 1, candidate_id: 1, last_log_index: 0, last_log_term: 0,
        };
        let (_, a2) = n2.handle_vote_request(vote_req.clone());
        let resp_from_2 = a2.messages.iter()
            .find_map(|(_, m)| if let RaftMessage::VoteResponse(r) = m { Some(r.clone()) } else { None })
            .unwrap();
        assert!(resp_from_2.vote_granted);

        // Majority for a 3-node cluster = 2 (self + one peer).
        // Receiving the first granted vote tips us over the majority threshold.
        let (leader, actions) = candidate.handle_vote_response(2, resp_from_2);

        assert_eq!(leader.role, Role::Leader, "should become leader on majority");
        // Leader must immediately broadcast AppendEntries (with Noop entry).
        assert!(
            !actions.messages.is_empty(),
            "leader must send AppendEntries to all peers on election"
        );
        assert_eq!(actions.messages.len(), 2, "one message per peer");
    }

    // ── Replication tests ──────────────────────────────────────────────────

    #[test]
    fn leader_rejects_client_write_when_not_leader() {
        let node = make_node(1, vec![2, 3]);
        let req = ClientRequest {
            id: 1,
            operation: crate::message::ClientOperation::Put {
                key: "k".into(),
                value: "v".into(),
            },
        };
        let (_, actions) = node.handle_client_request(req);
        assert!(actions.client_responses.iter().any(|r| {
            matches!(&r.result, ClientResult::NotLeader { .. })
        }));
    }

    #[test]
    fn follower_appends_entries_and_advances_commit() {
        let node = make_node(2, vec![1, 3]);
        let entries = vec![
            LogEntry { term: 1, index: 1, command: Command::Put { key: "a".into(), value: "1".into() } },
            LogEntry { term: 1, index: 2, command: Command::Put { key: "b".into(), value: "2".into() } },
        ];
        let req = AppendEntriesRequest {
            term: 1,
            leader_id: 1,
            prev_log_index: 0,
            prev_log_term: 0,
            entries,
            leader_commit: 2,
        };
        let (node, actions) = node.handle_append_entries(req);
        assert_eq!(node.volatile.commit_index, 2);
        assert_eq!(node.persistent.log.last_index(), 2);
        assert_eq!(actions.entries_to_apply.len(), 2);
        let accepted = actions.messages.iter().any(|(_, m)| {
            matches!(m, RaftMessage::AppendEntriesResponse(r) if r.success)
        });
        assert!(accepted);
    }

    #[test]
    fn follower_rejects_append_entries_on_log_mismatch() {
        let node = make_node(2, vec![1, 3]);
        let req = AppendEntriesRequest {
            term: 1,
            leader_id: 1,
            prev_log_index: 5, // follower has no log — mismatch
            prev_log_term: 1,
            entries: vec![],
            leader_commit: 0,
        };
        let (_, actions) = node.handle_append_entries(req);
        let rejected = actions.messages.iter().any(|(_, m)| {
            matches!(m, RaftMessage::AppendEntriesResponse(r) if !r.success)
        });
        assert!(rejected);
    }

    #[test]
    fn step_down_on_higher_term() {
        let mut node = make_node(1, vec![2, 3]);
        node.role = Role::Leader;
        node.persistent.current_term = 3;
        let (node, actions) = node.step_down(5);
        assert_eq!(node.role, Role::Follower);
        assert_eq!(node.current_term(), 5);
        assert!(node.persistent.voted_for.is_none());
        assert!(actions.persist.is_some());
    }
}
