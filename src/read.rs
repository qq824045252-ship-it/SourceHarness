use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::budget::{json_byte_len, to_json_string, MAX_RESPONSE_BYTES};
use crate::error::{ErrorCode, ErrorPayload, OkEnvelope};
use crate::store::{FoldStore, Snapshot};

pub const MIN_LINE_COUNT: usize = 1;
pub const MAX_LINE_COUNT: usize = 4096;

pub const ALLOWED_FIELDS: &[&str] = &["result_ref", "start_line", "line_count"];
pub const REQUIRED_FIELDS: &[&str] = &["result_ref", "start_line", "line_count"];

/// Strongly typed Rust request structure for `read_result` (unknown fields such as `offset` denied).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadRequest {
    pub result_ref: String,
    pub start_line: usize,
    pub line_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReadData {
    pub result_ref: String,
    pub content: String,
    pub returned_line_range: [usize; 2],
    pub total_lines: usize,
    pub complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_start_line: Option<usize>,
}

/// Executes `read_result` from raw JSON arguments and returns bounded JSON (`ok` or `error`).
pub fn execute_json(raw_args: &str, store: &FoldStore) -> String {
    match execute_inner(raw_args, store) {
        Ok(envelope) => to_json_string(&envelope),
        Err(err) => err.to_json_string(),
    }
}

/// Typed Rust entry point for `read_result`.
pub fn execute(req: &ReadRequest, store: &FoldStore) -> Result<OkEnvelope<ReadData>, ErrorPayload> {
    let raw = serde_json::to_string(req).map_err(|_| {
        ErrorPayload::new(ErrorCode::InvalidArgument, None, "INVALID_JSON")
    })?;
    execute_inner(&raw, store)
}

fn execute_inner(raw_args: &str, store: &FoldStore) -> Result<OkEnvelope<ReadData>, ErrorPayload> {
    let (result_ref, start_line, line_count) = parse_and_validate_args(raw_args)?;
    let snapshot = store.get(&result_ref).ok_or_else(|| {
        ErrorPayload::new(
            ErrorCode::ResultNotFound,
            Some("result_ref"),
            "UNKNOWN_OR_EVICTED_REF",
        )
    })?;

    run_read(&result_ref, start_line, line_count, &snapshot)
}

fn parse_and_validate_args(raw_args: &str) -> Result<(String, usize, usize), ErrorPayload> {
    let value: Value = serde_json::from_str(raw_args).map_err(|_| {
        ErrorPayload::new(ErrorCode::InvalidArgument, None, "INVALID_JSON")
    })?;
    let obj = value.as_object().ok_or_else(|| {
        ErrorPayload::new(ErrorCode::InvalidArgument, None, "EXPECTED_JSON_OBJECT")
    })?;

    for key in obj.keys() {
        if !ALLOWED_FIELDS.contains(&key.as_str()) {
            return Err(ErrorPayload::new(
                ErrorCode::InvalidArgument,
                Some(key.as_str()),
                "UNSUPPORTED_FIELD",
            ));
        }
    }

    for &req in REQUIRED_FIELDS {
        match obj.get(req) {
            None | Some(Value::Null) => {
                return Err(ErrorPayload::new(
                    ErrorCode::InvalidArgument,
                    Some(req),
                    "MISSING_REQUIRED_FIELD",
                ));
            }
            _ => {}
        }
    }

    let result_ref = match obj.get("result_ref") {
        Some(Value::String(s)) => s.clone(),
        _ => {
            return Err(ErrorPayload::new(
                ErrorCode::InvalidArgument,
                Some("result_ref"),
                "INVALID_TYPE",
            ));
        }
    };
    if result_ref.is_empty() {
        return Err(ErrorPayload::new(
            ErrorCode::InvalidArgument,
            Some("result_ref"),
            "INVALID_RESULT_REF",
        ));
    }

    let start_line_i64 = require_i64(obj, "start_line")?;
    let line_count_i64 = require_i64(obj, "line_count")?;

    if line_count_i64 < MIN_LINE_COUNT as i64 || line_count_i64 > MAX_LINE_COUNT as i64 {
        return Err(ErrorPayload::new(
            ErrorCode::InvalidArgument,
            Some("line_count"),
            "INVALID_LINE_COUNT",
        ));
    }

    if start_line_i64 < 1 {
        return Err(ErrorPayload::new(
            ErrorCode::InvalidLineRange,
            Some("start_line"),
            "OUT_OF_BOUNDS",
        ));
    }

    Ok((result_ref, start_line_i64 as usize, line_count_i64 as usize))
}

