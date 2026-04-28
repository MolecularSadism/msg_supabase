//! Configuration types for Supabase sync behavior.

/// Defines how data should be saved to Supabase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SaveMode {
    /// Always insert new rows.
    ///
    /// Each sync creates a new database entry. Use this for:
    /// - Event logs
    /// - Kill records
    /// - Any data that should create unique entries
    #[default]
    Insert,

    /// Insert on first sync, update on subsequent syncs.
    ///
    /// The first sync for a session inserts a new row and retrieves
    /// the primary key. All subsequent syncs update that same row.
    /// Use this for:
    /// - Session data that accumulates over time
    /// - Player stats that should be updated
    /// - Any data where you want one row per session
    Update,

    /// Always upsert (insert or update based on unique columns).
    ///
    /// Uses Supabase's `ON CONFLICT` mechanism to either insert
    /// a new row or update an existing one based on unique columns.
    Upsert,
}

/// Configuration for how a type should be synced to Supabase.
#[derive(Debug, Clone)]
pub struct SyncConfig {
    /// How to save data (insert, update, or upsert).
    pub save_mode: SaveMode,

    /// Whether to return the row representation after insert/update.
    ///
    /// Set to `true` to receive the database-assigned primary key.
    pub return_representation: bool,

    /// Custom table name override.
    ///
    /// If `None`, uses `SupabaseRow::table_name()`.
    pub table_override: Option<String>,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            save_mode: SaveMode::Insert,
            return_representation: true,
            table_override: None,
        }
    }
}

impl SyncConfig {
    /// Create a new config with the specified save mode.
    #[must_use]
    pub fn new(save_mode: SaveMode) -> Self {
        Self {
            save_mode,
            ..Default::default()
        }
    }

    /// Set the save mode.
    #[must_use]
    pub fn with_save_mode(mut self, mode: SaveMode) -> Self {
        self.save_mode = mode;
        self
    }

    /// Set whether to return representation after save.
    #[must_use]
    pub fn with_return_representation(mut self, return_repr: bool) -> Self {
        self.return_representation = return_repr;
        self
    }

    /// Override the table name.
    #[must_use]
    pub fn with_table(mut self, table: impl Into<String>) -> Self {
        self.table_override = Some(table.into());
        self
    }
}
