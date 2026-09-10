//! Work-origin + Ask/Answer contract tests. Independent target; only this
//! file is owned by the tester (dispatch b3fe2be9, baseline f4382ae).
//!
//! CLI spellings frozen for these tests (assumption — if the implementation
//! lands with different spellings, update THIS header and the args, never the
//! invariants):
//!   im mission create <op> --from <origin> --to <target> --key <key>
//!       --objective <question>   (template-less ask form; no template file)
//!   im mission submit <agent> <ms> --revision <N> --outcome answered
//!       --result <answer...>
//!   im mission submit <agent> <ms> --revision <N> --outcome declined
//!       --reason <text>
//!   (asks are handled exactly like any other mission — the receiver reads
//!       the vocabulary and revision from `im mission show`)
//!   im mission cancel <agent> <ms> --revision <N> [--reason <text>]
//!   im results <agent>          (result read plane, duty-station scoped)
//!   im mission result <ms>      (durable result for one ask)
//!   im mission create <op> --from <origin> --template <t> --key <k>
//!
//! At baseline f4382ae none of these subcommands exist, so every test is RED
//! by design. Each asserts real semantics (exactly-once results, CAS races,
//! station locks, partitioned idempotency keys, single-node contracts), so a
//! stub or a wrong implementation cannot turn them green by accident.
//!
//! Semantic choices the model text left open; tests pin the assumption and go
//! red if the implementation chooses otherwise:
//!   A1 duty is checked when the ask is created; a publisher leaving later
//!      does not cascade-cancel pending asks (publisher_leave_keeps_result…)
//!   A2 a manage-tier member counts as on-duty at a user station
//!      (user_work_on_both_sides_of_an_ask)
//!   A3 self-ask (origin == target, asker on duty) is legal (self_ask_round_trip)
//!   A4 the answer payload tolerates > 2000 chars — riding on submit's
//!      1..=2000 --reason field would fail this (answer_rejects_empty…)

use assert_cmd::Command;
use predicates::prelude::*;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use tempfile::TempDir;

fn im(workspace: &Path) -> Command {
    let mut cmd = Command::cargo_bin("im").unwrap();
    cmd.current_dir(workspace);
    // Isolated HOME: never touch the real ~/.im/workspaces.json.
    cmd.env("HOME", test_home());
    cmd
}

fn test_home() -> std::path::PathBuf {
    static HOME: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        let home = std::env::temp_dir().join(format!("im-ask-test-home-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        home
    })
    .clone()
}

/// boss=manage, alice/carol=publish, bob/pat=execute.
/// Stations: origin(alice) desk(carol) lab(bob) helpdesk(user station).
/// `init` also seeds the unbound design/plan/build/review stations; the keys
/// used here deliberately avoid those.
fn setup() -> TempDir {
    let tmp = TempDir::new().unwrap();
    im(tmp.path()).arg("init").assert().success();
    for id in ["boss", "alice", "carol", "bob", "pat"] {
        im(tmp.path()).args(["join", id]).assert().success();
    }
    seed_tier(tmp.path(), "boss", "manage");
    seed_tier(tmp.path(), "alice", "publish");
    seed_tier(tmp.path(), "carol", "publish");
    im(tmp.path())
        .args(["work", "create", "boss", "origin", "--executor", "alice"])
        .assert()
        .success();
    im(tmp.path())
        .args(["work", "create", "boss", "desk", "--executor", "carol"])
        .assert()
        .success();
    im(tmp.path())
        .args(["work", "create", "boss", "lab", "--executor", "bob"])
        .assert()
        .success();
    im(tmp.path())
        .args(["work", "create", "boss", "helpdesk"])
        .assert()
        .success();
    std::fs::write(
        tmp.path().join(".im").join("templates").join("review.yaml"),
        REVIEW_TEMPLATE,
    )
    .unwrap();
    tmp
}

fn seed_tier(ws: &Path, id: &str, tier: &str) {
    let db = Connection::open(ws.join(".im").join("im.db")).unwrap();
    db.execute(
        "UPDATE agents SET tier = ?2 WHERE id = ?1",
        rusqlite::params![id, tier],
    )
    .unwrap();
}

