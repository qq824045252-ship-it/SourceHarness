use crate::model::{self, ToolCall};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
type AnyError = Box<dyn std::error::Error>;
const NAMES: [&str; 3] = [
    "generate_nonce",
    "generate_receipt",
    "lookup_fixture_record",
];

fn schema(name: &str, desc: &str) -> Value {
    json!({"type":"function","function":{"name":name,"description":desc,
        "parameters":{"type":"object","properties":{},"required":[],"additionalProperties":false}}})
}

fn nonce() -> Result<String, AnyError> {
    let mut bytes = [0u8; 12];
    getrandom::fill(&mut bytes).map_err(|e| format!("Operating-system RNG failed: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn reply_messages(
    original: &Value,
    assistant: Value,
    results: &[(String, Value)],
) -> Result<Value, &'static str> {
    let message_calls = assistant["tool_calls"]
        .as_array()
        .ok_or("missing_tool_calls")?;
    let mut ids = std::collections::HashSet::new();
    for c in message_calls {
        let id = c["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("missing_call_id")?;
        if !ids.insert(id) {
            return Err("duplicate_call_id");
        }
    }
    if results.len() != ids.len() {
        return Err("result_count_mismatch");
    }
    let mut request = original.clone();
    let messages = request["messages"]
        .as_array_mut()
        .ok_or("missing_messages")?;
    messages.push(assistant);
    let message_calls = messages.last().unwrap()["tool_calls"].as_array().unwrap();
    let known: Vec<_> = message_calls
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_owned())
        .collect();
    let mut used = std::collections::HashSet::new();
    for (id, _) in results {
        if !known.contains(id) || !used.insert(id) {
            return Err("result_id_mismatch");
        }
    }
    for id in known {
        let result = &results.iter().find(|(i, _)| *i == id).unwrap().1;
        messages.push(json!({"role":"tool","tool_call_id":id,"content":result.to_string()}));
    }
    Ok(request)
}

fn no_arguments(call: &ToolCall) -> bool {
    serde_json::from_str::<Value>(&call.arguments)
        .ok()
        .and_then(|v| v.as_object().map(|o| o.is_empty()))
        == Some(true)
}

fn safe_request(req: &Value) -> Value {
    let mut out = req.clone();
    if let Some(messages) = out["messages"].as_array_mut() {
        for m in messages {
            if let Some(o) = m.as_object_mut() {
                o.remove("reasoning_content");
                o.remove("reasoning");
            }
        }
    }
    out
}

fn answer_matches(kind: usize, actual: &str, expected: &str) -> bool {
    let actual = actual.trim();
    if kind == 0 {
        return actual == expected;
    }
    match (actual.split_once(':'), expected.split_once(':')) {
        (Some((actual_status, actual_code)), Some((status, code))) => {
            actual_status == status && actual_code.trim_start_matches([' ', '\t']) == code
        }
        _ => false,
    }
}

async fn one(attempt: usize, kind: usize) -> Result<(bool, Value), AnyError> {
    let name = NAMES[kind];
    let desc = [
        "Generate a random nonce that cannot be known without actually using this tool.",
        "Generate a test receipt with independent accepted and receipt_code fields.",
        "Look up the configured test fixture record; returns a structured failure when missing.",
    ][kind];
    let variants=[
        ["Use the available tool to obtain a freshly generated nonce and return only its exact value.",
         "Fetch a new runtime-generated nonce from the tool; output the value alone.",
         "Ask the registered nonce generator to produce a new value. Copy only that value."],
        ["Get the test receipt using the available tool. Reply with exactly YES:<receipt_code> if accepted, NO:<receipt_code> otherwise.",
         "Call the test receipt generator. Format the result as YES:<receipt_code> for accepted, or NO:<receipt_code> for not accepted.",
         "Use the tool to produce a receipt. Copy receipt_code after YES: when accepted is true and after NO: when it is false."],
        ["Look up the fixture record using the supplied tool. If it fails, answer exactly FAILED:<error_code>.",
         "Use the lookup tool and report FAILED:<error_code> if it returns a failure.",
         "Attempt the fixture lookup. On error, give only FAILED: followed by the exact error_code."]
    ];
    let mut req = model::request(false, variants[kind][(attempt / 3) % 3]);
    req["tools"] = json!([schema(name, desc)]);
    req["tool_choice"] = json!("auto");
    let (first, raw) = match model::send(&req).await {
        Ok(x) => x,
        Err(e) => return Ok((false, json!({"request":req,"error_class":e.to_string()}))),
    };
    let wire = model::safe_wire(&raw, false).ok();
    let call = match first.calls.as_slice() {
        [c] if c.name == name && no_arguments(c) => c,
        _ => {
            return Ok((
                false,
                json!({"request":req,"first_response":wire,"error_class":"tool_selection_or_arguments"}),
            ))
        }
    };
    let secret = nonce()?;
    let accepted = attempt % 2 == 0;
    let (result, expected) = match kind {
        0 => (json!({"nonce":secret}), secret),
        1 => (
            json!({"accepted":accepted,"receipt_code":secret}),
            format!("{}:{secret}", if accepted { "YES" } else { "NO" }),
        ),
        _ => (
            json!({"ok":false,"error_code":format!("missing_{secret}")}),
            format!("FAILED:missing_{secret}"),
        ),
    };
    let original: Value = serde_json::from_slice(&raw)?;
    let assistant = original["choices"][0]["message"].clone();
    let mut follow = reply_messages(&req, assistant, &[(call.id.clone(), result.clone())])?;
    follow["tool_choice"] = json!("none");
    let safe_follow = safe_request(&follow);
    let (second, wire2) = match model::send(&follow).await {
        Ok(x) => x,
        Err(e) => {
            return Ok((
                false,
                json!({"request":req,"first_response":wire,"tool_call_id":call.id,"tool_result":result,
                                                 "followup":safe_follow,"error_class":e.to_string()}),
            ))
        }
    };
    let pass = second.calls.is_empty() && answer_matches(kind, &second.text, &expected);
    Ok((
        pass,
        json!({"request":req,"first_response":wire,"tool_call_id":call.id,"tool_result":result,
                    "followup":safe_follow,"assertion":"exact runtime value and status; separator whitespace ignored for R02/R03","response":model::safe_wire(&wire2,false).ok(),"expected":expected,"actual":second.text}),
    ))
}

pub async fn run(n: usize) -> Result<(), AnyError> {
    let epoch = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    fs::create_dir_all("evidence")?;
    let path = format!("evidence/roundtrip-{epoch}-{}.jsonl", std::process::id());
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut out = opts.open(&path)?;
    let mut attempted = [0; 3];
    let mut passed = [0; 3];
    // At most three synthetic cases at once; no automatic retries.
    for base in (0..n).step_by(3) {
        let mut jobs = tokio::task::JoinSet::new();
        for i in base..(base + 3).min(n) {
            jobs.spawn(async move {
                let start = Instant::now();
                let (pass, detail) = one(i, i % 3)
                    .await
                    .unwrap_or_else(|_| (false, json!({"error_class":"local_error"})));
                (i, pass, detail, start.elapsed().as_millis())
            });
        }
        while let Some(job) = jobs.join_next().await {
            let (i, pass, detail, ms) = job?;
            let kind = i % 3;
            attempted[kind] += 1;
            passed[kind] += usize::from(pass);
            writeln!(
                out,
                "{}",
                json!({"test_id":format!("R0{}",kind+1),"attempt_id":format!("{epoch}-{i}"),
                "status":if pass {"PASS"} else {"FAIL"},"detail":detail,"elapsed_ms":ms})
            )?;
            out.flush()?;
        }
    }
    for i in 0..3 {
        println!("R0{}: {}/{}", i + 1, passed[i], attempted[i]);
    }
    println!("Evidence: {path}");
    if (0..3).all(|i| attempted[i] > 0 && 100 * passed[i] >= 95 * attempted[i]) {
        Ok(())
    } else {
        Err("Roundtrip gate not met".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn status_and_payload_must_both_match() {
        assert!(answer_matches(1, "YES: abc", "YES:abc"));
        assert!(!answer_matches(1, "NO: abc", "YES:abc"));
        assert!(!answer_matches(2, "FAILED: xyz", "FAILED:abc"));
        assert!(!answer_matches(2, "FAILED: abc\nextra", "FAILED:abc"));
        assert!(!answer_matches(0, "ab c", "abc"));
    }
    #[test]
    fn r04_r05_match_by_id_and_reject_bad_or_duplicate_ids() {
        let request = model::request(false, "fixture");
        let c = json!({"role":"assistant","tool_calls":[{"id":"a"},{"id":"b"}]});
        let ok = reply_messages(
            &request,
            c.clone(),
            &[("b".into(), json!(2)), ("a".into(), json!(1))],
        )
        .unwrap();
        assert_eq!(ok["messages"][2]["content"], "1");
        assert_eq!(ok["messages"][3]["content"], "2");
        assert!(reply_messages(
            &request,
            c.clone(),
            &[("b".into(), json!(2)), ("x".into(), json!(1))]
        )
        .is_err());
        assert!(reply_messages(
            &request,
            c.clone(),
            &[("b".into(), json!(2)), ("b".into(), json!(1))]
        )
        .is_err());
        assert!(reply_messages(&request, c, &[("b".into(), json!(2))]).is_err());
    }
}
