use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

fn request(stream: &mut TcpStream) -> Value {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
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

fn respond(mut stream: TcpStream, item: Value) {
    let event = json!({"type":"response.completed","response":{"output":[item],"usage":{"total_tokens":15}}});
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
        .env("OPENAI_BASE_URL", gateway).env("OPENAI_MODEL", "fixture")
        .env_remove("SOURCE_HARNESS_TEST_INTERRUPT")
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().unwrap()
}

fn chat_logs(child: &mut Child) -> mpsc::Receiver<String> {
    let (sender, lines) = mpsc::channel();
    let stderr = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            if sender.send(line.unwrap()).is_err() { break; }
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
    input.as_array().unwrap().iter()
        .filter(|row| row["role"] == "user")
        .map(|row| row["content"].as_str().unwrap())
        .collect()
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
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let gateway = format!("http://{}/v1", listener.local_addr().unwrap());
    let (upstream, started) = mpsc::channel();
    let (release, accepted) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut initial, _) = listener.accept().unwrap();
        assert_eq!(user_texts(&request(&mut initial)["input"]), ["Task A"]);
        upstream.send(()).unwrap();
        accepted.recv_timeout(Duration::from_secs(10)).unwrap();
        respond(initial, call("count_1", "research", json!({"scope":"workspace"})));
        let limit = source_harness::transport::GatewayConfig::default().max_turns;
        for turn in 2..=limit {
            let (mut pending, _) = listener.accept().unwrap();
            let body = request(&mut pending);
            assert_eq!(user_texts(&body["input"]), ["Task A"]);
            if turn >= 3 {
                let last = body["input"].as_array().unwrap().last().unwrap();
                let output: Value = serde_json::from_str(last["output"].as_str().unwrap()).unwrap();
                assert_eq!(output["loop_warning"]["kind"], "EXACT_REPEAT");
                assert_eq!(output["loop_warning"]["count"], turn - 1);
            }
            respond(pending, call(&format!("count_{turn}"), "research", json!({"scope":"workspace"})));
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
            call("first_old", "write", json!({"path":"old-a.txt","mode":"create","content":"old"})),
            call("second_old", "write", json!({"path":"old-b.txt","mode":"create","content":"old"})),
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
            let reply: Value = serde_json::from_str(input[index]["output"].as_str().unwrap()).unwrap();
            assert_eq!(reply["code"], "SUPERSEDED_BY_STEER");
        }
        assert_eq!(input[5], json!({"role":"user","content":"Write corrected.txt instead"}));
        respond(redirected, call("corrected", "write",
            json!({"path":"corrected.txt","mode":"create","content":"corrected"})));

        let (mut last, _) = listener.accept().unwrap();
        let final_request = request(&mut last);
        let receipt: Value =
            serde_json::from_str(final_request["input"][7]["output"].as_str().unwrap()).unwrap();
        assert_eq!(receipt["outcome"], "committed");
        respond(last, json!({"type":"message","role":"assistant","content":[
            {"type":"output_text","text":"Updated corrected.txt."}
        ]}));
    });

    let mut child = Command::new(env!("CARGO_BIN_EXE_source_harness"))
        .args(["chat", "--root", root.path().to_str().unwrap(), "--session", "steering"])
        .env("OPENAI_BASE_URL", &gateway).env("OPENAI_MODEL", "fixture")
        .env_remove("SOURCE_HARNESS_TEST_INTERRUPT")
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (text, lines) = mpsc::channel();
    let (progress, logs) = mpsc::channel();
    let read_stdout = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if text.send(line.unwrap()).is_err() { break; }
        }
    });
    let read_stderr = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            if progress.send(line.unwrap()).is_err() { break; }
        }
    });
    writeln!(stdin, "Write the original files").unwrap();
    let signal = lines.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(signal, "Working on the original files");
    writeln!(stdin, ":steer Write corrected.txt instead").unwrap();
    let wait = Duration::from_secs(10);
    while logs.recv_timeout(wait).unwrap() != "Correction received." {}
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
    let journal = fs::read_to_string(root.path().join(".sourceharness/sessions/steering/events.jsonl")).unwrap();
    let events: Vec<Value> = journal.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    let steer = events.iter().find(|event| event["kind"] == "steer").unwrap();
    let redirected = events.iter().find(|event| event["kind"] == "model_request" && event["data"]["round"] == 2).unwrap();
    assert_eq!(steer["data"]["after_round"], 1);
    assert_eq!(redirected["data"]["steer_events"], json!([steer["seq"]]));
    assert_eq!(redirected["data"]["steer_input_start"], 5);
    assert_eq!(events.iter().filter(|event| event["kind"] == "tool_skipped").count(), 2);
    assert_eq!(events.iter().filter(|event| event["kind"] == "tool_attempt").count(), 1);
}

