use crate::{model, roundtrip::reply_messages, tools, wire};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

type AnyError = Box<dyn std::error::Error>;
const CASES: [&str; 8] = [
    "sum",
    "search_empty",
    "repair_input",
    "ask_for_input",
    "retry_allowed",
    "retry_forbidden",
    "permission_denied",
    "unknown_check_status",
];

struct Case {
    task: &'static str,
    start: &'static str,
    input: Value,
    result: Value,
    next: Option<(&'static str, Value)>,
}

fn scenario(k: usize, operation: &str) -> Case {
    let c = |task, start, input, result, next| Case {
        task,
        start,
        input,
        result,
        next,
    };
    match k {
        0 => c("Use the calculator to add 75 and 53; answer with the sum only.", "add_numbers",
             json!({"a":75,"b":53}), Value::Null, None),
        1 => c("Find the exact SKU A-314. If there isn't an exact match, search all SKUs starting with A-.", "find_exact",
             json!({"query":"A-314"}), wire::ok(json!({"found":false})), Some(("find_prefix",json!({"prefix":"A-"})))),
        2 => c("Look up my record using automatic selection first. My record key is R-817; supply it only if the lookup needs it.", "lookup_record",
             json!({}), json!({"status":"error","code":"MISSING_ARGUMENT","field":"key"}), Some(("lookup_record",json!({"key":"R-817"})))),
        3 => c("Look up my record using automatic selection. I haven't provided a record key. Ask me if you need one.", "lookup_record",
             json!({}), json!({"status":"error","code":"MISSING_ARGUMENT","field":"key"}), None),
        4 => c("Check the service once. If the failure is temporary and safe to retry, you may try once more.", "check_service",
             json!({}), json!({"status":"error","code":"SERVICE_UNAVAILABLE","retryable":true,"retry_after_ms":100}),Some(("check_service",json!({})))),
        5 => c("Check the service exactly once. If it fails, report the result without retrying.", "check_service",
             json!({}), json!({"status":"error","code":"SERVICE_UNAVAILABLE","retryable":true,"retry_after_ms":100}),None),
        6 => c("Check whether I can access the protected fixture and report the result.", "check_access",
             json!({}), wire::error("PERMISSION_DENIED"),None),
        _ => c("Start the simulated test operation. If completion is uncertain, check the same operation's status. Do not start it again.", "start_demo_operation",
             json!({}), wire::unknown("EXECUTION_TIMEOUT",operation),Some(("check_operation",json!({"operation_id":operation})))),
    }
}

fn schema(name: &str) -> Value {
    if name == "add_numbers" {
        return tools::add_schema();
    }
    let (desc, field, required) = match name {
        "find_exact" => ("Look up an exact fixture SKU.", "query", true),
        "find_prefix" => ("Find fixture SKUs by prefix.", "prefix", true),
        "lookup_record" => (
            "Look up a fixture record; omit key for automatic selection.",
            "key",
            false,
        ),
        "check_service" => ("Check the test service.", "", false),
        "check_access" => ("Check access to the protected fixture.", "", false),
        "start_demo_operation" => ("Start the simulated fixture operation.", "", false),
        "check_operation" => (
            "Check a previously started operation.",
            "operation_id",
            true,
        ),
        _ => unreachable!("fixture registry"),
    };
    let mut properties = json!({});
    if !field.is_empty() {
        properties[field] = json!({"type":"string"});
    }
    if name == "find_prefix" {
        properties["prefix"]["description"] = json!(
            "For family-wide search, include the family's final separator and omit the last item component."
        );
    }
    json!({"type":"function","function":{"name":name,"description":desc,"parameters":{
      "type":"object","properties":properties,"required":if required {vec![field]} else {Vec::<&str>::new()},"additionalProperties":false}}})
}

fn request(c: &Case) -> Value {
    let mut req = model::request(false, c.task);
    // Controls are configuration, not task hints; the same settings apply to all cases.
    req["tool_choice"] = json!("auto");
    let mut registered = vec![schema(c.start)];
    if c.start == "find_exact" {
        registered.push(schema("find_prefix"));
    }
    if c.start == "start_demo_operation" {
        registered.push(schema("check_operation"));
    }
    req["tools"] = json!(registered);
    req
}

fn unique_arguments(input: &str) -> Result<Value, serde_json::Error> {
    use serde::de::{Deserializer, Error, MapAccess, Visitor};
    use serde::Deserialize;

    struct Unique(BTreeMap<String, Value>);
    impl<'de> Deserialize<'de> for Unique {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct Object;
            impl<'de> Visitor<'de> for Object {
                type Value = Unique;
                fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    f.write_str("a JSON object with unique argument names")
                }
                fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Unique, M::Error> {
                    let mut entries = BTreeMap::new();
                    while let Some((name, value)) = map.next_entry::<String, Value>()? {
                        if entries.insert(name, value).is_some() {
                            return Err(M::Error::custom("duplicate argument name"));
                        }
                    }
                    Ok(Unique(entries))
                }
            }
            d.deserialize_map(Object)
        }
    }
    serde_json::from_str::<Unique>(input)
        .map(|object| Value::Object(object.0.into_iter().collect()))
}

