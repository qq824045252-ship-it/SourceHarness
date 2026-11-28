use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

fn request(stream: &mut TcpStream) -> Value {
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut reader = BufReader::new(stream);
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
    let mut body = vec![0; length.unwrap()];
    reader.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn respond(stream: TcpStream, item: Value) {
    respond_many(stream, vec![item]);
}

fn respond_many(mut stream: TcpStream, items: Vec<Value>) {
    let event = json!({"type":"response.completed","response":{"output":items,"usage":{"total_tokens":15}}});
    let sse = format!("event: response.completed\ndata: {event}\n\n");
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}", sse.len()).unwrap();
}

fn call(id: &str, name: &str, args: Value) -> Value {
    json!({"type":"function_call","call_id":id,"name":name,"arguments":args.to_string()})
}

fn answer(text: &str) -> Value {
    json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]})
}

fn start_chat(root: &Path, gateway: &str) -> Child {
    Command::new(env!("CARGO_BIN_EXE_source_harness"))
        .args(["chat", "--root", root.to_str().unwrap(), "--session", "acceptance"])
        .env("OPENAI_BASE_URL", gateway)
        .env("OPENAI_MODEL", "fixture")
        .env_remove("SOURCE_HARNESS_TEST_INTERRUPT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn chat_logs(child: &mut Child) -> mpsc::Receiver<String> {
    let (sender, lines) = mpsc::channel();
    let stderr = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            if sender.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    lines
}

fn await_log(logs: &mpsc::Receiver<String>, match_text: &str) {
    while !logs.recv_timeout(Duration::from_secs(10)).unwrap().contains(match_text) {}
}

fn session_events(root: &Path) -> Vec<Value> {
    let raw = fs::read_to_string(root.join(".sourceharness/sessions/acceptance/events.jsonl")).unwrap();
    raw.lines().map(|line| serde_json::from_str(line).unwrap()).collect()
}

fn user_texts(input: &Value) -> Vec<&str> {
    input
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["role"] == "user")
        .map(|row| row["content"].as_str().unwrap())
        .collect()
}

fn invoke(root: &Path, gateway: &str, interrupt: bool, args: &[&str]) -> Output {
    invoke_with_interruption(root, gateway, interrupt.then_some("after_file_replace"), args)
}

fn invoke_with_interruption(root: &Path, gateway: &str, interrupt: Option<&str>, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_source_harness"));
    command
        .args(args)
        .args(["--root", root.to_str().unwrap(), "--session", "acceptance"])
        .env("OPENAI_BASE_URL", gateway)
        .env("OPENAI_MODEL", "fixture")
        .env_remove("SOURCE_HARNESS_TEST_INTERRUPT");
    if let Some(point) = interrupt {
        command.env("SOURCE_HARNESS_TEST_INTERRUPT", point);
    }
    command.output().unwrap()
}

fn compact_round(listener: &TcpListener, prior: &mut Option<Value>) -> (TcpStream, Value) {
    let (mut stream, _) = listener.accept().unwrap();
    let body = request(&mut stream);
    if let Some(previous) = prior {
        assert_eq!(body["instructions"], previous["instructions"]);
        assert_eq!(body["tools"], previous["tools"]);
        assert!(body["input"].as_array().unwrap().starts_with(previous["input"].as_array().unwrap()));
    } else {
        assert!(!body["tools"].to_string().contains("expected_revision"));
    }
    *prior = Some(body.clone());
    (stream, body)
}

fn last_output(body: &Value) -> &str {
    body["input"].as_array().unwrap().last().unwrap()["output"].as_str().unwrap()
}

#[test]
fn acknowledged_prompt_and_steer_survive_a_stop_during_the_model_request() {
    let root = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let gateway = format!("http://{}/v1", listener.local_addr().unwrap());
    let (upstream, blocked) = mpsc::channel();
    let (terminated, was_terminated) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut initial, _) = listener.accept().unwrap();
        assert_eq!(user_texts(&request(&mut initial)["input"]), ["Task A"]);
        upstream.send(()).unwrap();
        was_terminated.recv_timeout(Duration::from_secs(10)).unwrap();
        drop(initial);

        let (mut resumed, _) = listener.accept().unwrap();
        assert_eq!(user_texts(&request(&mut resumed)["input"]), ["Task A", "Correct A"]);
        respond(resumed, answer("A corrected."));
        let (mut followup, _) = listener.accept().unwrap();
        assert_eq!(user_texts(&request(&mut followup)["input"]), ["Task A", "Correct A", "Task B"]);
        respond(followup, answer("B done."));
    });

    let mut child = start_chat(root.path(), &gateway);
    let logs = chat_logs(&mut child);
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "Task A").unwrap();
    await_log(&logs, "Message received.");
    blocked.recv_timeout(Duration::from_secs(10)).unwrap();
    writeln!(stdin, "Task B").unwrap();
    await_log(&logs, "Message received.");
    writeln!(stdin, ":steer Correct A").unwrap();
    await_log(&logs, "Correction received.");
    child.kill().unwrap();
    child.wait().unwrap();
    drop(stdin);
    terminated.send(()).unwrap();

    let events = session_events(root.path());
    assert_eq!(events.iter().filter(|e| e["kind"] == "user").count(), 1);
    assert!(!events.iter().any(|e| e["kind"] == "steer"));
    let resumed = invoke(root.path(), &gateway, false, &["agent", "--resume"]);
    assert!(resumed.status.success(), "{}", String::from_utf8_lossy(&resumed.stderr));
    assert_eq!(String::from_utf8_lossy(&resumed.stdout).trim(), "A corrected.");
    let mut reopened = start_chat(root.path(), &gateway);
    drop(reopened.stdin.take());
    let result = reopened.wait_with_output().unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "B done.");
    server.join().unwrap();
    let events = session_events(root.path());
    let users: Vec<_> = events.iter().filter(|e| e["kind"] == "user").collect();
    let steers: Vec<_> = events.iter().filter(|e| e["kind"] == "steer").collect();
    assert_eq!(users.len(), 2);
    assert_eq!(users[1]["data"]["inbox_id"], 2);
    assert_eq!(steers.len(), 1);
    assert_eq!(steers[0]["data"]["inbox_id"], 3);
    assert_eq!(steers[0]["run"], users[0]["run"]);
}

