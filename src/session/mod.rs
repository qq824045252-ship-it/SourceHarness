mod compact;
mod exec;
mod guard;
mod history;
mod inbox;
mod journal;
mod receipt;
mod rollback;
#[cfg(test)]
mod tests;
mod tools;
mod validation;

pub use history::{DEFAULT_BUDGET as DEFAULT_HISTORY_BUDGET, MAX_BUDGET as MAX_HISTORY_BUDGET, MIN_BUDGET as MIN_HISTORY_BUDGET};
pub use inbox::InputInbox;
pub use receipt::ReceiptMode;
pub use tools::EditMode;
pub use validation::Commands;

use crate::budget::{json_byte_len, truncate_utf8_bytes, MAX_RESPONSE_BYTES};
use crate::transport::sse::ParsedResponse;
use crate::transport::{
    extract_final_text, NoopRunObserver, ResponsesClient, RunObserver, ToolCallTrace, TransportError,
    TurnMetrics,
};
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
    pub compact: bool,
    pub edit: EditMode,
    pub history: HistoryMode,
    pub commands: Commands,
    pub freshness_instructions: Option<bool>,
    pub stale_projection: Option<bool>,
    pub workspace_change_notices: Option<bool>,
    pub history_budget: usize,
    pub receipt_mode: ReceiptMode,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            compact: false,
            edit: EditMode::default(),
            history: HistoryMode::default(),
            commands: Commands::default(),
            freshness_instructions: None,
            stale_projection: None,
            workspace_change_notices: None,
            history_budget: DEFAULT_HISTORY_BUDGET,
            receipt_mode: ReceiptMode::Versioned,
        }
    }
}

pub struct Session {
    ctx: WorkspaceContext,
    journal: Journal,
    inbox: InputInbox,
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
    queued_steering: Vec<(usize, Value)>,
    attached_steering: Vec<(usize, usize)>,
    claimed_inputs: HashSet<usize>,
    guard_pause: Option<Value>,
}

impl Checkpoint {
    fn attach_steering(&mut self) {
        for (event, text) in self.queued_steering.drain(..) {
            self.attached_steering.push((event, self.input.len()));
            self.input.push(json!({"role":"user","content":text}));
        }
    }

    fn steer_events(&self) -> Vec<usize> {
        self.attached_steering.iter().map(|&(event, _)| event).collect()
    }

    fn steer_input_indices(&self) -> Vec<usize> {
        self.attached_steering.iter().map(|&(_, index)| index).collect()
    }

    fn claim_input(&mut self, data: &Value) -> Result<(), String> {
        if let Some(id) = data["inbox_id"].as_u64() {
            if !self.claimed_inputs.insert(id as usize) {
                return Err(format!("Session inbox entry {id} was already claimed"));
            }
        }
        Ok(())
    }
}

pub struct SteeringInput {
    pub text: String,
    pub received_ms: u64,
}

