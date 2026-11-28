use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::mpsc;

use source_harness::session::{self, Commands, Options, Session};
use source_harness::transport::{GatewayConfig, ResponsesClient, RunObserver, ToolCallTrace, TurnMetrics};

fn main() {
    if let Err(err) = run_cli() {
        eprintln!("Error: {err}");
        process::exit(1);
    }
}

fn open_session(args: &[String], require_session: bool) -> Result<(PathBuf, String, Session), String> {
    let root = PathBuf::from(parse_flag(args, "--root")?.unwrap_or_else(|| ".".to_owned()));
    let name = match (parse_flag(args, "--session")?, require_session) {
        (Some(n), _) => n,
        (None, true) => return Err("Missing --session <name>".to_owned()),
        (None, false) => "default".to_owned(),
    };
    let commands = match parse_flag(args, "--checks")? {
        Some(path) => Commands::from_compact_file(Path::new(&path), &root)?,
        None => Commands::default(),
    };
    let session = Session::open(&root, &name, Options { commands })?;
    Ok((root, name, session))
}

fn run_cli() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().map(String::as_str) else {
        print_usage();
        return Ok(());
    };
    match cmd {
        "agent" => {
            let (_, _, mut session) = open_session(&args[1..], false)?;
            let cfg = apply_gateway_flags(GatewayConfig::from_env(), &args[1..])?;
            let prompt = parse_flag(&args[1..], "--prompt")?;
            let resume = args[1..].iter().any(|a| a == "--resume");
            let cont = args[1..].iter().any(|a| a == "--continue");
            if (resume && (cont || prompt.is_some())) || (!resume && !cont && prompt.is_none()) {
                return Err("Supply --prompt, --resume, or --continue with an optional --prompt correction.".to_owned());
            }
            let client = ResponsesClient::new(cfg);
            let out = if cont { session.continue_pending(&client, prompt.as_deref())? } else { session.run(&client, prompt.as_deref())? };
            println!("{out}");
            Ok(())
        }
        "chat" => {
            let (_, _, session) = open_session(&args[1..], false)?;
            let cfg = apply_gateway_flags(GatewayConfig::from_env(), &args[1..])?;
            run_session_chat(session, ResponsesClient::new(cfg))
        }
        "history" => {
            let root = PathBuf::from(parse_flag(&args[1..], "--root")?.unwrap_or_else(|| ".".to_owned()));
            let name = parse_flag(&args[1..], "--session")?.ok_or("Missing --session <name>")?;
            let reference = parse_flag(&args[1..], "--ref")?;
            let event = parse_flag(&args[1..], "--event")?.map(|r| r.parse()).transpose().map_err(|_| "Invalid --event number".to_owned())?;
            print!("{}", session::inspect(&root, &name, reference.as_deref(), event)?);
            Ok(())
        }
        "fork" => {
            let root = PathBuf::from(parse_flag(&args[1..], "--root")?.unwrap_or_else(|| ".".to_owned()));
            let name = parse_flag(&args[1..], "--session")?.ok_or("Missing --session <name>")?;
            let manifest = parse_flag(&args[1..], "--revision")?.ok_or("Missing --revision <workspace_revision>")?;
            let dest = parse_flag(&args[1..], "--to")?.ok_or("Missing --to <new-directory>")?;
            session::fork_snapshot(&root, &name, &manifest, &PathBuf::from(&dest))?;
            println!("Restored snapshot {manifest} to {dest}");
            Ok(())
        }
        "-h" | "--help" | "help" => {
            print_usage();
            Ok(())
        }
        other => Err(format!("Unknown subcommand: {other}")),
    }
}

fn parse_flag(args: &[String], flag: &str) -> Result<Option<String>, String> {
    args.iter()
        .position(|a| a == flag)
        .map(|i| args.get(i + 1).cloned().ok_or_else(|| format!("Missing value for {flag}")))
        .transpose()
}

fn apply_gateway_flags(mut cfg: GatewayConfig, args: &[String]) -> Result<GatewayConfig, String> {
    if let Some(model) = parse_flag(args, "--model")? {
        cfg.model = model;
    }
    if let Some(effort) = parse_flag(args, "--effort")? {
        cfg.reasoning_effort = Some(effort);
    }
    if let Some(raw) = parse_flag(args, "--max-turns")? {
        let parsed: usize = raw.parse().map_err(|_| format!("Invalid --max-turns `{raw}`"))?;
        if parsed == 0 {
            return Err("--max-turns must be >= 1".to_owned());
        }
        cfg.max_turns = parsed;
    }
    Ok(cfg)
}