#[test]
fn a_followup_waits_until_a_paused_run_resumes_and_finishes() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("notes.txt"), "0123456789".repeat(8)).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let gateway = format!("http://{}/v1", listener.local_addr().unwrap());
    let (upstream, started) = mpsc::channel();
    let (release, accepted) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut initial, _) = listener.accept().unwrap();
        assert_eq!(user_texts(&request(&mut initial)["input"]), ["Task A"]);
        upstream.send(()).unwrap();
        accepted.recv_timeout(Duration::from_secs(10)).unwrap();
        respond(initial, call("count_1", "read", json!({"path":"notes.txt","start_byte":0,"end_byte":1})));
        let limit = source_harness::transport::GatewayConfig::default().max_turns;
        for turn in 2..=limit {
            let (mut pending, _) = listener.accept().unwrap();
            let body = request(&mut pending);
            assert_eq!(user_texts(&body["input"]), ["Task A"]);
            respond(pending, call(&format!("count_{turn}"), "read", json!({"path":"notes.txt","start_byte":turn-1,"end_byte":turn})));
        }

        let (mut resumed, _) = listener.accept().unwrap();
        let request_body = request(&mut resumed);
        assert_eq!(user_texts(&request_body["input"]), ["Task A"]);
        assert_eq!(request_body["input"][2]["call_id"], "count_1");
        respond(resumed, answer("A done."));

        let (mut followup, _) = listener.accept().unwrap();
        assert_eq!(user_texts(&request(&mut followup)["input"]), ["Task A", "Task B"]);
        respond(followup, answer("B done."));
    });

    let mut child = start_chat(root.path(), &gateway);
    let logs = chat_logs(&mut child);
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "Task A").unwrap();
    await_log(&logs, "Message received.");
    started.recv_timeout(Duration::from_secs(10)).unwrap();
    writeln!(stdin, "Task B").unwrap();
    await_log(&logs, "Message received.");
    release.send(()).unwrap();
    await_log(&logs, "Model turn limit reached");
    let pending = session_events(root.path());
    assert_eq!(pending.iter().filter(|e| e["kind"] == "user").count(), 1);
    writeln!(stdin, ":resume").unwrap();
    drop(stdin);
    let result = child.wait_with_output().unwrap();
    assert!(result.status.success());
    assert_eq!(String::from_utf8_lossy(&result.stdout).lines().collect::<Vec<_>>(), ["A done.", "B done."]);
    server.join().unwrap();
    let events = session_events(root.path());
    let finish = events.iter().find(|e| e["kind"] == "model_response" && e["data"]["final_text"] == "A done.").unwrap();
    let next = events.iter().find(|e| e["kind"] == "user" && e["data"]["text"] == "Task B").unwrap();
    assert!(finish["seq"].as_u64().unwrap() < next["seq"].as_u64().unwrap());
    assert_eq!(next["data"]["inbox_id"], 2);
}

#[test]
fn continuation_preserves_sent_and_queued_steer_positions() {
    let root = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let gateway = format!("http://{}/v1", listener.local_addr().unwrap());
    let (upstream, started) = mpsc::channel();
    let (release, first_accepted) = mpsc::channel();
    let (terminated, was_terminated) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut initial, _) = listener.accept().unwrap();
        assert_eq!(user_texts(&request(&mut initial)["input"]), ["Task A"]);
        upstream.send(1).unwrap();
        first_accepted.recv_timeout(Duration::from_secs(10)).unwrap();
        respond(initial, answer("Interim response"));

        let (mut second, _) = listener.accept().unwrap();
        assert_eq!(user_texts(&request(&mut second)["input"]), ["Task A", "First correction"]);
        upstream.send(2).unwrap();
        was_terminated.recv_timeout(Duration::from_secs(10)).unwrap();
        drop(second);

        let (mut linked, _) = listener.accept().unwrap();
        let body = request(&mut linked);
        let input = &body["input"];
        assert_eq!(input[2], json!({"role":"user","content":"First correction"}));
        assert_eq!(input[3], json!({"role":"user","content":"Second correction"}));
        assert!(input[4]["content"].as_str().unwrap().contains("Clarify the continuation"));
        assert_eq!(input.as_array().unwrap().len(), 5);
        respond(linked, answer("Continued."));
    });
    let mut child = start_chat(root.path(), &gateway);
    let logs = chat_logs(&mut child);
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "Task A").unwrap();
    assert_eq!(started.recv_timeout(Duration::from_secs(10)).unwrap(), 1);
    writeln!(stdin, ":steer First correction").unwrap();
    await_log(&logs, "Correction received.");
    release.send(()).unwrap();
    assert_eq!(started.recv_timeout(Duration::from_secs(10)).unwrap(), 2);
    writeln!(stdin, ":steer Second correction").unwrap();
    await_log(&logs, "Correction received.");
    child.kill().unwrap();
    child.wait().unwrap();
    drop(stdin);
    terminated.send(()).unwrap();

    let continued = invoke(root.path(), &gateway, false, &["agent", "--continue", "--prompt", "Clarify the continuation"]);
    assert!(continued.status.success(), "{}", String::from_utf8_lossy(&continued.stderr));
    assert_eq!(String::from_utf8_lossy(&continued.stdout).trim(), "Continued.");
    server.join().unwrap();
    let events = session_events(root.path());
    let steers: Vec<_> = events.iter().filter(|e| e["kind"] == "steer").collect();
    assert_eq!(steers.len(), 2);
    assert_eq!(steers[0]["run"], steers[1]["run"]);
    let last_model = events.iter().rev().find(|e| e["kind"] == "model_request").unwrap();
    assert_eq!(last_model["data"]["steer_events"], json!([steers[0]["seq"], steers[1]["seq"]]));
    assert_eq!(last_model["data"]["steer_input_indices"], json!([2, 3]));
    assert_eq!(last_model["data"]["steer_input_start"], 2);
    let reloaded = invoke(root.path(), &gateway, false, &["agent", "--resume"]);
    assert!(reloaded.status.success(), "{}", String::from_utf8_lossy(&reloaded.stderr));
    assert_eq!(String::from_utf8_lossy(&reloaded.stdout).trim(), "Continued.");
}

