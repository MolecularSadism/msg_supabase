//! HTTP request construction and dispatch for Supabase's PostgREST API.
//!
//! Three layers, from lowest to highest:
//!
//! - [`build_write_request`] and [`build_select_request`] hand back an `ehttp::Request` for
//!   callers that want to send it themselves — with a custom timeout, or blocking.
//! - [`execute_write`], [`execute_write_returning`] and [`execute_select`] send a request and
//!   report the outcome through a callback.
//! - [`execute_sync`] applies a [`SyncConfig`] and is what
//!   [`SupabasePlugin`](crate::plugin::SupabasePlugin) drives.

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::config::{SaveMode, SyncConfig};
use crate::error::RequestError;
use crate::query::TableQuery;
use crate::traits::SupabaseRow;

/// Supabase connection configuration.
#[derive(Clone)]
pub struct SupabaseConnection {
    /// Supabase project URL (e.g., `https://xyz.supabase.co`)
    pub url: String,

    /// Supabase API key (anon/public key for client-side)
    pub api_key: String,
}

impl SupabaseConnection {
    /// Create a new connection configuration.
    pub fn new(url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            api_key: api_key.into(),
        }
    }

    /// Build the REST API URL for a table.
    pub fn table_url(&self, table: &str) -> String {
        format!("{}/rest/v1/{}", self.url, table)
    }

    /// Build the REST API URL for a table, with query parameters appended.
    pub fn table_url_with(&self, table: &str, query: &TableQuery) -> String {
        let base = self.table_url(table);
        if query.is_empty() {
            base
        } else {
            format!("{base}?{}", query.to_query_string())
        }
    }
}

/// Response from Supabase containing the primary key.
#[derive(serde::Deserialize, Debug)]
pub struct PrimaryKeyResponse {
    pub id: i64,
}

/// How a write resolves conflicts, and what it returns.
///
/// The default inserts rows and asks for no response body. Naming conflict
/// columns turns the write into an upsert; asking for returned rows makes the
/// server respond with them.
///
/// # Example
///
/// ```rust
/// use msg_supabase::prelude::*;
///
/// // Insert, ignoring the response body.
/// let plain = WriteOptions::new();
/// assert!(!plain.is_upsert());
///
/// // Upsert on (session_pk, run_index), returning the database ids.
/// let upsert = WriteOptions::new()
///     .on_conflict(["session_pk", "run_index"])
///     .returning("id,run_index");
/// assert!(upsert.is_upsert() && upsert.returns_rows());
/// ```
#[derive(Debug, Clone, Default)]
pub struct WriteOptions {
    on_conflict: Vec<String>,
    return_rows: bool,
    returning: Option<String>,
}

impl WriteOptions {
    /// Create options for a plain insert with no response body.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve conflicts on these columns, making the write an upsert.
    #[must_use]
    pub fn on_conflict<I, S>(mut self, columns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.on_conflict = columns.into_iter().map(Into::into).collect();
        self
    }

    /// Return the written rows, projected to `columns`.
    #[must_use]
    pub fn returning(mut self, columns: impl Into<String>) -> Self {
        self.return_rows = true;
        self.returning = Some(columns.into());
        self
    }

    /// Return the written rows in full.
    #[must_use]
    pub fn returning_all(mut self) -> Self {
        self.return_rows = true;
        self.returning = None;
        self
    }

    /// Whether the write resolves conflicts instead of failing on them.
    pub fn is_upsert(&self) -> bool {
        !self.on_conflict.is_empty()
    }

    /// Whether the server is asked to send the written rows back.
    pub fn returns_rows(&self) -> bool {
        self.return_rows
    }

    /// The `Prefer` header value for these options.
    fn prefer_header(&self) -> String {
        let return_part = if self.return_rows {
            "return=representation"
        } else {
            "return=minimal"
        };

        if self.is_upsert() {
            format!("resolution=merge-duplicates,{return_part}")
        } else {
            return_part.to_string()
        }
    }

    /// The URL query parameters for these options.
    fn query(&self) -> TableQuery {
        let mut query = TableQuery::new().on_conflict(self.on_conflict.iter().cloned());
        if let Some(ref columns) = self.returning {
            query = query.select(columns.clone());
        }
        query
    }

    /// The same options, asking for the written rows back.
    fn requesting_rows(&self) -> Self {
        let mut options = self.clone();
        options.return_rows = true;
        options
    }
}

fn build_headers(api_key: &str, prefer: &str) -> ehttp::Headers {
    ehttp::Headers::new(&[
        ("Content-Type", "application/json"),
        ("apikey", api_key),
        ("Authorization", &format!("Bearer {api_key}")),
        ("Prefer", prefer),
    ])
}

