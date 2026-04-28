//! HTTP request handling for Supabase operations.

use crate::config::{SaveMode, SyncConfig};
use crate::error::RequestError;
use crate::traits::SupabaseRow;

/// Supabase connection configuration.
#[derive(Clone)]
pub struct SupabaseConnection {
    /// Supabase project URL (e.g., "https://xyz.supabase.co")
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
}

/// Response from Supabase containing the primary key.
#[derive(serde::Deserialize, Debug)]
pub struct PrimaryKeyResponse {
    pub id: i64,
}

fn build_headers(api_key: &str, prefer: &str) -> ehttp::Headers {
    ehttp::Headers::new(&[
        ("Content-Type", "application/json"),
        ("apikey", api_key),
        ("Authorization", &format!("Bearer {}", api_key)),
        ("Prefer", prefer),
    ])
}

fn get_prefer_header(save_mode: SaveMode, return_representation: bool) -> String {
    let return_part = if return_representation {
        "return=representation"
    } else {
        "return=minimal"
    };

    match save_mode {
        SaveMode::Insert => return_part.to_string(),
        SaveMode::Update | SaveMode::Upsert => {
            format!("resolution=merge-duplicates,{}", return_part)
        }
    }
}

/// Execute an insert request to Supabase.
pub fn execute_insert<T, F>(connection: &SupabaseConnection, data: &T, table: &str, on_complete: F)
where
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

    let url = connection.table_url(table);
    let prefer = get_prefer_header(SaveMode::Insert, true);

    let request = ehttp::Request {
        method: "POST".to_string(),
        url,
        body: json.into_bytes(),
        headers: build_headers(&connection.api_key, &prefer),
        mode: Default::default(),
        timeout: Some(ehttp::Request::DEFAULT_TIMEOUT),
    };

    ehttp::fetch(request, move |result| {
        on_complete(handle_single_response(result, true));
    });
}

/// Execute an upsert request to Supabase.
///
/// Uses `ON CONFLICT` with the unique columns to either insert or update.
pub fn execute_upsert<T, F>(connection: &SupabaseConnection, data: &T, table: &str, on_complete: F)
where
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

    let url = connection.table_url(table);
    let prefer = get_prefer_header(SaveMode::Upsert, true);

    let request = ehttp::Request {
        method: "POST".to_string(),
        url,
        body: json.into_bytes(),
        headers: build_headers(&connection.api_key, &prefer),
        mode: Default::default(),
        timeout: Some(ehttp::Request::DEFAULT_TIMEOUT),
    };

    ehttp::fetch(request, move |result| {
        on_complete(handle_single_response(result, true));
    });
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

    let url = format!(
        "{}?{}=eq.{}",
        connection.table_url(table),
        pk_column,
        primary_key
    );
    let prefer = "return=minimal";

    let request = ehttp::Request {
        method: "PATCH".to_string(),
        url,
        body: json.into_bytes(),
        headers: build_headers(&connection.api_key, prefer),
        mode: Default::default(),
        timeout: Some(ehttp::Request::DEFAULT_TIMEOUT),
    };

    ehttp::fetch(request, move |result| {
        on_complete(handle_single_response(result, false));
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
        SaveMode::Update => {
            if has_primary_key {
                if let Some(pk) = primary_key {
                    execute_update(
                        connection,
                        data,
                        &table,
                        pk,
                        T::primary_key_column(),
                        move |result| {
                            on_complete(result, false);
                        },
                    );
                } else {
                    execute_upsert(connection, data, &table, move |result| {
                        on_complete(result, true);
                    });
                }
            } else {
                execute_upsert(connection, data, &table, move |result| {
                    on_complete(result, true);
                });
            }
        }
    }
}