#[test]
fn streams_a_running_session_and_steers_without_starting_queued_writes() {
    let root = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let gateway = format!("http://{}/v1", listener.local_addr().unwrap());
    let (release, correction_received) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut initial, _) = listener.accept().unwrap();
        let first = request(&mut initial);
        assert_eq!(first["input"][0]["content"], "Write the original files");
        let delta = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Working on the original files\\n\"}\n\n";
        let original = json!({"type":"response.completed","response":{"output":[
            call("first_old", "write", json!({"path":"old-a.txt","content":"old"})),
            call("second_old", "write", json!({"path":"old-b.txt","content":"old"})),
        ],"usage":{"total_tokens":15}}});
        let end = format!("event: response.completed\ndata: {original}\n\n");
        write!(initial, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{delta}", delta.len() + end.len()).unwrap();
        initial.flush().unwrap();
        correction_received.recv_timeout(Duration::from_secs(10)).unwrap();
        initial.write_all(end.as_bytes()).unwrap();
        drop(initial);

        let (mut redirected, _) = listener.accept().unwrap();
        let body = request(&mut redirected);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 6);
        for (index, id) in [(3, "first_old"), (4, "second_old")] {
            assert_eq!(input[index]["call_id"], id);
            let reply = input[index]["output"].as_str().unwrap();
            assert!(reply.starts_with("SUPERSEDED_BY_STEER"), "{reply}");
        }
        assert_eq!(input[5], json!({"role":"user","content":"Write corrected.txt instead"}));
        respond(redirected, call("corrected", "write", json!({"path":"corrected.txt","content":"corrected"})));

        let (mut last, _) = listener.accept().unwrap();
        let final_request = request(&mut last);
        assert!(final_request["input"][7]["output"].as_str().unwrap().starts_with("Wrote corrected.txt."));
        respond(last, answer("Updated corrected.txt."));
    });

    let mut child = start_chat(root.path(), &gateway);
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (text, lines) = mpsc::channel();
    let (progress, logs) = mpsc::channel();
    let read_stdout = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if text.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let read_stderr = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            if progress.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    writeln!(stdin, "Write the original files").unwrap();
    assert_eq!(lines.recv_timeout(Duration::from_secs(10)).unwrap(), "Working on the original files");
    writeln!(stdin, ":steer Write corrected.txt instead").unwrap();
    while logs.recv_timeout(Duration::from_secs(10)).unwrap() != "Correction received." {}
    release.send(()).unwrap();
    drop(stdin);
    assert!(child.wait().unwrap().success());
    server.join().unwrap();
    read_stdout.join().unwrap();
    read_stderr.join().unwrap();
    assert_eq!(lines.try_iter().filter(|line| line == "Updated corrected.txt.").count(), 1);
    assert!(!root.path().join("old-a.txt").exists());
    assert!(!root.path().join("old-b.txt").exists());
    assert_eq!(fs::read(root.path().join("corrected.txt")).unwrap(), b"corrected");
    let events = session_events(root.path());
    let steer = events.iter().find(|event| event["kind"] == "steer").unwrap();
    let redirected = events.iter().find(|event| event["kind"] == "model_request" && event["data"]["round"] == 2).unwrap();
    assert_eq!(steer["data"]["after_round"], 1);
    assert_eq!(redirected["data"]["steer_events"], json!([steer["seq"]]));
    assert_eq!(redirected["data"]["steer_input_start"], 5);
    assert_eq!(events.iter().filter(|event| event["kind"] == "tool_skipped").count(), 2);
    assert_eq!(events.iter().filter(|event| event["kind"] == "tool_attempt").count(), 1);
}

#[test]
fn compact_rejects_checks_configuration_in_the_workspace() {
    let root = tempfile::tempdir().unwrap();
    let checks = root.path().join("checks.json");
    fs::write(&checks, "{}").unwrap();
    let result = invoke(root.path(), "http://127.0.0.1:1/v1", false, &[
        "agent", "--checks", checks.to_str().unwrap(), "--prompt", "Run the check."
    ]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("Place the checks file outside the workspace"));
}

#[test]
fn compact_prefix_and_implicit_revision_hold_across_requests_and_followups() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("policy.txt");
    fs::write(&file, "keep=1\nlimit=30\n").unwrap();
    fs::write(root.path().join("notes.txt"), "previous\n").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let gateway = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let mut prior = None;
        let edit = |id: &str, old: &str, new: &str| call(id, "edit", json!({
            "path":"policy.txt","changes":[{"old_text":old,"new_text":new,"note":null}]
        }));
        let (stream, _) = compact_round(&listener, &mut prior);
        respond_many(stream, vec![
            edit("unobserved", "limit=30", "limit=40"),
            call("overwrite", "write", json!({
                "path":"notes.txt","content":"replacement\n","expected_revision":"extra field"
            })),
        ]);
        let (stream, req) = compact_round(&listener, &mut prior);
        let items = req["input"].as_array().unwrap();
        assert!(items[items.len()-2]["output"].as_str().unwrap().starts_with("Edited policy.txt."));
        assert!(last_output(&req).starts_with("Wrote notes.txt."));
        respond(stream, edit("inexact", "limit=999", "limit=50"));
        let (stream, req) = compact_round(&listener, &mut prior);
        assert!(last_output(&req).starts_with("TEXT_NOT_FOUND"));
        respond_many(stream, vec![
            call("read", "read", json!({"path":"policy.txt","limit":10,"paths":null})),
            edit("same_batch", "limit=40", "limit=50"),
        ]);
        let (stream, req) = compact_round(&listener, &mut prior);
        let items = req["input"].as_array().unwrap();
        let read_out = items[items.len()-2]["output"].as_str().unwrap();
        assert!(read_out.starts_with("\"policy.txt\" (lines 1-2/2, 16B, lf, r") && read_out.contains(")\n1\tkeep=1\n2\tlimit=40\n"), "{read_out}");
        assert!(last_output(&req).starts_with("Edited policy.txt."));
        fs::write(&file, "keep=2\nlimit=50\n").unwrap();
        respond(stream, edit("stale", "limit=50", "limit=60"));
        let (stream, req) = compact_round(&listener, &mut prior);
        assert!(last_output(&req).starts_with("REVISION_CONFLICT"));
        assert!(last_output(&req).contains("+keep=2"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "keep=2\nlimit=50\n");
        respond(stream, call("fresh", "read", json!({"path":"policy.txt","start_line":2})));
        let (stream, req) = compact_round(&listener, &mut prior);
        assert!(last_output(&req).starts_with("\"policy.txt\" (lines 2-2/2, 16B, lf, r") && last_output(&req).contains(")\n2\tlimit=50\n"), "{}", last_output(&req));
        respond(stream, call("edited", "edit", json!({"path":"policy.txt","changes":[
            {"old_text":"limit=50","new_text":"limit=60"},
            {"old_text":"keep=2","new_text":"keep=3"}
        ]})));
        let (stream, req) = compact_round(&listener, &mut prior);
        assert!(last_output(&req).starts_with("Edited policy.txt."));
        respond(stream, answer("Updated."));
        let (stream, req) = compact_round(&listener, &mut prior);
        assert_eq!(user_texts(&req["input"]), ["Update the limit.", "Adjust it again."]);
        respond(stream, edit("across_run", "limit=60", "limit=75"));
        let (stream, req) = compact_round(&listener, &mut prior);
        assert!(last_output(&req).starts_with("Edited policy.txt."));
        assert_eq!(fs::read_to_string(&file).unwrap(), "keep=3\nlimit=75\n");
        respond(stream, answer("Adjusted."));

        let (stream, _) = compact_round(&listener, &mut prior);
        respond(stream, edit("across_run_conflict", "limit=75", "limit=90"));
        let (stream, req) = compact_round(&listener, &mut prior);
        assert!(last_output(&req).starts_with("REVISION_CONFLICT"));
        assert!(last_output(&req).contains("+keep=4"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "keep=4\nlimit=75\n");
        fs::remove_file(&file).unwrap();
        respond(stream, call("unconfirmed_create", "write", json!({"path":"policy.txt","content":"limit=90\n"})));
        let (stream, req) = compact_round(&listener, &mut prior);
        assert!(last_output(&req).starts_with("REVISION_CONFLICT"));
        assert!(!file.exists());
        respond_many(stream, vec![
            call("missing", "read", json!({"path":"policy.txt"})),
            call("confirmed_create", "write", json!({"path":"policy.txt","content":"limit=90\n"})),
        ]);
        let (stream, req) = compact_round(&listener, &mut prior);
        assert!(last_output(&req).starts_with("Wrote policy.txt."));
        respond(stream, answer("Recreated the requested file."));
    });

    let run = |prompt| invoke(root.path(), &gateway, false, &["agent", "--prompt", prompt]);
    assert!(run("Update the limit.").status.success());
    assert!(run("Adjust it again.").status.success());
    fs::write(root.path().join("policy.txt"), "keep=4\nlimit=75\n").unwrap();
    assert!(run("Update or recreate the file.").status.success());
    server.join().unwrap();
    assert_eq!(fs::read_to_string(root.path().join("policy.txt")).unwrap(), "limit=90\n");
    assert_eq!(fs::read_to_string(root.path().join("notes.txt")).unwrap(), "replacement\n");
    let events = session_events(root.path());
    let read = events.iter().find(|event| event["data"]["call_id"] == "read" && event["kind"] == "tool_result").unwrap();
    assert_eq!(read["data"]["revision"].as_str().unwrap().len(), 64);
    assert!(read["data"]["live_observation"] == true);
}

