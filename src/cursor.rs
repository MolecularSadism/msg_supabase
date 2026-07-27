//! Bookkeeping for incremental syncs.
//!
//! A client that pushes the same growing data set on every autosave needs to
//! know what the server already has. Two shapes cover most of it:
//!
//! - [`SyncCursor`] — how many items of an append-only list have been pushed, so the next push
//!   carries only the tail. [`SyncCursors`] keeps one cursor per key, for lists grouped by run,
//!   level, or any other identifier.
//! - [`SyncWatermarks`] — the last value pushed for a counter that only grows, so unchanged
//!   counters are skipped.

use std::collections::HashMap;
use std::hash::Hash;

/// How many items of an append-only list have been synced.
///
/// # Example
///
/// ```rust
/// use msg_supabase::prelude::*;
///
/// let kills = ["goblin", "troll", "dragon"];
/// let mut cursor = SyncCursor::default();
///
/// assert_eq!(cursor.pending(&kills), &kills[..]);
/// cursor.mark_synced(2);
/// assert_eq!(cursor.pending(&kills), &["dragon"]);
/// ```
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SyncCursor {
    synced: usize,
}

impl SyncCursor {
    /// Create a cursor that has synced `count` items.
    #[must_use]
    pub fn new(count: usize) -> Self {
        Self { synced: count }
    }

    /// Number of items synced so far.
    pub fn synced(&self) -> usize {
        self.synced
    }

    /// The tail of `items` that has not been synced yet.
    pub fn pending<'a, T>(&self, items: &'a [T]) -> &'a [T] {
        items.get(self.synced.min(items.len())..).unwrap_or(&[])
    }

    /// Whether every item of `items` has been synced.
    pub fn is_caught_up<T>(&self, items: &[T]) -> bool {
        self.pending(items).is_empty()
    }

    /// Record that the first `count` items are synced.
    ///
    /// The cursor only moves forward, so a late callback reporting an older
    /// count leaves it untouched.
    pub fn mark_synced(&mut self, count: usize) {
        self.synced = self.synced.max(count);
    }

    /// Forget what was synced, so the next push carries everything again.
    pub fn reset(&mut self) {
        self.synced = 0;
    }
}

/// One [`SyncCursor`] per key, for append-only lists grouped by an identifier.
///
/// # Example
///
/// ```rust
/// use msg_supabase::prelude::*;
///
/// let mut cursors: SyncCursors<usize> = SyncCursors::default();
/// let run_kills = ["goblin", "troll"];
///
/// assert_eq!(cursors.pending(&0, &run_kills).len(), 2);
/// cursors.mark_synced(0, run_kills.len());
/// assert!(cursors.pending(&0, &run_kills).is_empty());
/// ```
#[derive(Debug, Clone)]
pub struct SyncCursors<K> {
    cursors: HashMap<K, SyncCursor>,
}

impl<K> Default for SyncCursors<K> {
    fn default() -> Self {
        Self {
            cursors: HashMap::new(),
        }
    }
}

impl<K: Eq + Hash> SyncCursors<K> {
    /// Create an empty set of cursors.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The cursor for `key`, or a fresh one if the key is unknown.
    pub fn cursor(&self, key: &K) -> SyncCursor {
        self.cursors.get(key).copied().unwrap_or_default()
    }

    /// Number of items synced for `key`.
    pub fn synced(&self, key: &K) -> usize {
        self.cursor(key).synced()
    }

    /// The tail of `items` not yet synced for `key`.
    pub fn pending<'a, T>(&self, key: &K, items: &'a [T]) -> &'a [T] {
        self.cursor(key).pending(items)
    }

    /// Record that the first `count` items of `key` are synced.
    pub fn mark_synced(&mut self, key: K, count: usize) {
        self.cursors.entry(key).or_default().mark_synced(count);
    }

    /// Number of keys with a recorded cursor.
    pub fn len(&self) -> usize {
        self.cursors.len()
    }

    /// Whether any key has a recorded cursor.
    pub fn is_empty(&self) -> bool {
        self.cursors.is_empty()
    }

    /// Forget every cursor.
    pub fn clear(&mut self) {
        self.cursors.clear();
    }
}

/// The highest value pushed for each of a set of monotonically growing counters.
///
/// # Example
///
/// ```rust
/// use msg_supabase::prelude::*;
///
/// let mut casts: SyncWatermarks<String> = SyncWatermarks::default();
///
/// assert!(casts.is_ahead(&"fireball".to_string(), 3));
/// casts.mark_synced("fireball".to_string(), 3);
/// assert!(!casts.is_ahead(&"fireball".to_string(), 3));
/// assert!(casts.is_ahead(&"fireball".to_string(), 4));
/// ```
#[derive(Debug, Clone)]
pub struct SyncWatermarks<K, V = u32> {
    marks: HashMap<K, V>,
}

