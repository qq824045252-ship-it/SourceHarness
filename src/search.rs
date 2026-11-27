use std::collections::HashMap;

use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::budget::{json_byte_len, to_json_string, truncate_utf8_bytes, MAX_HIT_TEXT_BYTES, MAX_RESPONSE_BYTES};
use crate::error::{CaptureGroupBounds, ErrorCode, ErrorPayload, OkEnvelope};
use crate::store::{FoldStore, Snapshot};

pub const MAX_QUERY_BYTES: usize = 128;
pub const DEFAULT_MAX_HITS: usize = 0;
pub const MAX_HITS_LIMIT: usize = 20;
pub const MIN_TOP_K: usize = 1;
pub const MAX_TOP_K: usize = 50;
pub const DEFAULT_TOP_K: usize = 20;
pub const REGEX_SIZE_LIMIT: usize = 512 * 1024;
pub const REGEX_DFA_SIZE_LIMIT: usize = 1024 * 1024;
pub const MAX_LINE_BYTES: usize = 64 * 1024;
pub const MAX_MATCHES_PER_LINE: usize = 256;
pub(crate) const MULTILINE_LITERAL_UNSUPPORTED: &str = "MULTILINE_LITERAL_UNSUPPORTED";

pub const ALLOWED_FIELDS: &[&str] = &[
    "result_ref",
    "query",
    "start_line",
    "end_line",
    "max_hits",
    "match_mode",
    "case_sensitive",
    "group_by_capture",
    "top_k",
];

pub const REQUIRED_FIELDS: &[&str] = &["result_ref", "query"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchMode {
    #[default]
    Literal,
    Regex,
}

impl MatchMode {
    fn rule_name(self, case_sensitive: bool) -> &'static str {
        match (self, case_sensitive) {
            (MatchMode::Literal, true) => "literal_case_sensitive",
            (MatchMode::Literal, false) => "literal_case_insensitive",
            (MatchMode::Regex, true) => "regex_case_sensitive",
            (MatchMode::Regex, false) => "regex_case_insensitive",
        }
    }
}

