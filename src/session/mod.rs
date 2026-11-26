mod history;
mod journal;
#[cfg(test)]
mod tests;
mod tools;
mod validation;

pub use history::{DEFAULT_BUDGET as DEFAULT_HISTORY_BUDGET, MAX_BUDGET as MAX_HISTORY_BUDGET, MIN_BUDGET as MIN_HISTORY_BUDGET};
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

#[derive(Clone, Copy, Debug, Default)]
pub enum HistoryMode {
    Conversation,
    #[default]
    Events,
    Facts,
}

impl HistoryMode {
    fn label(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::Events => "events",
            Self::Facts => "facts",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Options {
    pub edit: EditMode,
    pub history: HistoryMode,
    pub commands: Commands,
    pub freshness_instructions: Option<bool>,
    pub stale_projection: Option<bool>,
    pub history_budget: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            edit: EditMode::default(),
            history: HistoryMode::default(),
            commands: Commands::default(),
            freshness_instructions: None,
            stale_projection: None,
            history_budget: DEFAULT_HISTORY_BUDGET,
        }
    }
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

enum Start<'a> {
    Prompt(&'a str),
    Resume,
    Continue(Option<&'a str>),
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
        if !(MIN_HISTORY_BUDGET..=MAX_HISTORY_BUDGET).contains(&options.history_budget) {
            return Err(format!("History budget must be {MIN_HISTORY_BUDGET}..={MAX_HISTORY_BUDGET} bytes."));
        }
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
        self.run_with_start(client, prompt.map_or(Start::Resume, Start::Prompt))
    }

    pub fn continue_pending(
        &mut self,
        client: &ResponsesClient,
        correction: Option<&str>,
    ) -> Result<String, String> {
        self.run_with_start(client, Start::Continue(correction))
    }

    fn run_with_start(&mut self, client: &ResponsesClient, start: Start<'_>) -> Result<String, String> {
        let mut definitions = tools::definitions(self.options.edit);
        definitions.extend(self.options.commands.definition());
        let history_enabled = !matches!(self.options.history, HistoryMode::Conversation);
        let freshness = self.options.freshness_instructions.unwrap_or(history_enabled);
        let projection = self.options.stale_projection.unwrap_or(history_enabled);
        let profile = json!({"model":client.config().model,"effort":client.config().reasoning_effort,
            "gateway":client.config().base_url,"tools_revision":revision(&serde_json::to_vec(&definitions).map_err(|e|e.to_string())?),
            "binary_revision":self.binary_revision,"history_mode":format!("{:?}",self.options.history),
            "freshness_instructions":freshness,"stale_projection":projection,"history_budget_bytes":self.options.history_budget,
            "validation":self.options.commands,"validation_programs":self.options.commands.program_revisions(),
            "workspace":self.ctx.root()});
        let mut checkpoint = self.checkpoint()?;
        let entry = match start {
            Start::Prompt(prompt) => {
                if checkpoint.run > 0 && checkpoint.final_text.is_none() {
                    return Err("A run is pending. Use --resume or --continue before starting a new request.".to_owned());
                }
                Some(json!({"text":prompt,"profile":profile}))
            }
            Start::Resume => {
                if checkpoint.run == 0 {
                    return Err("No run to resume.".to_owned());
                }
                if let Some(final_text) = checkpoint.final_text {
                    return Ok(final_text);
                }
                if checkpoint.profile != profile {
                    return Err("Resume with the original model, executable, tool definitions, history mode, and validation configuration. Use --continue to start a linked run with current conditions.".to_owned());
                }
                None
            }
            Start::Continue(correction) => Some(self.continuation(&checkpoint, &profile, correction)?),
        };
        if let Some(data) = entry {
            let run = self.journal.events.len() + 1;
            self.journal.append(run, "user", data.clone())?;
            self.apply_user(&mut checkpoint, run, &data)?;
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
            let changes = if projection {
                self.project_workspace_evidence(&mut body["input"], checkpoint.prior_len)
            } else {
                Vec::new()
            };
            let records = if history_enabled {
                history::related(&self.ctx, &self.journal, run, &profile, &self.options.commands)
            } else {
                Vec::new()
            };
            let auxiliary = history::prepare(
                self.options.history,
                freshness,
                projection,
                self.options.history_budget,
                changes,
                records,
            );
            body["instructions"] = json!(format!("{INSTRUCTIONS}{}", auxiliary.suffix));
            let (request, response, elapsed_ms) = self.request(client, run, round, &profile, &body, &auxiliary.audit)?;
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
                        "usage":response.usage_reported.then_some(response.usage),"elapsed_ms":elapsed_ms}),
                    )?;
                    return Err(error);
                }
            };
            let final_text = calls
                .is_empty()
                .then(|| extract_final_text(&response.output_items));
            let usage = response.usage_reported.then_some(response.usage);
            self.journal.append(run,"model_response",json!({"request":request,"round":round,"output":output,
                "raw_response":raw_response,"response_id":response.response_id,"usage":usage,"elapsed_ms":elapsed_ms,
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
        history_auxiliary: &Value,
    ) -> Result<(usize, ParsedResponse, u128), String> {
        let payload = self.journal.store_json(body)?;
        let mut observer = NoopRunObserver;
        for attempt in 1..=2 {
            let request=self.journal.append(run,"model_request",json!({"round":round,"attempt":attempt,"input":payload,
                "model":client.config().model,"tools_revision":profile["tools_revision"],"effort":client.config().reasoning_effort,
                "history_auxiliary":history_auxiliary}))?;
            let started = Instant::now();
            match client.send_turn(body, &mut observer) {
                Ok(response) => return Ok((request, response, started.elapsed().as_millis())),
                Err(error) => {
                    self.journal.append(run,"model_error",json!({"request":request,"round":round,"error":error.to_string(),"usage":null,
                        "elapsed_ms":started.elapsed().as_millis()}))?;
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
                "user" => self.apply_user(&mut checkpoint, event.run, &event.data)?,
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

    fn apply_user(&self, checkpoint: &mut Checkpoint, run: usize, data: &Value) -> Result<(), String> {
        if let Some(link) = data.get("continuation") {
            let output = self.journal.load_json(&link["outputs"])?;
            let outputs = output.as_array().ok_or("Continuation has no output array")?;
            if link["run"] != checkpoint.run
                || checkpoint.final_text.is_some()
                || outputs.len() != checkpoint.pending.len()
                || outputs.iter().zip(&checkpoint.pending).any(|(output, call)| {
                    output["type"] != "function_call_output" || output["call_id"] != call.call_id
                })
            {
                return Err("Continuation does not match the prior pending calls.".to_owned());
            }
            checkpoint.input.extend(outputs.iter().cloned());
        }
        checkpoint.run = run;
        checkpoint.round = 0;
        checkpoint.profile = data["profile"].clone();
        checkpoint.final_text = None;
        checkpoint.pending.clear();
        checkpoint.prior_len = checkpoint.input.len();
        checkpoint.input.push(json!({"role":"user","content":data["text"]}));
        Ok(())
    }

    fn continuation(&self, checkpoint: &Checkpoint, profile: &Value, correction: Option<&str>) -> Result<Value, String> {
        if checkpoint.run == 0 || checkpoint.final_text.is_some() {
            return Err("No unfinished run to continue.".to_owned());
        }
        let outputs: Vec<Value> = checkpoint.pending.iter().map(|call| {
            function_output(call, &self.observe_pending(call))
        }).collect();
        let outputs = self.journal.store_json(&json!(outputs))?;
        let mut text = format!(
            "Continue the unfinished task from run {} under the current configuration. Pending calls from that run were reconciled from the journal and current workspace. Use the recorded outcomes and inspect the current workspace before deciding what work remains.",
            checkpoint.run
        );
        if let Some(correction) = correction {
            text.push_str("\n\nUser correction to the original task:\n");
            text.push_str(correction);
        }
        Ok(json!({"text":text,"profile":profile,"continuation":{
            "run":checkpoint.run,"round":checkpoint.round,"outputs":outputs,"correction":correction
        }}))
    }

    fn observe_pending(&self, call: &Call) -> Value {
        let records = self.journal.events.iter().rev().filter(|e| e.data["operation"] == call.operation);
        for event in records {
            match event.kind.as_str() {
                "edit_receipt" | "validation_receipt" => return event.data.clone(),
                "edit_intent" => {
                    let path = event.data["path"].as_str().unwrap_or("");
                    let current = tools::workspace_revision(&self.ctx, path);
                    let disposition = match current.as_ref() {
                        Some(observed) if observed == &event.data["after"] => "observed_target",
                        Some(observed) if observed == &event.data["before"] => "observed_original",
                        Some(_) => "observed_different_version",
                        None => "current_version_unavailable",
                    };
                    return json!({"status":"error","code":"CONTINUATION_OBSERVATION","outcome":disposition,
                        "intent_event":event.seq,"operation":call.operation,"path":path,
                        "expected_revision":event.data["before"],"planned_revision":event.data["after"],
                        "current_revision":current,"reason":"Inspect the live file before deciding on another edit."});
                }
                "validation_intent" => return json!({"status":"error","code":"VALIDATION_INTERRUPTED",
                    "intent_event":event.seq,"operation":call.operation,
                    "reason":"The original check has no completed receipt."}),
                _ => {}
            }
        }
        json!({"status":"error","code":"CALL_SUPERSEDED","operation":call.operation,
            "reason":"The pending call was superseded at the start of the linked continuation."})
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
            ("search_result", Ok(args)) => tools::search_result(&self.journal, &args)?,
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
        "data",
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
        let payload = match entry.kind.as_str() {
            "user" => entry.data["continuation"]["outputs"].as_str(),
            "tool_attempt" => entry.data["arguments"].as_str(),
            "tool_result" => entry.data["output"].as_str(),
            "model_request" => entry.data["input"].as_str(),
            "model_response" => entry.data["raw_response"].as_str(),
            _ => None,
        };
        if let Some(hash) = payload {
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
