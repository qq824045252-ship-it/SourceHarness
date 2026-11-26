use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output};
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
