use std::fs;
use std::io::Cursor;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use source_harness::budget::MAX_RESPONSE_BYTES;
use source_harness::transport::sse::{parse_sse_stream_with_observer, SseObserver};
use source_harness::transport::ToolExecutor;
use source_harness::workspace::{WorkspaceConfig, WorkspaceContext, WorkspaceToolRouter};

struct TempDirGuard {
    path: PathBuf,
}

impl TempDirGuard {
    fn new(prefix: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "source_harness_test_{prefix}_{}_{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn parse_json(s: &str) -> Value {
    serde_json::from_str(s).expect("output must be valid JSON")
}

#[test]
fn workspace_sandbox_containment_and_error_guards() {
    let dir = TempDirGuard::new("sandbox");
    fs::create_dir_all(dir.path.join("sub")).unwrap();
    fs::write(dir.path.join("sub/hello.txt"), "hello world").unwrap();
    fs::write(dir.path.join("binary.bin"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
    fs::write(dir.path.join("too_big.txt"), "A".repeat(2048)).unwrap();

    let mut cfg = WorkspaceConfig::with_root(&dir.path);
    cfg.max_file_bytes = 512;
    let ctx = WorkspaceContext::new(cfg).unwrap();
    let router = WorkspaceToolRouter::new(ctx);

    // Parent traversal and absolute path escape are rejected.
    let outside_rel = parse_json(
        &router
            .dispatch("read_file", &json!({ "path": "../escape.txt" }).to_string())
            .output_json,
    );
    assert_eq!(outside_rel["status"], "error");
    assert_eq!(outside_rel["code"], "INVALID_ARGUMENT");
    assert_eq!(outside_rel["reason"], "PATH_OUTSIDE_WORKSPACE");

    let abs_path = dir.path.join("sub/hello.txt").display().to_string();
    let outside_abs = parse_json(
        &router
            .dispatch("read_file", &json!({ "path": abs_path }).to_string())
            .output_json,
    );
    assert_eq!(outside_abs["code"], "INVALID_ARGUMENT");
    assert_eq!(outside_abs["reason"], "PATH_OUTSIDE_WORKSPACE");

    // Missing file, directory passed as file, non-UTF-8, and oversized file.
    let missing = parse_json(
        &router
            .dispatch("read_file", &json!({ "path": "missing.txt" }).to_string())
            .output_json,
    );
    assert_eq!(missing["code"], "INVALID_ARGUMENT");
    assert_eq!(missing["reason"], "FILE_NOT_FOUND");

    let not_file = parse_json(
        &router
            .dispatch("read_file", &json!({ "path": "sub" }).to_string())
            .output_json,
    );
    assert_eq!(not_file["reason"], "NOT_A_FILE");

    let non_utf8 = parse_json(
        &router
            .dispatch("read_file", &json!({ "path": "binary.bin" }).to_string())
            .output_json,
    );
    assert_eq!(non_utf8["reason"], "NON_UTF8_FILE");

    let too_big = parse_json(
        &router
            .dispatch("read_file", &json!({ "path": "too_big.txt" }).to_string())
            .output_json,
    );
    assert_eq!(too_big["reason"], "FILE_TOO_LARGE");
}

#[test]
fn workspace_inline_vs_folded_routing_and_follow_up_search() {
    let dir = TempDirGuard::new("routing");
    fs::write(dir.path.join("small.txt"), "MAX_LIMIT=42\n").unwrap();

    // 4000 NUL bytes <= 4096 raw threshold, but JSON escapes `\u0000` (6x) > 8192 bytes -> must auto-fold!
    fs::write(dir.path.join("escaped_small.txt"), "\u{0000}".repeat(4000)).unwrap();

    // Large file (> 4096 bytes) -> folds into FoldStore and can be queried via search_result/read_result.
    let mut big = String::from("# Big Log\n");
    for i in 1..=120 {
        big.push_str(&format!(
            "line={i:03} KIND=ALPHA padding_payload_to_exceed_4096_bytes_inline_threshold\n"
        ));
    }
    fs::write(dir.path.join("big.md"), &big).unwrap();

    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(&dir.path)).unwrap();
    let router = WorkspaceToolRouter::new(ctx);

    let small_out = router.dispatch("read_file", &json!({ "path": "small.txt" }).to_string());
    assert!(small_out.output_json.len() <= MAX_RESPONSE_BYTES);
    assert_eq!(small_out.delivery, Some("inline"));
    assert!(small_out.result_ref.is_none());
    let small_val = parse_json(&small_out.output_json);
    assert_eq!(small_val["delivery"], "inline");
    assert_eq!(small_val["content"], "MAX_LIMIT=42\n");

    let esc_out = router.dispatch(
        "read_file",
        &json!({ "path": "escaped_small.txt" }).to_string(),
    );
    assert!(esc_out.output_json.len() <= MAX_RESPONSE_BYTES);
    assert_eq!(esc_out.delivery, Some("folded"));
    assert!(esc_out.result_ref.is_some());

    let big_out = router.dispatch("read_file", &json!({ "path": "big.md" }).to_string());
    assert!(big_out.output_json.len() <= MAX_RESPONSE_BYTES);
    assert_eq!(big_out.delivery, Some("folded"));
    let ref_id = big_out.result_ref.expect("folded output must have result_ref");

    let search_out = router.dispatch(
        "search_result",
        &json!({
            "result_ref": ref_id,
            "query": "KIND=([A-Z]+)",
            "match_mode": "regex",
            "group_by_capture": 1
        })
        .to_string(),
    );
    let search_val = parse_json(&search_out.output_json);
    assert_eq!(search_val["status"], "ok");
    assert_eq!(search_val["data"]["matching_lines"], 120);
    assert_eq!(
        search_val["data"]["aggregation"]["value_counts"][0]["value"],
        "ALPHA"
    );
    assert_eq!(
        search_val["data"]["aggregation"]["value_counts"][0]["count"],
        120
    );
}

#[test]
fn workspace_list_files_and_grep_files_behavior() {
    let dir = TempDirGuard::new("list_grep");
    fs::create_dir_all(dir.path.join(".git")).unwrap();
    fs::write(dir.path.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::create_dir_all(dir.path.join("target/debug")).unwrap();
    fs::write(dir.path.join("target/debug/bin"), "MATCH_TARGET").unwrap();
    fs::create_dir_all(dir.path.join("src")).unwrap();
    fs::write(dir.path.join("src/a.rs"), "fn alpha() {}\n// FIND_ME_1\n").unwrap();
    fs::write(dir.path.join("src/b.rs"), "fn beta() {}\n// FIND_ME_2\n").unwrap();

    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(&dir.path)).unwrap();
    let router = WorkspaceToolRouter::new(ctx);

    let list_out = router.dispatch("list_files", "{}");
    let list_val = parse_json(&list_out.output_json);
    assert_eq!(list_val["status"], "ok");
    let content = list_val["content"].as_str().unwrap();
    assert!(content.contains("src/a.rs"));
    assert!(content.contains("src/b.rs"));
    assert!(!content.contains(".git"));
    assert!(!content.contains("target/"));

    let grep_out = router.dispatch(
        "grep_files",
        &json!({ "query": "FIND_ME_\\d+", "match_mode": "regex" }).to_string(),
    );
    let grep_val = parse_json(&grep_out.output_json);
    assert_eq!(grep_val["status"], "ok");
    assert_eq!(grep_val["match_count"], 2);
    let grep_text = grep_val["content"].as_str().unwrap();
    assert!(grep_text.contains("src/a.rs:2: // FIND_ME_1"));
    assert!(grep_text.contains("src/b.rs:2: // FIND_ME_2"));

    let bad_regex = router.dispatch(
        "grep_files",
        &json!({ "query": "(unclosed", "match_mode": "regex" }).to_string(),
    );
    let bad_val = parse_json(&bad_regex.output_json);
    assert_eq!(bad_val["status"], "error");
    assert_eq!(bad_val["code"], "INVALID_REGEX");
    assert_eq!(bad_val["reason"], "REGEX_COMPILE_ERROR");
}

#[test]
fn sse_observer_streams_text_deltas_and_never_forwards_reasoning() {
    struct CaptureObserver {
        text: String,
    }
    impl SseObserver for CaptureObserver {
        fn on_text_delta(&mut self, delta: &str) {
            self.text.push_str(delta);
        }
    }

    let sse = concat!(
        "event: response.reasoning_summary_text.delta\n",
        "data: {\"delta\":\"SECRET_REASONING_TEXT\"}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"delta\":\"Hello \"}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"delta\":\"world!\"}\n\n",
        "event: response.completed\n",
        "data: {\"response\":{\"id\":\"resp_x\",\"output\":[{\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"SECRET_REASONING_TEXT\"}]},{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello world!\"}]}],\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"total_tokens\":15}}}\n\n"
    );

    let mut obs = CaptureObserver {
        text: String::new(),
    };
    let parsed = parse_sse_stream_with_observer(Cursor::new(sse.as_bytes()), &mut obs).unwrap();
    assert_eq!(obs.text, "Hello world!");
    assert!(!obs.text.contains("SECRET_REASONING_TEXT"));
    assert_eq!(parsed.output_items.len(), 2);
    assert_eq!(parsed.output_items[0]["type"], "reasoning");
}
