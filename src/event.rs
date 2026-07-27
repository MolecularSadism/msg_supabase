//! Events for triggering and responding to Supabase sync operations.

use bevy::prelude::*;
use std::marker::PhantomData;

use crate::traits::{SupabaseRow, SupabaseView};

/// Event to trigger a sync operation for a specific data type.
///
/// Trigger this event to send one row or a whole batch to Supabase. The plugin
/// handles the HTTP request based on the configured `SaveMode`.
///
/// # Example
///
/// ```rust
/// use bevy::prelude::*;
/// use msg_supabase::prelude::*;
/// use serde::Serialize;
///
/// #[derive(Clone, Serialize)]
/// struct PlayerScore {
///     player_id: i64,
///     score: u32,
/// }
///
/// impl SupabaseRow for PlayerScore {
///     type Response = PrimaryKeyResponse;
///     fn table_name() -> &'static str { "scores" }
///     fn primary_key_column() -> &'static str { "id" }
///     fn unique_columns() -> &'static [&'static str] { &["player_id"] }
/// }
///
/// fn save_score(mut commands: Commands) {
///     let score = PlayerScore { player_id: 1, score: 100 };
///     commands.trigger(SyncToSupabase::new(score));
/// }
///
/// fn save_every_score(mut commands: Commands, scores: Vec<PlayerScore>) {
///     commands.trigger(SyncToSupabase::batch(scores));
/// }
/// ```
#[derive(Event, Debug, Clone)]
pub struct SyncToSupabase<T: SupabaseRow> {
    /// The rows to sync, written in one request.
    pub rows: Vec<T>,

    /// Deduplication keys, one per row. Empty to fall back on each row's
    /// [`SupabaseRow::sync_key`].
    pub sync_keys: Vec<String>,
}

impl<T: SupabaseRow> SyncToSupabase<T> {
    /// Create a sync event for a single row.
    pub fn new(data: T) -> Self {
        Self {
            rows: vec![data],
            sync_keys: Vec::new(),
        }
    }

    /// Create a sync event for many rows, written in one request.
    pub fn batch(rows: Vec<T>) -> Self {
        Self {
            rows,
            sync_keys: Vec::new(),
        }
    }

    /// Create a single-row sync event with a custom sync key for deduplication.
    pub fn with_key(data: T, key: impl Into<String>) -> Self {
        Self {
            rows: vec![data],
            sync_keys: vec![key.into()],
        }
    }

    /// Create a batch whose rows carry their own deduplication keys.
    ///
    /// Use this when a row cannot identify itself from its columns alone — two
    /// enemies killed in the same frame look identical, but their position in
    /// the list does not. Rows past the end of `keys` fall back on
    /// [`SupabaseRow::sync_key`].
    pub fn batch_with_keys(rows: Vec<T>, keys: Vec<String>) -> Self {
        Self {
            rows,
            sync_keys: keys,
        }
    }

    /// The sync key of the row at `index`: the key given for it, otherwise the
    /// row's own.
    pub fn effective_sync_key(&self, index: usize) -> Option<String> {
        self.sync_keys
            .get(index)
            .cloned()
            .or_else(|| self.rows.get(index).and_then(SupabaseRow::sync_key))
    }
}

/// Event fired when a sync operation completes successfully.
///
/// Carries the rows the server returned, so a chained workflow can read the
/// primary keys its next table needs as foreign keys.
#[derive(Event, Debug, Clone)]
pub struct SyncComplete<T: SupabaseRow> {
    /// The rows Supabase returned, empty when the type asks for no response
    /// body (see [`SupabaseRow::returning`]).
    pub rows: Vec<T::Response>,

    /// The primary keys Supabase assigned, in the order they came back.
    pub primary_keys: Vec<i64>,

    /// Whether this was an insert (true) or update (false).
    pub was_insert: bool,

    /// HTTP status code from the response.
    pub status: u16,
}

impl<T: SupabaseRow> SyncComplete<T> {
    /// Create a new sync complete event.
    pub fn new(
        rows: Vec<T::Response>,
        primary_keys: Vec<i64>,
        was_insert: bool,
        status: u16,
    ) -> Self {
        Self {
            rows,
            primary_keys,
            was_insert,
            status,
        }
    }

    /// The first primary key returned, if any.
    pub fn primary_key(&self) -> Option<i64> {
        self.primary_keys.first().copied()
    }
}

/// Event fired when a sync operation fails.
#[derive(Event, Debug, Clone)]
pub struct SyncError<T: SupabaseRow> {
    /// Error message describing what went wrong.
    pub message: String,

    /// HTTP status code (if available).
    pub status: Option<u16>,

    /// Response body (if available).
    pub body: Option<String>,

    _marker: PhantomData<T>,
}

impl<T: SupabaseRow> SyncError<T> {
    /// Create a new sync error event.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status: None,
            body: None,
            _marker: PhantomData,
        }
    }

    /// Create a sync error with HTTP response details.
    pub fn with_response(message: impl Into<String>, status: u16, body: Option<String>) -> Self {
        Self {
            message: message.into(),
            status: Some(status),
            body,
            _marker: PhantomData,
        }
    }
}