impl<K, V> Default for SyncWatermarks<K, V> {
    fn default() -> Self {
        Self {
            marks: HashMap::new(),
        }
    }
}

impl<K: Eq + Hash, V: Copy + Ord + Default> SyncWatermarks<K, V> {
    /// Create an empty set of watermarks.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The value last synced for `key`, or the default if it is unknown.
    pub fn synced(&self, key: &K) -> V {
        self.marks.get(key).copied().unwrap_or_default()
    }

    /// Whether `value` is above the watermark for `key` and so worth pushing.
    pub fn is_ahead(&self, key: &K, value: V) -> bool {
        value > self.synced(key)
    }

    /// Record `value` as synced for `key`, keeping the higher of the two.
    pub fn mark_synced(&mut self, key: K, value: V) {
        let mark = self.marks.entry(key).or_default();
        *mark = (*mark).max(value);
    }

    /// Number of keys with a recorded watermark.
    pub fn len(&self) -> usize {
        self.marks.len()
    }

    /// Whether any key has a recorded watermark.
    pub fn is_empty(&self) -> bool {
        self.marks.is_empty()
    }

    /// Forget every watermark.
    pub fn clear(&mut self) {
        self.marks.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_cursor_has_everything_pending() {
        let cursor = SyncCursor::default();
        assert_eq!(cursor.synced(), 0);
        assert_eq!(cursor.pending(&[1, 2, 3]), &[1, 2, 3]);
        assert!(!cursor.is_caught_up(&[1]));
    }

    #[test]
    fn cursor_skips_synced_prefix() {
        let mut cursor = SyncCursor::default();
        cursor.mark_synced(2);
        assert_eq!(cursor.pending(&[1, 2, 3, 4]), &[3, 4]);
    }

    #[test]
    fn cursor_beyond_list_length_yields_nothing() {
        let cursor = SyncCursor::new(9);
        assert_eq!(cursor.pending(&[1, 2]), &[] as &[i32]);
        assert!(cursor.is_caught_up(&[1, 2]));
    }

    #[test]
    fn cursor_only_moves_forward() {
        let mut cursor = SyncCursor::new(5);
        cursor.mark_synced(2);
        assert_eq!(cursor.synced(), 5);
    }

    #[test]
    fn cursor_reset_replays_everything() {
        let mut cursor = SyncCursor::new(3);
        cursor.reset();
        assert_eq!(cursor.pending(&[1, 2, 3]), &[1, 2, 3]);
    }

    #[test]
    fn cursors_track_keys_independently() {
        let mut cursors: SyncCursors<usize> = SyncCursors::new();
        cursors.mark_synced(0, 2);

        assert_eq!(cursors.pending(&0, &[1, 2, 3]), &[3]);
        assert_eq!(cursors.pending(&1, &[1, 2, 3]), &[1, 2, 3]);
        assert_eq!(cursors.len(), 1);
    }

    #[test]
    fn cursors_clear_forgets_progress() {
        let mut cursors: SyncCursors<&str> = SyncCursors::new();
        cursors.mark_synced("run", 4);
        cursors.clear();
        assert!(cursors.is_empty());
        assert_eq!(cursors.synced(&"run"), 0);
    }

    #[test]
    fn watermarks_skip_unchanged_counters() {
        let mut marks: SyncWatermarks<&str> = SyncWatermarks::new();
        marks.mark_synced("fireball", 7);

        assert_eq!(marks.synced(&"fireball"), 7);
        assert!(!marks.is_ahead(&"fireball", 7));
        assert!(marks.is_ahead(&"fireball", 8));
        assert!(marks.is_ahead(&"icebolt", 1));
    }

    #[test]
    fn watermarks_keep_the_highest_value() {
        let mut marks: SyncWatermarks<&str> = SyncWatermarks::new();
        marks.mark_synced("fireball", 7);
        marks.mark_synced("fireball", 3);
        assert_eq!(marks.synced(&"fireball"), 7);
    }

    #[test]
    fn watermarks_accept_composite_keys() {
        let mut marks: SyncWatermarks<(usize, String)> = SyncWatermarks::new();
        marks.mark_synced((0, "fireball".to_string()), 2);
        assert!(!marks.is_ahead(&(0, "fireball".to_string()), 2));
        assert!(marks.is_ahead(&(1, "fireball".to_string()), 1));
    }
}
