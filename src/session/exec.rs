use super::journal::Journal;
use super::tools::{parse, rejected, Reply};
use super::validation::save_workspace;
use crate::budget::truncate_utf8_bytes;
use crate::workspace::WorkspaceContext;
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub(super) const MAX_COMMAND_BYTES: usize = 32768;
pub(super) const MAX_STDIN_BYTES: usize = 65536;
pub(super) const DEFAULT_TIMEOUT_SECONDS: u64 = 60;
pub(super) const MAX_TIMEOUT_SECONDS: u64 = 300;
const STREAM_LIMIT: u64 = 512 * 1024;

#[derive(Deserialize)]
struct Args { command: String, timeout_seconds: Option<u64>, stdin: Option<String> }

pub(super) fn run(ctx: &WorkspaceContext, journal: &mut Journal, run: usize, operation: &str, args: &Value) -> Reply {
    let args: Args = match parse(args) { Ok(args) => args, Err(error) => return Ok(error) };
    let timeout = args.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECONDS);
    let stdin_bytes = args.stdin.as_deref().map_or(0, str::len);
    if args.command.trim().is_empty() || args.command.len() > MAX_COMMAND_BYTES
        || !(1..=MAX_TIMEOUT_SECONDS).contains(&timeout) || stdin_bytes > MAX_STDIN_BYTES
    {
        return Ok(rejected("INVALID_ARGUMENT", "Supply a command of at most 32768 bytes, optional stdin up to 65536 bytes, and a timeout of 1–300 seconds."));
    }
    if let Some(saved) = journal.events.iter().find(|e| e.kind == "exec_receipt" && e.data["operation"] == operation) {
        return Ok(saved.data.clone());
    }
    let before_workspace_revision = save_workspace(ctx, journal)?;
    if !journal.events.iter().any(|e| e.kind == "exec_intent" && e.data["operation"] == operation) {
        journal.append(run, "exec_intent", json!({
            "operation": operation, "command": args.command, "timeout_seconds": timeout,
            "stdin_bytes": stdin_bytes, "before_workspace_revision": before_workspace_revision,
        }))?;
    }
    let mut stdout = tempfile::tempfile().map_err(|err| err.to_string())?;
    let mut stderr = tempfile::tempfile().map_err(|err| err.to_string())?;
    let mut cmd = shell_command(&args.command);
    let stdin_cfg = if let Some(input) = &args.stdin {
        let mut input_file = tempfile::tempfile().map_err(|err| err.to_string())?;
        input_file.write_all(input.as_bytes()).map_err(|err| err.to_string())?;
        input_file.seek(SeekFrom::Start(0)).map_err(|err| err.to_string())?;
        Stdio::from(input_file)
    } else {
        Stdio::null()
    };
    cmd.current_dir(ctx.root()).stdin(stdin_cfg).env_remove("OPENAI_API_KEY")
        .stdout(stdout.try_clone().map_err(|e| e.to_string())?)
        .stderr(stderr.try_clone().map_err(|e| e.to_string())?);
    configure_process_group(&mut cmd);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => return Ok(rejected("EXEC_UNAVAILABLE", &format!("Command launch failed: {err}"))),
    };
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let (code, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status.code(), false),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            other => {
                kill_process_tree(&mut child);
                child.wait().map_err(|err| err.to_string())?;
                other.map_err(|err| err.to_string())?;
                break (None, true);
            }
        }
    };
    let (out, out_clipped) = capture(&mut stdout).map_err(|err| err.to_string())?;
    let (err, err_clipped) = capture(&mut stderr).map_err(|err| err.to_string())?;
    let complete = !out_clipped && !err_clipped;
    let mut text = String::from_utf8_lossy(&out).into_owned();
    if !err.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') { text.push('\n'); }
        text.push_str("stderr:\n");
        text.push_str(&String::from_utf8_lossy(&err));
    }
    let (preview, clipped) = truncate_utf8_bytes(&text, 2048);
    let folded = clipped || !complete;
    let workspace_revision = save_workspace(ctx, journal)?;
    let artifact = journal.artifact(run, json!({"kind": "exec", "operation": operation}), text.as_bytes())?;
    let mut receipt = json!({
        "status": if code == Some(0) { "ok" } else { "error" }, "operation": operation,
        "before_workspace_revision": before_workspace_revision, "workspace_revision": workspace_revision,
        "exit_code": code, "timed_out": timed_out,
        "streams": {"stdout": journal.put(&out)?, "stderr": journal.put(&err)?},
        "preview": preview, "output_complete": complete, "result_ref": artifact.result_ref,
        "total_bytes": text.len(), "total_lines": text.lines().count(),
        "delivery": if folded { "folded" } else { "inline" },
    });
    if code != Some(0) {
        let (err_code, reason) = if timed_out { ("EXEC_TIMEOUT", "Command timed out.") } else { ("EXEC_FAILED", "Command failed.") };
        receipt["code"] = json!(err_code);
        receipt["reason"] = json!(reason);
    }
    journal.append(run, "exec_receipt", receipt.clone())?;
    Ok(receipt)
}

fn shell_command(command: &str) -> Command {
    #[cfg(unix)]
    {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", command]);
        cmd
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut cmd = Command::new("cmd.exe");
        cmd.args(["/D", "/S", "/C"]);
        cmd.raw_arg(format!("\"{command}\""));
        cmd
    }
}

fn capture(file: &mut File) -> io::Result<(Vec<u8>, bool)> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take(STREAM_LIMIT + 1).read_to_end(&mut bytes)?;
    let clipped = bytes.len() > STREAM_LIMIT as usize;
    bytes.truncate(STREAM_LIMIT as usize);
    Ok((bytes, clipped))
}

pub(super) fn configure_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = cmd;
}

pub(super) fn kill_process_tree(child: &mut Child) {
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &child.id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(unix)]
    if child.id() > 0 {
        unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL); }
    }
    let _ = child.kill();
}
