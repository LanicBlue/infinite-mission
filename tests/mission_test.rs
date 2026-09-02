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
entry: make
works:
  make:
    completion:
      outcomes: [done, need-rework]
      terminal: []
      feedbackRequiredOn: []
    documentRights:
      read: [spec]
      write: [impl]
  audit:
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
  - from: make
    when: done
    to: audit
  - from: make
    when: need-rework
    to: make
    iterationPolicy: increment
  - from: audit
    when: fail
    to: make
    iterationPolicy: increment
  - from: audit
    when: any
    to: approval
"#;

/// boss (manager), worker at make, inspector at audit. One mission.
fn setup() -> (Fixture, String) {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().to_path_buf();

    im(&workspace).arg("init").assert().success();
    for id in ["boss", "worker", "inspector"] {
        im(&workspace).args(["join", id]).assert().success();
    }
    seed_tier(&workspace, "boss", "manage");
    im(&workspace)
        .args(["work", "create", "boss", "make", "--executor", "worker"])
        .assert()
        .success();
    im(&workspace)
        .args(["work", "create", "boss", "audit", "--executor", "inspector"])
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

/// Seed a member tier directly (console path; the bare CLI grant retired).
fn seed_tier(ws: &Path, id: &str, tier: &str) {
    let db = rusqlite::Connection::open(ws.join(".im").join("im.db")).unwrap();
    db.execute(
        "UPDATE agents SET tier = ?2 WHERE id = ?1",
        rusqlite::params![id, tier],
    )
    .unwrap();
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
        .args(["mission", "submit", "inspector", &ms, "--revision", "2", "--outcome", "fail", "--feedback", "fix it", "--next-node", "make"])
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
            "SELECT key_hash FROM mission_documents WHERE work_key = 'make' ORDER BY written_at DESC LIMIT 1",
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
        .args(["work", "set-executor", "boss", "make", "worker-2"])
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
    seed_tier(&ws, "boss", "manage");
    // make has an executor; approve is a USER station (no executor).
    im(&ws).args(["work", "create", "boss", "make", "--executor", "worker"]).assert().success();
    im(&ws).args(["work", "create", "boss", "approve"]).assert().success();

    std::fs::write(
        ws.join(".im").join("templates").join("handoff.yaml"),
        r#"schemaVersion: 4
name: handoff
entry: make
works:
  make:
    completion: {outcomes: [done], terminal: [], feedbackRequiredOn: []}
    documentRights: {read: [], write: [spec]}
  approve:
    completion: {outcomes: [ok], terminal: [ok], feedbackRequiredOn: []}
    documentRights: {read: [spec], write: []}
documents:
  - {id: spec, kind: file, path: spec.md}
paths:
  - {from: make, when: done, to: approve}
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
        .args(["mission", "doc", "write", "worker", &ms, "--id", "spec", "--file", "-"])
        .write_stdin("handoff spec")
        .assert()
        .success();
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

    // Document reads at a user station are resolved by a manage-tier member (aligned
    // with run_view's on-duty projection and manager-resolved submits).
    im(&ws)
        .args(["mission", "doc", "read", "boss", &ms, "spec.md"])
        .assert()
        .success()
        .stdout(predicate::str::contains("handoff spec"));
    im(&ws)
        .args(["mission", "doc", "read", "worker", &ms, "spec.md"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("resolved by a manage-tier member"));

    // A non-manager may NOT resolve a user station.
    im(&ws)
        .args(["mission", "submit", "worker", &ms, "--revision", "2", "--outcome", "ok"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("resolved by a manage-tier member"));

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
        .args(["mission", "submit", "inspector", &ms, "--revision", "2", "--outcome", "fail", "--feedback", "redo", "--next-node", "make"])
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
    // The rework hop bumped the iteration at make to 2.
    assert!(events.contains("\"from\":\"audit\",\"iteration\":2,\"revision\":3,\"to\":\"make\""));

    // The run view reflects the derived iteration.
    im(&ws)
        .args(["mission", "show", &ms, "--for", "worker"])
        .assert()
        .success()
        .stdout(predicate::str::contains("iteration 2"));
}

// --- The delivery pipeline (design → plan → build → review → final gate) ---

#[test]
fn pipeline_template_compiles_and_holds_the_designed_routing() {
    let template = im::contract::parse_template(im::pipeline::PIPELINE_TEMPLATE).unwrap();
    let contract =
        im::contract::compile(&template, "pipeline.yaml", im::pipeline::PIPELINE_TEMPLATE.as_bytes())
            .unwrap();

    assert_eq!(contract.entry, "design");
    // The grill happens in the design session conversation, not in mission
    // rounds: design's vocabulary has no needs-input, no user-station loop.
    let design = &contract.works["design"];
    assert!(!contract.works.contains_key("owner"));
    assert_eq!(
        design.completion.outcomes,
        vec!["spec-ready".to_string(), "accept".to_string(), "reject".to_string()]
    );
    // accept is terminal at design and carries no out-edge.
    assert!(design.completion.terminal.contains(&"accept".to_string()));
    assert!(!contract.paths.iter().any(|e| e.from == "design" && e.when == "accept"));
    // The final gate: review approved routes back to design with increment.
    let approved = contract
        .paths
        .iter()
        .find(|e| e.from == "review" && e.when == "approved")
        .unwrap();
    assert_eq!(approved.to, "design");
    assert_eq!(approved.iteration_policy.as_deref(), Some("increment"));
    // Rights: build works from the goal only, never the spec.
    assert!(!contract.works["build"].document_rights.read.contains(&"spec".to_string()));
    // Every preset charter keeps the interpolation slots.
    for preset in im::pipeline::PRESETS {
        assert!(preset.prompt.contains("{mission.objective}"), "{}", preset.key);
        assert!(preset.prompt.contains("{mission.reason}"), "{}", preset.key);
    }
}

/// boss (manager, also resolves the owner user station), arch at design,
/// strategist at plan, coder at build, auditor at review.
fn setup_pipeline() -> (Fixture, String) {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().to_path_buf();
    im(&workspace).arg("init").assert().success();
    for id in ["boss", "arch", "strategist", "coder", "auditor"] {
        im(&workspace).args(["join", id]).assert().success();
    }
    seed_tier(&workspace, "boss", "manage");
    for (work, agent) in [("design", "arch"), ("plan", "strategist"), ("build", "coder"), ("review", "auditor")] {
        im(&workspace)
            .args(["work", "set-executor", "boss", work, agent])
            .assert()
            .success();
    }
    im(&workspace)
        .args(["mission", "create", "boss", "--template", "pipeline", "--key", "p1", "--objective", "ship the widget"])
        .assert()
        .success();
    let mission_id = first_mission_id(&workspace);
    (Fixture { _tmp: tmp, workspace }, mission_id)
}

#[test]
fn pipeline_full_chain_with_rework_ends_at_the_design_gate() {
    let (fixture, ms) = setup_pipeline();
    let ws = fixture.workspace;

    // The grill happened in design's own session; the mission starts with
    // design parking the frozen SPEC (receipt) and forwarding to plan.
    let spec_receipt = String::from_utf8(
        im(&ws)
            .args(["mission", "doc", "write", "arch", &ms, "--id", "spec", "--file", "-"])
            .write_stdin("# SPEC\nfixed")
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    im(&ws)
        .args(["mission", "submit", "arch", &ms, "--revision", "1", "--outcome", "spec-ready", "--receipts", spec_receipt.trim(), "--reason", "spec frozen from the session conversation"])
        .assert()
        .success()
        .stdout(predicate::str::contains("→ plan"));
    let goal_receipt = String::from_utf8(
        im(&ws)
            .args(["mission", "doc", "write", "strategist", &ms, "--id", "goal", "--file", "-"])
            .write_stdin("# GOAL\n- [ ] widget ships")
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    im(&ws)
        .args(["mission", "submit", "strategist", &ms, "--revision", "2", "--outcome", "goal-ready", "--receipts", goal_receipt.trim()])
        .assert()
        .success();
    let impl_receipt = String::from_utf8(
        im(&ws)
            .args(["mission", "doc", "write", "coder", &ms, "--id", "impl", "--file", "-"])
            .write_stdin("# RECEIPT\nbaseline: abc123\ncriterion 1: satisfied (test)")
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    im(&ws)
        .args(["mission", "submit", "coder", &ms, "--revision", "3", "--outcome", "done", "--receipts", impl_receipt.trim(), "--reason", "widget built"])
        .assert()
        .success();

    // review requires feedback on rework — the findings channel.
    im(&ws)
        .args(["mission", "submit", "auditor", &ms, "--revision", "4", "--outcome", "rework"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("requires non-empty feedback"));
    let review_receipt = String::from_utf8(
        im(&ws)
            .args(["mission", "doc", "write", "auditor", &ms, "--id", "review", "--file", "-"])
            .write_stdin("# REVIEW\nF1: no test for the widget")
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    im(&ws)
        .args(["mission", "submit", "auditor", &ms, "--revision", "4", "--outcome", "rework", "--feedback", "F1: no test for the widget", "--reason", "F1", "--receipts", review_receipt.trim()])
        .assert()
        .success()
        .stdout(predicate::str::contains("iteration 2"));

    // build reads the findings, fixes, resubmits; review re-verifies.
    im(&ws)
        .args(["mission", "doc", "read", "coder", &ms, "review.md"])
        .assert()
        .success()
        .stdout(predicate::str::contains("F1"));
    im(&ws)
        .args(["mission", "submit", "coder", &ms, "--revision", "5", "--outcome", "done", "--reason", "F1 fixed"])
        .assert()
        .success();
    im(&ws)
        .args(["mission", "submit", "auditor", &ms, "--revision", "6", "--outcome", "approved", "--reason", "criterion 1 satisfied with test"])
        .assert()
        .success()
        .stdout(predicate::str::contains("iteration 2"));

    // The final gate: design's prompt carries the approved reason.
    im(&ws)
        .args(["mission", "show", &ms, "--for", "arch"])
        .assert()
        .success()
        .stdout(predicate::str::contains("criterion 1 satisfied"));
    im(&ws)
        .args(["mission", "submit", "arch", &ms, "--revision", "7", "--outcome", "accept", "--reason", "delivery honors the spec"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ended"));

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
    assert!(events.contains("\"outcome\":\"accept\""));
    assert!(events.contains("\"disposition\":\"completed\""));
    assert!(events.contains("\"from\":\"review\",\"iteration\":2"));
}

#[test]
fn pipeline_return_edges_cover_spec_gap_blocked_and_reject() {
    let (fixture, ms) = setup_pipeline();
    let ws = fixture.workspace;

    // plan finds the spec uncompilable → spec-gap returns to design.
    im(&ws).args(["mission", "submit", "arch", &ms, "--revision", "1", "--outcome", "spec-ready", "--reason", "spec v0"]).assert().success();
    im(&ws)
        .args(["mission", "submit", "strategist", &ms, "--revision", "2", "--outcome", "spec-gap", "--feedback", "no decision on the data model", "--reason", "gap: data model"])
        .assert()
        .success()
        .stdout(predicate::str::contains("→ design"));
    // design revises → plan recompiles → build blocks on the goal → plan.
    im(&ws).args(["mission", "submit", "arch", &ms, "--revision", "3", "--outcome", "spec-ready", "--reason", "spec v1"]).assert().success();
    im(&ws).args(["mission", "submit", "strategist", &ms, "--revision", "4", "--outcome", "goal-ready", "--reason", "goal v1"]).assert().success();
    im(&ws)
        .args(["mission", "submit", "coder", &ms, "--revision", "5", "--outcome", "blocked", "--feedback", "goal step 2 contradicts step 1", "--reason", "blocked at step 2"])
        .assert()
        .success()
        .stdout(predicate::str::contains("→ plan"));
    // plan recompiles → build delivers → review finds the goal itself wrong.
    im(&ws).args(["mission", "submit", "strategist", &ms, "--revision", "6", "--outcome", "goal-ready", "--reason", "goal v2"]).assert().success();
    im(&ws).args(["mission", "submit", "coder", &ms, "--revision", "7", "--outcome", "done", "--reason", "built per goal v2"]).assert().success();
    im(&ws)
        .args(["mission", "submit", "auditor", &ms, "--revision", "8", "--outcome", "spec-gap", "--feedback", "criteria assume the old API", "--reason", "goal wrong: old API"])
        .assert()
        .success()
        .stdout(predicate::str::contains("→ design"));
    // The full close: re-freeze → re-compile → build → approved → the gate
    // rejects (implementation deviation) → build repairs → gate accepts.
    im(&ws).args(["mission", "submit", "arch", &ms, "--revision", "9", "--outcome", "spec-ready", "--reason", "spec v2"]).assert().success();
    im(&ws).args(["mission", "submit", "strategist", &ms, "--revision", "10", "--outcome", "goal-ready", "--reason", "goal v3"]).assert().success();
    im(&ws).args(["mission", "submit", "coder", &ms, "--revision", "11", "--outcome", "done", "--reason", "built per goal v3"]).assert().success();
    im(&ws).args(["mission", "submit", "auditor", &ms, "--revision", "12", "--outcome", "approved", "--reason", "criteria met"]).assert().success();
    im(&ws)
        .args(["mission", "submit", "arch", &ms, "--revision", "13", "--outcome", "reject", "--feedback", "1) deviates from spec v2 section 2", "--reason", "deviation at section 2"])
        .assert()
        .success()
        .stdout(predicate::str::contains("→ build"));
    im(&ws).args(["mission", "submit", "coder", &ms, "--revision", "14", "--outcome", "done", "--reason", "fixed section 2"]).assert().success();
    im(&ws).args(["mission", "submit", "auditor", &ms, "--revision", "15", "--outcome", "approved", "--reason", "all green"]).assert().success();
    im(&ws)
        .args(["mission", "submit", "arch", &ms, "--revision", "16", "--outcome", "accept", "--reason", "honors spec v2"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ended"));
}
