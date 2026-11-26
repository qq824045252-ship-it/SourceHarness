use std::fs;
use std::path::Path;

use serde::Serialize;
use serde_json::{json, Value};

use crate::budget::MAX_RESPONSE_BYTES;
use crate::registry;
use crate::store::{FoldStore, IngestOutcome};
use crate::transport::injection::{
    EnvelopeInjector, InlineUserMessageInjector, NativeFunctionCallInjector,
};
use crate::transport::sse::UsageMetrics;
use crate::transport::{
    GatewayConfig, KernelToolExecutor, NoopRunObserver, ResponsesClient, RunResult, ToolCallTrace,
};
use crate::workspace::{WorkspaceConfig, WorkspaceContext, WorkspaceToolRouter};

#[derive(Debug, Clone, Serialize)]
pub struct EvalScenarioReport {
    pub name: String,
    pub passed: bool,
    pub turns: usize,
    pub reasoning_items: usize,
    pub tool_calls: usize,
    pub error_tool_calls: usize,
    pub inline_deliveries: usize,
    pub folded_deliveries: usize,
    pub max_tool_response_bytes: usize,
    pub usage: UsageMetrics,
    pub latency_ms: u128,
    pub failure_reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EvalSuiteReport {
    pub model: String,
    pub base_url: String,
    pub passed: usize,
    pub failed: usize,
    pub scenarios: Vec<EvalScenarioReport>,
}

/// Runs the end-to-end live verification and `inline` vs `native` paired comparison suite.
pub fn run_eval_suite(
    root: &Path,
    mut config: GatewayConfig,
    out_json_path: Option<&Path>,
) -> Result<EvalSuiteReport, String> {
    if config.reasoning_effort.is_none() {
        config.reasoning_effort = Some("low".to_owned());
    }
    let client = ResponsesClient::new(config.clone());
    let mut scenarios = Vec::new();

    println!(
        "Running SourceHarness live eval suite against {} (model: {})...",
        config.base_url, config.model
    );

    scenarios.push(run_workspace_mixed_inline_and_fold(root, &client)?);
    let (ab_inline, ab_native) = run_paired_ab_comparison(&client)?;
    scenarios.push(ab_inline);
    scenarios.push(ab_native);
    scenarios.push(run_whole_line_pagination_and_section_read(&client)?);
    scenarios.push(run_error_self_recovery_live(root, &client)?);

    let passed = scenarios.iter().filter(|s| s.passed).count();
    let failed = scenarios.len() - passed;

    let report = EvalSuiteReport {
        model: config.model,
        base_url: config.base_url,
        passed,
        failed,
        scenarios,
    };

    print_eval_summary(&report);

    if let Some(path) = out_json_path {
        let json_str = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
        fs::write(path, json_str)
            .map_err(|e| format!("Failed to write eval report `{}`: {e}", path.display()))?;
        println!("Saved structured eval report to {}", path.display());
    }

    if failed > 0 {
        return Err(format!("{failed} eval scenario(s) failed"));
    }
    Ok(report)
}

fn summarize_run(
    name: &str,
    result: &RunResult,
    mut failures: Vec<String>,
) -> EvalScenarioReport {
    let max_bytes = result
        .tool_traces
        .iter()
        .map(|t| t.response_bytes)
        .max()
        .unwrap_or(0);
    if max_bytes > MAX_RESPONSE_BYTES {
        failures.push(format!(
            "Tool response exceeded {MAX_RESPONSE_BYTES} B (max observed: {max_bytes} B)"
        ));
    }

    let reasoning_items = result.turns.iter().map(|t| t.reasoning_item_count).sum();
    let error_tool_calls = result
        .tool_traces
        .iter()
        .filter(|t| t.status == "error")
        .count();
    let inline_deliveries = result
        .tool_traces
        .iter()
        .filter(|t| t.delivery.as_deref() == Some("inline"))
        .count();
    let folded_deliveries = result
        .tool_traces
        .iter()
        .filter(|t| t.delivery.as_deref() == Some("folded"))
        .count();

    EvalScenarioReport {
        name: name.to_owned(),
        passed: failures.is_empty(),
        turns: result.turns.len(),
        reasoning_items,
        tool_calls: result.tool_traces.len(),
        error_tool_calls,
        inline_deliveries,
        folded_deliveries,
        max_tool_response_bytes: max_bytes,
        usage: result.total_usage,
        latency_ms: result.total_latency_ms,
        failure_reasons: failures,
    }
}

fn run_workspace_mixed_inline_and_fold(
    root: &Path,
    client: &ResponsesClient,
) -> Result<EvalScenarioReport, String> {
    let budget_src = fs::read_to_string(root.join("src/budget.rs"))
        .map_err(|e| format!("Failed to read src/budget.rs: {e}"))?;
    let search_src = fs::read_to_string(root.join("src/search.rs"))
        .map_err(|e| format!("Failed to read src/search.rs: {e}"))?;
    let read_src = fs::read_to_string(root.join("src/read.rs"))
        .map_err(|e| format!("Failed to read src/read.rs: {e}"))?;

    if !budget_src.contains("8192") || !read_src.contains("EXCEEDS_RESPONSE_BUDGET") {
        return Err("Workspace files missing expected budget/read constants".to_owned());
    }

    // Compute exact ErrorCode:: ground truth using the kernel itself.
    let truth_store = FoldStore::new();
    let truth_env = truth_store.insert(search_src);
    let truth_json: Value = serde_json::from_str(&registry::dispatch(
        "search_result",
        &json!({
            "result_ref": truth_env.result_ref,
            "query": "ErrorCode::([A-Za-z]+)",
            "match_mode": "regex",
            "group_by_capture": 1,
            "top_k": 50
        })
        .to_string(),
        &truth_store,
    ))
    .map_err(|e| e.to_string())?;

    let expected_lines = truth_json["data"]["matching_lines"]
        .as_u64()
        .ok_or_else(|| "missing matching_lines".to_owned())?;
    let expected_invalid_arg = truth_json["data"]["aggregation"]["value_counts"]
        .as_array()
        .and_then(|arr| {
            arr.iter()
                .find(|v| v["value"] == "InvalidArgument")
                .and_then(|v| v["count"].as_u64())
        })
        .ok_or_else(|| "missing InvalidArgument count".to_owned())?;

    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root))?;
    let router = WorkspaceToolRouter::new(ctx);

    let prompt = "Inspect the Rust workspace using the provided tools:\n\
1) Call `read_file` on `src/budget.rs` and report the exact `MAX_RESPONSE_BYTES` integer.\n\
2) Call `read_file` on `src/search.rs` (it is large and will fold into FoldStore), then call `search_result` on that `result_ref` to find:\n\
   - the exact number of lines matching `ErrorCode::`\n\
   - the exact occurrence count for `ErrorCode::InvalidArgument`\n\
3) Call `read_file` on `src/read.rs` and find the exact reason string used with `ErrorCode::UnreadableSingleLine`.\n\
Return the exact numbers and reason string concisely.";

    let input = vec![json!({ "role": "user", "content": prompt })];
    let mut observer = NoopRunObserver;
    let res = client
        .run_with_executor(
            input,
            Some("Use the workspace and result-folding tools strictly; do not guess."),
            &router,
            &mut observer,
        )
        .map_err(|e| format!("workspace_mixed_inline_and_fold failed: {e}"))?;

    let mut failures = Vec::new();
    let has_inline = res
        .tool_traces
        .iter()
        .any(|t| t.delivery.as_deref() == Some("inline"));
    let has_folded = res
        .tool_traces
        .iter()
        .any(|t| t.delivery.as_deref() == Some("folded"));
    let has_search = res.tool_traces.iter().any(|t| t.name == "search_result");

    if !has_inline {
        failures.push("Expected at least one inline producer tool delivery".to_owned());
    }
    if !has_folded {
        failures.push("Expected at least one folded producer tool delivery".to_owned());
    }
    if !has_search {
        failures.push("Expected at least one search_result kernel call".to_owned());
    }
    for (label, needle) in [
        ("MAX_RESPONSE_BYTES (8192)", "8192".to_owned()),
        ("ErrorCode:: matching lines", expected_lines.to_string()),
        ("ErrorCode::InvalidArgument count", expected_invalid_arg.to_string()),
        ("UnreadableSingleLine reason", "EXCEEDS_RESPONSE_BUDGET".to_owned()),
    ] {
        if !res.final_text.contains(&needle) {
            failures.push(format!("Final answer missing {label} (`{needle}`)"));
        }
    }

    Ok(summarize_run(
        "workspace_mixed_inline_and_fold",
        &res,
        failures,
    ))
}

