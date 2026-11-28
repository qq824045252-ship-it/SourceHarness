use super::journal::{revision, Artifact, Journal};
use crate::budget::{json_byte_len, truncate_utf8_bytes, MAX_RESPONSE_BYTES};
use crate::workspace::WorkspaceContext;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

#[cfg(test)]
#[path = "write_tests.rs"]
mod write_tests;

pub fn definition(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "function", "name": name, "description": description,
        "parameters": {"type": "object", "properties": properties, "required": required, "additionalProperties": false}
    })
}

#[derive(Deserialize)]
struct ReadArgs {
    path: Option<String>,
    result_ref: Option<String>,
    pattern: Option<String>,
    regex: Option<bool>,
    start_line: Option<usize>,
    end_line: Option<usize>,
    line_count: Option<usize>,
    start_byte: Option<usize>,
    end_byte: Option<usize>,
}

#[derive(Deserialize)]
pub(super) struct TextChange { old_text: String, new_text: String }

#[derive(Serialize, Deserialize)]
struct Intent {
    operation: String,
    path: String,
    before: Option<String>,
    after: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tx_id: Option<String>,
}

pub(super) enum Edit { Write(String), Text(Vec<TextChange>) }
pub(super) struct ChangesetItem {
    pub target: PathBuf,
    pub path: String,
    pub expected: Option<Option<String>>,
    pub edit: Edit,
}
pub(super) type Reply = Result<Value, String>;

pub(super) fn rejected(code: &str, message: &str) -> Value {
    json!({"status": "error", "code": code, "reason": message})
}

pub(super) fn parse<T: DeserializeOwned>(args: &Value) -> Result<T, Value> {
    serde_json::from_value(args.clone()).map_err(|err| rejected("INVALID_ARGUMENT", &err.to_string()))
}

fn reserved(part: &str) -> bool {
    matches!(part.to_ascii_lowercase().as_str(), ".git" | ".sourceharness" | "target" | ".env" | ".env.local")
        || part.starts_with(".sourceharness-write-")
}

fn walk_ignored(part: &str) -> bool {
    reserved(part)
        || matches!(
            part.to_ascii_lowercase().as_str(),
            "node_modules" | "__pycache__" | ".venv" | "venv" | "dist" | "build" | ".pytest_cache" | ".mypy_cache"
        )
}

pub(crate) fn permitted(path: &Path) -> bool {
    path.components().all(|c| match c {
        Component::CurDir => true,
        Component::Normal(n) => {
            let p = n.to_string_lossy();
            !p.contains([':', '\\']) && !p.ends_with(['.', ' ']) && !reserved(&p)
        }
        _ => false,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ResolvePath { Existing, MissingFile, MissingParents }

pub(super) fn workspace_path(ctx: &WorkspaceContext, path: &str, mode: ResolvePath) -> Result<(PathBuf, String), Value> {
    let requested = Path::new(path);
    if path.is_empty() || !permitted(requested) {
        return Err(rejected("INVALID_PATH", "Use a relative workspace path outside reserved directories."));
    }
    let candidate = ctx.root().join(requested);
    let canonical = match fs::symlink_metadata(&candidate) {
        Ok(_) => candidate.canonicalize(),
        Err(err) if mode != ResolvePath::Existing && err.kind() == io::ErrorKind::NotFound => {
            let parent = candidate.parent().ok_or_else(|| rejected("INVALID_PATH", "Missing parent"))?;
            let resolved = if mode == ResolvePath::MissingParents { resolve_missing_parent(parent) } else { parent.canonicalize() };
            resolved.map(|p| p.join(candidate.file_name().unwrap_or_default()))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(rejected("FILE_NOT_FOUND", "This workspace path is absent. Use write if the task calls for a new file."));
        }
        Err(err) => Err(err),
    }
    .map_err(|err| rejected("PATH_UNAVAILABLE", &err.to_string()))?;
    let relative = canonical.strip_prefix(ctx.root()).map_err(|_| rejected("INVALID_PATH", "Resolved target leaves the workspace."))?;
    if !permitted(relative) {
        return Err(rejected("INVALID_PATH", "Resolved target is in a reserved directory."));
    }
    Ok((canonical.clone(), ctx.relative_display(&canonical)))
}

fn resolve_missing_parent(mut parent: &Path) -> io::Result<PathBuf> {
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(parent) {
            Ok(_) => {
                let mut dir = parent.canonicalize()?;
                if !fs::metadata(&dir)?.is_dir() { return Err(io::ErrorKind::NotADirectory.into()); }
                dir.extend(missing.into_iter().rev());
                return Ok(dir);
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                missing.push(parent.file_name().ok_or(io::ErrorKind::NotFound)?);
                parent = parent.parent().ok_or(io::ErrorKind::NotFound)?;
            }
            Err(err) => return Err(err),
        }
    }
}

fn verify_parent(path: &Path) -> Result<(), Value> {
    let canonical = path.canonicalize().map_err(|e| rejected("PATH_UNAVAILABLE", &e.to_string()))?;
    if canonical != path { return Err(rejected("PATH_CHANGED", "A parent directory resolved to a different location.")); }
    if !canonical.is_dir() { return Err(rejected("PATH_UNAVAILABLE", "Expected a parent directory.")); }
    Ok(())
}

fn create_parents(ctx: &WorkspaceContext, parent: &Path) -> Result<(), Value> {
    let relative = parent.strip_prefix(ctx.root()).map_err(|_| rejected("INVALID_PATH", "Resolved parent leaves the workspace."))?;
    let mut current = ctx.root().to_path_buf();
    verify_parent(&current)?;
    for component in relative.components() {
        current.push(component);
        match fs::create_dir(&current) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(rejected("WRITE_ERROR", &err.to_string())),
        }
        verify_parent(&current)?;
    }
    Ok(())
}

pub(crate) fn walk(root: &Path, limit: usize) -> Result<(Vec<PathBuf>, bool), String> {
    if root.is_file() { return Ok((vec![root.to_path_buf()], true)); }
    let (mut files, mut pending) = (Vec::new(), vec![root.to_path_buf()]);
    while let Some(dir) = pending.pop() {
        let mut children = fs::read_dir(dir).map_err(|e| e.to_string())?.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?;
        children.sort_by_key(|e| e.file_name());
        for entry in children {
            if walk_ignored(&entry.file_name().to_string_lossy()) { continue; }
            let kind = entry.file_type().map_err(|e| e.to_string())?;
            if kind.is_dir() { pending.push(entry.path()); }
            else if kind.is_file() {
                if files.len() >= limit { return Ok((files, false)); }
                files.push(entry.path());
            }
        }
    }
    files.sort();
    Ok((files, true))
}

pub(super) fn file_bytes(path: &Path, limit: usize) -> Result<Option<Vec<u8>>, Value> {
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(rejected("PATH_UNAVAILABLE", &err.to_string())),
    };
    if !meta.is_file() || meta.len() > limit as u64 {
        return Err(rejected("FILE_LIMIT", "Expected a regular file within the workspace file size limit."));
    }
    let bytes = fs::read(path).map_err(|err| rejected("READ_ERROR", &err.to_string()))?;
    if bytes.len() > limit || std::str::from_utf8(&bytes).is_err() {
        return Err(rejected("UNSUPPORTED_CONTENT", "Expected a UTF-8 file within the workspace file size limit."));
    }
    Ok(Some(bytes))
}