fn one_call(r: &model::Reply, name: &str, args: &Value) -> bool {
    matches!(r.calls.as_slice(), [call] if call.name==name && !call.id.trim().is_empty() &&
      unique_arguments(&call.arguments).as_ref().is_ok_and(|got| got==args))
}

fn timestamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

type Exchange = std::result::Result<(model::Reply, Value), model::Error>;
async fn send(request: &Value, phase: &str, trace: &mut Vec<Value>) -> Exchange {
    let start = Instant::now();
    let epoch = timestamp();
    match model::send(request).await {
        Ok((r, raw)) => {
            let body = serde_json::from_slice::<Value>(&raw).map_err(|_| model::Error::Protocol)?;
            trace.push(json!({"phase":phase,"request_start_unix_ms":epoch,"elapsed_ms":start.elapsed().as_millis(),
              "input_tokens":body["usage"]["prompt_tokens"].as_u64(),
              "output_tokens":body["usage"]["completion_tokens"].as_u64(),
              "reasoning_tokens":body["usage"]["completion_tokens_details"]["reasoning_tokens"].as_u64()}));
            Ok((r, body["choices"][0]["message"].clone()))
        }
        Err(e) => {
            trace.push(json!({"phase":phase,"request_start_unix_ms":epoch,"elapsed_ms":start.elapsed().as_millis(),"error":e.to_string()}));
            Err(e)
        }
    }
}

fn final_matches(k: usize, text: &str) -> bool {
    let lower = text.trim().to_ascii_lowercase();
    match k {
        0 => lower == "128",
        3 => {
            lower.contains("key")
                && (lower.contains('?')
                    || lower.contains("provide")
                    || lower.contains("need")
                    || lower.contains("share"))
        }
        5 => {
            lower.contains("unavailable")
                || (lower.contains("service")
                    && (lower.contains("failed")
                        || lower.contains("unable")
                        || lower.contains("could not")))
        }
        6 => {
            lower.contains("permission")
                || lower.contains("denied")
                || lower.contains("not have access")
        }
        _ => false,
    }
}

fn final_after_followup(k: usize, text: &str) -> bool {
    let lower = text.trim().to_ascii_lowercase();
    match k {
        1 => lower.contains("a-319"),
        2 => lower.contains("blue spruce"),
        4 => lower.contains("available") && !lower.contains("unavailable"),
        7 => {
            lower.contains("finished") || lower.contains("completed") || lower.contains("succeeded")
        }
        _ => false,
    }
}

// The catalog is a test fixture, but these two lookups really execute against it.
const CATALOG: [&str; 3] = ["A-319", "B-207", "B-319"];

fn search(call: &model::ToolCall) -> Option<Value> {
    let args = unique_arguments(&call.arguments).ok()?;
    if args.as_object()?.len() != 1 {
        return None;
    }
    match call.name.as_str() {
        "find_exact" => {
            let query = args["query"].as_str()?;
            Some(wire::ok(json!({"found":CATALOG.contains(&query)})))
        }
        "find_prefix" => {
            let prefix = args["prefix"].as_str()?;
            let matches: Vec<_> = CATALOG
                .iter()
                .filter(|sku| sku.starts_with(prefix))
                .collect();
            Some(wire::ok(json!({"matches":matches})))
        }
        _ => None,
    }
}

