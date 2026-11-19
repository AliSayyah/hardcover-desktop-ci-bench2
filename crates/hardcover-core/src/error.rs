//! Error type shared by the whole crate.

use std::time::Duration;

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can go wrong. Messages never contain tokens or other secrets.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The token is missing, invalid or expired and could not be refreshed. Sign in again.
    #[error("not signed in, or the session has expired")]
    Unauthorized,
    /// HTTP 403 `insufficient_scope`: the token lacks a scope (`scope` names it when the server says).
    #[error("the token is missing a required scope{}", .scope.as_deref().map(|s| format!(" ({s})")).unwrap_or_default())]
    InsufficientScope { scope: Option<String> },
    /// HTTP 429. `retry_after` comes from the `Retry-After` header when present.
    #[error("rate limited by Hardcover, try again later")]
    RateLimited { retry_after: Option<Duration> },
    /// The request (or the server, HTTP 408) timed out.
    #[error("the request timed out")]
    Timeout,
    /// Transport failure or a 5xx response.
    #[error("network error: {0}")]
    Network(String),
    /// GraphQL `errors[]`, a mutation's `error` field, or another 4xx the API explained.
    #[error("Hardcover API error: {0}")]
    Api(String),
    /// The response did not have the expected shape.
    #[error("unexpected response from Hardcover: {0}")]
    Decode(String),
    /// Local storage (SQLite, keyring, cover files) failed.
    #[error("storage error: {0}")]
    Storage(String),
    /// OAuth failure (state/issuer mismatch, user denied access, token exchange failed, ...).
    #[error("sign-in failed: {0}")]
    Auth(String),
    /// The caller passed something the API cannot accept (checked before any request is sent).
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// Checking for, verifying or installing an app update failed.
    #[error("update failed: {0}")]
    Update(String),
    /// The operation was cancelled through a [`crate::auth::CancelHandle`].
    #[error("cancelled")]
    Cancelled,
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Storage(e.to_string())
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Storage(e.to_string())
    }
}
