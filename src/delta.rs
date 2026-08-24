//! Delta-sync bookkeeping: high-water marks, an in-flight guard, and
//! foreign-key hold-back for one table's append-only send queue.
//!
//! A host that pushes a growing list on every save wants to send only the
//! tail past what the server has acknowledged. [`SupabaseDeltaSync`] holds
//! that bookkeeping for one table:
//!
//! - **High-water marks** — how many records of each partition (a run, a
//!   level, or just `()` for an unpartitioned table) have been acknowledged.
//!   [`take_batch`](SupabaseDeltaSync::take_batch) slices each partition past
//!   its mark; [`commit`](SupabaseDeltaSync::commit) advances the marks only
//!   once the write lands, so a dropped or failed request is carried again by
//!   the next push.
//! - **In-flight guard and batch identity** — an insert is not idempotent, so
//!   while a batch is unanswered the table sits out further pushes rather
//!   than sending the same rows twice. [`take_batch`](SupabaseDeltaSync::take_batch)
//!   hands back a [`BatchId`] alongside the rows; the host captures it next
//!   to the request it sends and hands it back on the matching outcome —
//!   [`commit`](SupabaseDeltaSync::commit) on
//!   [`SyncComplete`](crate::SyncComplete),
//!   [`abort`](SupabaseDeltaSync::abort) on [`SyncError`](crate::SyncError).
//!   An outcome whose id does not match the in-flight batch — a stale answer,
//!   or another system's write of the same row type — is logged and ignored
//!   instead of advancing marks for rows that never landed.
//! - **Foreign-key hold-back and substitution** — child rows reference a key
//!   the server assigned to a parent row. A partition whose parent key has
//!   not been resolved yet is held back (its mark untouched, its records kept
//!   for a later push); once the resolver answers, the key is handed to the
//!   row builder for substitution.
//!
//! The send queues are append-only: records may only be added past the
//! acknowledged marks. When a new session replaces the histories, call
//! [`reset`](SupabaseDeltaSync::reset) so the marks start over.
//!
//! The host keeps the schema: the concrete table cascade order, the resolver
//! (typically a map filled from a parent table's `SyncComplete` rows), and
//! row construction. This type takes only the mechanism.
//!
//! ```
//! use msg_supabase::prelude::*;
//! use serde::Serialize;
//!
//! #[derive(Clone, Serialize)]
//! struct KillRow { run_id: i64, victim: String }
//! impl SupabaseRow for KillRow {
//!     type Response = PrimaryKeyResponse;
//!     fn table_name() -> &'static str { "kills" }
//!     fn primary_key_column() -> &'static str { "id" }
//!     fn unique_columns() -> &'static [&'static str] { &[] }
//! }
//!
//! let mut sync = SupabaseDeltaSync::<KillRow, usize, i64>::default();
//! let kills_per_run: Vec<(usize, Vec<String>)> =
//!     vec![(0, vec!["a".into(), "b".into()]), (1, vec!["c".into()])];
//! // Run 0's server id is known; run 1's row has not been acked yet.
//! let run_ids = [(0usize, 77i64)].into_iter().collect::<std::collections::HashMap<_, _>>();
//!
//! let (id, batch) = sync
//!     .take_batch(
//!         kills_per_run.iter().map(|(run, kills)| (*run, kills.as_slice())),
//!         |run| run_ids.get(run).copied(),
//!         |victim, &run_id| KillRow { run_id, victim: victim.clone() },
//!     )
//!     .expect("run 0 has pending rows");
//! assert_eq!(batch.len(), 2); // run 1's kill is held back
//!
//! // The host keeps `id` next to the request it sends, and its
//! // SyncComplete/SyncError observers hand back that captured id — an
//! // outcome from any other write of KillRow carries a different id (or
//! // none) and is ignored.
//! sync.commit(id); // the write landed: marks advance, the guard clears
//! assert_eq!(sync.acked(&0), 2);
//! assert_eq!(sync.acked(&1), 0);
//! ```

use std::collections::HashMap;
use std::fmt;
use std::hash::Hash;
use std::marker::PhantomData;

use bevy::ecs::resource::Resource;
use bevy::log::warn;

use crate::traits::SupabaseRow;

