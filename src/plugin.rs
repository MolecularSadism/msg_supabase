//! Bevy plugin for Supabase synchronization.

use bevy::log::{info, trace, warn};
use bevy::prelude::*;
use std::marker::PhantomData;

use crate::config::{SaveMode, SyncConfig};
use crate::event::{SyncComplete, SyncError, SyncToSupabase};
use crate::queue::{SyncOutcome, SyncResultQueue};
use crate::request::{
    SupabaseConnection, WriteOptions, WriteResponse, execute_update, execute_write_returning,
};
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

    /// The write options this configuration implies for `T`.
    ///
    /// Upserts resolve on [`SupabaseRow::unique_columns`], and the returned
    /// columns come from [`SupabaseRow::returning`].
    pub fn write_options(&self) -> WriteOptions {
        let mut options = WriteOptions::new();

        if self.config.save_mode != SaveMode::Insert {
            options = options.on_conflict(T::unique_columns().iter().copied());
        }

        match T::returning() {
            Some(columns) => options.returning(columns),
            None if self.config.return_representation => options.returning_all(),
            None => options,
        }
    }
}

/// Plugin that enables Supabase synchronization for a specific data type.
///
/// Registering the plugin for a type is all it takes to sync it: trigger
/// [`SyncToSupabase<T>`] with one row or a batch, and read the result from
/// [`SyncComplete<T>`], whose returned rows carry the primary keys a dependent
/// table needs as foreign keys.
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
///     type Response = PrimaryKeyResponse;
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
///             .with_save_mode(SaveMode::Upsert)
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
    let save_mode = supabase_config.config.save_mode;

    // In Insert mode, rows whose key was written before would duplicate.
    let mut rows: Vec<T> = Vec::with_capacity(event.rows.len());
    let mut sync_keys: Vec<String> = Vec::new();
    for (index, row) in event.rows.iter().enumerate() {
        let key = event.effective_sync_key(index);
        if let Some(ref key) = key
            && save_mode == SaveMode::Insert
            && sync_state
                .as_ref()
                .is_some_and(|state| state.is_synced(key))
        {
            trace!(
                "Skipping sync for {} - key '{}' already synced",
                T::table_name(),
                key
            );
            continue;
        }
        rows.push(row.clone());
        sync_keys.extend(key);
    }

    if rows.is_empty() {
        trace!("Nothing new to sync for {}", T::table_name());
        return;
    }

    let table = supabase_config
        .config
        .table_override
        .clone()
        .unwrap_or_else(|| T::table_name().to_string());
    let table_name = table.clone();
    let row_count = rows.len();
    let sender = queue.sender();

    // Update mode keeps one row per session: insert once, then patch it.
    let primary_key = sync_state.as_ref().and_then(|state| state.primary_key());
    if save_mode == SaveMode::Update
        && let Some(primary_key) = primary_key
        && let Some(row) = rows.first()
        && rows.len() == 1
    {
        execute_update(
            &supabase_config.connection,
            row,
            &table,
            primary_key,
            T::primary_key_column(),
            move |result| match result {
                Ok(_) => {
                    info!("Updated {} in Supabase", table_name);
                    sender.send(SyncOutcome::Success {
                        rows: Vec::new(),
                        primary_keys: vec![primary_key],
                        was_insert: false,
                        status: 200,
                        sync_keys,
                    });
                }
                Err(err) => {
                    warn!(
                        "Failed to sync {} to Supabase: {} (status: {:?})",
                        table_name, err.message, err.status
                    );
                    sender.send(SyncOutcome::Failure(err));
                }
            },
        );
        return;
    }

    execute_write_returning(
        &supabase_config.connection,
        &rows,
        &table,
        &supabase_config.write_options(),
        move |result: Result<WriteResponse<T::Response>, _>| match result {
            Ok(response) => {
                let primary_keys = response.primary_keys;
                info!(
                    "Wrote {} row(s) to {} in Supabase{}",
                    row_count,
                    table_name,
                    match primary_keys.first() {
                        Some(pk) => format!(" (pk: {pk})"),
                        None => String::new(),
                    }
                );
                sender.send(SyncOutcome::Success {
                    rows: response.rows,
                    primary_keys,
                    was_insert: true,
                    status: response.status,
                    sync_keys,
                });
            }
            Err(err) => {
                warn!(
                    "Failed to sync {} to Supabase: {} (status: {:?})",
                    table_name, err.message, err.status
                );
                if let Some(ref body) = err.body {
                    trace!("Response body: {}", body);
                }
                sender.send(SyncOutcome::Failure(err));
            }
        },
    );

    trace!(
        "Initiated Supabase sync for {} ({} row(s), mode: {:?})",
        T::table_name(),
        row_count,
        save_mode
    );
}

