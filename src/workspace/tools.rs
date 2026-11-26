use std::fs;
use std::path::Path;

use regex::{Regex, RegexBuilder};
use serde_json::{json, Map, Value};

use crate::error::{ErrorCode, ErrorPayload};
use crate::search::{MAX_QUERY_BYTES, REGEX_DFA_SIZE_LIMIT, REGEX_SIZE_LIMIT};
use crate::transport::ToolExecutionOutput;
use crate::workspace::{WorkspaceContext, WorkspaceToolRouter};

pub const LIST_FILES_NAME: &str = "list_files";
pub const READ_FILE_NAME: &str = "read_file";
pub const GREP_FILES_NAME: &str = "grep_files";

const DEFAULT_MAX_DEPTH: usize = 6;
const MIN_MAX_DEPTH: usize = 1;
const MAX_MAX_DEPTH: usize = 16;

pub fn producer_tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "name": LIST_FILES_NAME,
            "description": "List files and directories inside the workspace. Small listings return inline; large listings fold into FoldStore for search_result/read_result.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "default": ".",
                        "description": "Relative directory path inside the workspace (defaults to \".\")."
                    },
                    "max_depth": {
                        "type": "integer",
                        "minimum": MIN_MAX_DEPTH,
                        "maximum": MAX_MAX_DEPTH,
                        "default": DEFAULT_MAX_DEPTH,
                        "description": "Maximum directory traversal depth (1..=16)."
                    }
                },
                "required": [],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": READ_FILE_NAME,
            "description": "Read a UTF-8 file in the workspace. Small files return inline directly; large files fold into FoldStore and return a result_ref envelope for search_result/read_result.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Relative file path inside the workspace."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": GREP_FILES_NAME,
            "description": "Search UTF-8 files under a workspace path. Small match sets return inline; large match sets fold into FoldStore for search_result/read_result.",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_QUERY_BYTES,
                        "description": "Literal substring or Rust regex expression (1..=128 UTF-8 bytes)."
                    },
                    "path": {
                        "type": "string",
                        "default": ".",
                        "description": "Relative file or directory path inside the workspace (defaults to \".\")."
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
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        }),
    ]
}

fn err_out(err: ErrorPayload) -> ToolExecutionOutput {
    ToolExecutionOutput {
        output_json: err.to_json_string(),
        delivery: None,
        result_ref: None,
    }
}

fn parse_object(
    raw_args: &str,
    allowed: &[&str],
    required: &[&str],
) -> Result<Map<String, Value>, ErrorPayload> {
    let val: Value = serde_json::from_str(raw_args)
        .map_err(|_| ErrorPayload::new(ErrorCode::InvalidArgument, None, "INVALID_JSON"))?;
    let obj = val
        .as_object()
        .ok_or_else(|| ErrorPayload::new(ErrorCode::InvalidArgument, None, "EXPECTED_JSON_OBJECT"))?;

    for key in obj.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(ErrorPayload::new(
                ErrorCode::InvalidArgument,
                Some(key.as_str()),
                "UNSUPPORTED_FIELD",
            ));
        }
    }

    for &req in required {
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

    Ok(obj.clone())
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

pub fn exec_list_files(raw_args: &str, router: &WorkspaceToolRouter) -> ToolExecutionOutput {
    let obj = match parse_object(raw_args, &["path", "max_depth"], &[]) {
        Ok(o) => o,
        Err(e) => return err_out(e),
    };

    let rel_path = match optional_str(&obj, "path") {
        Ok(Some(s)) => s,
        Ok(None) => ".",
        Err(e) => return err_out(e),
    };

    let max_depth_i64 = match optional_i64(&obj, "max_depth") {
        Ok(Some(v)) => v,
        Ok(None) => DEFAULT_MAX_DEPTH as i64,
        Err(e) => return err_out(e),
    };
    if max_depth_i64 < MIN_MAX_DEPTH as i64 || max_depth_i64 > MAX_MAX_DEPTH as i64 {
        return err_out(ErrorPayload::new(
            ErrorCode::InvalidArgument,
            Some("max_depth"),
            "OUT_OF_RANGE",
        ));
    }
    let max_depth = max_depth_i64 as usize;

    let (canon_dir, display_rel) = match router.context().resolve_existing_path(rel_path) {
        Ok(res) => res,
        Err(e) => return err_out(e),
    };

    if !canon_dir.is_dir() {
        return err_out(ErrorPayload::new(
            ErrorCode::InvalidArgument,
            Some("path"),
            "NOT_A_DIRECTORY",
        ));
    }

    let mut entries = Vec::new();
    let mut truncated = false;
    collect_dir_entries(
        router.context(),
        &canon_dir,
        0,
        max_depth,
        router.context().config().max_list_entries,
        &mut entries,
        &mut truncated,
    );

    if truncated {
        entries.push(format!(
            "[truncated at {} entries]",
            router.context().config().max_list_entries
        ));
    }

    let raw_text = if entries.is_empty() {
        "(empty directory)".to_owned()
    } else {
        entries.join("\n")
    };

    let mut meta = Map::new();
    meta.insert("path".to_owned(), Value::String(display_rel));
    meta.insert(
        "entry_count".to_owned(),
        Value::from(entries.len().saturating_sub(usize::from(truncated))),
    );
    router.deliver_tool_output(LIST_FILES_NAME, meta, raw_text)
}

fn should_skip_dir(name: &str) -> bool {
    matches!(name, ".git" | "target")
}

fn collect_dir_entries(
    ctx: &WorkspaceContext,
    dir: &Path,
    depth: usize,
    max_depth: usize,
    max_entries: usize,
    out: &mut Vec<String>,
    truncated: &mut bool,
) {
    if depth >= max_depth || *truncated {
        return;
    }

    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<PathBufLike> = rd
        .filter_map(|res| res.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).ok()?;
            if meta.file_type().is_symlink() {
                return None;
            }
            if meta.is_dir() && should_skip_dir(&name) {
                return None;
            }
            Some(PathBufLike {
                name,
                path,
                is_dir: meta.is_dir(),
                len: meta.len(),
            })
        })
        .collect();

    children.sort_by(|a, b| a.name.cmp(&b.name));

    for child in children {
        if out.len() >= max_entries {
            *truncated = true;
            return;
        }
        let rel = ctx.relative_display(&child.path);
        if child.is_dir {
            out.push(format!("{rel}/"));
            collect_dir_entries(
                ctx,
                &child.path,
                depth + 1,
                max_depth,
                max_entries,
                out,
                truncated,
            );
        } else {
            out.push(format!("{rel} ({} B)", child.len));
        }
    }
}

