use super::{modify, EditMode, Journal};
use crate::session::journal::revision;
use crate::workspace::{WorkspaceConfig, WorkspaceContext};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

fn write(ctx: &WorkspaceContext, journal: &mut Journal, operation: &str, args: Value) -> Value {
    modify(ctx, journal, 1, operation, "write", EditMode::Text, &args).unwrap()
}

#[test]
fn create_makes_workspace_parents_and_preserves_no_clobber_behavior() {
    let root = tempfile::tempdir().unwrap();
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root.path())).unwrap();
    let mut journal = Journal::open(&root.path().join(".sourceharness")).unwrap();
    let relative = "src/new/nested/module.rs";
    let path = root.path().join(relative);
    let first = write(
        &ctx,
        &mut journal,
        "create",
        json!({"path":relative,"mode":"create","content":"mod tests;\n"}),
    );
    assert_eq!(first["outcome"], "committed");
    assert_eq!(first["source"]["path"], relative);
    assert_eq!(fs::read(&path).unwrap(), b"mod tests;\n");
    let repeated = write(
        &ctx,
        &mut journal,
        "repeat",
        json!({"path":relative,"mode":"create","content":"replacement"}),
    );
    assert_eq!(repeated["code"], "REVISION_CONFLICT");
    assert_eq!(repeated["current_revision"], first["revision"]);
    assert_eq!(fs::read(&path).unwrap(), b"mod tests;\n");
}

#[test]
fn resumes_a_create_intent_after_only_some_parents_were_created() {
    let root = tempfile::tempdir().unwrap();
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root.path())).unwrap();
    let state = root.path().join(".sourceharness");
    let relative = "src/new/nested/module.rs";
    let content = b"pub fn new() {}\n";
    {
        let mut journal = Journal::open(&state).unwrap();
        let after = journal.put(content).unwrap();
        journal
            .append(
                1,
                "edit_intent",
                json!({"operation":"create","path":relative,"before":null,"after":after}),
            )
            .unwrap();
    }
    fs::create_dir(root.path().join("src")).unwrap();
    let mut journal = Journal::open(&state).unwrap();
    let args = json!({"path":relative,"mode":"create","content":"pub fn new() {}\n"});
    let resumed = write(&ctx, &mut journal, "create", args.clone());
    assert_eq!(resumed["outcome"], "committed");
    assert_eq!(fs::read(root.path().join(relative)).unwrap(), content);
    assert_eq!(write(&ctx, &mut journal, "create", args), resumed);
    assert_eq!(
        journal
            .events
            .iter()
            .filter(|event| event.kind == "edit_receipt")
            .count(),
        1
    );
}

#[test]
fn patch_distinguishes_transport_delimiters_from_file_content() {
    let root = tempfile::tempdir().unwrap();
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root.path())).unwrap();
    let mut journal = Journal::open(&root.path().join(".sourceharness")).unwrap();
    let file = root.path().join("config.txt");
    let initial = b"[primary]\ntimeout=30\n\n[archive]\ntimeout=30\n";
    fs::write(&file, initial).unwrap();
    let apply = |journal: &mut Journal, operation: &str, expected_revision: String, patch: &str| {
        modify(
            &ctx,
            journal,
            1,
            operation,
            "edit",
            EditMode::Patch,
            &json!({"path":"config.txt","expected_revision":expected_revision,"patch":patch}),
        )
        .unwrap()
    };
    for (index, malformed) in ["[primary]\ntimeout=60", "@@"].iter().enumerate() {
        let rejected = apply(&mut journal, &format!("bad-{index}"), revision(initial), malformed);
        assert_eq!(rejected["code"], "INVALID_PATCH");
        assert!(rejected["reason"].as_str().unwrap().contains("@@ -1,2 +1,2 @@"));
        assert_eq!(fs::read(&file).unwrap(), initial);
    }
    assert!(!journal.events.iter().any(|event| event.kind == "edit_intent"));

    let patch = "--- a/config.txt\n+++ b/config.txt\n@@ -1,2 +1,2 @@\n [primary]\n-timeout=30\n+timeout=60";
    let changed = apply(&mut journal, "delimiter", revision(initial), patch);
    assert_eq!(changed["outcome"], "committed");
    assert_eq!(fs::read(&file).unwrap(), b"[primary]\ntimeout=60\n\n[archive]\ntimeout=30\n");

    fs::write(&file, b"before").unwrap();
    let patch = "--- a/config.txt\n+++ b/config.txt\n@@ -1 +1 @@\n-before\n\\ No newline at end of file\n+after\n\\ No newline at end of file";
    let changed = apply(&mut journal, "explicit-no-newline", revision(b"before"), patch);
    assert_eq!(changed["outcome"], "committed");
    assert_eq!(fs::read(file).unwrap(), b"after");
}

#[cfg(windows)]
fn redirect_directory(link: &Path, target: &Path) {
    let output = std::process::Command::new("cmd.exe")
        .args(["/D", "/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

#[cfg(unix)]
fn redirect_directory(link: &Path, target: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[cfg(any(windows, unix))]
#[test]
fn create_and_resume_reject_nested_directories_redirected_outside_workspace() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root.path())).unwrap();
    let mut journal = Journal::open(&root.path().join(".sourceharness")).unwrap();
    let relative = "redirect/missing/nested/module.rs";
    let after = journal.put(b"planned").unwrap();
    journal
        .append(
            1,
            "edit_intent",
            json!({"operation":"resume","path":relative,"before":null,"after":after}),
        )
        .unwrap();
    let link = root.path().join("redirect");
    redirect_directory(&link, outside.path());

    let args = json!({"path":relative,"mode":"create","content":"planned"});
    let fresh = write(&ctx, &mut journal, "fresh", args.clone());
    let resumed = write(&ctx, &mut journal, "resume", args);
    assert_eq!(fresh["code"], "INVALID_PATH");
    assert_eq!(resumed["code"], "INVALID_PATH");
    assert!(!outside.path().join("missing").exists());
    assert!(!journal.events.iter().any(|event| event.kind == "edit_receipt"));

    #[cfg(windows)]
    fs::remove_dir(link).unwrap();
    #[cfg(unix)]
    fs::remove_file(link).unwrap();
}
