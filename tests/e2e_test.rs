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

/// Seed a member tier directly (the console path — the bare `im grant <id>`
/// bootstrap retired with the tier ladder).
fn seed_tier(ws: &Path, id: &str, tier: &str) {
    let db = rusqlite::Connection::open(ws.join(".im").join("im.db")).unwrap();
    db.execute(
        "UPDATE agents SET tier = ?2 WHERE id = ?1",
        rusqlite::params![id, tier],
    )
    .unwrap();
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
    seed_tier(ws, "boss", "manage");
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
    let ws = tmp.path();
    im(ws).args(["join", "boss"]).assert().success();
    im(ws).args(["join", "alice"]).assert().success();
    seed_tier(ws, "boss", "manage");

    im(ws)
        .args(["grant", "boss", "alice"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Granted publish tier to alice."));
    im(ws)
        .args(["agents"])
        .assert()
        .success()
        .stdout(predicate::str::contains("alice"))  // id line
        ;
    let roster = String::from_utf8(
        im(ws).arg("agents").assert().success().get_output().stdout.clone(),
    )
    .unwrap();
    assert!(roster.contains("[publish]"), "roster shows tier: {roster}");
    im(ws)
        .args(["receive", "alice"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "[from boss] [membership] you were granted publish-tier permission.",
        ));

    im(ws).args(["revoke", "boss", "alice"]).assert().success();
    im(ws)
        .args(["receive", "alice"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "[membership] your publish-tier permission was revoked.",
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

    // Seed via the console path so the membership notice exists for the
    // archived-period assertion below.
    {
        let store = im::store::Store::open(&ws.join(".im").join("im.db")).unwrap();
        store
            .set_agent_tier("workspace", "alice", im::records::Tier::Manage)
            .unwrap();
    }
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
        .stdout(predicate::str::contains("tier was set to manage"));

    // A stale terminal (old session file) is displaced by the new join.
    std::fs::write(ws.join(".im").join("sessions").join("alice"), "stale-token").unwrap();
    im(ws)
        .args(["receive", "alice"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Session replaced"));
}

#[test]
fn gates_follow_the_tier_ladder() {
    let tmp = setup_workspace();
    let ws = tmp.path();
    for id in ["mgmt", "pub", "exec"] {
        im(ws).args(["join", id]).assert().success();
    }
    seed_tier(ws, "mgmt", "manage");
    seed_tier(ws, "pub", "publish");

    std::fs::write(
        ws.join(".im").join("templates").join("t.yaml"),
        "schemaVersion: 4\nname: t\nentry: alpha\nworks:\n  alpha:\n    completion: {outcomes: [done], terminal: [], feedbackRequiredOn: []}\n    documentRights: {read: [], write: []}\n  gate:\n    completion: {outcomes: [ok], terminal: [ok], feedbackRequiredOn: []}\n    documentRights: {read: [spec], write: [spec]}\npaths:\n  - {from: alpha, when: done, to: gate}\ndocuments:\n  - {id: spec, kind: file, path: docs/spec.md}\n",
    )
    .unwrap();
    im(ws)
        .args(["work", "create", "mgmt", "alpha", "--executor", "pub"])
        .assert()
        .success();
    im(ws).args(["work", "create", "mgmt", "gate"]).assert().success();

    // Execute: mission create fails.
    im(ws)
        .args(["mission", "create", "exec", "--template", "t", "--key", "k-exec"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("below publish tier"));

    // Publish: mission create is the one gate it passes…
    im(ws)
        .args(["mission", "create", "pub", "--template", "t", "--key", "k-pub"])
        .assert()
        .success();
    let ms = {
        let db = rusqlite::Connection::open(ws.join(".im").join("im.db")).unwrap();
        db.query_row("SELECT mission_id FROM missions", [], |r| r.get::<_, String>(0))
            .unwrap()
    };

    // …the five work ops stay manage-tier.
    for call in [
        vec!["work", "create", "pub", "extra"],
        vec!["work", "set-executor", "pub", "alpha", "-"],
        vec!["work", "set-prompt", "pub", "alpha", "nope"],
        vec!["work", "set-description", "pub", "alpha", "nope"],
        vec!["work", "delete", "pub", "alpha"],
    ] {
        im(ws)
            .args(&call)
            .assert()
            .failure()
            .stderr(predicate::str::contains("below manage tier"));
    }
    // Mission delete stays manage-tier.
    im(ws)
        .args(["mission", "end", "pub", &ms])
        .assert()
        .failure()
        .stderr(predicate::str::contains("below manage tier"));

    // Park the mission on the user station: publish may not resolve it.
    im(ws)
        .args(["mission", "submit", "pub", &ms, "--revision", "1", "--outcome", "done", "--reason", "over to the user"])
        .assert()
        .success();
    im(ws)
        .args(["mission", "submit", "pub", &ms, "--revision", "2", "--outcome", "ok"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("resolved by a manage-tier member"));
    im(ws)
        .args(["mission", "doc", "read", "pub", &ms, "docs/spec.md"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("resolved by a manage-tier member"));
    im(ws)
        .args(["mission", "doc", "write", "pub", &ms, "--id", "spec", "--file", "-"])
        .write_stdin("")
        .assert()
        .failure()
        .stderr(predicate::str::contains("resolved by a manage-tier member"));

    // Manage: everything above passes.
    im(ws)
        .args(["mission", "submit", "mgmt", &ms, "--revision", "2", "--outcome", "ok"])
        .assert()
        .success();
}

#[test]
fn cli_grant_revoke_and_member_delete_use_operator_forms() {
    let tmp = setup_workspace();
    let ws = tmp.path();
    for id in ["op", "publisher", "target", "guardian"] {
        im(ws).args(["join", id]).assert().success();
    }
    seed_tier(ws, "op", "manage");
    seed_tier(ws, "guardian", "manage"); // manage-tier target, console-only
    seed_tier(ws, "publisher", "publish");

    // Manage operator grants publish; visible in im agents; notice delivered.
    im(ws)
        .args(["grant", "op", "target"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Granted publish tier to target."));
    let roster = String::from_utf8(
        im(ws).arg("agents").assert().success().get_output().stdout.clone(),
    )
    .unwrap();
    assert!(roster.contains("target"), "roster: {roster}");
    assert!(roster.matches("[publish]").count() >= 1, "roster: {roster}");
    im(ws)
        .args(["receive", "target"])
        .assert()
        .success()
        .stdout(predicate::str::contains("granted publish-tier permission"));

    // A publish-tier operator is refused.
    im(ws)
        .args(["grant", "publisher", "target"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("below manage tier"));

    // A manage-tier target is refused with the console hint.
    im(ws)
        .args(["grant", "op", "guardian"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("console"));
    im(ws)
        .args(["revoke", "op", "guardian"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("console"));

    // The single-arg form is a usage error (the bare grant retired).
    im(ws)
        .args(["grant", "target"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Usage: im grant <operator> <target>"));

    // member delete: non-manage target goes (row + session file).
    im(ws)
        .args(["member", "delete", "op", "target"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Deleted member target."));
    {
        let db = rusqlite::Connection::open(ws.join(".im").join("im.db")).unwrap();
        let rows: i64 = db
            .query_row("SELECT COUNT(*) FROM agents WHERE id = 'target'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0);
    }
    assert!(!ws.join(".im").join("sessions").join("target").exists());

    // Manage-tier target refused; on-duty member refused.
    im(ws)
        .args(["member", "delete", "op", "guardian"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("console"));
    im(ws)
        .args(["work", "create", "op", "lab", "--executor", "publisher"])
        .assert()
        .success();
    im(ws)
        .args(["member", "delete", "op", "publisher"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("on duty"));
}

#[test]
fn mission_end_fans_out_to_past_participants() {
    let tmp = setup_workspace();
    let ws = tmp.path();
    for id in ["boss", "worker", "inspector"] {
        im(ws).args(["join", id]).assert().success();
    }
    seed_tier(ws, "boss", "manage");
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
fn init_seeds_pipeline_stations_and_reseeds_deleted_ones() {
    let tmp = setup_workspace();
    let ws = tmp.path();

    // Four stations out of the box, plus the template.
    let list = String::from_utf8(
        im(ws).args(["work", "list"]).assert().success().get_output().stdout.clone(),
    )
    .unwrap();
    for key in ["design", "plan", "build", "review"] {
        assert!(list.contains(key), "seeded station {key} missing: {list}");
    }
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
    let relist = String::from_utf8(
        im(ws).args(["work", "list"]).assert().success().get_output().stdout.clone(),
    )
    .unwrap();
    assert!(relist.contains("design"));

    // A deleted pipeline station is re-seeded on the next init.
    im(ws).args(["join", "boss"]).assert().success();
    seed_tier(ws, "boss", "manage");
    im(ws).args(["work", "delete", "boss", "design"]).assert().success();
    im(ws).arg("init").assert().success();
    let reseeded = String::from_utf8(
        im(ws).args(["work", "list"]).assert().success().get_output().stdout.clone(),
    )
    .unwrap();
    assert!(reseeded.contains("design"), "deleted pipeline key should have been re-seeded: {reseeded}");
}

#[test]
fn work_presets_fill_charters_and_the_station_lock_governs_delete() {
    let tmp = setup_workspace();
    let ws = tmp.path();
    im(ws).args(["join", "boss"]).assert().success();
    seed_tier(ws, "boss", "manage");

    // Unknown preset fails closed with the available list.
    im(ws)
        .args(["work", "create", "boss", "qa", "--preset", "nope"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown preset"));

    // --preset fills the charter (prompt + one-line summary); explicit flags win.
    im(ws).args(["work", "create", "boss", "qa", "--preset", "review"]).assert().success();
    im(ws)
        .args(["work", "create", "boss", "hotfix", "--preset", "build", "--prompt", "just ship it", "--description", "hotfix lane"])
        .assert()
        .success();
    let db = rusqlite::Connection::open(ws.join(".im").join("im.db")).unwrap();
    let qa_prompt: String = db
        .query_row("SELECT prompt FROM works WHERE work_key = 'qa'", [], |r| r.get(0))
        .unwrap();
    let qa_summary: String = db
        .query_row("SELECT description FROM works WHERE work_key = 'qa'", [], |r| r.get(0))
        .unwrap();
    let hotfix_prompt: String = db
        .query_row("SELECT prompt FROM works WHERE work_key = 'hotfix'", [], |r| r.get(0))
        .unwrap();
    let hotfix_summary: String = db
        .query_row("SELECT description FROM works WHERE work_key = 'hotfix'", [], |r| r.get(0))
        .unwrap();
    assert!(qa_prompt.contains("verify the implementation"), "qa: {qa_prompt}");
    assert!(qa_summary.contains("two evidence axes"), "qa summary: {qa_summary}");
    assert_eq!(hotfix_prompt, "just ship it");
    assert_eq!(hotfix_summary, "hotfix lane");

    // set-description edits the summary in place; set-prompt --preset applies
    // the whole charter (prompt + summary).
    im(ws)
        .args(["work", "set-description", "boss", "qa", "manual summary"])
        .assert()
        .success();
    im(ws)
        .args(["work", "set-prompt", "boss", "qa", "--preset", "design"])
        .assert()
        .success()
        .stdout(predicate::str::contains("charter updated from preset 'design'"));
    let refreshed: (String, String) = db
        .query_row(
            "SELECT prompt, description FROM works WHERE work_key = 'qa'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(refreshed.0.contains("final gate"), "design charter: {}", refreshed.0);
    assert!(refreshed.1.contains("final gate"), "design summary: {}", refreshed.1);

    // Station lock (PS semantics): an active mission referencing qa — even
    // parked elsewhere on a return edge — blocks deletion.
    std::fs::write(
        ws.join(".im").join("templates").join("t.yaml"),
        "schemaVersion: 4\nname: t\nentry: qa\nworks:\n  qa:\n    completion: {outcomes: [done], terminal: [done], feedbackRequiredOn: []}\n    documentRights: {read: [], write: []}\n",
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
    im(ws)
        .args(["work", "delete", "boss", "qa"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("locked by active missions"))
        .stderr(predicate::str::contains(&ms[..12]));

    // End the mission → the lock lifts → delete succeeds and frees the key.
    im(ws)
        .args(["mission", "abandon", "boss", &ms, "--revision", "1", "--reason", "done testing"])
        .assert()
        .success();
    im(ws).args(["work", "delete", "boss", "qa"]).assert().success();
    im(ws).args(["work", "create", "boss", "qa", "--preset", "review"]).assert().success();
}

#[test]
fn deleting_an_unlocked_station_frees_its_executor() {
    let tmp = setup_workspace();
    let ws = tmp.path();
    im(ws).args(["join", "boss"]).assert().success();
    im(ws).args(["join", "temp"]).assert().success();
    seed_tier(ws, "boss", "manage");
    im(ws)
        .args(["work", "create", "boss", "lab", "--executor", "temp"])
        .assert()
        .success();

    // Hard delete of an unlocked station: the row goes, so the member's
    // duty guard can never dangle off it.
    im(ws).args(["work", "delete", "boss", "lab"]).assert().success();
    let store = im::store::Store::open(&ws.join(".im").join("im.db")).unwrap();
    assert!(store.get_work("lab").is_err(), "station row must be gone");
    store.delete_agent("workspace", "temp").unwrap();
}

#[test]
fn work_list_shows_holding_and_en_route_occupancy() {
    let tmp = setup_workspace();
    let ws = tmp.path();
    im(ws).args(["join", "boss"]).assert().success();
    im(ws).args(["join", "worker"]).assert().success();
    seed_tier(ws, "boss", "manage");
    im(ws).args(["work", "create", "boss", "alpha", "--executor", "worker"]).assert().success();
    im(ws).args(["work", "create", "boss", "beta", "--executor", "worker"]).assert().success();
    std::fs::write(
        ws.join(".im").join("templates").join("t.yaml"),
        "schemaVersion: 4\nname: t\nentry: alpha\nworks:\n  alpha:\n    completion: {outcomes: [done], terminal: [], feedbackRequiredOn: []}\n    documentRights: {read: [], write: []}\n  beta:\n    completion: {outcomes: [ok], terminal: [ok], feedbackRequiredOn: []}\n    documentRights: {read: [], write: []}\npaths:\n  - {from: alpha, when: done, to: beta}\n",
    )
    .unwrap();
    im(ws)
        .args(["mission", "create", "boss", "--template", "t", "--key", "k1"])
        .assert()
        .success();

    // The mission parks at alpha (holding 1); beta is referenced by the
    // contract's path but not parked there (en route 1).
    let list = String::from_utf8(
        im(ws).args(["work", "list"]).assert().success().get_output().stdout.clone(),
    )
    .unwrap();
    assert!(
        list.contains("alpha — executor: worker, holding: 1, en route: 0"),
        "alpha occupancy: {list}"
    );
    assert!(
        list.contains("beta — executor: worker, holding: 0, en route: 1"),
        "beta occupancy: {list}"
    );

    // Ending the mission clears both counts.
    let ms = {
        let db = rusqlite::Connection::open(ws.join(".im").join("im.db")).unwrap();
        db.query_row("SELECT mission_id FROM missions", [], |r| r.get::<_, String>(0))
            .unwrap()
    };
    im(ws)
        .args(["mission", "submit", "worker", &ms, "--revision", "1", "--outcome", "done", "--reason", "over to beta"])
        .assert()
        .success();
    im(ws)
        .args(["mission", "submit", "worker", &ms, "--revision", "2", "--outcome", "ok"])
        .assert()
        .success();
    let after = String::from_utf8(
        im(ws).args(["work", "list"]).assert().success().get_output().stdout.clone(),
    )
    .unwrap();
    assert!(after.contains("beta — executor: worker, holding: 0, en route: 0"), "after: {after}");
}

#[test]
fn deleting_a_station_clears_its_arrival_notes() {
    let tmp = setup_workspace();
    let ws = tmp.path();
    im(ws).args(["join", "boss"]).assert().success();
    seed_tier(ws, "boss", "manage");
    im(ws).args(["work", "create", "boss", "lab"]).assert().success();
    {
        let db = rusqlite::Connection::open(ws.join(".im").join("im.db")).unwrap();
        db.execute(
            "INSERT INTO work_notes (work_key, kind, mission_id, content, created_at, read)
             VALUES ('lab', 'mission_arrived', 'ms_x', '[ms_x] arrived', 1000, 0)",
            [],
        )
        .unwrap();
    }
    im(ws).args(["work", "delete", "boss", "lab"]).assert().success();
    let db = rusqlite::Connection::open(ws.join(".im").join("im.db")).unwrap();
    let notes: i64 = db
        .query_row("SELECT COUNT(*) FROM work_notes WHERE work_key = 'lab'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(notes, 0, "stale arrival notes must not outlive the station");
}

#[test]
fn leave_releases_held_stations_to_the_user() {
    let tmp = setup_workspace();
    let ws = tmp.path();
    im(ws).args(["join", "boss"]).assert().success();
    seed_tier(ws, "boss", "manage");
    im(ws).args(["join", "temp"]).assert().success();
    im(ws).args(["join", "free"]).assert().success();
    im(ws)
        .args(["work", "create", "boss", "lab", "--executor", "temp"])
        .assert()
        .success();

    // Leaving with no stations stays silent; leaving with one releases it to the user.
    im(ws)
        .args(["leave", "free"])
        .assert()
        .success()
        .stdout(predicate::str::contains("released").not());
    im(ws)
        .args(["leave", "temp"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stations released to the user: lab"));

    let db = ws.join(".im").join("im.db");
    let store = im::store::Store::open(&db).unwrap();
    assert!(store.get_work("lab").unwrap().executor.is_none());
    drop(store);

    // Reactivation does not reclaim the station — the release is one-way;
    // reassignment is a deliberate manager act.
    im(ws).args(["join", "temp"]).assert().success();
    let store = im::store::Store::open(&db).unwrap();
    assert!(store.get_work("lab").unwrap().executor.is_none());
}

#[test]
fn leave_unlocks_member_deletion_and_missions_stay_put() {
    let tmp = setup_workspace();
    let ws = tmp.path();
    im(ws).args(["join", "boss"]).assert().success();
    seed_tier(ws, "boss", "manage");
    im(ws).args(["join", "worker"]).assert().success();
    im(ws)
        .args(["work", "create", "boss", "alpha", "--executor", "worker"])
        .assert()
        .success();
    std::fs::write(
        ws.join(".im").join("templates").join("t.yaml"),
        "schemaVersion: 4\nname: t\nentry: alpha\nworks:\n  alpha:\n    completion: {outcomes: [done], terminal: [], feedbackRequiredOn: []}\n    documentRights: {read: [], write: []}\npaths: []\n",
    )
    .unwrap();
    im(ws)
        .args(["mission", "create", "boss", "--template", "t", "--key", "k1"])
        .assert()
        .success();

    // The executor leaves: the mission stays parked at the station, which
    // now belongs to the user — and the member row becomes deletable.
    im(ws).args(["leave", "worker"]).assert().success();
    let list = String::from_utf8(
        im(ws).args(["work", "list"]).assert().success().get_output().stdout.clone(),
    )
    .unwrap();
    assert!(
        list.contains("alpha — executor: (user), holding: 1, en route: 0"),
        "station keeps the mission while returning to the user: {list}"
    );

    let store = im::store::Store::open(&ws.join(".im").join("im.db")).unwrap();
    store.delete_agent("workspace", "worker").unwrap();
    let archived: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM agents WHERE id = 'worker'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(archived, 0, "leave released the binding, so delete must pass");
}
