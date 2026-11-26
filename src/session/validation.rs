use super::journal::{revision, Journal};
use super::tools::{definition, walk};
use crate::budget::truncate_utf8_bytes;
use crate::workspace::WorkspaceContext;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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
    pub timeout_seconds: Option<u64>,
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
    let inputs = json!({"check":check,"workspace_revision":before,"program_revision":program_revision,
                        "config":config,"operation":operation});
    journal.append(run, "validation_intent", inputs)?;
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
    let result = json!({"status":"ok","check":check,"operation":operation,"passed":status==Some(0),
        "exit_code":status,"timed_out":timed_out,"launch_error":launch_error,"workspace_revision":before,
        "workspace_changed_during_check":before!=after,"program_revision":program_revision,"artifacts":artifacts,
        "result_ref":report.result_ref,"output_complete":output_complete,"streams":streams,
        "preview":truncate_utf8_bytes(&output,512).0});
    journal.append(run, "validation_receipt", result.clone())?;
    Ok(result)
}
