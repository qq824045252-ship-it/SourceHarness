use super::journal::Journal;
use super::tools::{self, definition, parse, rejected, EditMode, Reply, ResolvePath};
use crate::budget::{truncate_utf8_bytes, MAX_RESPONSE_BYTES};
use crate::workspace::WorkspaceContext;
use serde::Deserialize;
use serde_json::{json, Value};
use std::fmt::Write;

pub(super) const INSTRUCTIONS: &str = "Work in the workspace. Use exec for combined inspection and local tests on a fresh copy. Apply lasting changes with edit or write. Live reads and successful edits or writes still count across tasks in this session. Read existing files you have not observed, and reread on conflict. Group independent reads in one turn and substitutions for one file in one edit. Preserve unrelated text. When asked to verify, run validate on the finished files and report the result. Use rollback to recover from unsuccessful edits.";

pub(super) fn definitions() -> Vec<Value> {
    let text = json!({"type":"string"});
    let line = json!({"type":"integer","minimum":1});
    let byte = json!({"type":"integer","minimum":0});
    vec![
        definition("exec", "Run a shell command for inspection or local tests in a disposable workspace copy, without network access. Changes are discarded.",
            json!({"command":text,"timeout_seconds":{"type":"integer","minimum":1,"maximum":60}}), &["command"]),
        definition("read", "Read numbered lines from a file or an output reference. Exactly one of path or result_ref. Use inclusive line bounds or follow byte bounds in the result.",
            json!({"path":text,"result_ref":text,"start_line":line,"end_line":line,"start_byte":byte,"end_byte":byte}), &[]),
        definition("edit", "Apply exact, non-overlapping replacements to a file observed by read, edit or write in an earlier model turn of this session. Each old_text must appear once.",
            json!({"path":text,"changes":{"type":"array","items":{"type":"object","properties":{
                "old_text":text,"new_text":text},"required":["old_text","new_text"],"additionalProperties":false}}}), &["path","changes"]),
        definition("write", "Write a complete UTF-8 file. Replacing an existing file requires an earlier live read, edit or write in this session.",
            json!({"path":text,"content":text}), &["path","content"]),
        definition("validate", "Run a configured acceptance check on the current workspace. Omit check when only one is configured.",
            json!({"check":text}), &[]),
        definition("rollback", "Undo this run's edits since its latest passing validation or start. Stops if workspace files changed outside those edits.",
            json!({}), &[]),
    ]
}

pub(super) fn validate(
    ctx: &WorkspaceContext, journal: &mut Journal, run: usize, operation: &str,
    commands: &super::Commands, args: &Value,
) -> Reply {
    if commands.0.is_empty() {
        return Ok(rejected("NO_CHECKS", "No external checks were configured. Use exec for workspace-local tests."));
    }
    let check = if args.as_object().is_some_and(|args| args.is_empty()) && commands.0.len() == 1 {
        commands.0.keys().next().map(String::as_str)
    } else {
        args["check"].as_str().filter(|_| args.as_object().is_some_and(|args| args.len() == 1))
    };
    if let Some(check) = check.filter(|check| commands.0.contains_key(*check)) {
        super::validation::run(ctx, journal, run, operation, commands, &json!({"check":check}))
    } else {
        let checks = commands.0.keys().cloned().collect::<Vec<_>>().join(", ");
        Ok(rejected("UNKNOWN_CHECK", &format!("Choose a configured check from {checks}.")))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    path: String,
    changes: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
}

pub(super) fn modify(
    ctx: &WorkspaceContext,
    journal: &mut Journal,
    run: usize,
    round: usize,
    operation: &str,
    tool: &str,
    args: &Value,
) -> Reply {
    let (path, mut input) = if tool == "edit" {
        let args: EditArgs = match parse(args) {
            Ok(args) => args,
            Err(err) => return Ok(err),
        };
        (args.path, json!({"changes":args.changes}))
    } else {
        let args: WriteArgs = match parse(args) {
            Ok(args) => args,
            Err(err) => return Ok(err),
        };
        (args.path, json!({"content":args.content}))
    };
    let saved = journal.events.iter().any(|event|
        matches!(event.kind.as_str(), "edit_intent" | "edit_receipt") && event.data["operation"] == operation
    );
    if !saved {
        let (_, path) = match tools::workspace_path(ctx, &path, ResolvePath::MissingParents) {
            Ok(value) => value,
            Err(err) => return Ok(err),
        };
        let expected = observed_revision(journal, run, round, &path);
        if tool == "edit" && expected.is_none() {
            let mut error = rejected("READ_REQUIRED", "Use read with the live path before editing.");
            error["path"] = json!(path);
            return Ok(error);
        }
        input["path"] = json!(path);
        if tool == "write" {
            input["mode"] = json!(if expected.is_some() { "replace" } else { "create" });
        }
        if let Some(revision) = expected {
            input["expected_revision"] = json!(revision);
        }
    }
    let mut result = tools::modify(ctx, journal, run, operation, tool, EditMode::Text, &input)?;
    if tool == "write" && input["mode"] == "create" && result["code"] == "REVISION_CONFLICT"
        && result["current_revision"].is_string()
    {
        result["code"] = json!("READ_REQUIRED");
        result["reason"] = json!("Use read with the live path before replacing an existing file.");
    }
    if result["code"] == "REVISION_CONFLICT" && result["planned_to_current"].is_null() {
        tools::add_conflict_diff(journal, run, &mut result)?;
    }
    Ok(result)
}

fn observed_revision<'a>(
    journal: &'a Journal,
    run: usize,
    round: usize,
    path: &str,
) -> Option<&'a str> {
    for event in journal.events.iter().rev() {
        if event.kind == "rollback_receipt" && event.data["paths"].as_array()
            .is_some_and(|paths| paths.iter().any(|saved| saved == path))
        {
            break;
        }
        let earlier = event.run < run || (event.run == run
            && event.data["round"].as_u64().is_some_and(|observed| observed < round as u64));
        if event.kind != "tool_result" || !earlier {
            continue;
        }
        if event.data["observed_absent"] == path {
            break;
        }
        if event.data["live_observation"] == true && event.data["source"]["path"] == path {
            return event.data["revision"].as_str();
        }
    }
    None
}

