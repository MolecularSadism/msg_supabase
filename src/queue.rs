//! Channel bridge between ehttp async callbacks and the Bevy World.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use bevy::prelude::*;

use crate::error::RequestError;
use crate::traits::{SupabaseRow, SupabaseView};

/// A queue that HTTP callbacks push into and a Bevy system drains.
///
/// `ehttp` callbacks run off the main thread and have no World access, so they
/// hand their results to a queue instead; a system drains it each frame and
/// applies the results where the World is available.
///
/// # Example
///
/// ```rust
/// use bevy::prelude::*;
/// use msg_supabase::prelude::*;
///
/// #[derive(Resource, Default)]
/// struct ScoreInbox(ResultQueue<u32>);
///
/// let mut app = App::new();
/// app.add_plugins(MinimalPlugins);
/// app.init_resource::<ScoreInbox>();
///
/// // A sender is `Clone + Send`, so it can move into an HTTP callback.
/// let sender = app.world().resource::<ScoreInbox>().0.sender();
/// sender.send(42);
///
/// assert_eq!(app.world().resource::<ScoreInbox>().0.drain(), vec![42]);
/// ```
#[derive(Resource)]
pub struct ResultQueue<T: Send + 'static> {
    inner: Arc<Mutex<VecDeque<T>>>,
}

impl<T: Send + 'static> Default for ResultQueue<T> {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::new())),
        }
    }
}

impl<T: Send + 'static> Clone for ResultQueue<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T: Send + 'static> ResultQueue<T> {
    /// Create an empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Get a sender that can move into an HTTP callback.
    pub fn sender(&self) -> QueueSender<T> {
        QueueSender {
            inner: self.inner.clone(),
        }
    }

    /// Push a value onto the queue.
    pub fn send(&self, value: T) {
        if let Ok(mut queue) = self.inner.lock() {
            queue.push_back(value);
        }
    }

    /// Take everything queued so far, in the order it arrived.
    ///
    /// Returns nothing while a callback holds the queue; those values are
    /// drained by a later call.
    pub fn drain(&self) -> Vec<T> {
        let Ok(mut queue) = self.inner.try_lock() else {
            return Vec::new();
        };
        queue.drain(..).collect()
    }

    /// Whether the queue currently holds no values.
    ///
    /// Reports `true` while a callback holds the queue.
    pub fn is_empty(&self) -> bool {
        self.inner.try_lock().is_ok_and(|queue| queue.is_empty())
    }
}

/// The push half of a [`ResultQueue`], cheap to clone into HTTP callbacks.
pub struct QueueSender<T: Send + 'static> {
    inner: Arc<Mutex<VecDeque<T>>>,
}

impl<T: Send + 'static> Clone for QueueSender<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T: Send + 'static> QueueSender<T> {
    /// Push a value onto the queue.
    ///
    /// The value is dropped if a consumer panicked while holding the queue.
    pub fn send(&self, value: T) {
        if let Ok(mut queue) = self.inner.lock() {
            queue.push_back(value);
        }
    }
}

/// Outcome of a single sync operation, queued by the ehttp callback and
/// drained each frame by `poll_sync_results`.
pub(crate) enum SyncOutcome<T: SupabaseRow> {
    Success {
        /// Rows the server returned, empty when no response body was asked for.
        rows: Vec<T::Response>,
        /// Primary keys the server assigned, in the order they came back.
        primary_keys: Vec<i64>,
        was_insert: bool,
        /// HTTP status from the response.
        status: u16,
        /// Sync keys of the rows that were written, marked done in `SyncState`.
        sync_keys: Vec<String>,
    },
    Failure(RequestError),
}

/// Per-row-type queue of [`SyncOutcome`]s, held as its own resource so every
/// registered `SupabaseRow` drains independently.
#[derive(Resource)]
pub(crate) struct SyncResultQueue<T: SupabaseRow> {
    queue: ResultQueue<SyncOutcome<T>>,
}

impl<T: SupabaseRow> Default for SyncResultQueue<T> {
    fn default() -> Self {
        Self {
            queue: ResultQueue::new(),
        }
    }
}

impl<T: SupabaseRow> SyncResultQueue<T> {
    /// Get a sender for use in an ehttp callback.
    pub(crate) fn sender(&self) -> QueueSender<SyncOutcome<T>> {
        self.queue.sender()
    }

    /// Take every outcome queued so far.
    pub(crate) fn drain(&self) -> Vec<SyncOutcome<T>> {
        self.queue.drain()
    }
}

/// Outcome of a single view read.
pub(crate) enum ViewOutcome<R: SupabaseView> {
    Rows(Vec<R>),
    Failure(RequestError),
}

/// Per-view queue of [`ViewOutcome`]s.
#[derive(Resource)]
pub(crate) struct ViewResultQueue<R: SupabaseView> {
    queue: ResultQueue<ViewOutcome<R>>,
}

impl<R: SupabaseView> Default for ViewResultQueue<R> {
    fn default() -> Self {
        Self {
            queue: ResultQueue::new(),
        }
    }
}

impl<R: SupabaseView> ViewResultQueue<R> {
    /// Get a sender for use in an ehttp callback.
    pub(crate) fn sender(&self) -> QueueSender<ViewOutcome<R>> {
        self.queue.sender()
    }

    /// Take every outcome queued so far.
    pub(crate) fn drain(&self) -> Vec<ViewOutcome<R>> {
        self.queue.drain()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drains_in_arrival_order() {
        let queue: ResultQueue<u8> = ResultQueue::new();
        queue.send(1);
        queue.sender().send(2);
        queue.send(3);

        assert_eq!(queue.drain(), vec![1, 2, 3]);
        assert!(queue.drain().is_empty());
    }

    #[test]
    fn senders_share_one_queue() {
        let queue: ResultQueue<&str> = ResultQueue::new();
        let first = queue.sender();
        let second = first.clone();

        first.send("a");
        second.send("b");

        assert_eq!(queue.drain(), vec!["a", "b"]);
    }

    #[test]
    fn empty_until_a_value_arrives() {
        let queue: ResultQueue<u8> = ResultQueue::new();
        assert!(queue.is_empty());
        queue.send(7);
        assert!(!queue.is_empty());
    }
}