/// Strongly typed Rust request structure for `search_result` (unknown fields denied).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchRequest {
    pub result_ref: String,
    pub query: String,
    #[serde(default)]
    pub start_line: Option<usize>,
    #[serde(default)]
    pub end_line: Option<usize>,
    #[serde(default)]
    pub max_hits: Option<usize>,
    #[serde(default)]
    pub match_mode: Option<MatchMode>,
    #[serde(default)]
    pub case_sensitive: Option<bool>,
    #[serde(default)]
    pub group_by_capture: Option<usize>,
    #[serde(default)]
    pub top_k: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchHit {
    pub line: usize,
    pub partial: bool,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CaptureValueCount {
    pub value: String,
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CaptureAggregation {
    pub capture_group: usize,
    pub distinct_values: usize,
    pub total_captured: usize,
    pub value_counts: Vec<CaptureValueCount>,
    pub value_counts_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchData {
    pub result_ref: String,
    pub query: String,
    pub match_rule: &'static str,
    pub count_unit: &'static str,
    pub total_lines: usize,
    pub scanned_lines: [usize; 2],
    pub complete: bool,
    pub matching_lines: usize,
    pub total_occurrences: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hits: Option<Vec<SearchHit>>,
    #[serde(skip_serializing_if = "is_false")]
    pub hits_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub literal_mode_hint: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregation: Option<CaptureAggregation>,
}

fn is_false(v: &bool) -> bool {
    !*v
}

struct ParsedArgs {
    result_ref: String,
    query: String,
    start_line: usize,
    end_line: Option<usize>,
    max_hits: usize,
    match_mode: MatchMode,
    match_mode_explicit: bool,
    case_sensitive: bool,
    group_by_capture: Option<usize>,
    top_k: usize,
}

/// Executes `search_result` from raw JSON arguments and returns bounded JSON (`ok` or `error`).
pub fn execute_json(raw_args: &str, store: &FoldStore) -> String {
    match execute_inner(raw_args, store) {
        Ok(envelope) => to_json_string(&envelope),
        Err(err) => err.to_json_string(),
    }
}

/// Typed Rust entry point for `search_result`.
pub fn execute(req: &SearchRequest, store: &FoldStore) -> Result<OkEnvelope<SearchData>, ErrorPayload> {
    let raw = serde_json::to_string(req).map_err(|_| {
        ErrorPayload::new(ErrorCode::InvalidArgument, None, "INVALID_JSON")
    })?;
    execute_inner(&raw, store)
}

impl Serialize for SearchRequest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("result_ref", &self.result_ref)?;
        map.serialize_entry("query", &self.query)?;
        if let Some(v) = self.start_line {
            map.serialize_entry("start_line", &v)?;
        }
        if let Some(v) = self.end_line {
            map.serialize_entry("end_line", &v)?;
        }
        if let Some(v) = self.max_hits {
            map.serialize_entry("max_hits", &v)?;
        }
        if let Some(v) = self.match_mode {
            map.serialize_entry("match_mode", &v)?;
        }
        if let Some(v) = self.case_sensitive {
            map.serialize_entry("case_sensitive", &v)?;
        }
        if let Some(v) = self.group_by_capture {
            map.serialize_entry("group_by_capture", &v)?;
        }
        if let Some(v) = self.top_k {
            map.serialize_entry("top_k", &v)?;
        }
        map.end()
    }
}

fn execute_inner(
    raw_args: &str,
    store: &FoldStore,
) -> Result<OkEnvelope<SearchData>, ErrorPayload> {
    let args = parse_and_validate_args(raw_args)?;
    let snapshot = store.get(&args.result_ref).ok_or_else(unknown_reference)?;

    run_search(&args, &snapshot)
}

fn unknown_reference() -> ErrorPayload {
    ErrorPayload::new(
        ErrorCode::ResultNotFound,
        Some("result_ref"),
        "UNKNOWN_OR_EVICTED_REF",
    )
}

pub(crate) fn execute_snapshot(
    raw_args: &str,
    load: impl FnOnce(&str) -> Result<Option<Snapshot>, String>,
) -> Result<Result<OkEnvelope<SearchData>, ErrorPayload>, String> {
    let args = match parse_and_validate_args(raw_args) {
        Ok(args) => args,
        Err(err) => return Ok(Err(err)),
    };
    Ok(load(&args.result_ref)?
        .ok_or_else(unknown_reference)
        .and_then(|snapshot| run_search(&args, &snapshot)))
}

fn parse_and_validate_args(raw_args: &str) -> Result<ParsedArgs, ErrorPayload> {
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

    let result_ref = require_str(obj, "result_ref")?;
    if result_ref.is_empty() {
        return Err(ErrorPayload::new(
            ErrorCode::InvalidArgument,
            Some("result_ref"),
            "INVALID_RESULT_REF",
        ));
    }

    let query = require_str(obj, "query")?;
    if query.is_empty() || query.len() > MAX_QUERY_BYTES {
        return Err(ErrorPayload::new(
            ErrorCode::InvalidArgument,
            Some("query"),
            "INVALID_QUERY_LENGTH",
        ));
    }

    let max_hits_i64 = optional_i64(obj, "max_hits")?.unwrap_or(DEFAULT_MAX_HITS as i64);
    if max_hits_i64 > MAX_HITS_LIMIT as i64 {
        return Err(ErrorPayload::new(
            ErrorCode::InvalidMaxHits,
            Some("max_hits"),
            "EXCEEDS_20",
        ));
    }
    if max_hits_i64 < 0 {
        return Err(ErrorPayload::new(
            ErrorCode::InvalidMaxHits,
            Some("max_hits"),
            "OUT_OF_RANGE",
        ));
    }
    let max_hits = max_hits_i64 as usize;

    let (match_mode, match_mode_explicit) = match optional_str(obj, "match_mode")? {
        None => (MatchMode::Literal, false),
        Some("literal") => (MatchMode::Literal, true),
        Some("regex") => (MatchMode::Regex, true),
        Some(_) => {
            return Err(ErrorPayload::new(
                ErrorCode::InvalidArgument,
                Some("match_mode"),
                "INVALID_MATCH_MODE",
            ));
        }
    };
    if match_mode == MatchMode::Literal && query.contains('\n') {
        let mut error = ErrorPayload::new(
            ErrorCode::InvalidArgument,
            Some("query"),
            MULTILINE_LITERAL_UNSUPPORTED,
        );
        error.hint = Some("Search matches within each line. Read the requested range to inspect a cross-line literal.");
        return Err(error);
    }

    let case_sensitive = optional_bool(obj, "case_sensitive")?.unwrap_or(true);

    let group_by_capture = match optional_i64(obj, "group_by_capture")? {
        None => None,
        Some(idx) => {
            if match_mode != MatchMode::Regex {
                return Err(ErrorPayload::new(
                    ErrorCode::InvalidGroupByCapture,
                    Some("group_by_capture"),
                    "REQUIRES_REGEX_MODE",
                ));
            }
            if idx < 0 {
                return Err(ErrorPayload::new(
                    ErrorCode::InvalidGroupByCapture,
                    Some("group_by_capture"),
                    "CAPTURE_GROUP_OUT_OF_RANGE",
                ));
            }
            Some(idx as usize)
        }
    };

    let top_k = match optional_i64(obj, "top_k")? {
        None => DEFAULT_TOP_K,
        Some(k) => {
            if group_by_capture.is_none() {
                return Err(ErrorPayload::new(
                    ErrorCode::InvalidArgument,
                    Some("top_k"),
                    "TOP_K_REQUIRES_GROUP_BY_CAPTURE",
                ));
            }
            if k < MIN_TOP_K as i64 || k > MAX_TOP_K as i64 {
                return Err(ErrorPayload::new(
                    ErrorCode::InvalidTopK,
                    Some("top_k"),
                    "OUT_OF_RANGE",
                ));
            }
            k as usize
        }
    };

    let start_line_i64 = optional_i64(obj, "start_line")?.unwrap_or(1);
    if start_line_i64 < 1 {
        return Err(ErrorPayload::new(
            ErrorCode::InvalidLineRange,
            Some("start_line"),
            "OUT_OF_BOUNDS",
        ));
    }
    let start_line = start_line_i64 as usize;

    let end_line = match optional_i64(obj, "end_line")? {
        None => None,
        Some(end_i64) => {
            if end_i64 < start_line_i64 {
                return Err(ErrorPayload::new(
                    ErrorCode::InvalidLineRange,
                    Some("end_line"),
                    "OUT_OF_BOUNDS",
                ));
            }
            Some(end_i64 as usize)
        }
    };

    Ok(ParsedArgs {
        result_ref,
        query,
        start_line,
        end_line,
        max_hits,
        match_mode,
        match_mode_explicit,
        case_sensitive,
        group_by_capture,
        top_k,
    })
}

fn require_str(obj: &Map<String, Value>, field: &str) -> Result<String, ErrorPayload> {
    match obj.get(field) {
        Some(Value::String(s)) => Ok(s.clone()),
        _ => Err(ErrorPayload::new(
            ErrorCode::InvalidArgument,
            Some(field),
            "INVALID_TYPE",
        )),
    }
}

fn optional_str<'a>(obj: &'a Map<String, Value>, field: &str) -> Result<Option<&'a str>, ErrorPayload> {
    match obj.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(ErrorPayload::new(
            ErrorCode::InvalidArgument,
            Some(field),
            "INVALID_TYPE",
        )),
    }
}

