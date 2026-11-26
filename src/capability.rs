use crate::{model, result_pages::ResultPages, roundtrip::reply_messages};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const LOOKUP: &str = "check_fixture";
const PAGE: &str = "get_result_page";
const SOURCE: &str = "get_fixture_result";
const CASES: [&str; 4] = [
    "stop_missing",
    "ask_field",
    "retry_allowed",
    "retry_forbidden",
];

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Action {
    Stop,
    Ask,
    Retry,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Decision {
    action: Action,
    #[serde(rename = "ref")]
    reference: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    field: Option<String>,
}

fn token() -> Result<String, &'static str> {
    let mut bytes = [0u8; 12];
    getrandom::fill(&mut bytes).map_err(|_| "random_reference_unavailable")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn scenario(kind: usize, reference: &str) -> (&'static str, Value, Decision) {
    let (task, failure, action, field) = match kind {
        0 => (
            "Check once and tell me whether the record exists.",
            json!({"code":"RECORD_NOT_FOUND","retryable":false}),
            Action::Stop,
            None,
        ),
        1 => (
            "Check my record. If you need more information, ask me; do not assume it.",
            json!({"code":"MISSING_ARGUMENT","field":"customer_id","retryable":false}),
            Action::Ask,
            Some("customer_id".into()),
        ),
        2 => (
            "Check the record. If this read-only lookup temporarily fails, one retry is allowed.",
            json!({"code":"TEMPORARILY_UNAVAILABLE","retryable":true}),
            Action::Retry,
            None,
        ),
        _ => (
            "Check the record once. Do not retry under any circumstance.",
            json!({"code":"TEMPORARILY_UNAVAILABLE","retryable":true}),
            Action::Stop,
            None,
        ),
    };
    let mut failure = failure;
    failure["ok"] = json!(false);
    failure["ref"] = json!(reference);
    (
        task,
        failure,
        Decision {
            action,
            reference: reference.into(),
            field,
        },
    )
}

fn empty_schema(name: &str, description: &str) -> Value {
    json!({"type":"function","function":{"name":name,"description":description,
        "parameters":{"type":"object","properties":{},"required":[],"additionalProperties":false}}})
}

fn validated_call<'a>(reply: &'a model::Reply, name: &str) -> Option<&'a model::ToolCall> {
    match reply.calls.as_slice() {
        [c] if c.name == name
            && !c.id.trim().is_empty()
            && serde_json::from_str::<Value>(&c.arguments).ok() == Some(json!({})) =>
        {
            Some(c)
        }
        _ => None,
    }
}
fn assistant(raw: &[u8]) -> serde_json::Result<Value> {
    Ok(serde_json::from_slice::<Value>(raw)?["choices"][0]["message"].clone())
}

async fn pick(kind: usize) -> Result<(bool, Value), Box<dyn std::error::Error>> {
    let reference = token()?;
    let (task, result, expected) = scenario(kind, &reference);
    let mut req=model::request(false,&format!("{task} Use the supplied fixture tool. After seeing its result, choose your next action, but don't execute it. Return only JSON with action (stop, ask, or retry) and ref from the result. Include field only when asking for a missing input."));
    req["tools"] = json!([empty_schema(
        LOOKUP,
        "Check the test fixture. The service may ask for more input or report an error."
    )]);
    req["tool_choice"] = json!("auto");
    let (reply, raw) = match model::send(&req).await {
        Ok(x) => x,
        Err(e) => {
            return Ok((
                false,
                json!({"stage":"initial","error":e.to_string(),"upstream":e.confirmed_external()}),
            ))
        }
    };
    let Some(call) = validated_call(&reply, LOOKUP) else {
        return Ok((false, json!({"stage":"tool_selection","reply":reply})));
    };
    let mut follow = reply_messages(&req, assistant(&raw)?, &[(call.id.clone(), result.clone())])?;
    follow["tool_choice"] = json!("none");
    let (reply, _) = match model::send(&follow).await {
        Ok(x) => x,
        Err(e) => {
            return Ok((
                false,
                json!({"stage":"after_result","error":e.to_string(),"upstream":e.confirmed_external(),"tool_result":result,"expected":expected}),
            ))
        }
    };
    let actual = serde_json::from_str::<Decision>(reply.text.trim()).ok();
    let pass = reply.calls.is_empty() && actual.as_ref() == Some(&expected);
    let action_ok = actual.as_ref().is_some_and(|x| x.action == expected.action);
    let reference_ok = actual
        .as_ref()
        .is_some_and(|x| x.reference == expected.reference);
    let field_ok = actual.as_ref().is_some_and(|x| x.field == expected.field);
    Ok((
        pass,
        json!({"tool_result":result,"expected":expected,"actual":reply.text,
        "action_ok":action_ok,"ref_ok":reference_ok,"field_ok":field_ok}),
    ))
}

