mod journal;
#[cfg(test)]
mod tests;
mod tools;
mod validation;

pub use tools::EditMode;
pub use validation::Commands;

use crate::budget::{json_byte_len, truncate_utf8_bytes, MAX_RESPONSE_BYTES};
use crate::transport::sse::ParsedResponse;
use crate::transport::{extract_final_text, NoopRunObserver, ResponsesClient, TransportError};
use crate::workspace::{WorkspaceConfig, WorkspaceContext};
use journal::{get_blob, read_events, revision, Journal};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const INSTRUCTIONS: &str = "You are working in a real workspace. Tool results are evidence. Use research to locate evidence and read only when more exact content is needed. Preserve unrelated content. Copy the path and revision from the observed file into write or edit. On a version conflict, inspect the current evidence before retrying. Ground verification claims in validate results for the relevant revision. Answer when the user's objective and requested checks are complete.";

const FRESHNESS_INSTRUCTIONS: &str = "Use fresh workspace evidence for requirements that can change. When workspace_changes identifies an earlier read as stale, inspect the current source before relying on that earlier content. Earlier validations and snapshots remain available as historical evidence.";

#[derive(Clone, Copy, Debug, Default)]
pub enum HistoryMode {
    Conversation,
    #[default]
    Events,
    Facts,
}

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub edit: EditMode,
    pub history: HistoryMode,
    pub commands: Commands,
}

pub struct Session {
    ctx: WorkspaceContext,
    journal: Journal,
    options: Options,
    binary_revision: String,
    _workspace_lock: File,
}

#[derive(Debug)]
struct Call {
    operation: String,
    call_id: String,
    name: String,
    raw_args: String,
}

#[derive(Default)]
struct Checkpoint {
    run: usize,
    round: usize,
    input: Vec<Value>,
    prior_len: usize,
    pending: VecDeque<Call>,
    final_text: Option<String>,
    profile: Value,
}

fn session_path(root: &Path, name: &str) -> Result<PathBuf, String> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return Err(
            "Session name must contain 1–64 letters, digits, underscores, or hyphens.".to_owned(),
        );
    }
    Ok(root.join(".sourceharness").join("sessions").join(name))
}

