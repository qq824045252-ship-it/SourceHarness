use serde_json::{json, Value};

use crate::error::{ErrorCode, ErrorPayload};
use crate::read;
use crate::search;
use crate::store::FoldStore;

pub const SEARCH_RESULT_NAME: &str = "search_result";
pub const READ_RESULT_NAME: &str = "read_result";

/// Single-source-of-truth Responses API flat `function` tool declarations
/// derived directly from Rust validation constants in `search` and `read`.
pub fn tool_definitions() -> Vec<Value> {
    vec![search_result_schema(), read_result_schema()]
}

/// Unified dispatch entry point used by `transport` layers.
pub fn dispatch(name: &str, raw_args: &str, store: &FoldStore) -> String {
    match name {
        SEARCH_RESULT_NAME => search::execute_json(raw_args, store),
        READ_RESULT_NAME => read::execute_json(raw_args, store),
        _ => ErrorPayload::new(ErrorCode::InvalidArgument, Some("tool"), "UNKNOWN_TOOL")
            .to_json_string(),
    }
}

fn search_result_schema() -> Value {
    json!({
        "type": "function",
        "name": SEARCH_RESULT_NAME,
        "description": "Search a folded result snapshot with exact matching_lines and total_occurrences counts, optional line-bounded sample hits (max_hits, default 0), and optional regex capture-group aggregation.",
        "parameters": {
            "type": "object",
            "properties": {
                "result_ref": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Immutable folded result reference ID."
                },
                "query": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": search::MAX_QUERY_BYTES,
                    "description": "Literal substring or Rust regex expression (1..=128 UTF-8 bytes)."
                },
                "start_line": {
                    "type": "integer",
                    "minimum": 1,
                    "default": 1,
                    "description": "1-based inclusive start line."
                },
                "end_line": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based inclusive end line; defaults to total_lines."
                },
                "max_hits": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": search::MAX_HITS_LIMIT,
                    "default": search::DEFAULT_MAX_HITS,
                    "description": "Maximum sample lines to return; 0 returns exact counts only."
                },
                "match_mode": {
                    "type": "string",
                    "enum": ["literal", "regex"],
                    "default": "literal",
                    "description": "Matching mode: literal substring or Rust regular expression."
                },
                "case_sensitive": {
                    "type": "boolean",
                    "default": true,
                    "description": "Whether matching is case-sensitive."
                },
                "group_by_capture": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Regex capture group index (0 for whole match, 1+ for capture group). Only valid when match_mode is \"regex\"."
                },
                "top_k": {
                    "type": "integer",
                    "minimum": search::MIN_TOP_K,
                    "maximum": search::MAX_TOP_K,
                    "default": search::DEFAULT_TOP_K,
                    "description": "Max entries in aggregation.value_counts; only valid when group_by_capture is set."
                }
            },
            "required": search::REQUIRED_FIELDS,
            "additionalProperties": false
        }
    })
}

fn read_result_schema() -> Value {
    json!({
        "type": "function",
        "name": READ_RESULT_NAME,
        "description": "Read whole 1-based lines from a folded result snapshot within the 8 KiB response budget.",
        "parameters": {
            "type": "object",
            "properties": {
                "result_ref": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Immutable folded result reference ID."
                },
                "start_line": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based inclusive start line."
                },
                "line_count": {
                    "type": "integer",
                    "minimum": read::MIN_LINE_COUNT,
                    "maximum": read::MAX_LINE_COUNT,
                    "description": "Number of lines requested (1..=4096)."
                }
            },
            "required": read::REQUIRED_FIELDS,
            "additionalProperties": false
        }
    })
}
