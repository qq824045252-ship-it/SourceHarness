use super::super::compact;
use super::Journal;
use crate::workspace::{WorkspaceConfig, WorkspaceContext};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

fn write(ctx: &WorkspaceContext, journal: &mut Journal, operation: &str, args: Value) -> Value {
    compact::modify(ctx, journal, 1, operation, "write", &args).unwrap()
}

#[test]
fn create_makes_workspace_parents_and_preserves_no_clobber_behavior() {
    let root = tempfile::tempdir().unwrap();
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root.path())).unwrap();
    let mut journal = Journal::open(&root.path().join(".sourceharness")).unwrap();
    let rel = "src/new/nested/module.rs";
    let first = write(&ctx, &mut journal, "create", json!({"path":rel,"content":"mod tests;\n"}));
    assert_eq!(first["outcome"], "committed");
    assert_eq!(first["source"]["path"], rel);
    assert_eq!(fs::read(root.path().join(rel)).unwrap(), b"mod tests;\n");

    let repeated = write(&ctx, &mut journal, "repeat", json!({"path":rel,"content":"replacement"}));
    assert_eq!(repeated["outcome"], "committed");
    assert_eq!(fs::read(root.path().join(rel)).unwrap(), b"replacement");
}

#[test]
fn resumes_a_create_intent_after_only_some_parents_were_created() {
    let root = tempfile::tempdir().unwrap();
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root.path())).unwrap();
    let state = root.path().join(".sourceharness");
    let rel = "src/new/nested/module.rs";
    let content = b"pub fn new() {}\n";
    {
        let mut journal = Journal::open(&state).unwrap();
        let after = journal.put(content).unwrap();
        journal.append(1, "edit_intent", json!({"operation":"create","path":rel,"before":null,"after":after})).unwrap();
    }
    fs::create_dir(root.path().join("src")).unwrap();
    let mut journal = Journal::open(&state).unwrap();
    let args = json!({"path":rel,"content":"pub fn new() {}\n"});
    let resumed = write(&ctx, &mut journal, "create", args.clone());
    assert_eq!(resumed["outcome"], "committed");
    assert_eq!(fs::read(root.path().join(rel)).unwrap(), content);
    assert_eq!(write(&ctx, &mut journal, "create", args), resumed);
    assert_eq!(journal.events.iter().filter(|e| e.kind == "edit_receipt").count(), 1);
}

#[cfg(windows)]
fn redirect_directory(link: &Path, target: &Path) {
    let out = std::process::Command::new("cmd.exe").args(["/D", "/C", "mklink", "/J"]).arg(link).arg(target).output().unwrap();
    assert!(out.status.success(), "{out:?}");
}

#[cfg(unix)]
fn redirect_directory(link: &Path, target: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[cfg(any(windows, unix))]
#[test]
fn create_and_resume_reject_nested_directories_redirected_outside_workspace() {
    let (root, outside) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root.path())).unwrap();
    let mut journal = Journal::open(&root.path().join(".sourceharness")).unwrap();
    let rel = "redirect/missing/nested/module.rs";
    let after = journal.put(b"planned").unwrap();
    journal.append(1, "edit_intent", json!({"operation":"resume","path":rel,"before":null,"after":after})).unwrap();
    let link = root.path().join("redirect");
    redirect_directory(&link, outside.path());

    let args = json!({"path":rel,"content":"planned"});
    assert_eq!(write(&ctx, &mut journal, "fresh", args.clone())["code"], "INVALID_PATH");
    assert_eq!(write(&ctx, &mut journal, "resume", args)["code"], "INVALID_PATH");
    assert!(!outside.path().join("missing").exists());
    assert!(!journal.events.iter().any(|e| e.kind == "edit_receipt"));
    #[cfg(windows)]
    fs::remove_dir(link).unwrap();
    #[cfg(unix)]
    fs::remove_file(link).unwrap();
}

#[test]
fn edit_text_auto_preserves_crlf_diagnoses_anchors_and_warns_dropped_newline() {
    let root = tempfile::tempdir().unwrap();
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root.path())).unwrap();
    let mut journal = Journal::open(&root.path().join(".sourceharness")).unwrap();
    fs::write(root.path().join("service.cfg"), b"header\r\n[primary]\r\ntimeout=30\r\nretries=2\r\n").unwrap();

    let crlf_ok = compact::modify(
        &ctx, &mut journal, 1, "1.1.1", "edit",
        &json!({"path":"service.cfg","changes":[{"old_text":"timeout=30\nretries=2","new_text":"timeout=60\nretries=2"}]}),
    ).unwrap();
    assert_eq!(crlf_ok["status"], "ok");
    assert_eq!(crlf_ok["eol"], "crlf");
    assert_eq!(fs::read(root.path().join("service.cfg")).unwrap(), b"header\r\n[primary]\r\ntimeout=60\r\nretries=2\r\n");

    let anchor_err = compact::modify(
        &ctx, &mut journal, 1, "1.2.1", "edit",
        &json!({"path":"service.cfg","changes":[{"old_text":"  timeout=60\r\nwrong_next_line","new_text":"timeout=90"}]}),
    ).unwrap();
    assert_eq!(anchor_err["code"], "TEXT_NOT_FOUND");
    assert_eq!(anchor_err["anchor_lines"], json!([3]));
    assert!(anchor_err["read"]["result_ref"].is_string());

    let drop_nl = compact::modify(
        &ctx, &mut journal, 1, "1.3.1", "write",
        &json!({"path":"service.cfg","content":"timeout=90"}),
    ).unwrap();
    assert_eq!(drop_nl["status"], "ok");
    assert_eq!(drop_nl["dropped_trailing_newline"], json!(true));
    assert!(compact::present("write", &drop_nl, None).as_str().unwrap().contains("WARNING: dropped existing trailing newline."));
}

#[test]
fn normal_no_op_edit_is_rejected_without_creating_intent() {
    let root = tempfile::tempdir().unwrap();
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root.path())).unwrap();
    let mut journal = Journal::open(&root.path().join(".sourceharness")).unwrap();
    fs::write(root.path().join("note.txt"), b"alpha=1\n").unwrap();

    let res = compact::modify(
        &ctx, &mut journal, 1, "1.1.1", "edit",
        &json!({"path":"note.txt","changes":[{"old_text":"alpha=1","new_text":"alpha=1"}]}),
    ).unwrap();
    assert_eq!(res["code"], "NO_OP_EDIT");
    assert!(!journal.events.iter().any(|e| e.kind == "edit_intent"));
}