/// Drains the result queue each frame, updates `SyncState`, and fires outcome events.
fn poll_sync_results<T: SupabaseRow>(
    queue: Res<SyncResultQueue<T>>,
    mut state: ResMut<SyncState<T>>,
    mut commands: Commands,
) {
    for outcome in queue.drain() {
        match outcome {
            SyncOutcome::Success {
                rows,
                primary_keys,
                was_insert,
                status,
                sync_keys,
            } => {
                if was_insert {
                    match primary_keys.first() {
                        Some(pk) => state.set_primary_key(*pk),
                        None => state.mark_initial_insert_done(),
                    }
                }
                if sync_keys.is_empty() {
                    state.increment_sync_count();
                } else {
                    for key in sync_keys {
                        state.mark_synced(key);
                    }
                }
                commands.trigger(SyncComplete::<T>::new(
                    rows,
                    primary_keys,
                    was_insert,
                    status,
                ));
            }
            SyncOutcome::Failure(err) => {
                let event = match err.status {
                    Some(status) => SyncError::<T>::with_response(err.message, status, err.body),
                    None => SyncError::<T>::new(err.message),
                };
                commands.trigger(event);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::PrimaryKeyResponse;
    use serde::Serialize;

    #[derive(Clone, Serialize, Debug)]
    struct TestRecord {
        session_id: i64,
        value: String,
    }

    impl SupabaseRow for TestRecord {
        type Response = PrimaryKeyResponse;

        fn table_name() -> &'static str {
            "test_records"
        }
        fn primary_key_column() -> &'static str {
            "id"
        }
        fn unique_columns() -> &'static [&'static str] {
            &["session_id"]
        }
        fn returning() -> Option<&'static str> {
            Some("id")
        }
        fn sync_key(&self) -> Option<String> {
            Some(format!("record_{}", self.session_id))
        }
    }

    fn record(session_id: i64) -> TestRecord {
        TestRecord {
            session_id,
            value: "test".to_string(),
        }
    }

    fn config(save_mode: SaveMode) -> SupabaseConfig<TestRecord> {
        SupabaseConfig::new(
            SupabaseConnection::new("https://test.supabase.co", "key"),
            SyncConfig::new(save_mode),
        )
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
        let supabase_config = config(SaveMode::Upsert);

        assert_eq!(supabase_config.config.save_mode, SaveMode::Upsert);
        assert_eq!(supabase_config.connection.url, "https://test.supabase.co");
    }

    #[test]
    fn insert_mode_writes_without_conflict_columns() {
        let options = config(SaveMode::Insert).write_options();
        assert!(!options.is_upsert());
        assert_eq!(options.query().to_query_string(), "select=id");
    }

    #[test]
    fn upsert_mode_takes_conflict_columns_from_the_row_type() {
        let options = config(SaveMode::Upsert).write_options();
        assert!(options.is_upsert());
        assert_eq!(
            options.query().to_query_string(),
            "on_conflict=session_id&select=id"
        );
    }

    #[test]
    fn the_plugin_registers_its_resources_and_observer() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(SupabasePlugin::<TestRecord>::new(
            "https://test.supabase.co",
            "key",
        ));
        app.update();

        assert!(
            app.world()
                .get_resource::<SyncState<TestRecord>>()
                .is_some()
        );
        assert!(
            app.world()
                .get_resource::<SupabaseConfig<TestRecord>>()
                .is_some()
        );
    }

    #[test]
    fn already_synced_rows_are_not_sent_again() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(SupabasePlugin::<TestRecord>::new(
            "https://test.supabase.co",
            "key",
        ));

        // Pretend the first record already reached the database.
        app.world_mut()
            .resource_mut::<SyncState<TestRecord>>()
            .mark_synced("record_1".to_string());

        // A sync of only that record queues no request, so no outcome arrives.
        app.world_mut().trigger(SyncToSupabase::new(record(1)));
        app.update();

        assert_eq!(
            app.world().resource::<SyncState<TestRecord>>().sync_count(),
            1
        );
    }

    #[test]
    fn outcomes_reach_the_sync_state_and_fire_completion() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(SupabasePlugin::<TestRecord>::new(
            "https://test.supabase.co",
            "key",
        ));
        app.init_resource::<CompletedRows>();
        app.add_observer(
            |trigger: On<SyncComplete<TestRecord>>, mut seen: ResMut<CompletedRows>| {
                seen.0 = trigger.primary_keys.clone();
            },
        );

        app.world()
            .resource::<SyncResultQueue<TestRecord>>()
            .sender()
            .send(SyncOutcome::Success {
                rows: Vec::new(),
                primary_keys: vec![7, 8],
                was_insert: true,
                status: 201,
                sync_keys: vec!["record_1".to_string()],
            });
        app.update();

        let state = app.world().resource::<SyncState<TestRecord>>();
        assert_eq!(state.primary_key(), Some(7));
        assert!(state.is_synced("record_1"));
        assert_eq!(app.world().resource::<CompletedRows>().0, vec![7, 8]);
    }

    #[derive(Resource, Default)]
    struct CompletedRows(Vec<i64>);
}
