# msg_supabase

A flexible [Bevy](https://bevyengine.org/) plugin for syncing game data to [Supabase](https://supabase.com/) with configurable save modes.

## Features

- **Generic Plugin System** — register any `Serialize` type for Supabase sync
- **Three Save Modes**:
  - `Insert` — always creates new rows (event logs, kill records)
  - `Update` — first sync inserts, subsequent syncs update the same row (session stats)
  - `Upsert` — uses `ON CONFLICT` to insert or update based on unique columns
- **Deduplication** — skip already-synced records in Insert mode using sync keys
- **Primary Key Management** — automatically retrieves and stores database IDs for Update mode
- **Event-driven API** — trigger syncs with `SyncToSupabase<T>`; react to `SyncComplete<T>` or `SyncError<T>`
- **Request Building** — `WriteOptions` and `TableQuery` express upserts on explicit conflict columns, column projections, filters, ordering and limits
- **Reads** — `execute_select` pulls rows from tables and views, such as leaderboards
- **Async Bridge** — `ResultQueue` carries results from HTTP callbacks into Bevy systems
- **Incremental Sync** — `SyncCursor`, `SyncCursors` and `SyncWatermarks` track what the server already has
- **WASM-compatible** — uses [`ehttp`](https://github.com/emilk/ehttp) for native + browser support

## Bevy Compatibility

| `msg_supabase` | Bevy |
|----------------|------|
| 0.3            | 0.18 |
| 0.1            | 0.18 |

## Installation

```toml
[dependencies]
msg_supabase = { git = "https://github.com/MolecularSadism/msg_supabase", tag = "v0.3.0" }
```

## Quick Start

```rust
use bevy::prelude::*;
use msg_supabase::prelude::*;
use serde::Serialize;

#[derive(Clone, Serialize)]
struct PlayerStats {
    player_id: i64,
    kills: u32,
    deaths: u32,
}

impl SupabaseRow for PlayerStats {
    fn table_name() -> &'static str { "player_stats" }
    fn primary_key_column() -> &'static str { "id" }
    fn unique_columns() -> &'static [&'static str] { &["player_id"] }
}

fn main() {
    App::new()
        .add_plugins(DefaultPlugins)
        .add_plugins(
            SupabasePlugin::<PlayerStats>::new(
                "https://your-project.supabase.co",
                "your-anon-key",
            )
            .with_save_mode(SaveMode::Update),
        )
        .add_systems(Update, sync_stats)
        .run();
}

fn sync_stats(mut commands: Commands, stats: Res<PlayerStats>) {
    commands.trigger(SyncToSupabase::new(stats.clone()));
}
```

## Save Modes

### Insert

Each sync creates a new database row. Ideal for event logs or kill records where you want a historical trail.

```rust
SupabasePlugin::<KillEvent>::new(url, key)
    // SaveMode::Insert is the default
```

Use `SyncToSupabase::with_key(data, key)` to deduplicate — the same key will only be synced once per session.

### Update

The first sync inserts a row and retrieves its primary key. All subsequent syncs update that same row. Ideal for accumulating session data.

```rust
SupabasePlugin::<SessionData>::new(url, key)
    .with_save_mode(SaveMode::Update)
```

### Upsert

Uses Supabase's `ON CONFLICT` mechanism to insert or update based on the columns returned by `unique_columns()`. Useful when you want idempotent syncs regardless of session.

```rust
SupabasePlugin::<PlayerProfile>::new(url, key)
    .with_save_mode(SaveMode::Upsert)
```

## Reacting to Results

```rust
fn on_sync_complete(trigger: On<SyncComplete<PlayerStats>>) {
    if let Some(pk) = trigger.event().primary_key {
        println!("Synced! Primary key: {}", pk);
    }
}

fn on_sync_error(trigger: On<SyncError<PlayerStats>>) {
    eprintln!("Sync failed: {}", trigger.event().message);
}

app.add_observer(on_sync_complete);
app.add_observer(on_sync_error);
```

## Inspecting Sync State

```rust
fn inspect_state(state: Res<SyncState<PlayerStats>>) {
    println!("Synced {} times", state.sync_count());
    if let Some(pk) = state.primary_key() {
        println!("Row ID: {}", pk);
    }
}
```

## Writing Without the Plugin

Workflows that chain requests — insert a session, then its runs keyed by the returned id — drive
the request layer directly. `WriteOptions` decides how a write resolves conflicts and what it
returns:

```rust
use msg_supabase::prelude::*;

// Upsert the runs of a session, and read back the ids the database assigned.
execute_write_returning(
    &connection,
    &run_rows,
    "runs",
    &WriteOptions::new()
        .on_conflict(["session_pk", "run_index"])
        .returning("id,run_index"),
    move |result: Result<Vec<RunResponse>, RequestError>| {
        // ...
    },
);
```

Use `execute_write` when the response body is not needed. Both complete immediately when the row
slice is empty, so callers do not have to special-case "nothing to push".

`build_write_request` returns the request instead of sending it, for callers that need a different
timeout or a blocking send — a panic hook uploading a crash report, say.

## Reading

Reads work against tables and views alike:

```rust
use msg_supabase::prelude::*;

execute_select(
    &connection,
    "highscores_runs",
    &TableQuery::new().select("*").order("kills", Order::Descending).limit(10),
    move |result: Result<Vec<RunHighscore>, RequestError>| {
        // ...
    },
);
```

## Bridging Callbacks Into Bevy

HTTP callbacks run off the main thread with no World access, so they hand results to a
`ResultQueue` that a system drains:

```rust
use bevy::prelude::*;
use msg_supabase::prelude::*;

#[derive(Resource, Default)]
struct ScoreInbox(ResultQueue<Vec<RunHighscore>>);

fn drain_scores(inbox: Res<ScoreInbox>, mut board: ResMut<Leaderboard>) {
    for rows in inbox.0.drain() {
        board.rows = rows;
    }
}
```

## Incremental Syncs

A client that pushes the same growing data set on every autosave needs to know what the server
already has. `SyncCursor` tracks how much of an append-only list was pushed, `SyncCursors` keeps
one cursor per key, and `SyncWatermarks` records the last value pushed for counters that only grow:

```rust
use msg_supabase::prelude::*;

// Only the kills recorded since the last successful push.
let pending = cursors.pending(&run_index, &kill_records);

// Only the spells whose cast count changed.
if watermarks.is_ahead(&(run_index, spell.clone()), cast_count) {
    // ...
}
```

## Low-Level HTTP Functions

For custom workflows, the individual request functions are public:

```rust
use msg_supabase::request::{execute_insert, execute_update, execute_upsert, execute_batch_insert};
```

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option.
