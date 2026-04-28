//! Events for triggering and responding to Supabase sync operations.

use bevy::prelude::*;
use std::marker::PhantomData;

use crate::traits::SupabaseRow;

/// Event to trigger a sync operation for a specific data type.
///
/// Trigger this event to send data to Supabase. The plugin will handle
/// the HTTP request based on the configured `SaveMode`.
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
///     fn table_name() -> &'static str { "scores" }
///     fn primary_key_column() -> &'static str { "id" }
///     fn unique_columns() -> &'static [&'static str] { &["player_id"] }
/// }
///
/// fn save_score(mut commands: Commands) {
///     let score = PlayerScore { player_id: 1, score: 100 };
///     commands.trigger(SyncToSupabase::new(score));
/// }
/// ```
#[derive(Event, Debug, Clone)]
pub struct SyncToSupabase<T: SupabaseRow> {
    /// The data to sync.
    pub data: T,

    /// Optional sync key for deduplication.
    /// If not provided, uses `SupabaseRow::sync_key()`.
    pub sync_key: Option<String>,
}

impl<T: SupabaseRow> SyncToSupabase<T> {
    /// Create a new sync event with the given data.
    pub fn new(data: T) -> Self {
        Self {
            data,
            sync_key: None,
        }
    }

    /// Create a sync event with a custom sync key for deduplication.
    pub fn with_key(data: T, key: impl Into<String>) -> Self {
        Self {
            data,
            sync_key: Some(key.into()),
        }
    }

    /// Get the effective sync key (custom or from trait).
    pub fn effective_sync_key(&self) -> Option<String> {
        self.sync_key.clone().or_else(|| self.data.sync_key())
    }
}

/// Event fired when a sync operation completes successfully.
///
/// Contains information about the sync result, including any
/// primary key returned from Supabase.
#[derive(Event, Debug, Clone)]
pub struct SyncComplete<T: SupabaseRow> {
    /// The primary key returned from Supabase (if any).
    pub primary_key: Option<i64>,

    /// Whether this was an insert (true) or update (false).
    pub was_insert: bool,

    /// HTTP status code from the response.
    pub status: u16,

    _marker: PhantomData<T>,
}

impl<T: SupabaseRow> SyncComplete<T> {
    /// Create a new sync complete event.
    pub fn new(primary_key: Option<i64>, was_insert: bool, status: u16) -> Self {
        Self {
            primary_key,
            was_insert,
            status,
            _marker: PhantomData,
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[derive(Clone, Serialize, Debug, PartialEq)]
    struct TestData {
        value: i32,
    }

    impl SupabaseRow for TestData {
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
        assert_eq!(event.data, data);
        assert!(event.sync_key.is_none());
    }

    #[test]
    fn test_sync_event_with_key() {
        let data = TestData { value: 42 };
        let event = SyncToSupabase::with_key(data.clone(), "custom_key");
        assert_eq!(event.sync_key, Some("custom_key".to_string()));
    }

    #[test]
    fn test_effective_sync_key_custom() {
        let data = TestData { value: 42 };
        let event = SyncToSupabase::with_key(data, "custom");
        assert_eq!(event.effective_sync_key(), Some("custom".to_string()));
    }

    #[test]
    fn test_effective_sync_key_trait() {
        let data = TestData { value: 42 };
        let event = SyncToSupabase::new(data);
        assert_eq!(event.effective_sync_key(), Some("test_42".to_string()));
    }

    #[test]
    fn test_sync_complete() {
        let event = SyncComplete::<TestData>::new(Some(123), true, 201);
        assert_eq!(event.primary_key, Some(123));
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
