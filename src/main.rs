use std::fs;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process;

use serde_json::{json, Value};
use source_harness::bench;
use source_harness::eval;
use source_harness::registry;
use source_harness::store::{FoldStore, IngestOutcome};
use source_harness::transport::injection::{
    EnvelopeInjector, InlineUserMessageInjector, NativeFunctionCallInjector,
};
use source_harness::transport::sse::UsageMetrics;
use source_harness::transport::{
    GatewayConfig, ResponsesClient, RunObserver, RunResult, ToolCallTrace, TurnMetrics,
};
use source_harness::workspace::{WorkspaceConfig, WorkspaceContext, WorkspaceToolRouter};

fn main() {
    if let Err(err) = run_cli() {
        eprintln!("Error: {err}");
        process::exit(1);
    }
}

fn run_cli() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        print_usage();
        return Ok(());
    }

    match args[0].as_str() {
        "schema" => {
            let defs = registry::tool_definitions();
            let pretty = serde_json::to_string_pretty(&defs).map_err(|e| e.to_string())?;
            println!("{pretty}");
            Ok(())
        }
        "smoke" => {
            let strategy =
                parse_flag(&args[1..], "--strategy")?.unwrap_or_else(|| "inline".to_owned());
            let cfg = apply_gateway_flags(GatewayConfig::from_env(), &args[1..])?;
            run_smoke(&strategy, cfg)
        }
        "run" => {
            let file = parse_flag(&args[1..], "--file")?
                .ok_or_else(|| "Missing --file <path>".to_owned())?;
            let prompt = parse_flag(&args[1..], "--prompt")?
                .ok_or_else(|| "Missing --prompt <text>".to_owned())?;
            let strategy =
                parse_flag(&args[1..], "--strategy")?.unwrap_or_else(|| "inline".to_owned());
            let cfg = apply_gateway_flags(GatewayConfig::from_env(), &args[1..])?;
            run_file(&file, &prompt, &strategy, cfg)
        }
        "agent" => {
            let root = parse_flag(&args[1..], "--root")?.unwrap_or_else(|| ".".to_owned());
            let prompt = parse_flag(&args[1..], "--prompt")?
                .ok_or_else(|| "Missing --prompt <text>".to_owned())?;
            let cfg = apply_gateway_flags(GatewayConfig::from_env(), &args[1..])?;
            run_agent(PathBuf::from(root), &prompt, cfg)
        }
        "chat" => {
            let root = parse_flag(&args[1..], "--root")?.unwrap_or_else(|| ".".to_owned());
            let cfg = apply_gateway_flags(GatewayConfig::from_env(), &args[1..])?;
            run_chat(PathBuf::from(root), cfg)
        }
        "eval" => {
            let root = parse_flag(&args[1..], "--root")?.unwrap_or_else(|| ".".to_owned());
            let out = parse_flag(&args[1..], "--out")?.map(PathBuf::from);
            let cfg = apply_gateway_flags(GatewayConfig::from_env(), &args[1..])?;
            eval::run_eval_suite(&PathBuf::from(root), cfg, out.as_deref()).map(|_| ())
        }
        "bench" => {
            let strategy =
                parse_flag(&args[1..], "--strategy")?.unwrap_or_else(|| "inline".to_owned());
            let workers = parse_flag(&args[1..], "--workers")?
                .map(|s| {
                    s.parse::<usize>()
                        .map_err(|_| format!("Invalid --workers `{s}`"))
                })
                .transpose()?
                .unwrap_or(6);
            let limit = parse_flag(&args[1..], "--limit")?
                .map(|s| {
                    s.parse::<usize>()
                        .map_err(|_| format!("Invalid --limit `{s}`"))
                })
                .transpose()?;
            let out = parse_flag(&args[1..], "--out")?.map(PathBuf::from);
            let cfg = apply_gateway_flags(GatewayConfig::from_env(), &args[1..])?;
            bench::run_bench_suite(cfg, &strategy, workers, limit, out.as_deref()).map(|_| ())
        }
        "-h" | "--help" | "help" => {
            print_usage();
            Ok(())
        }
        other => Err(format!("Unknown subcommand: {other}")),
    }
}

