//! Bevy plugin for reading Supabase tables and views.

use bevy::log::{trace, warn};
use bevy::prelude::*;
use std::marker::PhantomData;

use crate::event::{FetchView, ViewFetchFailed, ViewFetched};
use crate::queue::{ViewOutcome, ViewResultQueue};
use crate::request::{SupabaseConnection, execute_select};
use crate::traits::SupabaseView;

/// Resource holding the Supabase connection for a view.
#[derive(Resource)]
pub struct SupabaseViewConfig<R: SupabaseView> {
    /// Connection details (URL and API key).
    pub connection: SupabaseConnection,

    _marker: PhantomData<R>,
}

impl<R: SupabaseView> SupabaseViewConfig<R> {
    /// Create a new config with the given connection.
    pub fn new(connection: SupabaseConnection) -> Self {
        Self {
            connection,
            _marker: PhantomData,
        }
    }
}

/// Plugin that enables reading one Supabase table or view.
///
/// Trigger [`FetchView<R>`] to start a read; the rows arrive as
/// [`ViewFetched<R>`], and a failure as [`ViewFetchFailed<R>`].
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
///     player_id: String,
///     kills: i64,
/// }
///
/// impl SupabaseView for Highscore {
///     fn view_name() -> &'static str { "highscores" }
///     fn query() -> TableQuery {
///         TableQuery::new().select("*").order("kills", Order::Descending).limit(10)
///     }
/// }
///
/// #[derive(Resource, Default)]
/// struct Leaderboard(Vec<Highscore>);
///
/// fn on_scores(mut trigger: On<ViewFetched<Highscore>>, mut board: ResMut<Leaderboard>) {
///     board.0 = std::mem::take(&mut trigger.rows);
/// }
///
/// fn setup_app() {
///     App::new()
///         .add_plugins(MinimalPlugins)
///         .add_plugins(SupabaseViewPlugin::<Highscore>::new(
///             "https://your-project.supabase.co",
///             "your-anon-key",
///         ))
///         .init_resource::<Leaderboard>()
///         .add_observer(on_scores);
/// }
/// ```
pub struct SupabaseViewPlugin<R: SupabaseView> {
    connection: SupabaseConnection,
    _marker: PhantomData<R>,
}

impl<R: SupabaseView> SupabaseViewPlugin<R> {
    /// Create a new plugin with the given Supabase URL and API key.
    pub fn new(url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            connection: SupabaseConnection::new(url, api_key),
            _marker: PhantomData,
        }
    }
}

impl<R: SupabaseView> Plugin for SupabaseViewPlugin<R> {
    fn build(&self, app: &mut App) {
        if !self.connection.url.starts_with("https://") {
            warn!(
                "msg_supabase: URL '{}' does not use HTTPS — API keys will be transmitted in plaintext",
                self.connection.url
            );
        }

        app.init_resource::<ViewResultQueue<R>>();
        app.insert_resource(SupabaseViewConfig::<R>::new(self.connection.clone()));

        app.add_observer(on_fetch_view::<R>);
        app.add_systems(Update, poll_view_results::<R>);
    }
}

/// Observer that handles `FetchView<R>` events.
fn on_fetch_view<R: SupabaseView>(
    _trigger: On<FetchView<R>>,
    config: Option<Res<SupabaseViewConfig<R>>>,
    queue: Option<Res<ViewResultQueue<R>>>,
) {
    let Some(config) = config else {
        warn!(
            "FetchView<{}> triggered but SupabaseViewConfig not found. \
             Did you forget to add SupabaseViewPlugin<{}>?",
            R::view_name(),
            R::view_name()
        );
        return;
    };

    let Some(queue) = queue else {
        return;
    };

    let sender = queue.sender();
    execute_select(
        &config.connection,
        R::view_name(),
        &R::query(),
        move |result: Result<Vec<R>, _>| {
            let outcome = match result {
                Ok(rows) => ViewOutcome::Rows(rows),
                Err(err) => ViewOutcome::Failure(err),
            };
            sender.send(outcome);
        },
    );

    trace!("Initiated Supabase read of {}", R::view_name());
}

/// Drains the result queue each frame and fires the outcome events.
fn poll_view_results<R: SupabaseView>(queue: Res<ViewResultQueue<R>>, mut commands: Commands) {
    for outcome in queue.drain() {
        match outcome {
            ViewOutcome::Rows(rows) => {
                trace!("Read {} rows from {}", rows.len(), R::view_name());
                commands.trigger(ViewFetched::<R> { rows });
            }
            ViewOutcome::Failure(err) => {
                warn!(
                    "Failed to read {} from Supabase: {} (status: {:?})",
                    R::view_name(),
                    err.message,
                    err.status
                );
                commands.trigger(ViewFetchFailed::<R>::new(err.message, err.status));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::RequestError;
    use serde::Deserialize;

    #[derive(Deserialize, Debug, PartialEq)]
    struct TestScore {
        kills: i64,
    }

    impl SupabaseView for TestScore {
        fn view_name() -> &'static str {
            "highscores"
        }
    }

    #[derive(Resource, Default)]
    struct Seen {
        rows: Vec<i64>,
        failure: Option<String>,
    }

    fn view_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(SupabaseViewPlugin::<TestScore>::new(
            "https://test.supabase.co",
            "key",
        ));
        app.init_resource::<Seen>();
        app.add_observer(
            |trigger: On<ViewFetched<TestScore>>, mut seen: ResMut<Seen>| {
                seen.rows = trigger.rows.iter().map(|row| row.kills).collect();
            },
        );
        app.add_observer(
            |trigger: On<ViewFetchFailed<TestScore>>, mut seen: ResMut<Seen>| {
                seen.failure = Some(trigger.message.clone());
            },
        );
        app
    }

    #[test]
    fn the_plugin_registers_its_resources() {
        let mut app = view_app();
        app.update();

        assert!(
            app.world()
                .get_resource::<SupabaseViewConfig<TestScore>>()
                .is_some()
        );
    }

    #[test]
    fn fetched_rows_reach_the_observer() {
        let mut app = view_app();

        app.world()
            .resource::<ViewResultQueue<TestScore>>()
            .sender()
            .send(ViewOutcome::Rows(vec![
                TestScore { kills: 3 },
                TestScore { kills: 5 },
            ]));
        app.update();

        assert_eq!(app.world().resource::<Seen>().rows, vec![3, 5]);
        assert!(app.world().resource::<Seen>().failure.is_none());
    }

    #[test]
    fn a_failed_read_reaches_the_observer() {
        let mut app = view_app();

        app.world()
            .resource::<ViewResultQueue<TestScore>>()
            .sender()
            .send(ViewOutcome::Failure(RequestError::network("offline")));
        app.update();

        assert_eq!(
            app.world().resource::<Seen>().failure.as_deref(),
            Some("offline")
        );
        assert!(app.world().resource::<Seen>().rows.is_empty());
    }
}
