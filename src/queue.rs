//! Channel bridge between ehttp async callbacks and the Bevy World.

use std::collections::VecDeque;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use bevy::prelude::*;

use crate::error::RequestError;
use crate::traits::SupabaseRow;

/// Outcome of a single sync operation, queued by the ehttp callback and
/// drained each frame by `poll_sync_results`.
pub(crate) enum SyncOutcome {
    Success {
        primary_key: Option<i64>,
        was_insert: bool,
        /// HTTP status from the response.
        status: u16,
        /// Sync key to mark as done in `SyncState`, if deduplication was used.
        sync_key: Option<String>,
    },
    Failure(RequestError),
}

/// Shared queue that ehttp callbacks push into and a Bevy system drains.
///
/// The inner `Arc<Mutex<...>>` is `Clone + Send + 'static`, so it can be
/// moved into ehttp callback closures safely.
#[derive(Resource, Clone)]
pub(crate) struct SyncResultQueue<T: SupabaseRow>(
    pub Arc<Mutex<VecDeque<SyncOutcome>>>,
    PhantomData<fn() -> T>,
);

impl<T: SupabaseRow> Default for SyncResultQueue<T> {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(VecDeque::new())), PhantomData)
    }
}

impl<T: SupabaseRow> SyncResultQueue<T> {
    /// Clone the inner `Arc` for use in an ehttp callback.
    pub(crate) fn sender(&self) -> Arc<Mutex<VecDeque<SyncOutcome>>> {
        self.0.clone()
    }
}
