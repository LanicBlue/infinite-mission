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
        .args(["join", "alice", "--role", "worker"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Joined as alice (role: worker)"));

    // Same ID again → auto-suffix, never impersonation.
    im(tmp.path())
        .args(["join", "alice", "--role", "worker"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Id 'alice' was taken. Joined as alice-2 (role: worker).",
        ));

    im(tmp.path())
        .arg("agents")
        .assert()
        .success()
        .stdout(predicate::str::contains("alice"))
        .stdout(predicate::str::contains("alice-2"));
}

#[test]
fn send_receive_reply_roundtrip() {
    let tmp = setup_workspace();
    for id in ["alice", "bob"] {
        im(tmp.path()).args(["join", id, "--role", "worker"]).assert().success();
    }

    im(tmp.path())
        .args(["send", "alice", "bob", "implement the auth module"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Sent to bob."));

    let received = String::from_utf8(
        im(tmp.path())
            .args(["receive", "bob"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    assert!(received.contains("[from alice] implement the auth module"));
    assert!(received.contains("Reply: im send bob alice"));

    // Consumed: a second receive is empty, pending stays empty.
    im(tmp.path())
        .args(["receive", "bob"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No new messages"));
    im(tmp.path())
        .args(["pending", "bob"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No pending messages"));

    // History still shows the roundtrip.
    im(tmp.path())
        .args(["history", "bob"])
        .assert()
        .success()
        .stdout(predicate::str::contains("alice -> bob"))
        .stdout(predicate::str::contains("implement the auth module"));
}

#[test]
fn broadcast_reaches_every_joined_agent() {
    let tmp = setup_workspace();
    for id in ["alice", "bob", "carol"] {
        im(tmp.path()).args(["join", id, "--role", "worker"]).assert().success();
    }
    im(tmp.path())
        .args(["send", "alice", "@all", "all hands at noon"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Broadcast to 2 agents"))
        .stdout(predicate::str::contains("bob"))
        .stdout(predicate::str::contains("carol"));

    im(tmp.path())
        .args(["receive", "carol"])
        .assert()
        .success()
        .stdout(predicate::str::contains("all hands at noon"));
}

#[test]
fn receive_wait_times_out_and_lock_is_exclusive() {
    let tmp = setup_workspace();
    im(tmp.path()).args(["join", "alice", "--role", "worker"]).assert().success();

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
fn receive_wait_wakes_on_arrival() {
    let tmp = setup_workspace();
    for id in ["alice", "bob"] {
        im(tmp.path()).args(["join", id, "--role", "worker"]).assert().success();
    }

    let ws = tmp.path().to_path_buf();
    let waiter = std::thread::spawn(move || {
        let out = StdCommand::new(env!("CARGO_BIN_EXE_im"))
            .current_dir(&ws)
            .args(["receive", "bob", "--wait", "--timeout", "10"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    });

    sleep(Duration::from_millis(900));
    im(tmp.path())
        .args(["send", "alice", "bob", "wake up"])
        .assert()
        .success();

    let out = waiter.join().unwrap();
    assert!(out.contains("wake up"), "got: {out}");
    assert!(!out.contains("timed out"), "waiter should return early: {out}");
}

#[test]
fn leave_archives_identity_and_join_reactivates_with_new_session() {
    let tmp = setup_workspace();
    let ws = tmp.path();
    im(ws).args(["join", "alice", "--role", "worker"]).assert().success();
    im(ws).args(["join", "bob", "--role", "worker"]).assert().success();

    im(ws)
        .args(["send", "bob", "alice", "read me when you're back"])
        .assert()
        .success();
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
        .args(["join", "alice", "--role", "worker"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Joined as alice"));
    // The unread message from the archived period survived.
    im(ws)
        .args(["receive", "alice"])
        .assert()
        .success()
        .stdout(predicate::str::contains("read me when you're back"));

    // A stale terminal (old session file) is displaced by the new join.
    std::fs::write(ws.join(".im").join("sessions").join("alice"), "stale-token").unwrap();
    im(ws)
        .args(["receive", "alice"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Session replaced"));
}

#[test]
fn operator_commands_are_gated_by_grant() {
    let tmp = setup_workspace();
    im(tmp.path()).args(["join", "boss", "--role", "manager"]).assert().success();
    im(tmp.path()).args(["join", "rando", "--role", "worker"]).assert().success();

    // Before any grant: nobody may create stations.
    im(tmp.path())
        .args(["work", "create", "boss", "build"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("operator"));

    im(tmp.path()).args(["grant", "boss"]).assert().success();
    im(tmp.path())
        .args(["work", "create", "boss", "build"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Created station build"));

    // Grant revoked → the gate closes again.
    im(tmp.path()).args(["revoke", "boss"]).assert().success();
    im(tmp.path())
        .args(["work", "create", "boss", "review"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("operator"));
}

#[test]
fn mission_end_fans_out_to_past_participants() {
    let tmp = setup_workspace();
    let ws = tmp.path();
    for (id, role) in [("boss", "manager"), ("worker", "worker"), ("inspector", "inspector")] {
        im(ws).args(["join", id, "--role", role]).assert().success();
    }
    im(ws).args(["grant", "boss"]).assert().success();
    im(ws).args(["work", "create", "boss", "build", "--executor", "worker"]).assert().success();
    im(ws).args(["work", "create", "boss", "review", "--executor", "inspector"]).assert().success();
    std::fs::write(
        ws.join(".im").join("templates").join("t.yaml"),
        "schemaVersion: 4\nname: t\nentry: build\nworks:\n  build:\n    completion: {outcomes: [done], terminal: [], feedbackRequiredOn: []}\n    documentRights: {read: [], write: []}\n  review:\n    completion: {outcomes: [pass, fail], terminal: [pass], feedbackRequiredOn: [fail]}\n    documentRights: {read: [], write: []}\npaths:\n  - {from: build, when: done, to: review}\n  - {from: review, when: fail, to: build}\n",
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
