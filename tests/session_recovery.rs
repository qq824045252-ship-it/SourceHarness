use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output, Stdio};
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
