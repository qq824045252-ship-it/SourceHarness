use super::exec;
use super::journal::Journal;
use super::tools::{self, definition, parse, rejected, ChangesetItem, Edit, Reply, ResolvePath, TextChange};
use crate::budget::{truncate_utf8_bytes, MAX_RESPONSE_BYTES};
use crate::workspace::WorkspaceContext;
use serde::Deserialize;
use serde_json::{json, Value};
use std::fmt::Write;

#[cfg(windows)]
pub(super) const INSTRUCTIONS: &str = "Work on the user's task in the workspace and report the result. read(path=\".\") recursively lists all workspace files; read(paths=[...]) batches up to 8 files; read(path=\"...\", pattern=\"...\") searches text; read headers show line count, bytes, and line endings (lf/crlf). Prefer edit for existing files, write only for task deliverables (never leave temporary helper scripts or scratch files in the workspace), and call validate only when external checks are configured. exec runs Windows cmd.exe (/D /S /C)—do not use POSIX syntax (ls, find, grep, heredocs); pass stdin to commands like python - instead of creating helper scripts, and do not inspect .sourceharness or external checker files. Stop as soon as validate passes or a workspace blocker is confirmed. Final response: when a JSON object, array, or exact format is requested, output ONLY that exact value using the exact requested key names—no explanation, markdown fences, or extra prose, even when reporting a blocker.";
#[cfg(not(windows))]
pub(super) const INSTRUCTIONS: &str = "Work on the user's task in the workspace and report the result. read(path=\".\") recursively lists all workspace files; read(paths=[...]) batches up to 8 files; read(path=\"...\", pattern=\"...\") searches text; read headers show line count, bytes, and line endings (lf/crlf). Prefer edit for existing files, write only for task deliverables (never leave temporary helper scripts or scratch files in the workspace), and call validate only when external checks are configured. exec runs POSIX /bin/sh -c—avoid bash-only syntax; pass stdin to commands like python - instead of creating helper scripts, and do not inspect .sourceharness or external checker files. Stop as soon as validate passes or a workspace blocker is confirmed. Final response: when a JSON object, array, or exact format is requested, output ONLY that exact value using the exact requested key names—no explanation, markdown fences, or extra prose, even when reporting a blocker.";
const MAX_READ_PATHS: usize = 8;

pub(super) fn definitions() -> Vec<Value> {
    let text = json!({"type":"string"});
    let line = json!({"type":"integer","minimum":1});
    let byte = json!({"type":"integer","minimum":0});
    let exec_desc = if cfg!(windows) {
        "Run a Windows cmd.exe command in the workspace (not POSIX sh/bash). Pass optional stdin (e.g. command=\"python -\", stdin=\"...\") instead of writing temporary helper scripts or complex shell quoting."
    } else {
        "Run a POSIX /bin/sh command in the workspace. Pass optional stdin (e.g. command=\"python -\", stdin=\"...\") instead of writing temporary helper scripts."
    };
    let mut definitions = vec![
        definition("exec", exec_desc,
            json!({"command":{"type":"string","maxLength":exec::MAX_COMMAND_BYTES},
                "stdin":{"type":"string","maxLength":exec::MAX_STDIN_BYTES},
                "timeout_seconds":{"type":"integer","minimum":1,"maximum":exec::MAX_TIMEOUT_SECONDS,"default":exec::DEFAULT_TIMEOUT_SECONDS}}), &["command"]),
        definition("read", "Read numbered text from a live file, list a directory (e.g. path=\".\"), search text with pattern (optional regex=true) in a file, directory, or result_ref, batch up to 8 files via paths, read a saved result_ref, or pass history=true to view the session causal timeline (or a file's historical snapshots incl. run-start).",
            json!({"path":text,"paths":{"type":"array","items":text,"minItems":1,"maxItems":MAX_READ_PATHS},
                "result_ref":text,"pattern":text,"regex":{"type":"boolean"},"history":{"type":"boolean"},"start_line":line,"end_line":line,"start_byte":byte,"end_byte":byte}), &[]),
        definition("edit", "Apply exact, non-overlapping replacements to an existing workspace file (path + changes), or atomically update multiple files together via files=[{path, changes|content}] (all-or-nothing preflight). Prefer edit over write for modifying existing files. Each old_text must appear once.",
            json!({"path":text,
                "changes":{"type":"array","items":{"type":"object","properties":{"old_text":text,"new_text":text},"required":["old_text","new_text"]}},
                "files":{"type":"array","minItems":1,"items":{"type":"object","properties":{"path":text,
                    "changes":{"type":"array","items":{"type":"object","properties":{"old_text":text,"new_text":text},"required":["old_text","new_text"]}},
                    "content":text},"required":["path"]}}}), &[]),
        definition("write", "Create a new workspace UTF-8 file or replace a file completely. Prefer edit for targeted updates to existing files.",
            json!({"path":text,"content":text}), &["path","content"]),
        definition("validate", "Run a configured external check against the current workspace snapshot (only when checks are configured). Omit check when only one is configured.",
            json!({"check":text}), &[]),
        definition("rollback", "Undo this run's edits/exec changes back to the latest passing validate or run start (or optional to_event). Optional paths restricts rollback to specific files; dry_run=true previews diffs without modifying files.",
            json!({"paths":{"type":"array","items":text,"minItems":1},"to_event":line,"dry_run":{"type":"boolean"}}), &[]),
    ];
    for tool in &mut definitions {
        tool["parameters"]["additionalProperties"] = json!(true);
    }
    definitions
}

