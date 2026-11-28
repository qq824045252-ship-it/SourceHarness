use super::*;
use crate::workspace::WorkspaceConfig;

#[test]
#[ignore = "subprocess fixture for declared external input validation"]
fn run_test_checker() {
    let rule = fs::read_to_string("rule_path.txt").expect("fixture has an external rule path");
    assert_eq!(fs::read(&rule).unwrap(), b"allow");
    if Path::new("change_rule_during_check").exists() {
        fs::write(rule, "deny").unwrap();
    }
}

#[test]
fn external_rule_content_changes_the_validation_and_its_provenance() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    fs::create_dir(&root).unwrap();
    let rule = temp.path().join("oracle.txt");
    fs::write(&rule, "allow").unwrap();
    fs::write(root.join("rule_path.txt"), rule.to_str().unwrap()).unwrap();

    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(&root)).unwrap();
    let mut journal = Journal::open(&root.join(".sourceharness/sessions/work")).unwrap();
    let child_test_name = format!(
        "{}::run_test_checker",
        module_path!().split_once("::").unwrap().1
    );
    let config = json!({"rule": {
        "program":std::env::current_exe().unwrap(),
        "args":["--exact",child_test_name,"--ignored"],
        "inputs":[rule],
    }});
    let commands: Commands = serde_json::from_value(config.clone()).unwrap();
    let profile = json!({"validation":commands});
    journal.append(1, "user", json!({"profile":profile})).unwrap();
    let tool_args = json!({"check":"rule"});
    let initial_inputs = commands.input_revision_profile();
    let first = run(&ctx, &mut journal, 1, "1.1.1", &commands, &tool_args).unwrap();
    assert_eq!(first["passed"], true);
    assert_eq!(first["input_revisions"][rule.to_str().unwrap()], revision(b"allow"));
    assert_eq!(first["input_revisions_stable"], true);

    fs::write(&rule, "deny").unwrap();
    assert_ne!(initial_inputs, commands.input_revision_profile());
    let second = run(&ctx, &mut journal, 2, "2.1.1", &commands, &tool_args).unwrap();
    assert_eq!(second["passed"], false);
    assert_eq!(first["workspace_revision"], second["workspace_revision"]);
    assert_eq!(first["program_revision"], second["program_revision"]);

    fs::write(&rule, "allow").unwrap();
    fs::write(root.join("change_rule_during_check"), []).unwrap();
    let changed = run(&ctx, &mut journal, 2, "2.2.1", &commands, &tool_args).unwrap();
    assert_eq!(changed["exit_code"], 0);
    assert_eq!(changed["passed"], false);
    assert_eq!(changed["input_revisions_stable"], false);
    assert_eq!(changed["input_revisions_after"][rule.to_str().unwrap()], revision(b"deny"));
}
