use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;
use tempfile::TempDir;

fn im(workspace: &Path) -> Command {
    let mut cmd = Command::cargo_bin("im").unwrap();
    cmd.current_dir(workspace);
    cmd
}

struct Fixture {
    _tmp: TempDir,
    workspace: std::path::PathBuf,
}

const REVIEW_TEMPLATE: &str = r#"schemaVersion: 4
name: review-loop
entry: build
works:
  build:
    completion:
      outcomes: [done, need-rework]
      terminal: []
      feedbackRequiredOn: []
    documentRights:
      read: [spec]
      write: [impl]
  review:
    completion:
      outcomes: [pass, fail]
      terminal: [pass]
      feedbackRequiredOn: [fail]
    documentRights:
      read: [impl]
      write: [notes]
  approval:
    completion:
      outcomes: [approved]
      terminal: [approved]
      feedbackRequiredOn: []
    documentRights: {read: [], write: []}
documents:
  - id: spec
    kind: file
    path: docs/spec.md
  - id: impl
    kind: file
    path: docs/impl.md
  - id: notes
    kind: file
    path: docs/notes.md
paths:
  - from: build
    when: done
    to: review
  - from: build
    when: need-rework
    to: build
    iterationPolicy: increment
  - from: review
    when: fail
    to: build
    iterationPolicy: increment
  - from: review
    when: any
    to: approval
"#;

/// boss (manager), worker at build, inspector at review. One mission.
fn setup() -> (Fixture, String) {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().to_path_buf();

    im(&workspace).arg("init").assert().success();
    for id in ["boss", "worker", "inspector"] {
        im(&workspace).args(["join", id]).assert().success();
    }
    im(&workspace).args(["grant", "boss"]).assert().success();
    im(&workspace)
        .args(["work", "create", "boss", "build", "--executor", "worker"])
        .assert()
        .success();
    im(&workspace)
        .args(["work", "create", "boss", "review", "--executor", "inspector"])
        .assert()
        .success();
    // approval is a user station (no executor) the review template routes into.
    im(&workspace)
        .args(["work", "create", "boss", "approval"])
        .assert()
        .success();
    std::fs::write(
        workspace.join(".im").join("templates").join("review.yaml"),
        REVIEW_TEMPLATE,
    )
    .unwrap();
    im(&workspace)
        .args(["mission", "create", "boss", "--template", "review", "--key", "v1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Created mission ms_"));

    let mission_id = first_mission_id(&workspace);
    (Fixture { _tmp: tmp, workspace }, mission_id)
}

