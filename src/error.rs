//! Error type for Supabase request failures.

/// Error returned when a Supabase HTTP request fails.
#[derive(Debug, Clone)]
pub struct RequestError {
    /// Human-readable description of the failure.
    pub message: String,

    /// HTTP status code, if a response was received.
    pub status: Option<u16>,

    /// Raw response body, if available.
    pub body: Option<String>,
}

impl RequestError {
    pub(crate) fn network(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status: None,
            body: None,
        }
    }

    pub(crate) fn http(message: impl Into<String>, status: u16, body: Option<String>) -> Self {
        Self {
            message: message.into(),
            status: Some(status),
            body,
        }
    }

    pub(crate) fn serialization(err: &serde_json::Error) -> Self {
        Self {
            message: format!("Serialization failed: {}", err),
            status: None,
            body: None,
        }
    }
}
