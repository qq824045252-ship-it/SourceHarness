use super::exec;
use super::journal::Journal;
use super::tools::{self, definition, parse, rejected, Edit, Reply, ResolvePath, TextChange};
use crate::budget::{truncate_utf8_bytes, MAX_RESPONSE_BYTES};
use crate::workspace::WorkspaceContext;
use serde::Deserialize;
use serde_json::{json, Value};
use std::fmt::Write;

pub(super) const INSTRUCTIONS: &str = "Work on the user's task in the workspace and report the result. Exec runs in a disposable sandbox, edit and write change the live workspace, and validate runs the configured checker.";

const MAX_READ_PATHS: usize = 8;

pub(super) fn definitions() -> Vec<Value> {
    let text = json!({"type":"string"});
    let line = json!({"type":"integer","minimum":1});
    let byte = json!({"type":"integer","minimum":0});
    let mut definitions = vec![
        definition("exec", "Run a shell command for inspection or local tests in a disposable workspace copy, without network access. Changes are discarded.",
            json!({"command":{"type":"string","maxLength":exec::MAX_COMMAND_BYTES},
                "timeout_seconds":{"type":"integer","minimum":1,"maximum":exec::MAX_TIMEOUT_SECONDS,
                    "default":exec::DEFAULT_TIMEOUT_SECONDS}}), &["command"]),
        definition("read", "Read numbered text from a live file, multiple files, or saved output. Use path, paths, or result_ref. Line and byte bounds apply to a single source.",
            json!({"path":text,"paths":{"type":"array","items":text,"minItems":1,"maxItems":MAX_READ_PATHS},
                "result_ref":text,"start_line":line,"end_line":line,"start_byte":byte,"end_byte":byte}), &[]),
        definition("edit", "Apply exact, non-overlapping replacements to a workspace-relative file. Each old_text must appear once.",
            json!({"path":text,"changes":{"type":"array","items":{"type":"object","properties":{
                "old_text":text,"new_text":text},"required":["old_text","new_text"]}}}), &["path","changes"]),
        definition("write", "Create or replace a workspace-relative UTF-8 file with complete content.",
            json!({"path":text,"content":text}), &["path","content"]),
        definition("validate", "Run a configured check against the current workspace snapshot. Omit check when only one is configured.",
            json!({"check":text}), &[]),
        definition("rollback", "Undo this run's edits since its latest passing validation or start. Stops if workspace files changed outside those edits.",
            json!({}), &[]),
    ];
    for tool in &mut definitions {
        if tool["name"] != "rollback" {
            tool["parameters"]["additionalProperties"] = json!(true);
        }
    }
    definitions
}

#[derive(Deserialize)]
struct ReadPaths {
    paths: Vec<String>,
}

pub(super) fn read(ctx: &WorkspaceContext, journal: &mut Journal, run: usize, args: &Value) -> Reply {
    if args["paths"].is_null() {
        return read_one(ctx, journal, run, args);
    }
    if ["path", "result_ref", "start_line", "end_line", "line_count", "start_byte", "end_byte"]
        .iter().any(|key| !args[*key].is_null())
    {
        return Ok(rejected("INVALID_ARGUMENT", "Use paths alone. Range bounds apply to a single path or result_ref."));
    }
    let batch: ReadPaths = match parse(args) {
        Ok(batch) => batch,
        Err(error) => return Ok(error),
    };
    if !(1..=MAX_READ_PATHS).contains(&batch.paths.len()) {
        return Ok(rejected("INVALID_ARGUMENT", "Use paths with 1 to 8 workspace-relative paths."));
    }
    let mut files = Vec::with_capacity(batch.paths.len());
    for path in batch.paths {
        files.push(read_one(ctx, journal, run, &json!({"path":path}))?);
    }
    let mut result = if files.iter().any(|file| file["status"] == "ok") {
        json!({"status":"ok"})
    } else {
        rejected("READ_BATCH_FAILED", "Could not read any of the requested files.")
    };
    result["files"] = json!(files);
    Ok(result)
}

fn read_one(ctx: &WorkspaceContext, journal: &mut Journal, run: usize, args: &Value) -> Reply {
    let mut result = tools::read(ctx, journal, run, args)?;
    if result["status"] == "error" && args["result_ref"].is_null() {
        if let Some(path) = args["path"].as_str() {
            result["path"] = json!(path);
            if result["code"] == "FILE_NOT_FOUND" {
                if let Ok((_, path)) = tools::workspace_path(ctx, path, ResolvePath::MissingParents) {
                    result["observed_absent"] = json!(path);
                }
            }
        }
    }
    Ok(result)
}

pub(super) fn batch_observations(files: &[Value]) -> Vec<Value> {
    files.iter().filter_map(|file| {
        if file["status"] == "ok" && file["source"]["kind"] == "workspace" {
            Some(json!({"path":file["source"]["path"],"revision":file["revision"]}))
        } else {
            file["observed_absent"].as_str().map(|path| json!({"path":path,"revision":null}))
        }
    }).collect()
}

