mod branch;
mod compact;
mod exec;
mod guard;
mod inbox;
mod journal;
mod merge;
mod rollback;
#[cfg(test)]
mod tests;
mod tools;
mod validation;

pub use branch::{branch_session, read_branch_info, BranchInfo};
pub use inbox::InputInbox;
pub use merge::{merge_worker, rebase_worker, spawn_worker, FailedCheck, MergeReport, RebaseReport, WorkerInfo};
pub use validation::Commands;

use crate::budget::{json_byte_len, truncate_utf8_bytes};
use crate::transport::sse::ParsedResponse;
use crate::transport::{extract_final_text, NoopRunObserver, ResponsesClient, RunObserver, ToolCallTrace, TransportError, TurnMetrics};
use crate::workspace::{WorkspaceConfig, WorkspaceContext};
use journal::{get_blob, read_events, revision, Journal};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct Options { pub commands: Commands }

pub struct Session {
    ctx: WorkspaceContext,
    journal: Journal,
    inbox: InputInbox,
    options: Options,
    _workspace_lock: File,
}

#[derive(Debug)]
struct Call { operation: String, call_id: String, name: String, raw_args: String }

#[derive(Default)]
struct Checkpoint {
    run: usize,
    round: usize,
    input: Vec<Value>,
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
    fn steer_events(&self) -> Vec<usize> { self.attached_steering.iter().map(|&(e, _)| e).collect() }
    fn steer_input_indices(&self) -> Vec<usize> { self.attached_steering.iter().map(|&(_, i)| i).collect() }
    fn claim_input(&mut self, data: &Value) -> Result<(), String> {
        if let Some(id) = data["inbox_id"].as_u64() {
            if !self.claimed_inputs.insert(id as usize) {
                return Err(format!("Session inbox entry {id} was already claimed"));
            }
        }
        Ok(())
    }
}

pub struct SteeringInput { pub text: String, pub received_ms: u64 }

enum Start<'a> { Prompt(&'a str), InboxPrompt(usize), Resume, Continue(Option<&'a str>) }

fn session_path(root: &Path, name: &str) -> Result<PathBuf, String> {
    if name.is_empty() || name.len() > 64 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')) {
        return Err("Session name must contain 1–64 letters, digits, underscores, or hyphens.".to_owned());
    }
    Ok(root.join(".sourceharness").join("sessions").join(name))
}

