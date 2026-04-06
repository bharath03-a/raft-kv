use crate::message::LogEntry;

/// An immutable replicated log with support for log compaction (§7).
///
/// All mutation methods return a *new* `RaftLog` — the original is never
/// modified. This makes the state transitions in `RaftNode` easy to reason
/// about and test deterministically.
///
/// Log indices are 1-based (index 0 is a sentinel "before the log begins").
///
/// After a snapshot is taken, entries before `snapshot_last_index` are
/// discarded. The snapshot metadata acts as a virtual "index 0" for the
/// remaining entries.
#[derive(Debug, Clone)]
pub struct RaftLog {
    entries: Vec<LogEntry>,
    /// Index of the last entry included in the most recent snapshot (0 = none).
    snapshot_last_index: u64,
    /// Term of that entry (needed for log consistency checks after compaction).
    snapshot_last_term: u64,
}

impl Default for RaftLog {
    fn default() -> Self {
        Self::new()
    }
}

impl RaftLog {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            snapshot_last_index: 0,
            snapshot_last_term: 0,
        }
    }

    /// Create a log whose base starts at an already-snapshotted point.
    /// Used on restart when loading state after a snapshot.
    pub fn new_with_snapshot(last_included_index: u64, last_included_term: u64) -> Self {
        Self {
            entries: Vec::new(),
            snapshot_last_index: last_included_index,
            snapshot_last_term: last_included_term,
        }
    }

    // ── Snapshot accessors ─────────────────────────────────────────────────

    pub fn snapshot_last_index(&self) -> u64 {
        self.snapshot_last_index
    }

    pub fn snapshot_last_term(&self) -> u64 {
        self.snapshot_last_term
    }

    // ── Queries ────────────────────────────────────────────────────────────

    /// The index of the last entry, or `snapshot_last_index` if no entries
    /// remain after compaction.
    pub fn last_index(&self) -> u64 {
        self.entries
            .last()
            .map_or(self.snapshot_last_index, |e| e.index)
    }

    /// The term of the last entry, or `snapshot_last_term` if no entries
    /// remain after compaction.
    pub fn last_term(&self) -> u64 {
        self.entries
            .last()
            .map_or(self.snapshot_last_term, |e| e.term)
    }

    /// Returns the entry at `index`, or `None` if out of range or compacted away.
    pub fn get(&self, index: u64) -> Option<&LogEntry> {
        if index == 0 {
            return None;
        }
        let pos = self.index_to_pos(index)?;
        self.entries.get(pos)
    }

    /// The term of the entry at `index`.
    /// Returns `snapshot_last_term` if `index == snapshot_last_index`,
    /// the entry's term if it's in the live log, or 0 otherwise.
    pub fn term_at(&self, index: u64) -> u64 {
        if index == 0 {
            return 0;
        }
        if index == self.snapshot_last_index {
            return self.snapshot_last_term;
        }
        self.get(index).map_or(0, |e| e.term)
    }

    /// All entries with index > `after_index`.
    pub fn entries_after(&self, after_index: u64) -> &[LogEntry] {
        match self.index_to_pos(after_index + 1) {
            Some(pos) => &self.entries[pos..],
            None if after_index >= self.last_index() => &[],
            None => &self.entries,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    // ── Mutations (return new RaftLog) ─────────────────────────────────────

    /// Returns a new log with `entry` appended.
    pub fn append(&self, entry: LogEntry) -> Self {
        debug_assert!(
            entry.index == self.last_index() + 1,
            "log entry index must be contiguous: expected {}, got {}",
            self.last_index() + 1,
            entry.index
        );
        let mut entries = self.entries.clone();
        entries.push(entry);
        Self {
            entries,
            snapshot_last_index: self.snapshot_last_index,
            snapshot_last_term: self.snapshot_last_term,
        }
    }

    /// Returns a new log with all entries having index >= `from_index` removed.
    ///
    /// Used by followers to resolve log conflicts before appending the
    /// leader's entries.
    pub fn truncate_from(&self, from_index: u64) -> Self {
        if from_index == 0 || from_index > self.last_index() + 1 {
            return self.clone();
        }
        match self.index_to_pos(from_index) {
            Some(pos) => {
                let mut entries = self.entries.clone();
                entries.truncate(pos);
                Self {
                    entries,
                    snapshot_last_index: self.snapshot_last_index,
                    snapshot_last_term: self.snapshot_last_term,
                }
            }
            None => self.clone(),
        }
    }

    /// Returns a new log with all entries up to and including `index` removed.
    ///
    /// The snapshot metadata is updated to record `index` as the new base.
    /// Entries strictly after `index` are preserved.
    pub fn compact_up_to(&self, index: u64) -> Self {
        if index <= self.snapshot_last_index {
            return self.clone();
        }
        let new_term = self.term_at(index);
        let entries: Vec<LogEntry> = self
            .entries
            .iter()
            .filter(|e| e.index > index)
            .cloned()
            .collect();
        Self {
            entries,
            snapshot_last_index: index,
            snapshot_last_term: new_term,
        }
    }

    // ── Private helpers ────────────────────────────────────────────────────

    /// Convert a 1-based log index to a 0-based `entries` array position.
    fn index_to_pos(&self, index: u64) -> Option<usize> {
        if self.entries.is_empty() || index == 0 {
            return None;
        }
        let first = self.entries[0].index;
        if index < first {
            return None;
        }
        let pos = (index - first) as usize;
        if pos < self.entries.len() {
            Some(pos)
        } else {
            None
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Command;

    fn entry(index: u64, term: u64) -> LogEntry {
        LogEntry {
            index,
            term,
            command: Command::Noop,
        }
    }

    fn log_with(entries: &[(u64, u64)]) -> RaftLog {
        entries
            .iter()
            .fold(RaftLog::new(), |l, &(idx, term)| l.append(entry(idx, term)))
    }

    #[test]
    fn empty_log_sentinels() {
        let log = RaftLog::new();
        assert_eq!(log.last_index(), 0);
        assert_eq!(log.last_term(), 0);
        assert!(log.is_empty());
    }

    #[test]
    fn append_and_query() {
        let log = log_with(&[(1, 1), (2, 1), (3, 2)]);
        assert_eq!(log.last_index(), 3);
        assert_eq!(log.last_term(), 2);
        assert_eq!(log.get(2).unwrap().index, 2);
        assert_eq!(log.term_at(1), 1);
        assert_eq!(log.term_at(99), 0); // out of range → 0
    }

    #[test]
    fn truncate_removes_tail() {
        let log = log_with(&[(1, 1), (2, 1), (3, 2), (4, 2)]);
        let truncated = log.truncate_from(3);
        assert_eq!(truncated.last_index(), 2);
        assert!(truncated.get(3).is_none());
    }

    #[test]
    fn truncate_beyond_end_is_noop() {
        let log = log_with(&[(1, 1)]);
        let unchanged = log.truncate_from(99);
        assert_eq!(unchanged.last_index(), 1);
    }

    #[test]
    fn entries_after_returns_suffix() {
        let log = log_with(&[(1, 1), (2, 1), (3, 2)]);
        let after = log.entries_after(1);
        assert_eq!(after.len(), 2);
        assert_eq!(after[0].index, 2);
    }

    #[test]
    fn entries_after_past_end_is_empty() {
        let log = log_with(&[(1, 1)]);
        assert!(log.entries_after(1).is_empty());
    }

    #[test]
    fn immutability_original_unchanged() {
        let original = log_with(&[(1, 1)]);
        let _new = original.append(entry(2, 1));
        assert_eq!(original.last_index(), 1); // original untouched
    }

    // ── Snapshot / compaction tests ────────────────────────────────────────

    #[test]
    fn compact_removes_prefix_and_updates_base() {
        let log = log_with(&[(1, 1), (2, 1), (3, 2), (4, 2), (5, 3)]);
        let compacted = log.compact_up_to(3);
        assert_eq!(compacted.snapshot_last_index(), 3);
        assert_eq!(compacted.snapshot_last_term(), 2);
        assert_eq!(compacted.last_index(), 5);
        assert!(compacted.get(1).is_none());
        assert!(compacted.get(3).is_none()); // compacted away
        assert!(compacted.get(4).is_some());
        assert!(compacted.get(5).is_some());
        assert_eq!(compacted.len(), 2);
    }

    #[test]
    fn compact_all_entries_leaves_empty_log_with_base() {
        let log = log_with(&[(1, 1), (2, 2), (3, 2)]);
        let compacted = log.compact_up_to(3);
        assert_eq!(compacted.snapshot_last_index(), 3);
        assert_eq!(compacted.snapshot_last_term(), 2);
        assert_eq!(compacted.last_index(), 3); // returns snapshot base
        assert_eq!(compacted.last_term(), 2);
        assert!(compacted.is_empty());
    }

    #[test]
    fn compact_noop_when_already_snapshotted() {
        let log = log_with(&[(1, 1), (2, 1)]);
        let c1 = log.compact_up_to(2);
        let c2 = c1.compact_up_to(1); // index before current snapshot — noop
        assert_eq!(c2.snapshot_last_index(), 2);
    }

    #[test]
    fn append_after_compaction_uses_snapshot_base() {
        let log = log_with(&[(1, 1), (2, 1), (3, 2)]);
        let compacted = log.compact_up_to(3); // entries is now empty, base = 3
        let extended = compacted.append(entry(4, 2));
        assert_eq!(extended.last_index(), 4);
        assert_eq!(extended.len(), 1);
    }

    #[test]
    fn term_at_snapshot_boundary() {
        let log = log_with(&[(1, 1), (2, 2), (3, 3)]);
        let compacted = log.compact_up_to(2);
        // Term at the snapshot boundary should return snapshot_last_term.
        assert_eq!(compacted.term_at(2), 2);
        // Term before the snapshot is gone.
        assert_eq!(compacted.term_at(1), 0);
        // Term after the snapshot is still in the log.
        assert_eq!(compacted.term_at(3), 3);
    }

    #[test]
    fn new_with_snapshot_has_correct_base() {
        let log = RaftLog::new_with_snapshot(50, 3);
        assert_eq!(log.last_index(), 50);
        assert_eq!(log.last_term(), 3);
        assert!(log.is_empty());
        // Can append at 51.
        let log = log.append(entry(51, 3));
        assert_eq!(log.last_index(), 51);
    }

    #[test]
    fn entries_after_compacted_prefix() {
        let log = log_with(&[(1, 1), (2, 1), (3, 2), (4, 2)]);
        let compacted = log.compact_up_to(2);
        // entries_after(0) should return remaining entries (3, 4).
        let after = compacted.entries_after(0);
        assert_eq!(after.len(), 2);
        assert_eq!(after[0].index, 3);
    }
}