fn run_paired_ab_comparison(
    client: &ResponsesClient,
) -> Result<(EvalScenarioReport, EvalScenarioReport), String> {
    let doc = build_paired_ab_document();
    let prompt = "Analyze the folded audit report via `search_result` (and `read_result` only if needed):\n\
1) Exact `matching_lines` and `total_occurrences` for `SEV=`.\n\
2) Exact counts for `SEV=HIGH` and `SEV=CRITICAL` using regex capture aggregation on `SEV=([A-Z]+)`.\n\
3) How many `## Region Check` sections exist in the envelope's `sections` index.\n\
Answer concisely with the exact numbers.";

    let inline_report =
        run_ab_arm("paired_ab_inline", &InlineUserMessageInjector, &doc, prompt, client)?;
    let native_report = run_ab_arm(
        "paired_ab_native",
        &NativeFunctionCallInjector::default(),
        &doc,
        prompt,
        client,
    )?;

    Ok((inline_report, native_report))
}

fn run_ab_arm(
    name: &str,
    injector: &dyn EnvelopeInjector,
    doc: &str,
    prompt: &str,
    client: &ResponsesClient,
) -> Result<EvalScenarioReport, String> {
    let store = FoldStore::new();
    let envelope = match store.ingest(doc.to_owned()) {
        IngestOutcome::Folded(env) => env,
        IngestOutcome::Inline(_) => return Err("Paired A/B document failed to fold".to_owned()),
    };

    let input = injector.build_initial_input(prompt, &envelope);
    let executor = KernelToolExecutor {
        store: &store,
        extra_tools: injector.extra_tool_definitions(),
    };
    let mut observer = NoopRunObserver;

    let res = client
        .run_with_executor(
            input,
            Some("Rely strictly on the folded envelope and tool outputs."),
            &executor,
            &mut observer,
        )
        .map_err(|e| format!("{name} failed: {e}"))?;

    let mut failures = Vec::new();
    if !res.tool_traces.iter().any(|t| t.name == "search_result") {
        failures.push("Expected at least one search_result call".to_owned());
    }
    for (label, needle) in [
        ("total SEV= count (45)", "45"),
        ("SEV=HIGH count (15)", "15"),
        ("SEV=CRITICAL count (7)", "7"),
        ("duplicate Region Check count (2)", "2"),
    ] {
        if !res.final_text.contains(needle) {
            failures.push(format!("Final answer missing {label}"));
        }
    }

    Ok(summarize_run(name, &res, failures))
}