impl Session {
    pub fn open(root: &Path, name: &str, options: Options) -> Result<Self, String> {
        let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root))?;
        options.commands.check_exec_boundary(ctx.root())?;
        let dir = session_path(ctx.root(), name)?;
        for path in [ctx.root().join(".sourceharness"), ctx.root().join(".sourceharness/sessions"), dir.clone()] {
            if let Ok(on_disk) = path.canonicalize() {
                if on_disk != path { return Err("Session storage cannot be redirected".to_owned()); }
            }
        }
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let canonical = dir.canonicalize().map_err(|e| e.to_string())?;
        if !canonical.starts_with(ctx.root().join(".sourceharness")) {
            return Err("Session storage must stay inside the workspace's .sourceharness directory".to_owned());
        }
        let lock = OpenOptions::new().create(true).truncate(false).read(true).write(true)
            .open(ctx.root().join(".sourceharness/workspace.lock")).map_err(|e| e.to_string())?;
        lock.try_lock().map_err(|e| format!("Workspace already has an open SourceHarness session: {e}"))?;
        let (journal, inbox) = (Journal::open(&canonical)?, InputInbox::open(&canonical)?);
        let mut session = Self { ctx, journal, inbox, options, _workspace_lock: lock };
        tools::resume_changesets(&session.ctx, &mut session.journal)?;
        let cp = session.checkpoint()?;
        if cp.run > 0 && cp.final_text.is_none() { session.inbox.lock()?.active_run = Some(cp.run); }
        Ok(session)
    }

    pub fn is_pending(&self) -> Result<bool, String> {
        let cp = self.checkpoint()?;
        Ok(cp.run > 0 && cp.final_text.is_none())
    }

    pub fn is_loop_paused(&self) -> Result<bool, String> { Ok(self.checkpoint()?.guard_pause.is_some()) }

    pub fn run(&mut self, client: &ResponsesClient, prompt: Option<&str>) -> Result<String, String> {
        self.run_observed(client, prompt, &mut NoopRunObserver, &mut || None)
    }

    pub fn run_observed(
        &mut self, client: &ResponsesClient, prompt: Option<&str>, observer: &mut dyn RunObserver,
        poll_steering: &mut dyn FnMut() -> Option<SteeringInput>,
    ) -> Result<String, String> {
        self.run_with_start(client, prompt.map_or(Start::Resume, Start::Prompt), observer, poll_steering)
    }

    pub fn input_inbox(&self) -> InputInbox { self.inbox.clone() }

    pub fn next_inbox_prompt(&self) -> Result<Option<usize>, String> {
        let cp = self.checkpoint()?;
        if cp.run > 0 && cp.final_text.is_none() { return Ok(None); }
        Ok(self.inbox.lock()?.entries.iter().find(|e| e.steer_run.is_none() && !cp.claimed_inputs.contains(&e.id)).map(|e| e.id))
    }

    pub fn has_inbox_steering(&self) -> Result<bool, String> {
        let cp = self.checkpoint()?;
        Ok(cp.run > 0 && cp.final_text.is_none()
            && self.inbox.lock()?.entries.iter().any(|e| e.steer_run == Some(cp.run) && !cp.claimed_inputs.contains(&e.id)))
    }

    pub fn run_inbox_prompt(&mut self, client: &ResponsesClient, id: usize, observer: &mut dyn RunObserver) -> Result<String, String> {
        self.run_with_start(client, Start::InboxPrompt(id), observer, &mut || None)
    }

    pub fn continue_pending(&mut self, client: &ResponsesClient, correction: Option<&str>) -> Result<String, String> {
        self.run_with_start(client, Start::Continue(correction), &mut NoopRunObserver, &mut || None)
    }

    fn run_with_start(
        &mut self, client: &ResponsesClient, start: Start<'_>, observer: &mut dyn RunObserver,
        poll_steering: &mut dyn FnMut() -> Option<SteeringInput>,
    ) -> Result<String, String> {
        let definitions = compact::definitions();
        let mut profile = json!({
            "model": client.config().model, "effort": client.config().reasoning_effort,
            "gateway": client.config().base_url,
            "tools_revision": revision(&serde_json::to_vec(&definitions).map_err(|e| e.to_string())?),
            "validation": self.options.commands,
            "validation_programs": self.options.commands.program_revisions(),
            "workspace": self.ctx.root(),
        });
        let validation_inputs = self.options.commands.input_revision_profile();
        if !validation_inputs.is_empty() { profile["validation_inputs"] = json!(validation_inputs); }

        let mut cp = self.checkpoint()?;
        let continuation = match start {
            Start::Continue(correction) => Some(self.continuation(&cp, &profile, correction)?),
            _ => None,
        };

        let inbox = self.inbox.clone();
        let mut gate = inbox.lock()?;
        let entry = match start {
            Start::Prompt(_) | Start::InboxPrompt(_) => {
                if cp.run > 0 && cp.final_text.is_none() {
                    return Err("A run is pending. Use --resume or --continue before starting a new request.".to_owned());
                }
                Some(match start {
                    Start::Prompt(prompt) => json!({"text": prompt, "profile": profile}),
                    Start::InboxPrompt(id) => {
                        let e = gate.entries.iter().find(|e| e.id == id && e.steer_run.is_none() && !cp.claimed_inputs.contains(&id))
                            .ok_or("Session inbox message is unavailable")?;
                        json!({"text": e.text, "profile": profile, "inbox_id": id, "received_ms": e.received_ms})
                    }
                    _ => unreachable!(),
                })
            }
            Start::Resume => {
                if cp.run == 0 { return Err("No run to resume.".to_owned()); }
                if let Some(final_text) = cp.final_text { return Ok(final_text); }
                if cp.profile != profile {
                    return Err("Resume with the original model, tool definitions, and validation configuration. Use --continue to start a linked run with current conditions.".to_owned());
                }
                None
            }
            Start::Continue(_) => { self.claim_inbox_steering(&mut cp, &gate)?; continuation }
        };

        if let Some(mut data) = entry {
            data["workspace_revision_at_start"] = json!(validation::save_workspace(&self.ctx, &self.journal)?);
            let run = self.journal.events.len() + 1;
            self.journal.append(run, "user", data.clone())?;
            self.apply_user(&mut cp, run, &data)?;
        }

        let run = cp.run;
        gate.active_run = Some(run);
        drop(gate);

        let mut turn = None::<(TurnMetrics, Instant)>;
        let last_pause = self.journal.events.iter().rev().take_while(|e| e.run == run)
            .find(|e| e.kind == "paused" && e.data["reason"] == "MODEL_TURN_LIMIT")
            .and_then(|e| e.data["round"].as_u64()).unwrap_or(0) as usize;
        let turn_budget = last_pause.saturating_add(client.config().max_turns).saturating_sub(cp.round);

        for step in 0..=turn_budget {
            let mut new_pause = None;
            while !cp.pending.is_empty() {
                self.collect_steering(&mut cp, poll_steering)?;
                let guarded = cp.guard_pause.is_some() || new_pause.is_some();
                if cp.queued_steering.is_empty() && !guarded {
                    let batch_len = self.parallel_prefix_len(&cp.pending);
                    if batch_len >= 2 {
                        let batch: Vec<Call> = (0..batch_len).filter_map(|_| cp.pending.pop_front()).collect();
                        let pause = self.execute_parallel_batch(run, cp.round, &batch, &mut cp, observer, poll_steering)?;
                        if pause.is_some() { new_pause = pause; }
                        continue;
                    }
                }
                let call = cp.pending.pop_front().expect("non-empty pending");
                let output = if cp.queued_steering.is_empty() && !guarded {
                    let (output, pause) = self.execute(run, cp.round, &call, observer)?;
                    new_pause = pause;
                    output
                } else {
                    self.close_pending_call(run, cp.round, &call, observer, guarded)?
                };
                cp.input.push(output);
            }
            if let Some((mut metrics, start)) = turn.take() {
                metrics.latency_ms = start.elapsed().as_millis();
                observer.on_turn_complete(&metrics);
            }
            self.collect_steering(&mut cp, poll_steering)?;
            if let Some(details) = new_pause.filter(|_| cp.queued_steering.is_empty()) {
                self.journal.append(run, "paused", json!({"reason":"LOOP_GUARD_TRIGGERED","round":cp.round,"guard":details}))?;
                return Err("Loop guard paused this run. Use :steer with guidance in chat or --continue --prompt with a correction.".to_owned());
            }
            if step == turn_budget { break; }

            cp.attach_steering();
            let round = cp.round + 1;
            let turn_start = Instant::now();
            observer.on_turn_start(round);
            let body = client.request_body(&cp.input, Some(compact::INSTRUCTIONS), &definitions);
            let exchange = self.request(client, &cp, &body, observer);
            if exchange.is_err() { self.collect_steering(&mut cp, poll_steering)?; }
            let (request, response, elapsed_ms) = exchange?;
            cp.guard_pause = None;
            observer.on_model_response_usage(response.usage_reported.then_some(&response.usage));
            let (output, raw_response) = (self.journal.store_json(&json!(response.output_items))?, self.journal.store_json(&response.response)?);
            cp.round = round;
            let calls = match calls_from(&response.output_items, run, round) {
                Ok(calls) => calls,
                Err(error) => {
                    self.collect_steering(&mut cp, poll_steering)?;
                    self.journal.append(run, "model_rejected", json!({
                        "request":request,"round":round,"output":output,"raw_response":raw_response,"error":error,
                        "usage":response.usage_reported.then_some(response.usage),"elapsed_ms":elapsed_ms
                    }))?;
                    return Err(error);
                }
            };
            self.collect_offered_steering(&mut cp, poll_steering)?;
            let mut gate = inbox.lock()?;
            self.claim_inbox_steering(&mut cp, &gate)?;
            let final_text = (calls.is_empty() && cp.queued_steering.is_empty()).then(|| extract_final_text(&response.output_items));
            let usage = response.usage_reported.then_some(response.usage);
            self.journal.append(run, "model_response", json!({
                "request":request,"round":round,"output":output,"raw_response":raw_response,
                "response_id":response.response_id,"usage":usage,"elapsed_ms":elapsed_ms,"final_text":final_text
            }))?;
            if final_text.is_some() { gate.active_run = None; }
            drop(gate);
            let metrics = TurnMetrics {
                turn: round,
                reasoning_item_count: response.output_items.iter().filter(|i| i["type"] == "reasoning").count(),
                function_calls: calls.iter().map(|c| c.name.clone()).collect(),
                usage: response.usage,
                latency_ms: turn_start.elapsed().as_millis(),
            };
            if let Some(final_text) = final_text {
                observer.on_turn_complete(&metrics);
                return Ok(final_text);
            }
            turn = Some((metrics, turn_start));
            cp.input.extend(response.output_items);
            cp.pending = calls;
            cp.attached_steering.clear();
        }
        self.journal.append(run, "paused", json!({"reason":"MODEL_TURN_LIMIT","round":cp.round}))?;
        Err("Model turn limit reached. The run can be resumed.".to_owned())
    }

    fn request(&mut self, client: &ResponsesClient, cp: &Checkpoint, body: &Value, observer: &mut dyn RunObserver) -> Result<(usize, ParsedResponse, u128), String> {
        let payload = self.journal.store_json(body)?;
        let (run, round) = (cp.run, cp.round + 1);
        for attempt in 1..=2 {
            let mut data = json!({
                "round": round, "attempt": attempt, "input": payload, "model": client.config().model,
                "tools_revision": cp.profile["tools_revision"], "effort": client.config().reasoning_effort,
            });
            if !cp.attached_steering.is_empty() {
                data["steer_events"] = json!(cp.steer_events());
                data["steer_input_start"] = json!(cp.attached_steering[0].1);
                data["steer_input_indices"] = json!(cp.steer_input_indices());
            }
            let request = self.journal.append(run, "model_request", data)?;
            let started = Instant::now();
            match client.send_turn(body, observer) {
                Ok(response) => return Ok((request, response, started.elapsed().as_millis())),
                Err(error) => {
                    self.journal.append(run, "model_error", json!({
                        "request":request,"round":round,"error":error.to_string(),"usage":null,"elapsed_ms":started.elapsed().as_millis()
                    }))?;
                    let temporary = matches!(&error, TransportError::Http(d) if ["HTTP 429:","HTTP 502:","HTTP 503:","HTTP 504:"].iter().any(|p| d.starts_with(p)));
                    if attempt == 1 && temporary { std::thread::sleep(Duration::from_millis(500)); }
                    else { return Err(format!("{error}. The run can be resumed.")); }
                }
            }
        }
        unreachable!()
    }

    fn collect_steering(&mut self, cp: &mut Checkpoint, poll: &mut dyn FnMut() -> Option<SteeringInput>) -> Result<(), String> {
        let inbox = self.inbox.clone();
        let state = inbox.lock()?;
        self.claim_inbox_steering(cp, &state)?;
        drop(state);
        self.collect_offered_steering(cp, poll)
    }

    fn claim_inbox_steering(&mut self, cp: &mut Checkpoint, inbox: &inbox::State) -> Result<(), String> {
        for entry in &inbox.entries {
            if entry.steer_run != Some(cp.run) || cp.claimed_inputs.contains(&entry.id) { continue; }
            let data = json!({"text":entry.text,"received_ms":entry.received_ms,"after_round":cp.round,"inbox_id":entry.id});
            let event = self.journal.append(cp.run, "steer", data.clone())?;
            cp.claim_input(&data)?;
            cp.queued_steering.push((event, data["text"].clone()));
        }
        Ok(())
    }

    fn collect_offered_steering(&mut self, cp: &mut Checkpoint, poll: &mut dyn FnMut() -> Option<SteeringInput>) -> Result<(), String> {
        while let Some(input) = poll() {
            let data = json!({"text":input.text,"received_ms":input.received_ms,"after_round":cp.round});
            let event = self.journal.append(cp.run, "steer", data.clone())?;
            cp.queued_steering.push((event, data["text"].clone()));
        }
        Ok(())
    }

    fn checkpoint(&self) -> Result<Checkpoint, String> {
        let mut cp = Checkpoint::default();
        for event in &self.journal.events {
            match event.kind.as_str() {
                "user" => self.apply_user(&mut cp, event.run, &event.data)?,
                "steer" => { cp.claim_input(&event.data)?; cp.queued_steering.push((event.seq, event.data["text"].clone())); }
                "model_request" => {
                    cp.guard_pause = None;
                    cp.attach_steering();
                    if !cp.attached_steering.is_empty() && event.data["steer_events"] != json!(cp.steer_events()) {
                        return Err("Model request does not match the recorded corrections".to_owned());
                    }
                    if event.data.get("steer_input_indices").is_some() && event.data["steer_input_indices"] != json!(cp.steer_input_indices()) {
                        return Err("Model request positions do not match the recorded corrections".to_owned());
                    }
                }
                "model_response" => {
                    let output = self.journal.load_json(&event.data["output"])?;
                    let items = output.as_array().ok_or("Stored response has no output array")?;
                    cp.round = event.data["round"].as_u64().ok_or("Response has no round")? as usize;
                    cp.pending = calls_from(items, event.run, cp.round)?;
                    cp.input.extend(items.iter().cloned());
                    cp.final_text = event.data["final_text"].as_str().map(str::to_owned);
                    cp.attached_steering.clear();
                }
                "tool_result" | "tool_skipped" | "tool_reconciled" => {
                    let idx = cp.pending.iter().position(|c| event.data["operation"] == c.operation).ok_or("Receipt has no matching pending operation")?;
                    let call = cp.pending.remove(idx).ok_or("Receipt has no pending tool")?;
                    let value = self.journal.load_json(event.data.get("model_output").unwrap_or(&event.data["output"]))?;
                    cp.input.push(function_output(&call, &value));
                    if event.data["guard"]["state"] == "pause" { cp.guard_pause = Some(event.data["guard"].clone()); }
                }
                _ => {}
            }
        }
        Ok(cp)
    }

    fn apply_user(&self, cp: &mut Checkpoint, run: usize, data: &Value) -> Result<(), String> {
        cp.claim_input(data)?;
        if data["continuation"]["carry_steering"] != true { cp.attached_steering.clear(); }
        if let Some(link) = data.get("continuation") {
            let output = self.journal.load_json(&link["outputs"])?;
            let outputs = output.as_array().ok_or("Continuation has no output array")?;
            if link["run"] != cp.run || cp.final_text.is_some() || outputs.len() != cp.pending.len()
                || outputs.iter().zip(&cp.pending).any(|(o, c)| o["type"] != "function_call_output" || o["call_id"] != c.call_id)
            {
                return Err("Continuation does not match the prior pending calls.".to_owned());
            }
            cp.input.extend(outputs.iter().cloned());
            cp.attach_steering();
        }
        cp.run = run;
        cp.round = 0;
        cp.profile = data["profile"].clone();
        cp.final_text = None;
        cp.guard_pause = None;
        cp.pending.clear();
        cp.input.push(json!({"role":"user","content":data["text"]}));
        Ok(())
    }

    fn continuation(&self, cp: &Checkpoint, profile: &Value, correction: Option<&str>) -> Result<Value, String> {
        if cp.run == 0 || cp.final_text.is_some() { return Err("No unfinished run to continue.".to_owned()); }
        let outputs: Vec<Value> = cp.pending.iter().map(|c| function_output(c, &compact::present(&c.name, &self.observe_pending(c), None))).collect();
        let outputs = self.journal.store_json(&json!(outputs))?;
        let mut text = format!(
            "Continue the unfinished task from run {} under the current configuration. Pending calls from that run were reconciled from the journal and current workspace. Use the recorded outcomes and inspect the current workspace before deciding what work remains.",
            cp.run
        );
        if let Some(correction) = correction {
            text.push_str("\n\nUser correction to the original task:\n");
            text.push_str(correction);
        }
        Ok(json!({
            "text": text, "profile": profile,
            "continuation": {"run":cp.run,"round":cp.round,"outputs":outputs,"correction":correction,"carry_steering":true}
        }))
    }

    fn observe_pending(&self, call: &Call) -> Value {
        for event in self.journal.events.iter().rev().filter(|e| e.data["operation"] == call.operation) {
            match event.kind.as_str() {
                "edit_receipt" | "changeset_receipt" | "validation_receipt" | "exec_receipt" | "rollback_receipt" => return event.data.clone(),
                "edit_intent" => {
                    let path = event.data["path"].as_str().unwrap_or("");
                    let current = tools::workspace_revision(&self.ctx, path);
                    let disposition = match current.as_ref() {
                        Some(obs) if obs == &event.data["after"] => "observed_target",
                        Some(obs) if obs == &event.data["before"] => "observed_original",
                        Some(_) => "observed_different_version",
                        None => "current_version_unavailable",
                    };
                    return json!({"status":"error","code":"CONTINUATION_OBSERVATION","outcome":disposition,
                        "intent_event":event.seq,"operation":call.operation,"path":path,
                        "expected_revision":event.data["before"],"planned_revision":event.data["after"],
                        "current_revision":current,"reason":"Inspect the live file before deciding on another edit."});
                }
                "validation_intent" => return json!({"status":"error","code":"VALIDATION_INTERRUPTED",
                    "intent_event":event.seq,"operation":call.operation,"reason":"The original check has no completed receipt."}),
                "rollback_intent" => return json!({"status":"error","code":"ROLLBACK_INTERRUPTED",
                    "intent_event":event.seq,"operation":call.operation,"reason":"Rollback has no completed receipt. Inspect the live files."}),
                _ => {}
            }
        }
        json!({"status":"error","code":"CALL_SUPERSEDED","operation":call.operation,
            "reason":"The pending call was superseded at the start of the linked continuation."})
    }

    fn close_pending_call(&mut self, run: usize, round: usize, call: &Call, observer: &mut dyn RunObserver, guarded: bool) -> Result<Value, String> {
        let attempt = self.journal.events.iter().rev().find(|e| e.kind == "tool_attempt" && e.data["operation"] == call.operation).map(|e| e.seq);
        let (kind, result) = if attempt.is_some() {
            let mut value = self.observe_pending(call);
            if value["code"] == "CALL_SUPERSEDED" {
                value["code"] = json!("STEER_UNCONFIRMED");
                value["reason"] = json!("The user redirected the run. This earlier attempt has no recorded outcome.");
            }
            ("tool_reconciled", value)
        } else if guarded {
            ("tool_skipped", json!({"status":"skipped","code":"LOOP_GUARD_PAUSED","reason":"The loop guard stopped this call before it started."}))
        } else {
            ("tool_skipped", json!({"status":"skipped","code":"SUPERSEDED_BY_STEER","reason":"The user redirected the run before this call started. Check the latest user message."}))
        };
        let (args, output) = (self.journal.put(call.raw_args.as_bytes())?, self.journal.store_json(&result)?);
        let presented = compact::present(&call.name, &result, None);
        self.journal.append(run, kind, json!({
            "operation": call.operation, "attempt": attempt, "round": round, "call_id": call.call_id,
            "name": call.name, "arguments": args, "output": output,
            "model_output": self.journal.store_json(&presented)?, "status": result["status"], "code": result["code"],
        }))?;
        observer.on_tool_call(&tool_trace(call, round, &result, 0));
        Ok(function_output(call, &presented))
    }

    fn parallel_prefix_len(&self, pending: &VecDeque<Call>) -> usize {
        let mut count = 0;
        let mut has_validate = false;
        for call in pending {
            if call.name == "validate" {
                let parsed = serde_json::from_str::<Value>(&call.raw_args).ok();
                if parsed.as_ref().and_then(|v| compact::resolve_check_name(&self.options.commands, v)).is_some() {
                    has_validate = true;
                    count += 1;
                    continue;
                }
                break;
            }
            if call.name == "read" {
                let parsed = serde_json::from_str::<Value>(&call.raw_args).ok();
                if parsed.as_ref().is_some_and(|v| v["history"] != true) {
                    count += 1;
                    continue;
                }
                break;
            }
            break;
        }
        if has_validate && count >= 2 { count } else { 0 }
    }

    fn execute_parallel_batch(
        &mut self, run: usize, round: usize, batch: &[Call], cp: &mut Checkpoint,
        observer: &mut dyn RunObserver, poll_steering: &mut dyn FnMut() -> Option<SteeringInput>,
    ) -> Result<Option<Value>, String> {
        let mut prepared: Vec<Option<Result<validation::PreparedValidation, Value>>> = Vec::with_capacity(batch.len());
        for call in batch {
            if call.name == "validate" {
                let args: Value = serde_json::from_str(&call.raw_args).map_err(|e| e.to_string())?;
                let check = compact::resolve_check_name(&self.options.commands, &args).ok_or("Missing check")?.to_owned();
                prepared.push(Some(validation::prepare_detached(&self.ctx, &self.journal, &call.operation, &self.options.commands, &check)?));
            } else {
                prepared.push(None);
            }
        }
        let executed: Vec<Option<Result<Result<validation::ExecutedValidation, Value>, String>>> = std::thread::scope(|scope| {
            let handles: Vec<_> = prepared.into_iter().map(|slot| match slot {
                Some(Ok(prep)) => Some(scope.spawn(move || validation::execute_prepared(prep).map(Ok))),
                Some(Err(early)) => Some(scope.spawn(move || Ok(Err(early)))),
                None => None,
            }).collect();
            handles.into_iter().map(|h| h.map(|t| t.join().unwrap_or_else(|_| Err("Validation worker panicked".to_owned())))).collect()
        });

        let mut new_pause = None;
        for (call, slot) in batch.iter().zip(executed) {
            self.collect_steering(cp, poll_steering)?;
            let guarded = cp.guard_pause.is_some() || new_pause.is_some();
            if !cp.queued_steering.is_empty() || guarded {
                let out = self.close_pending_call(run, round, call, observer, guarded)?;
                cp.input.push(out);
                continue;
            }
            let (out, pause) = match slot {
                None => self.execute(run, round, call, observer)?,
                Some(outcome) => {
                    let args: Value = serde_json::from_str(&call.raw_args).map_err(|e| e.to_string())?;
                    let input = self.journal.put(call.raw_args.as_bytes())?;
                    let attempt = self.journal.append(run, "tool_attempt", json!({
                        "operation":call.operation,"round":round,"call_id":call.call_id,"name":call.name,"arguments":input
                    }))?;
                    let (result, elapsed) = match outcome? {
                        Ok(done) => validation::finalize_executed(&self.ctx, &mut self.journal, run, done, true)?,
                        Err(early) => (early, 0),
                    };
                    self.record_tool_result(run, round, call, attempt, Some(&args), result, elapsed, observer)?
                }
            };
            if pause.is_some() { new_pause = pause; }
            cp.input.push(out);
        }
        Ok(new_pause)
    }

    fn execute(&mut self, run: usize, round: usize, call: &Call, observer: &mut dyn RunObserver) -> Result<(Value, Option<Value>), String> {
        let args: Result<Value, _> = serde_json::from_str(&call.raw_args);
        let action_args = args.as_ref().ok();
        let input = self.journal.put(call.raw_args.as_bytes())?;
        let attempt = self.journal.append(run, "tool_attempt", json!({
            "operation":call.operation,"round":round,"call_id":call.call_id,"name":call.name,"arguments":input
        }))?;
        let started = Instant::now();
        let result = match (call.name.as_str(), args.as_ref()) {
            (_, Err(err)) => json!({"status":"error","code":"INVALID_ARGUMENT","reason":err.to_string()}),
            ("read", Ok(args)) => compact::read(&self.ctx, &mut self.journal, run, args)?,
            ("exec", Ok(args)) => exec::run(&self.ctx, &mut self.journal, run, &call.operation, args)?,
            ("rollback", Ok(args)) => rollback::run(&self.ctx, &mut self.journal, run, &call.operation, args)?,
            ("edit" | "write", Ok(args)) => compact::modify(&self.ctx, &mut self.journal, run, &call.operation, &call.name, args)?,
            ("validate", Ok(args)) => compact::validate(&self.ctx, &mut self.journal, run, &call.operation, &self.options.commands, args)?,
            _ => json!({"status":"error","code":"UNKNOWN_TOOL","reason":"Use an available tool."}),
        };
        self.record_tool_result(run, round, call, attempt, action_args, result, started.elapsed().as_millis(), observer)
    }

    #[allow(clippy::too_many_arguments)]
    fn record_tool_result(
        &mut self, run: usize, round: usize, call: &Call, attempt: usize,
        action_args: Option<&Value>, result: Value, elapsed: u128, observer: &mut dyn RunObserver,
    ) -> Result<(Value, Option<Value>), String> {
        let raw_args = action_args.unwrap_or(&Value::Null);
        let signal = guard::compact_candidate(&call.name, raw_args, &result)
            .then(|| {
                result["workspace_revision"].as_str().map(str::to_owned)
                    .or_else(|| validation::workspace_version(&self.ctx, &self.journal).ok())
                    .map(|ws| guard::compact_signal(&self.journal, run, round, &call.name, raw_args, &result, &ws))
            })
            .flatten();
        let delivered = compact::present(&call.name, &result, signal.as_ref().filter(|s| s["state"] != "observe"));
        let output = self.journal.store_json(&result)?;
        let live_read = action_args.filter(|a| call.name == "read" && a["result_ref"].is_null()).and_then(|a| a["path"].as_str());
        let observed = result["status"] == "ok" && result["source"]["kind"] == "workspace"
            && (matches!(call.name.as_str(), "edit" | "write") || (call.name == "read" && live_read.is_some()));
        let mut data = json!({
            "operation": call.operation, "attempt": attempt, "round": round, "call_id": call.call_id,
            "name": call.name, "output": output, "model_output": self.journal.store_json(&delivered)?,
            "status": result["status"], "code": result["code"], "source": result["source"],
            "revision": result["revision"], "result_ref": result["result_ref"], "elapsed_ms": elapsed,
            "live_observation": observed, "guard": signal,
        });
        if let Some(files) = result["files"].as_array() { data["live_observations"] = json!(compact::batch_observations(files)); }
        if let Some(path) = result["observed_absent"].as_str() { data["observed_absent"] = json!(path); }
        self.journal.append(run, "tool_result", data)?;
        let mut trace = tool_trace(call, round, &result, elapsed);
        if let Some(text) = delivered.as_str() { trace.response_bytes = text.len(); }
        observer.on_tool_call(&trace);
        Ok((function_output(call, &delivered), signal.filter(|s| s["state"] == "pause")))
    }
}