fn optional_i64(obj: &Map<String, Value>, field: &str) -> Result<Option<i64>, ErrorPayload> {
    match obj.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n.as_i64().map(Some).ok_or_else(|| {
            ErrorPayload::new(ErrorCode::InvalidArgument, Some(field), "INVALID_TYPE")
        }),
        Some(_) => Err(ErrorPayload::new(
            ErrorCode::InvalidArgument,
            Some(field),
            "INVALID_TYPE",
        )),
    }
}

fn optional_bool(obj: &Map<String, Value>, field: &str) -> Result<Option<bool>, ErrorPayload> {
    match obj.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(ErrorPayload::new(
            ErrorCode::InvalidArgument,
            Some(field),
            "INVALID_TYPE",
        )),
    }
}

enum CompiledMatcher<'a> {
    LiteralSensitive(&'a str),
    Regex(Regex),
}

fn regex_compile_error(error: regex::Error) -> ErrorPayload {
    let mut payload = ErrorPayload::new(ErrorCode::InvalidRegex, Some("query"), "REGEX_COMPILE_ERROR");
    if let regex::Error::Syntax(message) = error {
        let diagnostic = message.lines().last().unwrap_or_default();
        if diagnostic.contains("look-around") {
            payload.hint = Some("Rust regex does not support lookaround. Use an anchored positive pattern or inspect the matching range.");
        } else if diagnostic.contains("backreferences are not supported") {
            payload.hint = Some("Rust regex does not support backreferences. Capture values for grouping or read the relevant text to compare strings.");
        }
    }
    payload
}

