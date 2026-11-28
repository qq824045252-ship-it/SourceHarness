use super::compact;
use super::tools::read;
use super::{Options, Session};
use crate::budget::json_byte_len;
use crate::workspace::{WorkspaceConfig, WorkspaceContext};
use super::journal::Journal;
use serde_json::{json, Value};
use std::fs;

#[test]
fn restores_receipts_by_operation_in_recorded_completion_order() {
    let root = tempfile::tempdir().unwrap();
    let mut session = Session::open(root.path(), "work", Options::default()).unwrap();
    session.journal.append(1, "user", json!({"text":"Read both files","profile":{}})).unwrap();
    let response = session.journal.store_json(&json!([
        {"type":"function_call","call_id":"read_a","name":"read","arguments":"{}"},
        {"type":"function_call","call_id":"read_b","name":"read","arguments":"{}"}
    ])).unwrap();
    session.journal.append(1, "model_response", json!({"round":1,"output":response})).unwrap();
    for (op, actual, delivered) in [("1.1.2", "raw b", "visible b"), ("1.1.1", "a", "a")] {
        let raw = session.journal.store_json(&json!({"content":actual})).unwrap();
        let model = session.journal.store_json(&json!({"content":delivered})).unwrap();
        session.journal.append(1, "tool_result", json!({"operation":op,"output":raw,"model_output":model})).unwrap();
    }
    let replay = session.checkpoint().unwrap();
    assert!(replay.pending.is_empty());
    assert_eq!(replay.input[3]["call_id"], "read_b");
    assert_eq!(replay.input[3]["output"], json!({"content":"visible b"}).to_string());
    assert_eq!(replay.input[4]["call_id"], "read_a");
}

#[test]
fn overlapping_matches_need_unique_context() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("note.txt");
    fs::write(&path, "caaaa").unwrap();
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root.path())).unwrap();
    let mut journal = Journal::open(&root.path().join(".sourceharness")).unwrap();
    let ambiguous = compact::modify(
        &ctx, &mut journal, 1, "1.1.1", "edit",
        &json!({"path":"note.txt","changes":[{"old_text":"aaa","new_text":"b"}]}),
    ).unwrap();
    assert_eq!(ambiguous["code"], "AMBIGUOUS_MATCH");
    assert_eq!(ambiguous["match_byte_starts"], json!([1, 2]));
    assert_eq!(fs::read_to_string(path).unwrap(), "caaaa");
}

#[test]
fn pages_an_oversized_utf8_line_after_the_live_file_changes() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("output.txt");
    let long = format!("{}\r\n", "字\"".repeat(3400));
    fs::write(&path, format!("start\r\n{long}end\r\n")).unwrap();
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root.path())).unwrap();
    let mut journal = Journal::open(&root.path().join(".sourceharness")).unwrap();
    let mut reply = read(&ctx, &mut journal, 1, &json!({"path":"output.txt","start_line":2,"line_count":1})).unwrap();
    assert_eq!(reply["delivery"], "folded");
    fs::write(path, "new version").unwrap();
    let rev = reply["revision"].clone();
    let mut got = String::new();
    for _ in 0..16 {
        assert_eq!(reply["revision"], rev);
        assert!(json_byte_len(&reply) <= 8192);
        got.push_str(reply["content"].as_str().unwrap());
        if reply["coverage"]["complete"] == true { break; }
        reply = read(&ctx, &mut journal, 1, &reply["next"]).unwrap();
    }
    assert_eq!(got, long);
}

#[test]
fn recovered_conflict_exposes_the_exact_difference_from_the_intended_version() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("settings.txt");
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root.path())).unwrap();
    let mut journal = Journal::open(&root.path().join(".sourceharness")).unwrap();
    let before = journal.put(b"timeout=30\n").unwrap();
    let after = journal.put(b"timeout=60\n").unwrap();
    journal.append(1, "edit_intent", json!({"operation":"1.1.1","path":"settings.txt","before":before,"after":after})).unwrap();
    fs::write(&path, "timeout=60\n# hand edited\n").unwrap();
    let res = compact::modify(&ctx, &mut journal, 1, "1.1.1", "edit", &Value::Null).unwrap();
    assert_eq!(res["code"], "REVISION_CONFLICT");
    assert_eq!(res["planned_revision"], after);
    let obs = read(&ctx, &mut journal, 1, &json!({"result_ref":res["planned_to_current"]["result_ref"]})).unwrap();
    let delta = obs["content"].as_str().unwrap();
    assert!(delta.contains("\n+# hand edited\n"));
    assert!(delta.contains("\n timeout=60\n"));
    assert_eq!(fs::read_to_string(path).unwrap(), "timeout=60\n# hand edited\n");
}