fn invoke(root: &Path, gateway: &str, interrupt: bool, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_source_harness"));
    command
        .args(args)
        .args(["--root", root.to_str().unwrap(), "--session", "acceptance"])
        .env("OPENAI_BASE_URL", gateway)
        .env("OPENAI_MODEL", "fixture")
        .env_remove("SOURCE_HARNESS_TEST_INTERRUPT");
    if interrupt {
        command.env("SOURCE_HARNESS_TEST_INTERRUPT", "after_file_replace");
    }
    command.output().unwrap()
}

#[test]
fn validation_reports_changes_and_preserves_history_across_followup_requests() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("workspace");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("policy.txt"), "limit=30\n").unwrap();
    let checks = fixture.path().join("checks.json");
    fs::write(&checks, json!({
        "acceptance":{"program":env!("CARGO_BIN_EXE_source_harness"),"args":["--help"]}
    }).to_string()).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let gateway = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut first, _) = listener.accept().unwrap();
        assert_eq!(user_texts(&request(&mut first)["input"]), ["Write a report."]);
        respond(first, call("write_1", "write", json!({
            "mode":"create","path":"report.txt","content":"limit=30\n"
        })));

        let (mut second, _) = listener.accept().unwrap();
        request(&mut second);
        respond(second, call("check_1", "validate", json!({"check":"acceptance"})));

        let (mut validated, _) = listener.accept().unwrap();
        let body = request(&mut validated);
        let output = body["input"].as_array().unwrap().iter()
            .find(|item| item["type"] == "function_call_output" && item["call_id"] == "check_1")
            .unwrap();
        let proof: Value = serde_json::from_str(output["output"].as_str().unwrap()).unwrap();
        assert_eq!(proof["passed"], true);
        assert_eq!(proof["workspace_changes_since_start"]["files"], json!([
            {"path":"report.txt","change":"added"}
        ]));
        assert_eq!(proof["workspace_changes_since_start"]["total"], 1);
        assert_eq!(proof["workspace_changes_since_start"]["complete"], true);
        respond(validated, answer("Report complete."));

        let (mut followup, _) = listener.accept().unwrap();
        let before = request(&mut followup);
        let texts = user_texts(&before["input"]);
        assert_eq!(&texts[..2], ["Write a report.", "Check the policy."]);
        assert_eq!(texts.len(), 3);
        let notice = texts[2];
        assert!(notice.contains("Related recorded evidence."));
        assert!(notice.contains("previous_runs"));
        respond(followup, call("read_1", "read", json!({"path":"policy.txt"})));

        let (mut after_read, _) = listener.accept().unwrap();
        let after = request(&mut after_read);
        assert_eq!(user_texts(&after["input"]), texts);
        assert_eq!(after["instructions"], before["instructions"]);
        respond(after_read, answer("Policy confirmed."));
        notice.len()
    });

    let check_path = checks.to_str().unwrap();
    for prompt in ["Write a report.", "Check the policy."] {
        let result = invoke(&root, &gateway, false, &["agent", "--checks", check_path, "--prompt", prompt]);
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    }

    let notice_bytes = server.join().unwrap();
    let events = session_events(&root);
    let followup = events.iter().rev().find(|event| event["kind"] == "user").unwrap();
    let audits: Vec<_> = events.iter()
        .filter(|event| event["kind"] == "model_request" && event["run"] == followup["run"])
        .map(|event| &event["data"]["history_auxiliary"])
        .collect();
    assert_eq!(audits.len(), 2);
    assert_eq!(audits[0]["bytes"], notice_bytes);
    assert_eq!(audits[0], audits[1]);
}

