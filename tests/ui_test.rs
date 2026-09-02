use assert_cmd::Command;
use std::path::Path;
use tempfile::TempDir;

fn im(workspace: &Path) -> Command {
    let mut cmd = Command::cargo_bin("im").unwrap();
    cmd.current_dir(workspace);
    cmd
}

/// The console page is a static file driven entirely by `state_json`; this
/// test pins that data contract so UI breakage surfaces as a test failure.
#[test]
fn state_json_exposes_the_console_data_contract() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path();
    im(ws).arg("init").assert().success();
    for id in ["boss", "worker"] {
        im(ws).args(["join", id]).assert().success();
    }
    im(ws).args(["grant", "boss"]).assert().success();
    im(ws).args(["work", "create", "boss", "make", "--executor", "worker"]).assert().success();
    im(ws).args(["work", "create", "boss", "approve"]).assert().success();
    std::fs::write(
        ws.join(".im").join("templates").join("t.yaml"),
        "schemaVersion: 4\nname: t\nentry: make\nworks:\n  make:\n    completion: {outcomes: [done], terminal: [], feedbackRequiredOn: []}\n    documentRights: {read: [], write: []}\n  approve:\n    completion: {outcomes: [ok], terminal: [ok], feedbackRequiredOn: []}\n    documentRights: {read: [], write: []}\npaths:\n  - {from: make, when: done, to: approve}\n",
    )
    .unwrap();
    im(ws)
        .args(["mission", "create", "boss", "--template", "t", "--key", "k1"])
        .assert()
        .success();
    let store = im::store::Store::open(&ws.join(".im").join("im.db")).unwrap();
    let mission_id: String = store
        .conn
        .query_row("SELECT mission_id FROM missions", [], |r| r.get(0))
        .unwrap();
    drop(store);

    // Hop onto the user station so the inbox has a row.
    im(ws)
        .args(["mission", "submit", "worker", &mission_id, "--revision", "1", "--outcome", "done", "--reason", "sign-off needed"])
        .assert()
        .success();

    let templates = std::fs::read_dir(ws.join(".im").join("templates"))
        .unwrap()
        .filter_map(|e| {
            let path = e.ok()?.path();
            if path.extension()? == "yaml" {
                path.file_stem()?.to_str().map(String::from)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    let store = im::store::Store::open(&ws.join(".im").join("im.db")).unwrap();
    let state = im::ui::state_json(&store, &ws.to_str().unwrap(), &templates).unwrap();

    // Top-level sections the page renders.
    for key in ["workspace", "agents", "managers", "works", "missions", "inbox", "events", "templates"] {
        assert!(state.get(key).is_some(), "state_json missing {key}");
    }
    assert_eq!(state["managers"].as_array().unwrap().len(), 1);
    assert_eq!(state["managers"][0].as_str().unwrap(), "boss");

    // Works carry executor and a holding count for the stations board.
    let works = state["works"].as_array().unwrap();
    let make = works.iter().find(|w| w["work_key"] == "make").expect("make station");
    assert_eq!(make["executor"].as_str().unwrap(), "worker");
    assert_eq!(make["holding"].as_i64().unwrap(), 0, "mailbox moved on");
    let approve = works.iter().find(|w| w["work_key"] == "approve").expect("user station");
    assert!(approve["executor"].is_null());

    // Missions carry at/revision; the inbox row carries the human reason.
    let missions = state["missions"].as_array().unwrap();
    assert_eq!(missions.len(), 1);
    assert_eq!(missions[0]["at"].as_str().unwrap(), "approve");
    assert!(missions[0]["revision"].as_i64().unwrap() >= 2);
    let inbox = state["inbox"].as_array().unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0]["mission_id"].as_str().unwrap(), mission_id);
    assert!(
        inbox[0]["reason"].as_str().unwrap().contains("sign-off"),
        "inbox row lost the reason: {}",
        inbox[0]
    );

    // Events feed the delivery-history timeline.
    let events = state["events"].as_array().unwrap();
    assert!(events.len() >= 2, "expected created+routed events, got {events:?}");
}

#[test]
fn console_can_grant_the_first_manager() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().to_path_buf();
    im(&ws).arg("init").assert().success();
    im(&ws).args(["join", "cursor"]).assert().success();

    let store = im::store::Store::open(&ws.join(".im").join("im.db")).unwrap();
    assert!(store.list_managers().unwrap().is_empty());

    let message = im::ui::apply_action(
        &store,
        &serde_json::json!({ "type": "grant", "agent": "cursor" }),
        &ws,
    )
    .unwrap();
    assert!(message.contains("granted manager to cursor"), "got: {message}");
    assert_eq!(store.list_managers().unwrap(), vec!["cursor".to_string()]);

    // Bootstrap grant is enough for the rest of the console (create a station).
    let created = im::ui::apply_action(
        &store,
        &serde_json::json!({
            "type": "work_create",
            "work": "staging",
            "display_name": "Staging",
            "executor": "cursor",
            "prompt": ""
        }),
        &ws,
    )
    .unwrap();
    assert!(created.contains("station staging created"), "got: {created}");
}

#[test]
fn state_json_lists_work_presets_for_the_create_modal() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path();
    im(ws).arg("init").assert().success();
    let store = im::store::Store::open(&ws.join(".im").join("im.db")).unwrap();
    let state = im::ui::state_json(&store, ws.to_str().unwrap(), &[]).unwrap();

    let presets = state["presets"].as_array().unwrap();
    let keys: Vec<&str> = presets.iter().map(|p| p["key"].as_str().unwrap()).collect();
    assert_eq!(keys, vec!["design", "plan", "build", "review"]);
    for preset in presets {
        assert!(
            preset["prompt"].as_str().unwrap().contains("{mission.objective}"),
            "{}",
            preset["key"]
        );
    }
}