impl Session {
    pub fn open(root: &Path, name: &str, options: Options) -> Result<Self, String> {
        let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root))?;
        let dir = session_path(ctx.root(), name)?;
        for path in [
            ctx.root().join(".sourceharness"),
            ctx.root().join(".sourceharness/sessions"),
            dir.clone(),
        ] {
            if let Ok(path_on_disk) = path.canonicalize() {
                if path_on_disk != path {
                    return Err("Session storage cannot be redirected".to_owned());
                }
            }
        }
        fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
        let canonical = dir.canonicalize().map_err(|err| err.to_string())?;
        if !canonical.starts_with(ctx.root().join(".sourceharness")) {
            return Err(
                "Session storage must stay inside the workspace's .sourceharness directory"
                    .to_owned(),
            );
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(ctx.root().join(".sourceharness/workspace.lock"))
            .map_err(|err| err.to_string())?;
        lock.try_lock()
            .map_err(|err| format!("Workspace already has an open SourceHarness session: {err}"))?;
        let journal = Journal::open(&canonical)?;
        let binary = std::env::current_exe()
            .and_then(fs::read)
            .map_err(|err| err.to_string())?;
        Ok(Self {
            ctx,
            journal,
            options,
            binary_revision: revision(&binary),
            _workspace_lock: lock,
        })
    }

    pub fn is_pending(&self) -> Result<bool, String> {
        let state = self.checkpoint()?;
        Ok(state.run > 0 && state.final_text.is_none())
    }

    pub fn run(
        &mut self,
        client: &ResponsesClient,
        prompt: Option<&str>,
    ) -> Result<String, String> {
        let mut definitions = tools::definitions(self.options.edit);
        definitions.extend(self.options.commands.definition());
        let profile = json!({"model":client.config().model,"effort":client.config().reasoning_effort,
            "gateway":client.config().base_url,"tools_revision":revision(&serde_json::to_vec(&definitions).map_err(|e|e.to_string())?),
            "binary_revision":self.binary_revision,"history_mode":format!("{:?}",self.options.history),
            "validation":self.options.commands,"workspace":self.ctx.root()});
        let mut checkpoint = self.checkpoint()?;
        if let Some(prompt) = prompt {
            if checkpoint.run > 0 && checkpoint.final_text.is_none() {
                return Err("A run is pending. Resume it before starting a new request.".to_owned());
            }
            let run = self.journal.events.len() + 1;
            self.journal
                .append(run, "user", json!({"text":prompt,"profile":profile}))?;
            checkpoint.run = run;
            checkpoint.round = 0;
            checkpoint.final_text = None;
            checkpoint.prior_len = checkpoint.input.len();
            checkpoint
                .input
                .push(json!({"role":"user","content":prompt}));
        } else {
            if checkpoint.run == 0 {
                return Err("No run to resume.".to_owned());
            }
            if let Some(final_text) = checkpoint.final_text {
                return Ok(final_text);
            }
            if checkpoint.profile != profile {
                return Err("Resume with the original model, executable, tool definitions, history mode, and validation configuration.".to_owned());
            }
        }
        let run = checkpoint.run;
        for step in 0..=client.config().max_turns {
            while let Some(call) = checkpoint.pending.pop_front() {
                checkpoint
                    .input
                    .push(self.execute(run, checkpoint.round, &call)?);
            }
            if step == client.config().max_turns {
                break;
            }

            let round = checkpoint.round + 1;
            let mut body = client.request_body(&checkpoint.input, None, &definitions);
            let changes = if matches!(self.options.history, HistoryMode::Conversation) {
                Vec::new()
            } else {
                self.project_workspace_evidence(&mut body["input"], checkpoint.prior_len)
            };
            body["instructions"] = json!(self.instructions(run, changes)?);
            let received = Instant::now();
            let (request, response) = self.request(client, run, round, &profile, &body)?;
            let output = self.journal.store_json(&json!(response.output_items))?;
            let raw_response = self.journal.store_json(&response.response)?;
            let calls = match calls_from(&response.output_items, run, round) {
                Ok(calls) => calls,
                Err(error) => {
                    self.journal.append(
                        run,
                        "model_rejected",
                        json!({"request":request,"round":round,
                        "output":output,"raw_response":raw_response,"error":error,
                        "usage":response.usage_reported.then_some(response.usage)}),
                    )?;
                    return Err(error);
                }
            };
            let final_text = calls
                .is_empty()
                .then(|| extract_final_text(&response.output_items));
            let usage = response.usage_reported.then_some(response.usage);
            self.journal.append(run,"model_response",json!({"request":request,"round":round,"output":output,
                "raw_response":raw_response,"response_id":response.response_id,"usage":usage,"elapsed_ms":received.elapsed().as_millis(),
                "final_text":final_text}))?;
            if let Some(final_text) = final_text {
                return Ok(final_text);
            }
            checkpoint.input.extend(response.output_items);
            checkpoint.round = round;
            checkpoint.pending = calls;
        }
        self.journal.append(
            run,
            "paused",
            json!({"reason":"MODEL_TURN_LIMIT","round":checkpoint.round}),
        )?;
        Err("Model turn limit reached. The run can be resumed.".to_owned())
    }

    fn request(
        &mut self,
        client: &ResponsesClient,
        run: usize,
        round: usize,
        profile: &Value,
        body: &Value,
    ) -> Result<(usize, ParsedResponse), String> {
        let payload = self.journal.store_json(body)?;
        let mut observer = NoopRunObserver;
        for attempt in 1..=2 {
            let request=self.journal.append(run,"model_request",json!({"round":round,"attempt":attempt,"input":payload,
                "model":client.config().model,"tools_revision":profile["tools_revision"],"effort":client.config().reasoning_effort}))?;
            match client.send_turn(body, &mut observer) {
                Ok(response) => return Ok((request, response)),
                Err(error) => {
                    self.journal.append(run,"model_error",json!({"request":request,"round":round,"error":error.to_string(),"usage":null}))?;
                    let temporary = matches!(&error,TransportError::Http(detail) if ["HTTP 429:","HTTP 502:","HTTP 503:","HTTP 504:"].iter().any(|prefix|detail.starts_with(prefix)));
                    if attempt == 1 && temporary {
                        std::thread::sleep(Duration::from_millis(500));
                    } else {
                        return Err(format!("{error}. The run can be resumed."));
                    }
                }
            }
        }
        unreachable!()
    }

    fn checkpoint(&self) -> Result<Checkpoint, String> {
        let mut checkpoint = Checkpoint::default();
        for event in &self.journal.events {
            match event.kind.as_str() {
                "user" => {
                    checkpoint.run = event.run;
                    checkpoint.round = 0;
                    checkpoint.profile = event.data["profile"].clone();
                    checkpoint.final_text = None;
                    checkpoint.pending.clear();
                    checkpoint.prior_len = checkpoint.input.len();
                    checkpoint
                        .input
                        .push(json!({"role":"user","content":event.data["text"]}));
                }
                "model_response" => {
                    let output = self.journal.load_json(&event.data["output"])?;
                    let items = output
                        .as_array()
                        .ok_or("Stored response has no output array")?;
                    checkpoint.round = event.data["round"]
                        .as_u64()
                        .ok_or("Response has no round")?
                        as usize;
                    checkpoint.pending = calls_from(items, event.run, checkpoint.round)?;
                    checkpoint.input.extend(items.iter().cloned());
                    checkpoint.final_text = event.data["final_text"].as_str().map(str::to_owned);
                }
                "tool_result" => {
                    let call = checkpoint
                        .pending
                        .pop_front()
                        .ok_or("Receipt has no pending tool")?;
                    if event.data["operation"] != call.operation {
                        return Err("Tool receipts are out of order".to_owned());
                    }
                    let value = self.journal.load_json(&event.data["output"])?;
                    checkpoint.input.push(function_output(&call, &value));
                }
                _ => {}
            }
        }
        Ok(checkpoint)
    }

    fn execute(&mut self, run: usize, round: usize, call: &Call) -> Result<Value, String> {
        let args: Result<Value, _> = serde_json::from_str(&call.raw_args);
        let input = self.journal.put(call.raw_args.as_bytes())?;
        let attempt = self.journal.append(
            run,
            "tool_attempt",
            json!({"operation":call.operation,"round":round,
            "call_id":call.call_id,"name":call.name,"arguments":input}),
        )?;
        let started = Instant::now();
        let mut result = match (call.name.as_str(), args) {
            (_, Err(err)) => {
                json!({"status":"error","code":"INVALID_ARGUMENT","reason":err.to_string()})
            }
            ("read", Ok(args)) => tools::read(&self.ctx, &mut self.journal, run, &args)?,
            ("research", Ok(args)) => tools::research(&self.ctx, &mut self.journal, run, &args)?,
            ("edit" | "write", Ok(args)) => tools::modify(
                &self.ctx,
                &mut self.journal,
                run,
                &call.operation,
                &call.name,
                self.options.edit,
                &args,
            )?,
            ("validate", Ok(args)) => validation::run(
                &self.ctx,
                &mut self.journal,
                run,
                &call.operation,
                &self.options.commands,
                &args,
            )?,
            _ => json!({"status":"error","code":"UNKNOWN_TOOL","reason":"Use an available tool."}),
        };
        if json_byte_len(&result) > MAX_RESPONSE_BYTES {
            let status = result["status"].clone();
            let code = result.get("code").cloned();
            let artifact = self.journal.artifact(
                run,
                json!({"kind":"tool_output","operation":call.operation}),
                &serde_json::to_vec_pretty(&result).map_err(|e| e.to_string())?,
            )?;
            result = json!({"status":status,"delivery":"folded","result_ref":artifact.result_ref,"source":artifact.source,
                          "revision":artifact.revision,"reason":"Full tool response is available through read."});
            if let Some(code) = code {
                result["code"] = code;
            }
        }
        let output = self.journal.store_json(&result)?;
        self.journal.append(run,"tool_result",json!({"operation":call.operation,"attempt":attempt,"round":round,
            "call_id":call.call_id,"name":call.name,"output":output,"status":result["status"],"code":result["code"],
            "source":result["source"],"revision":result["revision"],"result_ref":result["result_ref"],
            "elapsed_ms":started.elapsed().as_millis()}))?;
        Ok(function_output(call, &result))
    }

    fn project_workspace_evidence(&self, input: &mut Value, prior_len: usize) -> Vec<Value> {
        let Some(items) = input.as_array_mut() else {
            return Vec::new();
        };
        let mut observed = BTreeMap::new();
        for item in items.iter() {
            let Some(output) = tool_payload(item) else {
                continue;
            };
            for evidence in
                std::iter::once(&output).chain(output["hits"].as_array().into_iter().flatten())
            {
                if let Some(path) = workspace_source(evidence) {
                    observed.insert(path.to_owned(), evidence["revision"].clone());
                }
            }
        }
        let mut checked = BTreeMap::new();
        let mut changes = Vec::new();
        for (path, before) in observed {
            if let Some(current) = tools::workspace_revision(&self.ctx, &path) {
                if before != current {
                    changes.push(json!({"path":path,"last_observed_revision":before,"current_revision":current}));
                }
                checked.insert(path, current);
            }
        }
        for item in items.iter_mut().take(prior_len) {
            let Some(mut output) = tool_payload(item) else {
                continue;
            };
            let mut adjusted = project_historical_source(&mut output, &checked);
            if let Some(hits) = output.get_mut("hits").and_then(Value::as_array_mut) {
                for hit in hits {
                    adjusted |= project_historical_source(hit, &checked);
                }
            }
            if adjusted {
                item["output"] = json!(output.to_string());
            }
        }
        changes
    }

    fn instructions(&self, run: usize, workspace_changes: Vec<Value>) -> Result<String, String> {
        if matches!(self.options.history, HistoryMode::Conversation) {
            return Ok(INSTRUCTIONS.to_owned());
        }
        let validated = self
            .journal
            .events
            .iter()
            .rev()
            .find(|event| event.kind == "validation_receipt");
        let related = match validated {
            Some(event) => {
                let current = validation::workspace_version(&self.ctx, &self.journal).ok();
                let changes = self
                    .journal
                    .events
                    .iter()
                    .rev()
                    .filter(|e| e.seq > event.seq && e.kind == "edit_receipt")
                    .take(4)
                    .map(|e| json!(e))
                    .collect::<Vec<_>>();
                Some((current, json!(event), changes))
            }
            None => None,
        };
        let rejection = self
            .journal
            .events
            .iter()
            .rev()
            .find(|e| e.run == run && e.kind == "tool_result")
            .filter(|e| e.data["status"] == "error")
            .map(|e| json!(e));
        if related.is_none() && rejection.is_none() && workspace_changes.is_empty() {
            return Ok(format!("{INSTRUCTIONS}\n{FRESHNESS_INSTRUCTIONS}"));
        }
        let facts = match self.options.history {
            HistoryMode::Events => {
                let mut records = Vec::new();
                if let Some((current, receipt, changes)) = related {
                    records.push(receipt);
                    records.extend(changes);
                    records
                        .push(json!({"kind":"workspace_observation","workspace_current":current}));
                }
                records.extend(rejection);
                if !workspace_changes.is_empty() {
                    records.push(json!({"kind":"workspace_changes","sources":workspace_changes}));
                }
                json!(records)
            }
            HistoryMode::Facts => json!({"validation":related.map(|(current,receipt,changes)|{
                json!({"workspace_current":current,"receipt":receipt,"changes_after":changes})
            }),"current_run_rejection":rejection,"workspace_changes":workspace_changes}),
            HistoryMode::Conversation => unreachable!(),
        };
        Ok(format!("{INSTRUCTIONS}\n{FRESHNESS_INSTRUCTIONS}\n\nRelated recorded facts. A validation receipt with workspace_revision different from workspace_current only establishes the tested version.\n{facts}"))
    }
}