pub async fn run(n: usize) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all("evidence")?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let path = format!("evidence/capability-{stamp}-{}.jsonl", std::process::id());
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut out = opts.open(&path)?;
    let mut stats = [[0usize; 4]; 4];
    for i in 0..4 * n {
        // Interleave conditions, so comparisons aren't separated by time.
        let k = i % 4;
        let start = Instant::now();
        let (pass, detail) = pick(k).await.unwrap_or_else(|_| {
            (
                false,
                json!({"stage":"local","error":"test_harness_failure"}),
            )
        });
        let external = detail["upstream"] == true;
        let status = if pass {
            "PASS"
        } else if external {
            "EXCLUDED_UPSTREAM"
        } else {
            "FAIL"
        };
        stats[k][0] += 1;
        stats[k][1] += usize::from(pass);
        stats[k][2] += usize::from(external);
        stats[k][3] += usize::from(detail["action_ok"] == true);
        writeln!(
            out,
            "{}",
            json!({"case":CASES[k],
            "i":i,"status":status,"elapsed_ms":start.elapsed().as_millis(),"detail":detail})
        )?;
        out.flush()?;
        if i + 1 < 4 * n {
            tokio::time::sleep(Duration::from_millis(850)).await;
        }
    }
    for (label, [total, pass, excluded, action]) in [
        "stop_missing",
        "ask_field",
        "retry_allowed",
        "retry_forbidden",
    ]
    .into_iter()
    .zip(stats)
    {
        println!(
            "{label}: {pass}/{} strict; {action} action correct; {excluded} excluded",
            total - excluded
        );
    }
    println!("Evidence: {path}");
    if stats.iter().all(|[total, pass, excluded, _]| {
        let valid = total - excluded;
        valid >= n && 100 * pass >= 95 * valid
    }) {
        Ok(())
    } else {
        Err("Decision check did not pass; see evidence".into())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PageArgs {
    #[serde(rename = "ref")]
    reference: String,
    offset: usize,
}

pub async fn smoke_page() -> Result<(), Box<dyn std::error::Error>> {
    let mut req=model::request(false,"Use the fixture result tool. The text has a confirmation code on the page starting at byte offset 256. If the first result is paged, call the supplied page tool with that output_ref as ref and offset 256. Once you obtain the code, reply with only that code.");
    let page_schema = json!({"type":"function","function":{"name":PAGE,"description":"Get one page of this task's in-memory result.","parameters":{
        "type":"object","properties": {"ref":{"type":"string"},"offset":{"type":"integer","minimum":0}},
        "required":["ref","offset"],"additionalProperties":false}}});
    req["tools"] = json!([empty_schema(SOURCE, "Get the fixture text."), page_schema]);
    req["tool_choice"] = json!("auto");

    let (first, raw) = model::send(&req).await?;
    let call = validated_call(&first, SOURCE).ok_or("wrong initial tool call")?;
    let code = token()?;
    let mut pages = ResultPages::default();
    let first_page = pages.publish(format!(
        "{}CONFIRMATION:{}\n{}",
        "x".repeat(256),
        code,
        "z".repeat(2048)
    ));
    let reference = first_page["output_ref"]
        .as_str()
        .ok_or("missing result reference")?;
    let follow = reply_messages(
        &req,
        assistant(&raw)?,
        &[(call.id.clone(), first_page.clone())],
    )?;

    let (second, raw) = model::send(&follow).await?;
    let [page_call] = second.calls.as_slice() else {
        return Err("expected exactly one page call".into());
    };
    if page_call.name != PAGE || page_call.id == call.id {
        return Err("wrong page tool or reused call ID".into());
    }
    let args: PageArgs = serde_json::from_str(&page_call.arguments)?;
    if args.reference != reference || args.offset != 256 {
        return Err("model chose wrong result reference or offset".into());
    }
    let mut final_request = reply_messages(
        &follow,
        assistant(&raw)?,
        &[(
            page_call.id.clone(),
            pages.page(&args.reference, args.offset),
        )],
    )?;
    final_request["tool_choice"] = json!("none");
    let (reply, _) = model::send(&final_request).await?;
    if !reply.calls.is_empty() || reply.text.trim() != code {
        return Err("model did not copy the code from the later page".into());
    }
    println!("PASS: model retrieved the later in-memory page and returned its fresh code");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retry_policy_differs_only_in_user_budget() {
        let (a, first, retry) = scenario(2, "demo");
        let (b, second, stop) = scenario(3, "demo");
        assert_ne!(a, b);
        assert_eq!(first, second);
        assert_eq!(retry.action, Action::Retry);
        assert_eq!(stop.action, Action::Stop);
        assert_eq!(scenario(1, "demo").2.field.as_deref(), Some("customer_id"));
    }
}