fn source(path: &str) -> Value { json!({"kind": "workspace", "path": path}) }

fn file_snapshot(journal: &mut Journal, run: usize, path: &str, bytes: &[u8]) -> Result<Artifact, String> {
    journal.artifact(run, source(path), bytes)
}

pub(super) fn workspace_revision(ctx: &WorkspaceContext, path: &str) -> Option<Value> {
    let (file, _) = match workspace_path(ctx, path, ResolvePath::Existing) {
        Ok(target) => target,
        Err(err) if err["code"] == "FILE_NOT_FOUND" => return Some(Value::Null),
        Err(_) => return None,
    };
    file_bytes(&file, ctx.config().max_file_bytes).ok().map(|b| json!(b.as_deref().map(revision)))
}

fn detect_eol(text: &str) -> &'static str {
    let lf = text.bytes().filter(|&b| b == b'\n').count();
    if lf == 0 { return "none"; }
    let crlf = text.match_indices("\r\n").count();
    if crlf == 0 { "lf" } else if crlf == lf { "crlf" } else { "mixed" }
}

fn line_offsets(text: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    offsets.extend(text.match_indices('\n').map(|(i, _)| i + 1));
    if !text.is_empty() && !text.ends_with('\n') { offsets.push(text.len()); }
    offsets
}

fn line_location(lines: &[usize], byte: usize) -> usize {
    lines.partition_point(|&start| start <= byte).min(lines.len().saturating_sub(1)).max(1)
}

fn read_page(raw: &[u8], artifact: &Artifact, args: &ReadArgs) -> Reply {
    let text = std::str::from_utf8(raw).map_err(|e| e.to_string())?;
    let lines = line_offsets(text);
    let count = lines.len() - 1;
    let user = match (args.start_line, args.end_line, args.line_count, args.start_byte, args.end_byte) {
        (None, None, None, start, end) => Some((start.unwrap_or(0), end.unwrap_or(text.len()))),
        (start, end, page, None, None) if end.is_none() || page.is_none() => {
            let start = start.unwrap_or(1);
            let end = end.unwrap_or_else(|| page.map_or(count, |n| start.saturating_sub(1).saturating_add(n))).min(count);
            (start > 0 && end >= start).then(|| lines.get(start - 1).zip(lines.get(end)).map(|(&a, &b)| (a, b))).flatten()
        }
        _ => None,
    };
    let Some((start, end)) = user.filter(|&(a, b)| a <= b && text.is_char_boundary(a) && text.is_char_boundary(b)) else {
        let mut error = rejected("INVALID_RANGE", if args.end_byte.is_some_and(|byte| byte > text.len()) {
            "end_byte is beyond the file. Use total_bytes or omit end_byte to read to the end."
        } else {
            "Use valid source bounds. Choose end_line or line_count for line reads, or use byte offsets."
        });
        error["result_ref"] = json!(artifact.result_ref);
        error["revision"] = json!(artifact.revision);
        error["total_bytes"] = json!(text.len());
        error["total_lines"] = json!(count);
        return Ok(error);
    };
    let (eol, trailing_newline) = (detect_eol(text), text.ends_with('\n'));
    let mut content = truncate_utf8_bytes(&text[start..end], 6144).0;
    loop {
        let stop = start + content.len();
        let mut result = json!({
            "status": "ok", "delivery": if stop < end { "folded" } else { "inline" },
            "source": artifact.source, "result_ref": artifact.result_ref, "revision": artifact.revision,
            "total_bytes": text.len(), "total_lines": count, "eol": eol, "trailing_newline": trailing_newline,
            "range": {
                "byte_start": start, "byte_end": stop,
                "start_line": line_location(&lines, start), "end_line": line_location(&lines, stop.saturating_sub(1).max(start))
            },
            "coverage": {"requested_bytes": end - start, "returned_bytes": content.len(), "complete": stop == end},
            "content": content
        });
        if stop < end { result["next"] = json!({"result_ref": artifact.result_ref, "start_byte": stop, "end_byte": end}); }
        if json_byte_len(&result) <= MAX_RESPONSE_BYTES { return Ok(result); }
        if content.is_empty() { return Ok(rejected("METADATA_LIMIT", "Source metadata exceeds the result budget.")); }
        content = truncate_utf8_bytes(content, content.len() / 2).0;
    }
}

