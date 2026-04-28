//! A flexible Bevy plugin for syncing game data to Supabase.
//!
//! `msg_supabase` provides a generic, type-safe way to sync arbitrary data structures
//! to Supabase with configurable save modes (insert vs update).
//!
//! # Features
//!
//! - **Generic Plugin System**: Register any `Serialize` type for Supabase sync
//! - **Configurable Save Modes**:
//!   - `Insert`: Always creates new rows
//!   - `Update`: First save inserts, subsequent saves update the same row
//! - **Session-aware Sync State**: Tracks what has been synced to avoid duplicates
//! - **Primary Key Management**: Automatically retrieves and stores database IDs
//! - **Event-driven API**: Trigger syncs with `SyncToSupabase<T>` events;
//!     listen for `SyncComplete<T>` or `SyncError<T>` to react to outcomes
//!
//! # Example
//!
//! ```rust
//! use bevy::prelude::*;
//! use msg_supabase::prelude::*;
//! use serde::Serialize;
//!
//! #[derive(Resource, Clone, Serialize)]
//! struct PlayerStats {
//!     player_id: i64,
//!     kills: u32,
//!     deaths: u32,
//! }
//!
//! impl SupabaseRow for PlayerStats {
//!     fn table_name() -> &'static str { "player_stats" }
//!     fn primary_key_column() -> &'static str { "id" }
//!     fn unique_columns() -> &'static [&'static str] { &["player_id"] }
//! }
//!
//! fn setup_app() {
//!     App::new()
//!         .add_plugins(MinimalPlugins)
//!         .add_plugins(
//!             SupabasePlugin::<PlayerStats>::new(
//!                 "https://your-project.supabase.co",
//!                 "your-api-key",
//!             )
//!             .with_save_mode(SaveMode::Update)
//!         )
//!         .add_systems(Update, trigger_sync);
//! }
//!
//! fn trigger_sync(mut commands: Commands, stats: Res<PlayerStats>) {
//!     commands.trigger(SyncToSupabase::new(stats.clone()));
//! }
//! ```

mod config;
pub mod error;
mod event;
mod plugin;
pub(crate) mod queue;
pub mod request;
mod state;
mod traits;

pub use config::{SaveMode, SyncConfig};
pub use error::RequestError;
pub use event::{SyncComplete, SyncError, SyncToSupabase};
pub use plugin::SupabasePlugin;
pub use request::{SupabaseConnection, execute_insert};
pub use state::SyncState;
pub use traits::SupabaseRow;

/// Convenient imports for using `msg_supabase`.
pub mod prelude {
    pub use crate::config::{SaveMode, SyncConfig};
    pub use crate::error::RequestError;
    pub use crate::event::{SyncComplete, SyncError, SyncToSupabase};
    pub use crate::plugin::SupabasePlugin;
    pub use crate::request::{SupabaseConnection, execute_insert};
    pub use crate::state::SyncState;
    pub use crate::traits::SupabaseRow;
}

#[cfg(test)]
mod tests {
    use super::prelude::*;
    use bevy::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Serialize, Debug, PartialEq)]
    struct TestData {
        id: i64,
        value: String,
    }

    impl SupabaseRow for TestData {
        fn table_name() -> &'static str {
            "test_data"
        }
        fn primary_key_column() -> &'static str {
            "id"
        }
        fn unique_columns() -> &'static [&'static str] {
            &["id"]
        }
    }

    #[test]
    fn test_sync_config_default() {
        let config = SyncConfig::default();
        assert_eq!(config.save_mode, SaveMode::Insert);
    }

    #[test]
    fn test_sync_config_update_mode() {
        let config = SyncConfig::default().with_save_mode(SaveMode::Update);
        assert_eq!(config.save_mode, SaveMode::Update);
    }

    #[test]
    fn test_supabase_row_trait() {
        assert_eq!(TestData::table_name(), "test_data");
        assert_eq!(TestData::primary_key_column(), "id");
        assert_eq!(TestData::unique_columns(), &["id"]);
    }

    #[test]
    fn test_sync_to_supabase_event() {
        let data = TestData {
            id: 1,
            value: "test".to_string(),
        };
        let event = SyncToSupabase::new(data.clone());
        assert_eq!(event.data, data);
    }

    #[test]
    fn test_sync_state_default() {
        let state = SyncState::<TestData>::default();
        assert!(state.synced_keys().is_empty());
        assert!(state.primary_key().is_none());
    }

    #[test]
    fn test_sync_state_mark_synced() {
        let mut state = SyncState::<TestData>::default();
        state.mark_synced("key1".to_string());
        state.mark_synced("key2".to_string());
        assert!(state.is_synced("key1"));
        assert!(state.is_synced("key2"));
        assert!(!state.is_synced("key3"));
    }

    #[test]
    fn test_sync_state_set_primary_key() {
        let mut state = SyncState::<TestData>::default();
        assert!(state.primary_key().is_none());
        state.set_primary_key(42);
        assert_eq!(state.primary_key(), Some(42));
    }
}