fn calls_from(items: &[Value], run: usize, round: usize) -> Result<VecDeque<Call>, String> {
    let mut ids = HashSet::new();
    items.iter().filter(|i| i["type"] == "function_call").enumerate().map(|(idx, item)| {
        let id = item["call_id"].as_str().filter(|s| !s.is_empty()).ok_or("Tool call has no call_id")?;
        if !ids.insert(id) { return Err("Duplicate call_id inside one response".to_owned()); }
        Ok(Call {
            operation: format!("{run}.{round}.{}", idx + 1), call_id: id.to_owned(),
            name: item["name"].as_str().ok_or("Tool call has no name")?.to_owned(),
            raw_args: item["arguments"].as_str().ok_or("Tool call has no arguments")?.to_owned(),
        })
    }).collect()
}

fn function_output(call: &Call, value: &Value) -> Value {
    let text = value.as_str().map(str::to_owned).unwrap_or_else(|| value.to_string());
    json!({"type": "function_call_output", "call_id": call.call_id, "output": text})
}

fn tool_trace(call: &Call, turn: usize, result: &Value, duration_ms: u128) -> ToolCallTrace {
    ToolCallTrace {
        turn, name: call.name.clone(), arguments: call.raw_args.clone(), response_bytes: json_byte_len(result),
        status: result["status"].as_str().unwrap_or("unknown").to_owned(),
        delivery: result["delivery"].as_str().map(str::to_owned),
        result_ref: result["result_ref"].as_str().map(str::to_owned),
        error_code: result["code"].as_str().map(str::to_owned), duration_ms,
    }
}