pub(super) fn budget_hint(remaining: usize) -> Option<&'static str> {
    match remaining {
        2 => Some("[Budget for this task: 2 turns left. Validate if needed, then finish.]"),
        1 => Some("[Budget for this task: 1 turn left. Finish in the next response and report validation status.]"),
        _ => None,
    }
}

pub(super) fn present(
    tool: &str,
    result: &Value,
    warning: Option<&Value>,
    budget_hint: Option<&str>,
) -> Value {
    let mut footer = String::new();
    if let Some(warning) = warning {
        let _ = write!(footer, "\n{}: Repeated failure ({} rounds). Inspect fresh evidence or use rollback.",
            warning["kind"].as_str().unwrap_or("LOOP_WARNING"), warning["count"]);
    }
    if let Some(hint) = budget_hint {
        footer.push('\n');
        footer.push_str(hint);
    }
    let budget = MAX_RESPONSE_BYTES.saturating_sub(footer.len());
    let status = result["status"].as_str().unwrap_or("error");
    let path = result["source"]["path"].as_str().or_else(|| result["path"].as_str());
    let preview = result["preview"].as_str().unwrap_or("");
    let text = match (tool, status) {
        ("read", "ok") if result["content"].is_string() => numbered_text(result, budget),
        ("edit", "ok") => format!("Edited {}.", path.unwrap_or("file")),
        ("write", "ok") => format!("Wrote {}.", path.unwrap_or("file")),
        ("rollback", "ok") => format!(
            "Restored {} files to the checkpoint. Read live files before editing again.",
            result["paths"].as_array().map_or(0, Vec::len)
        ),
        ("validate", "ok") => {
            let check = result["check"].as_str().unwrap_or("check");
            if result["passed"] == true && result["workspace_changed_during_check"] == false {
                format!("PASS {check}. This revision is verified. Finish if done. Re-run validate after any edit.")
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
            text
        }
        _ => {
            let code = result["code"].as_str().unwrap_or("TOOL_ERROR");
            let mut reason = result["reason"].as_str().unwrap_or("Inspect the workspace.");
            if code == "REVISION_CONFLICT" {
                reason = if result["current_revision"].is_null() {
                    "The live file is absent."
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
    let (text, _) = truncate_utf8_bytes(&text, budget);
    json!(format!("{text}{footer}"))
}

fn numbered_text(result: &Value, budget: usize) -> String {
    let label = result["source"]["path"].as_str().or_else(|| result["result_ref"].as_str()).unwrap_or("Output");
    let start = result["range"]["start_line"].as_u64().unwrap_or(1);
    let bytes = result["range"]["byte_start"].as_u64().unwrap_or(0);
    let content = result["content"].as_str().unwrap_or("");
    let mut text = format!("{label:?}\n");
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