pub(super) fn validate(
    ctx: &WorkspaceContext, journal: &mut Journal, run: usize, operation: &str,
    commands: &super::Commands, args: &Value,
) -> Reply {
    if commands.0.is_empty() {
        return Ok(rejected("NO_CHECKS", "No external checks were configured. Use exec for workspace-local tests."));
    }
    #[derive(Deserialize)]
    struct Check {
        check: Option<String>,
    }
    let args: Check = match parse(args) {
        Ok(args) => args,
        Err(error) => return Ok(error),
    };
    let check = match args.check.as_deref() {
        None if commands.0.len() == 1 => commands.0.keys().next().map(String::as_str),
        check => check,
    };
    if let Some(check) = check.filter(|check| commands.0.contains_key(*check)) {
        super::validation::run(ctx, journal, run, operation, commands, &json!({"check":check}))
    } else {
        let checks = commands.0.keys().cloned().collect::<Vec<_>>().join(", ");
        Ok(rejected("UNKNOWN_CHECK", &format!("Choose a configured check from {checks}.")))
    }
}

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    changes: Vec<TextChange>,
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

pub(super) fn modify(
    ctx: &WorkspaceContext,
    journal: &mut Journal,
    run: usize,
    operation: &str,
    tool: &str,
    args: &Value,
) -> Reply {
    if let Some(result) = tools::resume_edit(ctx, journal, run, operation) {
        return result;
    }
    let (path, edit) = if tool == "edit" {
        let args: EditArgs = match parse(args) {
            Ok(args) => args,
            Err(err) => return Ok(err),
        };
        (args.path, Edit::Text(args.changes))
    } else {
        let args: WriteArgs = match parse(args) {
            Ok(args) => args,
            Err(err) => return Ok(err),
        };
        (args.path, Edit::Write(args.content))
    };
    let target = match tools::workspace_path(ctx, &path, ResolvePath::MissingParents) {
        Ok(value) => value,
        Err(err) => return Ok(err),
    };
    let expected = observed_revision(journal, &target.1).map(|revision| revision.map(str::to_owned));
    let mut result = tools::apply_edit(ctx, journal, run, operation, target, expected, edit)?;
    if result["code"] == "REVISION_CONFLICT" && result["planned_to_current"].is_null() {
        tools::add_conflict_diff(journal, run, &mut result)?;
    }
    Ok(result)
}

fn observed_revision<'a>(journal: &'a Journal, path: &str) -> Option<Option<&'a str>> {
    for event in journal.events.iter().rev() {
        if event.kind == "rollback_receipt" && event.data["paths"].as_array()
            .is_some_and(|paths| paths.iter().any(|saved| saved == path))
        {
            break;
        }
        if event.kind != "tool_result" {
            continue;
        }
        if let Some(observation) = event.data["live_observations"].as_array()
            .and_then(|files| files.iter().rev().find(|file| file["path"] == path))
        {
            return Some(observation["revision"].as_str());
        }
        if event.data["observed_absent"] == path {
            return Some(None);
        }
        if event.data["live_observation"] == true && event.data["source"]["path"] == path {
            return event.data["revision"].as_str().map(Some);
        }
    }
    None
}

pub(super) fn present(tool: &str, result: &Value, warning: Option<&Value>) -> Value {
    let mut footer = String::new();
    if let Some(warning) = warning {
        let kind = warning["kind"].as_str().unwrap_or("LOOP_WARNING");
        let detail = if kind == "EXEC_WITHOUT_PROGRESS" { "Shell probes made no progress" } else { "Repeated failure" };
        let _ = write!(footer, "\n{kind}: {detail} ({} rounds). Inspect fresh evidence or use rollback.", warning["count"]);
    }
    let budget = MAX_RESPONSE_BYTES.saturating_sub(footer.len());
    let text = if let Some(files) = result["files"].as_array().filter(|_| tool == "read") {
        let mut text = String::new();
        for (index, file) in files.iter().enumerate() {
            if index > 0 {
                text.push_str("\n\n");
            }
            let bytes = budget.saturating_sub(text.len() + 2 * (files.len() - 1 - index))
                / (files.len() - index);
            text.push_str(&result_text(tool, file, bytes));
        }
        text
    } else {
        result_text(tool, result, budget)
    };
    json!(format!("{text}{footer}"))
}