enum Start<'a> {
    Prompt(&'a str),
    InboxPrompt(usize),
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
        if options.compact {
            if !matches!(options.edit, EditMode::Text) {
                return Err("The compact tool profile uses exact text edits.".to_owned());
            }
            options.commands.check_exec_boundary(ctx.root())?;
        }
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
        let inbox = InputInbox::open(&canonical)?;
        let binary = std::env::current_exe()
            .and_then(fs::read)
            .map_err(|err| err.to_string())?;
        let session = Self {
            ctx,
            journal,
            inbox,
            options,
            binary_revision: revision(&binary),
            _workspace_lock: lock,
        };
        let checkpoint = session.checkpoint()?;
        if checkpoint.run > 0 && checkpoint.final_text.is_none() {
            session.inbox.lock()?.active_run = Some(checkpoint.run);
        }
        Ok(session)
    }

    pub fn is_pending(&self) -> Result<bool, String> {
        let state = self.checkpoint()?;
        Ok(state.run > 0 && state.final_text.is_none())
    }

    pub fn is_loop_paused(&self) -> Result<bool, String> {
        Ok(self.checkpoint()?.guard_pause.is_some())
    }

    pub fn run(
        &mut self,
        client: &ResponsesClient,
        prompt: Option<&str>,
    ) -> Result<String, String> {
        self.run_observed(client, prompt, &mut NoopRunObserver, &mut || None)
    }

    pub fn run_observed(
        &mut self,
        client: &ResponsesClient,
        prompt: Option<&str>,
        observer: &mut dyn RunObserver,
        poll_steering: &mut dyn FnMut() -> Option<SteeringInput>,
    ) -> Result<String, String> {
        self.run_with_start(client, prompt.map_or(Start::Resume, Start::Prompt), observer, poll_steering)
    }

    pub fn input_inbox(&self) -> InputInbox {
        self.inbox.clone()
    }

    pub fn next_inbox_prompt(&self) -> Result<Option<usize>, String> {
        let checkpoint = self.checkpoint()?;
        if checkpoint.run > 0 && checkpoint.final_text.is_none() {
            return Ok(None);
        }
        Ok(self.inbox.lock()?.entries.iter()
            .find(|entry| entry.steer_run.is_none() && !checkpoint.claimed_inputs.contains(&entry.id))
            .map(|entry| entry.id))
    }

    pub fn has_inbox_steering(&self) -> Result<bool, String> {
        let checkpoint = self.checkpoint()?;
        Ok(checkpoint.run > 0 && checkpoint.final_text.is_none()
            && self.inbox.lock()?.entries.iter().any(|entry| {
                entry.steer_run == Some(checkpoint.run) && !checkpoint.claimed_inputs.contains(&entry.id)
            }))
    }

    pub fn run_inbox_prompt(
        &mut self,
        client: &ResponsesClient,
        id: usize,
        observer: &mut dyn RunObserver,
    ) -> Result<String, String> {
        self.run_with_start(client, Start::InboxPrompt(id), observer, &mut || None)
    }

    pub fn continue_pending(
        &mut self,
        client: &ResponsesClient,
        correction: Option<&str>,
    ) -> Result<String, String> {
        self.run_with_start(client, Start::Continue(correction), &mut NoopRunObserver, &mut || None)
    }

    fn run_with_start(
        &mut self,
        client: &ResponsesClient,
        start: Start<'_>,
        observer: &mut dyn RunObserver,
        poll_steering: &mut dyn FnMut() -> Option<SteeringInput>,
    ) -> Result<String, String> {
        let compact = self.options.compact;
        let definitions = if compact {
            compact::definitions()
        } else {
            let mut definitions = tools::definitions(self.options.edit);
            definitions.extend(self.options.commands.definition());
            definitions
        };
        let history_enabled = !compact && !matches!(self.options.history, HistoryMode::Conversation);
        let freshness = !compact && self.options.freshness_instructions.unwrap_or(history_enabled);
        let projection = !compact && self.options.stale_projection.unwrap_or(history_enabled);
        let notices = !compact && self.options.workspace_change_notices.unwrap_or(projection);
        let history = if compact { HistoryMode::Conversation } else { self.options.history };
        let mut profile = json!({"model":client.config().model,"effort":client.config().reasoning_effort,
            "gateway":client.config().base_url,"tools_revision":revision(&serde_json::to_vec(&definitions).map_err(|e|e.to_string())?),
            "binary_revision":self.binary_revision,"history_mode":format!("{history:?}"),
            "freshness_instructions":freshness,"stale_projection":projection,"history_budget_bytes":self.options.history_budget,
            "validation":self.options.commands,"validation_programs":self.options.commands.program_revisions(),
            "workspace":self.ctx.root()});
        if compact {
            profile["tool_profile"] = json!("compact");
        } else if self.options.receipt_mode != ReceiptMode::Versioned {
            profile["receipt_mode"] = json!(self.options.receipt_mode.label());
        }
        if notices != projection {
            profile["workspace_change_notices"] = json!(notices);
        }
        let validation_inputs = self.options.commands.input_revision_profile();
        if !validation_inputs.is_empty() {
            profile["validation_inputs"] = json!(validation_inputs);
        }
        let mut checkpoint = self.checkpoint()?;
        if checkpoint.profile["tool_profile"] == "compact" && !compact {
            return Err("Use --tool-profile compact to reopen this session.".to_owned());
        }
        let continuation = match start {
            Start::Continue(correction) => Some(self.continuation(&checkpoint, &profile, correction)?),
            _ => None,
        };
        let inbox = self.inbox.clone();
        let mut gate = inbox.lock()?;
        let entry = match start {
            Start::Prompt(_) | Start::InboxPrompt(_) => {
                if checkpoint.run > 0 && checkpoint.final_text.is_none() {
                    return Err("A run is pending. Use --resume or --continue before starting a new request.".to_owned());
                }
                Some(match start {
                    Start::Prompt(prompt) => json!({"text":prompt,"profile":profile}),
                    Start::InboxPrompt(id) => {
                        let entry = gate.entries.iter().find(|entry|
                            entry.id == id && entry.steer_run.is_none() && !checkpoint.claimed_inputs.contains(&id)
                        ).ok_or("Session inbox message is unavailable")?;
                        json!({"text":entry.text,"profile":profile,"inbox_id":id,"received_ms":entry.received_ms})
                    }
                    _ => unreachable!(),
                })
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
            Start::Continue(_) => {
                self.claim_inbox_steering(&mut checkpoint, &gate)?;
                continuation
            }
        };
        if let Some(mut data) = entry {
            if compact {
                data["workspace_revision_at_start"] =
                    json!(validation::save_workspace(&self.ctx, &self.journal)?);
            } else if !self.options.commands.0.is_empty() {
                if let Ok(revision) = validation::workspace_version(&self.ctx, &self.journal) {
                    data["workspace_revision_at_start"] = json!(revision);
                }
            }
            let run = self.journal.events.len() + 1;
            self.journal.append(run, "user", data.clone())?;
            self.apply_user(&mut checkpoint, run, &data)?;
        }
        let run = checkpoint.run;
        gate.active_run = Some(run);
        drop(gate);
        let mut turn = None::<(TurnMetrics, Instant)>;
        let max_turns = client.config().max_turns;
        for step in 0..=max_turns {
            let mut new_pause = None;
            while let Some(call) = checkpoint.pending.pop_front() {
                self.collect_steering(&mut checkpoint, poll_steering)?;
                let guarded = checkpoint.guard_pause.is_some() || new_pause.is_some();
                let budget_hint = if compact && checkpoint.pending.is_empty() {
                    compact::budget_hint(max_turns - step)
                } else {
                    None
                };
                let output = if checkpoint.queued_steering.is_empty() && !guarded {
                    let (output, pause) = self.execute(run, checkpoint.round, &call, observer, budget_hint)?;
                    new_pause = pause;
                    output
                } else {
                    self.close_pending_call(run, checkpoint.round, &call, observer, guarded, budget_hint)?
                };
                checkpoint.input.push(output);
            }
            if let Some((mut metrics, start)) = turn.take() {
                metrics.latency_ms = start.elapsed().as_millis();
                observer.on_turn_complete(&metrics);
            }
            self.collect_steering(&mut checkpoint, poll_steering)?;
            if let Some(details) = new_pause.filter(|_| checkpoint.queued_steering.is_empty()) {
                self.journal.append(run, "paused", json!({
                    "reason":"LOOP_GUARD_TRIGGERED","round":checkpoint.round,"guard":details
                }))?;
                return Err("Loop guard paused this run. Use :steer with guidance in chat or --continue --prompt with a correction.".to_owned());
            }
            if step == max_turns {
                break;
            }

            checkpoint.attach_steering();
            let round = checkpoint.round + 1;
            let turn_start = Instant::now();
            observer.on_turn_start(round);
            let mut body = client.request_body(&checkpoint.input, None, &definitions);
            let changes = if projection || notices {
                self.project_workspace_evidence(&mut body["input"], checkpoint.prior_len, projection, notices)
            } else {
                Vec::new()
            };
            let records = if history_enabled {
                history::related(&self.ctx, &self.journal, run, &profile, &self.options.commands)
            } else {
                Vec::new()
            };
            let mut auxiliary = history::prepare(
                history,
                freshness,
                projection,
                self.options.history_budget,
                changes,
                records,
            );
            body["instructions"] = if compact {
                auxiliary.audit["delivery"] = json!("none");
                json!(compact::INSTRUCTIONS)
            } else {
                json!(format!("{INSTRUCTIONS}{}", auxiliary.static_suffix))
            };
            // Each Responses request carries its own history. Keep one current
            // notice in the wire input and leave the user transcript intact.
            if !auxiliary.dynamic_context.is_empty() {
                body["input"].as_array_mut().expect("Request input is an array").push(json!({
                    "role": "user",
                    "content": auxiliary.dynamic_context
                }));
            }
            let exchange = self.request(client, &checkpoint, &body, &auxiliary.audit, observer);
            if exchange.is_err() {
                self.collect_steering(&mut checkpoint, poll_steering)?;
            }
            let (request, response, elapsed_ms) = exchange?;
            checkpoint.guard_pause = None;
            observer.on_model_response_usage(response.usage_reported.then_some(&response.usage));
            let output = self.journal.store_json(&json!(response.output_items))?;
            let raw_response = self.journal.store_json(&response.response)?;
            checkpoint.round = round;
            let calls = match calls_from(&response.output_items, run, round) {
                Ok(calls) => calls,
                Err(error) => {
                    self.collect_steering(&mut checkpoint, poll_steering)?;
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
            self.collect_offered_steering(&mut checkpoint, poll_steering)?;
            let mut gate = inbox.lock()?;
            self.claim_inbox_steering(&mut checkpoint, &gate)?;
            let final_text = (calls.is_empty() && checkpoint.queued_steering.is_empty())
                .then(|| extract_final_text(&response.output_items));
            let usage = response.usage_reported.then_some(response.usage);
            self.journal.append(run,"model_response",json!({"request":request,"round":round,"output":output,
                "raw_response":raw_response,"response_id":response.response_id,"usage":usage,"elapsed_ms":elapsed_ms,
                "final_text":final_text}))?;
            if final_text.is_some() {
                gate.active_run = None;
            }
            drop(gate);
            let metrics = TurnMetrics {
                turn: round,
                reasoning_item_count: response.output_items.iter().filter(|item| item["type"] == "reasoning").count(),
                function_calls: calls.iter().map(|call| call.name.clone()).collect(),
                usage: response.usage,
                latency_ms: turn_start.elapsed().as_millis(),
            };
            if let Some(final_text) = final_text {
                observer.on_turn_complete(&metrics);
                return Ok(final_text);
            }
            turn = Some((metrics, turn_start));
            checkpoint.input.extend(response.output_items);
            checkpoint.pending = calls;
            checkpoint.attached_steering.clear();
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
        checkpoint: &Checkpoint,
        body: &Value,
        history_auxiliary: &Value,
        observer: &mut dyn RunObserver,
    ) -> Result<(usize, ParsedResponse, u128), String> {
        let payload = self.journal.store_json(body)?;
        let run = checkpoint.run;
        let round = checkpoint.round + 1;
        for attempt in 1..=2 {
            let mut data = json!({"round":round,"attempt":attempt,"input":payload,
                "model":client.config().model,"tools_revision":checkpoint.profile["tools_revision"],"effort":client.config().reasoning_effort,
                "history_auxiliary":history_auxiliary});
            if !checkpoint.attached_steering.is_empty() {
                data["steer_events"] = json!(checkpoint.steer_events());
                data["steer_input_start"] = json!(checkpoint.attached_steering[0].1);
                data["steer_input_indices"] = json!(checkpoint.steer_input_indices());
            }
            let request = self.journal.append(run, "model_request", data)?;
            let started = Instant::now();
            match client.send_turn(body, observer) {
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

    fn collect_steering(
        &mut self,
        checkpoint: &mut Checkpoint,
        poll: &mut dyn FnMut() -> Option<SteeringInput>,
    ) -> Result<(), String> {
        let inbox = self.inbox.clone();
        let state = inbox.lock()?;
        self.claim_inbox_steering(checkpoint, &state)?;
        drop(state);
        self.collect_offered_steering(checkpoint, poll)
    }

    fn claim_inbox_steering(&mut self, checkpoint: &mut Checkpoint, inbox: &inbox::State) -> Result<(), String> {
        for entry in &inbox.entries {
            if entry.steer_run != Some(checkpoint.run) || checkpoint.claimed_inputs.contains(&entry.id) {
                continue;
            }
            let data = json!({"text":entry.text,"received_ms":entry.received_ms,
                "after_round":checkpoint.round,"inbox_id":entry.id});
            let event = self.journal.append(checkpoint.run, "steer", data.clone())?;
            checkpoint.claim_input(&data)?;
            checkpoint.queued_steering.push((event, data["text"].clone()));
        }
        Ok(())
    }

    fn collect_offered_steering(
        &mut self, checkpoint: &mut Checkpoint, poll: &mut dyn FnMut() -> Option<SteeringInput>,
    ) -> Result<(), String> {
        while let Some(input) = poll() {
            let data = json!({"text":input.text,"received_ms":input.received_ms,"after_round":checkpoint.round});
            let event = self.journal.append(checkpoint.run, "steer", data.clone())?;
            checkpoint.queued_steering.push((event, data["text"].clone()));
        }
        Ok(())
    }

    fn checkpoint(&self) -> Result<Checkpoint, String> {
        let mut checkpoint = Checkpoint::default();
        for event in &self.journal.events {
            match event.kind.as_str() {
                "user" => self.apply_user(&mut checkpoint, event.run, &event.data)?,
                "steer" => {
                    checkpoint.claim_input(&event.data)?;
                    checkpoint.queued_steering.push((event.seq, event.data["text"].clone()));
                }
                "model_request" => {
                    checkpoint.guard_pause = None;
                    checkpoint.attach_steering();
                    if !checkpoint.attached_steering.is_empty()
                        && event.data["steer_events"] != json!(checkpoint.steer_events())
                    {
                        return Err("Model request does not match the recorded corrections".to_owned());
                    }
                    if event.data.get("steer_input_indices").is_some()
                        && event.data["steer_input_indices"] != json!(checkpoint.steer_input_indices())
                    {
                        return Err("Model request positions do not match the recorded corrections".to_owned());
                    }
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
                    checkpoint.attached_steering.clear();
                }
                "tool_result" | "tool_skipped" | "tool_reconciled" => {
                    let index = checkpoint.pending.iter()
                        .position(|call| event.data["operation"] == call.operation)
                        .ok_or("Receipt has no matching pending operation")?;
                    let call = checkpoint
                        .pending
                        .remove(index)
                        .ok_or("Receipt has no pending tool")?;
                    let hash = event.data.get("model_output").unwrap_or(&event.data["output"]);
                    let value = self.journal.load_json(hash)?;
                    checkpoint.input.push(function_output(&call, &value));
                    if event.data["guard"]["state"] == "pause" {
                        checkpoint.guard_pause = Some(event.data["guard"].clone());
                    }
                }
                _ => {}
            }
        }
        Ok(checkpoint)
    }

    fn apply_user(&self, checkpoint: &mut Checkpoint, run: usize, data: &Value) -> Result<(), String> {
        checkpoint.claim_input(data)?;
        if data["continuation"]["carry_steering"] != true {
            checkpoint.attached_steering.clear();
        }
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
            checkpoint.attach_steering();
        }
        checkpoint.run = run;
        checkpoint.round = 0;
        checkpoint.profile = data["profile"].clone();
        checkpoint.final_text = None;
        checkpoint.guard_pause = None;
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
            let result = self.observe_pending(call);
            let result = if self.options.compact {
                compact::present(&call.name, &result, None, None)
            } else {
                result
            };
            function_output(call, &result)
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
            "run":checkpoint.run,"round":checkpoint.round,"outputs":outputs,"correction":correction,"carry_steering":true
        }}))
    }

    fn observe_pending(&self, call: &Call) -> Value {
        let records = self.journal.events.iter().rev().filter(|e| e.data["operation"] == call.operation);
        for event in records {
            match event.kind.as_str() {
                "edit_receipt" | "validation_receipt" | "exec_receipt" | "rollback_receipt" => return event.data.clone(),
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
                "rollback_intent" => return json!({"status":"error","code":"ROLLBACK_INTERRUPTED",
                    "intent_event":event.seq,"operation":call.operation,
                    "reason":"Rollback has no completed receipt. Inspect the live files."}),
                _ => {}
            }
        }
        json!({"status":"error","code":"CALL_SUPERSEDED","operation":call.operation,
            "reason":"The pending call was superseded at the start of the linked continuation."})
    }

    fn close_pending_call(
        &mut self,
        run: usize,
        round: usize,
        call: &Call,
        observer: &mut dyn RunObserver,
        guarded: bool,
        budget_hint: Option<&str>,
    ) -> Result<Value, String> {
        let attempt = self.journal.events.iter().rev()
            .find(|e| e.kind == "tool_attempt" && e.data["operation"] == call.operation)
            .map(|e| e.seq);
        let (kind, result) = if attempt.is_some() {
            let mut value = self.observe_pending(call);
            if value["code"] == "CALL_SUPERSEDED" {
                value["code"] = json!("STEER_UNCONFIRMED");
                value["reason"] = json!("The user redirected the run. This earlier attempt has no recorded outcome.");
            }
            ("tool_reconciled", value)
        } else if guarded {
            ("tool_skipped", json!({
                "status":"skipped", "code":"LOOP_GUARD_PAUSED",
                "reason":"The loop guard stopped this call before it started."
            }))
        } else {
            ("tool_skipped", json!({
                "status":"skipped", "code":"SUPERSEDED_BY_STEER",
                "reason":"The user redirected the run before this call started. Check the latest user message."
            }))
        };
        let args = self.journal.put(call.raw_args.as_bytes())?;
        let output = self.journal.store_json(&result)?;
        let mut data = json!({
            "operation":call.operation,"attempt":attempt,"round":round,"call_id":call.call_id,
            "name":call.name,"arguments":args,"output":output,"status":result["status"],"code":result["code"]
        });
        let presented = if self.options.compact {
            let text = compact::present(&call.name, &result, None, budget_hint);
            data["model_output"] = json!(self.journal.store_json(&text)?);
            text
        } else {
            result.clone()
        };
        self.journal.append(run, kind, data)?;
        observer.on_tool_call(&tool_trace(call, round, &result, 0));
        Ok(function_output(call, &presented))
    }

    fn execute(
        &mut self,
        run: usize,
        round: usize,
        call: &Call,
        observer: &mut dyn RunObserver,
        budget_hint: Option<&str>,
    ) -> Result<(Value, Option<Value>), String> {
        let args: Result<Value, _> = serde_json::from_str(&call.raw_args);
        let compact = self.options.compact;
        let action_args = args.as_ref().ok();
        let input = self.journal.put(call.raw_args.as_bytes())?;
        let attempt = self.journal.append(
            run,
            "tool_attempt",
            json!({"operation":call.operation,"round":round,
            "call_id":call.call_id,"name":call.name,"arguments":input}),
        )?;
        let started = Instant::now();
        let result = match (call.name.as_str(), args.as_ref()) {
            (_, Err(err)) => {
                json!({"status":"error","code":"INVALID_ARGUMENT","reason":err.to_string()})
            }
            ("read", Ok(args)) => tools::read(&self.ctx, &mut self.journal, run, args)?,
            ("exec", Ok(args)) if compact => exec::run(&self.ctx, &mut self.journal, run, &call.operation, args)?,
            ("rollback", Ok(args)) if compact => rollback::run(&self.ctx, &mut self.journal, run, &call.operation, args)?,
            ("research", Ok(args)) if !compact => tools::research(&self.ctx, &mut self.journal, run, Some(attempt), args)?,
            ("search_result", Ok(args)) if !compact => tools::search_result(&self.journal, args)?,
            ("edit" | "write", Ok(args)) if compact => compact::modify(
                &self.ctx, &mut self.journal, run, round, &call.operation, &call.name, args,
            )?,
            ("edit" | "write", Ok(args)) => tools::modify(
                &self.ctx,
                &mut self.journal,
                run,
                &call.operation,
                &call.name,
                self.options.edit,
                args,
            )?,
            ("validate", Ok(args)) if compact => compact::validate(
                &self.ctx, &mut self.journal, run, &call.operation, &self.options.commands, args,
            )?,
            ("validate", Ok(args)) => validation::run(
                &self.ctx,
                &mut self.journal,
                run,
                &call.operation,
                &self.options.commands,
                args,
            )?,
            _ => json!({"status":"error","code":"UNKNOWN_TOOL","reason":"Use an available tool."}),
        };
        let signal = if compact && (result["status"] == "error" || result["passed"] == false) {
            result["workspace_revision"].as_str().map(str::to_owned)
                .or_else(|| validation::workspace_version(&self.ctx, &self.journal).ok())
                .map(|workspace| guard::compact_signal(
                    &self.journal, run, round, &call.name, action_args.unwrap_or(&Value::Null),
                    &result, &workspace,
                ))
        } else {
            None
        };
        let model_output = if compact {
            let warning = signal.as_ref().filter(|signal| signal["state"] != "observe");
            Some(compact::present(&call.name, &result, warning, budget_hint))
        } else {
            self.classic_output(run, call, attempt, &result)?
        };
        let output = self.journal.store_json(&result)?;
        let elapsed = started.elapsed().as_millis();
        let mut data = json!({"operation":call.operation,"attempt":attempt,"round":round,
            "call_id":call.call_id,"name":call.name,"output":output,"status":result["status"],"code":result["code"],
            "source":result["source"],"revision":result["revision"],"result_ref":result["result_ref"],
            "elapsed_ms":elapsed});
        if let Some(output) = &model_output {
            data["model_output"] = json!(self.journal.store_json(output)?);
        }
        if compact {
            let live_read = action_args
                .filter(|args| call.name == "read" && args["result_ref"].is_null())
                .and_then(|args| args["path"].as_str());
            let observed = result["status"] == "ok" && result["source"]["kind"] == "workspace"
                && match call.name.as_str() {
                    "read" => live_read.is_some(),
                    "edit" | "write" => true,
                    _ => false,
                };
            data["live_observation"] = json!(observed);
            if result["code"] == "FILE_NOT_FOUND" {
                if let Some((_, path)) = live_read.and_then(|path| {
                    tools::workspace_path(&self.ctx, path, tools::ResolvePath::MissingParents).ok()
                }) {
                    data["observed_absent"] = json!(path);
                }
            }
            data["guard"] = json!(signal);
        }
        self.journal.append(run, "tool_result", data)?;
        let delivered = model_output.as_ref().unwrap_or(&result);
        let mut trace = tool_trace(call, round, if compact { &result } else { delivered }, elapsed);
        if let Some(text) = delivered.as_str() {
            trace.response_bytes = text.len();
        }
        observer.on_tool_call(&trace);
        Ok((function_output(call, delivered), signal.filter(|signal| signal["state"] == "pause")))
    }

    fn classic_output(&mut self, run: usize, call: &Call, attempt: usize, result: &Value) -> Result<Option<Value>, String> {
        let mut model_output = receipt::project(self.options.receipt_mode, &call.name, result);
        let presented = model_output.as_ref().unwrap_or(result);
        if json_byte_len(presented) > MAX_RESPONSE_BYTES {
            let status = presented["status"].clone();
            let code = presented.get("code").cloned();
            let artifact = self.journal.artifact(
                run,
                json!({"kind":"tool_output","operation":call.operation}),
                &serde_json::to_vec_pretty(presented).map_err(|e| e.to_string())?,
            )?;
            let mut envelope = json!({"status":status,"delivery":"folded","result_ref":artifact.result_ref,"source":artifact.source,
                          "revision":artifact.revision,"reason":"Full tool response is available through read."});
            if let Some(code) = code {
                envelope["code"] = code;
            }
            model_output = Some(envelope);
        }
        if let Some(warning) = guard::evaluate(
            &self.journal,
            run,
            &guard::PendingOutcome {
                operation: &call.operation,
                attempt,
                name: &call.name,
                result,
            },
        ) {
            let mut candidate = model_output.clone().unwrap_or_else(|| result.clone());
            if let Some(fields) = candidate.as_object_mut() {
                fields.insert("loop_warning".to_owned(), warning);
                if json_byte_len(&candidate) <= MAX_RESPONSE_BYTES {
                    model_output = Some(candidate);
                }
            }
        }
        Ok(model_output)
    }

    fn project_workspace_evidence(
        &self,
        input: &mut Value,
        prior_len: usize,
        redact: bool,
        notices: bool,
    ) -> Vec<Value> {
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
                if notices && before != current {
                    changes.push(json!({"path":path,"last_observed_revision":before,"current_revision":current}));
                }
                checked.insert(path, current);
            }
        }
        if !redact {
            return changes;
        }
        for item in items.iter_mut().take(prior_len) {
            let Some(mut output) = tool_payload(item) else {
                continue;
            };
            let mut adjusted = project_historical_source(&mut output, &checked, notices);
            if let Some(hits) = output.get_mut("hits").and_then(Value::as_array_mut) {
                for hit in hits {
                    adjusted |= project_historical_source(hit, &checked, notices);
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

fn project_historical_source(
    evidence: &mut Value, checked: &BTreeMap<String, Value>, notices: bool,
) -> bool {
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
    if notices {
        fields.insert("current_revision".to_owned(), current.clone());
    }
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
    let text = value.as_str().map(str::to_owned).unwrap_or_else(|| value.to_string());
    json!({"type":"function_call_output","call_id":call.call_id,"output":text})
}

fn tool_trace(call: &Call, turn: usize, result: &Value, duration_ms: u128) -> ToolCallTrace {
    ToolCallTrace {
        turn,
        name: call.name.clone(),
        arguments: call.raw_args.clone(),
        response_bytes: json_byte_len(result),
        status: result["status"].as_str().unwrap_or("unknown").to_owned(),
        delivery: result["delivery"].as_str().map(str::to_owned),
        result_ref: result["result_ref"].as_str().map(str::to_owned),
        error_code: result["code"].as_str().map(str::to_owned),
        duration_ms,
    }
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
        let artifact = serde_json::from_value(item.data.clone()).map_err(|err| err.to_string())?;
        return tools::inspect_artifact_content(&artifact, |hash| get_blob(&dir, hash));
    }
    if let Some(seq) = event {
        let entry = events
            .get(seq.checked_sub(1).ok_or("Event numbers start at 1")?)
            .ok_or("Unknown event")?;
        let mut output = json!(entry);
        let payload = match entry.kind.as_str() {
            "user" => entry.data["continuation"]["outputs"].as_str(),
            "tool_attempt" => entry.data["arguments"].as_str(),
            "tool_result" | "tool_skipped" | "tool_reconciled" => entry.data["output"].as_str(),
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
        if let Some(hash) = entry.data["model_output"].as_str() {
            output["model_detail"] = serde_json::from_slice(&get_blob(&dir, hash)?)
                .map_err(|err| err.to_string())?;
        }
        return serde_json::to_string_pretty(&output).map_err(|err| err.to_string());
    }
    Ok(events
        .iter()
        .map(|entry| {
            let detail = match entry.kind.as_str() {
                "user" | "steer" => entry.data["text"].as_str().unwrap_or_default().to_owned(),
                "tool_attempt" | "tool_result" | "tool_skipped" | "tool_reconciled" => format!(
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
        (event.kind == "validation_intent" && event.data["workspace_revision"] == manifest)
            || (event.kind == "user" && event.data["profile"]["tool_profile"] == "compact"
                && event.data["workspace_revision_at_start"] == manifest)
    }) {
        return Err(
            "The supplied version was not a recorded validation or run-start snapshot in this session."
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