fn ask(ws: &Path, agent: &str, from: &str, to: &str, key: &str, question: &str) -> Command {
    let mut cmd = im(ws);
    cmd.args([
        "mission",
        "create",
        agent,
        "--from",
        from,
        "--to",
        to,
        "--key",
        key,
        "--objective",
        question,
    ]);
    cmd
}

fn sql_scalar(ws: &Path, sql: &str, param: &str) -> String {
    let db = Connection::open(ws.join(".im").join("im.db")).unwrap();
    db.query_row(sql, [param], |row| row.get::<_, String>(0))
        .unwrap()
}

fn mission_ids(ws: &Path) -> Vec<String> {
    let db = Connection::open(ws.join(".im").join("im.db")).unwrap();
    let mut stmt = db
        .prepare("SELECT mission_id FROM missions ORDER BY mission_id")
        .unwrap();
    let ids = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    ids
}

fn note_count(ws: &Path, work_key: &str) -> i64 {
    let db = Connection::open(ws.join(".im").join("im.db")).unwrap();
    db.query_row(
        "SELECT COUNT(*) FROM work_notes WHERE work_key = ?1",
        [work_key],
        |row| row.get(0),
    )
    .unwrap()
}

fn contract_of(ws: &Path, ms: &str) -> serde_json::Value {
    let json = sql_scalar(
        ws,
        "SELECT contract_json FROM missions WHERE mission_id = ?1",
        ms,
    );
    serde_json::from_str(&json).unwrap()
}

fn mission_at(ws: &Path, ms: &str) -> Option<String> {
    let db = Connection::open(ws.join(".im").join("im.db")).unwrap();
    db.query_row(
        "SELECT at FROM missions WHERE mission_id = ?1",
        [ms],
        |row| row.get::<_, Option<String>>(0),
    )
    .unwrap()
}

fn create_ask(ws: &Path, key: &str) -> String {
    ask(ws, "alice", "origin", "lab", key, "What ships first?")
        .assert()
        .success();
    mission_ids(ws)
        .into_iter()
        .find(|id| id.starts_with("ms_"))
        .unwrap()
}

/// The single mission-contract fixture shared with the workflow regression
/// tests (same shape the existing mission tests install by hand).
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

// --- Creation permission ---------------------------------------------------

#[test]
fn ask_requires_publish_tier_even_on_duty() {
    let tmp = setup();
    let ws = tmp.path();
    // bob holds `lab` but is execute-tier: duty alone is not permission.
    ask(ws, "bob", "lab", "origin", "q-tier", "ping?")
        .assert()
        .failure()
        .stderr(predicate::str::contains("tier"));
}

#[test]
fn ask_requires_duty_at_the_origin_work() {
    let tmp = setup();
    let ws = tmp.path();
    // alice is publish-tier but does not hold `desk`.
    ask(ws, "alice", "desk", "lab", "q-duty", "ping?")
        .assert()
        .failure()
        .stderr(predicate::str::contains("origin"));
    // No ask may exist afterwards.
    assert_eq!(mission_ids(ws).len(), 0);
}