/// Identity of one batch handed out by
/// [`take_batch`](SupabaseDeltaSync::take_batch).
///
/// Ids increase monotonically per [`SupabaseDeltaSync`] and are never reused,
/// not even across [`reset`](SupabaseDeltaSync::reset). The host captures the
/// id when it takes a batch and hands it back to
/// [`commit`](SupabaseDeltaSync::commit) or
/// [`abort`](SupabaseDeltaSync::abort) when the matching outcome arrives, so
/// a stale or unrelated outcome cannot advance the marks or clear the guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BatchId(u64);

/// Delta-sync bookkeeping for one table's append-only send queue: per-partition
/// high-water marks, an in-flight guard, and foreign-key hold-back.
///
/// Type parameters:
/// - `R` — the [`SupabaseRow`] this table writes; one `SupabaseDeltaSync` per
///   table keeps the states distinct.
/// - `P` — the partition key records are grouped by (e.g. a run index). Use
///   `()` for an unpartitioned table.
/// - `K` — the parent key children reference (e.g. the server-assigned run
///   id). Use `()` when the table has no parent.
///
/// Like the crate's other per-table state ([`SyncState`](crate::SyncState),
/// [`ResultQueue`](crate::ResultQueue)), this is a Bevy [`Resource`], so a
/// host stores one per table directly via `ResMut<SupabaseDeltaSync<R, P, K>>`.
///
/// See the [module docs](self) for the full contract and an example.
pub struct SupabaseDeltaSync<R: SupabaseRow, P = (), K = i64> {
    /// How many records of each partition have been acknowledged.
    acked: HashMap<P, usize>,
    /// The batch awaiting the server's answer: its id and the marks it would
    /// advance to.
    in_flight: Option<(BatchId, HashMap<P, usize>)>,
    /// The id the next batch will be handed; never reused.
    next_batch_id: u64,
    _marker: PhantomData<fn() -> (R, K)>,
}

impl<R, P, K> Resource for SupabaseDeltaSync<R, P, K>
where
    R: SupabaseRow,
    P: Send + Sync + 'static,
    K: 'static,
{
}

impl<R: SupabaseRow, P, K> Default for SupabaseDeltaSync<R, P, K> {
    fn default() -> Self {
        Self {
            acked: HashMap::new(),
            in_flight: None,
            next_batch_id: 0,
            _marker: PhantomData,
        }
    }
}

impl<R: SupabaseRow, P: fmt::Debug, K> fmt::Debug for SupabaseDeltaSync<R, P, K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SupabaseDeltaSync")
            .field("table", &R::table_name())
            .field("acked", &self.acked)
            .field("in_flight", &self.in_flight)
            .finish()
    }
}