#[test]
fn compact_batch_read_keeps_each_result_and_live_revision() {
    let root = tempfile::tempdir().unwrap();
    let a = root.path().join("a.txt");
    let b = root.path().join("b.txt");
    let missing = root.path().join("missing.txt");
    fs::write(&a, "alpha=1\n").unwrap();
    fs::write(&b, "beta=1\n").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let gateway = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let mut prior = None;
        let edit = |id, path, old, new| call(id, "edit", json!({
            "path":path, "changes":[{"old_text":old,"new_text":new}],
        }));
        let (stream, _) = compact_round(&listener, &mut prior);
        respond_many(stream, vec![
            call("batch", "read", json!({"paths":["a.txt","missing.txt","../outside.txt","./b.txt"],"path":null,"limit":10})),
            edit("early", "a.txt", "alpha=1", "alpha=2"),
        ]);
        let (stream, req) = compact_round(&listener, &mut prior);
        let items = req["input"].as_array().unwrap();
        let text = items[items.len() - 2]["output"].as_str().unwrap();
        assert!(text.contains("\"a.txt\" (lines 1-1/1, 8B, lf, r") && text.contains(")\n1\talpha=1\n"), "{text}");
        assert!(text.contains("FILE_NOT_FOUND") && text.contains("missing.txt"));
        assert!(text.contains("INVALID_PATH") && text.contains("../outside.txt"));
        assert!(text.contains("\"b.txt\" (lines 1-1/1, 7B, lf, r") && text.contains(")\n1\tbeta=1\n"), "{text}");
        assert!(last_output(&req).starts_with("Edited a.txt."));
        assert_eq!(fs::read_to_string(&a).unwrap(), "alpha=2\n");
        fs::write(&b, "beta=1\nexternal=true\n").unwrap();
        fs::write(&missing, "externally created\n").unwrap();
        respond_many(stream, vec![
            edit("stale_b", "b.txt", "beta=1", "beta=2"),
            call("stale_absent", "write", json!({"path":"missing.txt","content":"overwrite\n"})),
            call("outside", "write", json!({"path":"../outside.txt","content":"blocked\n"})),
        ]);
        let (stream, req) = compact_round(&listener, &mut prior);
        let items = req["input"].as_array().unwrap();
        let stale_b = items[items.len() - 3]["output"].as_str().unwrap();
        assert!(stale_b.starts_with("REVISION_CONFLICT"));
        assert!(stale_b.contains("+external=true"));
        assert!(items[items.len() - 2]["output"].as_str().unwrap().starts_with("REVISION_CONFLICT"));
        assert!(last_output(&req).starts_with("INVALID_PATH"));
        assert_eq!(fs::read_to_string(&missing).unwrap(), "externally created\n");
        respond(stream, call("mixed", "read", json!({"paths":["a.txt"],"path":"b.txt"})));
        let (stream, req) = compact_round(&listener, &mut prior);
        assert!(last_output(&req).starts_with("INVALID_ARGUMENT"));
        respond(stream, answer("Applied one edit."));
    });
    let result = invoke(root.path(), &gateway, false, &["agent", "--prompt", "Read the sources and update them."]);
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    server.join().unwrap();
    assert_eq!(fs::read_to_string(root.path().join("a.txt")).unwrap(), "alpha=2\n");
    assert_eq!(fs::read_to_string(root.path().join("b.txt")).unwrap(), "beta=1\nexternal=true\n");
    let events = session_events(root.path());
    let batch = events.iter().find(|event| event["kind"] == "tool_result" && event["data"]["call_id"] == "batch").unwrap();
    let observed = batch["data"]["live_observations"].as_array().unwrap();
    assert_eq!(observed.len(), 3);
    assert_eq!(observed[1], json!({"path":"missing.txt","revision":null}));
}