/// Event to trigger a read of a view.
///
/// # Example
///
/// ```rust
/// use bevy::prelude::*;
/// use msg_supabase::prelude::*;
/// use serde::Deserialize;
///
/// #[derive(Deserialize)]
/// struct Highscore {
///     kills: i64,
/// }
///
/// impl SupabaseView for Highscore {
///     fn view_name() -> &'static str { "highscores" }
/// }
///
/// fn refresh_leaderboard(mut commands: Commands) {
///     commands.trigger(FetchView::<Highscore>::new());
/// }
/// ```
#[derive(Event, Debug)]
pub struct FetchView<R: SupabaseView> {
    _marker: PhantomData<R>,
}

impl<R: SupabaseView> FetchView<R> {
    /// Request a fresh read of the view.
    pub fn new() -> Self {
        Self {
            _marker: PhantomData,
        }
    }
}

impl<R: SupabaseView> Default for FetchView<R> {
    fn default() -> Self {
        Self::new()
    }
}

/// Event fired when a view read completes successfully.
#[derive(Event, Debug)]
pub struct ViewFetched<R: SupabaseView> {
    /// The rows read from the view.
    pub rows: Vec<R>,
}

/// Event fired when a view read fails.
#[derive(Event, Debug)]
pub struct ViewFetchFailed<R: SupabaseView> {
    /// Error message describing what went wrong.
    pub message: String,

    /// HTTP status code (if available).
    pub status: Option<u16>,

    _marker: PhantomData<R>,
}

impl<R: SupabaseView> ViewFetchFailed<R> {
    /// Create a new view fetch failure event.
    pub fn new(message: impl Into<String>, status: Option<u16>) -> Self {
        Self {
            message: message.into(),
            status,
            _marker: PhantomData,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::PrimaryKeyResponse;
    use serde::Serialize;

    #[derive(Clone, Serialize, Debug, PartialEq)]
    struct TestData {
        value: i32,
    }

    impl SupabaseRow for TestData {
        type Response = PrimaryKeyResponse;

        fn table_name() -> &'static str {
            "test"
        }
        fn primary_key_column() -> &'static str {
            "id"
        }
        fn unique_columns() -> &'static [&'static str] {
            &[]
        }
        fn sync_key(&self) -> Option<String> {
            Some(format!("test_{}", self.value))
        }
    }

    #[test]
    fn test_sync_event_new() {
        let data = TestData { value: 42 };
        let event = SyncToSupabase::new(data.clone());
        assert_eq!(event.rows, vec![data]);
        assert!(event.sync_keys.is_empty());
    }

    #[test]
    fn batch_events_carry_every_row() {
        let rows = vec![TestData { value: 1 }, TestData { value: 2 }];
        let event = SyncToSupabase::batch(rows.clone());
        assert_eq!(event.rows, rows);
    }

    #[test]
    fn test_sync_event_with_key() {
        let data = TestData { value: 42 };
        let event = SyncToSupabase::with_key(data.clone(), "custom_key");
        assert_eq!(event.sync_keys, vec!["custom_key".to_string()]);
    }

    #[test]
    fn test_effective_sync_key_custom() {
        let data = TestData { value: 42 };
        let event = SyncToSupabase::with_key(data, "custom");
        assert_eq!(event.effective_sync_key(0), Some("custom".to_string()));
    }

    #[test]
    fn test_effective_sync_key_trait() {
        let data = TestData { value: 42 };
        let event = SyncToSupabase::new(data);
        assert_eq!(event.effective_sync_key(0), Some("test_42".to_string()));
    }

    #[test]
    fn given_keys_win_over_the_rows_own() {
        let event = SyncToSupabase::batch_with_keys(
            vec![TestData { value: 1 }, TestData { value: 1 }],
            vec!["kill_0".to_string(), "kill_1".to_string()],
        );
        assert_eq!(event.effective_sync_key(0), Some("kill_0".to_string()));
        assert_eq!(event.effective_sync_key(1), Some("kill_1".to_string()));
    }

    #[test]
    fn batch_rows_keep_their_own_keys() {
        let event = SyncToSupabase::batch(vec![TestData { value: 1 }, TestData { value: 2 }]);
        assert_eq!(event.effective_sync_key(0), Some("test_1".to_string()));
        assert_eq!(event.effective_sync_key(1), Some("test_2".to_string()));
        assert_eq!(event.effective_sync_key(2), None);
    }

    #[test]
    fn test_sync_complete() {
        let event = SyncComplete::<TestData>::new(Vec::new(), vec![123], true, 201);
        assert_eq!(event.primary_key(), Some(123));
        assert!(event.was_insert);
        assert_eq!(event.status, 201);
    }

    #[test]
    fn test_sync_error() {
        let event = SyncError::<TestData>::new("Failed");
        assert_eq!(event.message, "Failed");
        assert!(event.status.is_none());
    }

    #[test]
    fn test_sync_error_with_response() {
        let event =
            SyncError::<TestData>::with_response("Bad request", 400, Some("Invalid JSON".into()));
        assert_eq!(event.message, "Bad request");
        assert_eq!(event.status, Some(400));
        assert_eq!(event.body, Some("Invalid JSON".to_string()));
    }
}