fn build_matcher<'a>(args: &'a ParsedArgs) -> Result<CompiledMatcher<'a>, ErrorPayload> {
    match (args.match_mode, args.case_sensitive) {
        (MatchMode::Literal, true) => Ok(CompiledMatcher::LiteralSensitive(&args.query)),
        (MatchMode::Literal, false) => {
            let escaped = regex::escape(&args.query);
            let re = RegexBuilder::new(&escaped)
                .case_insensitive(true)
                .size_limit(REGEX_SIZE_LIMIT)
                .dfa_size_limit(REGEX_DFA_SIZE_LIMIT)
                .build()
                .map_err(regex_compile_error)?;
            Ok(CompiledMatcher::Regex(re))
        }
        (MatchMode::Regex, cs) => {
            let re = RegexBuilder::new(&args.query)
                .case_insensitive(!cs)
                .size_limit(REGEX_SIZE_LIMIT)
                .dfa_size_limit(REGEX_DFA_SIZE_LIMIT)
                .build()
                .map_err(regex_compile_error)?;
            if let Some(group_idx) = args.group_by_capture {
                if group_idx >= re.captures_len() {
                    let mut error = ErrorPayload::new(
                        ErrorCode::InvalidGroupByCapture,
                        Some("group_by_capture"),
                        "CAPTURE_GROUP_OUT_OF_RANGE",
                    );
                    error.capture_group_bounds = Some(CaptureGroupBounds {
                        requested: group_idx,
                        min: 0,
                        max: re.captures_len() - 1,
                    });
                    return Err(error);
                }
            }
            Ok(CompiledMatcher::Regex(re))
        }
    }
}

fn run_search(args: &ParsedArgs, snapshot: &Snapshot) -> Result<OkEnvelope<SearchData>, ErrorPayload> {
    let total_lines = snapshot.total_lines();
    if total_lines == 0 || args.start_line > total_lines {
        return Err(ErrorPayload::new(
            ErrorCode::InvalidLineRange,
            Some("start_line"),
            "OUT_OF_BOUNDS",
        ));
    }

    let actual_end = args.end_line.unwrap_or(total_lines).min(total_lines);
    if actual_end < args.start_line {
        return Err(ErrorPayload::new(
            ErrorCode::InvalidLineRange,
            Some("end_line"),
            "OUT_OF_BOUNDS",
        ));
    }

    let matcher = build_matcher(args)?;

    let mut matching_lines = 0usize;
    let mut total_occurrences = 0usize;
    let mut hits: Vec<SearchHit> = Vec::new();
    let mut total_captured = 0usize;
    let mut capture_counts: HashMap<String, usize> = HashMap::new();

    for line_no in args.start_line..=actual_end {
        let line_text = snapshot
            .line_text(line_no)
            .expect("line_no is within 1..=total_lines");
        if line_text.len() > MAX_LINE_BYTES {
            return Err(ErrorPayload::new(
                ErrorCode::LineTooLong,
                Some("start_line"),
                "EXCEEDS_MAX_LINE_BYTES",
            ));
        }

        let line_matches = match (&matcher, args.group_by_capture) {
            (CompiledMatcher::LiteralSensitive(pat), _) => {
                let mut count = 0usize;
                for _ in line_text.match_indices(pat) {
                    count += 1;
                    if count > MAX_MATCHES_PER_LINE {
                        return Err(ErrorPayload::new(
                            ErrorCode::TooManyMatchesOnLine,
                            Some("query"),
                            "EXCEEDS_MAX_MATCHES_PER_LINE",
                        ));
                    }
                }
                count
            }
            (CompiledMatcher::Regex(re), Some(group_idx)) => {
                let mut count = 0usize;
                for caps in re.captures_iter(line_text) {
                    count += 1;
                    if count > MAX_MATCHES_PER_LINE {
                        return Err(ErrorPayload::new(
                            ErrorCode::TooManyMatchesOnLine,
                            Some("query"),
                            "EXCEEDS_MAX_MATCHES_PER_LINE",
                        ));
                    }
                    if let Some(m) = caps.get(group_idx) {
                        total_captured += 1;
                        *capture_counts.entry(m.as_str().to_owned()).or_insert(0) += 1;
                    }
                }
                count
            }
            (CompiledMatcher::Regex(re), None) => {
                let mut count = 0usize;
                for _ in re.find_iter(line_text) {
                    count += 1;
                    if count > MAX_MATCHES_PER_LINE {
                        return Err(ErrorPayload::new(
                            ErrorCode::TooManyMatchesOnLine,
                            Some("query"),
                            "EXCEEDS_MAX_MATCHES_PER_LINE",
                        ));
                    }
                }
                count
            }
        };

        if line_matches > 0 {
            matching_lines += 1;
            total_occurrences += line_matches;
            if args.max_hits > 0 && hits.len() < args.max_hits {
                let (snippet, partial) = truncate_utf8_bytes(line_text, MAX_HIT_TEXT_BYTES);
                hits.push(SearchHit {
                    line: line_no,
                    partial,
                    text: snippet.to_owned(),
                });
            }
        }
    }

    let (hits_opt, hits_truncated) = if args.max_hits > 0 && matching_lines > 0 {
        let truncated = matching_lines > hits.len();
        (Some(hits), truncated)
    } else {
        (None, false)
    };

    let literal_mode_hint = if !args.match_mode_explicit
        && args.match_mode == MatchMode::Literal
        && matching_lines == 0
        && looks_like_regex(&args.query)
    {
        Some("Zero literal matches found; query contains regex metacharacters. Retry with match_mode=\"regex\" if you intended a regex search.")
    } else {
        None
    };

    let aggregation = args.group_by_capture.map(|capture_group| {
        let distinct_values = capture_counts.len();
        let mut sorted: Vec<CaptureValueCount> = capture_counts
            .into_iter()
            .map(|(value, count)| CaptureValueCount { value, count })
            .collect();
        sorted.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.value.cmp(&b.value)));
        let truncated = sorted.len() > args.top_k;
        sorted.truncate(args.top_k);
        CaptureAggregation {
            capture_group,
            distinct_values,
            total_captured,
            value_counts: sorted,
            value_counts_truncated: truncated,
        }
    });

    let mut envelope = OkEnvelope::new(SearchData {
        result_ref: args.result_ref.clone(),
        query: args.query.clone(),
        match_rule: args.match_mode.rule_name(args.case_sensitive),
        count_unit: "matching_lines",
        total_lines,
        scanned_lines: [args.start_line, actual_end],
        complete: true,
        matching_lines,
        total_occurrences,
        hits: hits_opt,
        hits_truncated,
        literal_mode_hint,
        aggregation,
    });

    enforce_response_budget(&mut envelope, MAX_RESPONSE_BYTES);
    Ok(envelope)
}