pub fn read(ctx: &WorkspaceContext, journal: &mut Journal, run: usize, args: &Value) -> Reply {
    let args: ReadArgs = match parse(args) { Ok(v) => v, Err(err) => return Ok(err) };
    if args.pattern.is_some() { return read_search(ctx, journal, run, &args); }
    let mut dir_meta = None;
    let artifact = match (&args.path, &args.result_ref) {
        (Some(path), None) => {
            let (file, path) = match workspace_path(ctx, path, ResolvePath::Existing) { Ok(p) => p, Err(e) => return Ok(e) };
            if file.is_dir() {
                let (entries, complete) = walk(&file, ctx.config().max_list_entries)?;
                dir_meta = Some((entries.len(), complete));
                let mut listing = entries.iter().map(|p| ctx.relative_display(p)).collect::<Vec<_>>().join("\n");
                if listing.is_empty() { listing.push_str("(empty directory)"); }
                listing.push('\n');
                if !complete { listing.push_str("... (truncated at file limit)\n"); }
                journal.artifact(run, json!({"kind": "directory", "path": path}), listing.as_bytes())?
            } else {
                let bytes = match file_bytes(&file, ctx.config().max_file_bytes) {
                    Ok(Some(bytes)) => bytes,
                    Ok(None) => return Ok(rejected("PATH_UNAVAILABLE", "File is missing.")),
                    Err(e) => return Ok(e),
                };
                file_snapshot(journal, run, &path, &bytes)?
            }
        }
        (path, Some(reference)) => match journal.resolve(reference) {
            Some(a) if path.as_deref().is_none_or(|p| a.source["path"].as_str() == Some(p)) => a,
            Some(_) => return Ok(rejected("SOURCE_MISMATCH", "The supplied path does not identify the stored snapshot's source.")),
            None => return Ok(rejected("RESULT_NOT_FOUND", "No such snapshot in this session.")),
        },
        _ => return Ok(rejected("INVALID_ARGUMENT", "Supply either path or result_ref.")),
    };
    let mut res = read_page(&journal.get(&artifact.revision)?, &artifact, &args)?;
    if let Some((n, complete)) = dir_meta {
        res["directory_entries"] = json!(n);
        res["directory_complete"] = json!(complete);
    }
    Ok(res)
}

fn read_search(ctx: &WorkspaceContext, journal: &mut Journal, run: usize, args: &ReadArgs) -> Reply {
    let pat = args.pattern.as_deref().unwrap_or("");
    if pat.is_empty() { return Ok(rejected("INVALID_ARGUMENT", "pattern must be nonempty.")); }
    let re = if args.regex == Some(true) {
        Some(match regex::Regex::new(pat) { Ok(r) => r, Err(e) => return Ok(rejected("INVALID_ARGUMENT", &e.to_string())) })
    } else { None };
    let matches_line = |line: &str| re.as_ref().map_or_else(|| line.contains(pat), |r| r.is_match(line));
    let (mut total, mut hits) = (0usize, Vec::new());
    let mut scan_text = |label: &str, text: &str| {
        for (i, line) in text.lines().enumerate() {
            if matches_line(line) {
                total += 1;
                if hits.len() < 100 { hits.push(format!("{label}:{}\t{line}", i + 1)); }
            }
        }
    };
    let label = match (&args.path, &args.result_ref) {
        (Some(p), None) => {
            let (file, rel) = match workspace_path(ctx, p, ResolvePath::Existing) { Ok(v) => v, Err(e) => return Ok(e) };
            if file.is_dir() {
                let (files, _) = walk(&file, ctx.config().max_list_entries)?;
                for f in files {
                    if let Ok(Some(bytes)) = file_bytes(&f, ctx.config().max_file_bytes) {
                        if let Ok(s) = std::str::from_utf8(&bytes) { scan_text(&ctx.relative_display(&f), s); }
                    }
                }
            } else {
                let bytes = match file_bytes(&file, ctx.config().max_file_bytes) {
                    Ok(Some(b)) => b, Ok(None) => return Ok(rejected("PATH_UNAVAILABLE", "File is missing.")), Err(e) => return Ok(e),
                };
                scan_text(&rel, std::str::from_utf8(&bytes).map_err(|e| e.to_string())?);
            }
            rel
        }
        (p, Some(reference)) => match journal.resolve(reference) {
            Some(a) if p.as_deref().is_none_or(|x| a.source["path"].as_str() == Some(x)) => {
                let raw = journal.get(&a.revision)?;
                let lbl = a.source["path"].as_str().unwrap_or(reference).to_owned();
                scan_text(&lbl, std::str::from_utf8(&raw).map_err(|e| e.to_string())?);
                lbl
            }
            Some(_) => return Ok(rejected("SOURCE_MISMATCH", "The supplied path does not identify the stored snapshot's source.")),
            None => return Ok(rejected("RESULT_NOT_FOUND", "No such snapshot in this session.")),
        },
        _ => return Ok(rejected("INVALID_ARGUMENT", "Supply either path or result_ref with pattern.")),
    };
    let body = if hits.is_empty() { format!("No matches for {pat:?}.\n") }
        else if total > hits.len() { format!("{total} matches (showing first {}):\n{}\n", hits.len(), hits.join("\n")) }
        else { format!("{total} matches:\n{}\n", hits.join("\n")) };
    let art = journal.artifact(run, json!({"kind": "search", "path": label, "pattern": pat}), body.as_bytes())?;
    let mut res = read_page(body.as_bytes(), &art, &ReadArgs {
        path: None, result_ref: None, pattern: None, regex: None,
        start_line: None, end_line: None, line_count: None, start_byte: None, end_byte: None,
    })?;
    res["total_matches"] = json!(total);
    Ok(res)
}

