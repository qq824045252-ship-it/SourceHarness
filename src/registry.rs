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

pub(crate) fn search_result_schema() -> Value {
    json!({
        "type": "function",
        "name": SEARCH_RESULT_NAME,
        "description": "Search a folded result for exact counts, sample lines, or regex groups. Omit line bounds to scan the whole snapshot. Use matching_lines for a line count and total_occurrences for a match count. Attribute each count to its query and scanned_lines. Sample limits affect only the displayed lines. When the counts cover the user's request, answer from those counts.",
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
                    "description": "Search within each line. Prefer an exact literal substring. Regex mode uses Rust regex without lookaround or backreferences. Up to 128 UTF-8 bytes."
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
                    "description": format!("Maximum matching-line samples to display (0-{}). Default 0 for counts only. The full requested range is scanned at every setting.", search::MAX_HITS_LIMIT)
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
                    "description": "Group regex matches. Requires match_mode=regex. Use 0 for the whole match or the index of an existing capture group in query."
                },
                "top_k": {
                    "type": "integer",
                    "minimum": search::MIN_TOP_K,
                    "maximum": search::MAX_TOP_K,
                    "description": format!("Maximum groups in aggregation.value_counts ({}-{}). Use only with group_by_capture, which defaults to returning {} groups.", search::MIN_TOP_K, search::MAX_TOP_K, search::DEFAULT_TOP_K)
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