#[test]
fn compact_resumes_past_the_old_limit_without_turn_reminders() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("workspace");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("short.txt"), "short\n").unwrap();
    fs::write(root.join("long.txt"), "line\n".repeat(2800)).unwrap();
    let checks = fixture.path().join("checks.json");
    fs::write(&checks, json!({
        "acceptance":{"program":env!("CARGO_BIN_EXE_source_harness"),"args":["--help"]}
    }).to_string()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let gateway = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let mut prior = None;
        let limit = source_harness::transport::GatewayConfig::default().max_turns;
        assert_eq!(limit, 50);
        for round in 1..=limit {
            let (stream, req) = compact_round(&listener, &mut prior);
            assert!(!req["input"].to_string().contains("[Budget"));
            if round == 13 {
                respond(stream, call("interrupted_write", "write", json!({"path":"log.txt","content":"saved"})));
                continue;
            }
            match limit - round {
                0 => {
                    assert!(last_output(&req).starts_with("PASS acceptance. This revision is verified."));
                    assert!(last_output(&req).contains("Re-run validate after any edit."));
                    respond(stream, call("last_read", "read", json!({"path":"short.txt"})));
                }
                1 => {
                    assert!(last_output(&req).contains("More: read("));
                    assert!(last_output(&req).len() <= 8192);
                    respond(stream, call("validate_late", "validate", json!({"check":null,"limit":1})));
                }
                2 => respond(stream, call("last_batch", "read", json!({"paths":["short.txt","long.txt"]}))),
                _ => respond(stream, call(&format!("read_{round}"), "read", json!({"path":"short.txt"}))),
            }
        }
        let (stream, req) = compact_round(&listener, &mut prior);
        assert!(!last_output(&req).contains("[Budget"));
        respond(stream, answer("Validated and finished."));
    });
    let stopped = invoke(&root, &gateway, true, &["agent", "--checks", checks.to_str().unwrap(), "--prompt", "Inspect and verify."]);
    assert_eq!(stopped.status.code(), Some(86), "{}", String::from_utf8_lossy(&stopped.stderr));
    let args = ["agent", "--checks", checks.to_str().unwrap(), "--resume"];
    let paused = invoke(&root, &gateway, false, &args);
    assert!(!paused.status.success());
    assert!(String::from_utf8_lossy(&paused.stderr).contains("Model turn limit reached"));
    let result = invoke(&root, &gateway, false, &args);
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "Validated and finished.");
    server.join().unwrap();
}

#[test]
fn compact_rollback_reverts_edits_and_exec_while_tolerating_external_files() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("workspace");
    fs::create_dir(&root).unwrap();
    let policy = root.join("policy.txt");
    let external = root.join("unrelated.txt");
    fs::write(&policy, "limit=30\n").unwrap();
    fs::write(&external, "original\n").unwrap();
    let checks = fixture.path().join("checks.json");
    fs::write(&checks, json!({
        "acceptance":{"program":env!("CARGO_BIN_EXE_source_harness"),"args":["--help"]}
    }).to_string()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let gateway = format!("http://{}/v1", listener.local_addr().unwrap());
    let exec_cmd = if cfg!(windows) {
        "echo from-exec > exec_added.txt"
    } else {
        "printf 'from-exec\\n' > exec_added.txt"
    };
    let server = std::thread::spawn(move || {
        let mut prior = None;
        let edit = |id: &str, old: &str, new: &str| call(id, "edit", json!({
            "path":"policy.txt","changes":[{"old_text":old,"new_text":new}]
        }));
        let (stream, _) = compact_round(&listener, &mut prior);
        respond(stream, call("read", "read", json!({"path":"policy.txt"})));
        let (stream, _) = compact_round(&listener, &mut prior);
        respond(stream, edit("good", "limit=30", "limit=60"));
        let (stream, _) = compact_round(&listener, &mut prior);
        respond(stream, call("validate", "validate", json!({})));
        let (stream, req) = compact_round(&listener, &mut prior);
        assert!(last_output(&req).starts_with("PASS acceptance. This revision is verified."));
        respond_many(stream, vec![
            edit("bad", "limit=60", "limit=0"),
            call("create", "write", json!({"path":"z-added.txt","content":"temporary"})),
            call("exec_create", "exec", json!({"command": exec_cmd})),
        ]);
        let (stream, _) = compact_round(&listener, &mut prior);
        // Unrelated external temp file added outside this run does NOT block rollback.
        fs::write(&external, "outside\n").unwrap();
        fs::write(external.with_file_name("extra.tmp"), "ignore me\n").unwrap();
        respond(stream, call("rollback", "rollback", json!({})));
        let (stream, req) = compact_round(&listener, &mut prior);
        assert!(last_output(&req).starts_with("Restored 3 files"));
        assert_eq!(fs::read_to_string(&policy).unwrap(), "limit=60\n");
        assert!(!policy.with_file_name("z-added.txt").exists());
        assert!(!policy.with_file_name("exec_added.txt").exists());
        assert_eq!(fs::read_to_string(&external).unwrap(), "outside\n");
        respond(stream, edit("expired", "limit=0", "limit=20"));
        let (stream, req) = compact_round(&listener, &mut prior);
        assert!(last_output(&req).starts_with("TEXT_NOT_FOUND"));
        respond(stream, call("noop", "rollback", json!({})));
        let (stream, req) = compact_round(&listener, &mut prior);
        assert!(last_output(&req).starts_with("Restored 0 files"));
        // Modifying a file that WAS touched by this run externally still blocks rollback.
        respond(stream, edit("bad2", "limit=60", "limit=99"));
        let (stream, _) = compact_round(&listener, &mut prior);
        fs::write(&policy, "limit=external\n").unwrap();
        respond(stream, call("blocked", "rollback", json!({})));
        let (stream, req) = compact_round(&listener, &mut prior);
        assert!(last_output(&req).starts_with("ROLLBACK_CONFLICT"));
        respond(stream, answer("Restored the last passing version."));
    });
    let args = ["agent", "--checks", checks.to_str().unwrap()];
    let stopped = invoke_with_interruption(&root, &gateway, Some("during_rollback"), &[&args[..], &["--prompt", "Try the change."]].concat());
    assert_eq!(stopped.status.code(), Some(86), "{}", String::from_utf8_lossy(&stopped.stderr));
    let resumed = invoke(&root, &gateway, false, &[&args[..], &["--resume"]].concat());
    assert!(resumed.status.success(), "{}", String::from_utf8_lossy(&resumed.stderr));
    server.join().unwrap();
}