fn build_read_headers(api_key: &str) -> ehttp::Headers {
    ehttp::Headers::new(&[
        ("apikey", api_key),
        ("Authorization", &format!("Bearer {api_key}")),
    ])
}

/// Build the POST request that writes `rows` to `table`.
///
/// Returns the request instead of sending it, so callers can adjust it — a
/// shorter timeout, or a blocking send from a panic hook.
///
/// # Errors
///
/// Returns a [`RequestError`] if `rows` cannot be serialized.
///
/// # Example
///
/// ```rust
/// use msg_supabase::prelude::*;
/// use serde::Serialize;
///
/// #[derive(Serialize)]
/// struct CrashReport {
///     message: String,
/// }
///
/// let connection = SupabaseConnection::new("https://xyz.supabase.co", "anon-key");
/// let report = CrashReport {
///     message: "panicked".to_string(),
/// };
///
/// let request = build_write_request(
///     &connection,
///     std::slice::from_ref(&report),
///     "crash_reports",
///     &WriteOptions::new(),
/// )
/// .expect("report serializes");
///
/// assert_eq!(request.url, "https://xyz.supabase.co/rest/v1/crash_reports");
/// ```
pub fn build_write_request<T: Serialize>(
    connection: &SupabaseConnection,
    rows: &[T],
    table: &str,
    options: &WriteOptions,
) -> Result<ehttp::Request, RequestError> {
    let json = serde_json::to_string(rows).map_err(|e| RequestError::serialization(&e))?;

    Ok(ehttp::Request {
        method: "POST".to_string(),
        url: connection.table_url_with(table, &options.query()),
        body: json.into_bytes(),
        headers: build_headers(&connection.api_key, &options.prefer_header()),
        mode: Default::default(),
        timeout: Some(ehttp::Request::DEFAULT_TIMEOUT),
    })
}

/// Build the GET request that reads rows from `table`.
///
/// Reads work against tables and views alike, so this also serves aggregate
/// views such as leaderboards.
///
/// # Example
///
/// ```rust
/// use msg_supabase::prelude::*;
///
/// let connection = SupabaseConnection::new("https://xyz.supabase.co", "anon-key");
/// let request = build_select_request(
///     &connection,
///     "highscores",
///     &TableQuery::new().select("*").limit(100),
/// );
///
/// assert_eq!(
///     request.url,
///     "https://xyz.supabase.co/rest/v1/highscores?select=*&limit=100"
/// );
/// ```
pub fn build_select_request(
    connection: &SupabaseConnection,
    table: &str,
    query: &TableQuery,
) -> ehttp::Request {
    ehttp::Request {
        method: "GET".to_string(),
        url: connection.table_url_with(table, query),
        body: Vec::new(),
        headers: build_read_headers(&connection.api_key),
        mode: Default::default(),
        timeout: Some(ehttp::Request::DEFAULT_TIMEOUT),
    }
}

/// Write `rows` to `table`, reporting only success or failure.
///
/// Completes immediately with `Ok` when `rows` is empty.
pub fn execute_write<T, F>(
    connection: &SupabaseConnection,
    rows: &[T],
    table: &str,
    options: &WriteOptions,
    on_complete: F,
) where
    T: Serialize,
    F: FnOnce(Result<(), RequestError>) + Send + 'static,
{
    if rows.is_empty() {
        on_complete(Ok(()));
        return;
    }

    match build_write_request(connection, rows, table, options) {
        Ok(request) => send(request, move |result| on_complete(result.map(|_| ()))),
        Err(err) => on_complete(Err(err)),
    }
}

/// Write `rows` to `table` and deserialize the rows the server sends back.
///
/// Asks for a response body even when `options` did not. Completes immediately
/// with an empty `Vec` when `rows` is empty.
pub fn execute_write_returning<T, R, F>(
    connection: &SupabaseConnection,
    rows: &[T],
    table: &str,
    options: &WriteOptions,
    on_complete: F,
) where
    T: Serialize,
    R: DeserializeOwned,
    F: FnOnce(Result<Vec<R>, RequestError>) + Send + 'static,
{
    if rows.is_empty() {
        on_complete(Ok(Vec::new()));
        return;
    }

    match build_write_request(connection, rows, table, &options.requesting_rows()) {
        Ok(request) => send(request, move |result| {
            on_complete(result.and_then(|response| decode_rows(&response)));
        }),
        Err(err) => on_complete(Err(err)),
    }
}

