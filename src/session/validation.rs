use super::journal::{revision, Journal};
use super::tools::walk;
use crate::budget::{json_byte_len, truncate_utf8_bytes, MAX_RESPONSE_BYTES};
use crate::workspace::WorkspaceContext;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

type InputRevisions = BTreeMap<PathBuf, String>;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Commands(pub BTreeMap<String, Validation>);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Validation {
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub artifacts: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<PathBuf>,
    pub timeout_seconds: Option<u64>,
}

impl Validation {
    fn input_revisions(&self) -> Result<Option<InputRevisions>, String> {
        if self.inputs.is_empty() { return Ok(None); }
        self.inputs.iter().map(|path| {
            if !path.is_absolute() || !path.is_file() {
                return Err(format!("Validation input must be a readable absolute file: {}", path.display()));
            }
            let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
            Ok((path.clone(), revision(&bytes)))
        }).collect::<Result<InputRevisions, String>>().map(Some)
    }
}

impl Commands {
    pub fn from_compact_file(path: &Path, workspace: &Path) -> Result<Self, String> {
        let path = path.canonicalize().map_err(|e| e.to_string())?;
        let workspace = workspace.canonicalize().map_err(|e| e.to_string())?;
        if path.starts_with(workspace) { return Err("Place the checks file outside the workspace.".to_owned()); }
        Self::from_file(&path)
    }

    pub fn from_file(path: &Path) -> Result<Self, String> {
        let commands: Self = serde_json::from_str(&fs::read_to_string(path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        for (name, cmd) in &commands.0 {
            if !cmd.program.is_absolute() || !cmd.program.is_file() {
                return Err(format!("{name} requires an absolute path to a validation executable"));
            }
            if cmd.artifacts.iter().any(|p| p.as_os_str().is_empty() || p.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))) {
                return Err(format!("{name} artifact paths must stay inside the workspace"));
            }
            if cmd.inputs.iter().any(|p| !p.is_absolute()) { return Err(format!("{name} inputs must be absolute file paths")); }
            if !matches!(cmd.timeout_seconds.unwrap_or(120), 1..=900) { return Err(format!("{name} timeout_seconds must be in 1..=900")); }
        }
        Ok(commands)
    }