#[test]
fn compact_guard_pauses_and_chat_accepts_steer() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("workspace");
    fs::create_dir(&root).unwrap();
    let checks = fixture.path().join("checks.json");
    fs::write(&checks, json!({
        "acceptance":{"program":env!("CARGO_BIN_EXE_source_harness"),"args":["invalid-check-fixture"]}
    }).to_string()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let gateway = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let mut prior = None;
        let (stream, _) = compact_round(&listener, &mut prior);
        respond_many(stream, vec![call("first", "validate", json!({})), call("same_round", "validate", json!({}))]);
        let (stream, req) = compact_round(&listener, &mut prior);
        assert!(last_output(&req).starts_with("FAIL acceptance"));
        assert!(!last_output(&req).contains("VALIDATION_OSCILLATION"));
        respond(stream, call("second", "validate", json!({})));
        let (stream, req) = compact_round(&listener, &mut prior);
        assert!(last_output(&req).contains("VALIDATION_OSCILLATION"));
        respond_many(stream, vec![
            call("third", "validate", json!({})),
            call("skipped", "write", json!({"path":"unexpected.txt","content":"unwanted"})),
        ]);
        let (stream, req) = compact_round(&listener, &mut prior);
        assert_eq!(user_texts(&req["input"]), ["Fix the check.", "Stop retrying and report."]);
        assert!(req["input"].as_array().unwrap().iter().any(|item| {
            item["call_id"] == "skipped" && item["output"].as_str().is_some_and(|text| text.starts_with("LOOP_GUARD_PAUSED"))
        }));
        respond(stream, answer("The configured check failed."));
    });
    let path = checks.to_str().unwrap();
    let result = invoke(&root, &gateway, false, &["agent", "--checks", path, "--prompt", "Fix the check."]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("Loop guard paused"));
    let mut chat = Command::new(env!("CARGO_BIN_EXE_source_harness"))
        .args(["chat", "--root", root.to_str().unwrap(), "--session", "acceptance", "--checks", path])
        .env("OPENAI_BASE_URL", &gateway)
        .env("OPENAI_MODEL", "fixture")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(chat.stdin.take().unwrap(), ":steer Stop retrying and report.").unwrap();
    let result = chat.wait_with_output().unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "The configured check failed.");
    assert!(!root.join("unexpected.txt").exists());
    server.join().unwrap();
    let events = session_events(&root);
    assert!(events.iter().any(|event| event["kind"] == "paused" && event["data"]["reason"] == "LOOP_GUARD_TRIGGERED"));
    assert!(!events.iter().any(|event| event["kind"] == "tool_attempt" && event["data"]["call_id"] == "skipped"));
}

#[test]
fn compact_exec_runs_in_workspace_handles_timeout_and_folds_output() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("workspace");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("policy.txt"), "limit=30\n").unwrap();
    fs::write(root.join(".env"), "SECRET=1\n").unwrap();
    fs::write(root.join(".env.example"), "SECRET=placeholder\n").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let gateway = format!("http://{}/v1", listener.local_addr().unwrap());

    let long_cmd = if cfg!(windows) {
        "python -c \"print('x' * 2500); print('FOLDED_TAIL')\""
    } else {
        "python3 -c \"print('x' * 2500); print('FOLDED_TAIL')\""
    };
    let sleep_cmd = if cfg!(windows) {
        "python -c \"import time; time.sleep(10)\""
    } else {
        "sleep 10"
    };
    let ok_cmd = if cfg!(windows) { "echo done" } else { "printf done" };

    let server = std::thread::spawn(move || {
        let mut prior = None;
        let (stream, _) = compact_round(&listener, &mut prior);
        respond_many(stream, vec![
            call("env_ex", "read", json!({"path":".env.example"})),
            call("env_secret", "read", json!({"path":".env"})),
        ]);
        let (stream, req) = compact_round(&listener, &mut prior);
        let items = req["input"].as_array().unwrap();
        assert!(items[items.len() - 2]["output"].as_str().unwrap().contains("SECRET=placeholder"));
        assert!(last_output(&req).starts_with("INVALID_PATH"));

        respond(stream, call("exec", "exec", json!({"command": long_cmd, "note": null})));
        let (stream, req) = compact_round(&listener, &mut prior);
        let output = last_output(&req);
        assert!(output.contains("More output: read(result_ref="), "{output}");
        let reference = output.split_once("result_ref=\"").unwrap().1.split('"').next().unwrap();
        respond(stream, call("folded", "read", json!({"result_ref":reference,"start_line":2,"end_line":2})));
        let (stream, req) = compact_round(&listener, &mut prior);
        let folded = last_output(&req);
        assert!(folded.replace("\r\n", "\n").ends_with("2\tFOLDED_TAIL\n"), "{folded:?}");

        respond(stream, call("timed", "exec", json!({"command": sleep_cmd, "timeout_seconds": 1})));
        let (stream, req) = compact_round(&listener, &mut prior);
        assert!(last_output(&req).starts_with("EXEC_TIMEOUT"), "{}", last_output(&req));

        respond_many(stream, vec![
            call("too_long", "exec", json!({"command":"#".repeat(32769)})),
            call("over_timeout", "exec", json!({"command": ok_cmd, "timeout_seconds": 301})),
            call("max_timeout", "exec", json!({"command": ok_cmd, "timeout_seconds": 300})),
        ]);
        let (stream, req) = compact_round(&listener, &mut prior);
        let items = req["input"].as_array().unwrap();
        assert!(items[items.len()-3]["output"].as_str().unwrap().starts_with("INVALID_ARGUMENT"));
        assert!(items[items.len()-2]["output"].as_str().unwrap().starts_with("INVALID_ARGUMENT"));
        assert!(last_output(&req).starts_with("done"));
        assert!(!last_output(&req).contains("Sandbox changes discarded."));
        respond(stream, answer("Cross-platform execution finished."));
    });
    let result = invoke(&root, &gateway, false, &["agent", "--prompt", "Inspect locally."]);
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    server.join().unwrap();
    let events = session_events(&root);
    assert!(events.iter().any(|event| event["kind"] == "exec_receipt" && event["data"]["delivery"] == "folded"));
    let timeouts: Vec<_> = events.iter().filter(|event| event["kind"] == "exec_intent")
        .map(|event| event["data"]["timeout_seconds"].as_u64().unwrap()).collect();
    assert_eq!(timeouts, [60, 1, 300]);
}