fn parse_flag(args: &[String], flag: &str) -> Result<Option<String>, String> {
    let mut i = 0usize;
    while i < args.len() {
        if args[i] == flag {
            let val = args
                .get(i + 1)
                .ok_or_else(|| format!("Missing value for {flag}"))?;
            return Ok(Some(val.clone()));
        }
        i += 1;
    }
    Ok(None)
}

fn apply_gateway_flags(mut cfg: GatewayConfig, args: &[String]) -> Result<GatewayConfig, String> {
    if let Some(model) = parse_flag(args, "--model")? {
        cfg.model = model;
    }
    if let Some(effort) = parse_flag(args, "--effort")? {
        cfg.reasoning_effort = Some(effort);
    }
    Ok(cfg)
}

struct StreamingTerminalObserver {
    printed_text_this_turn: bool,
}

impl StreamingTerminalObserver {
    fn new() -> Self {
        Self {
            printed_text_this_turn: false,
        }
    }
}

impl RunObserver for StreamingTerminalObserver {
    fn on_turn_start(&mut self, _turn: usize) {
        self.printed_text_this_turn = false;
    }

    fn on_text_delta(&mut self, delta: &str) {
        print!("{delta}");
        let _ = io::stdout().flush();
        self.printed_text_this_turn = true;
    }

    fn on_tool_call(&mut self, trace: &ToolCallTrace) {
        if self.printed_text_this_turn {
            println!();
            self.printed_text_this_turn = false;
        }
        let delivery = trace.delivery.as_deref().unwrap_or("-");
        let ref_info = trace.result_ref.as_deref().unwrap_or("-");
        let err_info = trace.error_code.as_deref().unwrap_or("-");
        eprintln!(
            "[tool] turn={} name={} status={} delivery={} ref={} err={} bytes={} {}ms",
            trace.turn,
            trace.name,
            trace.status,
            delivery,
            ref_info,
            err_info,
            trace.response_bytes,
            trace.duration_ms
        );
    }

    fn on_turn_complete(&mut self, metrics: &TurnMetrics) {
        if self.printed_text_this_turn {
            println!();
            self.printed_text_this_turn = false;
        }
        eprintln!(
            "[turn] turn={} reasoning_items={} tools={:?} tokens(in={}, out={}, reason={}, total={}) {}ms",
            metrics.turn,
            metrics.reasoning_item_count,
            metrics.function_calls,
            metrics.usage.input_tokens,
            metrics.usage.output_tokens,
            metrics.usage.reasoning_tokens,
            metrics.usage.total_tokens,
            metrics.latency_ms
        );
    }
}

fn workspace_instructions() -> &'static str {
    "You are running inside SourceHarness. Use `list_files`, `read_file`, and `grep_files` to inspect the workspace. \
Small outputs return inline directly. Large outputs fold into FoldStore and return a `result_ref` envelope; \
inspect folded outputs using `search_result` (exact counts & capture aggregation) and `read_result` (1-based whole-line ranges). \
Rely strictly on observed tool outputs and never guess file contents or counts."
}

fn run_agent(root: PathBuf, prompt: &str, cfg: GatewayConfig) -> Result<(), String> {
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root))?;
    let router = WorkspaceToolRouter::new(ctx);
    let client = ResponsesClient::new(cfg);

    let input = vec![json!({
        "role": "user",
        "content": prompt,
    })];
    let mut observer = StreamingTerminalObserver::new();
    let result = client
        .run_with_executor(input, Some(workspace_instructions()), &router, &mut observer)
        .map_err(|e| e.to_string())?;

    eprintln!(
        "[done] turns={} tool_calls={} total_tokens={} (reasoning={}) elapsed={}ms",
        result.turns.len(),
        result.tool_traces.len(),
        result.total_usage.total_tokens,
        result.total_usage.reasoning_tokens,
        result.total_latency_ms
    );
    Ok(())
}

