//! Bounded synthetic calibration/holdout. No production agent loop.
use crate::{
    fold_store::{FoldStore, View, FIRST},
    model,
    next_call::unique_arguments,
    result_pages::ResultPages,
    wire,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeSet, HashSet},
    error::Error,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    sync::Arc,
    time::Instant,
};

type AnyError = Box<dyn Error + Send + Sync>;
const MAX_ROUNDS: usize = 22;
const MAX_CALLS: usize = 20;
const MAX_TASK_TOKENS: u64 = 220_000;
const MAX_TASK_TIME: u64 = 180_000;
const SEED: u64 = 0x2026_1126_f01d_843a;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Arm {
    Full,
    Legacy,
    Plain,
    Folded,
}
impl Arm {
    fn code(self) -> &'static str {
        match self {
            Self::Full => "A",
            Self::Legacy => "B",
            Self::Plain => "C",
            Self::Folded => "D",
        }
    }
}
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }
    fn shuffle<T>(&mut self, a: &mut [T]) {
        for i in (1..a.len()).rev() {
            let j = self.next() as usize % (i + 1);
            a.swap(i, j);
        }
    }
}
fn fingerprint(s: &str) -> String {
    format!(
        "{:016x}",
        s.as_bytes()
            .iter()
            .fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(*b))
                .wrapping_mul(0x100000001b3))
    )
}
#[derive(Clone)]
struct Fixture {
    kind: usize,
    id: usize,
    prompt: String,
    raw: String,
    gold: Vec<String>,
    outcome: Value,
    evidence_lines: Vec<usize>,
    query: Option<String>,
    expected_status_lookup: bool,
}
impl Fixture {
    fn issue_key(&self) -> String {
        format!("report-{:05}", self.id)
    }
}
fn fixture(seed: u64, kind: usize, n: usize) -> Fixture {
    let id = n * 10 + kind;
    let mut rng = Rng(seed ^ (id as u64).wrapping_mul(0xdb4f0b9175ae2165));
    let key = format!("report-{id:05}");
    let a = format!("K{:08X}", rng.next() as u32);
    let b = format!("M{:08X}", rng.next() as u32);
    let query = format!("LOT-{:06X}", rng.next() as u32 & 0xffffff);
    let mut f = Fixture {
        kind,
        id,
        prompt: String::new(),
        raw: String::new(),
        gold: vec![a.clone()],
        outcome: wire::ok(json!({"fixture":key,"exit_code":0})),
        evidence_lines: vec![],
        query: None,
        expected_status_lookup: false,
    };
    if kind == 0 {
        f.raw = format!("## Tiny report\nverified calibration reference: {a}\n");
        f.prompt=format!("Open report {key} and tell me the verified calibration reference. No need for surrounding detail.");
        return f;
    }
    if kind == 7 {
        if n % 2 == 0 {
            f.outcome = wire::unknown("EXECUTION_TIMEOUT", &format!("OP{:08X}", rng.next() as u32));
            f.prompt=format!("Check report {key}. If execution may already have happened, check its existing status and tell me its receipt. Never run it a second time.");
            f.expected_status_lookup = true;
        } else {
            f.outcome = wire::error("PERMISSION_DENIED");
            f.gold = vec!["denied".into()];
            f.prompt=format!("Open report {key}. If access is denied, report that and stop. Do not try another route.");
        }
        return f;
    }
    if kind == 9 {
        f.raw = "synthetic large report row\r\n".repeat(2600);
        f.gold.clear();
        f.prompt=format!("Try to open report {key} and find the hidden checksum. If the report contents were not retained, say you cannot recover the checksum. Do not reopen or invent it.");
        return f;
    }
    // Vary document size and target placement. Data contains no expected tool arguments.
    let size = [4096usize, 16384, 65536][n % 3];
    let records = [54usize, 219, 835][n % 3];
    let mut sections = [
        "Setup Notes",
        "Release Review",
        "Dispatch Register",
        "Legacy Archive",
        "Calibration Queue",
        "Inventory Snapshots",
    ];
    rng.shuffle(&mut sections);
    let mut lines = Vec::new();
    let mut positions = vec![];
    for section in sections {
        lines.push(format!("## {section}"));
        let pick = (rng.next() as usize % (records / 6 - 2)) + 1;
        for j in 0..records / 6 {
            let raw = match (kind, section, j == pick) {
                (1, "Calibration Queue", true) => {
                    Some(format!("Verified calibration reference = {a}"))
                }
                (2 | 8, "Dispatch Register", true) => {
                    Some(format!("shipment {query} approved reference {a}"))
                }
                (2 | 8, "Legacy Archive", true) => {
                    Some(format!("shipment {query}-OLD cancelled reference {b}"))
                }
                (3, "Release Review", true) => Some(format!(
                    "経理承認 / approbation comptable / payable final reconciliation = {a}"
                )),
                (3, "Legacy Archive", true) => Some(format!(
                    "Earlier finance approval candidate = {b} (obsolete)"
                )),
                (4, "Dispatch Register", true) => {
                    Some(format!("confirmed shipping reference = {a}"))
                }
                (4, "Calibration Queue", true) => {
                    Some(format!("confirmed calibration reference = {b}"))
                }
                (6, "Dispatch Register", true) => {
                    Some(format!("shipment {query}-OLD cancelled reference {b}"))
                }
                _ => None,
            };
            if let Some(raw) = raw {
                if !raw.contains("OLD") && !raw.contains("obsolete") {
                    positions.push(lines.len() + 1);
                }
                lines.push(raw);
            } else if kind == 5 {
                lines.push(format!(
                    "card {:08x}: {}",
                    rng.next() as u32,
                    if rng.next() % 19 == 0 {
                        "REVIEW"
                    } else {
                        "CLEARED"
                    }
                ));
            } else {
                let pad = if j % 7 == 0 {
                    "仪表检修🙂 / shift journal"
                } else {
                    "routine equipment and account reconciliation"
                };
                lines.push(format!(
                    "row {:08x}: {pad} // {}",
                    rng.next() as u32,
                    rng.next() as u32
                ));
            }
        }
    }
    let used: usize = lines.iter().map(|l| l.len() + 1).sum();
    if used < size {
        let extra = size - used;
        // Fill with realistic-length rows, not one giant pathological trailing line.
        let line = "miscellaneous shift note; no decision recorded";
        for _ in 0..extra / (line.len() + 1) {
            lines.push(line.into());
        }
        let rem = extra % (line.len() + 1);
        if rem > 1 {
            lines.push(".".repeat(rem - 1));
        }
    }
    f.raw = lines.join("\n") + "\n";
    assert!(f.raw.len() <= 65_536, "fixture size {}", f.raw.len());
    f.evidence_lines = positions;
    f.prompt=match kind {
        1=>format!("Open {key}. At line {}, what is the verified calibration reference? Give the reference.",f.evidence_lines[0]),
        2|8=>{f.query=Some(query.clone());format!("Open {key}. What is the approved reference for the exact shipment {query}? An ID ending in -OLD is a different shipment.")},
        3=>format!("Open {key}. In the Release Review, give the current finance approval reference. The archive is not current."),
        4=>{f.gold.push(b); format!("Open {key}. Give both confirmed references: the shipping reference and the calibration reference.")},
        5=>{
            f.gold=vec![f.raw.lines().filter(|l|l.ends_with(": REVIEW")).count().to_string()];
            f.query=Some(": REVIEW".into());
            format!("Open {key}. Across the entire report, how many cards are marked REVIEW? Give the total count, without listing individual IDs. Check for further matches before concluding.")
        }
        6=>{
            f.gold=vec!["not found".into()];
            f.query=Some(query.clone());
            format!("Open {key}. Is there a record for the exact shipment {query}? Do not count a suffixed ID. If there's no exact record, say not found.")
        }
        _=>unreachable!()
    };
    if kind == 2 || kind == 8 {
        // A known-keyword task does not need to read every byte.
        f.evidence_lines
            .retain(|i| f.raw.lines().nth(i - 1).is_some_and(|l| l.contains(&a)));
    }
    f
}
fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({"type":"function","function":{"name":name,"description":description,"parameters":{"type":"object","properties":properties,"required":required,"additionalProperties":false}}})
}
fn tools(arm: Arm) -> Value {
    let integer = json!({"type":"integer","minimum":0});
    let string = json!({"type":"string"});
    let open = tool(
        "open_report",
        "Open one synthetic report by its exact ID. Do not reopen just to read more.",
        json!({"report":string}),
        &["report"],
    );
    let status = tool(
        "check_report_status",
        "Read the receipt of one existing operation.",
        json!({"operation_id":string}),
        &["operation_id"],
    );
    if arm == Arm::Legacy {
        return json!([
            open,
            status,
            tool(
                "get_result_page",
                "Read the next 128-byte chunk using the output_ref and next_offset.",
                json!({"output_ref":string,"offset":integer}),
                &["output_ref", "offset"]
            )
        ]);
    }
    json!([open,status,
        tool("read_result","Read raw lines of an existing report (1-based). If next_offset is present continue the same range with it; otherwise the requested range is complete.",json!({"ref":string,"start_line":{"type":"integer","minimum":1},"line_count":{"type":"integer","minimum":1,"maximum":4096},"offset":integer}),&["ref","start_line","line_count"]),
        tool("search_result","Case-sensitive literal search in an existing report, one hit per line. Limit 1 to 20. Continue with the same query and next_line if present.",json!({"ref":string,"query":{"type":"string","minLength":1,"maxLength":128},"start_line":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1,"maximum":20}}),&["ref","query"])])
}
fn assistant_calls(r: &model::Reply) -> Value {
    json!({"role":"assistant","content":if r.text.is_empty(){Value::Null}else{json!(r.text)},
        "tool_calls":r.calls.iter().map(|c|json!({"id":c.id,"type":"function","function":{"name":c.name,"arguments":c.arguments}})).collect::<Vec<_>>()})
}
fn insert_result(messages: &mut Vec<Value>, call: &str, body: &Value) {
    messages.push(json!({"role":"tool","tool_call_id":call,"content":body.to_string()}));
}
fn publish(store: &mut FoldStore, old: &mut ResultPages, f: &Fixture, a: Arm) -> Value {
    if a == Arm::Legacy && !f.raw.is_empty() {
        let mut out = old.publish(f.raw.clone());
        if out.get("result_status").is_none() && out.get("output_ref").is_none() {
            return f.outcome.clone().as_object().map(|_|json!({"status":"ok","data":f.outcome["data"],"output":out["data"]["text"]})).unwrap();
        }
        out["core"] = f.outcome["data"].clone();
        return out;
    }
    store.present(
        f.outcome.clone(),
        f.raw.clone(),
        match a {
            Arm::Full => View::Full,
            Arm::Plain => View::Plain,
            _ => View::Folded,
        },
    )
}
fn metric(v: &Value, key: &str) -> Option<u64> {
    v["usage"][key].as_u64()
}
fn result_range(value: &Value) -> Option<(usize, usize)> {
    let data = &value["data"];
    Some((
        data["offset"].as_u64()? as usize,
        data["end_offset"].as_u64()? as usize,
    ))
}
fn complete(raw: usize, intervals: &[(usize, usize)]) -> bool {
    let mut v = intervals.to_vec();
    v.sort_unstable();
    v.into_iter()
        .try_fold(0, |end, (a, b)| (a <= end).then_some(end.max(b)))
        .is_some_and(|e| e >= raw)
}
fn add_initial_coverage(first: &Value, covered: &mut Vec<(usize, usize)>) {
    let snippet = first["output"]
        .as_str()
        .or_else(|| first["data"]["chunk"].as_str())
        .or_else(|| first["result"]["preview"].as_str())
        .or_else(|| first["result"]["output"].as_str());
    if let Some(s) = snippet {
        covered.push((0, s.len()));
    }
}
fn mark_lines(f: &Fixture, covered: &[(usize, usize)], lines: &mut BTreeSet<usize>) {
    let mut start = 0;
    for (i, l) in f.raw.split_inclusive('\n').enumerate() {
        let end = start + l.len();
        if f.evidence_lines.contains(&(i + 1)) {
            let clipped = covered
                .iter()
                .filter_map(|(a, b)| {
                    let lo = (*a).max(start);
                    let hi = (*b).min(end);
                    (lo < hi).then(|| (lo - start, hi - start))
                })
                .collect::<Vec<_>>();
            if complete(l.len(), &clipped) {
                lines.insert(i + 1);
            }
        }
        start = end;
    }
}
fn answer_matches(kind: usize, gold: &[String], response: &str) -> bool {
    let t = response.to_ascii_lowercase();
    if kind == 5 {
        let numbers = t
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter_map(|s| s.parse::<usize>().ok())
            .collect::<Vec<_>>();
        return !numbers.is_empty()
            && gold[0]
                .parse::<usize>()
                .is_ok_and(|expected| numbers.iter().all(|v| *v == expected));
    }
    if kind == 9 {
        let absence = [
            "not retained",
            "wasn't retained",
            "not recover",
            "cannot recover",
            "can't recover",
            "not available",
            "unavailable",
            "not be retrieved",
            "could not",
            "unable to",
            "can't be recovered",
            "no access",
        ]
        .iter()
        .any(|x| t.contains(x));
        return absence
            && !t.split(|c: char| !c.is_ascii_alphanumeric()).any(|s| {
                s.len() == 9 && s.starts_with('k') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
            });
    }
    let rigid = ["not found", "denied"];
    gold.iter().all(|g| {
        let low = g.to_lowercase();
        if rigid.contains(&g.as_str()) {
            if g == "not found" {
                return [
                    "not found",
                    "no exact",
                    "no record",
                    "does not exist",
                    "could not find",
                    "couldn't find",
                ]
                .iter()
                .any(|p| t.contains(p));
            }
            return t.contains("denied")
                || t.contains("permission")
                || t.contains("not authorized");
        }
        t.split(|c: char| !c.is_ascii_alphanumeric())
            .any(|s| s == low)
    })
}