fn require_i64(obj: &Map<String, Value>, field: &str) -> Result<i64, ErrorPayload> {
    match obj.get(field) {
        Some(Value::Number(n)) => n.as_i64().ok_or_else(|| {
            ErrorPayload::new(ErrorCode::InvalidArgument, Some(field), "INVALID_TYPE")
        }),
        _ => Err(ErrorPayload::new(
            ErrorCode::InvalidArgument,
            Some(field),
            "INVALID_TYPE",
        )),
    }
}

fn run_read(
    result_ref: &str,
    start_line: usize,
    line_count: usize,
    snapshot: &Snapshot,
) -> Result<OkEnvelope<ReadData>, ErrorPayload> {
    let total_lines = snapshot.total_lines();
    if total_lines == 0 || start_line > total_lines {
        return Err(ErrorPayload::new(
            ErrorCode::InvalidLineRange,
            Some("start_line"),
            "OUT_OF_BOUNDS",
        ));
    }

    let target_end = start_line.saturating_add(line_count - 1).min(total_lines);

    let first_line_env = build_envelope(snapshot, result_ref, start_line, start_line, target_end);
    if json_byte_len(&first_line_env) > MAX_RESPONSE_BYTES {
        return Err(ErrorPayload::new(
            ErrorCode::UnreadableSingleLine,
            Some("start_line"),
            "EXCEEDS_RESPONSE_BUDGET",
        ));
    }

    if target_end == start_line {
        return Ok(first_line_env);
    }

    // Fast path: check full requested range if raw slice fits within 8 KiB.
    if snapshot
        .raw_range(start_line, target_end)
        .is_some_and(|s| s.len() <= MAX_RESPONSE_BYTES)
    {
        let full_env = build_envelope(snapshot, result_ref, start_line, target_end, target_end);
        if json_byte_len(&full_env) <= MAX_RESPONSE_BYTES {
            return Ok(full_env);
        }
    }

    // Binary search largest `end` in `start_line..target_end` (all have `complete = false`,
    // so `json_byte_len` grows monotonically with `end`).
    let mut lo = start_line;
    let mut hi = target_end - 1;
    let mut best_env = first_line_env;

    while lo <= hi {
        let mid = lo + (hi - lo) / 2;
        let raw_fits = snapshot
            .raw_range(start_line, mid)
            .is_some_and(|s| s.len() <= MAX_RESPONSE_BYTES);
        if raw_fits {
            let candidate = build_envelope(snapshot, result_ref, start_line, mid, target_end);
            if json_byte_len(&candidate) <= MAX_RESPONSE_BYTES {
                best_env = candidate;
                lo = mid + 1;
                continue;
            }
        }
        if mid == start_line {
            break;
        }
        hi = mid - 1;
    }

    Ok(best_env)
}

fn build_envelope(
    snapshot: &Snapshot,
    result_ref: &str,
    start_line: usize,
    end_line: usize,
    target_end: usize,
) -> OkEnvelope<ReadData> {
    let content = snapshot
        .raw_range(start_line, end_line)
        .expect("line range already validated")
        .to_owned();
    let complete = end_line == target_end;
    OkEnvelope::new(ReadData {
        result_ref: result_ref.to_owned(),
        content,
        returned_line_range: [start_line, end_line],
        total_lines: snapshot.total_lines(),
        complete,
        next_start_line: if complete { None } else { Some(end_line + 1) },
    })
}
