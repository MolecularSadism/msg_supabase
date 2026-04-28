//! Bevy plugin for Supabase synchronization.

use bevy::log::{info, trace, warn};
use bevy::prelude::*;
use std::marker::PhantomData;

use crate::config::{SaveMode, SyncConfig};
use crate::event::{SyncComplete, SyncError, SyncToSupabase};
use crate::queue::{SyncOutcome, SyncResultQueue};
use crate::request::{SupabaseConnection, execute_sync};
use crate::state::SyncState;
use crate::traits::SupabaseRow;

/// Resource holding the Supabase connection and configuration for a type.
#[derive(Resource)]
pub struct SupabaseConfig<T: SupabaseRow> {
    /// Connection details (URL and API key).
    pub connection: SupabaseConnection,

    /// Sync configuration (save mode, etc.).
    pub config: SyncConfig,

    _marker: PhantomData<T>,
}

impl<T: SupabaseRow> SupabaseConfig<T> {
    /// Create a new config with the given connection and sync settings.
    pub fn new(connection: SupabaseConnection, config: SyncConfig) -> Self {
        Self {
            connection,
            config,
            _marker: PhantomData,
        }
    }
}

/// Plugin that enables Supabase synchronization for a specific data type.
///
/// # Type Parameters
///
/// - `T`: The data type to sync. Must implement `SupabaseRow`.
///
/// # Example
///
/// ```rust
/// use bevy::prelude::*;
/// use msg_supabase::prelude::*;
/// use serde::Serialize;
///
/// #[derive(Clone, Serialize)]
/// struct GameStats {
///     game_id: i64,
///     kills: u32,
///     score: u32,
/// }
///
/// impl SupabaseRow for GameStats {
///     fn table_name() -> &'static str { "game_stats" }
///     fn primary_key_column() -> &'static str { "id" }
///     fn unique_columns() -> &'static [&'static str] { &["game_id"] }
/// }
///
/// fn setup_app() {
///     App::new()
///         .add_plugins(MinimalPlugins)
///         .add_plugins(
///             SupabasePlugin::<GameStats>::new(
///                 "https://your-project.supabase.co",
///                 "your-anon-key",
///             )
///             .with_save_mode(SaveMode::Update)
///         );
/// }
/// ```
pub struct SupabasePlugin<T: SupabaseRow> {
    connection: SupabaseConnection,
    config: SyncConfig,
    _marker: PhantomData<T>,
}

impl<T: SupabaseRow> SupabasePlugin<T> {
    /// Create a new plugin with the given Supabase URL and API key.
    ///
    /// Uses default configuration (Insert mode).
    pub fn new(url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            connection: SupabaseConnection::new(url, api_key),
            config: SyncConfig::default(),
            _marker: PhantomData,
        }
    }

    /// Set the save mode for this type.
    #[must_use]
    pub fn with_save_mode(mut self, mode: SaveMode) -> Self {
        self.config.save_mode = mode;
        self
    }

    /// Set a custom sync configuration.
    #[must_use]
    pub fn with_config(mut self, config: SyncConfig) -> Self {
        self.config = config;
        self
    }

    /// Override the table name for this type.
    #[must_use]
    pub fn with_table(mut self, table: impl Into<String>) -> Self {
        self.config.table_override = Some(table.into());
        self
    }

    /// Set whether to return representation after sync.
    #[must_use]
    pub fn with_return_representation(mut self, return_repr: bool) -> Self {
        self.config.return_representation = return_repr;
        self
    }
}

impl<T: SupabaseRow> Plugin for SupabasePlugin<T> {
    fn build(&self, app: &mut App) {
        if !self.connection.url.starts_with("https://") {
            warn!(
                "msg_supabase: URL '{}' does not use HTTPS — API keys will be transmitted in plaintext",
                self.connection.url
            );
        }

        app.init_resource::<SyncState<T>>();
        app.init_resource::<SyncResultQueue<T>>();
        app.insert_resource(SupabaseConfig::<T>::new(
            self.connection.clone(),
            self.config.clone(),
        ));

        app.add_observer(on_sync_to_supabase::<T>);
        app.add_systems(Update, poll_sync_results::<T>);
    }
}