#[test]
fn read_directory_and_history_work_while_explicit_cache_path_remains_readable() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("node_modules/pkg")).unwrap();
    fs::create_dir_all(root.path().join("dist")).unwrap();
    fs::write(root.path().join("node_modules/pkg/index.js"), "ignored").unwrap();
    fs::write(root.path().join("dist/out.txt"), "explicit ok\n").unwrap();
    fs::write(root.path().join("note.txt"), "v1\n").unwrap();
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root.path())).unwrap();
    let mut journal = Journal::open(&root.path().join(".sourceharness")).unwrap();

    let dir_out = compact::read(&ctx, &mut journal, 1, &json!({"path":"."})).unwrap();
    let dir_text = dir_out["content"].as_str().unwrap();
    assert!(dir_text.contains("note.txt"));
    assert!(!dir_text.contains("node_modules"));
    assert!(!dir_text.contains("dist/out.txt"));
    assert_eq!(read(&ctx, &mut journal, 1, &json!({"path":"dist/out.txt"})).unwrap()["content"], "explicit ok\n");

    let r1 = compact::read(&ctx, &mut journal, 1, &json!({"path":"note.txt"})).unwrap()["result_ref"].clone();
    fs::write(root.path().join("note.txt"), "v2\n").unwrap();
    let hist = compact::read(&ctx, &mut journal, 2, &json!({"history":true,"path":"note.txt"})).unwrap();
    let hist_text = hist["content"].as_str().unwrap();
    assert!(hist_text.contains(r1.as_str().unwrap()));
    assert_eq!(compact::read(&ctx, &mut journal, 2, &json!({"result_ref":r1})).unwrap()["content"], "v1\n");
}

#[test]
fn stat_cache_racy_clean_detects_same_length_rapid_rewrites() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("fast.txt");
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root.path())).unwrap();
    let journal = Journal::open(&root.path().join(".sourceharness")).unwrap();
    fs::write(&path, "aaaa").unwrap();
    let v1 = super::validation::workspace_version(&ctx, &journal).unwrap();
    fs::write(&path, "bbbb").unwrap();
    let v2 = super::validation::workspace_version(&ctx, &journal).unwrap();
    assert_ne!(v1, v2);
}

#[test]
fn rollback_removes_binary_file_created_by_exec() {
    let root = tempfile::tempdir().unwrap();
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root.path())).unwrap();
    let mut journal = Journal::open(&root.path().join(".sourceharness")).unwrap();
    let base = super::validation::save_workspace(&ctx, &journal).unwrap();
    journal.append(1, "user", json!({"text":"run","workspace_revision_at_start":base})).unwrap();
    let bin = root.path().join("out.bin");
    fs::write(&bin, [0xff_u8, 0x00, 0xfe, 0x80]).unwrap();
    let after = super::validation::save_workspace(&ctx, &journal).unwrap();
    journal.append(1, "exec_receipt", json!({"before_workspace_revision":base,"workspace_revision":after,"operation":"1.1.1"})).unwrap();
    let rb = super::rollback::run(&ctx, &mut journal, 1, "1.2.1", &json!({})).unwrap();
    assert_eq!(rb["status"], "ok");
    assert_eq!(rb["paths"], json!(["out.bin"]));
    assert!(!bin.exists());
}