fn run_chat(root: PathBuf, cfg: GatewayConfig) -> Result<(), String> {
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root))?;
    let root_display = ctx.root().display().to_string();
    let mut router = WorkspaceToolRouter::new(ctx);
    let client = ResponsesClient::new(cfg.clone());

    let mut history: Vec<Value> = Vec::new();
    let mut session_usage = UsageMetrics::default();
    let mut session_tool_calls = 0usize;

    println!(
        "SourceHarness interactive chat | root={} | model={}",
        root_display, cfg.model
    );
    println!("Commands: :quit, :exit, :reset, :stats");

    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut line = String::new();

    loop {
        print!("\nyou> ");
        let _ = io::stdout().flush();
        line.clear();
        let n = reader.read_line(&mut line).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        let user_msg = line.trim();
        if user_msg.is_empty() {
            continue;
        }
        match user_msg {
            ":quit" | ":exit" => break,
            ":reset" => {
                history.clear();
                router = WorkspaceToolRouter::new(router.context().clone());
                session_usage = UsageMetrics::default();
                session_tool_calls = 0;
                println!("Session history and FoldStore reset.");
                continue;
            }
            ":stats" => {
                println!(
                    "history_items={} tool_calls={} tokens(in={}, out={}, reasoning={}, total={})",
                    history.len(),
                    session_tool_calls,
                    session_usage.input_tokens,
                    session_usage.output_tokens,
                    session_usage.reasoning_tokens,
                    session_usage.total_tokens
                );
                continue;
            }
            _ => {}
        }

        let mut turn_input = history.clone();
        turn_input.push(json!({
            "role": "user",
            "content": user_msg,
        }));

        let mut observer = StreamingTerminalObserver::new();
        match client.run_with_executor(
            turn_input,
            Some(workspace_instructions()),
            &router,
            &mut observer,
        ) {
            Ok(res) => {
                history = res.final_input;
                session_tool_calls += res.tool_traces.len();
                session_usage.input_tokens += res.total_usage.input_tokens;
                session_usage.output_tokens += res.total_usage.output_tokens;
                session_usage.reasoning_tokens += res.total_usage.reasoning_tokens;
                session_usage.total_tokens += res.total_usage.total_tokens;
            }
            Err(err) => {
                eprintln!("[error] {err}");
            }
        }
    }

    Ok(())
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

fn run_smoke(strategy: &str, cfg: GatewayConfig) -> Result<(), String> {
    let injector = select_injector(strategy)?;
    let doc = build_smoke_document();
    let store = FoldStore::new();

    let envelope = match store.ingest(doc) {
        IngestOutcome::Folded(env) => env,
        IngestOutcome::Inline(_) => return Err("Smoke document was unexpectedly small".to_owned()),
    };

    let prompt = "Using search_result (and read_result only if needed), find:\n\
1) How many lines contain `ALERT_CODE=` and how many total occurrences there are.\n\
2) The exact top category counts from `ALERT_CODE=([A-Z_]+)`.\n\
3) The note on the line inside the `## Summary` section.\n\
Return the exact numbers concisely.";

    let input = injector.build_initial_input(prompt, &envelope);
    let extra_tools = injector.extra_tool_definitions();

    let client = ResponsesClient::new(cfg);
    let result = client
        .run_loop(
            input,
            extra_tools,
            Some("You are verifying folded tool outputs. Rely strictly on tool outputs."),
            &store,
        )
        .map_err(|e| e.to_string())?;

    print_run_result(injector.name(), Some(&envelope.result_ref), &result);
    Ok(())
}

