use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match (args.next().as_deref(), args.next()) {
        (None | Some("--help" | "-h"), None) => {
            println!("SourceHarness: Rust tool-calling test harness\n\nUsage: sourceharness [--help | --version]\n\nStage 1 / M01–M04: NOT_RUN; see docs/stage-1-tool-calling.md.");
            ExitCode::SUCCESS
        }
        (Some("--version" | "-V"), None) => {
            println!("sourceharness {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("Unknown arguments; use --help.");
            ExitCode::from(2)
        }
    }
}