#[derive(Deserialize)]
struct ReadPaths { paths: Vec<String> }

pub(super) fn read(ctx: &WorkspaceContext, journal: &mut Journal, run: usize, args: &Value) -> Reply {
    if args["paths"].is_null() {
        if !args["history"].is_null() && args["history"].as_bool().is_none() {
            return Ok(rejected("INVALID_ARGUMENT", "history must be a boolean."));
        }
        if args["history"] == true {
            return tools::read_history(ctx, journal, run, args);
        }
        return read_one(ctx, journal, run, args);
    }
    if ["path", "result_ref", "pattern", "regex", "history", "start_line", "end_line", "line_count", "start_byte", "end_byte"].iter().any(|k| !args[*k].is_null()) {
        return Ok(rejected("INVALID_ARGUMENT", "Use paths alone. Range bounds apply to a single path or result_ref."));
    }
    let batch: ReadPaths = match parse(args) { Ok(b) => b, Err(e) => return Ok(e) };
    if !(1..=MAX_READ_PATHS).contains(&batch.paths.len()) {
        return Ok(rejected("INVALID_ARGUMENT", "Use paths with 1 to 8 workspace-relative paths."));
    }
    let mut files = Vec::with_capacity(batch.paths.len());
    for path in batch.paths { files.push(read_one(ctx, journal, run, &json!({"path":path}))?); }
    let mut result = if files.iter().any(|f| f["status"] == "ok") { json!({"status":"ok"}) }
        else { rejected("READ_BATCH_FAILED", "Could not read any of the requested files.") };
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

pub(super) fn resolve_check_name<'a>(commands: &'a super::Commands, args: &'a Value) -> Option<&'a str> {
    if commands.0.is_empty() { return None; }
    match args.get("check") {
        None | Some(Value::Null) if commands.0.len() == 1 => commands.0.keys().next().map(String::as_str),
        Some(Value::String(s)) if commands.0.contains_key(s) => Some(s.as_str()),
        _ => None,
    }
}

pub(super) fn validate(ctx: &WorkspaceContext, journal: &mut Journal, run: usize, operation: &str, commands: &super::Commands, args: &Value) -> Reply {
    if commands.0.is_empty() {
        return Ok(rejected("NO_CHECKS", "No external checks were configured. Use exec for workspace-local tests."));
    }
    #[derive(Deserialize)]
    struct Check { check: Option<String> }
    let args: Check = match parse(args) { Ok(a) => a, Err(e) => return Ok(e) };
    let check = match args.check.as_deref() {
        None if commands.0.len() == 1 => commands.0.keys().next().map(String::as_str),
        c => c,
    };
    if let Some(check) = check.filter(|c| commands.0.contains_key(*c)) {
        super::validation::run(ctx, journal, run, operation, commands, &json!({"check":check}))
    } else {
        let checks = commands.0.iter()
            .map(|(name, cmd)| if cmd.description.is_empty() { name.clone() } else { format!("{name} ({})", cmd.description) })
            .collect::<Vec<_>>()
            .join(", ");
        Ok(rejected("UNKNOWN_CHECK", &format!("Choose a configured check from {checks}.")))
    }
}

#[derive(Deserialize)]
struct EditArgs { path: String, changes: Vec<TextChange> }
#[derive(Deserialize)]
struct FileEditArg { path: String, changes: Option<Vec<TextChange>>, content: Option<String> }
#[derive(Deserialize)]
struct MultiEditArgs { files: Vec<FileEditArg> }
#[derive(Deserialize)]
struct WriteArgs { path: String, content: String }

pub(super) fn modify(ctx: &WorkspaceContext, journal: &mut Journal, run: usize, operation: &str, tool: &str, args: &Value) -> Reply {
    if let Some(result) = tools::resume_edit(ctx, journal, run, operation) { return result; }
    if tool == "edit" && !args["files"].is_null() {
        if !args["path"].is_null() || !args["changes"].is_null() {
            return Ok(rejected("INVALID_ARGUMENT", "Use either files=[...] or path+changes, not both."));
        }
        let multi: MultiEditArgs = match parse(args) { Ok(a) => a, Err(e) => return Ok(e) };
        let mut items = Vec::with_capacity(multi.files.len());
        for entry in multi.files {
            let edit = match (entry.changes, entry.content) {
                (Some(changes), None) => Edit::Text(changes),
                (None, Some(content)) => Edit::Write(content),
                _ => return Ok(rejected("INVALID_ARGUMENT", "Each file entry in files must specify exactly one of changes or content.")),
            };
            let (target, rel) = match tools::workspace_path(ctx, &entry.path, ResolvePath::MissingParents) { Ok(v) => v, Err(e) => return Ok(e) };
            let expected = observed_revision(journal, &rel).map(|rev| rev.map(str::to_owned));
            items.push(ChangesetItem { target, path: rel, expected, edit });
        }
        let mut result = tools::apply_changeset(ctx, journal, run, operation, items)?;
        if result["code"] == "REVISION_CONFLICT" && result["planned_to_current"].is_null() {
            tools::add_conflict_diff(journal, run, &mut result)?;
        }
        return Ok(result);
    }
    let (path, edit) = if tool == "edit" {
        let a: EditArgs = match parse(args) { Ok(a) => a, Err(e) => return Ok(e) };
        (a.path, Edit::Text(a.changes))
    } else {
        let a: WriteArgs = match parse(args) { Ok(a) => a, Err(e) => return Ok(e) };
        (a.path, Edit::Write(a.content))
    };
    let target = match tools::workspace_path(ctx, &path, ResolvePath::MissingParents) { Ok(v) => v, Err(e) => return Ok(e) };
    let expected = observed_revision(journal, &target.1).map(|rev| rev.map(str::to_owned));
    let mut result = tools::apply_edit(ctx, journal, run, operation, target, expected, edit)?;
    if result["code"] == "REVISION_CONFLICT" && result["planned_to_current"].is_null() {
        tools::add_conflict_diff(journal, run, &mut result)?;
    }
    Ok(result)
}

fn observed_revision<'a>(journal: &'a Journal, path: &str) -> Option<Option<&'a str>> {
    for event in journal.events.iter().rev() {
        if event.kind == "rollback_receipt" && event.data["paths"].as_array().is_some_and(|p| p.iter().any(|s| s == path)) {
            break;
        }
        if event.kind != "tool_result" { continue; }
        if let Some(obs) = event.data["live_observations"].as_array().and_then(|f| f.iter().rev().find(|x| x["path"] == path)) {
            return Some(obs["revision"].as_str());
        }
        if event.data["observed_absent"] == path { return Some(None); }
        if event.data["live_observation"] == true && event.data["source"]["path"] == path {
            return event.data["revision"].as_str().map(Some);
        }
    }
    None
}