fn handle_single_response(
    result: Result<ehttp::Response, String>,
    expect_body: bool,
) -> Result<Option<i64>, RequestError> {
    match result {
        Ok(response) => {
            if response.ok {
                if expect_body {
                    if let Some(body) = response.text()
                        && let Ok(rows) = serde_json::from_str::<Vec<PrimaryKeyResponse>>(body)
                        && let Some(first) = rows.first()
                    {
                        return Ok(Some(first.id));
                    }
                }
                Ok(None)
            } else {
                let body = response.text().map(String::from);
                Err(RequestError::http(
                    format!("Request failed with status {}", response.status),
                    response.status,
                    body,
                ))
            }
        }
        Err(e) => Err(RequestError::network(e)),
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
    if data.is_empty() {
        on_complete(Ok(Vec::new()));
        return;
    }

    let json = match serde_json::to_string(data) {
        Ok(json) => json,
        Err(e) => {
            on_complete(Err(RequestError::serialization(&e)));
            return;
        }
    };

    let url = connection.table_url(table);
    let prefer = get_prefer_header(SaveMode::Insert, true);

    let request = ehttp::Request {
        method: "POST".to_string(),
        url,
        body: json.into_bytes(),
        headers: build_headers(&connection.api_key, &prefer),
        mode: Default::default(),
        timeout: Some(ehttp::Request::DEFAULT_TIMEOUT),
    };

    ehttp::fetch(request, move |result| match result {
        Ok(response) => {
            if response.ok {
                let ids = response
                    .text()
                    .and_then(|body| serde_json::from_str::<Vec<PrimaryKeyResponse>>(body).ok())
                    .map(|rows| rows.into_iter().map(|r| r.id).collect())
                    .unwrap_or_default();
                on_complete(Ok(ids));
            } else {
                let body = response.text().map(String::from);
                on_complete(Err(RequestError::http(
                    format!("Batch insert failed with status {}", response.status),
                    response.status,
                    body,
                )));
            }
        }
        Err(e) => {
            on_complete(Err(RequestError::network(e)));
        }
    });
}

/// Execute a batch upsert request to Supabase.
pub fn execute_batch_upsert<T, F>(
    connection: &SupabaseConnection,
    data: &[T],
    table: &str,
    on_complete: F,
) where
    T: SupabaseRow,
    F: FnOnce(Result<Vec<i64>, RequestError>) + Send + 'static,
{
    if data.is_empty() {
        on_complete(Ok(Vec::new()));
        return;
    }

    let json = match serde_json::to_string(data) {
        Ok(json) => json,
        Err(e) => {
            on_complete(Err(RequestError::serialization(&e)));
            return;
        }
    };

    let url = connection.table_url(table);
    let prefer = get_prefer_header(SaveMode::Upsert, true);

    let request = ehttp::Request {
        method: "POST".to_string(),
        url,
        body: json.into_bytes(),
        headers: build_headers(&connection.api_key, &prefer),
        mode: Default::default(),
        timeout: Some(ehttp::Request::DEFAULT_TIMEOUT),
    };

    ehttp::fetch(request, move |result| match result {
        Ok(response) => {
            if response.ok {
                let ids = response
                    .text()
                    .and_then(|body| serde_json::from_str::<Vec<PrimaryKeyResponse>>(body).ok())
                    .map(|rows| rows.into_iter().map(|r| r.id).collect())
                    .unwrap_or_default();
                on_complete(Ok(ids));
            } else {
                let body = response.text().map(String::from);
                on_complete(Err(RequestError::http(
                    format!("Batch upsert failed with status {}", response.status),
                    response.status,
                    body,
                )));
            }
        }
        Err(e) => {
            on_complete(Err(RequestError::network(e)));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_connection_table_url() {
        let conn = SupabaseConnection::new("https://test.supabase.co", "key123");
        assert_eq!(
            conn.table_url("users"),
            "https://test.supabase.co/rest/v1/users"
        );
    }

    #[test]
    fn test_prefer_header_insert() {
        let prefer = get_prefer_header(SaveMode::Insert, true);
        assert_eq!(prefer, "return=representation");

        let prefer = get_prefer_header(SaveMode::Insert, false);
        assert_eq!(prefer, "return=minimal");
    }

    #[test]
    fn test_prefer_header_upsert() {
        let prefer = get_prefer_header(SaveMode::Upsert, true);
        assert_eq!(prefer, "resolution=merge-duplicates,return=representation");

        let prefer = get_prefer_header(SaveMode::Upsert, false);
        assert_eq!(prefer, "resolution=merge-duplicates,return=minimal");
    }

    #[test]
    fn test_prefer_header_update() {
        let prefer = get_prefer_header(SaveMode::Update, true);
        assert_eq!(prefer, "resolution=merge-duplicates,return=representation");
    }
}