/// Read rows from `table` (or a view) and deserialize them.
///
/// # Example
///
/// ```rust
/// use msg_supabase::prelude::*;
/// use serde::Deserialize;
///
/// #[derive(Deserialize)]
/// struct Highscore {
///     player_id: String,
///     kills: i64,
/// }
///
/// fn refresh(connection: &SupabaseConnection, inbox: &ResultQueue<Vec<Highscore>>) {
///     let sender = inbox.sender();
///     execute_select(
///         connection,
///         "highscores",
///         &TableQuery::new()
///             .select("*")
///             .order("kills", Order::Descending)
///             .limit(10),
///         move |result: Result<Vec<Highscore>, RequestError>| {
///             if let Ok(rows) = result {
///                 sender.send(rows);
///             }
///         },
///     );
/// }
/// ```
pub fn execute_select<R, F>(
    connection: &SupabaseConnection,
    table: &str,
    query: &TableQuery,
    on_complete: F,
) where
    R: DeserializeOwned,
    F: FnOnce(Result<Vec<R>, RequestError>) + Send + 'static,
{
    send(
        build_select_request(connection, table, query),
        move |result| {
            on_complete(result.and_then(|response| decode_rows(&response)));
        },
    );
}

/// Execute an insert request to Supabase.
pub fn execute_insert<T, F>(connection: &SupabaseConnection, data: &T, table: &str, on_complete: F)
where
    T: SupabaseRow,
    F: FnOnce(Result<Option<i64>, RequestError>) + Send + 'static,
{
    write_one(
        connection,
        data,
        table,
        &WriteOptions::new().returning_all(),
        on_complete,
    );
}

/// Execute an upsert request to Supabase.
///
/// Conflicts resolve on [`SupabaseRow::unique_columns`]; with no unique columns
/// the server falls back to the table's primary key.
pub fn execute_upsert<T, F>(connection: &SupabaseConnection, data: &T, table: &str, on_complete: F)
where
    T: SupabaseRow,
    F: FnOnce(Result<Option<i64>, RequestError>) + Send + 'static,
{
    write_one(connection, data, table, &upsert_options::<T>(), on_complete);
}

/// Execute an update request to Supabase using the primary key.
pub fn execute_update<T, F>(
    connection: &SupabaseConnection,
    data: &T,
    table: &str,
    primary_key: i64,
    pk_column: &str,
    on_complete: F,
) where
    T: SupabaseRow,
    F: FnOnce(Result<Option<i64>, RequestError>) + Send + 'static,
{
    let json = match serde_json::to_string(data) {
        Ok(json) => json,
        Err(e) => {
            on_complete(Err(RequestError::serialization(&e)));
            return;
        }
    };

    let request = ehttp::Request {
        method: "PATCH".to_string(),
        url: connection.table_url_with(table, &TableQuery::new().eq(pk_column, primary_key)),
        body: json.into_bytes(),
        headers: build_headers(&connection.api_key, "return=minimal"),
        mode: Default::default(),
        timeout: Some(ehttp::Request::DEFAULT_TIMEOUT),
    };

    send(request, move |result| {
        on_complete(result.map(|_| None));
    });
}

/// Execute a request based on the sync configuration and current state.
pub fn execute_sync<T, F>(
    connection: &SupabaseConnection,
    data: &T,
    config: &SyncConfig,
    has_primary_key: bool,
    primary_key: Option<i64>,
    on_complete: F,
) where
    T: SupabaseRow,
    F: FnOnce(Result<Option<i64>, RequestError>, bool) + Send + 'static,
{
    let table: String = config
        .table_override
        .clone()
        .unwrap_or_else(|| T::table_name().to_string());

    match config.save_mode {
        SaveMode::Insert => {
            execute_insert(connection, data, &table, move |result| {
                on_complete(result, true);
            });
        }
        SaveMode::Upsert => {
            execute_upsert(connection, data, &table, move |result| {
                on_complete(result, true);
            });
        }
        SaveMode::Update => match (has_primary_key, primary_key) {
            (true, Some(pk)) => execute_update(
                connection,
                data,
                &table,
                pk,
                T::primary_key_column(),
                move |result| {
                    on_complete(result, false);
                },
            ),
            _ => execute_upsert(connection, data, &table, move |result| {
                on_complete(result, true);
            }),
        },
    }
}

/// Execute a batch insert request to Supabase.
pub fn execute_batch_insert<T, F>(
    connection: &SupabaseConnection,
    data: &[T],
    table: &str,
    on_complete: F,
) where
    T: SupabaseRow,
    F: FnOnce(Result<Vec<i64>, RequestError>) + Send + 'static,
{
    write_many(
        connection,
        data,
        table,
        &WriteOptions::new().returning_all(),
        on_complete,
    );
}