pub(super) fn read_history(ctx: &WorkspaceContext, journal: &mut Journal, run: usize, raw_args: &Value) -> Reply {
    if !raw_args["paths"].is_null() { return Ok(rejected("INVALID_ARGUMENT", "Use history with optional path alone.")); }
    let args: ReadArgs = match parse(raw_args) { Ok(v) => v, Err(err) => return Ok(err) };
    if args.result_ref.is_some() { return Ok(rejected("INVALID_ARGUMENT", "Use history with optional path alone.")); }
    let filter_path = match args.path.as_deref() {
        Some(p) => match workspace_path(ctx, p, ResolvePath::MissingParents) { Ok((_, rel)) => Some(rel), Err(e) => return Ok(e) },
        None => None,
    };
    let mut entries: Vec<(usize, String)> = Vec::new();
    if let Some(target) = &filter_path {
        let (mut seen_revs, mut pending_initials) = (HashSet::new(), Vec::new());
        for e in &journal.events {
            if e.kind == "artifact" && e.data["source"]["kind"] == "workspace" && e.data["source"]["path"] == *target {
                let (r, rev) = (e.data["result_ref"].as_str().unwrap_or(""), e.data["revision"].as_str().unwrap_or(""));
                seen_revs.insert(rev.to_owned());
                entries.push((e.seq, format!("run {} event {} artifact result_ref={r:?} source={target:?} bytes={} rev={}", e.run, e.seq, e.data["bytes"], &rev[..rev.len().min(12)])));
            } else if e.kind == "edit_receipt" && e.data["status"] == "ok" && e.data["source"]["path"] == *target {
                let rev = e.data["revision"].as_str().unwrap_or("");
                let extra = e.data["diff_ref"].as_str().map(|d| format!(" diff_ref={d:?}")).unwrap_or_default();
                entries.push((e.seq, format!("run {} event {} edit path={target:?} rev={}{extra}", e.run, e.seq, &rev[..rev.len().min(12)])));
            } else if e.kind == "user" {
                if let Some(m_hash) = e.data["workspace_revision_at_start"].as_str() {
                    if let Ok(m) = journal.get(m_hash).and_then(|b| serde_json::from_slice::<BTreeMap<String, String>>(&b).map_err(|err| err.to_string())) {
                        if let Some(blob) = m.get(target) { pending_initials.push((e.run, e.seq, blob.clone())); }
                    }
                }
            }
        }
        for (u_run, u_seq, blob) in pending_initials {
            if !seen_revs.insert(blob.clone()) { continue; }
            if let Ok(bytes) = journal.get(&blob) {
                let art = journal.artifact(run, source(target), &bytes)?;
                entries.push((u_seq, format!("run {u_run} event {u_seq} initial_snapshot result_ref={:?} source={target:?} bytes={} rev={}", art.result_ref, bytes.len(), &blob[..blob.len().min(12)])));
            }
        }
        entries.sort_by_key(|(seq, _)| *seq);
    } else {
        for e in &journal.events {
            let line = match e.kind.as_str() {
                "user" => e.data["workspace_revision_at_start"].as_str().map(|r| format!("run {} event {} user checkpoint={}", e.run, e.seq, &r[..r.len().min(12)])),
                "artifact" if e.data["source"]["kind"] == "workspace" => {
                    let (r, p, rev) = (e.data["result_ref"].as_str().unwrap_or(""), e.data["source"]["path"].as_str().unwrap_or(""), e.data["revision"].as_str().unwrap_or(""));
                    Some(format!("run {} event {} artifact result_ref={r:?} source={p:?} bytes={} rev={}", e.run, e.seq, e.data["bytes"], &rev[..rev.len().min(12)]))
                }
                "edit_receipt" if e.data["status"] == "ok" => {
                    let (p, rev) = (e.data["source"]["path"].as_str().unwrap_or(""), e.data["revision"].as_str().unwrap_or(""));
                    let extra = e.data["diff_ref"].as_str().map(|d| format!(" diff_ref={d:?}")).unwrap_or_default();
                    let tx = e.data["tx_id"].as_str().map(|t| format!(" tx={t:?}")).unwrap_or_default();
                    Some(format!("run {} event {} edit path={p:?} rev={}{tx}{extra}", e.run, e.seq, &rev[..rev.len().min(12)]))
                }
                "changeset_receipt" if e.data["status"] == "ok" => {
                    let files: Vec<&str> = e.data["files"].as_array().into_iter().flatten()
                        .filter_map(|f| f["source"]["path"].as_str()).collect();
                    Some(format!("run {} event {} changeset tx={:?} files={files:?}", e.run, e.seq, e.data["tx_id"].as_str().unwrap_or("")))
                }
                "exec_receipt" => {
                    let ch = match (e.data["before_workspace_revision"].as_str(), e.data["workspace_revision"].as_str()) {
                        (Some(bh), Some(ah)) if bh != ah => {
                            let load = |h| journal.get(h).and_then(|b| serde_json::from_slice::<BTreeMap<String, String>>(&b).map_err(|err| err.to_string()));
                            if let (Ok(bm), Ok(am)) = (load(bh), load(ah)) {
                                let mut p: Vec<&str> = bm.keys().chain(am.keys()).filter(|k| bm.get(*k) != am.get(*k)).map(String::as_str).collect();
                                p.sort_unstable(); p.dedup(); p.truncate(6);
                                if p.is_empty() { String::new() } else { format!(" changed={p:?}") }
                            } else { String::new() }
                        }
                        _ => String::new(),
                    };
                    Some(format!("run {} event {} exec exit={} result_ref={:?}{ch}", e.run, e.seq, e.data["exit_code"], e.data["result_ref"].as_str().unwrap_or("")))
                }
                "validation_receipt" => Some(format!(
                    "run {} event {} validate check={:?} {} result_ref={:?}",
                    e.run, e.seq, e.data["check"].as_str().unwrap_or(""), if e.data["passed"] == true { "PASS" } else { "FAIL" }, e.data["result_ref"].as_str().unwrap_or("")
                )),
                "rollback_receipt" => Some(format!("run {} event {} rollback target_event={} paths={}", e.run, e.seq, e.data["checkpoint_event"], e.data["paths"])),
                _ => None,
            };
            if let Some(l) = line { entries.push((e.seq, l)); }
        }
    }
    let mut text = entries.into_iter().map(|(_, l)| l).collect::<Vec<_>>().join("\n");
    text.push_str(if text.is_empty() { "No archived workspace timeline entries recorded in this session.\n" }
        else if filter_path.is_some() { "\nUse read(result_ref=\"...\") to inspect a historical snapshot; cite result_ref (e.g. r5) or event number when reporting a snapshot source.\n" }
        else { "\nUse read(result_ref=\"...\") for snapshots/outputs (cite result_ref or event number as source), or rollback(to_event=..., paths=[...], dry_run=true).\n" });
    let artifact = journal.artifact(run, json!({"kind": "history_index", "path": filter_path}), text.as_bytes())?;
    read_page(text.as_bytes(), &artifact, &args)
}

pub(super) fn inspect_artifact_content(artifact: &Artifact, load: impl Fn(&str) -> Result<Vec<u8>, String>) -> Result<String, String> {
    String::from_utf8(load(&artifact.revision)?).map_err(|e| e.to_string())
}

