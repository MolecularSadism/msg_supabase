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
- **WASM-compatible** — uses [`ehttp`](https://github.com/emilk/ehttp) for native + browser support

## Bevy Compatibility

| `msg_supabase` | Bevy |
|----------------|------|
| 0.1            | 0.18 |

## Installation

```toml
[dependencies]
msg_supabase = { git = "https://github.com/MolecularSadism/msg_supabase", tag = "v0.1.0" }
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
