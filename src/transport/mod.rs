pub mod injection;
pub mod sse;

use std::fmt;
use std::io::BufReader;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};

use crate::registry;
use crate::store::FoldStore;
use crate::transport::sse::{parse_sse_stream_with_observer, ParsedResponse, SseObserver, UsageMetrics};

#[derive(Debug)]
pub enum TransportError {
    Io(std::io::Error),
    Http(String),
    Upstream(String),
    Protocol(String),
    MaxTurnsExceeded(usize),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::Io(err) => write!(f, "IO error: {err}"),
            TransportError::Http(msg) => write!(f, "HTTP error: {msg}"),
            TransportError::Upstream(msg) => write!(f, "Upstream response error: {msg}"),
            TransportError::Protocol(msg) => write!(f, "SSE protocol error: {msg}"),
            TransportError::MaxTurnsExceeded(n) => write!(f, "Exceeded max turns ({n})"),
        }
    }
}

impl std::error::Error for TransportError {}

#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub max_turns: usize,
    pub timeout_secs: u64,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self::from_env()
    }
}

impl GatewayConfig {
    pub fn from_env() -> Self {
        let base_url = std::env::var("OPENAI_BASE_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "http://127.0.0.1:3425/v1".to_owned());
        let api_key = std::env::var("OPENAI_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "magpie".to_owned());
        let model = std::env::var("OPENAI_MODEL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "auto/deepseek-v4.1-flash-1017".to_owned());
        let reasoning_effort = std::env::var("OPENAI_REASONING_EFFORT")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "low".to_owned());

        Self {
            base_url,
            api_key,
            model,
            reasoning_effort: Some(reasoning_effort),
            max_turns: 16,
            timeout_secs: 120,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolExecutionOutput {
    pub output_json: String,
    pub delivery: Option<&'static str>,
    pub result_ref: Option<String>,
}

/// Pluggable tool executor supplying Responses API declarations and executing calls.
pub trait ToolExecutor {
    fn tool_definitions(&self) -> Vec<Value>;
    fn dispatch(&self, name: &str, raw_args: &str) -> ToolExecutionOutput;
}

/// Executor exposing only the pure SourceHarness kernel tools (`search_result`, `read_result`).
pub struct KernelToolExecutor<'a> {
    pub store: &'a FoldStore,
    pub extra_tools: Vec<Value>,
}

impl ToolExecutor for KernelToolExecutor<'_> {
    fn tool_definitions(&self) -> Vec<Value> {
        let mut defs = registry::tool_definitions();
        defs.extend(self.extra_tools.iter().cloned());
        defs
    }

    fn dispatch(&self, name: &str, raw_args: &str) -> ToolExecutionOutput {
        ToolExecutionOutput {
            output_json: registry::dispatch(name, raw_args, self.store),
            delivery: None,
            result_ref: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallTrace {
    pub turn: usize,
    pub name: String,
    pub arguments: String,
    pub response_bytes: usize,
    pub status: String,
    pub delivery: Option<String>,
    pub result_ref: Option<String>,
    pub error_code: Option<String>,
    pub duration_ms: u128,
}

#[derive(Debug, Clone, Serialize)]
pub struct TurnMetrics {
    pub turn: usize,
    pub reasoning_item_count: usize,
    pub function_calls: Vec<String>,
    pub usage: UsageMetrics,
    pub latency_ms: u128,
}

#[derive(Debug, Clone)]
pub struct RunResult {
    pub final_text: String,
    pub turns: Vec<TurnMetrics>,
    pub total_usage: UsageMetrics,
    pub usage_complete: bool,
    pub tool_traces: Vec<ToolCallTrace>,
    pub final_input: Vec<Value>,
    pub total_latency_ms: u128,
}

/// Real-time observer for turn progress, text streaming, and tool call execution.
pub trait RunObserver {
    fn on_turn_start(&mut self, _turn: usize) {}
    fn on_text_delta(&mut self, _delta: &str) {}
    fn on_tool_call(&mut self, _trace: &ToolCallTrace) {}
    fn on_turn_complete(&mut self, _metrics: &TurnMetrics) {}
}

#[derive(Debug, Default, Clone, Copy)]
pub struct NoopRunObserver;

impl RunObserver for NoopRunObserver {}

struct ObserverSseAdapter<'a> {
    inner: &'a mut dyn RunObserver,
}

impl SseObserver for ObserverSseAdapter<'_> {
    fn on_text_delta(&mut self, delta: &str) {
        self.inner.on_text_delta(delta);
    }
}

/// Stateless Responses API client: never sends `store` or `previous_response_id`,
/// and replays verbatim `output` items (`reasoning`, `message`, `function_call`) + `function_call_output`.
pub struct ResponsesClient {
    config: GatewayConfig,
    agent: ureq::Agent,
}

impl ResponsesClient {
    pub fn new(config: GatewayConfig) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(config.timeout_secs))
            .build();
        Self { config, agent }
    }

    pub fn config(&self) -> &GatewayConfig {
        &self.config
    }

    pub(crate) fn request_body(&self, input: &[Value], instructions: Option<&str>, tools: &[Value]) -> Value {
        let mut body = json!({
            "model": self.config.model, "input": input, "tools": tools, "stream": true,
        });
        if let Some(instructions) = instructions {
            body["instructions"] = json!(instructions);
        }
        if let Some(effort) = &self.config.reasoning_effort {
            body["reasoning"] = json!({ "effort": effort });
        }
        body
    }

    pub(crate) fn send_turn(&self, body: &Value, observer: &mut dyn RunObserver) -> Result<ParsedResponse, TransportError> {
        let url = format!("{}/responses", self.config.base_url.trim_end_matches('/'));
        let resp = self.agent.post(&url)
            .set("Authorization", &format!("Bearer {}", self.config.api_key))
            .set("Content-Type", "application/json")
            .send_string(&body.to_string())
            .map_err(|err| match err {
                ureq::Error::Status(code, resp) => {
                    let text = resp.into_string().unwrap_or_default();
                    TransportError::Http(format!("HTTP {code}: {text}"))
                }
                ureq::Error::Transport(err) => TransportError::Http(err.to_string()),
            })?;
        let mut adapter = ObserverSseAdapter { inner: observer };
        parse_sse_stream_with_observer(BufReader::new(resp.into_reader()), &mut adapter)
    }

    pub fn run_loop(
        &self,
        input: Vec<Value>,
        extra_tools: Vec<Value>,
        instructions: Option<&str>,
        store: &FoldStore,
    ) -> Result<RunResult, TransportError> {
        let executor = KernelToolExecutor { store, extra_tools };
        let mut observer = NoopRunObserver;
        self.run_with_executor(input, instructions, &executor, &mut observer)
    }

    pub fn run_with_executor(
        &self,
        mut input: Vec<Value>,
        instructions: Option<&str>,
        executor: &dyn ToolExecutor,
        observer: &mut dyn RunObserver,
    ) -> Result<RunResult, TransportError> {
        let tools = executor.tool_definitions();
        let run_started = Instant::now();

        let mut turn_metrics = Vec::new();
        let mut tool_traces = Vec::new();
        let mut total_usage = UsageMetrics::default();
        let mut usage_complete = true;

        for turn in 1..=self.config.max_turns {
            observer.on_turn_start(turn);
            let turn_started = Instant::now();

            let body = self.request_body(&input, instructions, &tools);
            let parsed = self.send_turn(&body, observer)?;
            usage_complete &= parsed.usage_reported;

            let reasoning_item_count = parsed
                .output_items
                .iter()
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"))
                .count();

            total_usage.input_tokens += parsed.usage.input_tokens;
            total_usage.output_tokens += parsed.usage.output_tokens;
            total_usage.reasoning_tokens += parsed.usage.reasoning_tokens;
            total_usage.total_tokens += parsed.usage.total_tokens;

            // Replay all upstream output items (including reasoning items) verbatim into next-turn `input`.
            input.extend(parsed.output_items.iter().cloned());

            let mut call_names = Vec::new();
            let mut function_outputs = Vec::new();

            for item in &parsed.output_items {
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    let call_id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    let arguments = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("{}")
                        .to_owned();

                    call_names.push(name.clone());
                    let tool_started = Instant::now();
                    let exec_out = executor.dispatch(&name, &arguments);
                    let duration_ms = tool_started.elapsed().as_millis();

                    let parsed_out: Option<Value> = serde_json::from_str(&exec_out.output_json).ok();
                    let status = parsed_out
                        .as_ref()
                        .and_then(|v| v.get("status"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_owned();
                    let error_code = parsed_out
                        .as_ref()
                        .and_then(|v| v.get("code"))
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);

                    let trace = ToolCallTrace {
                        turn,
                        name,
                        arguments,
                        response_bytes: exec_out.output_json.len(),
                        status,
                        delivery: exec_out.delivery.map(ToOwned::to_owned),
                        result_ref: exec_out.result_ref,
                        error_code,
                        duration_ms,
                    };
                    observer.on_tool_call(&trace);
                    tool_traces.push(trace);

                    function_outputs.push(json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": exec_out.output_json,
                    }));
                }
            }

            let metrics = TurnMetrics {
                turn,
                reasoning_item_count,
                function_calls: call_names,
                usage: parsed.usage,
                latency_ms: turn_started.elapsed().as_millis(),
            };
            observer.on_turn_complete(&metrics);
            turn_metrics.push(metrics);

            if function_outputs.is_empty() {
                let final_text = extract_final_text(&parsed.output_items);
                return Ok(RunResult {
                    final_text,
                    turns: turn_metrics,
                    total_usage,
                    usage_complete,
                    tool_traces,
                    final_input: input,
                    total_latency_ms: run_started.elapsed().as_millis(),
                });
            }

            input.extend(function_outputs);
        }

        Err(TransportError::MaxTurnsExceeded(self.config.max_turns))
    }
}

pub(crate) fn extract_final_text(items: &[Value]) -> String {
    let mut parts = Vec::new();
    for item in items {
        if item.get("type").and_then(Value::as_str) == Some("message") {
            if let Some(content) = item.get("content").and_then(Value::as_array) {
                for part in content {
                    if part.get("type").and_then(Value::as_str) == Some("output_text") {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            parts.push(text.to_owned());
                        }
                    }
                }
            }
        }
    }
    parts.join("\n")
}