fn conflict(journal: &mut Journal, run: usize, path: &str, before: &Option<String>, actual: Option<&[u8]>) -> Reply {
    let mut error = rejected("REVISION_CONFLICT", "Review the current content before changing this file.");
    error["path"] = json!(path);
    error["expected_revision"] = json!(before);
    if let Some(bytes) = actual {
        let artifact = file_snapshot(journal, run, path, bytes)?;
        let content = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
        error["source"] = json!(artifact.source);
        error["revision"] = json!(artifact.revision);
        error["current_revision"] = json!(artifact.revision);
        error["result_ref"] = json!(artifact.result_ref);
        error["preview"] = json!(truncate_utf8_bytes(content, 512).0);
    } else {
        error["current_revision"] = Value::Null;
        error["reason"] = json!("The file is absent. Use write if the task calls for creating it.");
    }
    Ok(error)
}

pub(super) fn add_conflict_diff(journal: &mut Journal, run: usize, result: &mut Value) -> Result<(), String> {
    let (Some(before), Some(after)) = (result["expected_revision"].as_str(), result["current_revision"].as_str()) else { return Ok(()); };
    let (prev, curr) = (journal.get(before)?, journal.get(after)?);
    let diff = diffy::create_patch(
        std::str::from_utf8(&prev).map_err(|e| e.to_string())?,
        std::str::from_utf8(&curr).map_err(|e| e.to_string())?,
    ).to_string();
    let (preview, clipped) = truncate_utf8_bytes(&diff, 1024);
    let artifact = journal.artifact(run, json!({"kind": "diff", "path": result["path"], "before": before, "after": after}), diff.as_bytes())?;
    result["diff"] = json!(preview);
    if clipped { result["diff_ref"] = json!(artifact.result_ref); }
    Ok(())
}

fn edit_text(old: &str, changes: Vec<TextChange>) -> Result<String, Value> {
    if changes.is_empty() { return Err(rejected("EMPTY_EDIT", "Supply at least one local edit.")); }
    let mut located = Vec::new();
    let file_pure_crlf = detect_eol(old) == "crlf";
    for change in changes {
        if change.old_text.is_empty() { return Err(rejected("EMPTY_MATCH", "old_text must identify existing source text.")); }
        let needle = if file_pure_crlf && !change.old_text.contains("\r\n") && change.old_text.contains('\n') {
            change.old_text.replace('\n', "\r\n")
        } else {
            change.old_text
        };
        let replacement = if file_pure_crlf && !change.new_text.contains("\r\n") && change.new_text.contains('\n') {
            change.new_text.replace('\n', "\r\n")
        } else {
            change.new_text
        };
        let Some(start) = old.find(&needle) else { return Err(missing_text_error(old, &needle)); };
        let next = start + needle.chars().next().expect("nonempty").len_utf8();
        if let Some(other) = old[next..].find(&needle).map(|o| next + o) {
            let offsets = line_offsets(old);
            let mut error = rejected("AMBIGUOUS_MATCH", "old_text matches more than once. Include unique context.");
            error["match_lines"] = json!([line_location(&offsets, start), line_location(&offsets, other)]);
            error["match_byte_starts"] = json!([start, other]);
            return Err(error);
        }
        located.push((start, start + needle.len(), replacement));
    }
    located.sort_by_key(|edit| (edit.0, edit.1));
    let (mut cursor, mut candidate) = (0, String::new());
    for (start, end, new_text) in located {
        if start < cursor { return Err(rejected("OVERLAPPING_EDITS", "Changes overlap in the original revision.")); }
        candidate.push_str(&old[cursor..start]);
        candidate.push_str(&new_text);
        cursor = end;
    }
    candidate.push_str(&old[cursor..]);
    Ok(candidate)
}

fn missing_text_error(old: &str, needle: &str) -> Value {
    let mut error = rejected("TEXT_NOT_FOUND", "old_text is absent. Read the exact source range.");
    let (file_crlf, needle_crlf) = (old.contains("\r\n"), needle.contains("\r\n"));
    let swapped = if file_crlf && needle.contains('\n') && !needle_crlf { Some((needle.replace('\n', "\r\n"), "crlf", "lf")) }
        else if !file_crlf && needle_crlf { Some((needle.replace("\r\n", "\n"), "lf", "crlf")) } else { None };
    if let Some((converted, file_eol, needle_eol)) = swapped {
        if let Some(start) = old.find(&converted) {
            let offsets = line_offsets(old);
            error["reason"] = json!(format!("old_text is absent. The file uses {file_eol} line endings while old_text uses {needle_eol}; read the exact range before retrying."));
            error["line_ending_mismatch"] = json!({"file": file_eol, "old_text": needle_eol});
            error["candidate_range"] = json!({"start_line": line_location(&offsets, start), "end_line": line_location(&offsets, start + converted.len().saturating_sub(1))});
            return error;
        }
    }
    if let Some(anchor) = needle.lines().map(str::trim).find(|l| !l.is_empty()) {
        let (mut total, mut hits) = (0usize, Vec::new());
        for (idx, line) in old.lines().enumerate() {
            total = idx + 1;
            if line.trim().contains(anchor) && hits.len() < 3 { hits.push(idx + 1); }
        }
        if let Some(&first) = hits.first() {
            error["anchor_lines"] = json!(hits);
            error["candidate_range"] = json!({"start_line": first.saturating_sub(2).max(1), "end_line": (first + 2).min(total.max(1))});
        }
    }
    error
}

pub(super) fn resume_edit(ctx: &WorkspaceContext, journal: &mut Journal, run: usize, operation: &str) -> Option<Reply> {
    if let Some(receipt) = journal.events.iter().find(|e| e.kind == "edit_receipt" && e.data["operation"] == operation) {
        return Some(Ok(receipt.data.clone()));
    }
    if let Some(saved) = journal.events.iter().find(|e| e.kind == "edit_intent" && e.data["operation"] == operation) {
        let intent = serde_json::from_value(saved.data.clone()).map_err(|e| e.to_string());
        return Some(intent.and_then(|intent| commit(ctx, journal, run, &intent, true)));
    }
    None
}