struct PathBufLike {
    name: String,
    path: std::path::PathBuf,
    is_dir: bool,
    len: u64,
}

pub fn exec_read_file(raw_args: &str, router: &WorkspaceToolRouter) -> ToolExecutionOutput {
    let obj = match parse_object(raw_args, &["path"], &["path"]) {
        Ok(o) => o,
        Err(e) => return err_out(e),
    };

    let rel_path = match optional_str(&obj, "path") {
        Ok(Some(s)) if !s.is_empty() => s,
        Ok(_) => {
            return err_out(ErrorPayload::new(
                ErrorCode::InvalidArgument,
                Some("path"),
                "INVALID_PATH",
            ))
        }
        Err(e) => return err_out(e),
    };

    let (canon_file, display_rel) = match router.context().resolve_existing_path(rel_path) {
        Ok(res) => res,
        Err(e) => return err_out(e),
    };

    let meta_fs = match fs::metadata(&canon_file) {
        Ok(m) => m,
        Err(_) => {
            return err_out(ErrorPayload::new(
                ErrorCode::InvalidArgument,
                Some("path"),
                "FILE_NOT_FOUND",
            ))
        }
    };

    if !meta_fs.is_file() {
        return err_out(ErrorPayload::new(
            ErrorCode::InvalidArgument,
            Some("path"),
            "NOT_A_FILE",
        ));
    }

    if meta_fs.len() > router.context().config().max_file_bytes as u64 {
        return err_out(ErrorPayload::new(
            ErrorCode::InvalidArgument,
            Some("path"),
            "FILE_TOO_LARGE",
        ));
    }

    let bytes = match fs::read(&canon_file) {
        Ok(b) => b,
        Err(_) => {
            return err_out(ErrorPayload::new(
                ErrorCode::InvalidArgument,
                Some("path"),
                "READ_ERROR",
            ))
        }
    };

    let raw_text = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => {
            return err_out(ErrorPayload::new(
                ErrorCode::InvalidArgument,
                Some("path"),
                "NON_UTF8_FILE",
            ))
        }
    };

    let mut meta = Map::new();
    meta.insert("path".to_owned(), Value::String(display_rel));
    meta.insert("bytes".to_owned(), Value::from(raw_text.len()));
    router.deliver_tool_output(READ_FILE_NAME, meta, raw_text)
}