fn run_whole_line_pagination_and_section_read(
    client: &ResponsesClient,
) -> Result<EvalScenarioReport, String> {
    let (doc, checksum_start, checksum_end) = build_pagination_document();
    let store = FoldStore::new();
    let envelope = match store.ingest(doc) {
        IngestOutcome::Folded(env) => env,
        IngestOutcome::Inline(_) => return Err("Pagination document failed to fold".to_owned()),
    };

    let injector = InlineUserMessageInjector;
    let prompt = "Inspect the folded release report:\n\
1) Report the exact `[start_line, end_line]` range of the `Release Checksums` section from the envelope.\n\
2) Using `read_result` (and `search_result` if helpful), read inside `Release Checksums` until you find the `SHA256-VERIFY-*` verification token near the end of that section, and report that exact token.";

    let input = injector.build_initial_input(prompt, &envelope);
    let executor = KernelToolExecutor {
        store: &store,
        extra_tools: Vec::new(),
    };
    let mut observer = NoopRunObserver;

    let res = client
        .run_with_executor(
            input,
            Some("Use read_result / search_result accurately; do not guess."),
            &executor,
            &mut observer,
        )
        .map_err(|e| format!("whole_line_pagination_and_section_read failed: {e}"))?;

    let mut failures = Vec::new();
    let used_kernel = res
        .tool_traces
        .iter()
        .any(|t| t.name == "read_result" || t.name == "search_result");
    if !used_kernel {
        failures.push("Expected read_result or search_result call".to_owned());
    }
    for (label, needle) in [
        ("verification token", "SHA256-VERIFY-9F4C2A".to_owned()),
        ("section start line", checksum_start.to_string()),
        ("section end line", checksum_end.to_string()),
    ] {
        if !res.final_text.contains(&needle) {
            failures.push(format!("Final answer missing {label} (`{needle}`)"));
        }
    }

    Ok(summarize_run(
        "whole_line_pagination_and_section_read",
        &res,
        failures,
    ))
}

