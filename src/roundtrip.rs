use crate::{
    model::{self, ToolCall},
    tools,
};
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

pub(crate) fn reply_messages(
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
        messages
            .push(json!({"role":"tool","tool_call_id":id,"content":tools::agent_content(result)?}));
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

async fn one(attempt: usize, kind: usize, split_error: bool) -> Result<(bool, Value), AnyError> {
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
    let variant = (attempt / 3) % 3;
    let prompt = variants[kind][variant];
    let split_prompt = prompt.replace("<error_code>", "<code>:<ref>").replace(
        "the exact error_code.",
        "the exact code, a colon, and the exact ref.",
    );
    let mut req = model::request(false, if split_error { &split_prompt } else { prompt });
    req["tools"] = json!([schema(name, desc)]);
    req["tool_choice"] = json!("auto");
    let first_started = epoch_ms();
    let (first, raw) = match model::send(&req).await {
        Ok(x) => x,
        Err(e) => {
            return Ok((
                false,
                json!({"request":req,"error_class":e.to_string(),"external":e.confirmed_external(),"request_start_unix_ms":first_started,"request_stage":"initial"}),
            ))
        }
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
        _ if split_error => (
            slim_missing_record(&secret),
            format!("FAILED:RECORD_NOT_FOUND:{secret}"),
        ),
        _ => (
            missing_record(&secret),
            format!("FAILED:record_not_found_{secret}"),
        ),
    };
    let original: Value = serde_json::from_slice(&raw)?;
    let assistant = original["choices"][0]["message"].clone();
    let mut follow = reply_messages(&req, assistant, &[(call.id.clone(), result.clone())])?;
    follow["tool_choice"] = json!("none");
    let safe_follow = safe_request(&follow);
    let follow_started = epoch_ms();
    let (second, wire2) = match model::send(&follow).await {
        Ok(x) => x,
        Err(e) => {
            return Ok((
                false,
                json!({"request":req,"first_response":wire,"tool_call_id":call.id,"tool_result":result,
                                                 "followup":safe_follow,"error_class":e.to_string(),"external":e.confirmed_external(),"request_start_unix_ms":follow_started,"request_stage":"tool_result"}),
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

fn slim_missing_record(reference: &str) -> Value {
    json!({"ok":false,"code":"RECORD_NOT_FOUND","reason":"fixture_absent","retryable":false,"ref":reference})
}

fn missing_record(secret: &str) -> Value {
    // Keep the opaque suffix to prove the model received the actual response.
    // The deterministic reason is not derived from that suffix.
    let mut result = tools::failure(
        "RECORD_NOT_FOUND",
        "execution",
        "The configured fixture record does not exist. This tool takes no arguments.",
        None,
    );
    result["error_code"] = json!(format!("record_not_found_{secret}"));
    result
}

fn epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

pub async fn run(n: usize) -> Result<(), AnyError> {
    run_cases(n, false).await
}

pub async fn compare_error_contract(n: usize) -> Result<(), AnyError> {
    run_cases(n, true).await
}

async fn run_cases(n: usize, compare: bool) -> Result<(), AnyError> {
    let epoch = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    fs::create_dir_all("evidence")?;
    let prefix = if compare {
        "error-contract-ab"
    } else {
        "roundtrip"
    };
    let path = format!("evidence/{prefix}-{epoch}-{}.jsonl", std::process::id());
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
    let mut excluded = [0; 3];
    let mut exact_first_line = [0; 3];
    // Alternate which contract goes first in each pair. The pair shares a prompt
    // variant, but each arm gets a separately generated runtime reference.
    let total = if compare { n * 2 } else { n };
    for i in 0..total {
        let (kind, arm, case, split) = if compare {
            let pair = i / 2;
            let arm = (i % 2 + pair % 2) % 2;
            (2, arm, pair * 3 + 2, arm == 1)
        } else {
            (i % 3, i % 3, i, false)
        };
        let start = Instant::now();
        let (pass, detail) = one(case, kind, split)
            .await
            .unwrap_or_else(|_| (false, json!({"error_class":"local_error"})));
        let ms = start.elapsed().as_millis();
        let external = detail["external"] == true;
        let first_line_matches = detail["actual"]
            .as_str()
            .zip(detail["expected"].as_str())
            .is_some_and(|(a, e)| answer_matches(kind, a.trim().lines().next().unwrap_or(""), e));
        attempted[arm] += 1;
        passed[arm] += usize::from(pass);
        exact_first_line[arm] += usize::from(first_line_matches);
        excluded[arm] += usize::from(external);
        let contract = if split { "single_code_ref" } else { "legacy" };
        writeln!(
            out,
            "{}",
            json!({"test_id":format!("R0{}",kind+1),"contract":contract,
            "paired_case":compare.then_some(i/2),"exact_first_line":first_line_matches,
            "attempt_id":format!("{epoch}-{i}"),"status":if pass {"PASS"} else if external {"EXCLUDED_UPSTREAM"} else {"FAIL"},
            "detail":detail,"elapsed_ms":ms})
        )?;
        out.flush()?;
        if i + 1 < total {
            tokio::time::sleep(std::time::Duration::from_millis(850)).await;
        }
    }
    let minimum = if n >= 100 || compare && n >= 33 {
        30
    } else {
        1
    };
    let labels: &[&str] = if compare {
        &["R03 original", "R03 single code + ref"]
    } else {
        &["R01", "R02", "R03"]
    };
    for (i, label) in labels.iter().enumerate() {
        println!("{label}: {}/{} strict ({} attempted, {} confirmed upstream exclusions; {} exact first line)",
            passed[i], attempted[i] - excluded[i], attempted[i], excluded[i], exact_first_line[i]);
    }
    println!("Evidence: {path}");
    if (if compare { 1..2 } else { 0..3 }).all(|i| {
        attempted[i] - excluded[i] >= minimum
            && 100 * passed[i] >= 95 * (attempted[i] - excluded[i])
    }) {
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
    fn slim_contract_keeps_only_one_error_code_and_diagnostic_ref() {
        let r = slim_missing_record("abcd");
        let follow = reply_messages(
            &model::request(false, "fixture"),
            json!({"role":"assistant","tool_calls":[{"id":"lookup-call"}]}),
            &[("lookup-call".into(), r.clone())],
        )
        .unwrap();
        let message = &follow["messages"][2];
        assert_eq!(message["tool_call_id"], "lookup-call");
        assert_eq!(
            serde_json::from_str::<Value>(message["content"].as_str().unwrap()).unwrap(),
            r
        );
        assert_eq!(
            r,
            json!({"ok":false,"code":"RECORD_NOT_FOUND","reason":"fixture_absent","retryable":false,"ref":"abcd"})
        );
        assert!(answer_matches(
            2,
            "FAILED:RECORD_NOT_FOUND:abcd",
            "FAILED:RECORD_NOT_FOUND:abcd"
        ));
        assert!(!answer_matches(
            2,
            "FAILED:RECORD_NOT_FOUND:abcd\nextra",
            "FAILED:RECORD_NOT_FOUND:abcd"
        ));
        assert!(!answer_matches(
            2,
            "FAILED:RECORD_NOT_FOUND:wrong",
            "FAILED:RECORD_NOT_FOUND:abcd"
        ));
    }
    #[test]
    fn missing_record_cannot_be_mistaken_for_missing_argument() {
        let r = missing_record("0123");
        assert_eq!(r["error_code"], "record_not_found_0123");
        assert_eq!(r["error"]["retryable"], false);
        assert!(r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("takes no arguments"));
        assert!(!answer_matches(
            2,
            "FAILED:record_not_found_0123\nMissing parameter, retry.",
            "FAILED:record_not_found_0123"
        ));
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
