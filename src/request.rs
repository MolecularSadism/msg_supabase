//! HTTP request construction and dispatch for Supabase's PostgREST API.
//!
//! Two layers:
//!
//! - [`build_write_request`] and [`build_select_request`] hand back an `ehttp::Request` for
//!   callers that want to send it themselves — with a custom timeout, or blocking.
//! - [`execute_write_returning`], [`execute_update`] and [`execute_select`] send a request and
//!   report the outcome through a callback. These are what
//!   [`SupabasePlugin`](crate::plugin::SupabasePlugin) and
//!   [`SupabaseViewPlugin`](crate::view::SupabaseViewPlugin) drive.

use serde::Serialize;
use serde::de::DeserializeOwned;

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

/// What a write returned: the rows the server sent back, the primary keys
/// among them, and the HTTP status.
#[derive(Debug)]
pub struct WriteResponse<R> {
    /// Rows the server returned, empty when no response body was asked for.
    pub rows: Vec<R>,

    /// Primary keys the server assigned, in the order they came back.
    pub primary_keys: Vec<i64>,

    /// HTTP status code from the response.
    pub status: u16,
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
    pub(crate) fn query(&self) -> TableQuery {
        let mut query = TableQuery::new().on_conflict(self.on_conflict.iter().cloned());
        if let Some(ref columns) = self.returning {
            query = query.select(columns.clone());
        }
        query
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

/// Write `rows` to `table` and deserialize what the server sends back.
///
/// A write that asked for no response body reports no rows, which is not an
/// error. Completes immediately with an empty response when `rows` is empty.
pub fn execute_write_returning<T, R, F>(
    connection: &SupabaseConnection,
    rows: &[T],
    table: &str,
    options: &WriteOptions,
    on_complete: F,
) where
    T: Serialize,
    R: DeserializeOwned,
    F: FnOnce(Result<WriteResponse<R>, RequestError>) + Send + 'static,
{
    if rows.is_empty() {
        on_complete(Ok(WriteResponse {
            rows: Vec::new(),
            primary_keys: Vec::new(),
            status: 0,
        }));
        return;
    }

    match build_write_request(connection, rows, table, options) {
        Ok(request) => send(request, move |result| {
            on_complete(result.and_then(|response| {
                Ok(WriteResponse {
                    rows: decode_rows(&response)?,
                    primary_keys: primary_keys(&response),
                    status: response.status,
                })
            }));
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

    if body.trim().is_empty() {
        return Ok(Vec::new());
    }

    serde_json::from_str::<Vec<R>>(body).map_err(|e| {
        RequestError::http(
            format!("Failed to parse response: {e}"),
            response.status,
            Some(body.to_string()),
        )
    })
}

/// Primary keys of the returned rows, empty if the body carries none.
pub(crate) fn primary_keys(response: &ehttp::Response) -> Vec<i64> {
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
        type Response = PrimaryKeyResponse;

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
    fn an_empty_write_reports_no_rows() {
        let rows: [TestRow; 0] = [];
        let outcome = Arc::new(Mutex::new(None));
        let sink = outcome.clone();

        execute_write_returning(
            &connection(),
            &rows,
            "runs",
            &WriteOptions::new(),
            move |result: Result<WriteResponse<PrimaryKeyResponse>, RequestError>| {
                *sink.lock().unwrap() = result.ok().map(|response| response.rows.len());
            },
        );

        assert_eq!(*outcome.lock().unwrap(), Some(0));
    }

    #[test]
    fn returning_all_omits_the_select() {
        let options = WriteOptions::new().returning_all();
        assert!(options.returns_rows());
        assert_eq!(options.prefer_header(), "return=representation");
        assert_eq!(options.query().to_query_string(), "");
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
}