pub(super) fn apply_edit(
    ctx: &WorkspaceContext, journal: &mut Journal, run: usize, operation: &str,
    (target, path): (PathBuf, String), expected: Option<Option<String>>, edit: Edit,
) -> Reply {
    let origin = match file_bytes(&target, ctx.config().max_file_bytes) { Ok(b) => b, Err(e) => return Ok(e) };
    let before = origin.as_deref().map(revision);
    if let Some(expected) = expected.filter(|exp| *exp != before) {
        return conflict(journal, run, &path, &expected, origin.as_deref());
    }
    if before.is_none() && !matches!(edit, Edit::Write(_)) {
        let mut error = rejected("FILE_NOT_FOUND", "The file is absent. Use write to create it.");
        error["path"] = json!(path);
        return Ok(error);
    }
    let old = std::str::from_utf8(origin.as_deref().unwrap_or_default()).map_err(|e| e.to_string())?;
    let next = match match edit { Edit::Text(c) => edit_text(old, c), Edit::Write(c) => Ok(c) } {
        Ok(next) => next,
        Err(mut error) => {
            let artifact = file_snapshot(journal, run, &path, old.as_bytes())?;
            error["source"] = json!(artifact.source);
            error["result_ref"] = json!(artifact.result_ref);
            error["current_revision"] = json!(artifact.revision);
            if let Some(range) = error.get("candidate_range").cloned() {
                error["read"] = json!({"result_ref": artifact.result_ref, "start_line": range["start_line"], "end_line": range["end_line"]});
            }
            return Ok(error);
        }
    };
    if next.len() > ctx.config().max_file_bytes {
        return Ok(rejected("FILE_LIMIT", "Candidate content exceeds the workspace file size limit."));
    }
    if let Some(bytes) = &origin { journal.put(bytes)?; }
    let after = journal.put(next.as_bytes())?;
    if before.as_deref() == Some(after.as_str()) {
        let artifact = file_snapshot(journal, run, &path, old.as_bytes())?;
        let mut error = rejected("NO_OP_EDIT", "The requested change produces identical file bytes; nothing was written.");
        error["source"] = json!(artifact.source);
        error["current_revision"] = json!(artifact.revision);
        error["result_ref"] = json!(artifact.result_ref);
        return Ok(error);
    }
    let intent = Intent { operation: operation.to_owned(), path, before, after, tx_id: None };
    journal.append(run, "edit_intent", json!(intent))?;
    commit(ctx, journal, run, &intent, false)
}

pub(super) fn apply_changeset(
    ctx: &WorkspaceContext, journal: &mut Journal, run: usize, operation: &str, items: Vec<ChangesetItem>,
) -> Reply {
    if let Some(saved) = journal.events.iter().find(|e| e.kind == "changeset_receipt" && e.data["operation"] == operation) {
        return Ok(saved.data.clone());
    }
    if let Some(event) = journal.events.iter().rev().find(|e| {
        e.kind == "changeset_recovery" && e.data["operation"] == operation && e.data["status"] == "conflict"
    }) {
        let mut err = rejected("CHANGESET_CONFLICT", "An earlier attempt at this atomic edit stopped partway and one of its files has changed since. Inspect the workspace before retrying.");
        err["path"] = event.data["path"].clone();
        return Ok(err);
    }
    if items.is_empty() {
        return Ok(rejected("INVALID_ARGUMENT", "files must contain at least one file edit."));
    }
    let mut seen = HashSet::new();
    let mut planned = Vec::with_capacity(items.len());
    for (idx, item) in items.into_iter().enumerate() {
        if !seen.insert(item.path.clone()) {
            let mut err = rejected("INVALID_ARGUMENT", "Duplicate path in atomic edit files list.");
            err["path"] = json!(item.path);
            err["failed_path"] = json!(item.path);
            err["failed_index"] = json!(idx);
            return Ok(err);
        }
        let origin = match file_bytes(&item.target, ctx.config().max_file_bytes) {
            Ok(b) => b,
            Err(mut err) => {
                err["failed_path"] = json!(item.path);
                err["failed_index"] = json!(idx);
                return Ok(err);
            }
        };
        let before = origin.as_deref().map(revision);
        if let Some(expected) = item.expected.filter(|exp| *exp != before) {
            let mut err = conflict(journal, run, &item.path, &expected, origin.as_deref())?;
            err["failed_path"] = json!(item.path);
            err["failed_index"] = json!(idx);
            return Ok(err);
        }
        if before.is_none() && !matches!(item.edit, Edit::Write(_)) {
            let mut err = rejected("FILE_NOT_FOUND", "The file is absent. Use content to create it.");
            err["path"] = json!(item.path);
            err["failed_path"] = json!(item.path);
            err["failed_index"] = json!(idx);
            return Ok(err);
        }
        let old = std::str::from_utf8(origin.as_deref().unwrap_or_default()).map_err(|e| e.to_string())?;
        let next = match match item.edit { Edit::Text(c) => edit_text(old, c), Edit::Write(c) => Ok(c) } {
            Ok(next) => next,
            Err(mut error) => {
                let artifact = file_snapshot(journal, run, &item.path, old.as_bytes())?;
                error["source"] = json!(artifact.source);
                error["path"] = json!(item.path);
                error["failed_path"] = json!(item.path);
                error["failed_index"] = json!(idx);
                error["result_ref"] = json!(artifact.result_ref);
                error["current_revision"] = json!(artifact.revision);
                if let Some(range) = error.get("candidate_range").cloned() {
                    error["read"] = json!({"result_ref": artifact.result_ref, "start_line": range["start_line"], "end_line": range["end_line"]});
                }
                return Ok(error);
            }
        };
        if next.len() > ctx.config().max_file_bytes {
            let mut err = rejected("FILE_LIMIT", "Candidate content exceeds the workspace file size limit.");
            err["path"] = json!(item.path);
            err["failed_path"] = json!(item.path);
            err["failed_index"] = json!(idx);
            return Ok(err);
        }
        let after = revision(next.as_bytes());
        if before.as_deref() == Some(after.as_str()) {
            let artifact = file_snapshot(journal, run, &item.path, old.as_bytes())?;
            let mut err = rejected("NO_OP_EDIT", "The requested change produces identical file bytes; nothing was written.");
            err["source"] = json!(artifact.source);
            err["path"] = json!(item.path);
            err["failed_path"] = json!(item.path);
            err["failed_index"] = json!(idx);
            err["current_revision"] = json!(artifact.revision);
            err["result_ref"] = json!(artifact.result_ref);
            return Ok(err);
        }
        planned.push((item.path, origin, next, before, after));
    }

    let before_workspace_revision = super::validation::save_workspace(ctx, journal)?;
    let mut planned_files = Vec::with_capacity(planned.len());
    for (path, origin, next, before, after) in &planned {
        if let Some(bytes) = origin { journal.put(bytes)?; }
        journal.put(next.as_bytes())?;
        planned_files.push(json!({"path": path, "before": before, "after": after}));
    }
    journal.append(run, "changeset_intent", json!({
        "operation": operation, "tx_id": operation,
        "before_workspace_revision": before_workspace_revision, "files": planned_files,
    }))?;
    let mut receipts = Vec::with_capacity(planned.len());
    for (idx, (path, _, _, before, after)) in planned.into_iter().enumerate() {
        let sub_op = format!("{operation}.{}", idx + 1);
        let intent = Intent { operation: sub_op, path, before, after, tx_id: Some(operation.to_owned()) };
        journal.append(run, "edit_intent", json!(intent))?;
        let rec = commit(ctx, journal, run, &intent, false)?;
        if rec["status"] != "ok" {
            return Ok(rec);
        }
        receipts.push(rec);
    }
    let workspace_revision = super::validation::save_workspace(ctx, journal)?;
    let receipt = json!({
        "status": "ok",
        "operation": operation,
        "tx_id": operation,
        "before_workspace_revision": before_workspace_revision,
        "workspace_revision": workspace_revision,
        "files": receipts,
    });
    journal.append(run, "changeset_receipt", receipt.clone())?;
    Ok(receipt)
}

