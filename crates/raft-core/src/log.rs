use crate::message::LogEntry;

/// An immutable replicated log.
///
/// All mutation methods return a *new* `RaftLog` — the original is never
/// modified. This makes the state transitions in `RaftNode` easy to reason
/// about and test deterministically.
///
/// Log indices are 1-based (index 0 is a sentinel "before the log begins").
#[derive(Debug, Clone, Default)]
pub struct RaftLog {
    entries: Vec<LogEntry>,
}

impl RaftLog {
    pub fn new() -> Self {
        Self::default()
    }

    // ── Queries ────────────────────────────────────────────────────────────

    /// The index of the last entry, or 0 if the log is empty.
    pub fn last_index(&self) -> u64 {
        self.entries.last().map_or(0, |e| e.index)
    }

    /// The term of the last entry, or 0 if the log is empty.
    pub fn last_term(&self) -> u64 {
        self.entries.last().map_or(0, |e| e.term)
    }

    /// Returns the entry at `index`, or `None` if out of range.
    pub fn get(&self, index: u64) -> Option<&LogEntry> {
        if index == 0 {
            return None;
        }
        // Entries are stored contiguously; index is 1-based.
        let pos = self.index_to_pos(index)?;
        self.entries.get(pos)
    }

    /// The term of the entry at `index`, or 0 if the index is 0 / out of range.
    pub fn term_at(&self, index: u64) -> u64 {
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
        Self { entries }
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
                Self { entries }
            }
            None => self.clone(),
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
}
