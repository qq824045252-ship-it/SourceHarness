use super::journal::{revision, Journal};
use super::tools::{definition, walk};
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
        if self.inputs.is_empty() {
            return Ok(None);
        }
        self.inputs
            .iter()
            .map(|path| {
                if !path.is_absolute() || !path.is_file() {
                    return Err(format!(
                        "Validation input must be a readable absolute file: {}",
                        path.display()
                    ));
                }
                let bytes = fs::read(path).map_err(|err| format!("{}: {err}", path.display()))?;
                Ok((path.clone(), revision(&bytes)))
            })
            .collect::<Result<InputRevisions, String>>()
            .map(Some)
    }
}

impl Commands {
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path).map_err(|err| err.to_string())?;
        let commands: Self = serde_json::from_str(&text).map_err(|err| err.to_string())?;
        for (name, command) in &commands.0 {
            if !command.program.is_absolute() || !command.program.is_file() {
                return Err(format!(
                    "{name} requires an absolute path to a validation executable"
                ));
            }
            if command.artifacts.iter().any(|p| {
                p.as_os_str().is_empty()
                    || p.components()
                        .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
            }) {
                return Err(format!(
                    "{name} artifact paths must stay inside the workspace"
                ));
            }
            if command.inputs.iter().any(|path| !path.is_absolute()) {
                return Err(format!("{name} inputs must be absolute file paths"));
            }
            if !matches!(command.timeout_seconds.unwrap_or(120), 1..=900) {
                return Err(format!("{name} timeout_seconds must be in 1..=900"));
            }
        }
        Ok(commands)
    }

    pub fn definition(&self) -> Option<Value> {
        if self.0.is_empty() {
            return None;
        }
        let names = self.0.keys().collect::<Vec<_>>();
        let descriptions = self
            .0
            .iter()
            .map(|(key, value)| format!("{key}: {}", value.description))
            .collect::<Vec<_>>()
            .join(". ");
        Some(definition("validate", &format!("Run a configured check against the current workspace. Read the exit status and tested revision before making a verification claim. {descriptions}"),
            json!({"check":{"type":"string","enum":names}}), &["check"]))
    }

    pub(crate) fn program_revision(&self, check: &str) -> Option<String> {
        let command = self.0.get(check)?;
        fs::read(&command.program).ok().map(|bytes| revision(&bytes))
    }

    pub(crate) fn program_revisions(&self) -> BTreeMap<&str, Option<String>> {
        self.0.keys().map(|check| (check.as_str(), self.program_revision(check))).collect()
    }

    pub(crate) fn input_revisions(&self, check: &str) -> Option<InputRevisions> {
        self.0.get(check)?.input_revisions().ok().flatten()
    }

    pub(crate) fn input_revision_profile(&self) -> BTreeMap<&str, Option<InputRevisions>> {
        self.0
            .iter()
            .filter(|(_, command)| !command.inputs.is_empty())
            .map(|(check, command)| (check.as_str(), command.input_revisions().ok().flatten()))
            .collect()
    }
}

fn manifest(
    ctx: &WorkspaceContext,
    journal: &Journal,
    copy: Option<&Path>,
) -> Result<String, String> {
    let (files, all) = walk(ctx.root(), ctx.config().max_list_entries)?;
    if !all {
        return Err("Workspace exceeds validation manifest file limit".to_owned());
    }
    let mut entries = BTreeMap::new();
    for path in files {
        let path = path.canonicalize().map_err(|err| err.to_string())?;
        if !path.starts_with(ctx.root()) {
            return Err("Validation input resolves outside the workspace".to_owned());
        }
        let bytes = fs::read(&path).map_err(|err| err.to_string())?;
        let name = ctx.relative_display(&path);
        let hash = if let Some(copy) = copy {
            let dest = copy.join(&name);
            fs::create_dir_all(dest.parent().ok_or("Missing snapshot parent")?)
                .map_err(|e| e.to_string())?;
            fs::write(dest, &bytes).map_err(|e| e.to_string())?;
            journal.put(&bytes)?
        } else {
            revision(&bytes)
        };
        entries.insert(name, hash);
    }
    journal.store_json(&json!(entries))
}

pub fn workspace_version(ctx: &WorkspaceContext, journal: &Journal) -> Result<String, String> {
    manifest(ctx, journal, None)
}

// Compare the exact manifest the checker used to the one captured at run start.
fn changes_since_start(journal: &Journal, run: usize, tested_revision: &str) -> Option<Value> {
    let baseline = journal.events.iter()
        .find(|event| event.run == run && event.kind == "user")?
        .data["workspace_revision_at_start"].as_str()?;
    let mut files = Vec::new();
    let mut total = 0;
    let mut bytes = 0;
    if baseline != tested_revision {
        let before: BTreeMap<String, String> =
            serde_json::from_slice(&journal.get(baseline).ok()?).ok()?;
        let tested: BTreeMap<String, String> =
            serde_json::from_slice(&journal.get(tested_revision).ok()?).ok()?;
        let paths = before.keys().chain(tested.keys().filter(|path| !before.contains_key(*path)));
        for path in paths {
            let change = match (before.get(path), tested.get(path)) {
                (None, Some(_)) => "added",
                (Some(_), None) => "removed",
                (Some(old), Some(new)) if old != new => "modified",
                _ => continue,
            };
            total += 1;
            let file = json!({"path":path,"change":change});
            let size = json_byte_len(&file) + 1;
            if bytes + size <= 1024 {
                bytes += size;
                files.push(file);
            }
        }
    }
    Some(json!({"from_revision":baseline,"files":files,"total":total,"complete":files.len()==total}))
}

