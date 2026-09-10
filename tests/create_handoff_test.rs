use assert_cmd::Command;
use im::mission::{MissionCreateOutcome, RoundSubmission, TemplateSource};
use im::records::Tier;
use im::store::Store;
use predicates::prelude::*;
use std::path::Path;
use tempfile::TempDir;

const TEMPLATE: &str = r#"schemaVersion: 4
name: handoff
entry: draft
works:
  draft:
    completion:
      outcomes: [ready, finish]
      terminal: [finish]
      feedbackRequiredOn: []
    documentRights: {read: [spec], write: [spec]}
  audit:
    completion:
      outcomes: [back]
      terminal: []
      feedbackRequiredOn: []
    documentRights: {read: [spec], write: []}
documents:
  - {id: spec, kind: file, path: spec.md}
paths:
  - {from: draft, when: ready, to: audit}
  - {from: audit, when: back, to: draft, iterationPolicy: increment}
"#;

fn cli(ws: &Path) -> Command {
    let mut cmd = Command::cargo_bin("im").unwrap();
    cmd.current_dir(ws);
    // 测试 HOME：把注册表写进一次性目录，绝不污染真实 ~/.im/workspaces.json
    cmd.env("HOME", test_home());
    cmd
}

fn test_home() -> std::path::PathBuf {
    static HOME: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        let home = std::env::temp_dir().join(format!("im-test-home-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        home
    })
    .clone()
}

fn setup(executor: Option<&str>) -> (TempDir, Store) {
    let tmp = TempDir::new().unwrap();
    cli(tmp.path()).arg("init").assert().success();
    for member in ["owner", "worker"] {
        cli(tmp.path()).args(["join", member]).assert().success();
    }
    let store = Store::open(&tmp.path().join(".im/im.db")).unwrap();
    store
        .set_agent_tier("workspace", "owner", Tier::Manage)
        .unwrap();
    store
        .set_agent_tier("workspace", "worker", Tier::Publish)
        .unwrap();
    store
        .create_work(
            "owner",
            "draft",
            "design",
            executor,
            "Freeze {mission.name}: {mission.objective}",
        )
        .unwrap();
    store
        .create_work("owner", "audit", "review", Some("worker"), "Review")
        .unwrap();
    // A user work the manage-tier creator can issue missions from.
    store
        .create_work("owner", "approve", "review", None, "User gate")
        .unwrap();
    std::fs::write(tmp.path().join(".im/templates/handoff.yaml"), TEMPLATE).unwrap();
    (tmp, store)
}

fn create(store: &Store, creator: &str) -> MissionCreateOutcome {
    let template = im::contract::parse_template(TEMPLATE).unwrap();
    store
        .create_mission(
            creator,
            &TemplateSource {
                template: &template,
                path: ".im/templates/handoff.yaml".into(),
                bytes: TEMPLATE.as_bytes(),
            },
            "one",
            None,
            Some("ship safely"),
        )
        .unwrap()
}

fn route(store: &Store, who: &str, id: &str, revision: i64, outcome: &str) {
    store
        .submit_mission(
            who,
            id,
            revision,
            outcome,
            &RoundSubmission {
                next_node: None,
                reason: Some("test"),
                feedback: None,
                result: None,
                receipt_ids: &[],
            },
        )
        .unwrap();
}