async fn task(f: &Fixture, a: Arm, natural: bool) -> Value {
    let started = Instant::now();
    let mut messages = vec![json!({"role":"user","content":f.prompt})];
    let defs = tools(a);
    let mut store = FoldStore::default();
    let mut old = ResultPages::default();
    let mut first = Value::Null;
    let mut opened = false;
    let mut calls = Vec::new();
    let mut rounds = 0;
    let (mut input, mut output, mut cached, mut cache_missing, mut missing) = (0, 0, 0, 0, 0);
    let mut used_ids = HashSet::new();
    let mut covered = Vec::new();
    let mut lines = BTreeSet::new();
    let mut searched = false;
    let mut query_complete = false;
    let mut expected_search_line = Some(1);
    let mut coverage_query: Option<String> = None;
    let mut checked = false;
    let mut response = String::new();
    let mut indexing_us = 0u128;
    let mut before_target_ms = None;
    let mut req_ms = Vec::new();
    let mut status = "round_budget";
    let mut upstream = None;
    if !natural {
        let c = format!("initial_{}", f.id);
        used_ids.insert(c.clone());
        messages.push(json!({"role":"assistant","content":null,"tool_calls":[{"type":"function","id":c,"function":{"name":"open_report","arguments":json!({"report":f.issue_key()}).to_string()}}]}));
        let idx = Instant::now();
        first = publish(&mut store, &mut old, f, a);
        indexing_us = idx.elapsed().as_micros();
        add_initial_coverage(&first, &mut covered);
        mark_lines(f, &covered, &mut lines);
        if !f.evidence_lines.is_empty() && f.evidence_lines.iter().all(|i| lines.contains(i)) {
            before_target_ms = Some(started.elapsed().as_millis());
        }
        insert_result(&mut messages, &c, &first);
        opened = true;
    }
    'outer: for _ in 0..MAX_ROUNDS {
        if matches!(a, Arm::Plain | Arm::Folded) && first.to_string().len() > FIRST {
            status = "first_budget";
            break;
        }
        if input + output >= MAX_TASK_TOKENS
            || started.elapsed().as_millis() >= u128::from(MAX_TASK_TIME)
        {
            status = "task_budget";
            break;
        }
        rounds += 1;
        let request = json!({"model":model::MODEL,"messages":messages,"tools":defs,"tool_choice":"auto","stream":false,"max_tokens":4096});
        let net = Instant::now();
        let remaining = MAX_TASK_TIME.saturating_sub(started.elapsed().as_millis() as u64);
        let reply = tokio::time::timeout(
            std::time::Duration::from_millis(remaining),
            model::send(&request),
        )
        .await;
        req_ms.push(net.elapsed().as_millis());
        let (r, raw) = match reply {
            Err(_) => {
                status = "task_budget";
                break;
            }
            Ok(Ok(x)) => x,
            Ok(Err(e)) => {
                status = if e.confirmed_external() {
                    "confirmed_upstream"
                } else {
                    "request_error"
                };
                upstream = Some(e.to_string());
                break;
            }
        };
        let v: Value = serde_json::from_slice(&raw).unwrap_or_default();
        if let (Some(i), Some(o)) = (metric(&v, "prompt_tokens"), metric(&v, "completion_tokens")) {
            input += i;
            output += o;
        } else {
            missing += 1;
        }
        let cache = v["usage"]["prompt_tokens_details"]["cached_tokens"]
            .as_u64()
            .or_else(|| metric(&v, "prompt_cache_hit_tokens"));
        if let Some(c) = cache {
            cached += c;
        } else {
            cache_missing += 1;
        }
        if input + output > MAX_TASK_TOKENS {
            status = "task_budget";
            break;
        }
        if r.calls.is_empty() {
            response = r.text;
            if !opened {
                status = "missing_initial_call";
                break;
            }
            let full = a == Arm::Full && f.kind != 9;
            let evidence = match f.kind {
                0 => calls.len() == usize::from(natural),
                5 => full || complete(f.raw.len(), &covered) || query_complete,
                6 => full || complete(f.raw.len(), &covered) || searched,
                7 => !f.expected_status_lookup || checked,
                9 => true,
                _ => full || f.evidence_lines.iter().all(|i| lines.contains(i)),
            };
            status = if answer_matches(f.kind, &f.gold, &response) && evidence {
                "pass"
            } else if !evidence && answer_matches(f.kind, &f.gold, &response) {
                "missing_evidence"
            } else {
                "wrong_final"
            };
            break;
        }
        messages.push(assistant_calls(&r));
        for c in &r.calls {
            if calls.len() >= MAX_CALLS + usize::from(natural) {
                status = "retrieval_budget";
                break 'outer;
            }
            if !used_ids.insert(c.id.clone()) {
                status = "duplicate_call_id";
                break 'outer;
            }
            let args = match unique_arguments(&c.arguments) {
                Ok(v) => v,
                Err(_) => {
                    status = "invalid_arguments";
                    break 'outer;
                }
            };
            let value = |k| args.get(k).and_then(Value::as_str);
            let number = |k| {
                args.get(k)
                    .and_then(Value::as_u64)
                    .and_then(|v| usize::try_from(v).ok())
            };
            let shape = |names: &[&str]| {
                args.as_object()
                    .is_some_and(|m| m.keys().all(|x| names.contains(&x.as_str())))
            };
            let result = match c.name.as_str() {
                "open_report"
                    if !opened && shape(&["report"]) && value("report") == Some(&f.issue_key()) =>
                {
                    opened = true;
                    let idx = Instant::now();
                    first = publish(&mut store, &mut old, f, a);
                    indexing_us = idx.elapsed().as_micros();
                    add_initial_coverage(&first, &mut covered);
                    first.clone()
                }
                "open_report" => {
                    status = if opened {
                        "unexpected_reopen"
                    } else {
                        "wrong_initial_call"
                    };
                    calls.push(json!({"name":c.name,"args":args,"rejected":true}));
                    break 'outer;
                }
                "get_result_page" if a == Arm::Legacy && shape(&["output_ref", "offset"]) => {
                    let r = old.page(
                        value("output_ref").unwrap_or(""),
                        number("offset").unwrap_or(usize::MAX),
                    );
                    if let (Some(x), Some(s)) = (r["offset"].as_u64(), r["data"]["chunk"].as_str())
                    {
                        covered.push((x as usize, x as usize + s.len()));
                    }
                    r
                }
                "read_result"
                    if a != Arm::Legacy
                        && shape(&["ref", "start_line", "line_count", "offset"])
                        && (args.get("offset").is_none() || number("offset").is_some()) =>
                {
                    let r = store.read(
                        value("ref").unwrap_or(""),
                        number("start_line").unwrap_or(0),
                        number("line_count").unwrap_or(0),
                        number("offset"),
                    );
                    if let Some(p) = result_range(&r) {
                        covered.push(p);
                    }
                    r
                }
                "search_result"
                    if a != Arm::Legacy
                        && shape(&["ref", "query", "start_line", "limit"])
                        && (args.get("start_line").is_none() || number("start_line").is_some())
                        && (args.get("limit").is_none() || number("limit").is_some()) =>
                {
                    let start = number("start_line").unwrap_or(1);
                    let q = value("query").unwrap_or("");
                    let r = store.search(
                        value("ref").unwrap_or(""),
                        q,
                        start,
                        number("limit").unwrap_or(10),
                    );
                    if r["status"] == "ok" {
                        let exact = f.query.as_deref() == Some(q);
                        let covers_all_marked = f.kind == 5
                            && f.raw
                                .lines()
                                .filter(|line| line.ends_with(": REVIEW"))
                                .all(|line| line.contains(q));
                        if covers_all_marked
                            && expected_search_line == Some(start)
                            && coverage_query
                                .as_deref()
                                .is_none_or(|selected| selected == q)
                        {
                            coverage_query = Some(q.to_owned());
                            searched = true;
                            query_complete = r["data"]["has_more"] == false;
                            expected_search_line =
                                r["data"]["next_line"].as_u64().map(|n| n as usize);
                        }
                        if f.kind == 6 && exact && start == 1 && r["data"]["has_more"] == false {
                            searched = true;
                        }
                        for hit in r["data"]["matches"].as_array().into_iter().flatten() {
                            // Evidence only when the visible chunk actually contains a target value.
                            if hit["text"]
                                .as_str()
                                .is_some_and(|s| f.gold.iter().any(|g| s.contains(g)))
                            {
                                if let Some(i) = hit["line"].as_u64() {
                                    lines.insert(i as usize);
                                }
                            }
                        }
                    }
                    r
                }
                "check_report_status"
                    if shape(&["operation_id"])
                        && f.expected_status_lookup
                        && value("operation_id") == f.outcome["operation_id"].as_str() =>
                {
                    checked = true;
                    wire::ok(json!({"status":"completed","receipt":f.gold[0]}))
                }
                _ => wire::error("INVALID_TOOL_CALL"),
            };
            if c.name != "open_report" && result["status"] == "error" {
                // Reject invalid or out-of-scope proposals even if a subsequent model turn could repair them.
                status = "tool_call_rejected";
                calls
                    .push(json!({"name":c.name,"args":args,"code":result["code"],"rejected":true}));
                break 'outer;
            }
            mark_lines(f, &covered, &mut lines);
            if before_target_ms.is_none()
                && !f.evidence_lines.is_empty()
                && f.evidence_lines.iter().all(|l| lines.contains(l))
            {
                before_target_ms = Some(started.elapsed().as_millis());
            }
            calls.push(json!({"name":c.name,"args":args,"bytes":result.to_string().len(),"returned_next_line":result["data"]["next_line"],"result":result}));
            insert_result(&mut messages, &c.id, &result);
        }
    }
    let unique_calls = calls
        .iter()
        .map(|x| (x["name"].to_string(), x["args"].to_string()))
        .collect::<BTreeSet<_>>()
        .len();
    json!({"case_id":f.id,"scenario":format!("F{:02}",f.kind+1),"arm":a.code(),"natural":natural,"prompt":f.prompt,
        "model":model::MODEL,"max_tokens":4096,"seed":SEED,"os":std::env::consts::OS,
        "fixture_fnv1a64":fingerprint(&f.raw),"bytes":f.raw.len(),
        "first_response":first,"first_bytes":first.to_string().len(),"index_us":indexing_us,
        "outcome":status,"rounds":rounds,"requests_ms":req_ms,"calls":calls,"duplicate_calls":calls.len()-unique_calls,
        "evidence_lines":lines,"target_ms":before_target_ms,"wall_ms":started.elapsed().as_millis(),
        "input_tokens":input,"output_tokens":output,"cache_tokens":cached,"cache_usage_missing":cache_missing,"usage_missing":missing,
        "upstream":upstream,"answer":response,"gold":f.gold})
}

