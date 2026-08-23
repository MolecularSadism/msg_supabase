//! Benchmarks for [`SupabaseDeltaSync::take_batch`]: the steady-state call
//! where every partition is caught up (which must stay allocation-light),
//! and a moderate batch of pending rows.

use std::collections::HashMap;
use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use msg_supabase::prelude::*;
use serde::Serialize;

#[derive(Clone, Serialize)]
struct ChildRow {
    parent_id: i64,
    value: u64,
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

const PARTITIONS: usize = 3;
const RECORDS_PER_PARTITION: usize = 1000;

fn fixtures() -> (Vec<(usize, Vec<u64>)>, HashMap<usize, i64>) {
    let lists = (0..PARTITIONS)
        .map(|p| {
            let base = (p * RECORDS_PER_PARTITION) as u64;
            (
                p,
                (0..RECORDS_PER_PARTITION as u64)
                    .map(|i| base + i)
                    .collect(),
            )
        })
        .collect();
    let parents = (0..PARTITIONS).map(|p| (p, p as i64 + 100)).collect();
    (lists, parents)
}

fn take(
    sync: &mut SupabaseDeltaSync<ChildRow, usize, i64>,
    lists: &[(usize, Vec<u64>)],
    parents: &HashMap<usize, i64>,
) -> Option<(BatchId, Vec<ChildRow>)> {
    sync.take_batch(
        lists.iter().map(|(p, r)| (*p, r.as_slice())),
        |p| parents.get(p).copied(),
        |&value, &parent_id| ChildRow { parent_id, value },
    )
}

fn bench_take_batch(c: &mut Criterion) {
    let (lists, parents) = fixtures();

    c.bench_function("take_batch/steady_state_none", |b| {
        let mut sync = SupabaseDeltaSync::<ChildRow, usize, i64>::default();
        let (id, _) = take(&mut sync, &lists, &parents).expect("initial batch");
        sync.commit(id);
        b.iter(|| {
            let none = take(&mut sync, black_box(&lists), &parents);
            debug_assert!(none.is_none());
            black_box(none)
        });
    });

    c.bench_function("take_batch/3x1000_pending", |b| {
        let mut sync = SupabaseDeltaSync::<ChildRow, usize, i64>::default();
        b.iter(|| {
            let (id, rows) = take(&mut sync, black_box(&lists), &parents).expect("pending rows");
            sync.abort(id);
            black_box(rows)
        });
    });
}

criterion_group!(benches, bench_take_batch);
criterion_main!(benches);
