mod model;
mod roundtrip;
mod tools;
use serde_json::json;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    process::ExitCode,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] | ["--help" | "-h"] => {
            println!("SourceHarness: Rust tool-calling test harness\n\nUsage: sourceharness [--help | --version | smoke-model | smoke-declare | smoke-execute | roundtrip <3..100> | compare-error-contract <3..100>]");
            ExitCode::SUCCESS
        }
        ["--version" | "-V"] => {
            println!("sourceharness {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        [mode @ ("smoke-declare" | "smoke-execute")] => {
            match tools::smoke(*mode == "smoke-execute").await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("{e}");
                    ExitCode::FAILURE
                }
            }
        }
        [mode @ ("roundtrip" | "compare-error-contract"), n] => match n.parse::<usize>() {
            Ok(n @ 3..=100) => match if *mode == "compare-error-contract" {
                roundtrip::compare_error_contract(n).await
            } else {
                roundtrip::run(n).await
            } {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("{e}");
                    ExitCode::FAILURE
                }
            },
            _ => {
                eprintln!("Use a sample count from 3 to 100.");
                ExitCode::from(2)
            }
        },
        ["smoke-model"] => match smoke_model().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!("Unknown arguments; use --help.");
            ExitCode::from(2)
        }
    }
}

async fn smoke_model() -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all("evidence")?;
    let epoch = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let path = format!("evidence/model-{epoch}-{}.jsonl", std::process::id());
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut out = options.open(&path)?;
    let mut all = true;
    for (id, stream, expected) in [("M01", false, "M01-ready"), ("M02", true, "流式验证·完成")]
    {
        let req = model::request(
            stream,
            &format!("Reply with exactly this text and nothing else: {expected}"),
        );
        let started = Instant::now();
        let result = model::send(&req).await;
        let (pass, detail) = match result {
            Ok((reply, wire)) => {
                let pass = reply.text.trim() == expected && reply.calls.is_empty();
                (
                    pass,
                    json!({"reply": reply, "response": model::safe_wire(&wire, stream).ok()}),
                )
            }
            Err(err) => (false, json!({"error_class":err.to_string()})),
        };
        all &= pass;
        let outcome = if pass { "PASS" } else { "FAIL" };
        writeln!(
            out,
            "{}",
            json!({"test_id":id,"attempt_id":format!("{epoch}-{id}"),"request":req,"provider":"Magpie/OpenCode Go","platform":std::env::consts::OS,"elapsed_ms":started.elapsed().as_millis(),"expected":expected,"status":outcome,"detail":detail})
        )?;
        println!("{id}: {outcome}");
    }
    out.sync_all()?;
    println!("Evidence: {path}");
    if all {
        Ok(())
    } else {
        Err("Model smoke did not pass".into())
    }
}