/// Observer that handles `SyncToSupabase<T>` events.
fn on_sync_to_supabase<T: SupabaseRow>(
    trigger: On<SyncToSupabase<T>>,
    supabase_config: Option<Res<SupabaseConfig<T>>>,
    sync_state: Option<Res<SyncState<T>>>,
    queue: Option<Res<SyncResultQueue<T>>>,
) {
    let Some(supabase_config) = supabase_config else {
        warn!(
            "SyncToSupabase<{}> triggered but SupabaseConfig not found. \
             Did you forget to add SupabasePlugin<{}>?",
            T::table_name(),
            T::table_name()
        );
        return;
    };

    let Some(queue) = queue else {
        return;
    };

    let event = trigger.event();
    let data = &event.data;
    let sync_key = event.effective_sync_key();

    let (has_primary_key, primary_key) = if let Some(ref state) = sync_state {
        // Skip Insert-mode duplicates
        if let Some(ref key) = sync_key
            && supabase_config.config.save_mode == SaveMode::Insert
            && state.is_synced(key)
        {
            trace!(
                "Skipping sync for {} - key '{}' already synced",
                T::table_name(),
                key
            );
            return;
        }
        (state.initial_insert_done(), state.primary_key())
    } else {
        (false, None)
    };

    let table_name = T::table_name().to_string();
    let sender = queue.sender();

    execute_sync(
        &supabase_config.connection,
        data,
        &supabase_config.config,
        has_primary_key,
        primary_key,
        move |result, was_insert| {
            let outcome = match result {
                Ok(primary_key) => {
                    if was_insert {
                        if let Some(pk) = primary_key {
                            info!("Inserted {} to Supabase (pk: {})", table_name, pk);
                        } else {
                            info!("Inserted {} to Supabase", table_name);
                        }
                    } else {
                        info!("Updated {} in Supabase", table_name);
                    }
                    // status 200/201 — we don't have the raw status here, use 200 as sentinel
                    SyncOutcome::Success {
                        primary_key,
                        was_insert,
                        status: if was_insert { 201 } else { 200 },
                        sync_key,
                    }
                }
                Err(err) => {
                    warn!(
                        "Failed to sync {} to Supabase: {} (status: {:?})",
                        table_name, err.message, err.status
                    );
                    if let Some(ref body) = err.body {
                        trace!("Response body: {}", body);
                    }
                    SyncOutcome::Failure(err)
                }
            };

            if let Ok(mut queue) = sender.lock() {
                queue.push_back(outcome);
            }
        },
    );

    trace!(
        "Initiated Supabase sync for {} (mode: {:?})",
        T::table_name(),
        supabase_config.config.save_mode
    );
}

/// Drains the result queue each frame, updates `SyncState`, and fires outcome events.
fn poll_sync_results<T: SupabaseRow>(
    queue: Res<SyncResultQueue<T>>,
    mut state: ResMut<SyncState<T>>,
    mut commands: Commands,
) {
    let Ok(mut locked) = queue.0.try_lock() else {
        return;
    };

    while let Some(outcome) = locked.pop_front() {
        match outcome {
            SyncOutcome::Success {
                primary_key,
                was_insert,
                status,
                sync_key,
            } => {
                if was_insert {
                    if let Some(pk) = primary_key {
                        state.set_primary_key(pk);
                    } else {
                        state.mark_initial_insert_done();
                    }
                }
                if let Some(key) = sync_key {
                    state.mark_synced(key);
                } else {
                    state.increment_sync_count();
                }
                commands.trigger(SyncComplete::<T>::new(primary_key, was_insert, status));
            }
            SyncOutcome::Failure(err) => {
                let event = if let Some(status) = err.status {
                    SyncError::<T>::with_response(err.message, status, err.body)
                } else {
                    SyncError::<T>::new(err.message)
                };
                commands.trigger(event);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[derive(Clone, Serialize, Debug)]
    struct TestRecord {
        session_id: i64,
        value: String,
    }

    impl SupabaseRow for TestRecord {
        fn table_name() -> &'static str {
            "test_records"
        }
        fn primary_key_column() -> &'static str {
            "id"
        }
        fn unique_columns() -> &'static [&'static str] {
            &["session_id"]
        }
    }

    #[test]
    fn test_plugin_builder() {
        let plugin = SupabasePlugin::<TestRecord>::new("https://test.supabase.co", "key123")
            .with_save_mode(SaveMode::Update)
            .with_table("custom_table")
            .with_return_representation(false);

        assert_eq!(plugin.config.save_mode, SaveMode::Update);
        assert_eq!(
            plugin.config.table_override,
            Some("custom_table".to_string())
        );
        assert!(!plugin.config.return_representation);
    }

    #[test]
    fn test_supabase_config_creation() {
        let conn = SupabaseConnection::new("https://test.supabase.co", "key");
        let config = SyncConfig::new(SaveMode::Upsert);
        let supabase_config = SupabaseConfig::<TestRecord>::new(conn, config);

        assert_eq!(supabase_config.config.save_mode, SaveMode::Upsert);
        assert_eq!(supabase_config.connection.url, "https://test.supabase.co");
    }
}