#[test]
fn read_headers_pattern_search_and_validate_truncation_hints() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("crlf.cfg"), b"a=1\r\nb=2\r\n").unwrap();
    fs::write(root.path().join("ver.txt"), b"1.0.0").unwrap();
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root.path())).unwrap();
    let mut journal = Journal::open(&root.path().join(".sourceharness")).unwrap();

    let dir_txt = compact::present("read", &compact::read(&ctx, &mut journal, 1, &json!({"path":"."})).unwrap(), None).as_str().unwrap().to_owned();
    assert!(dir_txt.contains("directory, 2 files, complete"), "{dir_txt}");
    let crlf_res = compact::read(&ctx, &mut journal, 1, &json!({"path":"crlf.cfg"})).unwrap();
    let crlf_txt = compact::present("read", &crlf_res, None).as_str().unwrap().to_owned();
    assert!(crlf_txt.contains("lines 1-2/2, 10B, crlf, r"), "{crlf_txt}");
    let ver_txt = compact::present("read", &compact::read(&ctx, &mut journal, 1, &json!({"path":"ver.txt"})).unwrap(), None).as_str().unwrap().to_owned();
    assert!(ver_txt.contains("no-trailing-newline, r"), "{ver_txt}");

    let lit = compact::read(&ctx, &mut journal, 1, &json!({"path":".","pattern":"b=2"})).unwrap();
    assert!(lit["content"].as_str().unwrap().contains("crlf.cfg:2\tb=2"), "{lit}");
    let rx = compact::read(&ctx, &mut journal, 1, &json!({"path":".","pattern":r"\d+\.\d+\.\d+","regex":true})).unwrap();
    assert!(rx["content"].as_str().unwrap().contains("ver.txt:1\t1.0.0"), "{rx}");
    let by_ref = compact::read(&ctx, &mut journal, 1, &json!({"result_ref":crlf_res["result_ref"],"pattern":"a=1"})).unwrap();
    assert_eq!(by_ref["total_matches"], 1);

    let short_fail = compact::present("validate", &json!({"status":"ok","check":"u","passed":false,"exit_code":1,"preview":"err","result_ref":"r9","output_truncated":false,"output_complete":true}), None).as_str().unwrap().to_owned();
    assert!(!short_fail.contains("Full output:"), "{short_fail}");
    let long_fail = compact::present("validate", &json!({"status":"ok","check":"u","passed":false,"exit_code":1,"preview":"err","result_ref":"r9","output_truncated":true,"output_complete":true}), None).as_str().unwrap().to_owned();
    assert!(long_fail.contains("Full output: read(result_ref=\"r9\")"), "{long_fail}");
}

#[test]
fn exec_supports_stdin_without_workspace_pollution() {
    let root = tempfile::tempdir().unwrap();
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root.path())).unwrap();
    let mut journal = Journal::open(&root.path().join(".sourceharness")).unwrap();
    let cmd = if cfg!(windows) { "findstr \".*\"" } else { "cat" };
    let res = super::exec::run(&ctx, &mut journal, 1, "1.1.1", &json!({"command": cmd, "stdin": "alpha\nbeta\n"})).unwrap();
    assert_eq!(res["status"], "ok", "{res}");
    assert!(res["preview"].as_str().unwrap().contains("alpha"), "{res}");
    assert!(res["preview"].as_str().unwrap().contains("beta"), "{res}");
    assert_eq!(res["before_workspace_revision"], res["workspace_revision"]);

    let too_big = "x".repeat(super::exec::MAX_STDIN_BYTES + 1);
    let rej = super::exec::run(&ctx, &mut journal, 1, "1.2.1", &json!({"command": cmd, "stdin": too_big})).unwrap();
    assert_eq!(rej["code"], "INVALID_ARGUMENT", "{rej}");
}

#[test]
fn exec_timeout_kills_descendant_process_tree() {
    let root = tempfile::tempdir().unwrap();
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root.path())).unwrap();
    let mut journal = Journal::open(&root.path().join(".sourceharness")).unwrap();
    let cmd = if cfg!(windows) { "cmd /c ping -n 20 127.0.0.1 > nul" } else { "sh -c 'sleep 20'" };
    let started = std::time::Instant::now();
    let res = super::exec::run(&ctx, &mut journal, 1, "1.1.1", &json!({"command": cmd, "timeout_seconds": 1})).unwrap();
    assert_eq!(res["code"], "EXEC_TIMEOUT", "{res}");
    assert_eq!(res["timed_out"], true, "{res}");
    assert!(started.elapsed().as_secs_f32() < 5.0, "elapsed={:?}", started.elapsed());
}