/// Execute a batch upsert request to Supabase.
///
/// Conflicts resolve on [`SupabaseRow::unique_columns`]; with no unique columns
/// the server falls back to the table's primary key.
pub fn execute_batch_upsert<T, F>(
    connection: &SupabaseConnection,
    data: &[T],
    table: &str,
    on_complete: F,
) where
    T: SupabaseRow,
    F: FnOnce(Result<Vec<i64>, RequestError>) + Send + 'static,
{
    write_many(connection, data, table, &upsert_options::<T>(), on_complete);
}

/// Upsert options resolving on a row type's unique columns.
fn upsert_options<T: SupabaseRow>() -> WriteOptions {
    WriteOptions::new()
        .on_conflict(T::unique_columns().iter().copied())
        .returning_all()
}

/// Write a single row, reporting the primary key the server assigned.
fn write_one<T, F>(
    connection: &SupabaseConnection,
    data: &T,
    table: &str,
    options: &WriteOptions,
    on_complete: F,
) where
    T: SupabaseRow,
    F: FnOnce(Result<Option<i64>, RequestError>) + Send + 'static,
{
    match build_write_request(connection, std::slice::from_ref(data), table, options) {
        Ok(request) => send(request, move |result| {
            on_complete(result.map(|response| first_primary_key(&response)));
        }),
        Err(err) => on_complete(Err(err)),
    }
}

/// Write several rows, reporting the primary keys the server assigned.
fn write_many<T, F>(
    connection: &SupabaseConnection,
    data: &[T],
    table: &str,
    options: &WriteOptions,
    on_complete: F,
) where
    T: SupabaseRow,
    F: FnOnce(Result<Vec<i64>, RequestError>) + Send + 'static,
{
    if data.is_empty() {
        on_complete(Ok(Vec::new()));
        return;
    }

    match build_write_request(connection, data, table, options) {
        Ok(request) => send(request, move |result| {
            on_complete(result.map(|response| primary_keys(&response)));
        }),
        Err(err) => on_complete(Err(err)),
    }
}

/// Send `request`, handing the callback either a successful response or a
/// [`RequestError`] describing the network or HTTP failure.
fn send<F>(request: ehttp::Request, on_complete: F)
where
    F: FnOnce(Result<ehttp::Response, RequestError>) + Send + 'static,
{
    ehttp::fetch(request, move |result| {
        on_complete(check_response(result));
    });
}

fn check_response(
    result: Result<ehttp::Response, String>,
) -> Result<ehttp::Response, RequestError> {
    match result {
        Ok(response) if response.ok => Ok(response),
        Ok(response) => {
            let body = response.text().map(String::from);
            Err(RequestError::http(
                format!("Request failed with status {}", response.status),
                response.status,
                body,
            ))
        }
        Err(e) => Err(RequestError::network(e)),
    }
}

/// Deserialize the rows of a successful response.
fn decode_rows<R: DeserializeOwned>(response: &ehttp::Response) -> Result<Vec<R>, RequestError> {
    let Some(body) = response.text() else {
        return Err(RequestError::http(
            "Response body was not valid UTF-8",
            response.status,
            None,
        ));
    };

    serde_json::from_str::<Vec<R>>(body).map_err(|e| {
        RequestError::http(
            format!("Failed to parse response: {e}"),
            response.status,
            Some(body.to_string()),
        )
    })
}

/// Primary key of the first returned row, if the body carries one.
fn first_primary_key(response: &ehttp::Response) -> Option<i64> {
    primary_keys(response).first().copied()
}