#[test]
fn continues_with_a_changed_tool_contract_and_new_user_correction() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("config.txt");
    fs::write(&file, b"primary\r\nalpha: 30\r\narchive\r\nalpha: 30\r\n").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let gateway = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        request(&mut stream);
        respond(stream, call("read_1", "read", json!({"path":"config.txt"})));

        let (mut stream, _) = listener.accept().unwrap();
        let before = request(&mut stream);
        let output: Value =
            serde_json::from_str(before["input"][2]["output"].as_str().unwrap()).unwrap();
        respond(
            stream,
            call(
                "edit_1",
                "edit",
                json!({
                    "path":"config.txt","expected_revision":output["revision"],
                    "changes":[{"old_text":"primary\r\nalpha: 30","new_text":"primary\r\nalpha: 60"}]
                }),
            ),
        );

        let (mut stream, _) = listener.accept().unwrap();
        let continued = request(&mut stream);
        let archived = &continued["input"][4];
        assert_eq!(archived["call_id"], "edit_1");
        let observed: Value = serde_json::from_str(archived["output"].as_str().unwrap()).unwrap();
        assert_eq!(observed["outcome"], "observed_target");
        assert_eq!(observed["current_revision"], observed["planned_revision"]);
        assert!(continued["input"][5]["content"]
            .as_str()
            .unwrap()
            .contains("Use 40 instead."));
        let schema = continued["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "edit")
            .unwrap();
        assert_eq!(
            schema["parameters"]["properties"]["changes"]["items"]["properties"]["start_line"]
                ["type"],
            "integer"
        );
        respond(stream, call("read_2", "read", json!({"path":"config.txt"})));

        let (mut stream, _) = listener.accept().unwrap();
        let refreshed = request(&mut stream);
        let output: Value =
            serde_json::from_str(refreshed["input"][7]["output"].as_str().unwrap()).unwrap();
        assert!(output["content"]
            .as_str()
            .unwrap()
            .contains("primary\r\nalpha: 60"));
        respond(
            stream,
            call(
                "edit_2",
                "edit",
                json!({
                    "path":"config.txt","expected_revision":output["revision"],
                    "changes":[{"start_line":2,"end_line":2,"new_text":"alpha: 40\r\n"}]
                }),
            ),
        );

        let (mut stream, _) = listener.accept().unwrap();
        let corrected = request(&mut stream);
        let output: Value =
            serde_json::from_str(corrected["input"][9]["output"].as_str().unwrap()).unwrap();
        assert_eq!(output["outcome"], "committed");
        respond(
            stream,
            json!({"type":"message","role":"assistant",
            "content":[{"type":"output_text","text":"Primary alpha is 40."}]}),
        );
    });

    let interrupted = invoke(
        root.path(),
        &gateway,
        true,
        &["agent", "--prompt", "Use 60 for primary alpha."],
    );
    assert_eq!(interrupted.status.code(), Some(86));
    let rejected = invoke(
        root.path(),
        &gateway,
        false,
        &["agent", "--resume", "--edit-mode", "range"],
    );
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("Use --continue"));
    assert_eq!(
        fs::read(&file).unwrap(),
        b"primary\r\nalpha: 60\r\narchive\r\nalpha: 30\r\n"
    );

    let args = [
        "agent",
        "--continue",
        "--edit-mode",
        "range",
        "--prompt",
        "Use 40 instead.",
    ];
    let continued = invoke(root.path(), &gateway, false, &args);
    assert!(
        continued.status.success(),
        "{}",
        String::from_utf8_lossy(&continued.stderr)
    );
    server.join().unwrap();
    assert_eq!(
        fs::read(file).unwrap(),
        b"primary\r\nalpha: 40\r\narchive\r\nalpha: 30\r\n"
    );

    let events = fs::read_to_string(
        root.path()
            .join(".sourceharness/sessions/acceptance/events.jsonl"),
    )
    .unwrap();
    let events = events
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    let users = events
        .iter()
        .filter(|e| e["kind"] == "user")
        .collect::<Vec<_>>();
    assert_eq!(users.len(), 2);
    assert_eq!(users[1]["data"]["continuation"]["run"], users[0]["run"]);
    assert_eq!(
        users[1]["data"]["continuation"]["correction"],
        "Use 40 instead."
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e["kind"] == "edit_receipt")
            .count(),
        1
    );
    let final_text = invoke(
        root.path(),
        &gateway,
        false,
        &["agent", "--resume", "--edit-mode", "range"],
    );
    assert!(final_text.status.success());
    assert_eq!(
        String::from_utf8_lossy(&final_text.stdout).trim(),
        "Primary alpha is 40."
    );
}

