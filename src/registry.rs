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
        "description": "Search a folded result for exact counts, matching lines, or regex groups. Omit line bounds for one full-snapshot scan. Use matching_lines for a line count and total_occurrences for a match count. Once the query and scanned_lines cover the request, answer from those counts. Omitted or truncated samples do not affect the counts. Request more evidence only when needed to answer the user.",
        "parameters": {
            "type": "object",
            "properties": {
                "result_ref": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Copy result_ref from the folded result envelope."
                },
                "query": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": search::MAX_QUERY_BYTES,
                    "description": "Use an exact literal substring when possible. Regex mode uses Rust regex without lookaround or backreferences. Up to 128 UTF-8 bytes."
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
                    "description": "Sample lines to return when examples are needed. Omit or use 0 for counts only."
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
                    "description": "Set only to group regex matches. Requires match_mode=regex. Use 0 for whole matches or 1+ for a capture group in query."
                },
                "top_k": {
                    "type": "integer",
                    "minimum": search::MIN_TOP_K,
                    "maximum": search::MAX_TOP_K,
                    "default": search::DEFAULT_TOP_K,
                    "description": "Max entries in aggregation.value_counts. Omit unless group_by_capture is set."
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
        "description": "Read a requested range of whole lines or context around a match. If next_start_line is returned, continue there for the remaining requested lines. Use search_result when the task only needs a count.",
        "parameters": {
            "type": "object",
            "properties": {
                "result_ref": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Copy result_ref from the folded result envelope."
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