fn run_session_chat(mut session: Session, client: ResponsesClient) -> Result<(), String> {
    let (sender, input) = mpsc::channel();
    let inbox = session.input_inbox();
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else {
                let _ = sender.send(Err("Failed reading stdin".to_owned()));
                break;
            };
            let text = line.trim();
            let signal = match text {
                "" => continue,
                ":quit" | ":exit" => {
                    let _ = sender.send(Ok(ChatSignal::Exit));
                    break;
                }
                ":resume" => ChatSignal::Resume,
                ":steer" => {
                    eprintln!("Supply the instruction after :steer.");
                    continue;
                }
                _ => {
                    let correction = steer_text(text);
                    if let Err(err) = inbox.accept(correction.unwrap_or(text).to_owned(), correction.is_some()) {
                        let _ = sender.send(Err(err));
                        break;
                    }
                    eprintln!("{}", if correction.is_some() { "Correction received." } else { "Message received." });
                    if correction.is_some() { ChatSignal::Correction } else { ChatSignal::Prompt }
                }
            };
            if sender.send(Ok(signal)).is_err() {
                break;
            }
        }
    });
    eprintln!("Use :steer <instruction> to redirect an active run, :resume to resume, or :quit to exit.");
    if session.is_pending()? && (!session.is_loop_paused()? || session.has_inbox_steering()?) {
        run_chat_turn(&mut session, &client, None);
    }
    loop {
        if let Some(id) = session.next_inbox_prompt()? {
            run_chat_turn(&mut session, &client, Some(id));
            continue;
        }
        let Ok(signal) = input.recv() else { break };
        match signal? {
            ChatSignal::Exit => break,
            ChatSignal::Resume => run_chat_turn(&mut session, &client, None),
            ChatSignal::Correction if session.has_inbox_steering()? => run_chat_turn(&mut session, &client, None),
            ChatSignal::Prompt | ChatSignal::Correction => {}
        }
    }
    Ok(())
}

enum ChatSignal { Prompt, Correction, Resume, Exit }

fn steer_text(text: &str) -> Option<&str> {
    let (cmd, rest) = text.split_once(char::is_whitespace)?;
    let rest = rest.trim();
    (cmd == ":steer" && !rest.is_empty()).then_some(rest)
}

fn run_chat_turn(session: &mut Session, client: &ResponsesClient, prompt: Option<usize>) {
    let mut observer = StreamingTerminalObserver::default();
    let res = match prompt {
        Some(id) => session.run_inbox_prompt(client, id, &mut observer),
        None => session.run_observed(client, None, &mut observer, &mut || None),
    };
    match res {
        Ok(text) if !observer.streamed => println!("{text}"),
        Ok(_) => {}
        Err(err) => {
            if observer.printed {
                println!();
            }
            eprintln!("Error: {err}");
        }
    }
}

#[derive(Default)]
struct StreamingTerminalObserver { printed: bool, streamed: bool }

impl RunObserver for StreamingTerminalObserver {
    fn on_turn_start(&mut self, _turn: usize) { self.printed = false; self.streamed = false; }

    fn on_text_delta(&mut self, delta: &str) {
        print!("{delta}");
        let _ = io::stdout().flush();
        self.printed = true;
        self.streamed = true;
    }

    fn on_tool_call(&mut self, t: &ToolCallTrace) {
        if self.printed { println!(); self.printed = false; }
        eprintln!(
            "[tool] turn={} name={} status={} delivery={} ref={} err={} bytes={} {}ms",
            t.turn, t.name, t.status, t.delivery.as_deref().unwrap_or("-"),
            t.result_ref.as_deref().unwrap_or("-"), t.error_code.as_deref().unwrap_or("-"),
            t.response_bytes, t.duration_ms
        );
    }

    fn on_turn_complete(&mut self, m: &TurnMetrics) {
        if self.printed { println!(); self.printed = false; }
        eprintln!(
            "[turn] turn={} reasoning_items={} tools={:?} tokens(in={}, out={}, reason={}, total={}) {}ms",
            m.turn, m.reasoning_item_count, m.function_calls,
            m.usage.input_tokens, m.usage.output_tokens, m.usage.reasoning_tokens, m.usage.total_tokens, m.latency_ms
        );
    }
}

fn print_usage() {
    println!(
        "Usage:\n  \
         source_harness agent --root <dir> [--session <name>] (--prompt <text> | --resume | --continue [--prompt <correction>]) [--checks <file>] [--model <m>] [--effort <e>] [--max-turns <n>]\n  \
         source_harness chat --root <dir> [--session <name>] [--checks <file>] [--model <m>] [--effort <e>] [--max-turns <n>]\n  \
         source_harness history --root <dir> --session <name> [--ref <result_ref> | --event <seq>]\n  \
         source_harness fork --root <dir> --session <name> --revision <workspace_revision> --to <new-directory>"
    );
}