fn note_count(store: &Store) -> i64 {
    store
        .conn
        .query_row("SELECT COUNT(*) FROM work_notes", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn creator_gets_complete_view_atomically_without_first_arrival() {
    let (_tmp, store) = setup(Some("owner"));
    let created = create(&store, "owner");
    let view = created.run_view.as_ref().unwrap();
    assert!(!created.existed);
    assert!(view.on_duty);
    assert_eq!(view.revision, 1);
    assert_eq!(view.iteration, Some(1));
    assert_eq!(view.at.as_deref(), Some("draft"));
    assert_eq!(view.prompt.as_deref(), Some("Freeze handoff: ship safely"));
    assert_eq!(view.outcomes, ["ready", "finish"]);
    assert_eq!(view.terminal, ["finish"]);
    assert_eq!(view.routes.len(), 3); // ready, finish and the reserved abandon route
    assert_eq!(view.documents[0].path, "spec.md");
    assert!(view.documents[0].may_read && view.documents[0].may_write);
    assert_eq!(note_count(&store), 0);
    assert!(!store.has_unread_work_notes("owner").unwrap());
    assert_eq!(store.mission_events(&created.mission_id).unwrap().len(), 1);
    let retry = create(&store, "owner");
    assert!(retry.existed);
    assert_eq!(
        serde_json::to_value(retry.run_view).unwrap(),
        serde_json::to_value(view).unwrap()
    );
    assert_eq!(note_count(&store), 0);
    // Suppressing a redundant work arrival must not consume membership notices.
    assert!(store.has_unread_messages("owner").unwrap());
}

#[test]
fn other_executor_still_receives_exactly_one_first_arrival() {
    let (_tmp, store) = setup(Some("worker"));
    let created = create(&store, "owner");
    assert!(created.run_view.is_none());
    assert!(create(&store, "owner").run_view.is_none());
    assert_eq!(note_count(&store), 1);
    let notes = store.receive_work_notes("worker").unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(
        notes[0].mission_id.as_deref(),
        Some(created.mission_id.as_str())
    );
    assert!(store.receive_work_notes("worker").unwrap().is_empty());
}

#[test]
fn manage_rights_at_user_station_are_not_an_executor_match() {
    let (_tmp, store) = setup(None);
    let created = create(&store, "owner");
    assert!(created.run_view.is_none());
    assert_eq!(note_count(&store), 1);
    assert_eq!(store.inbox_missions().unwrap().len(), 1);
    assert!(
        store
            .run_view(&created.mission_id, Some("owner"))
            .unwrap()
            .on_duty
    );
}

#[test]
fn later_arrivals_and_retries_use_current_authority_not_creation_revision() {
    let (_tmp, store) = setup(Some("owner"));
    let id = create(&store, "owner").mission_id;
    route(&store, "owner", &id, 1, "ready");
    assert!(create(&store, "owner").run_view.is_none());
    assert_eq!(note_count(&store), 1);
    assert_eq!(store.receive_work_notes("worker").unwrap().len(), 1);
    route(&store, "worker", &id, 2, "back");
    let retry = create(&store, "owner");
    let view = retry.run_view.unwrap();
    assert_eq!(view.revision, 3);
    assert_eq!(view.iteration, Some(2));
    assert!(retry.existed);
    // Returning the current view on a retry does NOT consume a later arrival.
    assert_eq!(note_count(&store), 2);
    assert_eq!(store.receive_work_notes("owner").unwrap().len(), 1);
    route(&store, "owner", &id, 3, "finish");
    let ended_retry = create(&store, "owner");
    assert!(ended_retry.existed && ended_retry.run_view.is_none());
    assert_eq!(store.get_mission(&id).unwrap().revision, 4);
    assert_eq!(note_count(&store), 2);
}

#[test]
fn retry_respects_changed_executor_and_does_not_resurrect_first_notice() {
    let (_tmp, store) = setup(Some("owner"));
    let id = create(&store, "owner").mission_id;
    store
        .set_work_executor("owner", "draft", Some("worker"))
        .unwrap();
    assert!(create(&store, "owner").run_view.is_none());
    let new_executor = create(&store, "worker").run_view.unwrap();
    assert!(new_executor.on_duty);
    assert_eq!(new_executor.mission_id, id);
    assert_eq!(new_executor.revision, 1);
    assert_eq!(note_count(&store), 0);
}

#[test]
fn cli_creation_prints_the_same_full_view_as_show_and_remains_discoverable() {
    let (tmp, store) = setup(Some("owner"));
    let output = cli(tmp.path())
        .args([
            "mission",
            "create",
            "owner",
            "--from",
            "draft",
            "--template",
            "handoff",
            "--key",
            "one",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Created mission ms_"))
        .stdout(predicate::str::contains("no extra arrival wake-up"))
        .stdout(predicate::str::contains("on duty: YES"))
        .get_output()
        .stdout
        .clone();
    // Work-origin CLI keys and legacy Store keys live in partitioned spaces,
    // so resolve the created mission id from the CLI output itself.
    let id = String::from_utf8(output.clone())
        .unwrap()
        .split_whitespace()
        .find(|word| word.starts_with("ms_"))
        .unwrap()
        .to_string();
    let show = cli(tmp.path())
        .args(["mission", "show", &id, "--for", "owner"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(String::from_utf8(output)
        .unwrap()
        .ends_with(&String::from_utf8(show).unwrap()));
    cli(tmp.path())
        .args(["missions", "owner"])
        .assert()
        .success()
        .stdout(predicate::str::contains(id));
    assert_eq!(note_count(&store), 0);
}

#[test]
fn console_creation_returns_structured_view_only_for_bound_creator() {
    for (executor, expect_view) in [
        (Some("owner"), true),
        (Some("worker"), false),
        (None, false),
    ] {
        let (tmp, store) = setup(executor);
        let response = im::ui::apply_action_response(
            &store,
            &serde_json::json!({
                "type": "mission_create", "template": "handoff", "key": "one",
                "originWork": "approve"
            }),
            tmp.path(),
        )
        .unwrap();
        assert_eq!(response["ok"], true);
        assert_eq!(response.get("runView").is_some(), expect_view);
        if expect_view {
            assert_eq!(response["runView"]["revision"], 1);
            assert_eq!(response["runView"]["on_duty"], true);
            assert_eq!(response["runView"]["documents"][0]["may_write"], true);
        }
        assert_eq!(note_count(&store), if expect_view { 0 } else { 1 });
    }
}

#[test]
fn waiting_creator_is_not_woken_by_synchronous_creation() {
    let (tmp, store) = setup(Some("owner"));
    store.receive_messages("owner").unwrap();
    let mut waiter = std::process::Command::new(env!("CARGO_BIN_EXE_im"))
        .current_dir(tmp.path())
        .args(["receive", "owner", "--wait", "--timeout", "2"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    create(&store, "owner");
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(waiter.try_wait().unwrap().is_none());
    let output = waiter.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("timed out after 2s"));
}