fn tool_payload(item: &Value) -> Option<Value> {
    if item["type"] != "function_call_output" {
        return None;
    }
    serde_json::from_str(item["output"].as_str()?).ok()
}

fn workspace_source(evidence: &Value) -> Option<&str> {
    if evidence["source"]["kind"] == "workspace" && evidence["revision"].is_string() {
        evidence["source"]["path"].as_str()
    } else {
        None
    }
}

fn project_historical_source(evidence: &mut Value, checked: &BTreeMap<String, Value>) -> bool {
    let Some(current) = workspace_source(evidence)
        .and_then(|path| checked.get(path))
        .filter(|current| **current != evidence["revision"])
    else {
        return false;
    };
    let fields = evidence
        .as_object_mut()
        .expect("workspace evidence is an object");
    for field in [
        "content",
        "text",
        "text_complete",
        "diff",
        "next",
        "preview",
        "planned_to_current",
    ] {
        fields.remove(field);
    }
    fields.insert("historical".to_owned(), json!(true));
    fields.insert("current_revision".to_owned(), current.clone());
    true
}

fn calls_from(items: &[Value], run: usize, round: usize) -> Result<VecDeque<Call>, String> {
    let mut ids = HashSet::new();
    items
        .iter()
        .filter(|item| item["type"] == "function_call")
        .enumerate()
        .map(|(i, item)| {
            let id = item["call_id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or("Tool call has no call_id")?;
            if !ids.insert(id) {
                return Err("Duplicate call_id inside one response".to_owned());
            }
            Ok(Call {
                operation: format!("{run}.{round}.{}", i + 1),
                call_id: id.to_owned(),
                name: item["name"]
                    .as_str()
                    .ok_or("Tool call has no name")?
                    .to_owned(),
                raw_args: item["arguments"]
                    .as_str()
                    .ok_or("Tool call has no arguments")?
                    .to_owned(),
            })
        })
        .collect()
}

fn function_output(call: &Call, value: &Value) -> Value {
    json!({"type":"function_call_output","call_id":call.call_id,"output":value.to_string()})
}

pub fn inspect(
    root: &Path,
    name: &str,
    reference: Option<&str>,
    event: Option<usize>,
) -> Result<String, String> {
    let dir = session_path(root, name)?;
    let (events, _) = read_events(&dir)?;
    if let Some(reference) = reference {
        let item = events
            .iter()
            .find(|event| event.kind == "artifact" && event.data["result_ref"] == reference)
            .ok_or("Unknown result_ref in this session")?;
        let hash = item.data["revision"]
            .as_str()
            .ok_or("Missing artifact version")?;
        return String::from_utf8(get_blob(&dir, hash)?).map_err(|err| err.to_string());
    }
    if let Some(seq) = event {
        let entry = events
            .get(seq.checked_sub(1).ok_or("Event numbers start at 1")?)
            .ok_or("Unknown event")?;
        let mut output = json!(entry);
        let key = match entry.kind.as_str() {
            "tool_attempt" => Some("arguments"),
            "tool_result" => Some("output"),
            "model_request" => Some("input"),
            "model_response" => Some("raw_response"),
            _ => None,
        };
        if let Some(hash) = key.and_then(|key| entry.data[key].as_str()) {
            let bytes = get_blob(&dir, hash)?;
            let receipt = String::from_utf8_lossy(&bytes);
            let mut detail: Value =
                serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(receipt));
            for key in ["input", "output"] {
                if let Some(items) = detail.get_mut(key).and_then(Value::as_array_mut) {
                    for item in items {
                        if item.get("type").and_then(Value::as_str) == Some("reasoning") {
                            *item = json!({"type":"reasoning","id":item["id"],"redacted":true});
                        }
                    }
                }
            }
            output["detail"] = detail;
        }
        return serde_json::to_string_pretty(&output).map_err(|err| err.to_string());
    }
    Ok(events
        .iter()
        .map(|entry| {
            let detail = match entry.kind.as_str() {
                "user" => entry.data["text"].as_str().unwrap_or_default().to_owned(),
                "tool_attempt" | "tool_result" => format!(
                    "{} {} {}",
                    entry.data["operation"],
                    entry.data["name"],
                    entry.data.get("status").unwrap_or(&Value::Null)
                ),
                "model_request" | "model_response" => format!("round {}", entry.data["round"]),
                "artifact" => format!("{} {}", entry.data["result_ref"], entry.data["source"]),
                _ => entry.data.to_string(),
            };
            format!(
                "{:>5} run {:>5} {:<20} {}",
                entry.seq,
                entry.run,
                entry.kind,
                truncate_utf8_bytes(&detail, 220).0
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

pub fn fork_snapshot(
    root: &Path,
    name: &str,
    manifest: &str,
    destination: &Path,
) -> Result<(), String> {
    let dir = session_path(root, name)?;
    let (events, _) = read_events(&dir)?;
    if !events.iter().any(|event| {
        event.kind == "validation_intent" && event.data["workspace_revision"] == manifest
    }) {
        return Err(
            "The supplied version was not a recorded validation snapshot in this session."
                .to_owned(),
        );
    }
    let entries: BTreeMap<String, String> =
        serde_json::from_slice(&get_blob(&dir, manifest)?).map_err(|e| e.to_string())?;
    for (path, hash) in &entries {
        if path.is_empty() || !tools::permitted(Path::new(path)) {
            return Err(format!("Unsafe snapshot path: {path}"));
        }
        get_blob(&dir, hash)?;
    }
    fs::create_dir(destination).map_err(|err| format!("Create a new fork directory: {err}"))?;
    for (path, hash) in entries {
        let target = destination.join(path);
        fs::create_dir_all(target.parent().ok_or("Missing fork parent")?)
            .map_err(|err| err.to_string())?;
        fs::write(target, get_blob(&dir, &hash)?).map_err(|err| err.to_string())?;
    }
    Ok(())
}