/// Primary keys of the returned rows, empty if the body carries none.
fn primary_keys(response: &ehttp::Response) -> Vec<i64> {
    response
        .text()
        .and_then(|body| serde_json::from_str::<Vec<PrimaryKeyResponse>>(body).ok())
        .map(|rows| rows.into_iter().map(|row| row.id).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, serde::Serialize)]
    struct TestRow {
        session_pk: i64,
        run_index: i32,
    }

    impl SupabaseRow for TestRow {
        fn table_name() -> &'static str {
            "runs"
        }
        fn primary_key_column() -> &'static str {
            "id"
        }
        fn unique_columns() -> &'static [&'static str] {
            &["session_pk", "run_index"]
        }
    }

    fn connection() -> SupabaseConnection {
        SupabaseConnection::new("https://test.supabase.co", "key123")
    }

    fn header(request: &ehttp::Request, name: &str) -> Option<String> {
        request.headers.get(name).map(String::from)
    }

    #[test]
    fn test_connection_table_url() {
        assert_eq!(
            connection().table_url("users"),
            "https://test.supabase.co/rest/v1/users"
        );
    }

    #[test]
    fn table_url_appends_query_parameters() {
        let query = TableQuery::new().select("*").limit(10);
        assert_eq!(
            connection().table_url_with("users", &query),
            "https://test.supabase.co/rest/v1/users?select=*&limit=10"
        );
    }

    #[test]
    fn table_url_without_parameters_is_bare() {
        assert_eq!(
            connection().table_url_with("users", &TableQuery::new()),
            "https://test.supabase.co/rest/v1/users"
        );
    }

    #[test]
    fn insert_options_ask_for_no_body() {
        let options = WriteOptions::new();
        assert!(!options.is_upsert());
        assert!(!options.returns_rows());
        assert_eq!(options.prefer_header(), "return=minimal");
        assert_eq!(options.query().to_query_string(), "");
    }

    #[test]
    fn upsert_options_carry_conflict_columns() {
        let options = WriteOptions::new()
            .on_conflict(["session_pk", "run_index"])
            .returning("id,run_index");

        assert!(options.is_upsert());
        assert_eq!(
            options.prefer_header(),
            "resolution=merge-duplicates,return=representation"
        );
        assert_eq!(
            options.query().to_query_string(),
            "on_conflict=session_pk,run_index&select=id,run_index"
        );
    }

    #[test]
    fn returning_all_omits_the_select() {
        let options = WriteOptions::new().returning_all();
        assert!(options.returns_rows());
        assert_eq!(options.prefer_header(), "return=representation");
        assert_eq!(options.query().to_query_string(), "");
    }

    #[test]
    fn requesting_rows_upgrades_a_minimal_write() {
        let options = WriteOptions::new().on_conflict(["id"]).requesting_rows();
        assert!(options.returns_rows());
        assert!(options.is_upsert());
    }

    #[test]
    fn upsert_options_follow_the_row_type() {
        let options = upsert_options::<TestRow>();
        assert_eq!(
            options.query().to_query_string(),
            "on_conflict=session_pk,run_index"
        );
    }

    #[test]
    fn write_request_carries_rows_and_headers() {
        let rows = [TestRow {
            session_pk: 7,
            run_index: 2,
        }];
        let options = WriteOptions::new()
            .on_conflict(["session_pk", "run_index"])
            .returning("id");

        let request = build_write_request(&connection(), &rows, "runs", &options).unwrap();

        assert_eq!(request.method, "POST");
        assert_eq!(
            request.url,
            "https://test.supabase.co/rest/v1/runs?on_conflict=session_pk,run_index&select=id"
        );
        assert_eq!(
            String::from_utf8(request.body.clone()).unwrap(),
            r#"[{"session_pk":7,"run_index":2}]"#
        );
        assert_eq!(header(&request, "apikey").as_deref(), Some("key123"));
        assert_eq!(
            header(&request, "authorization").as_deref(),
            Some("Bearer key123")
        );
        assert_eq!(
            header(&request, "prefer").as_deref(),
            Some("resolution=merge-duplicates,return=representation")
        );
    }

    #[test]
    fn select_request_is_a_get_without_a_body() {
        let query = TableQuery::new().select("*").limit(1000);
        let request = build_select_request(&connection(), "highscores_runs", &query);

        assert_eq!(request.method, "GET");
        assert!(request.body.is_empty());
        assert_eq!(
            request.url,
            "https://test.supabase.co/rest/v1/highscores_runs?select=*&limit=1000"
        );
        assert_eq!(header(&request, "apikey").as_deref(), Some("key123"));
    }

    #[test]
    fn empty_write_completes_without_a_request() {
        let rows: [TestRow; 0] = [];
        let outcome = Arc::new(Mutex::new(None));
        let sink = outcome.clone();

        execute_write(
            &connection(),
            &rows,
            "runs",
            &WriteOptions::new(),
            move |result| {
                *sink.lock().unwrap() = Some(result.is_ok());
            },
        );

        assert_eq!(*outcome.lock().unwrap(), Some(true));
    }

    #[test]
    fn empty_batch_insert_returns_no_ids() {
        let rows: [TestRow; 0] = [];
        let outcome = Arc::new(Mutex::new(None));
        let sink = outcome.clone();

        execute_batch_insert(&connection(), &rows, "runs", move |result| {
            *sink.lock().unwrap() = result.ok();
        });

        assert_eq!(*outcome.lock().unwrap(), Some(Vec::new()));
    }
}