    pub(super) fn check_exec_boundary(&self, workspace: &Path) -> Result<(), String> {
        for (check, cmd) in &self.0 {
            if cmd.program.canonicalize().map_err(|e| e.to_string())?.starts_with(workspace) {
                return Err(format!("Validation program for {check} must be outside the workspace."));
            }
            let arg_files = cmd.args.iter().map(Path::new).filter(|p| p.is_absolute() && p.is_file());
            for input in cmd.inputs.iter().map(PathBuf::as_path).chain(arg_files) {
                if input.canonicalize().map_err(|e| e.to_string())?.starts_with(workspace) {
                    return Err(format!("Validation input for {check} must be outside the workspace."));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn program_revision(&self, check: &str) -> Option<String> {
        fs::read(&self.0.get(check)?.program).ok().map(|b| revision(&b))
    }

    pub(crate) fn program_revisions(&self) -> BTreeMap<&str, Option<String>> {
        self.0.keys().map(|k| (k.as_str(), self.program_revision(k))).collect()
    }

    pub(crate) fn input_revision_profile(&self) -> BTreeMap<&str, Option<InputRevisions>> {
        self.0.iter().filter(|(_, c)| !c.inputs.is_empty()).map(|(k, c)| (k.as_str(), c.input_revisions().ok().flatten())).collect()
    }
}

fn manifest(ctx: &WorkspaceContext, journal: &Journal, copy: Option<&Path>, retain: bool) -> Result<String, String> {
    let (files, all) = walk(ctx.root(), ctx.config().max_list_entries)?;
    if !all { return Err("Workspace exceeds validation manifest file limit".to_owned()); }
    let mut entries = BTreeMap::new();
    for path in files {
        let rel = path.strip_prefix(ctx.root()).map_err(|e| e.to_string())?;
        if path.canonicalize().map_err(|e| e.to_string())? != path || !super::tools::permitted(rel) {
            return Err("Workspace snapshot path changed or is unavailable".to_owned());
        }
        let meta = fs::metadata(&path).map_err(|e| e.to_string())?;
        if !meta.is_file() { return Err("Workspace snapshot requires regular files".to_owned()); }
        let name = rel.to_str().ok_or("Workspace snapshot requires UTF-8 paths")?.replace(std::path::MAIN_SEPARATOR, "/");
        let hash = if let Some(copy) = copy {
            let bytes = fs::read(&path).map_err(|e| e.to_string())?;
            let dest = copy.join(&name);
            fs::create_dir_all(dest.parent().ok_or("Missing snapshot parent")?).map_err(|e| e.to_string())?;
            fs::write(&dest, &bytes).map_err(|e| e.to_string())?;
            fs::set_permissions(dest, meta.permissions()).map_err(|e| e.to_string())?;
            if retain { journal.put(&bytes)? } else { revision(&bytes) }
        } else {
            journal.hash_cached_file(&path, &meta, retain)?
        };
        entries.insert(name, hash);
    }
    journal.store_json(&json!(entries))
}

pub fn workspace_version(ctx: &WorkspaceContext, journal: &Journal) -> Result<String, String> { manifest(ctx, journal, None, false) }
pub(super) fn save_workspace(ctx: &WorkspaceContext, journal: &Journal) -> Result<String, String> { manifest(ctx, journal, None, true) }
pub(super) fn copy_workspace(ctx: &WorkspaceContext, journal: &Journal, path: &Path) -> Result<String, String> { manifest(ctx, journal, Some(path), true) }

fn changed_since_checkpoint(journal: &Journal, run: usize, tested_revision: &str) -> Option<Vec<String>> {
    let baseline = journal.events.iter().rev().filter(|e| e.run == run).find_map(|e| match e.kind.as_str() {
        "validation_receipt" if e.data["passed"] == true && e.data["workspace_changed_during_check"] == false => {
            e.data["workspace_revision"].as_str()
        }
        "user" => e.data["workspace_revision_at_start"].as_str(),
        _ => None,
    })?;
    if baseline == tested_revision { return Some(Vec::new()); }
    let before: BTreeMap<String, String> = serde_json::from_slice(&journal.get(baseline).ok()?).ok()?;
    let tested: BTreeMap<String, String> = serde_json::from_slice(&journal.get(tested_revision).ok()?).ok()?;
    let mut changed: Vec<String> = before.keys().chain(tested.keys()).filter(|p| before.get(*p) != tested.get(*p)).cloned().collect();
    changed.sort();
    changed.dedup();
    if changed.len() > 12 { changed.truncate(12); changed.push("...".to_owned()); }
    Some(changed)
}

fn changes_since_start(journal: &Journal, run: usize, tested_revision: &str) -> Option<Value> {
    let baseline = journal.events.iter().find(|e| e.run == run && e.kind == "user")?.data["workspace_revision_at_start"].as_str()?;
    let (mut files, mut total, mut bytes) = (Vec::new(), 0, 0);
    if baseline != tested_revision {
        let before: BTreeMap<String, String> = serde_json::from_slice(&journal.get(baseline).ok()?).ok()?;
        let tested: BTreeMap<String, String> = serde_json::from_slice(&journal.get(tested_revision).ok()?).ok()?;
        for path in before.keys().chain(tested.keys().filter(|p| !before.contains_key(*p))) {
            let change = match (before.get(path), tested.get(path)) {
                (None, Some(_)) => "added",
                (Some(_), None) => "removed",
                (Some(old), Some(new)) if old != new => "modified",
                _ => continue,
            };
            total += 1;
            let file = json!({"path": path, "change": change});
            let size = json_byte_len(&file) + 1;
            if bytes + size <= 1024 { bytes += size; files.push(file); }
        }
    }
    Some(json!({"from_revision": baseline, "files": files, "total": total, "complete": files.len() == total}))
}

pub fn run(ctx: &WorkspaceContext, journal: &mut Journal, run: usize, operation: &str, commands: &Commands, args: &Value) -> Result<Value, String> {
    let Some(check) = args.get("check").and_then(Value::as_str).filter(|_| args.as_object().is_some_and(|o| o.len() == 1)) else {
        return Ok(json!({"status": "error", "code": "INVALID_ARGUMENT", "reason": "Supply one configured check name."}));
    };
    let Some(config) = commands.0.get(check) else {
        return Ok(json!({"status": "error", "code": "UNKNOWN_CHECK", "reason": "Use a configured validation check."}));
    };
    if let Some(receipt) = journal.events.iter().find(|e| e.kind == "validation_receipt" && e.data["operation"] == operation) {
        return Ok(receipt.data.clone());
    }
    if journal.events.iter().any(|e| e.kind == "validation_intent" && e.data["operation"] == operation) {
        return Ok(json!({
            "status": "error", "code": "VALIDATION_INTERRUPTED", "operation": operation,
            "reason": "No completed receipt exists. Start a new validation if a fresh result is needed."
        }));
    }

    let snapshot = tempfile::Builder::new().prefix("check-").tempdir_in(ctx.root().join(".sourceharness")).map_err(|e| e.to_string())?;
    let before = copy_workspace(ctx, journal, snapshot.path())?;
    let program_revision = revision(&fs::read(&config.program).map_err(|e| e.to_string())?);
    let input_revisions = match config.input_revisions() {
        Ok(rev) => rev,
        Err(reason) => return Ok(json!({"status": "error", "code": "VALIDATION_INPUT_UNAVAILABLE", "check": check, "reason": reason})),
    };
    let mut intent = json!({"check": check, "workspace_revision": before, "program_revision": program_revision, "config": config, "operation": operation});
    if let Some(rev) = &input_revisions { intent["input_revisions"] = json!(rev); }
    journal.append(run, "validation_intent", intent)?;

    let (mut stdout, mut stderr) = (tempfile::tempfile().map_err(|e| e.to_string())?, tempfile::tempfile().map_err(|e| e.to_string())?);
    let mut cmd = Command::new(&config.program);
    cmd.args(&config.args).current_dir(snapshot.path()).stdin(Stdio::null()).env_remove("OPENAI_API_KEY")
        .stdout(stdout.try_clone().map_err(|e| e.to_string())?).stderr(stderr.try_clone().map_err(|e| e.to_string())?);
    super::exec::configure_process_group(&mut cmd);
    let launched = cmd.spawn();

    let (status, timed_out, launch_error) = match launched {
        Ok(mut child) => {
            let deadline = Instant::now() + Duration::from_secs(config.timeout_seconds.unwrap_or(120));
            loop {
                if let Some(exit) = child.try_wait().map_err(|e| e.to_string())? { break (exit.code(), false, None); }
                if Instant::now() >= deadline {
                    super::exec::kill_process_tree(&mut child);
                    child.wait().map_err(|e| e.to_string())?;
                    break (None, true, None);
                }
                std::thread::sleep(Duration::from_millis(40));
            }
        }
        Err(err) => (None, false, Some(err.to_string())),
    };

    let input_revisions_after = config.input_revisions();
    let input_revisions_stable = matches!(&input_revisions_after, Ok(after) if *after == input_revisions);

    let (mut bytes, mut streams, mut output_complete) = (Vec::new(), BTreeMap::new(), true);
    for (name, file) in [("stdout", &mut stdout), ("stderr", &mut stderr)] {
        file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        let start = bytes.len();
        file.take(8 * 1024 * 1024 + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
        if bytes.len() > start + 8 * 1024 * 1024 {
            bytes.truncate(start + 8 * 1024 * 1024);
            output_complete = false;
        }
        streams.insert(name, journal.put(&bytes[start..])?);
    }

    let output = String::from_utf8_lossy(&bytes);
    let report = journal.artifact(run, json!({"kind": "validation", "check": check, "workspace_revision": before, "operation": operation}), output.as_bytes())?;
    let after = workspace_version(ctx, journal)?;
    let mut artifacts = BTreeMap::new();
    for rel in &config.artifacts {
        if let Ok(path) = snapshot.path().join(rel).canonicalize() {
            if path.starts_with(snapshot.path()) && path.is_file() {
                artifacts.insert(rel, journal.put(&fs::read(path).map_err(|e| e.to_string())?)?);
            }
        }
    }

    let (preview, preview_clipped) = truncate_utf8_bytes(&output, 2048);
    let mut result = json!({
        "status": "ok", "check": check, "operation": operation, "passed": status == Some(0) && input_revisions_stable,
        "exit_code": status, "timed_out": timed_out, "launch_error": launch_error,
        "workspace_revision": before, "workspace_changed_during_check": before != after, "program_revision": program_revision,
        "artifacts": artifacts, "result_ref": report.result_ref, "output_complete": output_complete,
        "output_truncated": preview_clipped || !output_complete,
        "streams": streams, "preview": preview
    });
    if let Some(revisions) = input_revisions {
        result["input_revisions"] = json!(revisions);
        result["input_revisions_stable"] = json!(input_revisions_stable);
        if !input_revisions_stable {
            result["reason"] = json!("A declared validation input changed or was unavailable after execution. Run the check again against the current inputs.");
            match input_revisions_after {
                Ok(after) => result["input_revisions_after"] = json!(after),
                Err(error) => result["input_revisions_after_error"] = json!(error),
            }
        }
    }
    if result["passed"] == true {
        if before == after {
            if let Some(changes) = changes_since_start(journal, run, &before) {
                let field = "workspace_changes_since_start";
                if json_byte_len(&result) + json_byte_len(&changes) + field.len() + 4 <= MAX_RESPONSE_BYTES { result[field] = changes; }
            }
        }
    } else if let Some(changed) = changed_since_checkpoint(journal, run, &before).filter(|c| !c.is_empty()) {
        result["changed_since_checkpoint"] = json!(changed);
    }
    journal.append(run, "validation_receipt", result.clone())?;
    Ok(result)
}

#[cfg(test)]
#[path = "validation_input_tests.rs"]
mod input_tests;