/// Deterministic 8 KiB budget trim (SPEC §4.3):
/// 1) Pop tail of `hits` and mark `hits_truncated = true`.
/// 2) Pop tail of `aggregation.value_counts` and mark `value_counts_truncated = true`.
///
/// Scalar truth fields are NEVER altered.
pub(crate) fn enforce_response_budget(envelope: &mut OkEnvelope<SearchData>, max_bytes: usize) {
    while json_byte_len(envelope) > max_bytes {
        if let Some(hits) = envelope.data.hits.as_mut() {
            if !hits.is_empty() {
                hits.pop();
                envelope.data.hits_truncated = true;
                continue;
            }
        }
        if let Some(agg) = envelope.data.aggregation.as_mut() {
            if !agg.value_counts.is_empty() {
                agg.value_counts.pop();
                agg.value_counts_truncated = true;
                continue;
            }
        }
        break;
    }
}

fn looks_like_regex(query: &str) -> bool {
    const TOKENS: &[&str] = &[
        "|", "\\d", "\\w", "\\s", "\\b", "\\D", "\\W", "\\S", "\\B", ".*", ".+", "^", "$",
    ];
    if TOKENS.iter().any(|tok| query.contains(tok)) {
        return true;
    }
    has_nonempty_bracket_pair(query, '[', ']') || has_nonempty_bracket_pair(query, '(', ')')
}

fn has_nonempty_bracket_pair(s: &str, open: char, close: char) -> bool {
    let mut open_idx = None;
    for (idx, ch) in s.char_indices() {
        if ch == open && open_idx.is_none() {
            open_idx = Some(idx);
        } else if ch == close {
            if let Some(start) = open_idx {
                if idx > start + open.len_utf8() {
                    return true;
                }
            }
        }
    }
    false
}
