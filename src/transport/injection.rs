use serde_json::{json, Value};

use crate::store::FoldEnvelope;

/// Pluggable strategy for delivering a folded first-packet [`FoldEnvelope`] into Responses API `input`.
pub trait EnvelopeInjector: Send + Sync {
    fn name(&self) -> &'static str;

    fn build_initial_input(&self, user_prompt: &str, envelope: &FoldEnvelope) -> Vec<Value>;

    /// Optional read-only source tool schema if the strategy emits a synthetic `function_call`.
    fn extra_tool_definitions(&self) -> Vec<Value> {
        Vec::new()
    }
}

/// Injects the folded envelope inside a clearly delimited untrusted JSON block in the user message.
#[derive(Debug, Default, Clone, Copy)]
pub struct InlineUserMessageInjector;

impl EnvelopeInjector for InlineUserMessageInjector {
    fn name(&self) -> &'static str {
        "inline"
    }

    fn build_initial_input(&self, user_prompt: &str, envelope: &FoldEnvelope) -> Vec<Value> {
        let env_json = serde_json::to_string_pretty(envelope)
            .expect("FoldEnvelope serialization is infallible");
        let content = format!(
            "{user_prompt}\n\n[UNTRUSTED FOLDED RESULT ENVELOPE]\n```json\n{env_json}\n```\n\
Use `search_result` and `read_result` with `result_ref` = `{}` to inspect the stored content.",
            envelope.result_ref
        );
        vec![json!({
            "role": "user",
            "content": content,
        })]
    }
}

/// Injects the folded envelope as a native Responses API `function_call` + `function_call_output` pair.
#[derive(Debug, Clone)]
pub struct NativeFunctionCallInjector {
    pub call_id: String,
    pub tool_name: String,
    pub arguments_json: String,
}

impl Default for NativeFunctionCallInjector {
    fn default() -> Self {
        Self {
            call_id: "call_source_fetch_1".to_owned(),
            tool_name: "fetch_source_document".to_owned(),
            arguments_json: "{\"source\":\"workspace_input\"}".to_owned(),
        }
    }
}

impl EnvelopeInjector for NativeFunctionCallInjector {
    fn name(&self) -> &'static str {
        "native"
    }

    fn build_initial_input(&self, user_prompt: &str, envelope: &FoldEnvelope) -> Vec<Value> {
        let env_json =
            serde_json::to_string(envelope).expect("FoldEnvelope serialization is infallible");
        vec![
            json!({
                "role": "user",
                "content": user_prompt,
            }),
            json!({
                "type": "function_call",
                "id": format!("fc_{}", self.call_id),
                "call_id": self.call_id,
                "name": self.tool_name,
                "arguments": self.arguments_json,
                "status": "completed",
            }),
            json!({
                "type": "function_call_output",
                "call_id": self.call_id,
                "output": env_json,
            }),
        ]
    }

    fn extra_tool_definitions(&self) -> Vec<Value> {
        vec![json!({
            "type": "function",
            "name": self.tool_name,
            "description": "Retrieve the folded source document envelope already attached in context.",
            "parameters": {
                "type": "object",
                "properties": {
                    "source": {
                        "type": "string",
                        "description": "Source identifier."
                    }
                },
                "required": ["source"],
                "additionalProperties": false
            }
        })]
    }
}