#[test]
fn resumes_an_applied_edit_and_retains_exact_old_content() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("config.txt"), b"primary\r\nalpha: 30\r\narchive\r\nalpha: 30\r\n").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let gateway = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut unavailable, _) = listener.accept().unwrap();
        let failed = request(&mut unavailable);
        write!(unavailable, "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        drop(unavailable);
        let (mut first, _) = listener.accept().unwrap();
        let initial = request(&mut first);
        assert_eq!(initial, failed);
        respond(first, call("read_1", "read", json!({"path":"config.txt","start_line":1,"line_count":2})));

        let (mut second, _) = listener.accept().unwrap();
        request(&mut second);
        respond(second, call("edit_1", "edit", json!({
            "path":"config.txt",
            "changes":[{"old_text":"primary\r\nalpha: 30","new_text":"primary\r\nalpha: 60"}]
        })));

        let (mut third, _) = listener.accept().unwrap();
        let resumed = request(&mut third);
        let output = resumed["input"][4]["output"].as_str().unwrap();
        assert_eq!(resumed["input"][4]["call_id"], "edit_1");
        assert!(output.starts_with("Edited config.txt."));
        respond(third, answer("Primary alpha is 60."));
    });

    let stopped = invoke(root.path(), &gateway, true, &["agent", "--prompt", "Raise the primary alpha setting"]);
    assert_eq!(stopped.status.code(), Some(86), "{}", String::from_utf8_lossy(&stopped.stderr));
    assert_eq!(fs::read(root.path().join("config.txt")).unwrap(), b"primary\r\nalpha: 60\r\narchive\r\nalpha: 30\r\n");

    let journal = root.path().join(".sourceharness/sessions/acceptance/events.jsonl");
    OpenOptions::new().append(true).open(&journal).unwrap().write_all(b"{\"seq\":").unwrap();

    let resumed = invoke(root.path(), &gateway, false, &["agent", "--resume"]);
    assert!(resumed.status.success(), "{}", String::from_utf8_lossy(&resumed.stderr));
    assert_eq!(String::from_utf8_lossy(&resumed.stdout).trim(), "Primary alpha is 60.");
    server.join().unwrap();
    let events = session_events(root.path());
    let receipt = events.iter().find(|e| e["kind"] == "edit_receipt").unwrap();
    assert_eq!(receipt["data"]["outcome"], "observed_target_on_recovery");
    assert_eq!(events.iter().filter(|e| e["kind"] == "model_error").count(), 1);
}

#[test]
fn timeline_history_surgical_rollback_and_validate_scope_hint() {
    let py = ["python3", "python", "python.exe", "python3.exe"].into_iter().find_map(|name| {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).map(|d| d.join(name)).find(|p| p.is_file())
    }).expect("python executable in PATH");
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("workspace");
    let checks = fixture.path().join("checks.json");
    let checker = fixture.path().join("check.py");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.txt"), "a-v1\n").unwrap();
    fs::write(root.join("b.txt"), "b-v1\n").unwrap();
    fs::write(root.join("never_read.txt"), "initial-bytes\n").unwrap();
    fs::write(&checker, "import pathlib, sys\nsys.exit(0 if pathlib.Path('a.txt').read_text() == 'a-v1\\n' else 1)\n").unwrap();
    fs::write(&checks, serde_json::to_string(&json!({
        "unit": { "program": py, "args": [checker], "timeout_seconds": 10 }
    })).unwrap()).unwrap();
    let exec_cmd = format!("{} -c \"open('b.txt', 'wb').write(b'b-v2\\n')\"", if cfg!(windows) { "python" } else { "python3" });
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let gateway = format!("http://{}/v1", listener.local_addr().unwrap());
    let root_for_server = root.clone();
    let server = std::thread::spawn(move || {
        let mut prior = None;
        let (stream, _) = compact_round(&listener, &mut prior);
        respond(stream, call("e_a", "edit", json!({"path":"a.txt","changes":[{"old_text":"a-v1","new_text":"a-v2"}]})));
        let (stream, _) = compact_round(&listener, &mut prior);
        respond(stream, call("x_b", "exec", json!({"command": exec_cmd})));
        let (stream, _) = compact_round(&listener, &mut prior);
        respond(stream, call("v_fail", "validate", json!({})));
        let (stream, req) = compact_round(&listener, &mut prior);
        let v_out = last_output(&req);
        assert!(v_out.contains("FAIL unit") && v_out.contains("Changed since checkpoint:") && v_out.contains("a.txt"), "{v_out}");
        respond(stream, call("hist_all", "read", json!({"history":true})));
        let (stream, req) = compact_round(&listener, &mut prior);
        let h_out = last_output(&req);
        assert!(h_out.contains(" edit path=\"a.txt\"") && h_out.contains(" exec ") && h_out.contains(" validate check=\"unit\" FAIL"), "{h_out}");
        respond(stream, call("hist_nr", "read", json!({"history":true,"path":"never_read.txt"})));
        let (stream, req) = compact_round(&listener, &mut prior);
        let nr_out = last_output(&req);
        assert!(nr_out.contains("initial_snapshot") && nr_out.contains("result_ref="), "{nr_out}");
        let nr_ref = nr_out.split_once("result_ref=\"").unwrap().1.split('"').next().unwrap().to_owned();
        respond(stream, call("read_nr", "read", json!({"result_ref": nr_ref})));
        let (stream, req) = compact_round(&listener, &mut prior);
        assert!(last_output(&req).contains("initial-bytes"), "{}", last_output(&req));
        respond(stream, call("rb_dry", "rollback", json!({"paths":["a.txt"],"dry_run":true})));
        let (stream, req) = compact_round(&listener, &mut prior);
        let dry_out = last_output(&req);
        assert!(dry_out.contains("Dry-run rollback") && dry_out.contains("-a-v2") && dry_out.contains("+a-v1"), "{dry_out}");
        respond(stream, call("rb_a", "rollback", json!({"paths":["a.txt"]})));
        let (stream, req) = compact_round(&listener, &mut prior);
        assert!(last_output(&req).starts_with("Restored 1 files"), "rb_a={}", last_output(&req));
        assert_eq!(fs::read_to_string(root_for_server.join("a.txt")).unwrap(), "a-v1\n");
        assert_eq!(fs::read_to_string(root_for_server.join("b.txt")).unwrap(), "b-v2\n");
        let events_now = session_events(&root_for_server);
        let exec_seq = events_now.iter().find(|e| e["kind"] == "exec_receipt").unwrap()["seq"].as_u64().unwrap();
        respond(stream, call("rb_exec", "rollback", json!({"to_event": exec_seq, "paths":["b.txt"]})));
        let (stream, req) = compact_round(&listener, &mut prior);
        assert!(last_output(&req).starts_with("Restored 1 files"), "rb_exec={}", last_output(&req));
        respond(stream, answer("Timeline and surgical rollback complete."));
    });
    let res = invoke(&root, &gateway, false, &["agent", "--checks", checks.to_str().unwrap(), "--prompt", "Test timeline."]);
    assert!(res.status.success(), "{}", String::from_utf8_lossy(&res.stderr));
    server.join().unwrap();
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "a-v1\n");
    assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "b-v1\n");
}