fn result_text(tool: &str, result: &Value, budget: usize) -> String {
    let status = result["status"].as_str().unwrap_or("error");
    let path = result["source"]["path"].as_str().or_else(|| result["path"].as_str());
    let preview = result["preview"].as_str().unwrap_or("");
    let text = match (tool, status) {
        ("read", "ok") if result["content"].is_string() => numbered_text(result, budget),
        ("edit", "ok") => format!("Edited {}.", path.unwrap_or("file")),
        ("write", "ok") => format!("Wrote {}.", path.unwrap_or("file")),
        ("rollback", "ok") => format!(
            "Restored {} files to the checkpoint.",
            result["paths"].as_array().map_or(0, Vec::len)
        ),
        ("validate", "ok") => {
            let check = result["check"].as_str().unwrap_or("check");
            if result["passed"] == true && result["workspace_changed_during_check"] == false {
                format!("PASS {check}. This revision is verified. Re-run validate after any edit.")
            } else if result["passed"] == true {
                format!("PASS {check} on the tested snapshot. Workspace changed during validation. Run the check again.")
            } else {
                let reason = result["reason"].as_str().unwrap_or(preview);
                let mut text = format!("FAIL {check} (exit {})\n{reason}", result["exit_code"]);
                if let Some(reference) = result["result_ref"].as_str() {
                    let _ = write!(text, "\nFull output: read(result_ref={reference:?}).");
                }
                text
            }
        }
        ("exec", _) => {
            let mut text = if status == "ok" {
                if preview.is_empty() { "Exit 0.".to_owned() } else { preview.to_owned() }
            } else {
                format!("{}: {} (exit {})\n{preview}",
                    result["code"].as_str().unwrap_or("EXEC_FAILED"),
                    result["reason"].as_str().unwrap_or("Command did not complete."),
                    result["exit_code"])
            };
            if result["delivery"] == "folded" {
                if let Some(reference) = result["result_ref"].as_str() {
                    let _ = write!(text, "\nMore output: read(result_ref={reference:?}).");
                }
                if result["output_complete"] == false {
                    text.push_str("\nCapture limit reached.");
                }
            }
            if result["status"] == "ok" || result["code"] == "EXEC_FAILED" || result["code"] == "EXEC_TIMEOUT" {
                text.push_str("\nSandbox changes discarded.");
            }
            text
        }
        _ => {
            let code = result["code"].as_str().unwrap_or("TOOL_ERROR");
            let mut reason = result["reason"].as_str().unwrap_or("Inspect the workspace.");
            if code == "REVISION_CONFLICT" {
                reason = if result["current_revision"].is_null() {
                    "The live file is absent. Read the path to confirm before recreating it."
                } else {
                    "The live file differs from the observed version. Read it before writing."
                };
            } else if code == "FILE_NOT_FOUND" && tool == "read" {
                reason = "The live file is absent. Use write if the task calls for creating it.";
            }
            let mut text = format!("{code}: {reason}");
            if let Some(path) = path {
                let _ = write!(text, "\nFile: {path:?}");
            }
            if let Some(start) = result["candidate_range"]["start_line"].as_u64() {
                let end = result["candidate_range"]["end_line"].as_u64().unwrap_or(start);
                let _ = write!(text, "\nRead lines {start}–{end}.");
            }
            let diff = result["diff"].as_str().or_else(|| result["planned_to_current"]["diff"].as_str());
            if let Some(diff) = diff {
                let _ = write!(text, "\n{diff}");
            }
            if let Some(reference) = result["diff_ref"].as_str().or_else(|| result["planned_to_current"]["result_ref"].as_str()) {
                let _ = write!(text, "\nFull diff: read(result_ref={reference:?}).");
            }
            text
        }
    };
    truncate_utf8_bytes(&text, budget).0.to_owned()
}

fn numbered_text(result: &Value, budget: usize) -> String {
    let label = result["source"]["path"].as_str().or_else(|| result["result_ref"].as_str()).unwrap_or("Output");
    let start = result["range"]["start_line"].as_u64().unwrap_or(1);
    let bytes = result["range"]["byte_start"].as_u64().unwrap_or(0);
    let content = result["content"].as_str().unwrap_or("");
    let mut text = format!("{label:?}\n");
    if text.len() + 256 >= budget {
        text = format!("{}\n", result["result_ref"].as_str().unwrap_or("Output"));
    }
    let mut consumed = 0;
    for (offset, line) in content.split_inclusive('\n').enumerate() {
        let prefix = format!("{}\t", start + offset as u64);
        let available = budget.saturating_sub(256 + text.len() + prefix.len());
        if available == 0 {
            break;
        }
        let (part, clipped) = truncate_utf8_bytes(line, available);
        text.push_str(&prefix);
        text.push_str(part);
        consumed += part.len();
        if clipped {
            break;
        }
    }
    let end = result["next"]["end_byte"].as_u64()
        .unwrap_or(bytes + content.len() as u64);
    let next = bytes + consumed as u64;
    if next < end {
        if let Some(reference) = result["result_ref"].as_str() {
            let _ = write!(text, "\nMore: read(result_ref={reference:?}, start_byte={next}, end_byte={end}).");
        }
    } else if content.is_empty() {
        text.push_str("(empty)");
    }
    text
}