pub fn inspect(root: &Path, name: &str, reference: Option<&str>, event: Option<usize>) -> Result<String, String> {
    let dir = session_path(root, name)?;
    let (events, _) = read_events(&dir)?;
    if let Some(reference) = reference {
        let item = events.iter().find(|e| e.kind == "artifact" && e.data["result_ref"] == reference).ok_or("Unknown result_ref in this session")?;
        let artifact = serde_json::from_value(item.data.clone()).map_err(|e| e.to_string())?;
        return tools::inspect_artifact_content(&artifact, |hash| get_blob(&dir, hash));
    }
    if let Some(seq) = event {
        let entry = events.get(seq.checked_sub(1).ok_or("Event numbers start at 1")?).ok_or("Unknown event")?;
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
            let mut detail: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes)));
            for key in ["input", "output"] {
                if let Some(items) = detail.get_mut(key).and_then(Value::as_array_mut) {
                    for item in items.iter_mut().filter(|i| i.get("type").and_then(Value::as_str) == Some("reasoning")) {
                        *item = json!({"type":"reasoning","id":item["id"],"redacted":true});
                    }
                }
            }
            output["detail"] = detail;
        }
        if let Some(hash) = entry.data["model_output"].as_str() {
            output["model_detail"] = serde_json::from_slice(&get_blob(&dir, hash)?).map_err(|e| e.to_string())?;
        }
        return serde_json::to_string_pretty(&output).map_err(|e| e.to_string());
    }
    let mut lines: Vec<String> = Vec::new();
    if let Some(info) = branch::read_branch_info(&dir) {
        let note = info.lesson.as_deref().map(|l| format!(" lesson={l:?}")).unwrap_or_default();
        lines.push(format!("# branch from {:?} at event {} workspace={}{note}", info.parent_session, info.fork_event, &info.workspace_revision[..info.workspace_revision.len().min(12)]));
    }
    lines.extend(events.iter().map(|entry| {
        let detail = match entry.kind.as_str() {
            "user" | "steer" => entry.data["text"].as_str().unwrap_or_default().to_owned(),
            "tool_attempt" | "tool_result" | "tool_skipped" | "tool_reconciled" => format!(
                "{} {} {}", entry.data["operation"], entry.data["name"], entry.data.get("status").unwrap_or(&Value::Null)
            ),
            "model_request" | "model_response" => format!("round {}", entry.data["round"]),
            "artifact" => format!("{} {}", entry.data["result_ref"], entry.data["source"]),
            _ => entry.data.to_string(),
        };
        format!("{:>5} run {:>5} {:<20} {}", entry.seq, entry.run, entry.kind, truncate_utf8_bytes(&detail, 220).0)
    }));
    Ok(lines.join("\n"))
}

