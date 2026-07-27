//! Sync state tracking for Supabase operations.

use bevy::prelude::*;
use std::collections::HashSet;
use std::marker::PhantomData;

use crate::traits::SupabaseRow;

/// Resource that tracks sync state for a specific type.
///
/// Each registered `SupabaseRow` type gets its own `SyncState<T>` resource
/// to track:
/// - Which records have been synced (to avoid duplicates)
/// - The primary key assigned by Supabase (for Update mode)
/// - Sync statistics
///
/// # Example
///
/// ```rust
/// use bevy::prelude::*;
/// use msg_supabase::prelude::*;
///
/// fn check_sync_state<T: SupabaseRow>(state: Res<SyncState<T>>) {
///     if let Some(pk) = state.primary_key() {
///         println!("Primary key: {}", pk);
///     }
///     println!("Total synced: {}", state.sync_count());
/// }
/// ```
#[derive(Resource, Clone)]
pub struct SyncState<T: SupabaseRow> {
    /// Set of sync keys that have been successfully synced.
    synced_keys: HashSet<String>,

    /// Primary key returned from Supabase after first insert.
    /// Used for subsequent updates in `SaveMode::Update` mode.
    primary_key: Option<i64>,

    /// Total number of successful syncs for this type.
    sync_count: u32,

    /// Whether the initial insert has been completed (for Update mode).
    initial_insert_done: bool,

    _marker: PhantomData<T>,
}

impl<T: SupabaseRow> Default for SyncState<T> {
    fn default() -> Self {
        Self {
            synced_keys: HashSet::new(),
            primary_key: None,
            sync_count: 0,
            initial_insert_done: false,
            _marker: PhantomData,
        }
    }
}

impl<T: SupabaseRow> SyncState<T> {
    /// Create a new sync state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if a sync key has already been synced.
    pub fn is_synced(&self, key: &str) -> bool {
        self.synced_keys.contains(key)
    }

    /// Mark a sync key as synced.
    pub fn mark_synced(&mut self, key: String) {
        self.synced_keys.insert(key);
        self.sync_count += 1;
    }

    /// Get all synced keys.
    pub fn synced_keys(&self) -> &HashSet<String> {
        &self.synced_keys
    }

    /// Get the number of synced keys.
    pub fn synced_count(&self) -> usize {
        self.synced_keys.len()
    }

    /// Get the total sync count (including updates to same key).
    pub fn sync_count(&self) -> u32 {
        self.sync_count
    }

    /// Get the primary key if one has been assigned.
    pub fn primary_key(&self) -> Option<i64> {
        self.primary_key
    }

    /// Set the primary key (called after initial insert).
    pub fn set_primary_key(&mut self, key: i64) {
        self.primary_key = Some(key);
        self.initial_insert_done = true;
    }

    /// Check if the initial insert has been done (for Update mode).
    pub fn initial_insert_done(&self) -> bool {
        self.initial_insert_done
    }

    /// Mark the initial insert as done without setting a primary key.
    pub fn mark_initial_insert_done(&mut self) {
        self.initial_insert_done = true;
    }

    /// Clear all sync state (useful for testing or session reset).
    pub fn clear(&mut self) {
        self.synced_keys.clear();
        self.primary_key = None;
        self.sync_count = 0;
        self.initial_insert_done = false;
    }

    /// Increment the sync count without marking a key.
    pub fn increment_sync_count(&mut self) {
        self.sync_count += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[derive(Clone, Serialize)]
    struct TestRow {
        id: i64,
    }

    impl SupabaseRow for TestRow {
        type Response = crate::request::PrimaryKeyResponse;

        fn table_name() -> &'static str {
            "test"
        }
        fn primary_key_column() -> &'static str {
            "id"
        }
        fn unique_columns() -> &'static [&'static str] {
            &["id"]
        }
    }

    #[test]
    fn test_default_state() {
        let state = SyncState::<TestRow>::default();
        assert!(state.synced_keys.is_empty());
        assert!(state.primary_key.is_none());
        assert_eq!(state.sync_count, 0);
        assert!(!state.initial_insert_done);
    }

    #[test]
    fn test_mark_synced() {
        let mut state = SyncState::<TestRow>::new();
        state.mark_synced("key1".to_string());
        assert!(state.is_synced("key1"));
        assert!(!state.is_synced("key2"));
        assert_eq!(state.sync_count(), 1);
    }

    #[test]
    fn test_primary_key() {
        let mut state = SyncState::<TestRow>::new();
        assert!(!state.initial_insert_done());
        state.set_primary_key(123);
        assert_eq!(state.primary_key(), Some(123));
        assert!(state.initial_insert_done());
    }

    #[test]
    fn test_clear() {
        let mut state = SyncState::<TestRow>::new();
        state.mark_synced("key1".to_string());
        state.set_primary_key(123);
        state.clear();
        assert!(state.synced_keys.is_empty());
        assert!(state.primary_key.is_none());
        assert_eq!(state.sync_count, 0);
        assert!(!state.initial_insert_done);
    }
}