fn first_mission_id(workspace: &Path) -> String {
    let db = rusqlite::Connection::open(workspace.join(".im").join("im.db")).unwrap();
    db.query_row("SELECT mission_id FROM missions", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn mission_create_is_idempotent_by_key() {
    let (fixture, mission_id) = setup();
    let ws = fixture.workspace;
    // Same key → same mission, no duplicate.
    im(&ws)
        .args(["mission", "create", "boss", "--template", "review", "--key", "v1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("already exists"));
    let db = rusqlite::Connection::open(ws.join(".im").join("im.db")).unwrap();
    let count: i64 = db.query_row("SELECT COUNT(*) FROM missions", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 1);
    let _ = mission_id;
}

#[test]
fn templates_are_validated_fail_closed() {
    let (fixture, _ms) = setup();
    let ws = fixture.workspace;
    let templates = ws.join(".im").join("templates");

    // Each template is structurally complete except for one targeted defect.
    let cases = [
        ("schemaVersion: 3\nentry: build\nworks:\n  build:\n    completion: {outcomes: [done], terminal: [], feedbackRequiredOn: []}\n", "unsupported template schemaVersion"),
        ("schemaVersion: 4\nentry: ghost\nworks:\n  build:\n    completion: {outcomes: [done], terminal: [], feedbackRequiredOn: []}\n", "has no discipline"),
        ("schemaVersion: 4\nentry: build\nworks:\n  build:\n    completion: {outcomes: [abandon], terminal: [], feedbackRequiredOn: []}\n", "reserved"),
        ("schemaVersion: 4\nentry: build\nworks:\n  build:\n    completion: {outcomes: [done], terminal: [done], feedbackRequiredOn: []}\npaths:\n  - {from: build, when: done, to: build}\n", "terminal outcome"),
        ("schemaVersion: 4\nentry: build\nworks:\n  build:\n    completion: {outcomes: [done], terminal: [], feedbackRequiredOn: []}\npaths:\n  - {from: build, when: bogus, to: build}\n", "not in the source vocabulary"),
        ("schemaVersion: 4\nentry: build\nworks:\n  build:\n    completion: {outcomes: [done], terminal: [], feedbackRequiredOn: []}\n    documentRights: {read: [ghost], write: []}\n", "undeclared document"),
        ("schemaVersion: 4\nentry: build\nworks:\n  build:\n    completion: {outcomes: [done], terminal: [], feedbackRequiredOn: []}\ndocuments:\n  - {id: x, kind: file, path: a/}\n", "empty or dot segments"),
    ];
    for (index, (body, expected)) in cases.iter().enumerate() {
        std::fs::write(templates.join(format!("bad{index}.yaml")), body).unwrap();
        im(&ws)
            .args(["mission", "create", "boss", "--project", "demo", "--template", &format!("bad{index}"), "--key", &format!("k{index}")])
            .assert()
            .failure()
            .stderr(predicate::str::contains(*expected));
    }
}

#[test]
fn unknown_station_references_are_rejected() {
    let (fixture, _ms) = setup();
    let ws = fixture.workspace;
    std::fs::write(
        ws.join(".im").join("templates").join("ghost.yaml"),
        "schemaVersion: 4\nentry: ghost\nworks:\n  ghost:\n    completion: {outcomes: [done], terminal: [], feedbackRequiredOn: []}\n",
    )
    .unwrap();
    im(&ws)
        .args(["mission", "create", "boss", "--project", "demo", "--template", "ghost", "--key", "g1"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown stations: ghost"));
}

#[test]
fn submit_adjudication_matrix() {
    let (fixture, ms) = setup();
    let ws = fixture.workspace;

    // CAS: stale revision is rejected with the current one surfaced.
    im(&ws)
        .args(["mission", "submit", "worker", &ms, "--revision", "9", "--outcome", "done"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Mission state was superseded (current revision: 1)"));

    // Attribution: an agent who is not on duty is rejected.
    im(&ws)
        .args(["mission", "submit", "inspector", &ms, "--revision", "1", "--outcome", "done"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Mission belongs to another executor"));

    // Vocabulary: outcome outside the station language is rejected with permitted list.
    im(&ws)
        .args(["mission", "submit", "worker", &ms, "--revision", "1", "--outcome", "pass"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("permitted: done, need-rework, abandon"));

    // Auto-follow: exactly one edge → no --next-node needed.
    im(&ws)
        .args(["mission", "submit", "worker", &ms, "--revision", "1", "--outcome", "done"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Routed"));

    // feedbackRequiredOn: fail without feedback is rejected.
    im(&ws)
        .args(["mission", "submit", "inspector", &ms, "--revision", "2", "--outcome", "fail"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("requires non-empty feedback"));

    // Ambiguous edges: review has fail→build AND any→approval for outcome
    // fail (exact-edge preferred but any also matches) → 2+ targets require
    // explicit choice... exact-edge preference: PS prefers exact over any.
    // fail has an exact edge → single candidate. Test the `any` fallback with
    // an outcome lacking an exact edge: pass is terminal so use approval side.
    // Instead verify explicit next-node rejection for a non-candidate.
    im(&ws)
        .args(["mission", "submit", "inspector", &ms, "--revision", "2", "--outcome", "fail", "--feedback", "fix it", "--next-node", "build"])
        .assert()
        .success()
        .stdout(predicate::str::contains("iteration 2"));

    // Receipts minted at another station are rejected.
    im(&ws)
        .args(["mission", "doc", "write", "worker", &ms, "--id", "impl", "--file", "-"])
        .write_stdin("v2 content")
        .assert()
        .success();
    let receipt = {
        let db = rusqlite::Connection::open(ws.join(".im").join("im.db")).unwrap();
        db.query_row(
            "SELECT key_hash FROM mission_documents WHERE work_key = 'build' ORDER BY written_at DESC LIMIT 1",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap()
    };
    im(&ws)
        .args(["mission", "submit", "worker", &ms, "--revision", "3", "--outcome", "done", "--receipts", &format!("document:{receipt}")])
        .assert()
        .success();

    // Inspector submits a bogus receipt → rejected (minted at build, not review).
    im(&ws)
        .args(["mission", "submit", "inspector", &ms, "--revision", "4", "--outcome", "fail", "--feedback", "again", "--receipts", &format!("document:{receipt}")])
        .assert()
        .failure()
        .stderr(predicate::str::contains("minted at another station"));

    // Terminal outcome ends the mission.
    im(&ws)
        .args(["mission", "submit", "inspector", &ms, "--revision", "4", "--outcome", "pass"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ended"));
    im(&ws)
        .args(["mission", "submit", "worker", &ms, "--revision", "5", "--outcome", "done"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Mission has already ended"));
}

#[test]
fn abandon_and_manager_delete_endings() {
    let (fixture, ms) = setup();
    let ws = fixture.workspace;

    // Abandon via the dedicated verb requires the current revision and is
    // always legal.
    im(&ws)
        .args(["mission", "abandon", "worker", &ms, "--revision", "1", "--reason", "blocked on spec"])
        .assert()
        .success()
        .stdout(predicate::str::contains("abandoned"));

    // Second mission: manager delete path.
    im(&ws)
        .args(["mission", "create", "boss", "--template", "review", "--key", "v2"])
        .assert()
        .success();
    let ms2 = {
        let db = rusqlite::Connection::open(ws.join(".im").join("im.db")).unwrap();
        db.query_row(
            "SELECT mission_id FROM missions WHERE mission_id != ?1",
            [&ms],
            |r| r.get::<_, String>(0),
        )
        .unwrap()
    };
    im(&ws)
        .args(["mission", "end", "boss", &ms2, "--reason", "obsolete"])
        .assert()
        .success();
    im(&ws)
        .args(["mission", "events", &ms2])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"disposition\":\"deleted\""));
}

#[test]
fn documents_are_content_addressed_and_right_scoped() {
    let (fixture, ms) = setup();
    let ws = fixture.workspace;

    // Same bytes → same receipt.
    let first = im(&ws)
        .args(["mission", "doc", "write", "worker", &ms, "--id", "impl", "--file", "-"])
        .write_stdin("hello")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let second = im(&ws)
        .args(["mission", "doc", "write", "worker", &ms, "--id", "impl", "--file", "-"])
        .write_stdin("hello")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(String::from_utf8_lossy(&first), String::from_utf8_lossy(&second));

    // Undeclared document id is rejected; the contract fixes the path.
    im(&ws)
        .args(["mission", "doc", "write", "worker", &ms, "--id", "ghost", "--file", "-"])
        .write_stdin("x")
        .assert()
        .failure()
        .stderr(predicate::str::contains("not declared by the mission contract"));

    // Write right ≠ read right: worker wrote impl but may not read it.
    im(&ws)
        .args(["mission", "doc", "read", "worker", &ms, "docs/impl.md"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("may not read document"));

    // Non-on-duty agent may not read: build holds read:spec, but inspector
    // is not build's executor (rights are station-scoped; asker must be on
    // duty at that station).
    im(&ws)
        .args(["mission", "doc", "read", "inspector", &ms, "docs/spec.md"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("on-duty executor"));

    // Mailbox moves on → the former executor loses the read. Review holds
    // read:impl, but worker is not review's executor anymore.
    im(&ws)
        .args(["mission", "submit", "worker", &ms, "--revision", "1", "--outcome", "done"])
        .assert()
        .success();
    im(&ws)
        .args(["mission", "doc", "read", "worker", &ms, "docs/impl.md"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("on-duty executor"));
    // Inspector (now on duty) may read impl.
    im(&ws)
        .args(["mission", "doc", "read", "inspector", &ms, "docs/impl.md"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello"));
    // After the mission ends, reads close for everyone.
    im(&ws)
        .args(["mission", "submit", "inspector", &ms, "--revision", "2", "--outcome", "pass"])
        .assert()
        .success();
    im(&ws)
        .args(["mission", "doc", "read", "inspector", &ms, "docs/impl.md"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("mailbox has moved on"));
}

#[test]
fn rebind_is_a_pointer_move_not_a_migration() {
    let (fixture, ms) = setup();
    let ws = fixture.workspace;

    // worker-2 joins; manager rebinds the build station.
    im(&ws).args(["join", "worker-2"]).assert().success();
    im(&ws)
        .args(["work", "set-executor", "boss", "build", "worker-2"])
        .assert()
        .success();

    // Old identity can no longer submit; new one can, same mission, same revision.
    im(&ws)
        .args(["mission", "submit", "worker", &ms, "--revision", "1", "--outcome", "done"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Mission belongs to another executor"));
    im(&ws)
        .args(["mission", "submit", "worker-2", &ms, "--revision", "1", "--outcome", "done"])
        .assert()
        .success();

    // The arrival note follows the binding: worker-2 holds the unread note.
    let inbox = String::from_utf8(
        im(&ws)
            .args(["receive", "worker-2"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    assert!(inbox.contains(&ms[..12]), "arrival note missing for worker-2: {inbox}");
    assert!(inbox.contains("station"), "note should name the station: {inbox}");
}

#[test]
fn user_stations_need_a_reason_and_surface_in_inbox() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().to_path_buf();
    im(&ws).arg("init").assert().success();
    im(&ws).args(["join", "boss"]).assert().success();
    im(&ws).args(["join", "worker"]).assert().success();
    im(&ws).args(["grant", "boss"]).assert().success();
    // build has an executor; approve is a USER station (no executor).
    im(&ws).args(["work", "create", "boss", "build", "--executor", "worker"]).assert().success();
    im(&ws).args(["work", "create", "boss", "approve"]).assert().success();

    std::fs::write(
        ws.join(".im").join("templates").join("handoff.yaml"),
        r#"schemaVersion: 4
name: handoff
entry: build
works:
  build:
    completion: {outcomes: [done], terminal: [], feedbackRequiredOn: []}
    documentRights: {read: [], write: []}
  approve:
    completion: {outcomes: [ok], terminal: [ok], feedbackRequiredOn: []}
    documentRights: {read: [], write: []}
paths:
  - {from: build, when: done, to: approve}
"#,
    )
    .unwrap();
    im(&ws)
        .args(["mission", "create", "boss", "--template", "handoff", "--key", "h1"])
        .assert()
        .success();
    let ms = first_mission_id(&ws);

    // Hop onto the user station without a reason → rejected.
    im(&ws)
        .args(["mission", "submit", "worker", &ms, "--revision", "1", "--outcome", "done"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("requires a non-empty --reason"));

    // With a reason it lands; the inbox shows it waiting for a human.
    im(&ws)
        .args(["mission", "submit", "worker", &ms, "--revision", "1", "--outcome", "done", "--reason", "needs your sign-off"])
        .assert()
        .success();
    im(&ws)
        .arg("inbox")
        .assert()
        .success()
        .stdout(predicate::str::contains(&ms[..12]))
        .stdout(predicate::str::contains("needs your sign-off"))
        .stdout(predicate::str::contains("resolve it"));

    // A non-manager may NOT resolve a user station.
    im(&ws)
        .args(["mission", "submit", "worker", &ms, "--revision", "2", "--outcome", "ok"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("resolved by a manager"));

    // The manager resolves it; the terminal outcome ends the mission, and
    // the round records the manager plane.
    im(&ws)
        .args(["mission", "submit", "boss", &ms, "--revision", "2", "--outcome", "ok"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ended"));
    im(&ws)
        .arg("inbox")
        .assert()
        .success()
        .stdout(predicate::str::contains("Inbox empty"));
    im(&ws)
        .args(["mission", "events", &ms])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"plane\":\"manager\""));
}

#[test]
fn events_are_the_history_and_iteration_derives() {
    let (fixture, ms) = setup();
    let ws = fixture.workspace;
    im(&ws)
        .args(["mission", "submit", "worker", &ms, "--revision", "1", "--outcome", "done"])
        .assert()
        .success();
    im(&ws)
        .args(["mission", "submit", "inspector", &ms, "--revision", "2", "--outcome", "fail", "--feedback", "redo", "--next-node", "build"])
        .assert()
        .success();
    let events = String::from_utf8(
        im(&ws)
            .args(["mission", "events", &ms])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    assert!(events.contains("mission.created"));
    assert!(events.contains("mission.round.completed"));
    assert!(events.contains("mission.routed"));
    // The rework hop bumped the iteration at build to 2.
    assert!(events.contains("\"from\":\"review\",\"iteration\":2,\"revision\":3,\"to\":\"build\""));

    // The run view reflects the derived iteration.
    im(&ws)
        .args(["mission", "show", &ms, "--for", "worker"])
        .assert()
        .success()
        .stdout(predicate::str::contains("iteration 2"));
}