impl<R, P, K> SupabaseDeltaSync<R, P, K>
where
    R: SupabaseRow,
    P: Eq + Hash + Clone,
{
    /// A fresh state: nothing acknowledged, nothing in flight.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many records of `partition` the server has acknowledged.
    #[must_use]
    pub fn acked(&self, partition: &P) -> usize {
        self.acked.get(partition).copied().unwrap_or(0)
    }

    /// The acknowledged marks of every partition seen so far.
    #[must_use]
    pub fn marks(&self) -> &HashMap<P, usize> {
        &self.acked
    }

    /// Whether a batch is currently awaiting the server's answer.
    ///
    /// While this is `true`, [`take_batch`](Self::take_batch) refuses to
    /// produce another batch — an insert is not idempotent, so the same rows
    /// must not go out twice.
    #[must_use]
    pub fn is_in_flight(&self) -> bool {
        self.in_flight.is_some()
    }

    /// The id of the batch currently awaiting the server's answer, if any.
    #[must_use]
    pub fn in_flight_id(&self) -> Option<BatchId> {
        self.in_flight.as_ref().map(|(id, _)| *id)
    }

    /// Slice every partition past its mark and build the rows of one batch.
    ///
    /// For each `(partition, records)` pair, `resolve_parent` answers the
    /// server-assigned key the partition's rows reference:
    ///
    /// - `Some(key)` — the records past the partition's mark are turned into
    ///   rows via `to_row(record, &key)` (foreign-key substitution), and the
    ///   batch proposes advancing that partition's mark to `records.len()`.
    /// - `None` — the parent row has not been acknowledged yet, so the whole
    ///   partition is held back: no rows, mark untouched, records carried by
    ///   a later push.
    ///
    /// A partition's records list must be append-only: one that shrank below
    /// its acknowledged mark is held back untouched (with a warning) rather
    /// than re-sending rows the server already has. Call
    /// [`reset`](Self::reset) when a new session replaces the histories.
    ///
    /// Returns `None` when a batch is already in flight, or when nothing is
    /// pending. Otherwise the proposed marks are parked in flight under the
    /// returned [`BatchId`] — call [`commit`](Self::commit) with that id when
    /// the write lands, or [`abort`](Self::abort) with it when the write
    /// fails so the rows go out again.
    #[must_use = "dropping the batch still parks the proposed marks in flight; \
                  send the rows and commit() or abort() the returned id"]
    pub fn take_batch<'a, Rec, I>(
        &mut self,
        partitions: I,
        resolve_parent: impl Fn(&P) -> Option<K>,
        mut to_row: impl FnMut(&'a Rec, &K) -> R,
    ) -> Option<(BatchId, Vec<R>)>
    where
        Rec: 'a,
        I: IntoIterator<Item = (P, &'a [Rec])>,
    {
        if self.in_flight.is_some() {
            return None;
        }

        let mut rows = Vec::new();
        let mut proposed: Option<HashMap<P, usize>> = None;

        for (partition, records) in partitions {
            let Some(key) = resolve_parent(&partition) else {
                continue;
            };
            // Read through the proposal being built so a partition key
            // appearing twice in the input emits its rows only once.
            let already_sent = match &proposed {
                Some(marks) => marks.get(&partition).copied().unwrap_or(0),
                None => self.acked(&partition),
            };
            if records.len() < already_sent {
                warn!(
                    "Delta sync for table '{}': a partition's records list shrank below its \
                     acknowledged mark ({} < {}); holding it back — call reset() when a new \
                     session replaces the send queues",
                    R::table_name(),
                    records.len(),
                    already_sent,
                );
                continue;
            }
            if records.len() == already_sent {
                continue;
            }
            rows.extend(records.iter().skip(already_sent).map(|r| to_row(r, &key)));
            proposed
                .get_or_insert_with(|| self.acked.clone())
                .insert(partition, records.len());
        }

        let proposed = proposed?;
        let id = BatchId(self.next_batch_id);
        self.next_batch_id += 1;
        self.in_flight = Some((id, proposed));
        Some((id, rows))
    }

    /// The write landed: advance the marks the in-flight batch proposed and
    /// clear the guard.
    ///
    /// `id` must be the [`BatchId`] handed out by the
    /// [`take_batch`](Self::take_batch) call that produced the batch. When it
    /// does not match the in-flight batch — or nothing is in flight — the
    /// outcome is stale or belongs to another write of `R`, so it is logged
    /// and ignored rather than advancing marks for rows that never landed.
    pub fn commit(&mut self, id: BatchId) {
        if let Some((_, marks)) = self.in_flight.take_if(|(in_flight, _)| *in_flight == id) {
            self.acked = marks;
            return;
        }
        match &self.in_flight {
            Some((in_flight, _)) => warn!(
                "Delta sync for table '{}': ignoring commit of {id:?} while {in_flight:?} is in \
                 flight",
                R::table_name(),
            ),
            None => warn!(
                "Delta sync for table '{}': ignoring commit of {id:?} with nothing in flight",
                R::table_name(),
            ),
        }
    }

    /// The write failed or was dropped: discard the proposed marks, leaving
    /// the acknowledged marks untouched so the next push carries the same
    /// rows again.
    ///
    /// `id` must be the [`BatchId`] handed out by the
    /// [`take_batch`](Self::take_batch) call that produced the batch. When it
    /// does not match the in-flight batch — or nothing is in flight — the
    /// outcome is stale or belongs to another write of `R`, so it is logged
    /// and ignored rather than clearing the guard early.
    pub fn abort(&mut self, id: BatchId) {
        if self
            .in_flight
            .take_if(|(in_flight, _)| *in_flight == id)
            .is_some()
        {
            return;
        }
        match &self.in_flight {
            Some((in_flight, _)) => warn!(
                "Delta sync for table '{}': ignoring abort of {id:?} while {in_flight:?} is in \
                 flight",
                R::table_name(),
            ),
            None => warn!(
                "Delta sync for table '{}': ignoring abort of {id:?} with nothing in flight",
                R::table_name(),
            ),
        }
    }

    /// Forget everything — marks and in-flight state — e.g. when a new
    /// session begins and the send queues start over. This is the sanctioned
    /// path for replaced histories: without it, a records list that shrank
    /// below its acknowledged mark is held back by
    /// [`take_batch`](Self::take_batch) rather than re-sent.
    ///
    /// [`BatchId`]s keep counting up across a reset, so an outcome from
    /// before the reset can never be mistaken for a later batch's.
    pub fn reset(&mut self) {
        self.acked.clear();
        self.in_flight = None;
    }
}

impl<R, K> SupabaseDeltaSync<R, (), K>
where
    R: SupabaseRow,
{
    /// [`take_batch`](Self::take_batch) for an unpartitioned table with no
    /// parent key: one list, sliced past its single mark.
    #[must_use = "dropping the batch still parks the proposed marks in flight; \
                  send the rows and commit() or abort() the returned id"]
    pub fn take_pending<'a, Rec>(
        &mut self,
        records: &'a [Rec],
        mut to_row: impl FnMut(&'a Rec) -> R,
    ) -> Option<(BatchId, Vec<R>)>
    where
        Rec: 'a,
        K: Default,
    {
        self.take_batch(
            [((), records)],
            |()| Some(K::default()),
            |record, _| to_row(record),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::PrimaryKeyResponse;
    use serde::Serialize;

    /// Mock child row carrying the parent key it was patched with.
    #[derive(Clone, Serialize, Debug, PartialEq)]
    struct ChildRow {
        parent_id: i64,
        value: String,
    }

    impl SupabaseRow for ChildRow {
        type Response = PrimaryKeyResponse;
        fn table_name() -> &'static str {
            "children"
        }
        fn primary_key_column() -> &'static str {
            "id"
        }
        fn unique_columns() -> &'static [&'static str] {
            &[]
        }
    }

    fn records(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_string()).collect()
    }

    fn take(
        sync: &mut SupabaseDeltaSync<ChildRow, usize, i64>,
        partitions: &[(usize, Vec<String>)],
        parents: &HashMap<usize, i64>,
    ) -> Option<(BatchId, Vec<ChildRow>)> {
        sync.take_batch(
            partitions.iter().map(|(p, r)| (*p, r.as_slice())),
            |p| parents.get(p).copied(),
            |value, &parent_id| ChildRow {
                parent_id,
                value: value.clone(),
            },
        )
    }

    #[test]
    fn high_water_mark_advances_only_on_ack() {
        let mut sync = SupabaseDeltaSync::<ChildRow, usize, i64>::default();
        let parents: HashMap<usize, i64> = [(0, 10)].into_iter().collect();
        let lists = vec![(0, records(&["a", "b"]))];

        let (id, batch) = take(&mut sync, &lists, &parents).expect("two rows pending");
        assert_eq!(batch.len(), 2);
        assert_eq!(sync.acked(&0), 0, "marks advance on ack, not on send");

        sync.commit(id);
        assert_eq!(sync.acked(&0), 2);
        assert!(!sync.is_in_flight());
    }

    #[test]
    fn a_second_push_carries_only_the_new_records() {
        let mut sync = SupabaseDeltaSync::<ChildRow, usize, i64>::default();
        let parents: HashMap<usize, i64> = [(0, 10)].into_iter().collect();

        let lists = vec![(0, records(&["a", "b"]))];
        let (id, _) = take(&mut sync, &lists, &parents).expect("first batch");
        sync.commit(id);

        // The list has grown; only the tail goes out.
        let lists = vec![(0, records(&["a", "b", "c"]))];
        let (id, batch) = take(&mut sync, &lists, &parents).expect("one new row");
        assert_eq!(
            batch,
            vec![ChildRow {
                parent_id: 10,
                value: "c".to_string()
            }]
        );

        sync.commit(id);
        assert_eq!(sync.acked(&0), 3);

        // Caught up: nothing to send, and no guard is raised.
        assert!(take(&mut sync, &lists, &parents).is_none());
        assert!(!sync.is_in_flight());
    }

    #[test]
    fn no_double_sync_while_a_batch_is_in_flight() {
        let mut sync = SupabaseDeltaSync::<ChildRow, usize, i64>::default();
        let parents: HashMap<usize, i64> = [(0, 10)].into_iter().collect();
        let lists = vec![(0, records(&["a"]))];

        let (id, _) = take(&mut sync, &lists, &parents).expect("first batch");
        assert!(sync.is_in_flight());
        assert_eq!(sync.in_flight_id(), Some(id));

        // The same rows must not go out twice while unanswered.
        assert!(take(&mut sync, &lists, &parents).is_none());

        sync.commit(id);
        assert!(take(&mut sync, &lists, &parents).is_none(), "caught up");
    }

    #[test]
    fn children_wait_for_their_parent_and_get_its_key_substituted() {
        let mut sync = SupabaseDeltaSync::<ChildRow, usize, i64>::default();
        let lists = vec![(0, records(&["a"])), (1, records(&["b"]))];

        // Only parent 0 has been acked so far.
        let parents: HashMap<usize, i64> = [(0, 10)].into_iter().collect();
        let (id, batch) = take(&mut sync, &lists, &parents).expect("parent 0's child goes");
        assert_eq!(
            batch,
            vec![ChildRow {
                parent_id: 10,
                value: "a".to_string()
            }]
        );
        sync.commit(id);
        assert_eq!(sync.acked(&1), 0, "held-back partition's mark is untouched");

        // Parent 1's key arrives: its held-back child goes out, patched with
        // the server-assigned key.
        let parents: HashMap<usize, i64> = [(0, 10), (1, 20)].into_iter().collect();
        let (id, batch) = take(&mut sync, &lists, &parents).expect("parent 1's child goes");
        assert_eq!(
            batch,
            vec![ChildRow {
                parent_id: 20,
                value: "b".to_string()
            }]
        );
        sync.commit(id);
        assert_eq!(sync.acked(&0), 1);
        assert_eq!(sync.acked(&1), 1);
    }

    #[test]
    fn all_partitions_held_back_raises_no_batch() {
        let mut sync = SupabaseDeltaSync::<ChildRow, usize, i64>::default();
        let lists = vec![(0, records(&["a"])), (1, records(&["b"]))];

        // Records are pending but no parent has been resolved yet.
        let parents: HashMap<usize, i64> = HashMap::new();
        assert!(take(&mut sync, &lists, &parents).is_none());
        assert!(!sync.is_in_flight(), "a held-back push raises no guard");

        // Once a parent resolves, its rows go out.
        let parents: HashMap<usize, i64> = [(0, 10)].into_iter().collect();
        let (id, batch) = take(&mut sync, &lists, &parents).expect("parent 0 resolved");
        assert_eq!(batch.len(), 1);
        sync.commit(id);
        assert_eq!(sync.acked(&0), 1);
    }

    #[test]
    fn a_duplicated_partition_key_emits_its_rows_once() {
        let mut sync = SupabaseDeltaSync::<ChildRow, usize, i64>::default();
        let parents: HashMap<usize, i64> = [(0, 10)].into_iter().collect();
        let lists = vec![(0, records(&["a", "b"])), (0, records(&["a", "b"]))];

        let (id, batch) = take(&mut sync, &lists, &parents).expect("two rows pending");
        assert_eq!(batch.len(), 2, "the repeated key contributes nothing new");

        sync.commit(id);
        assert_eq!(sync.acked(&0), 2);
        assert!(take(&mut sync, &lists, &parents).is_none(), "caught up");
    }

    #[test]
    fn a_shrunk_records_list_is_held_back_untouched() {
        let mut sync = SupabaseDeltaSync::<ChildRow, usize, i64>::default();
        let parents: HashMap<usize, i64> = [(0, 10), (1, 20)].into_iter().collect();
        let lists = vec![(0, records(&["a", "b"])), (1, records(&["c"]))];

        let (id, _) = take(&mut sync, &lists, &parents).expect("three rows");
        sync.commit(id);
        assert_eq!(sync.acked(&0), 2);

        // Partition 0's history was replaced with a shorter list without
        // reset(): it is held back rather than re-sending acked rows, while
        // partition 1's new row still goes out.
        let lists = vec![(0, records(&["x"])), (1, records(&["c", "d"]))];
        let (id, batch) = take(&mut sync, &lists, &parents).expect("partition 1's new row");
        assert_eq!(
            batch,
            vec![ChildRow {
                parent_id: 20,
                value: "d".to_string()
            }]
        );
        sync.commit(id);
        assert_eq!(sync.acked(&0), 2, "shrunk partition's mark is untouched");
        assert_eq!(sync.acked(&1), 2);

        // The shrunk partition alone raises no batch.
        let lists = vec![(0, records(&["x"]))];
        assert!(take(&mut sync, &lists, &parents).is_none());
        assert!(!sync.is_in_flight());
    }

    #[test]
    fn a_failed_batch_leaves_marks_untouched_and_retries() {
        let mut sync = SupabaseDeltaSync::<ChildRow, usize, i64>::default();
        let parents: HashMap<usize, i64> = [(0, 10)].into_iter().collect();
        let lists = vec![(0, records(&["a", "b"]))];

        let (id, batch) = take(&mut sync, &lists, &parents).expect("first attempt");
        assert_eq!(batch.len(), 2);

        // The write failed: the guard clears but no mark moves.
        sync.abort(id);
        assert!(!sync.is_in_flight());
        assert_eq!(sync.acked(&0), 0);

        // The next push carries the same rows again.
        let (retry_id, retry) = take(&mut sync, &lists, &parents).expect("retry");
        assert_eq!(retry, batch);
        sync.commit(retry_id);
        assert_eq!(sync.acked(&0), 2);
    }

    #[test]
    fn commit_with_nothing_in_flight_is_a_warned_no_op() {
        let mut sync = SupabaseDeltaSync::<ChildRow, usize, i64>::default();
        let parents: HashMap<usize, i64> = [(0, 10)].into_iter().collect();
        let lists = vec![(0, records(&["a"]))];

        let (id, _) = take(&mut sync, &lists, &parents).expect("batch");
        sync.commit(id);
        assert_eq!(sync.acked(&0), 1);

        // A duplicate outcome for the already-answered batch changes nothing.
        sync.commit(id);
        sync.abort(id);
        assert_eq!(sync.acked(&0), 1);
        assert!(!sync.is_in_flight());
    }

    #[test]
    fn an_outcome_with_a_stale_batch_id_is_ignored() {
        let mut sync = SupabaseDeltaSync::<ChildRow, usize, i64>::default();
        let parents: HashMap<usize, i64> = [(0, 10)].into_iter().collect();
        let lists = vec![(0, records(&["a"]))];

        let (stale_id, _) = take(&mut sync, &lists, &parents).expect("first attempt");
        sync.abort(stale_id);
        let (retry_id, _) = take(&mut sync, &lists, &parents).expect("retry");
        assert_ne!(stale_id, retry_id);

        // A late outcome from the aborted attempt must not touch the retry.
        sync.commit(stale_id);
        assert!(sync.is_in_flight());
        assert_eq!(sync.acked(&0), 0);
        sync.abort(stale_id);
        assert!(sync.is_in_flight());

        sync.commit(retry_id);
        assert!(!sync.is_in_flight());
        assert_eq!(sync.acked(&0), 1);
    }

    #[test]
    fn unpartitioned_tables_use_take_pending() {
        #[derive(Clone, Serialize, Debug, PartialEq)]
        struct FlatRow {
            value: String,
        }
        impl SupabaseRow for FlatRow {
            type Response = PrimaryKeyResponse;
            fn table_name() -> &'static str {
                "flat"
            }
            fn primary_key_column() -> &'static str {
                "id"
            }
            fn unique_columns() -> &'static [&'static str] {
                &[]
            }
        }

        let mut sync = SupabaseDeltaSync::<FlatRow, (), ()>::default();
        let list = records(&["a", "b"]);

        let (id, batch) = sync
            .take_pending(&list, |value| FlatRow {
                value: value.clone(),
            })
            .expect("two rows pending");
        assert_eq!(batch.len(), 2);
        sync.commit(id);

        assert!(
            sync.take_pending(&list, |value| FlatRow {
                value: value.clone(),
            })
            .is_none(),
            "caught up"
        );
    }

    #[test]
    fn reset_forgets_marks_and_in_flight_state() {
        let mut sync = SupabaseDeltaSync::<ChildRow, usize, i64>::default();
        let parents: HashMap<usize, i64> = [(0, 10)].into_iter().collect();
        let lists = vec![(0, records(&["a"]))];

        let (stale_id, _) = take(&mut sync, &lists, &parents).expect("batch");
        sync.reset();

        assert!(!sync.is_in_flight());
        assert_eq!(sync.acked(&0), 0);
        // Everything goes out again from the start, under a fresh id the
        // pre-reset outcome cannot match.
        let (id, batch) = take(&mut sync, &lists, &parents).expect("batch after reset");
        assert_ne!(stale_id, id);
        assert_eq!(batch.len(), 1);
    }

    #[test]
    fn delta_sync_is_a_bevy_resource() {
        fn assert_resource<T: Resource>() {}
        assert_resource::<SupabaseDeltaSync<ChildRow, usize, i64>>();
        assert_resource::<SupabaseDeltaSync<ChildRow>>();
    }
}