pub(super) fn resume_changesets(ctx: &WorkspaceContext, journal: &mut Journal) -> Result<(), String> {
    loop {
        let pending = journal.events.iter().find_map(|event| {
            let operation = event.data["operation"].clone();
            let done = journal.events.iter().any(|other| {
                other.data["operation"] == operation
                    && (other.kind == "changeset_receipt"
                        || (other.kind == "changeset_recovery" && other.data["status"] == "conflict"))
            });
            (event.kind == "changeset_intent" && !done).then(|| (event.run, event.data.clone()))
        });
        let Some((run, intent)) = pending else { return Ok(()); };
        let operation = intent["operation"].as_str().unwrap_or_default().to_owned();
        let (mut receipts, mut applied) = (Vec::new(), Vec::new());
        let mut blocked = None;
        for (index, file) in intent["files"].as_array().cloned().unwrap_or_default().into_iter().enumerate() {
            let path = file["path"].as_str().unwrap_or_default().to_owned();
            let (before, after) = (file["before"].as_str().map(str::to_owned), file["after"].as_str().unwrap_or_default().to_owned());
            let Ok((target, _)) = workspace_path(ctx, &path, ResolvePath::MissingParents) else {
                blocked = Some(path);
                break;
            };
            let current = match file_bytes(&target, ctx.config().max_file_bytes) {
                Ok(bytes) => bytes,
                Err(_) => {
                    blocked = Some(path);
                    break;
                }
            };
            let revision = current.as_deref().map(revision);
            if revision.as_deref() == Some(after.as_str()) {
                let sub_op = format!("{operation}.{}", index + 1);
                let saved = journal.events.iter().find(|event| event.kind == "edit_receipt" && event.data["operation"] == sub_op);
                receipts.push(match saved {
                    Some(saved) => saved.data.clone(),
                    None => {
                        let content = journal.get(&after)?;
                        let text = std::str::from_utf8(&content).unwrap_or_default();
                        json!({
                            "status": "ok", "outcome": "already_applied", "operation": sub_op,
                            "path": path, "source": source(&path), "revision": after, "bytes": content.len(),
                            "lines": line_offsets(text).len().saturating_sub(1), "eol": detect_eol(text),
                            "trailing_newline": content.ends_with(b"\n"),
                        })
                    }
                });
                applied.push(json!(path));
                continue;
            }
            if revision != before {
                blocked = Some(path);
                break;
            }
            let sub_op = format!("{operation}.{}", index + 1);
            let reply = match resume_edit(ctx, journal, run, &sub_op) {
                Some(reply) => reply,
                None => {
                    let intent = Intent { operation: sub_op, path: path.clone(), before, after: after.clone(), tx_id: Some(operation.clone()) };
                    journal.append(run, "edit_intent", json!(intent))?;
                    commit(ctx, journal, run, &intent, true)
                }
            };
            let receipt = reply?;
            if receipt["status"] != "ok" {
                blocked = Some(path);
                break;
            }
            receipts.push(receipt);
        }
        if let Some(path) = blocked {
            journal.append(run, "changeset_recovery", json!({
                "status": "conflict", "operation": operation, "path": path,
                "reason": "A file moved since the interrupted changeset; inspect the workspace before retrying."
            }))?;
            return Ok(());
        }
        let workspace_revision = super::validation::save_workspace(ctx, journal)?;
        journal.append(run, "changeset_recovery", json!({
            "status": "completed", "operation": operation, "already_applied": applied,
        }))?;
        journal.append(run, "changeset_receipt", json!({
            "status": "ok", "operation": operation, "tx_id": operation,
            "before_workspace_revision": intent["before_workspace_revision"],
            "workspace_revision": workspace_revision, "files": receipts,
        }))?;
    }
}

pub(super) fn restore_file(
    ctx: &WorkspaceContext, journal: &mut Journal, run: usize, operation: &str, path: &str, before: Option<&str>, after: Option<&str>,
) -> Reply {
    if let Some(after) = after {
        if journal.events.iter().any(|e| e.kind == "edit_receipt" && e.data["operation"] == operation) {
            return Ok(if workspace_revision(ctx, path).as_ref().and_then(Value::as_str) == Some(after) {
                json!({"status": "ok"})
            } else { rejected("REVISION_CONFLICT", "A restored file changed before rollback completed.") });
        }
        return commit(ctx, journal, run, &Intent {
            operation: operation.to_owned(), path: path.to_owned(), before: before.map(str::to_owned), after: after.to_owned(), tx_id: None
        }, true);
    }
    let (target, name) = match workspace_path(ctx, path, ResolvePath::MissingParents) { Ok(v) => v, Err(e) => return Ok(e) };
    if name != path { return Ok(rejected("PATH_CHANGED", "The rollback path resolves to another file.")); }
    match fs::symlink_metadata(&target) {
        Ok(meta) if !meta.is_file() => return Ok(rejected("FILE_LIMIT", "Expected a regular file to remove.")),
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(json!({"status": "ok"})),
        Err(err) => return Ok(rejected("PATH_UNAVAILABLE", &err.to_string())),
    }
    let bytes = fs::read(&target).map_err(|e| e.to_string())?;
    if before.is_some_and(|expected| revision(&bytes) != expected) {
        return Ok(rejected("REVISION_CONFLICT", "The created file has changed."));
    }
    if let Err(err) = verify_parent(target.parent().ok_or("Missing rollback parent")?) { return Ok(err); }
    fs::remove_file(target).map_err(|e| e.to_string())?;
    Ok(json!({"status": "ok"}))
}