fn receipt(k: usize) -> Value {
    match k {
        2 => wire::ok(json!({"label":"Blue Spruce"})),
        4 => wire::ok(json!({"available":true})),
        7 => wire::ok(json!({"state":"completed"})),
        _ => unreachable!("no follow-up fixture"),
    }
}

#[derive(Default)]
struct Outcome {
    selected: bool,
    tool: bool,
    decision: bool,
    delivered: bool,
    upstream: bool,
    trace: Vec<Value>,
    observed: Value,
}

async fn evaluate(k: usize) -> Outcome {
    let mut e = Outcome::default();
    let mut nonce = [0u8; 12];
    if getrandom::fill(&mut nonce).is_err() {
        e.observed = json!({"error":"operation_id_generation_failed"});
        return e;
    }
    let id = nonce.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let c = scenario(k, &id);
    let req = request(&c);
    let (initial, assistant) = match send(&req, "initial", &mut e.trace).await {
        Ok(x) => x,
        Err(err) => {
            e.upstream = err.confirmed_external();
            return e;
        }
    };
    if !one_call(&initial, c.start, &c.input) {
        e.observed =
            json!({"stage":"initial_selection","expected":[c.start,c.input],"actual":initial});
        return e;
    }
    e.selected = true;
    let call = &initial.calls[0];
    let result = if k == 0 {
        match tools::sum_for_fixture(call) {
            Some(sum) => wire::ok(json!({"sum":sum})),
            None => return e,
        }
    } else if k == 1 {
        let Some(found) = search(call) else {
            e.observed = json!({"error":"fixture_search_rejected"});
            return e;
        };
        if found != c.result {
            e.observed =
                json!({"error":"unexpected_fixture_result","actual":found,"expected":c.result});
            return e;
        }
        found
    } else {
        c.result
    };
    let follow = match reply_messages(&req, assistant, &[(call.id.clone(), result.clone())]) {
        Ok(x) => x,
        Err(err) => {
            e.observed = json!({"error":err});
            return e;
        }
    };
    e.tool = true;
    let (next, assistant) = match send(&follow, "decision", &mut e.trace).await {
        Ok(x) => x,
        Err(err) => {
            e.upstream = err.confirmed_external();
            return e;
        }
    };
    e.observed = json!({"submitted_result":result,"reply_after_result":next});
    if let Some((name, args)) = c.next {
        e.decision = one_call(&next, name, &args) && next.calls[0].id != call.id;
        e.observed["expected_next"] = json!([name, args]);
        if !e.decision {
            return e;
        }
        if k == 4 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // Only deterministic in-memory test fixtures execute. Unknown real effects
        // would require a status query; never repeat the initial unknown operation.
        let next_result = if k == 1 {
            let Some(result) = search(&next.calls[0]) else {
                e.observed["local_error"] = json!("fixture_search_rejected");
                return e;
            };
            if result != wire::ok(json!({"matches":["A-319"]})) {
                e.observed["local_error"] = json!("unexpected_fixture_result");
                e.observed["followup_result"] = result;
                return e;
            }
            result
        } else {
            receipt(k)
        };
        e.observed["followup_result"] = next_result.clone();
        let mut last = match reply_messages(
            &follow,
            assistant,
            &[(next.calls[0].id.clone(), next_result)],
        ) {
            Ok(x) => x,
            Err(err) => {
                e.observed["local_error"] = json!(err);
                return e;
            }
        };
        last["tool_choice"] = json!("none");
        match send(&last, "delivery", &mut e.trace).await {
            Ok((reply, _)) => {
                e.delivered = reply.calls.is_empty() && final_after_followup(k, &reply.text);
                e.observed["final"] = json!(reply.text);
            }
            Err(err) => e.upstream = err.confirmed_external(),
        }
    } else {
        e.decision = next.calls.is_empty();
        e.delivered = e.decision && final_matches(k, &next.text);
    }
    e
}