pub(super) fn present(tool: &str, result: &Value, warning: Option<&Value>) -> Value {
    let mut footer = String::new();
    if let Some(w) = warning {
        let kind = w["kind"].as_str().unwrap_or("LOOP_WARNING");
        let rounds = &w["count"];
        match kind {
            "VALIDATION_OSCILLATION" => {
                let _ = write!(footer, "\nVALIDATION_OSCILLATION: Repeated check failure ({rounds} rounds; one more repeated failure will pause the run). Stop retrying now—if blocked or checks conflict, call rollback to restore starting files before replying.");
            }
            "EXEC_WITHOUT_PROGRESS" => {
                let _ = write!(footer, "\nEXEC_WITHOUT_PROGRESS: Shell probes made no progress ({rounds} rounds). Stop probing and edit, validate, or conclude.");
            }
            _ => {
                let _ = write!(footer, "\n{kind}: Repeated failure ({rounds} rounds). Inspect fresh evidence or use rollback.");
            }
        }
    }
    let budget = MAX_RESPONSE_BYTES.saturating_sub(footer.len());
    let text = if let Some(files) = result["files"].as_array().filter(|_| tool == "read") {
        let mut text = String::new();
        for (i, file) in files.iter().enumerate() {
            if i > 0 { text.push_str("\n\n"); }
            let bytes = budget.saturating_sub(text.len() + 2 * (files.len() - 1 - i)) / (files.len() - i);
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
        ("edit", "ok") if result["files"].is_array() => {
            let files = result["files"].as_array().unwrap();
            let tx = result["tx_id"].as_str().unwrap_or("");
            let mut parts = Vec::with_capacity(files.len());
            let mut dropped_any = false;
            for f in files {
                let p = f["source"]["path"].as_str().unwrap_or("file");
                let lines = f["lines"].as_u64().unwrap_or(0);
                let bytes = f["bytes"].as_u64().unwrap_or(0);
                let eol = f["eol"].as_str().unwrap_or("lf");
                let no_nl = if bytes > 0 && f["trailing_newline"] == false { ", no-trailing-newline" } else { "" };
                if f["dropped_trailing_newline"] == true { dropped_any = true; }
                parts.push(format!("{p} ({lines} lines, {bytes}B, {eol}{no_nl})"));
            }
            let warn = if dropped_any { " WARNING: dropped existing trailing newline." } else { "" };
            format!("Edited {} files (tx {tx}): {}.{warn}", files.len(), parts.join(", "))
        }
        ("edit" | "write", "ok") => {
            let verb = if tool == "edit" { "Edited" } else { "Wrote" };
            let target = path.unwrap_or("file");
            if let (Some(bytes), Some(eol)) = (result["bytes"].as_u64(), result["eol"].as_str()) {
                let lines = result["lines"].as_u64().unwrap_or(0);
                let no_nl = if bytes > 0 && result["trailing_newline"] == false { ", no-trailing-newline" } else { "" };
                let warn = if result["dropped_trailing_newline"] == true { " WARNING: dropped existing trailing newline." } else { "" };
                format!("{verb} {target}. ({lines} lines, {bytes}B, {eol}{no_nl}){warn}")
            } else {
                format!("{verb} {target}.")
            }
        }
        ("rollback", "ok") if result["dry_run"] == true => format!(
            "Dry-run rollback to event {} ({} files):\n{}",
            result["checkpoint_event"], result["paths"].as_array().map_or(0, Vec::len), preview
        ),
        ("rollback", "ok") => format!("Restored {} files to the checkpoint.", result["paths"].as_array().map_or(0, Vec::len)),
        ("validate", "ok") => {
            let check = result["check"].as_str().unwrap_or("check");
            if result["passed"] == true && result["workspace_changed_during_check"] == false {
                format!("PASS {check}. This revision is verified. Re-run validate after any edit.")
            } else if result["passed"] == true {
                format!("PASS {check} on the tested snapshot. Workspace changed during validation. Run the check again.")
            } else {
                let reason = result["reason"].as_str().unwrap_or(preview);
                let mut text = format!("FAIL {check} (exit {})\n{reason}", result["exit_code"]);
                if let Some(changed) = result["changed_since_checkpoint"].as_array().filter(|a| !a.is_empty()) {
                    let list: Vec<&str> = changed.iter().filter_map(Value::as_str).collect();
                    let _ = write!(text, "\nChanged since checkpoint: {}", list.join(", "));
                }
                let truncated = result["output_truncated"].as_bool().unwrap_or(result["output_complete"] == false);
                if truncated {
                    if let Some(r) = result["result_ref"].as_str() { let _ = write!(text, "\nFull output: read(result_ref={r:?})."); }
                }
                text
            }
        }
        ("exec", _) => {
            let mut text = if status == "ok" {
                if preview.is_empty() { "Exit 0.".to_owned() } else { preview.to_owned() }
            } else {
                format!("{}: {} (exit {})\n{preview}", result["code"].as_str().unwrap_or("EXEC_FAILED"),
                    result["reason"].as_str().unwrap_or("Command did not complete."), result["exit_code"])
            };
            if result["delivery"] == "folded" {
                if let Some(r) = result["result_ref"].as_str() {
                    if let (Some(b), Some(l)) = (result["total_bytes"].as_u64(), result["total_lines"].as_u64()) {
                        let _ = write!(text, "\nMore output: read(result_ref={r:?}) ({b}B, {l} lines).");
                    } else {
                        let _ = write!(text, "\nMore output: read(result_ref={r:?}).");
                    }
                }
                if result["output_complete"] == false { text.push_str("\nCapture limit reached."); }
            }
            text
        }
        _ => {
            let code = result["code"].as_str().unwrap_or("TOOL_ERROR");
            let mut reason = result["reason"].as_str().unwrap_or("Inspect the workspace.");
            if code == "REVISION_CONFLICT" {
                reason = if result["current_revision"].is_null() {
                    "The live file is absent. Read the path to confirm before recreating it."
                } else { "The live file differs from the observed version. Read it before writing." };
            } else if code == "FILE_NOT_FOUND" && tool == "read" {
                reason = "The live file is absent. Use write if the task calls for creating it.";
            }
            let mut text = format!("{code}: {reason}");
            if let Some(path) = path { let _ = write!(text, "\nFile: {path:?}"); }
            if let Some(start) = result["candidate_range"]["start_line"].as_u64() {
                let end = result["candidate_range"]["end_line"].as_u64().unwrap_or(start);
                let _ = write!(text, "\nRead lines {start}–{end}.");
            }
            if let Some(diff) = result["diff"].as_str().or_else(|| result["planned_to_current"]["diff"].as_str()) {
                let _ = write!(text, "\n{diff}");
            }
            if let Some(r) = result["diff_ref"].as_str().or_else(|| result["planned_to_current"]["result_ref"].as_str()) {
                let _ = write!(text, "\nFull diff: read(result_ref={r:?}).");
            }
            text
        }
    };
    truncate_utf8_bytes(&text, budget).0.to_owned()
}

fn numbered_text(result: &Value, budget: usize) -> String {
    let label = result["source"]["path"]
        .as_str()
        .or_else(|| (result["source"]["kind"] == "history_index").then_some("history"))
        .or_else(|| result["result_ref"].as_str())
        .unwrap_or("Output");
    let start = result["range"]["start_line"].as_u64().unwrap_or(1);
    let bytes = result["range"]["byte_start"].as_u64().unwrap_or(0);
    let content = result["content"].as_str().unwrap_or("");
    let mut text = match result["source"]["kind"].as_str() {
        Some("directory") if result["directory_entries"].is_u64() => format!(
            "{label:?} (directory, {} files, {})\n",
            result["directory_entries"], if result["directory_complete"] == false { "truncated" } else { "complete" }
        ),
        Some("workspace") => {
            let end_l = result["range"]["end_line"].as_u64().unwrap_or(start);
            let tot_l = result["total_lines"].as_u64().unwrap_or(end_l);
            let tot_b = result["total_bytes"].as_u64().unwrap_or(0);
            let eol = result["eol"].as_str().unwrap_or("lf");
            let no_nl = if tot_b > 0 && result["trailing_newline"] == false { ", no-trailing-newline" } else { "" };
            let ref_tag = result["result_ref"].as_str().map(|r| format!(", {r}")).unwrap_or_default();
            format!("{label:?} (lines {start}-{end_l}/{tot_l}, {tot_b}B, {eol}{no_nl}{ref_tag})\n")
        }
        _ => format!("{label:?}\n"),
    };
    if text.len() + 256 >= budget { text = format!("{}\n", result["result_ref"].as_str().unwrap_or("Output")); }
    let mut consumed = 0;
    for (offset, line) in content.split_inclusive('\n').enumerate() {
        let prefix = format!("{}\t", start + offset as u64);
        let available = budget.saturating_sub(256 + text.len() + prefix.len());
        if available == 0 { break; }
        let (part, clipped) = truncate_utf8_bytes(line, available);
        text.push_str(&prefix);
        text.push_str(part);
        consumed += part.len();
        if clipped { break; }
    }
    let remaining = result["coverage"]["requested_bytes"].as_u64().unwrap_or(0) > consumed as u64;
    if remaining || result["coverage"]["complete"] == false {
        let next = bytes + consumed as u64;
        let total = result["total_bytes"].as_u64().unwrap_or(next);
        if next < total {
            let reference = result["result_ref"].as_str().unwrap_or("");
            let _ = write!(text, "\nMore: read(result_ref={reference:?}, start_byte={next}).");
        }
    }
    text
}