fn commit(ctx: &WorkspaceContext, journal: &mut Journal, run: usize, intent: &Intent, recovering: bool) -> Reply {
    let creating = intent.before.is_none();
    let resolve = if creating { ResolvePath::MissingParents } else { ResolvePath::MissingFile };
    let (target, path) = match workspace_path(ctx, &intent.path, resolve) { Ok(p) => p, Err(e) => return Ok(e) };
    if path != intent.path { return Ok(rejected("PATH_CHANGED", "The recorded path resolved to another file.")); }
    let before = match file_bytes(&target, ctx.config().max_file_bytes) { Ok(v) => v, Err(e) => return Ok(e) };
    let current = before.as_deref().map(revision);
    let next = journal.get(&intent.after)?;
    let disposition = if current.as_deref() == Some(&intent.after) {
        if recovering { "observed_target_on_recovery" } else { "already_current" }
    } else if current != intent.before {
        let mut result = conflict(journal, run, &path, &intent.before, before.as_deref())?;
        if recovering {
            result["planned_revision"] = json!(intent.after);
            if let Some(actual) = before.as_deref() {
                let delta = diffy::create_patch(
                    std::str::from_utf8(&next).map_err(|e| e.to_string())?,
                    std::str::from_utf8(actual).map_err(|e| e.to_string())?,
                ).to_string();
                let diff = journal.artifact(
                    run, json!({"kind":"diff","path":path,"before":intent.after,"after":current,"comparison":"planned_to_current"}), delta.as_bytes()
                )?;
                let (preview, clipped) = truncate_utf8_bytes(&delta, 800);
                result["planned_to_current"] = json!({"result_ref":diff.result_ref,"diff":preview,"complete":!clipped});
            }
        }
        return Ok(result);
    } else {
        let parent = target.parent().ok_or("Missing target parent")?;
        if creating { if let Err(err) = create_parents(ctx, parent) { return Ok(err); } }
        if let Err(err) = verify_parent(parent) { return Ok(err); }
        let mut temp = tempfile::Builder::new().prefix(".sourceharness-write-").tempfile_in(parent).map_err(|e| e.to_string())?;
        temp.write_all(&next).map_err(|e| e.to_string())?;
        if before.is_some() {
            temp.as_file().set_permissions(fs::metadata(&target).map_err(|e| e.to_string())?.permissions()).map_err(|e| e.to_string())?;
        }
        temp.as_file().sync_all().map_err(|e| e.to_string())?;
        if let Err(err) = verify_parent(parent) { return Ok(err); }
        let latest = match file_bytes(&target, ctx.config().max_file_bytes) { Ok(v) => v, Err(e) => return Ok(e) };
        if latest.as_deref().map(revision) != intent.before { return conflict(journal, run, &path, &intent.before, latest.as_deref()); }
        let persisted = if before.is_some() { temp.persist(&target) } else { temp.persist_noclobber(&target) };
        if let Err(err) = persisted {
            if err.error.kind() == io::ErrorKind::AlreadyExists {
                let now = match file_bytes(&target, ctx.config().max_file_bytes) { Ok(v) => v, Err(e) => return Ok(e) };
                return conflict(journal, run, &path, &intent.before, now.as_deref());
            }
            return Ok(rejected("WRITE_ERROR", &err.error.to_string()));
        }
        match std::env::var("SOURCE_HARNESS_TEST_INTERRUPT").as_deref() {
            Ok("after_file_replace") => std::process::exit(86),
            #[cfg(unix)] Ok("kill_after_file_replace") => unsafe { libc::raise(libc::SIGKILL); }
            #[cfg(not(unix))] Ok("kill_after_file_replace") => std::process::exit(86),
            _ => {}
        }
        let observed = match file_bytes(&target, ctx.config().max_file_bytes) { Ok(v) => v, Err(e) => return Ok(e) };
        if observed.as_deref().map(revision).as_deref() != Some(&intent.after) {
            return conflict(journal, run, &path, &Some(intent.after.clone()), observed.as_deref());
        }
        "committed"
    };
    let original = intent.before.as_deref().map(|h| journal.get(h)).transpose()?.unwrap_or_default();
    let next_str = std::str::from_utf8(&next).map_err(|e| e.to_string())?;
    let dropped_trailing_newline = original.ends_with(b"\n") && !next.is_empty() && !next.ends_with(b"\n");
    let delta = diffy::create_patch(std::str::from_utf8(&original).map_err(|e| e.to_string())?, next_str).to_string();
    let (preview, clipped) = truncate_utf8_bytes(&delta, 1024);
    let diff = journal.artifact(run, json!({"kind":"diff","path":path,"before":intent.before,"after":intent.after}), delta.as_bytes())?;
    let mut receipt = json!({
        "status": "ok", "outcome": disposition, "operation": intent.operation, "source": source(&path),
        "before_revision": intent.before, "revision": intent.after, "bytes": next.len(),
        "lines": line_offsets(next_str).len().saturating_sub(1), "eol": detect_eol(next_str),
        "trailing_newline": next_str.ends_with('\n'), "diff": preview, "diff_complete": !clipped
    });
    if dropped_trailing_newline { receipt["dropped_trailing_newline"] = json!(true); }
    if let Some(tx_id) = &intent.tx_id { receipt["tx_id"] = json!(tx_id); }
    if clipped { receipt["diff_ref"] = json!(diff.result_ref); }
    journal.append(run, "edit_receipt", receipt.clone())?;
    Ok(receipt)
}
