use std::collections::BTreeMap;

use raft_core::message::Command;

/// Immutable key-value state machine.
///
/// Every mutation returns a **new** `KvStore`; the original is never modified.
/// This aligns with the immutable-data principle and makes snapshot-based
/// log compaction straightforward to add later.
#[derive(Debug, Clone, Default)]
pub struct KvStore {
    data: BTreeMap<String, String>,
}

impl KvStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a committed log command and return (new_store, optional_result).
    ///
    /// `optional_result` is `Some(value)` for operations that have a
    /// meaningful return value (e.g. the previous value of a key), and
    /// `None` for Noop entries.
    pub fn apply(&self, command: &Command) -> (KvStore, Option<String>) {
        match command {
            Command::Put { key, value } => {
                let mut data = self.data.clone();
                let prev = data.insert(key.clone(), value.clone());
                (KvStore { data }, prev)
            }
            Command::Delete { key } => {
                let mut data = self.data.clone();
                let removed = data.remove(key);
                (KvStore { data }, removed)
            }
            Command::Noop => (self.clone(), None),
        }
    }

    /// Look up a key without mutating state.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.data.get(key).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_and_get() {
        let store = KvStore::new();
        let (store, _) = store.apply(&Command::Put {
            key: "x".into(),
            value: "1".into(),
        });
        assert_eq!(store.get("x"), Some("1"));
    }

    #[test]
    fn overwrite_returns_previous() {
        let store = KvStore::new();
        let (store, _) = store.apply(&Command::Put {
            key: "x".into(),
            value: "1".into(),
        });
        let (store, prev) = store.apply(&Command::Put {
            key: "x".into(),
            value: "2".into(),
        });
        assert_eq!(prev, Some("1".into()));
        assert_eq!(store.get("x"), Some("2"));
    }

    #[test]
    fn delete_existing_key() {
        let store = KvStore::new();
        let (store, _) = store.apply(&Command::Put {
            key: "x".into(),
            value: "1".into(),
        });
        let (store, removed) = store.apply(&Command::Delete { key: "x".into() });
        assert_eq!(removed, Some("1".into()));
        assert!(store.get("x").is_none());
    }

    #[test]
    fn delete_nonexistent_key_returns_none() {
        let store = KvStore::new();
        let (store, removed) = store.apply(&Command::Delete {
            key: "missing".into(),
        });
        assert!(removed.is_none());
        assert!(store.is_empty());
    }

    #[test]
    fn noop_is_identity() {
        let store = KvStore::new();
        let (store, _) = store.apply(&Command::Put {
            key: "a".into(),
            value: "1".into(),
        });
        let (after, result) = store.clone().apply(&Command::Noop);
        assert!(result.is_none());
        assert_eq!(after.get("a"), Some("1"));
    }

    #[test]
    fn immutability_original_unchanged() {
        let store = KvStore::new();
        let (store_with_key, _) = store.apply(&Command::Put {
            key: "k".into(),
            value: "v".into(),
        });
        // Original store is untouched.
        assert!(store.get("k").is_none());
        assert_eq!(store_with_key.get("k"), Some("v"));
    }
}
