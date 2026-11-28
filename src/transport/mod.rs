pub mod sse;

use crate::transport::sse::{parse_sse_stream_with_observer, ParsedResponse, SseObserver, UsageMetrics};
use serde::Serialize;
use serde_json::{json, Value};
use std::fmt;
use std::io::BufReader;
use std::time::Duration;

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
            Self::Io(err) => write!(f, "IO error: {err}"),
            Self::Http(msg) => write!(f, "HTTP error: {msg}"),
            Self::Upstream(msg) => write!(f, "Upstream response error: {msg}"),
            Self::Protocol(msg) => write!(f, "SSE protocol error: {msg}"),
            Self::MaxTurnsExceeded(n) => write!(f, "Exceeded max turns ({n})"),
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
    fn default() -> Self { Self::from_env() }
}

impl GatewayConfig {
    pub fn from_env() -> Self {
        let env_or = |name, default: &str| {
            std::env::var(name).ok().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| default.to_owned())
        };
        Self {
            base_url: env_or("OPENAI_BASE_URL", "http://127.0.0.1:3425/v1"),
            api_key: env_or("OPENAI_API_KEY", "magpie"),
            model: env_or("OPENAI_MODEL", "auto/deepseek-v4.1-flash-1017"),
            reasoning_effort: Some(env_or("OPENAI_REASONING_EFFORT", "low")),
            max_turns: 50,
            timeout_secs: 120,
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

pub trait RunObserver {
    fn on_turn_start(&mut self, _turn: usize) {}
    fn on_text_delta(&mut self, _delta: &str) {}
    fn on_model_response_usage(&mut self, _usage: Option<&UsageMetrics>) {}
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
    fn on_text_delta(&mut self, delta: &str) { self.inner.on_text_delta(delta); }
}

pub struct ResponsesClient {
    config: GatewayConfig,
    agent: ureq::Agent,
}

impl ResponsesClient {
    pub fn new(config: GatewayConfig) -> Self {
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(config.timeout_secs)).build();
        Self { config, agent }
    }
    pub fn config(&self) -> &GatewayConfig { &self.config }

    pub(crate) fn request_body(&self, input: &[Value], instructions: Option<&str>, tools: &[Value]) -> Value {
        let mut body = json!({"model": self.config.model, "input": input, "tools": tools, "stream": true});
        if let Some(instructions) = instructions { body["instructions"] = json!(instructions); }
        if let Some(effort) = &self.config.reasoning_effort { body["reasoning"] = json!({"effort": effort}); }
        body
    }

    pub(crate) fn send_turn(&self, body: &Value, observer: &mut dyn RunObserver) -> Result<ParsedResponse, TransportError> {
        let url = format!("{}/responses", self.config.base_url.trim_end_matches('/'));
        let resp = self.agent.post(&url)
            .set("Authorization", &format!("Bearer {}", self.config.api_key))
            .set("Content-Type", "application/json")
            .send_string(&body.to_string())
            .map_err(|err| match err {
                ureq::Error::Status(code, r) => TransportError::Http(format!("HTTP {code}: {}", r.into_string().unwrap_or_default())),
                ureq::Error::Transport(t) => TransportError::Http(t.to_string()),
            })?;
        parse_sse_stream_with_observer(BufReader::new(resp.into_reader()), &mut ObserverSseAdapter { inner: observer })
    }
}

pub(crate) fn extract_final_text(items: &[Value]) -> String {
    items.iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("output_text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}
