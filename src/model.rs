use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, fmt, time::Duration};

pub const MODEL: &str = "opencode-go/deepseek-v4.1-flash";
pub const ENDPOINT: &str = "http://127.0.0.1:3425/v1/chat/completions";
const CAP: usize = 2 * 1024 * 1024;
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Transport,
    TimeoutBeforeHeaders,
    TimeoutReadingBody,
    Auth,
    RateLimit,
    Http(u16),
    Provider,
    Truncated,
    Protocol,
    TooLarge,
}
impl Error {
    pub fn confirmed_external(&self) -> bool {
        matches!(self, Self::RateLimit | Self::Http(504))
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
fn network(e: reqwest::Error, timeout: Error) -> Error {
    if e.is_timeout() {
        timeout
    } else {
        Error::Transport
    }
}
fn status(c: StatusCode) -> Error {
    match c.as_u16() {
        401 | 403 => Error::Auth,
        429 => Error::RateLimit,
        n => Error::Http(n),
    }
}
fn parse(bytes: &[u8]) -> Result<Value> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| Error::Protocol)?;
    if value.get("error").is_some_and(|v| !v.is_null()) {
        return Err(Error::Provider);
    }
    Ok(value)
}
fn one_choice(value: &Value) -> Result<&Value> {
    let choices = value["choices"].as_array().ok_or(Error::Protocol)?;
    if choices.len() != 1 || choices[0]["index"] != 0 {
        return Err(Error::Protocol);
    }
    Ok(&choices[0])
}
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}
fn validate(calls: &[ToolCall], finish: &str) -> Result<()> {
    if !matches!(finish, "stop" | "tool_calls") {
        return Err(if finish == "length" {
            Error::Truncated
        } else {
            Error::Protocol
        });
    }
    if (finish == "tool_calls") != !calls.is_empty() {
        return Err(Error::Protocol);
    }
    let mut seen = std::collections::HashSet::new();
    for call in calls {
        if call.id.is_empty() || call.name.is_empty() || !seen.insert(&call.id) {
            return Err(Error::Protocol);
        }
        if !serde_json::from_str::<Value>(&call.arguments).is_ok_and(|v| v.is_object()) {
            return Err(Error::Protocol);
        }
    }
    Ok(())
}
#[derive(Debug, Serialize)]
pub struct Reply {
    pub id: String,
    pub model: String,
    pub text: String,
    pub calls: Vec<ToolCall>,
    pub finish: String,
}
fn plain(data: &[u8]) -> Result<Reply> {
    let data = parse(data)?;
    let choice = one_choice(&data)?;
    let msg = &choice["message"];
    if msg["role"] != "assistant" {
        return Err(Error::Protocol);
    }
    let calls = msg["tool_calls"]
        .as_array()
        .map(|calls| {
            calls
                .iter()
                .map(|c| {
                    if c["type"] != "function" {
                        return Err(Error::Protocol);
                    }
                    Ok(ToolCall {
                        id: required(&c["id"])?.to_owned(),
                        name: required(&c["function"]["name"])?.to_owned(),
                        arguments: required(&c["function"]["arguments"])?.to_owned(),
                    })
                })
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();
    let finish = required(&choice["finish_reason"])?;
    validate(&calls, finish)?;
    Ok(Reply {
        id: required(&data["id"])?.into(),
        model: required(&data["model"])?.into(),
        text: msg["content"].as_str().unwrap_or_default().into(),
        calls,
        finish: finish.into(),
    })
}
fn required(v: &Value) -> Result<&str> {
    v.as_str().filter(|s| !s.is_empty()).ok_or(Error::Protocol)
}

#[derive(Default)]
struct Sse {
    bytes: Vec<u8>,
    data: Vec<String>,
    total: usize,
    id: String,
    model: String,
    text: String,
    calls: BTreeMap<usize, ToolCall>,
    finish: Option<String>,
    done: bool,
}
impl Sse {
    fn push(&mut self, chunk: &[u8]) -> Result<()> {
        self.total += chunk.len();
        if self.total > CAP {
            return Err(Error::TooLarge);
        }
        self.bytes.extend_from_slice(chunk);
        let mut start = 0;
        while let Some(rel) = self.bytes[start..].iter().position(|b| *b == b'\n') {
            let end = start + rel;
            let line = std::str::from_utf8(&self.bytes[start..end])
                .map_err(|_| Error::Protocol)?
                .trim_end_matches('\r')
                .to_owned();
            start = end + 1;
            if line.is_empty() {
                if !self.data.is_empty() {
                    let event = std::mem::take(&mut self.data).join("\n");
                    self.event(&event)?;
                }
            } else if let Some(value) = line.strip_prefix("data:") {
                self.data
                    .push(value.strip_prefix(' ').unwrap_or(value).into());
            }
        }
        self.bytes.drain(..start);
        Ok(())
    }
    fn event(&mut self, event: &str) -> Result<()> {
        if self.done {
            return Err(Error::Protocol);
        }
        if event == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let v = parse(event.as_bytes())?;
        if let Some(s) = v["id"].as_str() {
            if !self.id.is_empty() && self.id != s {
                return Err(Error::Protocol);
            }
            self.id = s.into();
        }
        if let Some(s) = v["model"].as_str() {
            if !self.model.is_empty() && self.model != s {
                return Err(Error::Protocol);
            }
            self.model = s.into();
        }
        if v["choices"].as_array().is_some_and(|a| a.is_empty()) {
            return Ok(());
        }
        if self.finish.is_some() {
            return Err(Error::Protocol);
        }
        let ch = one_choice(&v)?;
        let d = &ch["delta"];
        if let Some(t) = d["content"].as_str() {
            self.text.push_str(t);
        }
        if let Some(calls) = d["tool_calls"].as_array() {
            for c in calls {
                let idx = c["index"]
                    .as_u64()
                    .filter(|n| *n < 128)
                    .ok_or(Error::Protocol)? as usize;
                let entry = self.calls.entry(idx).or_default();
                if let Some(id) = c["id"].as_str().filter(|s| !s.is_empty()) {
                    if !entry.id.is_empty() && entry.id != id {
                        return Err(Error::Protocol);
                    }
                    entry.id = id.into();
                }
                if c.get("type").is_some_and(|t| t != "function") {
                    return Err(Error::Protocol);
                }
                if let Some(n) = c["function"]["name"].as_str() {
                    entry.name.push_str(n);
                }
                if let Some(a) = c["function"]["arguments"].as_str() {
                    entry.arguments.push_str(a);
                }
            }
        }
        if let Some(f) = ch["finish_reason"].as_str() {
            self.finish = Some(f.into());
        }
        Ok(())
    }
    fn finish(self) -> Result<Reply> {
        if !self.done || self.finish.is_none() || !self.bytes.is_empty() || !self.data.is_empty() {
            return Err(Error::Truncated);
        }
        let finish = self.finish.unwrap();
        if self.id.is_empty()
            || self.model.is_empty()
            || self.calls.keys().copied().ne(0..self.calls.len())
        {
            return Err(Error::Protocol);
        }
        let calls = self.calls.into_values().collect::<Vec<_>>();
        validate(&calls, &finish)?;
        Ok(Reply {
            id: self.id,
            model: self.model,
            text: self.text,
            calls,
            finish,
        })
    }
}

pub fn request(stream: bool, text: &str) -> Value {
    json!({"model": MODEL, "messages":[{"role":"user", "content":text}], "stream": stream, "max_tokens":512})
}
pub async fn send(request: &Value) -> Result<(Reply, Vec<u8>)> {
    let client = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(45))
        .build()
        .map_err(|e| network(e, Error::TimeoutBeforeHeaders))?;
    let mut res = client
        .post(ENDPOINT)
        .json(request)
        .send()
        .await
        .map_err(|e| network(e, Error::TimeoutBeforeHeaders))?;
    if !res.status().is_success() {
        return Err(status(res.status()));
    }
    let stream = request["stream"] == true;
    let mut sse = Sse::default();
    let mut raw = Vec::new();
    while let Some(chunk) = res
        .chunk()
        .await
        .map_err(|e| network(e, Error::TimeoutReadingBody))?
    {
        if raw.len() + chunk.len() > CAP {
            return Err(Error::TooLarge);
        }
        raw.extend_from_slice(&chunk);
        if stream {
            sse.push(&chunk)?;
        }
    }
    let reply = if stream { sse.finish()? } else { plain(&raw)? };
    if reply.model != MODEL {
        return Err(Error::Protocol);
    }
    Ok((reply, raw))
}

// Preserve result and tool fragments for diagnosis without persisting reasoning or credentials.
pub fn safe_wire(raw: &[u8], stream: bool) -> Result<Value> {
    fn scrub(v: &mut Value) {
        match v {
            Value::Object(m) => {
                m.retain(|k, _| {
                    !matches!(
                        k.to_ascii_lowercase().as_str(),
                        "reasoning_content" | "reasoning" | "authorization" | "api_key" | "token"
                    )
                });
                m.values_mut().for_each(scrub);
            }
            Value::Array(a) => a.iter_mut().for_each(scrub),
            _ => {}
        }
    }
    if stream {
        let text = std::str::from_utf8(raw).map_err(|_| Error::Protocol)?;
        text.replace("\r\n", "\n")
            .split("\n\n")
            .filter_map(|block| {
                let data = block
                    .lines()
                    .filter_map(|l| l.strip_prefix("data:"))
                    .map(|s| s.strip_prefix(' ').unwrap_or(s))
                    .collect::<Vec<_>>()
                    .join("\n");
                if data.is_empty() {
                    None
                } else if data == "[DONE]" {
                    Some(Ok(json!("[DONE]")))
                } else {
                    Some(parse(data.as_bytes()).map(|mut v| {
                        scrub(&mut v);
                        v
                    }))
                }
            })
            .collect::<Result<Vec<_>>>()
            .map(Value::Array)
    } else {
        let mut v = parse(raw)?;
        scrub(&mut v);
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event(delta: Value, finish: Value) -> String {
        format!(
            "data: {}\r\n\r\n",
            json!({"id":"fixture","model":MODEL,"choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
        )
    }
    #[test]
    fn m02_fragmented_utf8() {
        let wire = event(json!({"content":"你好"}), Value::Null)
            + &event(json!({"content":"·完成"}), json!("stop"))
            + "data: [DONE]\r\n\r\n";
        let mut s = Sse::default();
        for b in wire.as_bytes() {
            s.push(&[*b]).unwrap();
        }
        assert_eq!(s.finish().unwrap().text, "你好·完成");
    }
    #[test]
    fn m03_tool_arguments_are_not_available_until_complete() {
        let a = event(
            json!({"tool_calls":[{"index":0,"id":"call-fixture","type":"function","function":{"name":"add_numbers","arguments":"{\"a\":7,"}}]}),
            Value::Null,
        );
        let b = event(
            json!({"tool_calls":[{"index":0,"function":{"arguments":"\"b\":-2}"}}]}),
            json!("tool_calls"),
        );
        let mut s = Sse::default();
        s.push(a.as_bytes()).unwrap();
        assert_eq!(s.finish().unwrap_err(), Error::Truncated);
        let mut s = Sse::default();
        for c in (a + &b + "data: [DONE]\r\n\r\n").as_bytes().chunks(3) {
            s.push(c).unwrap();
        }
        assert_eq!(
            s.finish().unwrap().calls,
            vec![ToolCall {
                id: "call-fixture".into(),
                name: "add_numbers".into(),
                arguments: r#"{"a":7,"b":-2}"#.into()
            }]
        );
    }
    #[test]
    fn m04_failure_is_not_success() {
        assert_eq!(status(StatusCode::TOO_MANY_REQUESTS), Error::RateLimit);
        assert_eq!(status(StatusCode::UNAUTHORIZED), Error::Auth);
        assert_eq!(status(StatusCode::BAD_GATEWAY), Error::Http(502));
        assert_eq!(
            parse(br#"{"error":{"message":"fixture"}}"#).unwrap_err(),
            Error::Provider
        );
        let mut s = Sse::default();
        s.push(event(json!({"content":"partial"}), json!("stop")).as_bytes())
            .unwrap();
        assert_eq!(s.finish().unwrap_err(), Error::Truncated);
        let mut s = Sse::default();
        s.push((event(json!({}), json!("length")) + "data: [DONE]\n\n").as_bytes())
            .unwrap();
        assert_eq!(s.finish().unwrap_err(), Error::Truncated);
    }
    #[test]
    fn wire_does_not_save_reasoning() {
        let v = safe_wire(
            br#"{"choices":[{"message":{"reasoning_content":"SECRET","content":"OK"}}]}"#,
            false,
        )
        .unwrap();
        assert_eq!(v["choices"][0]["message"]["content"], "OK");
        assert!(!v.to_string().contains("SECRET"));
    }
}
