use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};

use crate::budget::MAX_RESPONSE_BYTES;
use crate::store::{FoldStore, IngestOutcome};
use crate::transport::injection::{
    EnvelopeInjector, InlineUserMessageInjector, NativeFunctionCallInjector,
};
use crate::transport::sse::UsageMetrics;
use crate::transport::{
    GatewayConfig, KernelToolExecutor, ResponsesClient, RunObserver, ToolCallTrace,
    ToolExecutionOutput, ToolExecutor, TransportError,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SizeTier {
    K1,
    K4,
    K16,
}

impl SizeTier {
    pub fn label(self) -> &'static str {
        match self {
            SizeTier::K1 => "1k",
            SizeTier::K4 => "4k",
            SizeTier::K16 => "16k",
        }
    }

    pub fn byte_bounds(self) -> (usize, usize) {
        match self {
            SizeTier::K1 => (950, 1450),
            SizeTier::K4 => (4250, 5100),
            SizeTier::K16 => (16400, 19500),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BenchCategory {
    LiteralExactCount,
    CaseInsensitiveCount,
    RegexCaptureSortTopk,
    SortTiebreakAndDedup,
    SectionScopedSearch,
    TargetLineContextRead,
    ShardedReadPagination,
    BudgetTrimScalarConservation,
    ZeroHitAndRegexPattern,
    MultiQuestionParallelCalls,
}

impl BenchCategory {
    pub fn label(self) -> &'static str {
        match self {
            BenchCategory::LiteralExactCount => "literal_exact_count",
            BenchCategory::CaseInsensitiveCount => "case_insensitive_count",
            BenchCategory::RegexCaptureSortTopk => "regex_capture_sort_topk",
            BenchCategory::SortTiebreakAndDedup => "sort_tiebreak_and_dedup",
            BenchCategory::SectionScopedSearch => "section_scoped_search",
            BenchCategory::TargetLineContextRead => "target_line_context_read",
            BenchCategory::ShardedReadPagination => "sharded_read_pagination",
            BenchCategory::BudgetTrimScalarConservation => "budget_trim_scalar_conservation",
            BenchCategory::ZeroHitAndRegexPattern => "zero_hit_and_regex_pattern",
            BenchCategory::MultiQuestionParallelCalls => "multi_question_parallel_calls",
        }
    }
}

#[derive(Debug, Clone)]
pub enum ExpectedCheck {
    Contains { label: String, needle: String },
    Int { label: String, value: usize },
    Ordered { label: String, sequence: Vec<String> },
}

impl ExpectedCheck {
    fn contains(label: impl Into<String>, needle: impl Into<String>) -> Self {
        Self::Contains {
            label: label.into(),
            needle: needle.into(),
        }
    }

    fn int(label: impl Into<String>, value: usize) -> Self {
        Self::Int {
            label: label.into(),
            value,
        }
    }

    fn ordered(label: impl Into<String>, sequence: Vec<String>) -> Self {
        Self::Ordered {
            label: label.into(),
            sequence,
        }
    }

    pub fn verify(&self, text: &str) -> Result<(), String> {
        match self {
            ExpectedCheck::Contains { label, needle } => {
                if text.contains(needle) {
                    Ok(())
                } else {
                    Err(format!("missing {label} (`{needle}`)"))
                }
            }
            ExpectedCheck::Int { label, value } => {
                let pat = format!(r"(?:^|[^0-9]){value}(?:$|[^0-9])");
                let re = Regex::new(&pat).expect("valid integer boundary regex");
                if re.is_match(text) {
                    Ok(())
                } else {
                    Err(format!("missing {label} (`{value}`)"))
                }
            }
            ExpectedCheck::Ordered { label, sequence } => {
                let mut cursor = 0usize;
                for item in sequence {
                    let Some(pos) = text[cursor..].find(item) else {
                        return Err(format!("missing/out-of-order {label} (`{item}`)"));
                    };
                    cursor += pos + item.len();
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct BenchCase {
    pub id: String,
    pub category: BenchCategory,
    pub size_tier: SizeTier,
    pub document: String,
    pub prompt: String,
    pub ideal_tool_rounds: usize,
    pub checks: Vec<ExpectedCheck>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BenchCaseResult {
    pub id: String,
    pub category: BenchCategory,
    pub size_tier: SizeTier,
    pub raw_bytes: usize,
    pub delivery: String,
    pub passed: bool,
    pub failure_reasons: Vec<String>,
    pub ideal_tool_rounds: usize,
    /// Round metrics refer to the final attempt, which determines the case outcome.
    pub llm_turns: usize,
    pub tool_rounds: usize,
    /// Tool counts and traces include all attempts.
    pub total_tool_calls: usize,
    pub over_called: bool,
    pub tool_sequence_by_turn: Vec<Vec<String>>,
    pub error_tool_calls: usize,
    pub error_codes: Vec<String>,
    pub hint_triggered: bool,
    pub sharded_reads: usize,
    pub sorted_aggregations: usize,
    pub max_tool_response_bytes: usize,
    pub reasoning_items: usize,
    /// Known usage across every attempt, with unreported requests counted separately.
    pub usage: UsageMetrics,
    pub unknown_usage_requests: usize,
    /// Total attempt elapsed time, excluding inter-attempt backoff.
    pub latency_ms: u128,
    pub tool_traces: Vec<BenchToolTrace>,
    pub attempts: Vec<BenchAttempt>,
}

impl BenchCaseResult {
    fn final_attempt_completed(&self) -> bool {
        self.attempts
            .last()
            .is_some_and(|attempt| attempt.error.is_none())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BenchToolTrace {
    pub attempt: usize,
    #[serde(flatten)]
    pub trace: ToolCallTrace,
}

#[derive(Debug, Clone, Serialize)]
pub struct BenchAttempt {
    /// 1-based attempt number within its case.
    pub attempt: usize,
    pub passed: bool,
    pub error: Option<String>,
    pub total_tokens: Option<u64>,
    pub known_usage: UsageMetrics,
    pub model_requests: usize,
    pub unknown_usage_requests: usize,
    pub elapsed_ms: u128,
    pub total_tool_calls: usize,
    pub error_tool_calls: usize,
    /// Zero-based [start, end) slice in the case's tool_traces.
    pub tool_trace_range: [usize; 2],
    pub hint_triggered: bool,
    pub sharded_reads: usize,
    pub sorted_aggregations: usize,
}

#[derive(Default)]
struct AttemptObserver {
    model_requests: usize,
    reported_requests: usize,
    known_usage: UsageMetrics,
    tool_traces: Vec<ToolCallTrace>,
    hint_triggered: bool,
    sharded_reads: usize,
    sorted_aggregations: usize,
}

impl RunObserver for AttemptObserver {
    fn on_turn_start(&mut self, _turn: usize) {
        self.model_requests += 1;
    }

    fn on_model_response_usage(&mut self, usage: Option<&UsageMetrics>) {
        if let Some(usage) = usage {
            self.reported_requests += 1;
            add_usage(&mut self.known_usage, usage);
        }
    }

    fn on_tool_call(&mut self, trace: &ToolCallTrace) {
        self.tool_traces.push(trace.clone());
    }
}

fn add_usage(total: &mut UsageMetrics, usage: &UsageMetrics) {
    total.input_tokens += usage.input_tokens;
    total.output_tokens += usage.output_tokens;
    total.reasoning_tokens += usage.reasoning_tokens;
    total.total_tokens += usage.total_tokens;
}

#[derive(Debug, Clone, Serialize)]
pub struct BenchAggregateStats {
    pub total: usize,
    pub passed: usize,
    pub within_ideal_rounds: usize,
    pub over_called: usize,
    pub total_tool_calls: usize,
    pub error_tool_calls: usize,
    pub hint_triggers: usize,
    pub sharded_reads: usize,
    pub sorted_aggregations: usize,
    pub total_attempts: usize,
    pub total_tokens: u64,
    pub unknown_usage_requests: usize,
    pub total_elapsed_ms: u128,
    pub avg_latency_ms: u128,
}

#[derive(Debug, Clone, Serialize)]
pub struct BenchReport {
    pub model: String,
    pub base_url: String,
    pub strategy: String,
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub inline_zero_round_rate: String,
    pub folded_one_round_rate: String,
    pub within_ideal_rate: String,
    pub over_called_count: usize,
    pub total_tool_calls: usize,
    pub total_tool_errors: usize,
    pub error_code_counts: BTreeMap<String, usize>,
    pub total_hint_triggers: usize,
    pub total_sharded_reads: usize,
    pub total_sorted_aggregations: usize,
    pub total_attempts: usize,
    pub total_tokens: u64,
    pub total_reasoning_tokens: u64,
    pub unknown_usage_requests: usize,
    pub total_elapsed_ms: u128,
    pub by_tier: BTreeMap<String, BenchAggregateStats>,
    pub by_category: BTreeMap<String, BenchAggregateStats>,
    pub results: Vec<BenchCaseResult>,
}

struct TracingExecutor<'a> {
    inner: KernelToolExecutor<'a>,
    sharded_reads: AtomicUsize,
    sorted_aggregations: AtomicUsize,
    hint_triggers: AtomicUsize,
}

impl<'a> TracingExecutor<'a> {
    fn new(store: &'a FoldStore, extra_tools: Vec<Value>) -> Self {
        Self {
            inner: KernelToolExecutor { store, extra_tools },
            sharded_reads: AtomicUsize::new(0),
            sorted_aggregations: AtomicUsize::new(0),
            hint_triggers: AtomicUsize::new(0),
        }
    }
}

impl ToolExecutor for TracingExecutor<'_> {
    fn tool_definitions(&self) -> Vec<Value> {
        self.inner.tool_definitions()
    }

    fn dispatch(&self, name: &str, raw_args: &str) -> ToolExecutionOutput {
        let out = self.inner.dispatch(name, raw_args);
        if let Ok(val) = serde_json::from_str::<Value>(&out.output_json) {
            if val.get("status").and_then(Value::as_str) == Some("ok") {
                if name == "read_result"
                    && val
                        .get("data")
                        .and_then(|d| d.get("complete"))
                        .and_then(Value::as_bool)
                        == Some(false)
                {
                    self.sharded_reads.fetch_add(1, Ordering::Relaxed);
                }
                if name == "search_result" {
                    if val
                        .get("data")
                        .and_then(|d| d.get("aggregation"))
                        .is_some()
                    {
                        self.sorted_aggregations.fetch_add(1, Ordering::Relaxed);
                    }
                    if val
                        .get("data")
                        .and_then(|d| d.get("literal_mode_hint"))
                        .is_some()
                    {
                        self.hint_triggers.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        out
    }
}

/// Runs the 100-task benchmark suite concurrently against the Responses API gateway.
pub fn run_bench_suite(
    mut config: GatewayConfig,
    strategy: &str,
    workers: usize,
    limit: Option<usize>,
    out_json_path: Option<&Path>,
) -> Result<BenchReport, String> {
    if config.reasoning_effort.is_none() {
        config.reasoning_effort = Some("low".to_owned());
    }
    let workers = workers.max(1);

    let mut cases = build_100_cases();
    if let Some(n) = limit {
        cases.truncate(n);
    }
    let total_cases = cases.len();
    let queue = Mutex::new(VecDeque::from(cases));
    let completed = AtomicUsize::new(0);
    let results_mutex = Mutex::new(Vec::with_capacity(total_cases));

    println!(
        "Running SourceHarness bench ({total_cases} tasks, workers={workers}, strategy={strategy}, model={}, url={})...",
        config.model, config.base_url
    );

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                let client = ResponsesClient::new(config.clone());
                loop {
                    let case = {
                        let mut q = queue.lock().expect("queue lock");
                        q.pop_front()
                    };
                    let Some(case) = case else {
                        break;
                    };
                    let res = execute_case_with_retry(&client, &case, strategy);
                    let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                    if done.is_multiple_of(10)
                        || done == total_cases
                        || !res.passed
                        || res.over_called
                    {
                        println!(
                            "[{done:>3}/{total_cases}] {} ({}/{}) pass={} rounds={}/{} tools={} err={} known_tokens={} unknown_usage_requests={} {}ms",
                            res.id,
                            res.category.label(),
                            res.size_tier.label(),
                            res.passed,
                            res.tool_rounds,
                            res.ideal_tool_rounds,
                            res.total_tool_calls,
                            res.error_tool_calls,
                            res.usage.total_tokens,
                            res.unknown_usage_requests,
                            res.latency_ms
                        );
                    }
                    results_mutex.lock().expect("results lock").push(res);
                }
            });
        }
    });

    let mut results = results_mutex.into_inner().expect("results lock");
    results.sort_by(|a, b| a.id.cmp(&b.id));

    let report = build_report(&config, strategy, results);
    print_bench_summary(&report);

    if let Some(path) = out_json_path {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let json_str = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
        fs::write(path, json_str)
            .map_err(|e| format!("Failed to write bench report `{}`: {e}", path.display()))?;
        println!("Saved full bench telemetry to {}", path.display());
    }

    Ok(report)
}

fn select_injector(strategy: &str) -> Result<Box<dyn EnvelopeInjector>, String> {
    match strategy {
        "inline" => Ok(Box::new(InlineUserMessageInjector)),
        "native" => Ok(Box::new(NativeFunctionCallInjector::default())),
        other => Err(format!(
            "Unknown strategy `{other}` (expected `inline` or `native`)"
        )),
    }
}

fn execute_case_with_retry(
    client: &ResponsesClient,
    case: &BenchCase,
    strategy: &str,
) -> BenchCaseResult {
    let delays = [500u64, 1500u64];
    let mut attempts = Vec::with_capacity(delays.len() + 1);
    let mut tool_traces = Vec::new();
    for attempt in 0..=delays.len() {
        let started = Instant::now();
        let mut observer = AttemptObserver::default();
        let outcome = execute_single_case(client, case, strategy, &mut observer);
        let unknown_usage_requests = observer.model_requests - observer.reported_requests;
        let attempt_number = attempt + 1;
        let trace_start = tool_traces.len();
        let total_tool_calls = observer.tool_traces.len();
        let error_tool_calls = observer
            .tool_traces
            .iter()
            .filter(|trace| trace.status == "error")
            .count();
        tool_traces.extend(
            observer.tool_traces.into_iter().map(|trace| BenchToolTrace {
                attempt: attempt_number,
                trace,
            }),
        );
        attempts.push(BenchAttempt {
            attempt: attempt_number,
            passed: outcome.as_ref().is_ok_and(|result| result.passed),
            error: outcome.as_ref().err().map(ToString::to_string),
            total_tokens: (unknown_usage_requests == 0).then_some(observer.known_usage.total_tokens),
            known_usage: observer.known_usage,
            model_requests: observer.model_requests,
            unknown_usage_requests,
            elapsed_ms: started.elapsed().as_millis(),
            total_tool_calls,
            error_tool_calls,
            tool_trace_range: [trace_start, tool_traces.len()],
            hint_triggered: observer.hint_triggered,
            sharded_reads: observer.sharded_reads,
            sorted_aggregations: observer.sorted_aggregations,
        });
        let mut result = match outcome {
            Ok(result) => result,
            Err(err) => {
                let retryable = matches!(err, TransportError::Http(_) | TransportError::Io(_));
                if retryable && attempt < delays.len() {
                    std::thread::sleep(Duration::from_millis(delays[attempt]));
                    continue;
                }
                BenchCaseResult {
                    id: case.id.clone(),
                    category: case.category,
                    size_tier: case.size_tier,
                    raw_bytes: case.document.len(),
                    delivery: if case.size_tier == SizeTier::K1 {
                        "inline".to_owned()
                    } else {
                        "folded".to_owned()
                    },
                    passed: false,
                    failure_reasons: vec![format!("transport error: {err}")],
                    ideal_tool_rounds: case.ideal_tool_rounds,
                    llm_turns: 0,
                    tool_rounds: 0,
                    total_tool_calls: 0,
                    over_called: false,
                    tool_sequence_by_turn: Vec::new(),
                    error_tool_calls: 0,
                    error_codes: Vec::new(),
                    hint_triggered: false,
                    sharded_reads: 0,
                    sorted_aggregations: 0,
                    max_tool_response_bytes: 0,
                    reasoning_items: 0,
                    usage: UsageMetrics::default(),
                    unknown_usage_requests: 0,
                    latency_ms: 0,
                    tool_traces: Vec::new(),
                    attempts: Vec::new(),
                }
            }
        };
        for attempt in &attempts {
            add_usage(&mut result.usage, &attempt.known_usage);
            result.unknown_usage_requests += attempt.unknown_usage_requests;
            result.latency_ms += attempt.elapsed_ms;
            result.hint_triggered |= attempt.hint_triggered;
            result.sharded_reads += attempt.sharded_reads;
            result.sorted_aggregations += attempt.sorted_aggregations;
        }
        result.total_tool_calls = tool_traces.len();
        result.error_codes = tool_traces
            .iter()
            .filter(|record| record.trace.status == "error")
            .map(|record| record.trace.error_code.clone().unwrap_or_else(|| "UNKNOWN".to_owned()))
            .collect();
        result.error_tool_calls = result.error_codes.len();
        result.max_tool_response_bytes = tool_traces
            .iter()
            .map(|record| record.trace.response_bytes)
            .max()
            .unwrap_or(0);
        result.tool_traces = tool_traces;
        result.attempts = attempts;
        return result;
    }
    unreachable!()
}

fn execute_single_case(
    client: &ResponsesClient,
    case: &BenchCase,
    strategy: &str,
    observer: &mut AttemptObserver,
) -> Result<BenchCaseResult, TransportError> {
    let injector = select_injector(strategy).expect("valid bench strategy");
    let store = FoldStore::new();

    let (delivery, input, extra_tools) = match store.ingest(case.document.clone()) {
        IngestOutcome::Inline(content) => (
            "inline".to_owned(),
            vec![json!({
                "role": "user",
                "content": format!(
                    "{}\n\n```text\n{}\n```",
                    case.prompt, content
                ),
            })],
            Vec::new(),
        ),
        IngestOutcome::Folded(env) => (
            "folded".to_owned(),
            injector.build_initial_input(&case.prompt, &env),
            injector.extra_tool_definitions(),
        ),
    };

    let executor = TracingExecutor::new(&store, extra_tools);

    let outcome = client.run_with_executor(
        input,
        Some("Answer accurately and concisely using the provided content or result tools."),
        &executor,
        observer,
    );
    observer.hint_triggered = executor.hint_triggers.load(Ordering::Relaxed) > 0;
    observer.sharded_reads = executor.sharded_reads.load(Ordering::Relaxed);
    observer.sorted_aggregations = executor.sorted_aggregations.load(Ordering::Relaxed);
    let run = outcome?;

    let mut failure_reasons = Vec::new();
    for check in &case.checks {
        if let Err(msg) = check.verify(&run.final_text) {
            failure_reasons.push(msg);
        }
    }

    let max_tool_response_bytes = run
        .tool_traces
        .iter()
        .map(|t| t.response_bytes)
        .max()
        .unwrap_or(0);
    if max_tool_response_bytes > MAX_RESPONSE_BYTES {
        failure_reasons.push(format!(
            "tool response exceeded {MAX_RESPONSE_BYTES} B ({max_tool_response_bytes})"
        ));
    }

    let tool_rounds = run
        .turns
        .iter()
        .filter(|t| !t.function_calls.is_empty())
        .count();
    let tool_sequence_by_turn: Vec<Vec<String>> = run
        .turns
        .iter()
        .filter(|t| !t.function_calls.is_empty())
        .map(|t| t.function_calls.clone())
        .collect();

    let reasoning_items = run.turns.iter().map(|t| t.reasoning_item_count).sum();

    let passed = failure_reasons.is_empty();
    Ok(BenchCaseResult {
        id: case.id.clone(),
        category: case.category,
        size_tier: case.size_tier,
        raw_bytes: case.document.len(),
        delivery,
        passed,
        failure_reasons,
        ideal_tool_rounds: case.ideal_tool_rounds,
        llm_turns: run.turns.len(),
        tool_rounds,
        total_tool_calls: 0,
        over_called: tool_rounds > case.ideal_tool_rounds,
        tool_sequence_by_turn,
        error_tool_calls: 0,
        error_codes: Vec::new(),
        hint_triggered: false,
        sharded_reads: 0,
        sorted_aggregations: 0,
        max_tool_response_bytes: 0,
        reasoning_items,
        usage: UsageMetrics::default(),
        unknown_usage_requests: 0,
        latency_ms: 0,
        tool_traces: Vec::new(),
        attempts: Vec::new(),
    })
}

fn aggregate_slice<'a>(items: impl Iterator<Item = &'a BenchCaseResult>) -> BenchAggregateStats {
    let mut total = 0usize;
    let mut passed = 0usize;
    let mut within_ideal_rounds = 0usize;
    let mut over_called = 0usize;
    let mut total_tool_calls = 0usize;
    let mut error_tool_calls = 0usize;
    let mut hint_triggers = 0usize;
    let mut sharded_reads = 0usize;
    let mut sorted_aggregations = 0usize;
    let mut total_attempts = 0usize;
    let mut total_tokens = 0u64;
    let mut unknown_usage_requests = 0usize;
    let mut total_latency = 0u128;

    for r in items {
        total += 1;
        if r.passed {
            passed += 1;
        }
        if r.final_attempt_completed() && r.tool_rounds <= r.ideal_tool_rounds {
            within_ideal_rounds += 1;
        }
        if r.over_called {
            over_called += 1;
        }
        total_tool_calls += r.total_tool_calls;
        error_tool_calls += r.error_tool_calls;
        if r.hint_triggered {
            hint_triggers += 1;
        }
        sharded_reads += r.sharded_reads;
        sorted_aggregations += r.sorted_aggregations;
        total_attempts += r.attempts.len();
        total_tokens += r.usage.total_tokens;
        unknown_usage_requests += r.unknown_usage_requests;
        total_latency += r.latency_ms;
    }

    BenchAggregateStats {
        total,
        passed,
        within_ideal_rounds,
        over_called,
        total_tool_calls,
        error_tool_calls,
        hint_triggers,
        sharded_reads,
        sorted_aggregations,
        total_attempts,
        total_tokens,
        unknown_usage_requests,
        total_elapsed_ms: total_latency,
        avg_latency_ms: if total == 0 {
            0
        } else {
            total_latency / total as u128
        },
    }
}

fn build_report(
    config: &GatewayConfig,
    strategy: &str,
    results: Vec<BenchCaseResult>,
) -> BenchReport {
    let total = results.len();
    let passed = results.iter().filter(|r| r.passed).count();
    let failed = total - passed;

    let inline_cases: Vec<&BenchCaseResult> =
        results.iter().filter(|r| r.delivery == "inline").collect();
    let inline_zero = inline_cases
        .iter()
        .filter(|r| r.final_attempt_completed() && r.tool_rounds == 0)
        .count();

    let single_round_folded: Vec<&BenchCaseResult> = results
        .iter()
        .filter(|r| r.delivery == "folded" && r.ideal_tool_rounds == 1)
        .collect();
    let folded_one = single_round_folded
        .iter()
        .filter(|r| r.tool_rounds == 1)
        .count();

    let within_ideal = results
        .iter()
        .filter(|r| r.final_attempt_completed() && r.tool_rounds <= r.ideal_tool_rounds)
        .count();
    let over_called_count = results.iter().filter(|r| r.over_called).count();

    let mut error_code_counts = BTreeMap::new();
    let mut total_tool_errors = 0usize;
    for r in &results {
        for code in &r.error_codes {
            total_tool_errors += 1;
            *error_code_counts.entry(code.clone()).or_insert(0) += 1;
        }
    }

    let mut by_tier = BTreeMap::new();
    for tier in [SizeTier::K1, SizeTier::K4, SizeTier::K16] {
        let stats = aggregate_slice(results.iter().filter(|r| r.size_tier == tier));
        by_tier.insert(tier.label().to_owned(), stats);
    }

    let mut by_category = BTreeMap::new();
    for cat in [
        BenchCategory::LiteralExactCount,
        BenchCategory::CaseInsensitiveCount,
        BenchCategory::RegexCaptureSortTopk,
        BenchCategory::SortTiebreakAndDedup,
        BenchCategory::SectionScopedSearch,
        BenchCategory::TargetLineContextRead,
        BenchCategory::ShardedReadPagination,
        BenchCategory::BudgetTrimScalarConservation,
        BenchCategory::ZeroHitAndRegexPattern,
        BenchCategory::MultiQuestionParallelCalls,
    ] {
        let stats = aggregate_slice(results.iter().filter(|r| r.category == cat));
        by_category.insert(cat.label().to_owned(), stats);
    }

    BenchReport {
        model: config.model.clone(),
        base_url: config.base_url.clone(),
        strategy: strategy.to_owned(),
        total,
        passed,
        failed,
        inline_zero_round_rate: format!("{inline_zero}/{}", inline_cases.len()),
        folded_one_round_rate: format!("{folded_one}/{}", single_round_folded.len()),
        within_ideal_rate: format!("{within_ideal}/{total}"),
        over_called_count,
        total_tool_calls: results.iter().map(|r| r.total_tool_calls).sum(),
        total_tool_errors,
        error_code_counts,
        total_hint_triggers: results.iter().filter(|r| r.hint_triggered).count(),
        total_sharded_reads: results.iter().map(|r| r.sharded_reads).sum(),
        total_sorted_aggregations: results.iter().map(|r| r.sorted_aggregations).sum(),
        total_attempts: results.iter().map(|r| r.attempts.len()).sum(),
        total_tokens: results.iter().map(|r| r.usage.total_tokens).sum(),
        total_reasoning_tokens: results.iter().map(|r| r.usage.reasoning_tokens).sum(),
        unknown_usage_requests: results.iter().map(|r| r.unknown_usage_requests).sum(),
        total_elapsed_ms: results.iter().map(|r| r.latency_ms).sum(),
        by_tier,
        by_category,
        results,
    }
}

fn print_bench_summary(report: &BenchReport) {
    println!();
    println!("================ 100-TASK BENCH SUMMARY ================");
    println!(
        "passed={}/{} | within_ideal={} | over_called(follow-up)={} | 1k_zero_round={} | folded_one_round={}",
        report.passed,
        report.total,
        report.within_ideal_rate,
        report.over_called_count,
        report.inline_zero_round_rate,
        report.folded_one_round_rate
    );
    println!(
        "tool_calls={} | tool_errors={} {:?} | hint_triggers={} | sharded_reads={} | sorted_aggregations={} | known_tokens={} (reason={}) | unknown_usage_requests={} | attempts={}",
        report.total_tool_calls,
        report.total_tool_errors,
        report.error_code_counts,
        report.total_hint_triggers,
        report.total_sharded_reads,
        report.total_sorted_aggregations,
        report.total_tokens,
        report.total_reasoning_tokens,
        report.unknown_usage_requests,
        report.total_attempts
    );

    println!("\n--- BY SIZE TIER ---");
    println!(
        "{:<8} | {:>5} | {:>6} | {:>5} | {:>4} | {:>4} | {:>6} | {:>6} | {:>8}",
        "Tier", "Pass", "Ideal", "Over", "Errs", "Hint", "Shard", "Sort", "Tokens"
    );
    for tier in ["1k", "4k", "16k"] {
        if let Some(s) = report.by_tier.get(tier) {
            println!(
                "{:<8} | {:>2}/{:<2} | {:>3}/{:<2} | {:>5} | {:>4} | {:>4} | {:>6} | {:>6} | {:>8}",
                tier,
                s.passed,
                s.total,
                s.within_ideal_rounds,
                s.total,
                s.over_called,
                s.error_tool_calls,
                s.hint_triggers,
                s.sharded_reads,
                s.sorted_aggregations,
                s.total_tokens
            );
        }
    }

    println!("\n--- BY CATEGORY ---");
    println!(
        "{:<32} | {:>5} | {:>6} | {:>5} | {:>4} | {:>4} | {:>6} | {:>6} | {:>8}",
        "Category", "Pass", "Ideal", "Over", "Errs", "Hint", "Shard", "Sort", "Tokens"
    );
    for (cat, s) in &report.by_category {
        println!(
            "{:<32} | {:>2}/{:<2} | {:>3}/{:<2} | {:>5} | {:>4} | {:>4} | {:>6} | {:>6} | {:>8}",
            cat,
            s.passed,
            s.total,
            s.within_ideal_rounds,
            s.total,
            s.over_called,
            s.error_tool_calls,
            s.hint_triggers,
            s.sharded_reads,
            s.sorted_aggregations,
            s.total_tokens
        );
    }

    let notable: Vec<&BenchCaseResult> = report
        .results
        .iter()
        .filter(|r| !r.passed || r.over_called || r.error_tool_calls > 0 || r.hint_triggered)
        .collect();

    if !notable.is_empty() {
        println!("\n--- CASES WITH FOLLOW-UP / ERRORS / HINTS / FAILURES ({}) ---", notable.len());
        for r in notable {
            println!(
                "{} [{}/{}] pass={} rounds={}/{} seq={:?} errs={:?} hint={} reasons={:?}",
                r.id,
                r.category.label(),
                r.size_tier.label(),
                r.passed,
                r.tool_rounds,
                r.ideal_tool_rounds,
                r.tool_sequence_by_turn,
                r.error_codes,
                r.hint_triggered,
                r.failure_reasons
            );
        }
    }
    println!("========================================================");
}

fn pad_document_to_tier(doc: &mut String, tier: SizeTier, seed: usize) {
    let (min_bytes, max_bytes) = tier.byte_bounds();
    let mut seq = 1usize;
    while doc.len() < min_bytes {
        let line = format!(
            "filler seed={seed:03} row={seq:04} status=NOMINAL detail=baseline_telemetry_padding_payload_for_size_tier_{}\n",
            tier.label()
        );
        if doc.len() + line.len() > max_bytes {
            break;
        }
        doc.push_str(&line);
        seq += 1;
    }
    assert!(
        doc.len() >= min_bytes && doc.len() <= max_bytes,
        "case seed={seed} tier={} size {} out of bounds {:?}",
        tier.label(),
        doc.len(),
        (min_bytes, max_bytes)
    );
}

/// Generates all 100 deterministic benchmark tasks (30×1k, 35×4k, 35×16k across 10 categories).
pub fn build_100_cases() -> Vec<BenchCase> {
    let mut cases = Vec::with_capacity(100);

    // 1. literal_exact_count (3×1k, 4×4k, 3×16k)
    let t1 = [
        SizeTier::K1,
        SizeTier::K1,
        SizeTier::K1,
        SizeTier::K4,
        SizeTier::K4,
        SizeTier::K4,
        SizeTier::K4,
        SizeTier::K16,
        SizeTier::K16,
        SizeTier::K16,
    ];
    for (idx, &tier) in t1.iter().enumerate() {
        cases.push(make_literal_exact_case(idx, tier));
    }

    // 2. case_insensitive_count (3×1k, 4×4k, 3×16k)
    for (idx, &tier) in t1.iter().enumerate() {
        cases.push(make_case_insensitive_case(idx, tier));
    }

    // 3. regex_capture_sort_topk (3×1k, 3×4k, 4×16k)
    let t3 = [
        SizeTier::K1,
        SizeTier::K1,
        SizeTier::K1,
        SizeTier::K4,
        SizeTier::K4,
        SizeTier::K4,
        SizeTier::K16,
        SizeTier::K16,
        SizeTier::K16,
        SizeTier::K16,
    ];
    for (idx, &tier) in t3.iter().enumerate() {
        cases.push(make_regex_sort_topk_case(idx, tier));
    }

    // 4. sort_tiebreak_and_dedup (3×1k, 3×4k, 4×16k)
    for (idx, &tier) in t3.iter().enumerate() {
        cases.push(make_sort_tiebreak_case(idx, tier));
    }

    // 5. section_scoped_search (3×1k, 4×4k, 3×16k)
    for (idx, &tier) in t1.iter().enumerate() {
        cases.push(make_section_scoped_case(idx, tier));
    }

    // 6. target_line_context_read (3×1k, 4×4k, 3×16k)
    for (idx, &tier) in t1.iter().enumerate() {
        cases.push(make_target_line_read_case(idx, tier));
    }

    // 7. sharded_read_pagination (2×1k, 3×4k, 5×16k)
    let t7 = [
        SizeTier::K1,
        SizeTier::K1,
        SizeTier::K4,
        SizeTier::K4,
        SizeTier::K4,
        SizeTier::K16,
        SizeTier::K16,
        SizeTier::K16,
        SizeTier::K16,
        SizeTier::K16,
    ];
    for (idx, &tier) in t7.iter().enumerate() {
        cases.push(make_sharded_read_case(idx, tier));
    }

    // 8. budget_trim_scalar_conservation (3×1k, 3×4k, 4×16k)
    for (idx, &tier) in t3.iter().enumerate() {
        cases.push(make_budget_trim_case(idx, tier));
    }

    // 9. zero_hit_and_regex_pattern (3×1k, 4×4k, 3×16k)
    for (idx, &tier) in t1.iter().enumerate() {
        cases.push(make_zero_hit_or_regex_case(idx, tier));
    }

    // 10. multi_question_parallel_calls (4×1k, 3×4k, 3×16k)
    let t10 = [
        SizeTier::K1,
        SizeTier::K1,
        SizeTier::K1,
        SizeTier::K1,
        SizeTier::K4,
        SizeTier::K4,
        SizeTier::K4,
        SizeTier::K16,
        SizeTier::K16,
        SizeTier::K16,
    ];
    for (idx, &tier) in t10.iter().enumerate() {
        cases.push(make_multi_question_case(idx, tier));
    }

    cases
}

fn make_literal_exact_case(idx: usize, tier: SizeTier) -> BenchCase {
    let token = format!("ERR_SIG_{idx:02}");
    let single_lines = 4 + (idx % 4); // 4..7 lines with 1 occurrence
    let double_lines = 2 + (idx % 3); // 2..4 lines with 2 occurrences
    let matching_lines = single_lines + double_lines;
    let total_occurrences = single_lines + double_lines * 2;

    let mut doc = format!("# Event Stream {idx:02}\n## Records\n");
    for i in 0..single_lines {
        doc.push_str(&format!("seq={i:02} code={token} host=srv-a{i:02}\n"));
    }
    for i in 0..double_lines {
        doc.push_str(&format!(
            "seq=d{i:02} primary={token} backup={token} host=srv-b{i:02}\n"
        ));
    }
    // Lowercase distractor that must NOT match case-sensitive search.
    doc.push_str(&format!("noise=lowercase_{}\n", token.to_ascii_lowercase()));
    pad_document_to_tier(&mut doc, tier, 100 + idx);

    let ideal = if tier == SizeTier::K1 { 0 } else { 1 };
    let prompt = format!(
        "Count exact case-sensitive occurrences of `{token}`.\n\
         Report two numbers: `matching_lines` and `total_occurrences`."
    );

    BenchCase {
        id: format!("c01_{:02}_{}", idx + 1, tier.label()),
        category: BenchCategory::LiteralExactCount,
        size_tier: tier,
        document: doc,
        prompt,
        ideal_tool_rounds: ideal,
        checks: vec![
            ExpectedCheck::int("matching_lines", matching_lines),
            ExpectedCheck::int("total_occurrences", total_occurrences),
        ],
    }
}

fn make_case_insensitive_case(idx: usize, tier: SizeTier) -> BenchCase {
    let base = format!("warn_gate_{idx:02}");
    let variants = [
        base.to_ascii_uppercase(),
        base.to_ascii_lowercase(),
        format!("Warn_Gate_{idx:02}"),
    ];
    let n_lines = 6 + idx; // 6..15 lines
    let extra_occ = 2; // first 2 lines contain 2 occurrences
    let total_occ = n_lines + extra_occ;

    let mut doc = format!("# Mixed Case Trace {idx:02}\r\n## Entries\r\n");
    for i in 0..n_lines {
        let v = &variants[i % variants.len()];
        if i < extra_occ {
            doc.push_str(&format!("row={i:02} tag1={v} tag2={}\n", variants[(i + 1) % 3]));
        } else {
            doc.push_str(&format!("row={i:02} tag={v}\r\n"));
        }
    }
    pad_document_to_tier(&mut doc, tier, 200 + idx);

    let ideal = if tier == SizeTier::K1 { 0 } else { 1 };
    let prompt = format!(
        "Perform a case-insensitive count for `{base}`.\n\
         Report the exact `matching_lines` and `total_occurrences`."
    );

    BenchCase {
        id: format!("c02_{:02}_{}", idx + 1, tier.label()),
        category: BenchCategory::CaseInsensitiveCount,
        size_tier: tier,
        document: doc,
        prompt,
        ideal_tool_rounds: ideal,
        checks: vec![
            ExpectedCheck::int("matching_lines", n_lines),
            ExpectedCheck::int("total_occurrences", total_occ),
        ],
    }
}

fn make_regex_sort_topk_case(idx: usize, tier: SizeTier) -> BenchCase {
    // 5 distinct categories with strictly decreasing counts: 9, 7, 5, 3, 2 (total = 26).
    let cats = [
        (format!("MOD_ZETA_{idx:02}"), 9usize),
        (format!("MOD_BETA_{idx:02}"), 7usize),
        (format!("MOD_DELTA_{idx:02}"), 5usize),
        (format!("MOD_ALPHA_{idx:02}"), 3usize),
        (format!("MOD_GAMMA_{idx:02}"), 2usize),
    ];
    let total_captured: usize = cats.iter().map(|c| c.1).sum();

    let mut doc = format!("# Telemetry Sort {idx:02}\n## Metrics\n");
    for (name, count) in &cats {
        for j in 0..*count {
            doc.push_str(&format!("tick={j:02} MODULE={name} latency_ms={}\n", 10 + j));
        }
    }
    pad_document_to_tier(&mut doc, tier, 300 + idx);

    let ideal = if tier == SizeTier::K1 { 0 } else { 1 };
    let prompt = "Aggregate `MODULE=([A-Z0-9_]+)` sorted by count descending.\n\
        Report `distinct_values`, `total_captured`, and the top 3 module names in descending order of count."
        .to_owned();

    BenchCase {
        id: format!("c03_{:02}_{}", idx + 1, tier.label()),
        category: BenchCategory::RegexCaptureSortTopk,
        size_tier: tier,
        document: doc,
        prompt,
        ideal_tool_rounds: ideal,
        checks: vec![
            ExpectedCheck::int("distinct_values", cats.len()),
            ExpectedCheck::int("total_captured", total_captured),
            ExpectedCheck::int("top1_count", 9),
            ExpectedCheck::ordered(
                "top3_order",
                vec![cats[0].0.clone(), cats[1].0.clone(), cats[2].0.clone()],
            ),
        ],
    }
}

fn make_sort_tiebreak_case(idx: usize, tier: SizeTier) -> BenchCase {
    // Winner has 6; three tied keys each have 4; verify lexicographical ASC tiebreak (ALPHA, BRAVO, CHARLIE).
    let top_key = format!("NODE_OMEGA_{idx:02}");
    let tie_keys = [
        format!("NODE_ALPHA_{idx:02}"),
        format!("NODE_BRAVO_{idx:02}"),
        format!("NODE_CHARLIE_{idx:02}"),
    ];
    let mut doc = format!("# Tiebreak Sort {idx:02}\n## Hosts\n");
    for j in 0..6 {
        doc.push_str(&format!("evt={j:02} HOST={top_key}\n"));
    }
    // Insert in reverse lexicographical order to verify sorting actually runs!
    for key in tie_keys.iter().rev() {
        for j in 0..4 {
            doc.push_str(&format!("evt={j:02} HOST={key}\n"));
        }
    }
    pad_document_to_tier(&mut doc, tier, 400 + idx);

    let ideal = if tier == SizeTier::K1 { 0 } else { 1 };
    let prompt = "Aggregate `HOST=([A-Z0-9_]+)` sorted by count DESC, then name ASC for ties.\n\
        Report `distinct_values` (4), `total_captured` (18), and list all 4 host names in that exact sorted order."
        .to_owned();

    BenchCase {
        id: format!("c04_{:02}_{}", idx + 1, tier.label()),
        category: BenchCategory::SortTiebreakAndDedup,
        size_tier: tier,
        document: doc,
        prompt,
        ideal_tool_rounds: ideal,
        checks: vec![
            ExpectedCheck::int("distinct_values", 4),
            ExpectedCheck::int("total_captured", 18),
            ExpectedCheck::ordered(
                "sorted_tiebreak_order",
                vec![
                    top_key,
                    tie_keys[0].clone(),
                    tie_keys[1].clone(),
                    tie_keys[2].clone(),
                ],
            ),
        ],
    }
}

fn make_section_scoped_case(idx: usize, tier: SizeTier) -> BenchCase {
    let marker = format!("TARGET_FLAG_{idx:02}");
    let mut doc = format!(
        "# Service Report {idx:02}\r\n\
         ## Alpha Section\r\n\
         item=1 {marker}\r\n\
         item=2 {marker}\r\n\
         ```markdown\r\n\
         ## Beta Section\r\n\
         fake=1 {marker}\r\n\
         ```\r\n\
         ## Beta Section\r\n"
    );
    let beta_count = 3 + (idx % 4);
    for i in 0..beta_count {
        doc.push_str(&format!("beta_row={i:02} {marker}\n"));
    }
    doc.push_str("## Gamma Section\n");
    doc.push_str(&format!("gamma_row=01 {marker}\n"));
    pad_document_to_tier(&mut doc, tier, 500 + idx);

    let ideal = if tier == SizeTier::K1 { 0 } else { 1 };
    let prompt = format!(
        "Find how many lines contain `{marker}` ONLY inside the real `## Beta Section` \
         (ignoring the fenced code block and other sections).\n\
         Report that exact section-scoped line count."
    );

    BenchCase {
        id: format!("c05_{:02}_{}", idx + 1, tier.label()),
        category: BenchCategory::SectionScopedSearch,
        size_tier: tier,
        document: doc,
        prompt,
        ideal_tool_rounds: ideal,
        checks: vec![ExpectedCheck::int("beta_section_count", beta_count)],
    }
}

fn make_target_line_read_case(idx: usize, tier: SizeTier) -> BenchCase {
    let secret = format!("SIGN_TOKEN_K{idx:02}_{}", 700 + idx);
    let mut doc = String::from("# Config Snapshot\r\n## Header\nversion=2\r\n");
    pad_document_to_tier_prefix(&mut doc, tier, 600 + idx);
    let target_line = doc.lines().count() + 2; // next line is `## Target`, then `secret`
    doc.push_str("## Target\r\n");
    doc.push_str(&format!("key_entry={secret} encoding=UTF8-中文验证\r\n"));
    doc.push_str("## Tail\nstatus=done\n");
    pad_document_to_tier(&mut doc, tier, 650 + idx);

    let ideal = if tier == SizeTier::K1 { 0 } else { 1 };
    let prompt = format!(
        "Read the `## Target` section (around line {target_line}) and report the exact `key_entry` token starting with `SIGN_TOKEN_`."
    );

    BenchCase {
        id: format!("c06_{:02}_{}", idx + 1, tier.label()),
        category: BenchCategory::TargetLineContextRead,
        size_tier: tier,
        document: doc,
        prompt,
        ideal_tool_rounds: ideal,
        checks: vec![ExpectedCheck::contains("secret_token", secret)],
    }
}

fn pad_document_to_tier_prefix(doc: &mut String, tier: SizeTier, seed: usize) {
    let prefix_target = match tier {
        SizeTier::K1 => 350,
        SizeTier::K4 => 2200,
        SizeTier::K16 => 8500,
    };
    let mut seq = 1usize;
    while doc.len() < prefix_target {
        doc.push_str(&format!(
            "preface seed={seed:03} row={seq:04} status=OK detail=pre_target_padding_line\n"
        ));
        seq += 1;
    }
}

fn make_sharded_read_case(idx: usize, tier: SizeTier) -> BenchCase {
    let first_code = format!("CHK-FIRST-{idx:02}");
    let mid_code = format!("CHK-MID-{idx:02}");
    let last_code = format!("CHK-LAST-{idx:02}");

    if tier == SizeTier::K16 && idx < 8 {
        // Explicit multi-shard full-section pagination over > 12 KiB `## Data Blocks`.
        let mut doc = String::from("# Sharded Manifest\n## Overview\nheader=ok\n## Data Blocks\n");
        let start_line = doc.lines().count(); // line of `## Data Blocks`
        let total_block_lines = 128usize;
        let mid_idx = total_block_lines / 2;
        for i in 0..total_block_lines {
            let line = if i == 0 {
                format!(
                    "row={i:03} marker={first_code} payload={}\n",
                    "D".repeat(95)
                )
            } else if i == mid_idx {
                format!(
                    "row={i:03} marker={mid_code} payload={}\n",
                    "D".repeat(95)
                )
            } else if i + 1 == total_block_lines {
                format!(
                    "row={i:03} marker={last_code} payload={}\n",
                    "D".repeat(95)
                )
            } else {
                format!("row={i:03} status=DATA payload={}\n", "D".repeat(100))
            };
            doc.push_str(&line);
        }
        doc.push_str("## Closing\nend=true\n");
        pad_document_to_tier(&mut doc, tier, 700 + idx);

        let prompt = format!(
            "The `## Data Blocks` section starts at line {start_line} and spans > 12 KiB.\n\
             Use `read_result` starting at line {start_line} and follow `next_start_line` across shards to read the full `## Data Blocks` section.\n\
             Report the exact three `CHK-*` codes (`CHK-FIRST-*`, `CHK-MID-*`, `CHK-LAST-*`)."
        );

        BenchCase {
            id: format!("c07_{:02}_{}", idx + 1, tier.label()),
            category: BenchCategory::ShardedReadPagination,
            size_tier: tier,
            document: doc,
            prompt,
            ideal_tool_rounds: 3,
            checks: vec![
                ExpectedCheck::contains("first_code", first_code),
                ExpectedCheck::contains("mid_code", mid_code),
                ExpectedCheck::contains("last_code", last_code),
            ],
        }
    } else {
        // Cross-shard targeted check (1k inline, 4k single read, 16k targeted read/search).
        let mut doc = format!(
            "# Checkpoint Bundle {idx:02}\n## Head\nrow=001 marker={first_code}\n"
        );
        if tier == SizeTier::K16 {
            let mut seq = 2usize;
            while doc.len() < 8400 {
                doc.push_str(&format!(
                    "row={seq:04} filler=mid_padding_payload_for_cross_shard_offset\n"
                ));
                seq += 1;
            }
            doc.push_str("## Middle\n");
            doc.push_str(&format!("row=mid marker={mid_code}\n"));
            while doc.len() < 16300 {
                doc.push_str(&format!(
                    "row={seq:04} filler=tail_padding_payload_for_cross_shard_offset\n"
                ));
                seq += 1;
            }
            doc.push_str("## Tail\n");
            doc.push_str(&format!("row=last marker={last_code}\n"));
            pad_document_to_tier(&mut doc, tier, 750 + idx);
        } else {
            doc.push_str("## Middle\n");
            doc.push_str(&format!("row=mid marker={mid_code}\n"));
            doc.push_str("## Tail\n");
            doc.push_str(&format!("row=last marker={last_code}\n"));
            pad_document_to_tier(&mut doc, tier, 750 + idx);
        }

        let ideal = if tier == SizeTier::K1 { 0 } else { 1 };
        let prompt = "Locate the three `CHK-*` codes in the `Head`, `Middle`, and `Tail` sections and report all three exact codes.".to_owned();

        BenchCase {
            id: format!("c07_{:02}_{}", idx + 1, tier.label()),
            category: BenchCategory::ShardedReadPagination,
            size_tier: tier,
            document: doc,
            prompt,
            ideal_tool_rounds: ideal,
            checks: vec![
                ExpectedCheck::contains("first_code", first_code),
                ExpectedCheck::contains("mid_code", mid_code),
                ExpectedCheck::contains("last_code", last_code),
            ],
        }
    }
}

fn make_budget_trim_case(idx: usize, tier: SizeTier) -> BenchCase {
    // Many long matching lines + distinct capture groups so naive max_hits=20 + top_k=50 exceeds 8 KiB,
    // verifying scalar truth conservation.
    let distinct = match tier {
        SizeTier::K1 => 6,
        SizeTier::K4 => 20,
        SizeTier::K16 => 55,
    };
    let top_name = format!("BUCKET_TOP_{idx:02}");
    let top_count = 4usize;
    let total_captured = top_count + (distinct - 1);

    let note_len = match tier {
        SizeTier::K1 => 40,
        SizeTier::K4 => 140,
        SizeTier::K16 => 180,
    };
    let mut doc = format!("# Budget Conservation {idx:02}\n## Buckets\n");
    for _j in 0..top_count {
        doc.push_str(&format!(
            "BKT={top_name} note={}\n",
            "N".repeat(note_len)
        ));
    }
    for k in 1..distinct {
        let bname = format!("BUCKET_ITEM_{idx:02}_{k:02}_{}", "V".repeat(16));
        doc.push_str(&format!(
            "BKT={bname} note={}\n",
            "N".repeat(if tier == SizeTier::K1 { 20 } else { note_len })
        ));
    }
    pad_document_to_tier(&mut doc, tier, 800 + idx);

    let ideal = if tier == SizeTier::K1 { 0 } else { 1 };
    let prompt = "Count all `BKT=([A-Z0-9_]+)` occurrences.\n\
        Report exact `matching_lines`, `distinct_values`, `total_captured`, and the #1 top `BKT` name."
        .to_owned();

    BenchCase {
        id: format!("c08_{:02}_{}", idx + 1, tier.label()),
        category: BenchCategory::BudgetTrimScalarConservation,
        size_tier: tier,
        document: doc,
        prompt,
        ideal_tool_rounds: ideal,
        checks: vec![
            ExpectedCheck::int("matching_lines", total_captured),
            ExpectedCheck::int("distinct_values", distinct),
            ExpectedCheck::contains("top_bucket", top_name),
        ],
    }
}

fn make_zero_hit_or_regex_case(idx: usize, tier: SizeTier) -> BenchCase {
    let ideal = if tier == SizeTier::K1 { 0 } else { 1 };

    if idx.is_multiple_of(2) {
        // Zero-hit verification: must report 0 and stop in 1 round without retrying endlessly.
        let absent = format!("PHANTOM_CODE_XYZ_{idx:02}");
        let mut doc = format!("# Clean Audit {idx:02}\n## Status\nall_checks=healthy\n");
        pad_document_to_tier(&mut doc, tier, 900 + idx);

        let prompt = format!(
            "Check how many lines match `{absent}`.\n\
             If it is absent, explicitly state `0`."
        );
        BenchCase {
            id: format!("c09_{:02}_{}", idx + 1, tier.label()),
            category: BenchCategory::ZeroHitAndRegexPattern,
            size_tier: tier,
            document: doc,
            prompt,
            ideal_tool_rounds: ideal,
            checks: vec![ExpectedCheck::int("zero_matches", 0)],
        }
    } else {
        // Regex alternation (`ALPHA|OMEGA`) with word boundary (`\b`).
        let alpha_n = 3 + (idx % 3);
        let omega_n = 2 + (idx % 2);
        let total = alpha_n + omega_n;
        let mut doc = format!("# Regex Alternation {idx:02}\n## Signals\n");
        for i in 0..alpha_n {
            doc.push_str(&format!("sig={i:02} state=SIG_ALPHA_{idx:02}\n"));
        }
        for i in 0..omega_n {
            doc.push_str(&format!("sig={i:02} state=SIG_OMEGA_{idx:02}\n"));
        }
        doc.push_str(&format!("noise=SIG_BETA_{idx:02}\n"));
        pad_document_to_tier(&mut doc, tier, 950 + idx);

        let prompt = format!(
            "Using regex alternation `SIG_(ALPHA|OMEGA)_{idx:02}`, count total matching lines \
             and report both individual counts plus the combined total ({total})."
        );
        BenchCase {
            id: format!("c09_{:02}_{}", idx + 1, tier.label()),
            category: BenchCategory::ZeroHitAndRegexPattern,
            size_tier: tier,
            document: doc,
            prompt,
            ideal_tool_rounds: ideal,
            checks: vec![
                ExpectedCheck::int("alpha_count", alpha_n),
                ExpectedCheck::int("omega_count", omega_n),
                ExpectedCheck::int("combined_total", total),
            ],
        }
    }
}

fn make_multi_question_case(idx: usize, tier: SizeTier) -> BenchCase {
    let top_svc = format!("SVC_CORE_{idx:02}");
    let second_svc = format!("SVC_EDGE_{idx:02}");
    let note = format!("NOTE-APPROVE-{idx:02}");
    let top_n = 6usize;
    let second_n = 3usize;
    let total_n = top_n + second_n;

    let mut doc = format!("# Compound Summary {idx:02}\n## Services\n");
    for i in 0..top_n {
        doc.push_str(&format!("call={i:02} SERVICE={top_svc} status=WARN\n"));
    }
    for i in 0..second_n {
        doc.push_str(&format!("call={i:02} SERVICE={second_svc} status=WARN\n"));
    }
    doc.push_str("## Verdict\n");
    doc.push_str(&format!("signoff={note}\n"));
    pad_document_to_tier(&mut doc, tier, 1000 + idx);

    let ideal = if tier == SizeTier::K1 { 0 } else { 1 };
    let prompt = format!(
        "Answer all three in one pass:\n\
         1) Total lines containing `SERVICE=` ({total_n} expected from data).\n\
         2) Top `SERVICE=([A-Z0-9_]+)` name and its count.\n\
         3) Exact `signoff=` value in `## Verdict`."
    );
    // Do not leak ground truth in the prompt!
    let clean_prompt = prompt.replace(&format!(" ({total_n} expected from data)"), "");

    BenchCase {
        id: format!("c10_{:02}_{}", idx + 1, tier.label()),
        category: BenchCategory::MultiQuestionParallelCalls,
        size_tier: tier,
        document: doc,
        prompt: clean_prompt,
        ideal_tool_rounds: ideal,
        checks: vec![
            ExpectedCheck::int("total_service_lines", total_n),
            ExpectedCheck::contains("top_service", top_svc),
            ExpectedCheck::int("top_service_count", top_n),
            ExpectedCheck::contains("signoff_note", note),
        ],
    }
}
