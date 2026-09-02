use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;
use std::process::Command as StdCommand;
use std::thread::sleep;
use std::time::Duration;
use tempfile::TempDir;

fn im(workspace: &Path) -> Command {
    let mut cmd = Command::cargo_bin("im").unwrap();
    cmd.current_dir(workspace);
    cmd
}

fn setup_workspace() -> TempDir {
    let tmp = TempDir::new().unwrap();
    im(tmp.path()).arg("init").assert().success();
    tmp
}

#[test]
fn join_collision_appends_suffix() {
    let tmp = setup_workspace();
    im(tmp.path())
        .args(["join", "alice"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Joined as alice"));

    // Same ID again → auto-suffix, never impersonation.
    im(tmp.path())
        .args(["join", "alice"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Id 'alice' was taken. Joined as alice-2.",
        ));

    im(tmp.path())
        .arg("agents")
        .assert()
        .success()
        .stdout(predicate::str::contains("alice"))
        .stdout(predicate::str::contains("alice-2"));
}

#[test]
fn peer_send_is_rejected() {
    let tmp = setup_workspace();
    for id in ["alice", "bob"] {
        im(tmp.path()).args(["join", id]).assert().success();
    }

    im(tmp.path())
        .args(["send", "alice", "bob", "implement the auth module"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no peer messaging"));

    im(tmp.path())
        .args(["send", "alice", "@all", "all hands at noon"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no peer messaging"));
}

#[test]
fn receive_wait_times_out_and_lock_is_exclusive() {
    let tmp = setup_workspace();
    im(tmp.path()).args(["join", "alice"]).assert().success();

    let ws = tmp.path().to_path_buf();
    let waiter = std::thread::spawn(move || {
        let out = StdCommand::new(env!("CARGO_BIN_EXE_im"))
            .current_dir(&ws)
            .args(["receive", "alice", "--wait", "--timeout", "2"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    });

    // Give the waiter time to grab the lock.
    sleep(Duration::from_millis(900));
    im(tmp.path())
        .args(["receive", "alice", "--wait", "--timeout", "1"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already running"));

    let out = waiter.join().unwrap();
    assert!(out.contains("No new messages (timed out after 2s)"), "got: {out}");
}

#[test]
fn receive_wait_wakes_on_station_arrival() {
    let tmp = setup_workspace();
    let ws = tmp.path();
    for id in ["boss", "worker"] {
        im(ws).args(["join", id]).assert().success();
    }
    im(ws).args(["grant", "boss"]).assert().success();
    im(ws)
        .args(["work", "create", "boss", "alpha", "--executor", "worker"])
        .assert()
        .success();
    std::fs::write(
        ws.join(".im").join("templates").join("t.yaml"),
        "schemaVersion: 4\nname: t\nentry: alpha\nworks:\n  alpha:\n    completion: {outcomes: [done], terminal: [done], feedbackRequiredOn: []}\n    documentRights: {read: [], write: []}\n",
    )
    .unwrap();

    let waiter_ws = ws.to_path_buf();
    let waiter = std::thread::spawn(move || {
        let out = StdCommand::new(env!("CARGO_BIN_EXE_im"))
            .current_dir(&waiter_ws)
            .args(["receive", "worker", "--wait", "--timeout", "10"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    });

    sleep(Duration::from_millis(900));
    im(ws)
        .args(["mission", "create", "boss", "--template", "t", "--key", "k1"])
        .assert()
        .success();

    let out = waiter.join().unwrap();
    assert!(out.contains("[station alpha]"), "got: {out}");
    assert!(!out.contains("timed out"), "waiter should return early: {out}");
}

#[test]
fn membership_changes_notify_the_member() {
    let tmp = setup_workspace();
    im(tmp.path()).args(["join", "alice"]).assert().success();

    im(tmp.path())
        .args(["grant", "alice"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Granted manager to alice."));
    im(tmp.path())
        .args(["receive", "alice"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "[from workspace] [membership] you were granted manager permission.",
        ));

    im(tmp.path()).args(["revoke", "alice"]).assert().success();
    im(tmp.path())
        .args(["receive", "alice"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "[membership] your manager permission was revoked.",
        ));
}

#[test]
fn receive_wait_farewells_when_membership_ends() {
    let tmp = setup_workspace();
    im(tmp.path()).args(["join", "alice"]).assert().success();

    let ws = tmp.path().to_path_buf();
    let waiter = std::thread::spawn(move || {
        let out = StdCommand::new(env!("CARGO_BIN_EXE_im"))
            .current_dir(&ws)
            .args(["receive", "alice", "--wait", "--timeout", "10"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    });

    // Once the listener is inside its poll loop, delete the member under it.
    sleep(Duration::from_millis(1200));
    let store = im::store::Store::open(&tmp.path().join(".im").join("im.db")).unwrap();
    store.delete_agent("workspace", "alice").unwrap();
    drop(store);

    let out = waiter.join().unwrap();
    assert!(
        out.contains("no longer an active member"),
        "waiter should farewell, got: {out}"
    );
    assert!(!out.contains("Error"), "should not error, got: {out}");
}

#[test]
fn leave_archives_identity_and_join_reactivates_with_new_session() {
    let tmp = setup_workspace();
    let ws = tmp.path();
    im(ws).args(["join", "alice"]).assert().success();
    im(ws).args(["join", "bob"]).assert().success();

    im(ws).args(["grant", "alice"]).assert().success();
    im(ws).args(["leave", "alice"]).assert().success();

    // Archived: gone from the active roster, still visible with --all.
    im(ws).arg("agents").assert().success().stdout(predicate::str::contains("bob"));
    let active = String::from_utf8(
        im(ws).arg("agents").assert().success().get_output().stdout.clone(),
    )
    .unwrap();
    assert!(!active.contains("alice"));
    im(ws)
        .args(["agents", "--all"])
        .assert()
        .success()
        .stdout(predicate::str::contains("alice"));

    // Joining the same id reactivates it (comeback), with a fresh session token.
    im(ws)
        .args(["join", "alice"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Joined as alice"));
    // The unread membership notice from the archived period survived.
    im(ws)
        .args(["receive", "alice"])
        .assert()
        .success()
        .stdout(predicate::str::contains("granted manager permission"));

    // A stale terminal (old session file) is displaced by the new join.
    std::fs::write(ws.join(".im").join("sessions").join("alice"), "stale-token").unwrap();
    im(ws)
        .args(["receive", "alice"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Session replaced"));
}

#[test]
fn manager_commands_are_gated_by_grant() {
    let tmp = setup_workspace();
    im(tmp.path()).args(["join", "boss"]).assert().success();
    im(tmp.path()).args(["join", "rando"]).assert().success();

    // Before any grant: nobody may create stations.
    im(tmp.path())
        .args(["work", "create", "boss", "alpha"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("manager"));

    im(tmp.path()).args(["grant", "boss"]).assert().success();
    im(tmp.path())
        .args(["work", "create", "boss", "alpha"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Created station alpha"));

    // Grant revoked → the gate closes again.
    im(tmp.path()).args(["revoke", "boss"]).assert().success();
    im(tmp.path())
        .args(["work", "create", "boss", "beta"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("manager"));
}

#[test]
fn mission_end_fans_out_to_past_participants() {
    let tmp = setup_workspace();
    let ws = tmp.path();
    for id in ["boss", "worker", "inspector"] {
        im(ws).args(["join", id]).assert().success();
    }
    im(ws).args(["grant", "boss"]).assert().success();
    im(ws).args(["work", "create", "boss", "alpha", "--executor", "worker"]).assert().success();
    im(ws).args(["work", "create", "boss", "beta", "--executor", "inspector"]).assert().success();
    std::fs::write(
        ws.join(".im").join("templates").join("t.yaml"),
        "schemaVersion: 4\nname: t\nentry: alpha\nworks:\n  alpha:\n    completion: {outcomes: [done], terminal: [], feedbackRequiredOn: []}\n    documentRights: {read: [], write: []}\n  beta:\n    completion: {outcomes: [pass, fail], terminal: [pass], feedbackRequiredOn: [fail]}\n    documentRights: {read: [], write: []}\npaths:\n  - {from: alpha, when: done, to: beta}\n  - {from: beta, when: fail, to: alpha}\n",
    )
    .unwrap();
    im(ws)
        .args(["mission", "create", "boss", "--template", "t", "--key", "k1"])
        .assert()
        .success();
    let ms = {
        let db = rusqlite::Connection::open(ws.join(".im").join("im.db")).unwrap();
        db.query_row("SELECT mission_id FROM missions", [], |r| r.get::<_, String>(0))
            .unwrap()
    };

    // build → review → terminal pass: worker is a past participant.
    im(ws).args(["mission", "submit", "worker", &ms, "--revision", "1", "--outcome", "done"]).assert().success();
    im(ws).args(["mission", "submit", "inspector", &ms, "--revision", "2", "--outcome", "pass"]).assert().success();

    // The ended fan-out reaches the worker even though the mailbox moved on.
    im(ws)
        .args(["receive", "worker"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ended"))
        .stdout(predicate::str::contains(&ms[..12]));
}

#[test]
fn init_seeds_pipeline_stations_and_reactivates_retired_ones() {
    let tmp = setup_workspace();
    let ws = tmp.path();

    // Four stations out of the box, all active, plus the template.
    let list = String::from_utf8(
        im(ws).args(["work", "list"]).assert().success().get_output().stdout.clone(),
    )
    .unwrap();
    for key in ["design", "plan", "build", "review"] {
        assert!(list.contains(key), "seeded station {key} missing: {list}");
    }
    assert!(!list.contains("[retired]"), "fresh seeds must be active: {list}");
    assert!(ws.join(".im").join("templates").join("pipeline.yaml").exists());

    // Seeded stations carry their preset charters.
    let db = rusqlite::Connection::open(ws.join(".im").join("im.db")).unwrap();
    let design_prompt: String = db
        .query_row("SELECT prompt FROM works WHERE work_key = 'design'", [], |r| r.get(0))
        .unwrap();
    assert!(design_prompt.contains("final gate"), "design charter missing: {design_prompt}");
    drop(db);

    // Re-init is a no-op for existing stations.
    im(ws).arg("init").assert().success();

    // A retired pipeline key would break every pipeline mission create
    // (retired stations cannot be referenced) — re-init reactivates it.
    im(ws).args(["join", "boss"]).assert().success();
    im(ws).args(["grant", "boss"]).assert().success();
    im(ws).args(["work", "retire", "boss", "design"]).assert().success();
    im(ws).arg("init").assert().success();
    let relist = String::from_utf8(
        im(ws).args(["work", "list"]).assert().success().get_output().stdout.clone(),
    )
    .unwrap();
    assert!(
        !relist.contains("[retired]"),
        "retired pipeline key should have been reactivated: {relist}"
    );
}

#[test]
fn work_presets_fill_charters_and_unretire_reopens_a_key() {
    let tmp = setup_workspace();
    let ws = tmp.path();
    im(ws).args(["join", "boss"]).assert().success();
    im(ws).args(["grant", "boss"]).assert().success();

    // Unknown preset fails closed with the available list.
    im(ws)
        .args(["work", "create", "boss", "qa", "--preset", "nope"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown preset"));

    // --preset fills the charter (and display name); an explicit --prompt wins.
    im(ws).args(["work", "create", "boss", "qa", "--preset", "review"]).assert().success();
    im(ws)
        .args(["work", "create", "boss", "hotfix", "--preset", "build", "--prompt", "just ship it"])
        .assert()
        .success();
    let db = rusqlite::Connection::open(ws.join(".im").join("im.db")).unwrap();
    let qa_prompt: String = db
        .query_row("SELECT prompt FROM works WHERE work_key = 'qa'", [], |r| r.get(0))
        .unwrap();
    let qa_name: String = db
        .query_row("SELECT display_name FROM works WHERE work_key = 'qa'", [], |r| r.get(0))
        .unwrap();
    let hotfix_prompt: String = db
        .query_row("SELECT prompt FROM works WHERE work_key = 'hotfix'", [], |r| r.get(0))
        .unwrap();
    assert!(qa_prompt.contains("verify the implementation"), "qa: {qa_prompt}");
    assert_eq!(qa_name, "Review");
    assert_eq!(hotfix_prompt, "just ship it");

    // set-prompt --preset re-applies a charter on an existing station.
    im(ws)
        .args(["work", "set-prompt", "boss", "qa", "--preset", "design"])
        .assert()
        .success()
        .stdout(predicate::str::contains("preset 'design'"));

    // retire → the key is dead for mission creates → unretire reopens it.
    im(ws).args(["work", "retire", "boss", "qa"]).assert().success();
    std::fs::write(
        ws.join(".im").join("templates").join("t.yaml"),
        "schemaVersion: 4\nname: t\nentry: qa\nworks:\n  qa:\n    completion: {outcomes: [done], terminal: [done], feedbackRequiredOn: []}\n    documentRights: {read: [], write: []}\n",
    )
    .unwrap();
    im(ws)
        .args(["mission", "create", "boss", "--template", "t", "--key", "k1"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("retired"));
    im(ws).args(["work", "unretire", "boss", "qa"]).assert().success();
    im(ws)
        .args(["mission", "create", "boss", "--template", "t", "--key", "k1"])
        .assert()
        .success();
    // unretire on an active station fails closed.
    im(ws)
        .args(["work", "unretire", "boss", "qa"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not retired"));
}

#[test]
fn retiring_a_station_releases_its_executor() {
    let tmp = setup_workspace();
    let ws = tmp.path();
    im(ws).args(["join", "boss"]).assert().success();
    im(ws).args(["join", "temp"]).assert().success();
    im(ws).args(["grant", "boss"]).assert().success();
    im(ws)
        .args(["work", "create", "boss", "lab", "--executor", "temp"])
        .assert()
        .success();

    // Retire releases the guard: the executor binding goes with it, so
    // member leave/delete can never dangle off a dead station.
    im(ws).args(["work", "retire", "boss", "lab"]).assert().success();
    let store = im::store::Store::open(&ws.join(".im").join("im.db")).unwrap();
    let work = store.get_work("lab").unwrap();
    assert_eq!(work.lifecycle, "retired");
    assert!(work.executor.is_none(), "retire must release the executor");
    store.delete_agent("workspace", "temp").unwrap();
    drop(store);

    // Unretire brings the station back as a user station.
    im(ws).args(["work", "unretire", "boss", "lab"]).assert().success();
    let store = im::store::Store::open(&ws.join(".im").join("im.db")).unwrap();
    assert!(store.get_work("lab").unwrap().executor.is_none());
}