pub async fn run(n: usize) -> Result<(), AnyError> {
    fs::create_dir_all("evidence")?;
    let path = format!(
        "evidence/next-call-{}-{}.jsonl",
        timestamp(),
        std::process::id()
    );
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut out = opts.open(&path)?;
    let mut tally = [[0usize; 4]; 8];
    for i in 0..8 * n {
        let k = i % 8;
        let now = Instant::now();
        let e = evaluate(k).await;
        let s = &mut tally[k];
        if !e.upstream {
            s[0] += 1;
            s[1] += usize::from(e.tool);
            s[2] += usize::from(e.decision);
            s[3] += usize::from(e.delivered);
        }
        writeln!(
            out,
            "{}",
            json!({"case":CASES[k],"sample":i/8,"external":e.upstream,
             "selected":e.selected,"tool_ok":e.tool,"decision_ok":e.decision,"delivery_ok":e.delivered,
             "elapsed_ms":now.elapsed().as_millis(),"requests":e.trace,"observed":e.observed})
        )?;
        out.flush()?;
        if i + 1 < 8 * n {
            tokio::time::sleep(Duration::from_millis(850)).await;
        }
    }
    for (label, [valid, tool, decision, delivery]) in CASES.into_iter().zip(tally) {
        println!("{label}: tool {tool}/{valid}, next {decision}/{valid}, delivery {delivery}/{valid}, excluded {}",n-valid);
    }
    println!("Evidence: {path}");
    if tally.iter().all(|[valid, tool, decision, delivery]| {
        *valid == n && tool == valid && 100 * decision >= 95 * valid && 100 * delivery >= 95 * valid
    }) {
        Ok(())
    } else {
        Err("Next-call check did not meet its gate".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reject_duplicate_arguments_even_if_last_value_matches_the_expected_call() {
        let duplicate = r#"{"prefix":"B-","prefix":"A-"}"#;
        assert_eq!(
            serde_json::from_str::<Value>(duplicate).unwrap(),
            json!({"prefix":"A-"})
        );
        assert!(unique_arguments(duplicate).is_err());
        assert!(unique_arguments(r#"{"prefix":"B-","pre\u0066ix":"A-"}"#).is_err());
        assert!(unique_arguments("[]").is_err());
        assert!(unique_arguments("{} trailing").is_err());
        assert_eq!(unique_arguments(" {} ").unwrap(), json!({}));
    }

    #[test]
    fn fixture_search_validates_and_executes_actual_arguments() {
        let search_with = |name: &str, arguments: &str| {
            search(&model::ToolCall {
                id: "fixture".into(),
                name: name.into(),
                arguments: arguments.into(),
            })
        };
        assert_eq!(
            search_with("find_exact", r#"{"query":"A-314"}"#),
            Some(wire::ok(json!({"found":false})))
        );
        assert_eq!(
            search_with("find_exact", r#"{"query":"A-319"}"#),
            Some(wire::ok(json!({"found":true})))
        );
        assert_eq!(
            search_with("find_prefix", r#"{"prefix":"A-"}"#),
            Some(wire::ok(json!({"matches":["A-319"]})))
        );
        assert_eq!(
            search_with("find_prefix", r#"{"prefix":"B-"}"#),
            Some(wire::ok(json!({"matches":["B-207","B-319"]})))
        );
        assert_eq!(
            search_with("find_prefix", r#"{"prefix":"Z-"}"#),
            Some(wire::ok(json!({"matches":[]})))
        );
        for bad in [
            "{}",
            r#"{"prefix":42}"#,
            r#"{"prefix":"A-","extra":1}"#,
            r#"{"prefix":"B-","prefix":"A-"}"#,
        ] {
            assert_eq!(search_with("find_prefix", bad), None);
        }
    }

    #[test]
    fn transient_differs_by_permission_and_unknown_only_checks_status() {
        let a = scenario(4, "id");
        let b = scenario(5, "id");
        assert_eq!(a.result, b.result);
        assert_ne!(a.task, b.task);
        assert_eq!(a.next.unwrap().0, "check_service");
        assert!(b.next.is_none());
        let c = scenario(7, "id");
        assert_eq!(c.result["status"], "unknown");
        assert_eq!(
            c.next,
            Some(("check_operation", json!({"operation_id":"id"})))
        );
        assert_eq!(scenario(1, "id").result, wire::ok(json!({"found":false})));
        assert_eq!(request(&c)["tool_choice"], "auto");
    }
}
