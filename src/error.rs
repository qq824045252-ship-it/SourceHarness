use serde::{Deserialize, Serialize};

/// Canonical error codes returned by `search_result`, `read_result`, and `registry::dispatch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    ResultNotFound,
    InvalidArgument,
    InvalidMaxHits,
    InvalidLineRange,
    InvalidRegex,
    InvalidGroupByCapture,
    InvalidTopK,
    LineTooLong,
    TooManyMatchesOnLine,
    UnreadableSingleLine,
}

/// Slim, deterministic error payload allowing model self-recovery without stack traces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorPayload {
    pub status: &'static str,
    pub code: ErrorCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    pub reason: &'static str,
    pub same_input_retry: bool,
}

impl ErrorPayload {
    /// Creates a recoverable error payload with `status = "error"` and `same_input_retry = false`.
    pub fn new(code: ErrorCode, field: Option<&str>, reason: &'static str) -> Self {
        Self {
            status: "error",
            code,
            field: field.map(ToOwned::to_owned),
            reason,
            same_input_retry: false,
        }
    }

    /// Serializes the error payload into compact JSON.
    pub fn to_json_string(&self) -> String {
        serde_json::to_string(self).expect("ErrorPayload serialization is infallible")
    }
}

/// Standard success wrapper `{"status":"ok","data":...}` for tool responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OkEnvelope<T> {
    pub status: &'static str,
    pub data: T,
}

impl<T> OkEnvelope<T> {
    /// Wraps tool output data in `status = "ok"`.
    pub fn new(data: T) -> Self {
        Self { status: "ok", data }
    }
}