#[test]
fn mission_create_with_from_requires_duty_at_origin() {
    let tmp = setup();
    let ws = tmp.path();
    // Install the template's stations first, so the only possible failure
    // reason is the origin duty check (baseline without --from support
    // instead SUCCEEDS here — which this red-by-design test rejects).
    im(ws)
        .args(["work", "create", "boss", "make", "--executor", "bob"])
        .assert()
        .success();
    im(ws)
        .args(["work", "create", "boss", "audit", "--executor", "pat"])
        .assert()
        .success();
    im(ws)
        .args(["work", "create", "boss", "approval"])
        .assert()
        .success();
    im(ws)
        .args([
            "mission",
            "create",
            "alice",
            "--from",
            "desk",
            "--template",
            "review",
            "--key",
            "m1",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("origin"));
    assert_eq!(mission_ids(ws).len(), 0);
}

// --- Ask shape: single-node mission, arrival, idempotency -------------------

#[test]
fn ask_is_a_single_node_mission_parked_at_target() {
    let tmp = setup();
    let ws = tmp.path();
    let ms = create_ask(ws, "q1");

    // Single node: target only. The origin is a column, never a contract node.
    let contract = contract_of(ws, &ms);
    let works = contract["works"].as_object().unwrap();
    assert_eq!(works.len(), 1, "ask contract must have exactly one work");
    assert!(works.contains_key("lab"));
    assert_eq!(contract["entry"], "lab");
    assert_eq!(contract["paths"].as_array().unwrap().len(), 0);
    assert_eq!(mission_at(ws, &ms).as_deref(), Some("lab"));

    // Exactly one arrival note at the target station.
    assert_eq!(note_count(ws, "lab"), 1);
    assert_eq!(note_count(ws, "origin"), 0);
}

#[test]
fn ask_is_idempotent_by_key_and_origin() {
    let tmp = setup();
    let ws = tmp.path();
    let ms1 = create_ask(ws, "q1");
    // Retry with the same key + origin: same ask, no duplicate delivery.
    ask(ws, "alice", "origin", "lab", "q1", "What ships first?")
        .assert()
        .success();
    assert_eq!(mission_ids(ws).len(), 1);
    assert_eq!(mission_ids(ws)[0], ms1);
    assert_eq!(note_count(ws, "lab"), 1);
}

#[test]
fn ask_queue_is_bounded_without_breaking_idempotent_retries() {
    let tmp = setup();
    let ws = tmp.path();
    for index in 0..8 {
        ask(
            ws,
            "alice",
            "origin",
            "lab",
            &format!("bounded-{index}"),
            "queued question",
        )
        .assert()
        .success();
    }
    ask(ws, "alice", "origin", "lab", "bounded-8", "one too many")
        .assert()
        .failure()
        .stderr(predicate::str::contains("queue limit"));
    ask(ws, "alice", "origin", "lab", "bounded-0", "queued question")
        .assert()
        .success();
    assert_eq!(mission_ids(ws).len(), 8);
}

#[test]
fn same_key_from_different_origins_yields_distinct_asks() {
    let tmp = setup();
    let ws = tmp.path();
    ask(ws, "alice", "origin", "lab", "shared", "alice's question")
        .assert()
        .success();
    ask(ws, "carol", "desk", "lab", "shared", "carol's question")
        .assert()
        .success();
    // The idempotency key is namespaced by origin: two asks, not one.
    let ids = mission_ids(ws);
    assert_eq!(
        ids.len(),
        2,
        "same key from different origins must not collide"
    );
    assert_ne!(ids[0], ids[1]);
    assert_eq!(note_count(ws, "lab"), 2);
}

#[test]
fn ask_keys_and_mission_keys_live_in_partitioned_spaces() {
    let tmp = setup();
    let ws = tmp.path();
    im(ws)
        .args(["work", "create", "boss", "make", "--executor", "bob"])
        .assert()
        .success();
    im(ws)
        .args(["work", "create", "boss", "audit", "--executor", "pat"])
        .assert()
        .success();
    im(ws)
        .args(["work", "create", "boss", "approval"])
        .assert()
        .success();
    im(ws)
        .args([
            "mission",
            "create",
            "boss",
            "--from",
            "helpdesk",
            "--template",
            "review",
            "--key",
            "shared",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Created mission ms_"));
    let mission_id = mission_ids(ws).into_iter().next().unwrap();

    ask(
        ws,
        "alice",
        "origin",
        "lab",
        "shared",
        "does not reuse the mission key",
    )
    .assert()
    .success();

    let ids = mission_ids(ws);
    assert_eq!(
        ids.len(),
        2,
        "an ask key must not resolve to an unrelated mission"
    );
    assert!(ids.contains(&mission_id));
}

// --- Answer: atomic end + exactly-once durable result -----------------------

#[test]
fn answer_ends_mission_and_returns_result_exactly_once() {
    let tmp = setup();
    let ws = tmp.path();
    let ms = create_ask(ws, "q1");

    // A losing answer (stale revision) must leave NO trace: no note, no end.
    im(ws)
        .args([
            "mission",
            "submit",
            "bob",
            &ms,
            "--revision",
            "99",
            "--outcome",
            "answered",
            "--result",
            "too late",
        ])
        .assert()
        .failure();
    assert_eq!(note_count(ws, "origin"), 0);

    im(ws)
        .args([
            "mission",
            "submit",
            "bob",
            &ms,
            "--revision",
            "1",
            "--outcome",
            "answered",
            "--result",
            "the widget ships first",
        ])
        .assert()
        .success();

    // A second answer against the ended mission must fail the same way.
    im(ws)
        .args([
            "mission",
            "submit",
            "bob",
            &ms,
            "--revision",
            "1",
            "--outcome",
            "answered",
            "--result",
            "again",
        ])
        .assert()
        .failure();

    // Ended exactly once.
    let events = im(ws).args(["mission", "events", &ms]).output().unwrap();
    let events = String::from_utf8(events.stdout).unwrap();
    assert_eq!(events.matches("mission.ended").count(), 1);

    // Result note at the origin work, consumable exactly once.
    assert_eq!(note_count(ws, "origin"), 1);
    im(ws)
        .args(["receive", "alice"])
        .assert()
        .success()
        .stdout(predicate::str::contains(&ms));
    im(ws)
        .args(["receive", "alice"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No new messages"));

    // The result is durable after the note is consumed and after the mission
    // ended — the asker must not depend on the ephemeral note.
    im(ws)
        .args(["mission", "result", &ms])
        .assert()
        .success()
        .stdout(predicate::str::contains("the widget ships first"));
    im(ws)
        .args(["results", "alice"])
        .assert()
        .success()
        .stdout(predicate::str::contains(&ms));
}

#[test]
fn decline_ends_ask_and_returns_outcome_to_origin() {
    let tmp = setup();
    let ws = tmp.path();
    let ms = create_ask(ws, "q1");
    im(ws)
        .args([
            "mission",
            "submit",
            "bob",
            &ms,
            "--revision",
            "1",
            "--outcome",
            "declined",
            "--reason",
            "out of scope",
        ])
        .assert()
        .success();

    let events = im(ws).args(["mission", "events", &ms]).output().unwrap();
    let events = String::from_utf8(events.stdout).unwrap();
    assert_eq!(events.matches("mission.ended").count(), 1);

    // The origin hears about the refusal exactly once, durably.
    assert_eq!(note_count(ws, "origin"), 1);
    im(ws)
        .args(["mission", "result", &ms])
        .assert()
        .success()
        .stdout(predicate::str::contains("out of scope"));
}

#[test]
fn answer_rejects_empty_and_preserves_text_over_the_reason_cap() {
    let tmp = setup();
    let ws = tmp.path();
    let ms = create_ask(ws, "q1");

    im(ws)
        .args([
            "mission",
            "submit",
            "bob",
            &ms,
            "--revision",
            "1",
            "--outcome",
            "answered",
            "--result",
            "   ",
        ])
        .assert()
        .failure();

    // 2001 chars: one past submit's --reason cap (1..=2000). If the answer
    // rides on reason this fails — answers need their own bounded payload.
    let long = format!("{}END-OF-ANSWER", "x".repeat(2001));
    im(ws)
        .args([
            "mission",
            "submit",
            "bob",
            &ms,
            "--revision",
            "1",
            "--outcome",
            "answered",
            "--result",
            &long,
        ])
        .assert()
        .success();
    im(ws)
        .args(["mission", "result", &ms])
        .assert()
        .success()
        .stdout(predicate::str::contains("END-OF-ANSWER"));
}

// --- Rebind: executor is a switchable station attribute ---------------------

#[test]
fn target_rebind_hands_the_round_to_the_new_executor() {
    let tmp = setup();
    let ws = tmp.path();
    let ms = create_ask(ws, "q1");
    im(ws)
        .args(["work", "set-executor", "boss", "lab", "pat"])
        .assert()
        .success();

    // The stale executor cannot answer; attribution follows the current duty.
    im(ws)
        .args([
            "mission",
            "submit",
            "bob",
            &ms,
            "--revision",
            "1",
            "--outcome",
            "answered",
            "--result",
            "stale",
        ])
        .assert()
        .failure();
    im(ws)
        .args([
            "mission",
            "submit",
            "pat",
            &ms,
            "--revision",
            "1",
            "--outcome",
            "answered",
            "--result",
            "from the new executor",
        ])
        .assert()
        .success();

    im(ws)
        .args(["receive", "alice"])
        .assert()
        .success()
        .stdout(predicate::str::contains(&ms));
}

#[test]
fn origin_rebind_delivers_the_result_to_the_new_executor() {
    let tmp = setup();
    let ws = tmp.path();
    let ms = create_ask(ws, "q1");
    im(ws)
        .args(["work", "set-executor", "boss", "origin", "carol"])
        .assert()
        .success();

    im(ws)
        .args([
            "mission",
            "submit",
            "bob",
            &ms,
            "--revision",
            "1",
            "--outcome",
            "answered",
            "--result",
            "for whoever holds origin",
        ])
        .assert()
        .success();

    // The return is Work-level: the current duty holder consumes it.
    im(ws)
        .args(["receive", "carol"])
        .assert()
        .success()
        .stdout(predicate::str::contains(&ms));
    im(ws)
        .args(["receive", "alice"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No new messages"));
}

// --- Station locks -----------------------------------------------------------

#[test]
fn pending_ask_locks_both_origin_and_target_stations() {
    let tmp = setup();
    let ws = tmp.path();
    let ms = create_ask(ws, "q1");
    assert!(!ms.is_empty());

    // Target: a live contract node — locked (existing station-lock rule).
    im(ws)
        .args(["work", "delete", "boss", "lab"])
        .assert()
        .failure();
    // Origin: not a contract node, but a pending result return pins it too.
    im(ws)
        .args(["work", "delete", "boss", "origin"])
        .assert()
        .failure();

    // The lock is a pending-ask lock: both stations free again after the end.
    im(ws)
        .args([
            "mission",
            "submit",
            "bob",
            &ms,
            "--revision",
            "1",
            "--outcome",
            "answered",
            "--result",
            "done",
        ])
        .assert()
        .success();
    im(ws)
        .args(["work", "delete", "boss", "lab"])
        .assert()
        .success();
    im(ws)
        .args(["work", "delete", "boss", "origin"])
        .assert()
        .success();
}

// --- Races -------------------------------------------------------------------

#[test]
fn double_answer_race_admits_exactly_one_result() {
    let tmp = setup();
    let ws = tmp.path();
    let ms = create_ask(ws, "q1");

    let ws1: PathBuf = ws.to_path_buf();
    let ws2 = ws1.clone();
    let ms1 = ms.clone();
    let ms2 = ms.clone();
    let barrier = Arc::new(Barrier::new(2));
    let b2 = barrier.clone();
    let j1 = std::thread::spawn(move || {
        barrier.wait();
        im(&ws1)
            .args([
                "mission",
                "submit",
                "bob",
                &ms1,
                "--revision",
                "1",
                "--outcome",
                "answered",
                "--result",
                "first",
            ])
            .output()
            .unwrap()
            .status
            .success()
    });
    let j2 = std::thread::spawn(move || {
        b2.wait();
        im(&ws2)
            .args([
                "mission",
                "submit",
                "bob",
                &ms2,
                "--revision",
                "1",
                "--outcome",
                "answered",
                "--result",
                "second",
            ])
            .output()
            .unwrap()
            .status
            .success()
    });
    let wins = [j1.join().unwrap(), j2.join().unwrap()];
    assert_eq!(
        wins.iter().filter(|w| **w).count(),
        1,
        "exactly one answer may win"
    );

    let events = im(ws).args(["mission", "events", &ms]).output().unwrap();
    let events = String::from_utf8(events.stdout).unwrap();
    assert_eq!(events.matches("mission.ended").count(), 1);
    assert_eq!(
        note_count(ws, "origin"),
        1,
        "exactly one result, even under a race"
    );
}

#[test]
fn answer_and_cancel_race_admit_exactly_one_outcome() {
    let tmp = setup();
    let ws = tmp.path();
    let ms = create_ask(ws, "q1");

    let ws1: PathBuf = ws.to_path_buf();
    let ws2 = ws1.clone();
    let ms_a = ms.clone();
    let ms_c = ms.clone();
    let barrier = Arc::new(Barrier::new(2));
    let b2 = barrier.clone();
    let j1 = std::thread::spawn(move || {
        barrier.wait();
        im(&ws1)
            .args([
                "mission",
                "submit",
                "bob",
                &ms_a,
                "--revision",
                "1",
                "--outcome",
                "answered",
                "--result",
                "raced answer",
            ])
            .output()
            .unwrap()
            .status
            .success()
    });
    let j2 = std::thread::spawn(move || {
        b2.wait();
        im(&ws2)
            .args([
                "mission",
                "cancel",
                "alice",
                &ms_c,
                "--revision",
                "1",
                "--reason",
                "raced cancel",
            ])
            .output()
            .unwrap()
            .status
            .success()
    });
    let outcomes = [j1.join().unwrap(), j2.join().unwrap()];
    assert_eq!(
        outcomes.iter().filter(|w| **w).count(),
        1,
        "answer xor cancel"
    );

    // Whichever disposition won, the end is single and the origin sees
    // exactly one result whose content matches the winning disposition.
    let events = im(ws).args(["mission", "events", &ms]).output().unwrap();
    let events = String::from_utf8(events.stdout).unwrap();
    assert_eq!(events.matches("mission.ended").count(), 1);
    assert_eq!(note_count(ws, "origin"), 1);
    let result = im(ws).args(["mission", "result", &ms]).output().unwrap();
    let result = String::from_utf8(result.stdout).unwrap();
    let answered_won = outcomes[0];
    if answered_won {
        assert!(
            result.contains("raced answer"),
            "result must carry the answer"
        );
    } else {
        assert!(
            result.contains("raced cancel"),
            "result must carry the cancel reason"
        );
    }
}

#[test]
fn cancellation_is_a_durable_distinct_disposition() {
    let tmp = setup();
    let ws = tmp.path();
    let ms = create_ask(ws, "cancelled");
    im(ws)
        .args([
            "mission",
            "cancel",
            "alice",
            &ms,
            "--revision",
            "1",
            "--reason",
            "no longer needed",
        ])
        .assert()
        .success();
    assert_eq!(
        sql_scalar(
            ws,
            "SELECT ended_disposition FROM missions WHERE mission_id = ?1",
            &ms,
        ),
        "cancelled"
    );
    im(ws)
        .args(["mission", "result", &ms])
        .assert()
        .success()
        .stdout(predicate::str::contains("no longer needed"))
        .stdout(predicate::str::contains("cancelled"))
        // Only a completed mission carries a result — a cancelled one must
        // not pass mid-flight payloads off as the final answer.
        .stdout(predicate::str::contains("\"result\": null"));
    assert_eq!(note_count(ws, "origin"), 1);
}

#[test]
fn abandon_cannot_smuggle_an_oversized_result_past_validation() {
    let tmp = setup();
    let ws = tmp.path();
    let ms = create_ask(ws, "q1");
    let oversized = "x".repeat(20_000);
    // abandon short-circuits before the outcome vocabulary — the payload
    // bounds must be checked before that early return, same as answered.
    im(ws)
        .args([
            "mission",
            "submit",
            "bob",
            &ms,
            "--revision",
            "1",
            "--outcome",
            "abandon",
            "--result",
            &oversized,
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("16384"));
    assert_eq!(note_count(ws, "origin"), 0, "no trace of the losing submit");
    // The mission is untouched and still answerable.
    im(ws)
        .args([
            "mission",
            "submit",
            "bob",
            &ms,
            "--revision",
            "1",
            "--outcome",
            "answered",
            "--result",
            "still fine",
        ])
        .assert()
        .success();
}

// --- Leave / user work / self-ask --------------------------------------------

#[test]
fn publisher_leave_keeps_the_result_reachable_at_origin_work() {
    let tmp = setup();
    let ws = tmp.path();
    let ms = create_ask(ws, "q1");

    // A1: leaving does not cascade-cancel; the ask stays answerable.
    im(ws).args(["leave", "alice"]).assert().success();
    im(ws)
        .args([
            "mission",
            "submit",
            "bob",
            &ms,
            "--revision",
            "1",
            "--outcome",
            "answered",
            "--result",
            "answered after the asker left",
        ])
        .assert()
        .success();

    // The released origin work (executor NULL) can be rebound; the result
    // follows the WORK, so the new duty holder consumes it.
    im(ws)
        .args(["work", "set-executor", "boss", "origin", "carol"])
        .assert()
        .success();
    im(ws)
        .args(["receive", "carol"])
        .assert()
        .success()
        .stdout(predicate::str::contains(&ms));
    im(ws)
        .args(["mission", "result", &ms])
        .assert()
        .success()
        .stdout(predicate::str::contains("answered after the asker left"));
}

#[test]
fn user_work_on_both_sides_of_an_ask() {
    let tmp = setup();
    let ws = tmp.path();

    // Target side: an ask parked at a user station waits on the human plane.
    ask(
        ws,
        "alice",
        "origin",
        "helpdesk",
        "q-user",
        "please check the queue",
    )
    .assert()
    .success();
    let ms = mission_ids(ws).into_iter().next().unwrap();
    im(ws)
        .args(["inbox"])
        .assert()
        .success()
        .stdout(predicate::str::contains("please check the queue"));
    // A manage-tier member resolves it; execute-tier members may not.
    im(ws)
        .args([
            "mission",
            "submit",
            "bob",
            &ms,
            "--revision",
            "1",
            "--outcome",
            "answered",
            "--result",
            "nope",
        ])
        .assert()
        .failure();
    im(ws)
        .args([
            "mission",
            "submit",
            "boss",
            &ms,
            "--revision",
            "1",
            "--outcome",
            "answered",
            "--result",
            "queue checked",
        ])
        .assert()
        .success();
    im(ws)
        .args(["receive", "alice"])
        .assert()
        .success()
        .stdout(predicate::str::contains(&ms));

    // Origin side (A2): a manage-tier member counts as on-duty at a user
    // station, so boss can ask FROM helpdesk; execute-tier cannot.
    ask(
        ws,
        "boss",
        "helpdesk",
        "lab",
        "q-from-user",
        "ask on the human's behalf",
    )
    .assert()
    .success();
    ask(
        ws,
        "bob",
        "helpdesk",
        "lab",
        "q-from-user2",
        "execute tier is not duty here",
    )
    .assert()
    .failure();
}

#[test]
fn self_ask_round_trip() {
    let tmp = setup();
    let ws = tmp.path();
    // A3: origin == target with the asker on duty is legal.
    ask(ws, "carol", "desk", "desk", "q-self", "note to self")
        .assert()
        .success();
    let ms = mission_ids(ws).into_iter().next().unwrap();
    assert_eq!(mission_at(ws, &ms).as_deref(), Some("desk"));

    im(ws)
        .args([
            "mission",
            "submit",
            "carol",
            &ms,
            "--revision",
            "1",
            "--outcome",
            "answered",
            "--result",
            "self answered",
        ])
        .assert()
        .success();
    im(ws)
        .args(["mission", "result", &ms])
        .assert()
        .success()
        .stdout(predicate::str::contains("self answered"));
}

// --- Legacy compatibility and workflow regression ----------------------------

#[test]
fn legacy_workspace_without_origin_still_runs_old_missions() {
    let tmp = setup();
    let ws = tmp.path();

    // Workflow stations first, so the old template compiles.
    im(ws)
        .args(["work", "create", "boss", "make", "--executor", "bob"])
        .assert()
        .success();
    im(ws)
        .args(["work", "create", "boss", "audit", "--executor", "pat"])
        .assert()
        .success();
    im(ws)
        .args(["work", "create", "boss", "approval"])
        .assert()
        .success();

    // The ask exists before the old mission, so every store open below runs
    // through the origin-aware code path against this pre-origin database.
    let ms_ask = create_ask(ws, "q1");

    // Old-style mission (no --from): full lifecycle must be untouched. The
    // CLI refuses origin-less creation, so seed the row exactly the way the
    // pre-origin binary did — Store API, origin NULL.
    let store = im::store::Store::open(&ws.join(".im").join("im.db")).unwrap();
    let legacy_template = im::contract::parse_template(REVIEW_TEMPLATE).unwrap();
    store
        .create_mission(
            "boss",
            &im::mission::TemplateSource {
                template: &legacy_template,
                path: ".im/templates/review.yaml".into(),
                bytes: REVIEW_TEMPLATE.as_bytes(),
            },
            "old-k",
            None,
            None,
        )
        .unwrap();
    let ms_old = mission_ids(ws)
        .into_iter()
        .find(|id| *id != ms_ask)
        .unwrap();
    im(ws)
        .args([
            "mission",
            "submit",
            "bob",
            &ms_old,
            "--revision",
            "1",
            "--outcome",
            "done",
        ])
        .assert()
        .success();
    im(ws)
        .args([
            "mission",
            "submit",
            "pat",
            &ms_old,
            "--revision",
            "2",
            "--outcome",
            "pass",
        ])
        .assert()
        .success();
    im(ws)
        .args(["mission", "show", &ms_old])
        .assert()
        .success()
        .stdout(predicate::str::contains("— ended"));

    // The old mission's history is intact next to the new ask.
    let events = im(ws)
        .args(["mission", "events", &ms_old])
        .output()
        .unwrap();
    let events = String::from_utf8(events.stdout).unwrap();
    assert_eq!(events.matches("mission.ended").count(), 1);
}

#[test]
fn old_pipeline_flow_regression_alongside_asks() {
    let tmp = setup();
    let ws = tmp.path();

    // Workflow stations for the review-loop, bound as in the mission tests.
    im(ws)
        .args(["work", "create", "boss", "make", "--executor", "bob"])
        .assert()
        .success();
    im(ws)
        .args(["work", "create", "boss", "audit", "--executor", "pat"])
        .assert()
        .success();
    im(ws)
        .args(["work", "create", "boss", "approval"])
        .assert()
        .success();

    // A pending ask coexists with a live workflow mission.
    ask(
        ws,
        "alice",
        "origin",
        "lab",
        "q-pipe",
        "quick side question",
    )
    .assert()
    .success();
    im(ws)
        .args([
            "mission",
            "create",
            "boss",
            "--from",
            "helpdesk",
            "--template",
            "review",
            "--key",
            "flow",
        ])
        .assert()
        .success();
    let ms_flow = mission_ids(ws)
        .into_iter()
        .find(|id| mission_at(ws, id).as_deref() == Some("make"))
        .unwrap();

    im(ws)
        .args([
            "mission",
            "submit",
            "bob",
            &ms_flow,
            "--revision",
            "1",
            "--outcome",
            "done",
        ])
        .assert()
        .success();
    im(ws)
        .args([
            "mission",
            "submit",
            "pat",
            &ms_flow,
            "--revision",
            "2",
            "--outcome",
            "pass",
        ])
        .assert()
        .success();

    // The workflow ran to its terminal exactly as before asks existed...
    im(ws)
        .args(["mission", "show", &ms_flow])
        .assert()
        .success()
        .stdout(predicate::str::contains("— ended"));
    let events = im(ws)
        .args(["mission", "events", &ms_flow])
        .output()
        .unwrap();
    let events = String::from_utf8(events.stdout).unwrap();
    assert_eq!(events.matches("mission.ended").count(), 1);

    // ...and the ask still completes on its own terms.
    let ms_ask = mission_ids(ws)
        .into_iter()
        .find(|id| *id != ms_flow)
        .unwrap();
    im(ws)
        .args([
            "mission",
            "submit",
            "bob",
            &ms_ask,
            "--revision",
            "1",
            "--outcome",
            "answered",
            "--result",
            "side question answered",
        ])
        .assert()
        .success();
    im(ws)
        .args(["mission", "result", &ms_ask])
        .assert()
        .success()
        .stdout(predicate::str::contains("side question answered"));
}
