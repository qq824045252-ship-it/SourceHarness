use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use serde_json::{json, Value};
use source_harness::bench::{build_100_cases, run_bench_suite, BenchCategory, SizeTier};
use source_harness::registry;
use source_harness::store::{FoldStore, IngestOutcome};
use source_harness::transport::GatewayConfig;

#[test]
fn bench_100_cases_distribution_bounds_and_routing() {
    let cases = build_100_cases();
    assert_eq!(cases.len(), 100);

    let mut ids = HashSet::new();
    let mut tier_counts: BTreeMap<SizeTier, usize> = BTreeMap::new();
    let mut cat_counts: BTreeMap<BenchCategory, usize> = BTreeMap::new();

    for case in &cases {
        assert!(ids.insert(case.id.clone()), "duplicate id {}", case.id);
        let (min_b, max_b) = case.size_tier.byte_bounds();
        assert!(
            case.document.len() >= min_b && case.document.len() <= max_b,
            "{} size {} outside {:?}",
            case.id,
            case.document.len(),
            (min_b, max_b)
        );
        assert!(!case.checks.is_empty(), "{} has no checks", case.id);

        *tier_counts.entry(case.size_tier).or_insert(0) += 1;
        *cat_counts.entry(case.category).or_insert(0) += 1;

        let store = FoldStore::new();
        match (case.size_tier, store.ingest(case.document.clone())) {
            (SizeTier::K1, IngestOutcome::Inline(_)) => {}
            (SizeTier::K4 | SizeTier::K16, IngestOutcome::Folded(_)) => {}
            (tier, outcome) => panic!(
                "{} ({:?}) routed unexpectedly: {:?}",
                case.id, tier, outcome
            ),
        }
    }

    assert_eq!(tier_counts.get(&SizeTier::K1).copied(), Some(30));
    assert_eq!(tier_counts.get(&SizeTier::K4).copied(), Some(35));
    assert_eq!(tier_counts.get(&SizeTier::K16).copied(), Some(35));
    assert_eq!(cat_counts.len(), 10);
    for (cat, count) in cat_counts {
        assert_eq!(count, 10, "category {cat:?} count mismatch");
    }
}

#[test]
fn bench_sharded_and_sorted_ground_truth_self_consistency() {
    let cases = build_100_cases();

    // Verify 16k sharded pagination cases actually produce `complete: false` + `next_start_line`.
    let sharded_16k: Vec<_> = cases
        .iter()
        .filter(|c| c.category == BenchCategory::ShardedReadPagination && c.ideal_tool_rounds == 3)
        .collect();
    assert_eq!(sharded_16k.len(), 3);

    for case in sharded_16k {
        let store = FoldStore::new();
        let env = store.insert(case.document.clone());
        let data_sec = env
            .sections
            .iter()
            .find(|s| s.name == "Data Blocks")
            .expect("Data Blocks section missing");
        let section_len = data_sec.lines[1] - data_sec.lines[0] + 1;

        let mut start = data_sec.lines[0];
        let mut remaining = section_len;
        let mut pages = 0usize;
        let mut collected = String::new();

        while remaining > 0 {
            pages += 1;
            let raw = registry::dispatch(
                "read_result",
                &json!({
                    "result_ref": env.result_ref,
                    "start_line": start,
                    "line_count": remaining
                })
                .to_string(),
                &store,
            );
            let val: Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(val["status"], "ok");
            collected.push_str(val["data"]["content"].as_str().unwrap());

            if val["data"]["complete"].as_bool().unwrap() {
                break;
            }
            let next_start = val["data"]["next_start_line"].as_u64().unwrap() as usize;
            let returned_end = val["data"]["returned_line_range"][1].as_u64().unwrap() as usize;
            assert_eq!(next_start, returned_end + 1);
            remaining -= returned_end - start + 1;
            start = next_start;
        }

        assert!(
            pages >= 2,
            "{} must require >=2 read_result shards (got {pages})",
            case.id
        );
        for check in &case.checks {
            check
                .verify(&collected)
                .expect("check must match collected section");
        }
    }

    // Verify tiebreak sort order cases against search_result capture aggregation.
    for case in cases
        .iter()
        .filter(|c| c.category == BenchCategory::SortTiebreakAndDedup)
    {
        let store = FoldStore::new();
        let env = store.insert(case.document.clone());
        let raw = registry::dispatch(
            "search_result",
            &json!({
                "result_ref": env.result_ref,
                "query": "HOST=([A-Z0-9_]+)",
                "match_mode": "regex",
                "group_by_capture": 1,
                "top_k": 10
            })
            .to_string(),
            &store,
        );
        let val: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(val["status"], "ok");
        assert_eq!(val["data"]["aggregation"]["distinct_values"], 4);
        assert_eq!(val["data"]["aggregation"]["total_captured"], 18);
        let vc = val["data"]["aggregation"]["value_counts"]
            .as_array()
            .unwrap();
        let mut summary_text = format!(
            "distinct={} total={} ",
            val["data"]["aggregation"]["distinct_values"],
            val["data"]["aggregation"]["total_captured"]
        );
        summary_text.push_str(
            &vc.iter()
                .map(|e| format!("{}:{}", e["value"].as_str().unwrap(), e["count"]))
                .collect::<Vec<_>>()
                .join(" "),
        );
        for check in &case.checks {
            check
                .verify(&summary_text)
                .expect("sort tiebreak check failed");
        }
    }
}