#[test]
fn phase2_intra_turn_parallel_validation_and_phase3_prefix_preserving_branch() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("workspace");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("note.txt"), "init\n").unwrap();
    let ext = tempfile::tempdir().unwrap();
    let checks = ext.path().join("checks.json");
    #[cfg(windows)]
    {
        let make_slow = |name: &str| {
            let p = ext.path().join(format!("{name}.cmd"));
            fs::write(&p, "@echo off\r\nping -n 2 127.0.0.1 > nul\r\nfindstr /C:\"good\" note.txt > nul\r\n").unwrap();
            p
        };
        let (sa, sb) = (make_slow("slow_a"), make_slow("slow_b"));
        let cmd_exe = std::env::var("COMSPEC").unwrap_or_else(|_| "C:\\Windows\\System32\\cmd.exe".to_owned());
        fs::write(&checks, serde_json::to_vec(&json!({
            "slow_a": {"program": cmd_exe, "args": ["/C", sa.to_str().unwrap()], "description": "Slow check A"},
            "slow_b": {"program": cmd_exe, "args": ["/C", sb.to_str().unwrap()], "description": "Slow check B"}
        })).unwrap()).unwrap();
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let make_slow = |name: &str| {
            let p = ext.path().join(format!("{name}.sh"));
            fs::write(&p, "#!/bin/sh\nsleep 0.5\ngrep -q good note.txt\n").unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        let (sa, sb) = (make_slow("slow_a"), make_slow("slow_b"));
        fs::write(&checks, serde_json::to_vec(&json!({
            "slow_a": {"program": sa.to_str().unwrap(), "args": [], "description": "Slow check A"},
            "slow_b": {"program": sb.to_str().unwrap(), "args": [], "description": "Slow check B"}
        })).unwrap()).unwrap();
    }

    // Phase 2: 2 concurrent validate calls in one turn + write barrier (edit followed by read).
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let gateway = format!("http://{}/v1", listener.local_addr().unwrap());
    let root_for_server = root.clone();
    let saved_prefix = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let saved_prefix_srv = saved_prefix.clone();
    let good_seq = std::sync::Arc::new(std::sync::Mutex::new(0usize));
    let good_seq_srv = good_seq.clone();

    let server = std::thread::spawn(move || {
        let mut prior = None;
        let (stream, _) = compact_round(&listener, &mut prior);
        respond(stream, call("e_good", "edit", json!({"path":"note.txt","changes":[{"old_text":"init","new_text":"good"}]})));
        let (stream, _) = compact_round(&listener, &mut prior);
        let t0 = std::time::Instant::now();
        respond_many(stream, vec![
            call("val_a", "validate", json!({"check":"slow_a"})),
            call("val_b", "validate", json!({"check":"slow_b"})),
        ]);
        let (stream, req) = compact_round(&listener, &mut prior);
        let wall_ms = t0.elapsed().as_millis();
        let items = req["input"].as_array().unwrap();
        let out_a = &items[items.len() - 2];
        let out_b = &items[items.len() - 1];
        assert_eq!(out_a["call_id"], "val_a");
        assert_eq!(out_b["call_id"], "val_b");
        assert!(out_a["output"].as_str().unwrap().starts_with("PASS slow_a"));
        assert!(out_b["output"].as_str().unwrap().starts_with("PASS slow_b"));
        let evs = session_events(&root_for_server);
        let elapsed_sum: u128 = evs.iter()
            .filter(|e| e["kind"] == "tool_result" && (e["data"]["call_id"] == "val_a" || e["data"]["call_id"] == "val_b"))
            .filter_map(|e| e["data"]["elapsed_ms"].as_u64())
            .map(u128::from)
            .sum();
        assert!(wall_ms < elapsed_sum, "expected parallel wall_ms ({wall_ms}) < sum ({elapsed_sum})");
        *saved_prefix_srv.lock().unwrap() = items.clone();
        *good_seq_srv.lock().unwrap() = evs.last().unwrap()["seq"].as_u64().unwrap() as usize;

        // Dead-end edit after the good checkpoint, then finish main session with a dead-end state.
        respond(stream, call("e_bad", "edit", json!({"path":"note.txt","changes":[{"old_text":"good","new_text":"dead-end"}]})));
        let (stream, _) = compact_round(&listener, &mut prior);
        respond(stream, answer("Dead end reached."));
        listener
    });
    let res = invoke(&root, &gateway, false, &["agent", "--checks", checks.to_str().unwrap(), "--prompt", "Try task."]);
    assert!(res.status.success(), "{}", String::from_utf8_lossy(&res.stderr));
    let listener = server.join().unwrap();
    assert_eq!(fs::read_to_string(root.join("note.txt")).unwrap(), "dead-end\n");

    // Phase 3: Branch back to good_seq, verify workspace rewound and exact 0..K prefix preserved!
    let k = *good_seq.lock().unwrap();
    let br = Command::new(env!("CARGO_BIN_EXE_source_harness"))
        .args(["branch", "--root", root.to_str().unwrap(), "--from", "acceptance", "--to", "alt", "--event", &k.to_string(), "--lesson", "Avoid dead-end edit"])
        .output().unwrap();
    assert!(br.status.success(), "{}", String::from_utf8_lossy(&br.stderr));
    assert_eq!(fs::read_to_string(root.join("note.txt")).unwrap(), "good\n");

    let hist = Command::new(env!("CARGO_BIN_EXE_source_harness"))
        .args(["history", "--root", root.to_str().unwrap(), "--session", "alt"])
        .output().unwrap();
    assert!(hist.status.success());
    let hist_out = String::from_utf8_lossy(&hist.stdout);
    assert!(hist_out.contains("# branch from \"acceptance\" at event "), "{hist_out}");

    let expected_prefix = saved_prefix.lock().unwrap().clone();
    let server2 = std::thread::spawn(move || {
        let mut prior = None;
        let (stream, req) = compact_round(&listener, &mut prior);
        let alt_items = req["input"].as_array().unwrap();
        assert_eq!(&alt_items[..expected_prefix.len()], &expected_prefix[..], "0..K prefix must stay byte-identical for KV cache");
        let last = alt_items.last().unwrap();
        assert_eq!(last["role"], "user");
        assert!(last["content"].as_str().unwrap().contains("Lesson from prior branch: Avoid dead-end edit"), "{last}");
        respond(stream, answer("Recovered on branch alt."));
    });
    let res2 = Command::new(env!("CARGO_BIN_EXE_source_harness"))
        .args(["agent", "--root", root.to_str().unwrap(), "--session", "alt", "--checks", checks.to_str().unwrap(), "--resume"])
        .env("OPENAI_BASE_URL", &gateway)
        .env("OPENAI_API_KEY", "fixture-key")
        .env("OPENAI_MODEL", "fixture")
        .env("OPENAI_REASONING_EFFORT", "low")
        .env_remove("SOURCE_HARNESS_TEST_INTERRUPT")
        .output().unwrap();
    assert!(res2.status.success(), "{}", String::from_utf8_lossy(&res2.stderr));
    assert!(String::from_utf8_lossy(&res2.stdout).contains("Recovered on branch alt."));
    server2.join().unwrap();
}
