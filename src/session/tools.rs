use super::journal::{revision, Artifact, Journal};
use crate::budget::{json_byte_len, truncate_utf8_bytes, MAX_RESPONSE_BYTES};
use crate::store::{build_line_spans, Snapshot};
use crate::workspace::WorkspaceContext;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

#[cfg(test)]
#[path = "retrieval_tests.rs"]
mod retrieval_tests;

#[derive(Clone, Copy, Debug, Default)]
pub enum EditMode {
    #[default]
    Text,
    Range,
    Patch,
}

pub fn definition(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({"type":"function","name":name,"description":description,
           "parameters":{"type":"object","properties":properties,"required":required,"additionalProperties":false}})
}

pub fn definitions(mode: EditMode) -> Vec<Value> {
    let path =
        json!({"type":"string","description":"Workspace-relative file path from the evidence."});
    let version = json!({"type":"string","description":"Exact revision issued by read or research for this file."});
    let reference =
        json!({"type":"string","description":"Immutable result_ref from a prior result."});
    let edit = match mode {
        EditMode::Text => definition("edit", "Apply exact, non-overlapping substitutions together against one file revision. Each old_text must match once. Use exact source text including indentation and line endings.",
            json!({"path":path,"expected_revision":version,"changes":{"type":"array","items":{"type":"object","properties":{
                "old_text":{"type":"string"},"new_text":{"type":"string"}},"required":["old_text","new_text"],"additionalProperties":false}}}), &["path","expected_revision","changes"]),
        EditMode::Range => definition("edit", "Replace non-overlapping 1-based inclusive source line ranges in one file revision. new_text replaces the exact raw range, including its ending newline when present.",
            json!({"path":path,"expected_revision":version,"changes":{"type":"array","items":{"type":"object","properties":{
                "start_line":{"type":"integer","minimum":1},"end_line":{"type":"integer","minimum":1},"new_text":{"type":"string"}},"required":["start_line","end_line","new_text"],"additionalProperties":false}}}), &["path","expected_revision","changes"]),
        EditMode::Patch => definition("edit", "Apply a unified text patch to exactly one file revision. Use the supplied path as the target and preserve unrelated content.",
            json!({"path":path,"expected_revision":version,"patch":{"type":"string"}}), &["path","expected_revision","patch"]),
    };
    let mut indexed_search = crate::registry::search_result_schema();
    indexed_search["description"] = json!("Search lines in a saved session result for exact counts, matching lines, or regex capture groups. Use a result_ref from this session. Omit line bounds for one full scan. Use matching_lines for a line count and total_occurrences for a match count. Once those counts answer the task, respond without another search.");
    indexed_search["parameters"]["properties"]["result_ref"]["description"] =
        json!("Copy the immutable result_ref from a prior session tool result.");
    vec![
        definition("research", "Find literal evidence in the workspace, a saved result, or session history. Pass next back to research to enumerate the saved match set. Read the hit's result_ref for the original bytes. Historical calls can include arguments_ref for read.",
            json!({"scope":{"type":"string","enum":["workspace","result","history","page"]},
                "path":{"type":"string","description":"Workspace scope only. Omit to search the whole workspace."},
                "result_ref":{"type":"string","description":"Result or page scope only. Copy a saved result_ref."},
                "query":{"type":"string","description":"Case-sensitive literal, including exact line endings for multiline text. Omit to list workspace paths or recent history."},
                "offset":{"type":"integer","minimum":0,"description":"Copy the offset from next when scope is page."},
                "limit":{"type":"integer","minimum":1,"maximum":20,"default":8,"description":"Hits per page, up to 20. Follow next for more."}}), &["scope"]),
        definition("read", "Read exact original text by file path or an immutable result_ref. A path supplied with a reference must match its source. Use 1-based source lines or UTF-8 byte offsets. Follow next exactly to keep the same snapshot and requested range.",
            json!({"path":path,"result_ref":reference,"start_line":{"type":"integer","minimum":1},
                "line_count":{"type":"integer","minimum":1},"start_byte":{"type":"integer","minimum":0},"end_byte":{"type":"integer","minimum":0}}), &[]),
        edit,
        definition("write", "Create a UTF-8 file at an absent path with mode=create. For create, omit expected_revision. For a full replacement, use mode=replace and pass the observed revision of that same file. Parent directory must exist. Returns a verified revision and diff.",
            json!({"path":path,"content":{"type":"string"},"mode":{"type":"string","enum":["create","replace"]},
                "expected_revision":{"type":"string","description":"Use the observed file revision for mode=replace. Omit this parameter for mode=create."}}), &["path","content","mode"]),
        indexed_search,
    ]
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: Option<String>,
    result_ref: Option<String>,
    start_line: Option<usize>,
    line_count: Option<usize>,
    start_byte: Option<usize>,
    end_byte: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResearchArgs {
    scope: String,
    path: Option<String>,
    result_ref: Option<String>,
    query: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
    mode: String,
    expected_revision: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs<T> {
    path: String,
    expected_revision: String,
    changes: Vec<T>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextChange {
    old_text: String,
    new_text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RangeChange {
    start_line: usize,
    end_line: usize,
    new_text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchArgs {
    path: String,
    expected_revision: String,
    patch: String,
}

#[derive(Serialize, Deserialize)]
struct Intent {
    operation: String,
    path: String,
    before: Option<String>,
    after: String,
}

type Reply = Result<Value, String>;

fn rejected(code: &str, message: &str) -> Value {
    json!({"status":"error","code":code,"reason":message})
}

fn parse<T: DeserializeOwned>(args: &Value) -> Result<T, Value> {
    serde_json::from_value(args.clone())
        .map_err(|err| rejected("INVALID_ARGUMENT", &err.to_string()))
}

fn reserved(part: &str) -> bool {
    matches!(
        part.to_ascii_lowercase().as_str(),
        ".git" | ".sourceharness" | "target" | ".env"
    ) || part.starts_with(".sourceharness-write-")
        || part.starts_with(".env.")
}

pub(crate) fn permitted(path: &Path) -> bool {
    path.components().all(|component| match component {
        Component::CurDir => true,
        Component::Normal(name) => {
            let part = name.to_string_lossy();
            !part.contains([':', '\\']) && !part.ends_with(['.', ' ']) && !reserved(&part)
        }
        _ => false,
    })
}

pub(crate) fn workspace_path(
    ctx: &WorkspaceContext,
    path: &str,
    allow_new: bool,
) -> Result<(PathBuf, String), Value> {
    let requested = Path::new(path);
    if path.is_empty() || !permitted(requested) {
        return Err(rejected(
            "INVALID_PATH",
            "Use a relative workspace path outside reserved directories.",
        ));
    }
    let candidate = ctx.root().join(requested);
    let canonical = match fs::symlink_metadata(&candidate) {
        Ok(_) => candidate.canonicalize(),
        Err(err) if allow_new && err.kind() == io::ErrorKind::NotFound => {
            let parent = candidate
                .parent()
                .ok_or_else(|| rejected("INVALID_PATH", "Missing parent"))?;
            parent
                .canonicalize()
                .map(|parent| parent.join(candidate.file_name().unwrap_or_default()))
        }
        Err(err) if err.kind()==io::ErrorKind::NotFound => {
            return Err(rejected("FILE_NOT_FOUND","This workspace path is absent. Use write with mode=create if the task calls for a new file."));
        }
        Err(err) => Err(err),
    }
    .map_err(|err| rejected("PATH_UNAVAILABLE", &err.to_string()))?;
    let relative = canonical
        .strip_prefix(ctx.root())
        .map_err(|_| rejected("INVALID_PATH", "Resolved target leaves the workspace."))?;
    if !permitted(relative) {
        return Err(rejected(
            "INVALID_PATH",
            "Resolved target is in a reserved directory.",
        ));
    }
    Ok((canonical.clone(), ctx.relative_display(&canonical)))
}

pub(crate) fn walk(root: &Path, limit: usize) -> Result<(Vec<PathBuf>, bool), String> {
    if root.is_file() {
        return Ok((vec![root.to_path_buf()], true));
    }
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let mut children = fs::read_dir(dir)
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        children.sort_by_key(|entry| entry.file_name());
        for entry in children {
            if reserved(&entry.file_name().to_string_lossy()) {
                continue;
            }
            let kind = entry.file_type().map_err(|e| e.to_string())?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                if files.len() >= limit {
                    return Ok((files, false));
                }
                files.push(entry.path());
            }
        }
    }
    files.sort();
    Ok((files, true))
}

fn file_bytes(path: &Path, limit: usize) -> Result<Option<Vec<u8>>, Value> {
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(rejected("PATH_UNAVAILABLE", &err.to_string())),
    };
    if !meta.is_file() || meta.len() > limit as u64 {
        return Err(rejected(
            "FILE_LIMIT",
            "Expected a regular file within the workspace file size limit.",
        ));
    }
    let bytes = fs::read(path).map_err(|err| rejected("READ_ERROR", &err.to_string()))?;
    if bytes.len() > limit || std::str::from_utf8(&bytes).is_err() {
        return Err(rejected(
            "UNSUPPORTED_CONTENT",
            "Expected a UTF-8 file within the workspace file size limit.",
        ));
    }
    Ok(Some(bytes))
}

fn source(path: &str) -> Value {
    json!({"kind":"workspace","path":path})
}

fn file_snapshot(
    journal: &mut Journal,
    run: usize,
    path: &str,
    bytes: &[u8],
) -> Result<Artifact, String> {
    journal.artifact(run, source(path), bytes)
}

pub(super) fn workspace_revision(ctx: &WorkspaceContext, path: &str) -> Option<Value> {
    let (file, _) = match workspace_path(ctx, path, false) {
        Ok(target) => target,
        Err(error) if error["code"] == "FILE_NOT_FOUND" => return Some(Value::Null),
        Err(_) => return None,
    };
    file_bytes(&file, ctx.config().max_file_bytes)
        .ok()
        .map(|bytes| json!(bytes.as_deref().map(revision)))
}

fn line_offsets(text: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    offsets.extend(build_line_spans(text).into_iter().map(|line| line.raw_end));
    offsets
}

fn line_location(lines: &[usize], byte: usize) -> usize {
    lines
        .partition_point(|&start| start <= byte)
        .min(lines.len().saturating_sub(1))
        .max(1)
}

fn read_page(journal: &Journal, artifact: &Artifact, args: &ReadArgs) -> Reply {
    let raw = journal.get(&artifact.revision)?;
    let text = std::str::from_utf8(&raw).map_err(|e| e.to_string())?;
    let lines = line_offsets(text);
    let count = lines.len() - 1;
    let user = match (
        args.start_line,
        args.line_count,
        args.start_byte,
        args.end_byte,
    ) {
        (None, None, start, end) => Some((start.unwrap_or(0), end.unwrap_or(text.len()))),
        (start, page, None, None) => {
            let start = start.unwrap_or(1);
            let end = page.map_or(count, |n| {
                start.saturating_sub(1).saturating_add(n).min(count)
            });
            if start > 0 && end >= start {
                lines
                    .get(start - 1)
                    .zip(lines.get(end))
                    .map(|(&a, &b)| (a, b))
            } else {
                None
            }
        }
        _ => None,
    };
    let Some((start, end)) =
        user.filter(|&(a, b)| a <= b && text.is_char_boundary(a) && text.is_char_boundary(b))
    else {
        let mut error = rejected(
            "INVALID_RANGE",
            "Use one coordinate system and valid source boundaries.",
        );
        error["result_ref"] = json!(artifact.result_ref);
        error["revision"] = json!(artifact.revision);
        error["total_bytes"] = json!(text.len());
        error["total_lines"] = json!(count);
        return Ok(error);
    };
    let mut content = truncate_utf8_bytes(&text[start..end], 4096).0;
    loop {
        let stop = start + content.len();
        let mut result = json!({"status":"ok","delivery":if stop<end {"folded"} else {"inline"},
            "source":artifact.source,"result_ref":artifact.result_ref,"revision":artifact.revision,
            "total_bytes":text.len(),"total_lines":count,
            "range":{"byte_start":start,"byte_end":stop,
                     "start_line":line_location(&lines,start),"end_line":line_location(&lines,stop.saturating_sub(1).max(start))},
            "coverage":{"requested_bytes":end-start,"returned_bytes":content.len(),"complete":stop==end},
            "content":content});
        if stop < end {
            result["next"] =
                json!({"result_ref":artifact.result_ref,"start_byte":stop,"end_byte":end});
        }
        if json_byte_len(&result) <= MAX_RESPONSE_BYTES {
            return Ok(result);
        }
        if content.is_empty() {
            return Ok(rejected(
                "METADATA_LIMIT",
                "Source metadata exceeds the result budget.",
            ));
        }
        content = truncate_utf8_bytes(content, content.len() / 2).0;
    }
}

pub fn read(ctx: &WorkspaceContext, journal: &mut Journal, run: usize, args: &Value) -> Reply {
    let args: ReadArgs = match parse(args) {
        Ok(v) => v,
        Err(err) => return Ok(err),
    };
    let artifact = match (&args.path, &args.result_ref) {
        (Some(path), None) => {
            let (file, path) = match workspace_path(ctx, path, false) {
                Ok(p) => p,
                Err(e) => return Ok(e),
            };
            let bytes = match file_bytes(&file, ctx.config().max_file_bytes) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => return Ok(rejected("PATH_UNAVAILABLE", "File is missing.")),
                Err(e) => return Ok(e),
            };
            file_snapshot(journal, run, &path, &bytes)?
        }
        (path, Some(reference)) => match journal.resolve(reference) {
            Some(artifact)
                if path
                    .as_deref()
                    .is_none_or(|path| artifact.source["path"].as_str() == Some(path)) =>
            {
                artifact
            }
            Some(_) => {
                return Ok(rejected(
                    "SOURCE_MISMATCH",
                    "The supplied path does not identify the stored snapshot's source.",
                ))
            }
            None => {
                return Ok(rejected(
                    "RESULT_NOT_FOUND",
                    "No such snapshot in this session.",
                ))
            }
        },
        _ => {
            return Ok(rejected(
                "INVALID_ARGUMENT",
                "Supply either path or result_ref.",
            ))
        }
    };
    read_page(journal, &artifact, &args)
}

pub fn search_result(journal: &Journal, args: &Value) -> Reply {
    if args["query"]
        .as_str()
        .is_some_and(|query| query.contains('\n'))
        && args["match_mode"].as_str().unwrap_or("literal") == "literal"
    {
        return Ok(rejected(
            "INVALID_ARGUMENT",
            "search_result matches within lines. Use research on this result for a multiline literal.",
        ));
    }
    let mut metadata = json!({});
    let outcome = crate::search::execute_snapshot(&args.to_string(), |reference| {
        let Some(artifact) = journal.resolve(reference) else {
            return Ok(None);
        };
        let raw =
            String::from_utf8(journal.get(&artifact.revision)?).map_err(|err| err.to_string())?;
        metadata = json!({"source":artifact.source,"revision":artifact.revision,"result_ref":artifact.result_ref});
        Ok(Some(Snapshot::new(artifact.result_ref, raw)))
    })?;
    let mut envelope = match outcome {
        Ok(envelope) => envelope,
        Err(error) => return serde_json::to_value(error).map_err(|err| err.to_string()),
    };
    let available = MAX_RESPONSE_BYTES.saturating_sub(json_byte_len(&metadata) - 1);
    crate::search::enforce_response_budget(&mut envelope, available);
    let mut value = serde_json::to_value(envelope).map_err(|err| err.to_string())?;
    if json_byte_len(&value) <= available {
        value
            .as_object_mut()
            .ok_or("Expected search envelope object")?
            .append(
                metadata
                    .as_object_mut()
                    .ok_or("Expected search metadata object")?,
            );
    }
    Ok(value)
}

const RESEARCH_BATCH_SIZE: usize = 128;

#[derive(Default)]
struct ResearchHits {
    count: usize,
    pending: Vec<Value>,
    batches: Vec<String>,
}

impl ResearchHits {
    fn push(&mut self, journal: &Journal, hit: Value) -> Result<(), String> {
        self.count += 1;
        self.pending.push(hit);
        if self.pending.len() == RESEARCH_BATCH_SIZE {
            self.flush(journal)?;
        }
        Ok(())
    }

    fn flush(&mut self, journal: &Journal) -> Result<(), String> {
        if !self.pending.is_empty() {
            let bytes = serde_json::to_vec(&self.pending).map_err(|err| err.to_string())?;
            self.batches.push(journal.put(&bytes)?);
            self.pending.clear();
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct ResearchReport {
    coverage: Value,
    count: usize,
    batches: Vec<String>,
}

impl ResearchReport {
    fn batch(&self, journal: &Journal, index: usize) -> Result<Vec<Value>, String> {
        let hash = self
            .batches
            .get(index)
            .ok_or("Missing archived research batch")?;
        serde_json::from_slice(&journal.get(hash)?).map_err(|err| err.to_string())
    }

    fn page(
        &self,
        journal: &mut Journal,
        run: usize,
        artifact: &Artifact,
        offset: usize,
        limit: usize,
    ) -> Reply {
        if offset > self.count {
            return Ok(rejected(
                "INVALID_RANGE",
                "Use an offset from the saved research result.",
            ));
        }
        let end = offset.saturating_add(limit).min(self.count);
        let mut hits = Vec::new();
        let mut index = offset;
        while index < end {
            let batch = self.batch(journal, index / RESEARCH_BATCH_SIZE)?;
            let skip = index % RESEARCH_BATCH_SIZE;
            let available = batch.len().saturating_sub(skip);
            let take = available.min(end - index);
            if take == 0 {
                return Err("Incomplete archived research batch".to_owned());
            }
            hits.extend(batch.into_iter().skip(skip).take(take));
            index += take;
        }
        loop {
            let shown = hits.len();
            let next_offset = offset + shown;
            let mut value = json!({"status":"ok","source":{"kind":"research","scope":artifact.source["scope"]},
                "revision":artifact.revision,"result_ref":artifact.result_ref,
                "coverage":self.coverage,"presentation":{"matching_items":self.count,"retained":self.count,
                    "offset":offset,"shown":shown,"complete":offset==0 && shown==self.count},"hits":hits});
            if next_offset < self.count {
                value["next"] = json!({"scope":"page","result_ref":artifact.result_ref,
                    "offset":next_offset,"limit":limit});
            }
            if json_byte_len(&value) <= MAX_RESPONSE_BYTES {
                return Ok(value);
            }
            if hits.len() == 1 {
                let payload = journal.artifact(
                    run,
                    json!({"kind":"research_hit","report":artifact.result_ref,"index":offset}),
                    &serde_json::to_vec_pretty(&hits[0]).map_err(|err| err.to_string())?,
                )?;
                hits[0] = json!({"delivery":"folded","result_ref":payload.result_ref});
            } else {
                hits.pop();
            }
        }
    }
}

fn research_hit(
    artifact: &Artifact,
    start: usize,
    end: usize,
    range: (usize, usize),
    text: &str,
) -> Value {
    let (excerpt, truncated) = truncate_utf8_bytes(text, 320);
    json!({"source":artifact.source,"revision":artifact.revision,"result_ref":artifact.result_ref,
        "range":{"start_line":start,"end_line":end,"byte_start":range.0,"byte_end":range.1},
        "text":excerpt,"text_complete":!truncated})
}

fn match_lines(
    journal: &Journal,
    text: &str,
    query: &str,
    artifact: &Artifact,
    hits: &mut ResearchHits,
) -> Result<(), String> {
    if query.contains('\n') {
        let lines = line_offsets(text);
        for (start, exact) in text.match_indices(query) {
            let stop = start + exact.len();
            hits.push(
                journal,
                research_hit(
                    artifact,
                    line_location(&lines, start),
                    line_location(&lines, stop - 1),
                    (start, stop),
                    exact,
                ),
            )?;
        }
        return Ok(());
    }

    let mut offset = 0;
    for (idx, line) in text.split_inclusive('\n').enumerate() {
        let end = offset + line.len();
        if line.contains(query) {
            hits.push(
                journal,
                research_hit(artifact, idx + 1, idx + 1, (offset, end), line),
            )?;
        }
        offset = end;
    }
    Ok(())
}

pub fn research(ctx: &WorkspaceContext, journal: &mut Journal, run: usize, args: &Value) -> Reply {
    let mut args: ResearchArgs = match parse(args) {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    args.path = args.path.filter(|value| !value.is_empty());
    args.result_ref = args.result_ref.filter(|value| !value.is_empty());
    args.query = args.query.filter(|value| !value.is_empty());
    let query = args.query.as_deref().unwrap_or("");
    let limit = args.limit.unwrap_or(8);
    if !(1..=20).contains(&limit) || query.len() > 512 {
        return Ok(rejected(
            "INVALID_ARGUMENT",
            "Use a query of at most 512 bytes and limit 1 to 20.",
        ));
    }
    if args.scope == "page" {
        if args.path.is_some() || args.query.is_some() || args.offset.is_none() {
            return Ok(rejected(
                "INVALID_ARGUMENT",
                "Use the next arguments from a research result.",
            ));
        }
        let offset = args.offset.unwrap_or_default();
        let Some(artifact) = args
            .result_ref
            .as_deref()
            .and_then(|reference| journal.resolve(reference))
        else {
            return Ok(rejected(
                "RESULT_NOT_FOUND",
                "Use the result_ref from a research result.",
            ));
        };
        let Some(report_hash) = artifact.source["research_report"].as_str() else {
            return Ok(rejected(
                "INVALID_ARGUMENT",
                "This reference has no research continuation.",
            ));
        };
        let report: ResearchReport =
            serde_json::from_slice(&journal.get(report_hash)?).map_err(|err| err.to_string())?;
        return report.page(journal, run, &artifact, offset, limit);
    }
    if args.offset.unwrap_or_default() != 0 {
        return Ok(rejected(
            "INVALID_ARGUMENT",
            "Use offset only with scope=page.",
        ));
    }

    let mut hits = ResearchHits::default();
    let mut inspected = 0;
    let mut skipped = 0;
    let mut scanned_all = true;
    match args.scope.as_str() {
        "workspace" if args.result_ref.is_none() => {
            let (root, _) = match workspace_path(ctx, args.path.as_deref().unwrap_or("."), false) {
                Ok(v) => v,
                Err(e) => return Ok(e),
            };
            let (files, complete) = match walk(&root, ctx.config().max_list_entries) {
                Ok(result) => result,
                Err(err) => return Ok(rejected("SCAN_ERROR", &err)),
            };
            scanned_all = complete;
            let mut bytes_seen = 0;
            for path in files {
                let Ok(path) = path.canonicalize() else {
                    skipped += 1;
                    continue;
                };
                if !path.starts_with(ctx.root()) {
                    skipped += 1;
                    continue;
                }
                let display = ctx.relative_display(&path);
                if query.is_empty() {
                    inspected += 1;
                    hits.push(journal, json!({"source":source(&display)}))?;
                    continue;
                }
                let Ok(Some(bytes)) = file_bytes(&path, ctx.config().max_grep_file_bytes) else {
                    skipped += 1;
                    continue;
                };
                bytes_seen += bytes.len();
                if bytes_seen > 32 * 1024 * 1024 {
                    scanned_all = false;
                    break;
                }
                inspected += 1;
                let text = std::str::from_utf8(&bytes).map_err(|e| e.to_string())?;
                if text.contains(query) {
                    let artifact = file_snapshot(journal, run, &display, &bytes)?;
                    match_lines(journal, text, query, &artifact, &mut hits)?;
                } else if display.contains(query) {
                    hits.push(journal, json!({"source":source(&display)}))?;
                }
            }
        }
        "result" if args.path.is_none() && !query.is_empty() => {
            let Some(artifact) = args.result_ref.as_deref().and_then(|r| journal.resolve(r)) else {
                return Ok(rejected(
                    "RESULT_NOT_FOUND",
                    "Supply a result_ref from this session.",
                ));
            };
            let raw = journal.get(&artifact.revision)?;
            match_lines(
                journal,
                std::str::from_utf8(&raw).map_err(|e| e.to_string())?,
                query,
                &artifact,
                &mut hits,
            )?;
            inspected = 1;
        }
        "history" if args.path.is_none() && args.result_ref.is_none() => {
            let records: Vec<_> = journal
                .events
                .iter()
                .rev()
                .filter(|e| {
                    matches!(
                        e.kind.as_str(),
                        "artifact"
                            | "tool_result"
                            | "tool_attempt"
                            | "validation_receipt"
                            | "validation_intent"
                            | "edit_intent"
                            | "user"
                    ) && !(e.kind == "artifact"
                        && matches!(
                            e.data["source"]["kind"].as_str(),
                            Some(
                                "history"
                                    | "history_arguments"
                                    | "history_result"
                                    | "research"
                                    | "research_hit"
                            )
                        ))
                })
                .cloned()
                .collect();
            for event in records {
                inspected += 1;
                let mut record =
                    json!({"seq":event.seq,"run":event.run,"kind":event.kind,"data":event.data});
                let mut found = record.to_string().contains(query);
                if !found && event.kind == "tool_result" {
                    found = journal
                        .load_json(&event.data["output"])?
                        .to_string()
                        .contains(query);
                }
                if !found && event.kind == "tool_attempt" {
                    let hash = event.data["arguments"]
                        .as_str()
                        .ok_or("Missing recorded arguments")?;
                    found = String::from_utf8_lossy(&journal.get(hash)?).contains(query);
                }
                if found {
                    let source = json!({"kind":"history","event":event.seq});
                    let attempt = match event.kind.as_str() {
                        "tool_attempt" => Some(&event),
                        "tool_result" => event.data["attempt"]
                            .as_u64()
                            .and_then(|seq| usize::try_from(seq).ok())
                            .and_then(|seq| seq.checked_sub(1))
                            .and_then(|index| journal.events.get(index))
                            .filter(|entry| entry.kind == "tool_attempt"),
                        _ => None,
                    };
                    if let Some(attempt) = attempt {
                        let arguments = journal.get(
                            attempt.data["arguments"]
                                .as_str()
                                .ok_or("Missing recorded arguments")?,
                        )?;
                        let arguments_source =
                            json!({"kind":"history_arguments","event":attempt.seq});
                        let artifact = journal.artifact(run, arguments_source, &arguments)?;
                        record["arguments_ref"] = json!(artifact.result_ref);
                    }
                    if event.kind == "tool_result" {
                        let output = journal.load_json(&event.data["output"])?;
                        let payload = journal.artifact(
                            run,
                            json!({"kind":"history_result","event":event.seq}),
                            &serde_json::to_vec_pretty(&output).map_err(|err| err.to_string())?,
                        )?;
                        record["output_ref"] = json!(payload.result_ref);
                    }
                    if event.kind == "validation_receipt" {
                        record["output_ref"] = event.data["result_ref"].clone();
                    }
                    let record_artifact = journal.artifact(
                        run,
                        source.clone(),
                        &serde_json::to_vec_pretty(&record).map_err(|err| err.to_string())?,
                    )?;
                    record["source"] = source;
                    record["result_ref"] = json!(record_artifact.result_ref);
                    hits.push(journal, record)?;
                }
            }
        }
        _ => {
            return Ok(rejected(
                "INVALID_ARGUMENT",
                "Choose workspace, result with a query and result_ref, history, or a saved page.",
            ))
        }
    }
    hits.flush(journal)?;
    let report = ResearchReport {
        coverage: json!({"items_scanned":inspected,"skipped_items":skipped,"scan_complete":scanned_all&&skipped==0}),
        count: hits.count,
        batches: hits.batches,
    };
    let preview = if report.count == 0 {
        Vec::new()
    } else {
        report.batch(journal, 0)?
    };
    let hash = journal.put(&serde_json::to_vec(&report).map_err(|err| err.to_string())?)?;
    let artifact = journal.artifact(
        run,
        json!({"kind":"research","scope":args.scope,"research_report":hash}),
        &serde_json::to_vec_pretty(&preview).map_err(|err| err.to_string())?,
    )?;
    report.page(journal, run, &artifact, 0, limit)
}

fn conflict(
    journal: &mut Journal,
    run: usize,
    path: &str,
    before: &Option<String>,
    actual: Option<&[u8]>,
) -> Reply {
    let mut error = rejected(
        "REVISION_CONFLICT",
        "Review the current content before changing this file.",
    );
    error["path"] = json!(path);
    error["expected_revision"] = json!(before);
    if let Some(bytes) = actual {
        let artifact = file_snapshot(journal, run, path, bytes)?;
        let content = std::str::from_utf8(bytes).map_err(|err| err.to_string())?;
        error["source"] = json!(artifact.source);
        error["revision"] = json!(artifact.revision);
        error["current_revision"] = json!(artifact.revision);
        error["result_ref"] = json!(artifact.result_ref);
        error["preview"] = json!(truncate_utf8_bytes(content, 512).0);
    } else {
        error["current_revision"] = Value::Null;
    }
    Ok(error)
}

fn transform(old: &str, mut changes: Vec<(usize, usize, String)>) -> Result<String, Value> {
    if changes.is_empty() {
        return Err(rejected("EMPTY_EDIT", "Supply at least one local edit."));
    }
    changes.sort_by_key(|edit| (edit.0, edit.1));
    let mut cursor = 0;
    let mut candidate = String::new();
    for (start, end, new_text) in changes {
        if start < cursor {
            return Err(rejected(
                "OVERLAPPING_EDITS",
                "Changes overlap in the original revision.",
            ));
        }
        candidate.push_str(&old[cursor..start]);
        candidate.push_str(&new_text);
        cursor = end;
    }
    candidate.push_str(&old[cursor..]);
    Ok(candidate)
}

fn edit_text(old: &str, changes: Vec<TextChange>) -> Result<String, Value> {
    let mut located = Vec::new();
    for change in changes {
        if change.old_text.is_empty() {
            return Err(rejected(
                "EMPTY_MATCH",
                "old_text must identify existing source text.",
            ));
        }
        let Some(start) = old.find(&change.old_text) else {
            return Err(rejected(
                "TEXT_NOT_FOUND",
                "old_text is absent. Read the exact source range.",
            ));
        };
        let first_char = change
            .old_text
            .chars()
            .next()
            .expect("nonempty match")
            .len_utf8();
        let next = start + first_char;
        if let Some(other) = old[next..]
            .find(&change.old_text)
            .map(|offset| next + offset)
        {
            let offsets = line_offsets(old);
            let mut error = rejected(
                "AMBIGUOUS_MATCH",
                "old_text matches more than once. Include unique context.",
            );
            error["match_lines"] = json!([
                line_location(&offsets, start),
                line_location(&offsets, other)
            ]);
            error["match_byte_starts"] = json!([start, other]);
            return Err(error);
        }
        located.push((start, start + change.old_text.len(), change.new_text));
    }
    transform(old, located)
}

fn edit_range(old: &str, changes: Vec<RangeChange>) -> Result<String, Value> {
    let lines = line_offsets(old);
    let mut located = Vec::new();
    for change in changes {
        if change.start_line == 0
            || change.end_line < change.start_line
            || change.end_line >= lines.len()
        {
            return Err(rejected(
                "INVALID_RANGE",
                "Use nonempty inclusive line ranges in the specified revision.",
            ));
        }
        located.push((
            lines[change.start_line - 1],
            lines[change.end_line],
            change.new_text,
        ));
    }
    transform(old, located)
}

pub fn modify(
    ctx: &WorkspaceContext,
    journal: &mut Journal,
    run: usize,
    operation: &str,
    tool: &str,
    mode: EditMode,
    input: &Value,
) -> Reply {
    if let Some(receipt) = journal
        .events
        .iter()
        .find(|event| event.kind == "edit_receipt" && event.data["operation"] == operation)
    {
        return Ok(receipt.data.clone());
    }
    if let Some(saved) = journal
        .events
        .iter()
        .find(|event| event.kind == "edit_intent" && event.data["operation"] == operation)
    {
        let intent = serde_json::from_value(saved.data.clone()).map_err(|err| err.to_string())?;
        return commit(ctx, journal, run, &intent, true);
    }

    let (path, before, edit, content) = if tool == "write" {
        let args: WriteArgs = match parse(input) {
            Ok(v) => v,
            Err(e) => return Ok(e),
        };
        // Some providers supply an empty string for an omitted optional argument.
        let expected = args.expected_revision.filter(|value| !value.is_empty());
        match (args.mode.as_str(), expected) {
            ("create", None) => (args.path, None, None, Some(args.content)),
            ("replace", Some(rev)) => (args.path, Some(rev), None, Some(args.content)),
            ("create", Some(_)) => {
                let mut error = rejected(
                    "INVALID_ARGUMENT",
                    "Remove the expected_revision parameter from this call. For mode=create, send path, content and mode only. The tool checks that the file is absent.",
                );
                error["omit"] = json!(["expected_revision"]);
                return Ok(error);
            }
            ("replace", None) => {
                return Ok(rejected(
                    "INVALID_ARGUMENT",
                    "For mode=replace, set expected_revision to the current revision of this path from read or research.",
                ));
            }
            _ => {
                return Ok(rejected(
                    "INVALID_ARGUMENT",
                    "Set mode to create for an absent path or replace for an existing file.",
                ))
            }
        }
    } else {
        let (path, rev, edit) = match mode {
            EditMode::Text => {
                let args: EditArgs<TextChange> = match parse(input) {
                    Ok(v) => v,
                    Err(e) => return Ok(e),
                };
                (args.path, args.expected_revision, Edit::Text(args.changes))
            }
            EditMode::Range => {
                let args: EditArgs<RangeChange> = match parse(input) {
                    Ok(v) => v,
                    Err(e) => return Ok(e),
                };
                (args.path, args.expected_revision, Edit::Range(args.changes))
            }
            EditMode::Patch => {
                let args: PatchArgs = match parse(input) {
                    Ok(v) => v,
                    Err(e) => return Ok(e),
                };
                (args.path, args.expected_revision, Edit::Patch(args.patch))
            }
        };
        (path, Some(rev), Some(edit), None)
    };
    let (target, path) = match workspace_path(ctx, &path, true) {
        Ok(p) => p,
        Err(e) => return Ok(e),
    };
    let origin = match file_bytes(&target, ctx.config().max_file_bytes) {
        Ok(b) => b,
        Err(e) => return Ok(e),
    };
    let actual = origin.as_deref().map(revision);
    if actual != before {
        return conflict(journal, run, &path, &before, origin.as_deref());
    }
    let old = std::str::from_utf8(origin.as_deref().unwrap_or_default())
        .map_err(|err| err.to_string())?;
    let next = match edit {
        Some(Edit::Text(changes)) => edit_text(old, changes),
        Some(Edit::Range(changes)) => edit_range(old, changes),
        Some(Edit::Patch(patch)) => diffy::Patch::from_str(&patch)
            .map_err(|err| rejected("INVALID_PATCH", &err.to_string()))
            .and_then(|patch| {
                diffy::apply(old, &patch)
                    .map_err(|err| rejected("PATCH_MISMATCH", &err.to_string()))
            }),
        None => Ok(content.ok_or("Missing write content")?),
    };
    let next = match next {
        Ok(next) => next,
        Err(mut error) => {
            let artifact = file_snapshot(journal, run, &path, old.as_bytes())?;
            error["result_ref"] = json!(artifact.result_ref);
            error["current_revision"] = json!(artifact.revision);
            return Ok(error);
        }
    };
    if next.len() > ctx.config().max_file_bytes {
        return Ok(rejected(
            "FILE_LIMIT",
            "Candidate content exceeds the workspace file size limit.",
        ));
    }
    if let Some(bytes) = &origin {
        journal.put(bytes)?;
    }
    let after = journal.put(next.as_bytes())?;
    let intent = Intent {
        operation: operation.to_owned(),
        path,
        before,
        after,
    };
    journal.append(run, "edit_intent", json!(intent))?;
    commit(ctx, journal, run, &intent, false)
}

enum Edit {
    Text(Vec<TextChange>),
    Range(Vec<RangeChange>),
    Patch(String),
}

fn commit(
    ctx: &WorkspaceContext,
    journal: &mut Journal,
    run: usize,
    intent: &Intent,
    recovering: bool,
) -> Reply {
    let (target, path) = match workspace_path(ctx, &intent.path, true) {
        Ok(p) => p,
        Err(e) => return Ok(e),
    };
    if path != intent.path {
        return Ok(rejected(
            "PATH_CHANGED",
            "The recorded path resolved to another file.",
        ));
    }
    let before = match file_bytes(&target, ctx.config().max_file_bytes) {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let current = before.as_deref().map(revision);
    let next = journal.get(&intent.after)?;
    let disposition = if current.as_deref() == Some(&intent.after) {
        if recovering {
            "observed_target_on_recovery"
        } else {
            "already_current"
        }
    } else if current != intent.before {
        let mut result = conflict(journal, run, &path, &intent.before, before.as_deref())?;
        if recovering {
            result["planned_revision"] = json!(intent.after);
            if let Some(actual) = before.as_deref() {
                let planned = std::str::from_utf8(&next).map_err(|err| err.to_string())?;
                let observed = std::str::from_utf8(actual).map_err(|err| err.to_string())?;
                let delta = diffy::create_patch(planned, observed).to_string();
                let diff = journal.artifact(
                    run,
                    json!({"kind":"diff","path":path,"before":intent.after,
                    "after":current,"comparison":"planned_to_current"}),
                    delta.as_bytes(),
                )?;
                let (preview, clipped) = truncate_utf8_bytes(&delta, 800);
                result["planned_to_current"] =
                    json!({"result_ref":diff.result_ref,"diff":preview,"complete":!clipped});
            }
        }
        return Ok(result);
    } else {
        let parent = target.parent().ok_or("Missing target parent")?;
        let mut temp = tempfile::Builder::new()
            .prefix(".sourceharness-write-")
            .tempfile_in(parent)
            .map_err(|e| e.to_string())?;
        temp.write_all(&next).map_err(|e| e.to_string())?;
        if before.is_some() {
            let permissions = fs::metadata(&target)
                .map_err(|e| e.to_string())?
                .permissions();
            temp.as_file()
                .set_permissions(permissions)
                .map_err(|e| e.to_string())?;
        }
        temp.as_file().sync_all().map_err(|e| e.to_string())?;
        let latest = match file_bytes(&target, ctx.config().max_file_bytes) {
            Ok(v) => v,
            Err(e) => return Ok(e),
        };
        if latest.as_deref().map(revision) != intent.before {
            return conflict(journal, run, &path, &intent.before, latest.as_deref());
        }
        let result = if before.is_some() {
            temp.persist(&target)
        } else {
            temp.persist_noclobber(&target)
        };
        if let Err(err) = result {
            if err.error.kind() == io::ErrorKind::AlreadyExists {
                let now = match file_bytes(&target, ctx.config().max_file_bytes) {
                    Ok(v) => v,
                    Err(e) => return Ok(e),
                };
                return conflict(journal, run, &path, &intent.before, now.as_deref());
            }
            return Ok(rejected("WRITE_ERROR", &err.error.to_string()));
        }
        if std::env::var("SOURCE_HARNESS_TEST_INTERRUPT").as_deref() == Ok("after_file_replace") {
            std::process::exit(86);
        }
        let observed = match file_bytes(&target, ctx.config().max_file_bytes) {
            Ok(v) => v,
            Err(e) => return Ok(e),
        };
        if observed.as_deref().map(revision).as_deref() != Some(&intent.after) {
            return conflict(
                journal,
                run,
                &path,
                &Some(intent.after.clone()),
                observed.as_deref(),
            );
        }
        "committed"
    };
    let original = match &intent.before {
        Some(hash) => journal.get(hash)?,
        None => Vec::new(),
    };
    let delta = diffy::create_patch(
        std::str::from_utf8(&original).map_err(|e| e.to_string())?,
        std::str::from_utf8(&next).map_err(|e| e.to_string())?,
    )
    .to_string();
    let artifact = file_snapshot(journal, run, &path, &next)?;
    let diff = journal.artifact(
        run,
        json!({"kind":"diff","path":path,"before":intent.before,"after":intent.after}),
        delta.as_bytes(),
    )?;
    let receipt = json!({"status":"ok","source":artifact.source,"operation":intent.operation,"before_revision":intent.before,
        "revision":artifact.revision,"result_ref":artifact.result_ref,"outcome":disposition,"diff_ref":diff.result_ref,
        "diff":truncate_utf8_bytes(&delta,1000).0});
    journal.append(run, "edit_receipt", receipt.clone())?;
    Ok(receipt)
}