fn accept_bench_request(listener: &TcpListener) -> TcpStream {
    let (mut stream, _) = listener.accept().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(&mut stream);
    let mut length = None;
    for line in reader.by_ref().lines() {
        let line = line.unwrap();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = Some(value.trim().parse::<usize>().unwrap());
        }
    }
    reader.read_exact(&mut vec![0; length.unwrap()]).unwrap();
    stream
}

fn bench_response(stream: &mut TcpStream, output: Value, usage: Value) {
    let event = json!({"type":"response.completed","response":{"output":[output],"usage":usage}});
    let sse = format!("event: response.completed\ndata: {event}\n\n");
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}", sse.len()).unwrap();
}

fn bench_gateway(listener: &TcpListener, max_turns: usize) -> GatewayConfig {
    GatewayConfig {
        base_url: format!("http://{}/v1", listener.local_addr().unwrap()),
        api_key: "fixture".into(),
        model: "fixture".into(),
        reasoning_effort: Some("low".into()),
        max_turns,
        timeout_secs: 5,
    }
}

#[test]
fn bench_reports_tool_traces_and_costs_across_retries() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let config = bench_gateway(&listener, 2);
    let server = std::thread::spawn(move || {
        let call = json!({"type":"function_call","call_id":"call_1","name":"search_result",
            "arguments":"{}"});
        bench_response(
            &mut accept_bench_request(&listener),
            call,
            json!({"input_tokens":50,"output_tokens":10,"total_tokens":60,
                "output_tokens_details":{"reasoning_tokens":5}}),
        );
        let mut failing = accept_bench_request(&listener);
        std::thread::sleep(Duration::from_millis(35));
        write!(
            failing,
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        drop(failing);
        bench_response(
            &mut accept_bench_request(&listener),
            json!({"type":"function_call","call_id":"call_1","name":"read_result","arguments":"{}"}),
            Value::Null,
        );
        bench_response(
            &mut accept_bench_request(&listener),
            json!({"type":"message","role":"assistant","content":[{"type":"output_text",
                "text":"matching_lines = 6, total_occurrences = 8"}]}),
            json!({"input_tokens":90,"output_tokens":10,"total_tokens":100,
                "output_tokens_details":{"reasoning_tokens":4}}),
        );
    });

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("report.json");
    run_bench_suite(config, "inline", 1, Some(1), Some(&path)).unwrap();
    server.join().unwrap();
    let report: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(report["passed"], 1);
    assert_eq!(report["total_attempts"], 2);
    assert_eq!(report["total_tool_calls"], 2);
    assert_eq!(report["total_tool_errors"], 2);
    assert_eq!(report["error_code_counts"], json!({"INVALID_ARGUMENT":2}));
    assert_eq!(report["total_tokens"], 160);
    assert_eq!(report["total_reasoning_tokens"], 9);
    assert_eq!(report["unknown_usage_requests"], 2);
    let case = &report["results"][0];
    assert_eq!(case["total_tool_calls"], 2);
    assert_eq!(case["error_tool_calls"], 2);
    assert_eq!(case["tool_rounds"], 1);
    assert_eq!(case["tool_sequence_by_turn"], json!([["read_result"]]));
    let attempts = case["attempts"].as_array().unwrap();
    let traces = case["tool_traces"].as_array().unwrap();
    assert_eq!(traces.len(), 2);
    assert!(attempts[0]["error"].as_str().unwrap().contains("503"));
    for (index, (known, name)) in [(60, "search_result"), (100, "read_result")]
        .into_iter()
        .enumerate()
    {
        let attempt = &attempts[index];
        assert_eq!(attempt["attempt"], index + 1);
        assert_eq!(attempt["total_tool_calls"], 1);
        assert_eq!(attempt["error_tool_calls"], 1);
        assert_eq!(attempt["tool_trace_range"], json!([index, index + 1]));
        let trace = &traces[index];
        assert_eq!(trace["attempt"], index + 1);
        assert_eq!(trace["turn"], 1);
        assert_eq!(trace["name"], name);
        assert_eq!(trace["status"], "error");
        assert_eq!(trace["error_code"], "INVALID_ARGUMENT");
        assert_eq!(attempt["known_usage"]["total_tokens"], known);
        assert_eq!(attempt["total_tokens"], Value::Null);
        assert_eq!(attempt["model_requests"], 2);
        assert_eq!(attempt["unknown_usage_requests"], 1);
    }
    assert!(attempts[0]["elapsed_ms"].as_u64().unwrap() >= 30);
    let total_elapsed: u64 = attempts
        .iter()
        .map(|a| a["elapsed_ms"].as_u64().unwrap())
        .sum();
    assert_eq!(case["latency_ms"], total_elapsed);
    assert_eq!(report["total_elapsed_ms"], total_elapsed);
    assert_eq!(report["by_tier"]["1k"]["avg_latency_ms"], total_elapsed);
    assert_eq!(report["by_tier"]["1k"]["total_tokens"], 160);
    assert_eq!(report["by_tier"]["1k"]["total_tool_calls"], 2);
    assert_eq!(report["by_tier"]["1k"]["error_tool_calls"], 2);
    assert_eq!(report["by_tier"]["1k"]["unknown_usage_requests"], 2);
}

#[test]
fn bench_preserves_calls_when_the_final_attempt_has_no_outcome() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let config = bench_gateway(&listener, 1);
    let server = std::thread::spawn(move || {
        bench_response(
            &mut accept_bench_request(&listener),
            json!({"type":"function_call","call_id":"call_1","name":"search_result","arguments":"{}"}),
            json!({"total_tokens":25}),
        );
    });

    let report = run_bench_suite(config, "inline", 1, Some(1), None).unwrap();
    server.join().unwrap();
    assert_eq!(report.failed, 1);
    assert_eq!(report.total_tool_calls, 1);
    assert_eq!(report.total_tool_errors, 1);
    assert_eq!(report.total_tokens, 25);
    assert_eq!(report.inline_zero_round_rate, "0/1");
    assert_eq!(report.within_ideal_rate, "0/1");
    assert_eq!(report.by_tier["1k"].error_tool_calls, 1);
}