pub fn exec_grep_files(raw_args: &str, router: &WorkspaceToolRouter) -> ToolExecutionOutput {
    let obj = match parse_object(
        raw_args,
        &["query", "path", "match_mode", "case_sensitive"],
        &["query"],
    ) {
        Ok(o) => o,
        Err(e) => return err_out(e),
    };

    let query = match optional_str(&obj, "query") {
        Ok(Some(q)) => q,
        Ok(None) => {
            return err_out(ErrorPayload::new(
                ErrorCode::InvalidArgument,
                Some("query"),
                "MISSING_REQUIRED_FIELD",
            ))
        }
        Err(e) => return err_out(e),
    };
    if query.is_empty() || query.len() > MAX_QUERY_BYTES {
        return err_out(ErrorPayload::new(
            ErrorCode::InvalidArgument,
            Some("query"),
            "INVALID_QUERY_LENGTH",
        ));
    }

    let rel_path = match optional_str(&obj, "path") {
        Ok(Some(s)) => s,
        Ok(None) => ".",
        Err(e) => return err_out(e),
    };

    let is_regex = match optional_str(&obj, "match_mode") {
        Ok(None) | Ok(Some("literal")) => false,
        Ok(Some("regex")) => true,
        Ok(Some(_)) => {
            return err_out(ErrorPayload::new(
                ErrorCode::InvalidArgument,
                Some("match_mode"),
                "INVALID_MATCH_MODE",
            ))
        }
        Err(e) => return err_out(e),
    };

    let case_sensitive = match optional_bool(&obj, "case_sensitive") {
        Ok(Some(b)) => b,
        Ok(None) => true,
        Err(e) => return err_out(e),
    };

    let (canon_target, display_rel) = match router.context().resolve_existing_path(rel_path) {
        Ok(res) => res,
        Err(e) => return err_out(e),
    };

    let matcher = match compile_grep_matcher(query, is_regex, case_sensitive) {
        Ok(m) => m,
        Err(e) => return err_out(e),
    };

    let mut files = Vec::new();
    if canon_target.is_file() {
        files.push(canon_target);
    } else if canon_target.is_dir() {
        collect_grep_files(&canon_target, &mut files);
        files.sort();
    } else {
        return err_out(ErrorPayload::new(
            ErrorCode::InvalidArgument,
            Some("path"),
            "INVALID_PATH",
        ));
    }

    let cfg = router.context().config();
    let mut matches = Vec::new();
    let mut truncated = false;

    for file_path in files {
        if truncated {
            break;
        }
        let Ok(meta) = fs::metadata(&file_path) else {
            continue;
        };
        if !meta.is_file() || meta.len() > cfg.max_grep_file_bytes as u64 {
            continue;
        }
        let Ok(bytes) = fs::read(&file_path) else {
            continue;
        };
        if bytes.contains(&0) {
            continue;
        }
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };

        let rel = router.context().relative_display(&file_path);
        for (idx, line) in text.lines().enumerate() {
            let line_clean = line.strip_suffix('\r').unwrap_or(line);
            let matched = match &matcher {
                GrepMatcher::LiteralSensitive(pat) => line_clean.contains(pat),
                GrepMatcher::Regex(re) => re.is_match(line_clean),
            };
            if matched {
                if matches.len() >= cfg.max_grep_matches {
                    truncated = true;
                    break;
                }
                matches.push(format!("{rel}:{}: {line_clean}", idx + 1));
            }
        }
    }

    let match_count = matches.len();
    if truncated {
        matches.push(format!("[truncated at {} matches]", cfg.max_grep_matches));
    }
    let raw_text = if matches.is_empty() {
        "No matches found.".to_owned()
    } else {
        matches.join("\n")
    };

    let mut meta = Map::new();
    meta.insert("path".to_owned(), Value::String(display_rel));
    meta.insert("query".to_owned(), Value::String(query.to_owned()));
    meta.insert("match_count".to_owned(), Value::from(match_count));
    router.deliver_tool_output(GREP_FILES_NAME, meta, raw_text)
}

enum GrepMatcher<'a> {
    LiteralSensitive(&'a str),
    Regex(Regex),
}

fn compile_grep_matcher(
    query: &str,
    is_regex: bool,
    case_sensitive: bool,
) -> Result<GrepMatcher<'_>, ErrorPayload> {
    if !is_regex && case_sensitive {
        return Ok(GrepMatcher::LiteralSensitive(query));
    }
    let pattern = if is_regex {
        query.to_owned()
    } else {
        regex::escape(query)
    };
    let re = RegexBuilder::new(&pattern)
        .case_insensitive(!case_sensitive)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_DFA_SIZE_LIMIT)
        .build()
        .map_err(|_| {
            ErrorPayload::new(ErrorCode::InvalidRegex, Some("query"), "REGEX_COMPILE_ERROR")
        })?;
    Ok(GrepMatcher::Regex(re))
}

fn collect_grep_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = rd.filter_map(|r| r.ok()).collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            if !should_skip_dir(&name) {
                collect_grep_files(&path, out);
            }
        } else if meta.is_file() {
            out.push(path);
        }
    }
}