pub async fn run(mode: &str, offset: usize, count: usize, evidence: &str) -> Result<(), AnyError> {
    let arms = &match mode {
        "calibrate" => vec![Arm::Full, Arm::Legacy, Arm::Plain, Arm::Folded],
        "holdout" => vec![Arm::Plain, Arm::Folded],
        "natural" => vec![Arm::Full, Arm::Plain, Arm::Folded],
        _ => return Err("unknown folding mode".into()),
    };
    let path = std::path::Path::new("evidence").join(evidence);
    if !evidence.ends_with(".jsonl") || std::path::Path::new(evidence).components().count() != 1 {
        return Err("evidence filename must be local JSONL".into());
    }
    fs::create_dir_all("evidence")?;
    let mut completed = HashSet::new();
    if let Ok(file) = fs::File::open(&path) {
        for line in BufReader::new(file).lines() {
            let v: Value = serde_json::from_str(&line?)?;
            completed.insert((
                v["case_id"].as_u64().ok_or("bad case ID")? as usize,
                v["arm"].as_str().ok_or("bad test arm")?.to_owned(),
            ));
        }
    }
    let mut jobs = vec![];
    for i in offset..offset + count {
        let n = match mode {
            "calibrate" => i,
            "holdout" => i + 100,
            "natural" => i + 300,
            _ => unreachable!(),
        };
        for kind in 0..10 {
            if mode == "natural" && ![0, 1, 2, 4, 7, 8].contains(&kind) {
                continue;
            }
            let f = Arc::new(fixture(SEED, kind, n));
            for arm in arms {
                if !completed.contains(&(f.id, arm.code().into())) {
                    jobs.push((Arc::clone(&f), *arm, mode == "natural"));
                }
            }
        }
    }
    Rng(SEED ^ offset as u64).shuffle(&mut jobs);
    // No concurrent writers; resumptions preserve every completed task verdict.
    let lock = path.with_extension("lock");
    let _guard = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)?;
    struct Guard(std::path::PathBuf);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    let _cleanup = Guard(lock);
    let mut dest = OpenOptions::new().create(true).append(true).open(path)?;
    println!(
        "FOLD_BEGIN mode={mode} jobs={} workers=4 previously_logged={}",
        jobs.len(),
        completed.len()
    );
    let mut iter = jobs.into_iter();
    let mut set = tokio::task::JoinSet::new();
    let submit = |set: &mut tokio::task::JoinSet<Value>,
                  (f, a, natural): (Arc<Fixture>, Arm, bool)| {
        set.spawn(async move { task(&f, a, natural).await });
    };
    for _ in 0..4 {
        if let Some(j) = iter.next() {
            submit(&mut set, j);
        }
    }
    let mut n = 0;
    let mut consecutive = 0;
    while let Some(r) = set.join_next().await {
        let row = r?;
        serde_json::to_writer(&mut dest, &row)?;
        dest.write_all(b"\n")?;
        dest.flush()?;
        dest.sync_data()?;
        n += 1;
        consecutive = if matches!(
            row["outcome"].as_str(),
            Some("confirmed_upstream" | "request_error")
        ) {
            consecutive + 1
        } else {
            0
        };
        println!(
            "{} {} {} {}",
            row["case_id"], row["arm"], row["outcome"], row["wall_ms"]
        );
        if consecutive >= 8 {
            return Err("model route unhealthy; retained completed evidence".into());
        }
        if let Some(j) = iter.next() {
            submit(&mut set, j);
        }
    }
    println!("FOLD_END mode={mode} completed={n}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixture_contains_only_real_index_and_disjoint_holdout() {
        for n in (0..10).chain(100..200).chain(300..310) {
            for k in 0..10 {
                let f = fixture(SEED, k, n);
                if k != 9 {
                    assert!(f.raw.len() <= 65536);
                }
                assert_eq!(f.id, fixture(SEED, k, n).id);
                assert!(!f.prompt.contains(
                    &f.gold
                        .first()
                        .cloned()
                        .filter(|s| s.starts_with('K'))
                        .unwrap_or("unrelated".into())
                ));
            }
        }
    }
    #[test]
    fn retrieved_ranges_must_cover_every_byte() {
        assert!(complete(20, &[(8, 20), (0, 8)]));
        assert!(!complete(20, &[(9, 20), (0, 8)]));
        assert!(!complete(20, &[(0, 8), (6, 12)]));
    }
    #[test]
    fn presentation_changes_but_tools_for_strong_arms_do_not() {
        assert_eq!(tools(Arm::Full), tools(Arm::Plain));
        assert_eq!(tools(Arm::Plain), tools(Arm::Folded));
        let f = fixture(SEED, 3, 1);
        let c = publish(
            &mut FoldStore::default(),
            &mut ResultPages::default(),
            &f,
            Arm::Plain,
        );
        let d = publish(
            &mut FoldStore::default(),
            &mut ResultPages::default(),
            &f,
            Arm::Folded,
        );
        assert_eq!(c["data"], d["data"]);
        assert!(d["result"]["sections"].is_array());
    }
}
