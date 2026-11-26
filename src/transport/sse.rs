use std::collections::BTreeMap;
use std::io::BufRead;

use serde::Serialize;
use serde_json::Value;

use crate::transport::TransportError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct UsageMetrics {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedResponse {
    pub response_id: Option<String>,
    pub output_items: Vec<Value>,
    pub usage: UsageMetrics,
    pub usage_reported: bool,
    pub response: Value,
}

/// Observer notified of incremental assistant text deltas during SSE streaming.
/// Reasoning text is intentionally never forwarded (SPEC §7.2).
pub trait SseObserver {
    fn on_text_delta(&mut self, _delta: &str) {}
}

#[derive(Debug, Default, Clone, Copy)]
pub struct NoopSseObserver;

impl SseObserver for NoopSseObserver {}

/// Parses a Responses API Server-Sent Events stream into canonical output items and usage metrics.
pub fn parse_sse_stream<R: BufRead>(reader: R) -> Result<ParsedResponse, TransportError> {
    let mut noop = NoopSseObserver;
    parse_sse_stream_with_observer(reader, &mut noop)
}

/// Parses a Responses API SSE stream while forwarding incremental assistant text deltas to `observer`.
pub fn parse_sse_stream_with_observer<R: BufRead, O: SseObserver + ?Sized>(
    mut reader: R,
    observer: &mut O,
) -> Result<ParsedResponse, TransportError> {
    let mut current_event: Option<String> = None;
    let mut data_lines: Vec<String> = Vec::new();
    let mut line = String::new();

    let mut done_items: BTreeMap<usize, Value> = BTreeMap::new();
    let mut completed_response: Option<Value> = None;

    loop {
        line.clear();
        let bytes = reader.read_line(&mut line).map_err(TransportError::Io)?;
        if bytes == 0 {
            if !data_lines.is_empty() || current_event.is_some() {
                process_sse_event(
                    current_event.take(),
                    &mut data_lines,
                    &mut done_items,
                    &mut completed_response,
                    observer,
                )?;
            }
            break;
        }

        let trimmed = line.trim_end_matches(&['\r', '\n'][..]);
        if trimmed.is_empty() {
            if !data_lines.is_empty() || current_event.is_some() {
                process_sse_event(
                    current_event.take(),
                    &mut data_lines,
                    &mut done_items,
                    &mut completed_response,
                    observer,
                )?;
            }
            continue;
        }

        if trimmed.starts_with(':') {
            continue;
        }

        if let Some(ev) = trimmed.strip_prefix("event:") {
            current_event = Some(ev.trim().to_owned());
        } else if let Some(data) = trimmed.strip_prefix("data:") {
            let payload = data.strip_prefix(' ').unwrap_or(data);
            data_lines.push(payload.to_owned());
        }
    }

    let response = completed_response.ok_or_else(|| {
        TransportError::Protocol("SSE stream ended before response.completed".to_owned())
    })?;

    let response_id = response
        .get("id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    let output_items = match response.get("output").and_then(Value::as_array) {
        Some(arr) if !arr.is_empty() => arr.clone(),
        _ => done_items.into_values().collect(),
    };

    let usage = parse_usage(response.get("usage"));
    let usage_reported = response.get("usage").is_some_and(Value::is_object);

    Ok(ParsedResponse {
        response_id,
        output_items,
        usage,
        usage_reported,
        response,
    })
}

fn process_sse_event<O: SseObserver + ?Sized>(
    event_name: Option<String>,
    data_lines: &mut Vec<String>,
    done_items: &mut BTreeMap<usize, Value>,
    completed_response: &mut Option<Value>,
    observer: &mut O,
) -> Result<(), TransportError> {
    let raw_data = data_lines.join("\n");
    data_lines.clear();

    if raw_data.is_empty() || raw_data == "[DONE]" {
        return Ok(());
    }

    let payload: Value = serde_json::from_str(&raw_data).map_err(|err| {
        TransportError::Protocol(format!("Invalid JSON in SSE event: {err}"))
    })?;

    let event_type = event_name
        .as_deref()
        .or_else(|| payload.get("type").and_then(Value::as_str))
        .unwrap_or("");

    match event_type {
        "response.output_text.delta" => {
            if let Some(delta) = payload.get("delta").and_then(Value::as_str) {
                if !delta.is_empty() {
                    observer.on_text_delta(delta);
                }
            }
        }
        "response.output_item.done" => {
            if let Some(item) = payload.get("item") {
                let idx = payload
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize)
                    .unwrap_or_else(|| done_items.len());
                done_items.insert(idx, item.clone());
            }
        }
        "response.completed" => {
            let resp = payload.get("response").cloned().unwrap_or(payload);
            *completed_response = Some(resp);
        }
        "response.failed" | "response.incomplete" | "error" => {
            return Err(TransportError::Upstream(raw_data));
        }
        _ => {}
    }

    Ok(())
}

fn parse_usage(usage_val: Option<&Value>) -> UsageMetrics {
    let Some(u) = usage_val else {
        return UsageMetrics::default();
    };
    let input_tokens = u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
    let output_tokens = u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
    let reasoning_tokens = u
        .get("output_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total_tokens = u
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(input_tokens + output_tokens);
    UsageMetrics {
        input_tokens,
        output_tokens,
        reasoning_tokens,
        total_tokens,
    }
}