#[test]
fn resumes_an_applied_edit_and_retains_exact_old_content() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("config.txt"),
        b"primary\r\nalpha: 30\r\narchive\r\nalpha: 30\r\n",
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let gateway = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut unavailable, _) = listener.accept().unwrap();
        let failed = request(&mut unavailable);
        write!(
            unavailable,
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        drop(unavailable);
        let (mut first, _) = listener.accept().unwrap();
        let initial = request(&mut first);
        assert_eq!(initial, failed);
        assert_eq!(
            initial["input"][0]["content"],
            "Raise the primary alpha setting"
        );
        respond(
            first,
            call(
                "read_1",
                "read",
                json!({"path":"config.txt","start_line":1,"line_count":2}),
            ),
        );

        let (mut second, _) = listener.accept().unwrap();
        let reply = request(&mut second);
        let output: Value =
            serde_json::from_str(reply["input"][2]["output"].as_str().unwrap()).unwrap();
        assert_eq!(output["content"], "primary\r\nalpha: 30\r\n");
        let reference = output["result_ref"].as_str().unwrap().to_owned();
        respond(
            second,
            call(
                "edit_1",
                "edit",
                json!({"path":"config.txt","expected_revision":output["revision"],
            "changes":[{"old_text":"primary\r\nalpha: 30","new_text":"primary\r\nalpha: 60"}]}),
            ),
        );

        let (mut third, _) = listener.accept().unwrap();
        let resumed = request(&mut third);
        let result: Value =
            serde_json::from_str(resumed["input"][4]["output"].as_str().unwrap()).unwrap();
        assert_eq!(resumed["input"][4]["call_id"], "edit_1");
        assert_eq!(result["outcome"], "observed_target_on_recovery");
        assert!(result["diff"].as_str().unwrap().contains("+alpha: 60"));
        respond(
            third,
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"Primary alpha is 60."}]}),
        );
        reference
    });

    let stopped = invoke(
        root.path(),
        &gateway,
        true,
        &["agent", "--prompt", "Raise the primary alpha setting"],
    );
    assert_eq!(
        stopped.status.code(),
        Some(86),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    assert_eq!(
        fs::read(root.path().join("config.txt")).unwrap(),
        b"primary\r\nalpha: 60\r\narchive\r\nalpha: 30\r\n"
    );
    let journal = root
        .path()
        .join(".sourceharness/sessions/acceptance/events.jsonl");
    OpenOptions::new()
        .append(true)
        .open(&journal)
        .unwrap()
        .write_all(b"{\"seq\":")
        .unwrap();

    let resumed = invoke(root.path(), &gateway, false, &["agent", "--resume"]);
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&resumed.stdout).trim(),
        "Primary alpha is 60."
    );
    let reference = server.join().unwrap();

    let history = invoke(
        root.path(),
        &gateway,
        false,
        &["history", "--ref", &reference],
    );
    assert_eq!(
        history.stdout,
        b"primary\r\nalpha: 30\r\narchive\r\nalpha: 30\r\n"
    );
    let result = fs::read_to_string(journal).unwrap();
    let events: Vec<Value> = result
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        events
            .iter()
            .filter(|event| event["kind"] == "edit_receipt")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event["kind"] == "model_error")
            .count(),
        1
    );
    assert_eq!(
        fs::read(root.path().join("config.txt")).unwrap(),
        b"primary\r\nalpha: 60\r\narchive\r\nalpha: 30\r\n"
    );
}