fn run_error_self_recovery_live(
    root: &Path,
    client: &ResponsesClient,
) -> Result<EvalScenarioReport, String> {
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root))?;
    let router = WorkspaceToolRouter::new(ctx);

    let prompt = "Step 1: Attempt `read_file` on `src/does_not_exist.rs`.\n\
Step 2: When that returns an error, recover by using `list_files` on `src` to locate the error-definition file, read it, and report:\n\
1) The exact count of variants in `enum ErrorCode`.\n\
2) The exact boolean value assigned to `same_input_retry` in `ErrorPayload::new`.";

    let input = vec![json!({ "role": "user", "content": prompt })];
    let mut observer = NoopRunObserver;
    let res = client
        .run_with_executor(
            input,
            Some("Follow Step 1 literally first, then recover cleanly from the tool error."),
            &router,
            &mut observer,
        )
        .map_err(|e| format!("error_self_recovery_live failed: {e}"))?;

    let mut failures = Vec::new();
    let saw_error_trace = res
        .tool_traces
        .iter()
        .any(|t: &ToolCallTrace| t.status == "error");
    if !saw_error_trace {
        failures.push("Expected at least one recoverable error tool call trace".to_owned());
    }
    if !res.final_text.contains("10") {
        failures.push("Final answer missing ErrorCode variant count `10`".to_owned());
    }
    if !res.final_text.to_ascii_lowercase().contains("false") {
        failures.push("Final answer missing `same_input_retry = false`".to_owned());
    }

    Ok(summarize_run("error_self_recovery_live", &res, failures))
}

fn build_paired_ab_document() -> String {
    let mut s = String::from("# Cluster Audit Log\r\n\r\n");
    s.push_str("```text\r\n# Fake Fenced Heading 1\r\n# Fake Fenced Heading 2\r\n```\r\n\r\n");
    s.push_str("~~~\n## Fake Tilde Heading\n~~~\r\n\r\n");
    s.push_str("## Region Check\r\n");
    // 45 SEV= lines: 15 HIGH, 13 MEDIUM, 10 LOW, 7 CRITICAL (total 45 lines & 45 occurrences).
    for i in 1..=140 {
        if i == 70 {
            s.push_str("## Region Check\n");
        }
        let line = match i {
            1..=15 => format!("row={i:03} SEV=HIGH region=us-east detail=queue_depth_threshold_exceeded_padding_payload_for_fold\r\n"),
            21..=33 => format!("row={i:03} SEV=MEDIUM region=eu-west detail=retry_rate_elevated_padding_payload_for_fold_threshold\n"),
            41..=50 => format!("row={i:03} SEV=LOW region=ap-south detail=cache_warm_notice_padding_payload_for_fold_threshold\r\n"),
            81..=87 => format!("row={i:03} SEV=CRITICAL region=us-west detail=quorum_loss_alert_padding_payload_for_fold_threshold\n"),
            _ => format!("row={i:03} status=NOMINAL region=global detail=heartbeat_ok_padding_payload_to_ensure_size_over_fold_limit\n"),
        };
        s.push_str(&line);
    }
    s.push_str("\r\n## Footer\r\nAudit completed.\r\n");
    s
}

fn build_pagination_document() -> (String, usize, usize) {
    let mut lines = vec![
        "# Release Verification Bundle".to_owned(),
        "Generated for pagination verification.".to_owned(),
        "## Release Checksums".to_owned(),
    ];
    let checksum_start = lines.len(); // 3
    for i in 1..=119 {
        lines.push(format!(
            "ITEM-ROW-{i:03} artifact=bundle-{i:03}.tar.zst digest=0123456789abcdef0123456789abcdef0123456789abcdef"
        ));
    }
    lines.push("ITEM-ROW-120 verification_token=SHA256-VERIFY-9F4C2A status=verified".to_owned());
    let checksum_end = lines.len();
    lines.push("## Tail Notes".to_owned());
    lines.push("All checksums signed.".to_owned());
    (lines.join("\n"), checksum_start, checksum_end)
}

fn print_eval_summary(report: &EvalSuiteReport) {
    println!();
    println!(
        "{:<40} | {:<6} | {:>5} | {:>5} | {:>5} | {:>7} | {:>7} | {:>8} | {:>8}",
        "Scenario", "Status", "Turns", "Tools", "Errs", "Inline", "Folded", "MaxBytes", "Tokens"
    );
    println!("{}", "-".repeat(110));
    for s in &report.scenarios {
        println!(
            "{:<40} | {:<6} | {:>5} | {:>5} | {:>5} | {:>7} | {:>7} | {:>8} | {:>8}",
            s.name,
            if s.passed { "PASS" } else { "FAIL" },
            s.turns,
            s.tool_calls,
            s.error_tool_calls,
            s.inline_deliveries,
            s.folded_deliveries,
            s.max_tool_response_bytes,
            s.usage.total_tokens,
        );
        for reason in &s.failure_reasons {
            println!("  -> FAIL: {reason}");
        }
    }
    println!();
    println!(
        "Eval summary: {} passed, {} failed (model: {})",
        report.passed, report.failed, report.model
    );
}