pub fn fork_snapshot(root: &Path, name: &str, manifest: &str, destination: &Path) -> Result<(), String> {
    let dir = session_path(root, name)?;
    let (events, _) = read_events(&dir)?;
    if !events.iter().any(|e| {
        (e.kind == "validation_intent" && e.data["workspace_revision"] == manifest)
            || (e.kind == "user" && e.data["workspace_revision_at_start"] == manifest)
    }) { return Err("The supplied version was not a recorded validation or run-start snapshot in this session.".to_owned()); }
    let entries: BTreeMap<String, String> = serde_json::from_slice(&get_blob(&dir, manifest)?).map_err(|e| e.to_string())?;
    for (path, hash) in &entries {
        if path.is_empty() || !tools::permitted(Path::new(path)) { return Err(format!("Unsafe snapshot path: {path}")); }
        get_blob(&dir, hash)?;
    }
    fs::create_dir(destination).map_err(|e| format!("Create a new fork directory: {e}"))?;
    for (path, hash) in entries {
        let target = destination.join(path);
        fs::create_dir_all(target.parent().ok_or("Missing fork parent")?).map_err(|e| e.to_string())?;
        fs::write(target, get_blob(&dir, &hash)?).map_err(|e| e.to_string())?;
    }
    Ok(())
}