pub fn run(
    ctx: &WorkspaceContext,
    journal: &mut Journal,
    run: usize,
    operation: &str,
    commands: &Commands,
    args: &Value,
) -> Result<Value, String> {
    let Some(check) = args
        .get("check")
        .and_then(Value::as_str)
        .filter(|_| args.as_object().is_some_and(|obj| obj.len() == 1))
    else {
        return Ok(
            json!({"status":"error","code":"INVALID_ARGUMENT","reason":"Supply one configured check name."}),
        );
    };
    let Some(config) = commands.0.get(check) else {
        return Ok(
            json!({"status":"error","code":"UNKNOWN_CHECK","reason":"Use a configured validation check."}),
        );
    };
    if let Some(receipt) = journal
        .events
        .iter()
        .find(|e| e.kind == "validation_receipt" && e.data["operation"] == operation)
    {
        return Ok(receipt.data.clone());
    }
    if journal
        .events
        .iter()
        .any(|e| e.kind == "validation_intent" && e.data["operation"] == operation)
    {
        return Ok(
            json!({"status":"error","code":"VALIDATION_INTERRUPTED","operation":operation,
            "reason":"No completed receipt exists. Start a new validation if a fresh result is needed."}),
        );
    }

    let snapshot = tempfile::Builder::new()
        .prefix("check-")
        .tempdir_in(ctx.root().join(".sourceharness"))
        .map_err(|err| err.to_string())?;
    let before = manifest(ctx, journal, Some(snapshot.path()))?;
    let program = fs::read(&config.program).map_err(|err| err.to_string())?;
    let program_revision = revision(&program);
    let input_revisions = match config.input_revisions() {
        Ok(revisions) => revisions,
        Err(reason) => return Ok(json!({
            "status":"error","code":"VALIDATION_INPUT_UNAVAILABLE","check":check,"reason":reason
        })),
    };
    let mut intent = json!({"check":check,"workspace_revision":before,"program_revision":program_revision,
                           "config":config,"operation":operation});
    if let Some(revisions) = &input_revisions {
        intent["input_revisions"] = json!(revisions);
    }
    journal.append(run, "validation_intent", intent)?;
    let mut stdout = tempfile::tempfile().map_err(|err| err.to_string())?;
    let mut stderr = tempfile::tempfile().map_err(|err| err.to_string())?;
    let launched = Command::new(&config.program)
        .args(&config.args)
        .current_dir(snapshot.path())
        .stdin(Stdio::null())
        .env_remove("OPENAI_API_KEY")
        .stdout(stdout.try_clone().map_err(|err| err.to_string())?)
        .stderr(stderr.try_clone().map_err(|err| err.to_string())?)
        .spawn();
    let (status, timed_out, launch_error) = match launched {
        Ok(mut child) => {
            let deadline =
                Instant::now() + Duration::from_secs(config.timeout_seconds.unwrap_or(120));
            loop {
                if let Some(exit) = child.try_wait().map_err(|err| err.to_string())? {
                    break (exit.code(), false, None);
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    child.wait().map_err(|err| err.to_string())?;
                    break (None, true, None);
                }
                std::thread::sleep(Duration::from_millis(40));
            }
        }
        Err(err) => (None, false, Some(err.to_string())),
    };
    let input_revisions_after = config.input_revisions();
    let input_revisions_stable =
        matches!(&input_revisions_after, Ok(after) if *after == input_revisions);
    let mut bytes = Vec::new();
    let mut streams = BTreeMap::new();
    let mut output_complete = true;
    for (name, file) in [("stdout", &mut stdout), ("stderr", &mut stderr)] {
        file.seek(SeekFrom::Start(0))
            .map_err(|err| err.to_string())?;
        let length = bytes.len();
        file.take(8 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|err| err.to_string())?;
        if bytes.len() > length + 8 * 1024 * 1024 {
            bytes.truncate(length + 8 * 1024 * 1024);
            output_complete = false;
        }
        streams.insert(name, journal.put(&bytes[length..])?);
    }
    let output = String::from_utf8_lossy(&bytes);
    let report=journal.artifact(run,json!({"kind":"validation","check":check,"workspace_revision":before,"operation":operation}),output.as_bytes())?;
    let after = workspace_version(ctx, journal)?;
    let mut artifacts = BTreeMap::new();
    for relative in &config.artifacts {
        if let Ok(path) = snapshot.path().join(relative).canonicalize() {
            if path.starts_with(snapshot.path()) && path.is_file() {
                artifacts.insert(
                    relative,
                    journal.put(&fs::read(path).map_err(|err| err.to_string())?)?,
                );
            }
        }
    }
    let mut result = json!({"status":"ok","check":check,"operation":operation,"passed":status==Some(0)&&input_revisions_stable,
        "exit_code":status,"timed_out":timed_out,"launch_error":launch_error,"workspace_revision":before,
        "workspace_changed_during_check":before!=after,"program_revision":program_revision,"artifacts":artifacts,
        "result_ref":report.result_ref,"output_complete":output_complete,"streams":streams,
        "preview":truncate_utf8_bytes(&output,512).0});
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
    if result["passed"] == true && before == after {
        if let Some(changes) = changes_since_start(journal, run, &before) {
            let field = "workspace_changes_since_start";
            if json_byte_len(&result) + json_byte_len(&changes) + field.len() + 4 <= MAX_RESPONSE_BYTES {
                result[field] = changes;
            }
        }
    }
    journal.append(run, "validation_receipt", result.clone())?;
    Ok(result)
}

#[cfg(test)]
#[path = "validation_input_tests.rs"]
mod input_tests;