fn run_file(path: &str, prompt: &str, strategy: &str, cfg: GatewayConfig) -> Result<(), String> {
    let raw = fs::read_to_string(path).map_err(|e| format!("Failed to read `{path}`: {e}"))?;
    let injector = select_injector(strategy)?;
    let store = FoldStore::new();

    let (input, extra_tools, ref_id) = match store.ingest(raw) {
        IngestOutcome::Inline(content) => (
            vec![json!({
                "role": "user",
                "content": format!("{prompt}\n\n```text\n{content}\n```"),
            })],
            Vec::new(),
            None,
        ),
        IngestOutcome::Folded(env) => {
            let ref_id = env.result_ref.clone();
            (
                injector.build_initial_input(prompt, &env),
                injector.extra_tool_definitions(),
                Some(ref_id),
            )
        }
    };

    let client = ResponsesClient::new(cfg);
    let result = client
        .run_loop(input, extra_tools, None, &store)
        .map_err(|e| e.to_string())?;

    print_run_result(injector.name(), ref_id.as_deref(), &result);
    Ok(())
}

fn print_run_result(strategy: &str, result_ref: Option<&str>, result: &RunResult) {
    println!(
        "strategy={strategy} result_ref={}",
        result_ref.unwrap_or("inline")
    );
    for t in &result.turns {
        println!(
            "turn={} reasoning_items={} tool_calls={:?} input_tokens={} output_tokens={} reasoning_tokens={} total_tokens={}",
            t.turn,
            t.reasoning_item_count,
            t.function_calls,
            t.usage.input_tokens,
            t.usage.output_tokens,
            t.usage.reasoning_tokens,
            t.usage.total_tokens
        );
    }
    println!(
        "total_usage: input={} output={} reasoning={} total={}",
        result.total_usage.input_tokens,
        result.total_usage.output_tokens,
        result.total_usage.reasoning_tokens,
        result.total_usage.total_tokens
    );
    println!("--- FINAL ANSWER ---");
    println!("{}", result.final_text);
}

fn build_smoke_document() -> String {
    let mut s = String::from("# Incident Audit Report\r\n\r\n");
    s.push_str(
        "```markdown\r\n# Fake Heading Inside Fence\r\nlet example = \"ignored_inside_fence\";\r\n```\r\n\r\n",
    );
    s.push_str("## Events\r\n");
    // 110 lines (> 8 KiB) with deterministic counts:
    // DISK_FULL: 12 lines, TIMEOUT: 5 lines, AUTH_ERR: 2 lines (total 19 matching lines, 19 occurrences).
    for i in 1..=110 {
        let line = match i {
            1..=12 => format!("line={i:03} status=WARN ALERT_CODE=DISK_FULL host=node-{i:02} detail=storage_threshold_exceeded_padding_for_envelope_size_over_8k\r\n"),
            21..=25 => format!("line={i:03} status=ERR ALERT_CODE=TIMEOUT host=node-{i:02} detail=upstream_gateway_timeout_padding_for_envelope_size_over_8k\n"),
            41..=42 => format!("line={i:03} status=ERR ALERT_CODE=AUTH_ERR host=node-{i:02} detail=token_rejected_padding_for_envelope_size_over_8k\r\n"),
            _ => format!("line={i:03} status=OK host=node-{i:02} detail=heartbeat_nominal_padding_payload_to_guarantee_folding_threshold_exceeded\n"),
        };
        s.push_str(&line);
    }
    s.push_str("\r\n## Summary\r\n");
    s.push_str(
        "FINAL_NOTE: All 19 alerts came from cluster-west during maintenance window W-42.\r\n",
    );
    s
}

fn print_usage() {
    println!(
        "Usage:\n  \
         source_harness schema\n  \
         source_harness smoke [--strategy inline|native] [--model <m>] [--effort <e>]\n  \
         source_harness run --file <path> --prompt <text> [--strategy inline|native] [--model <m>] [--effort <e>]\n  \
         source_harness agent --root <dir> --prompt <text> [--model <m>] [--effort <e>]\n  \
         source_harness chat --root <dir> [--model <m>] [--effort <e>]\n  \
         source_harness eval [--root <dir>] [--model <m>] [--effort <e>] [--out <json_path>]\n  \
         source_harness bench [--workers <n>] [--strategy inline|native] [--limit <n>] [--model <m>] [--effort <e>] [--out <json_path>]\n\n\
         Defaults: --strategy inline, --effort low (override with OPENAI_REASONING_EFFORT or --effort)."
    );
}
