use im::store::Store;
use rusqlite::params;
use tempfile::TempDir;

fn open() -> (TempDir, Store) {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(&tmp.path().join("im.db")).unwrap();
    (tmp, store)
}

#[test]
fn schema_creates_all_domain_tables() {
    let (_tmp, store) = open();
    let names: Vec<String> = store
        .conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for expected in [
        "agents",
        "messages",
        "managers",
        "workspace_meta",
        "works",
        "missions",
        "mission_events",
        "mission_documents",
        "work_notes",
    ] {
        assert!(names.iter().any(|n| n == expected), "missing table {expected}: {names:?}");
    }
}

#[test]
fn register_agent_unique_never_overwrites() {
    let (_tmp, store) = open();
    let (first, token_a) = store.register_agent_unique("alice", "worker").unwrap();
    let (second, _token_b) = store.register_agent_unique("alice", "worker").unwrap();
    assert_eq!(first, "alice");
    assert_eq!(second, "alice-2");
    // Distinct session tokens per join.
    assert_ne!(token_a, _token_b);

    let archived = store.list_agents(true).unwrap();
    assert!(archived.iter().any(|a| a.id == "alice-2"));
}

#[test]
fn receive_consumes_messages_exactly_once() {
    let (_tmp, store) = open();
    store.register_agent_unique("alice", "worker").unwrap();
    store.register_agent_unique("bob", "worker").unwrap();

    store.send_message("alice", "bob", "one").unwrap();
    store.send_message("alice", "bob", "two").unwrap();
    let batch = store.receive_messages("bob").unwrap();
    assert_eq!(batch.len(), 2);
    let again = store.receive_messages("bob").unwrap();
    assert!(again.is_empty());
    assert!(!store.has_unread_messages("bob").unwrap());
}

#[test]
fn messages_to_archived_identities_are_preserved() {
    let (_tmp, store) = open();
    store.register_agent_unique("alice", "worker").unwrap();
    store.register_agent_unique("bob", "worker").unwrap();
    store.send_message("alice", "bob", "keep me").unwrap();

    store.unregister_agent("bob").unwrap();
    // The row survives; has_unread still reports it for history consumers.
    let all = store.all_messages(Some("bob")).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].content, "keep me");
    assert_eq!(all[0].read, false);
}

#[test]
fn manager_gate_is_a_table_not_a_role() {
    let (_tmp, store) = open();
    store.register_agent_unique("boss", "manager").unwrap();
    store.register_agent_unique("worker", "worker").unwrap();

    assert!(store.require_manager("boss").is_err());
    store.grant_manager("boss").unwrap();
    assert!(store.require_manager("boss").is_ok());
    // Role alone confers nothing.
    assert!(store.require_manager("worker").is_err());
    store.revoke_manager("boss").unwrap();
    assert!(store.require_manager("boss").is_err());
}

#[test]
fn work_notes_are_consumed_only_by_the_bound_executor() {
    let (_tmp, store) = open();
    store.register_agent_unique("worker", "worker").unwrap();
    store.register_agent_unique("inspector", "inspector").unwrap();
    store.conn
        .execute_batch(
            "INSERT INTO works (work_key, display_name, executor, prompt, lifecycle, created_at)
                 VALUES ('build', 'Build', 'worker', '', 'active', 0);
             INSERT INTO work_notes (work_key, kind, mission_id, content, created_at, read)
                 VALUES ('build', 'arrival', 'ms_t', 'mission arrived', 1000, 0);",
        )
        .unwrap();

    // Inspector is not the executor of build: nothing to consume, nothing marked.
    assert!(!store.has_unread_work_notes("inspector").unwrap());
    let wrong = store.receive_work_notes("inspector").unwrap();
    assert!(wrong.is_empty());

    // The bound executor sees and consumes it.
    assert!(store.has_unread_work_notes("worker").unwrap());
    let notes = store.receive_work_notes("worker").unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].mission_id, Some("ms_t".to_string()));
    assert!(store.receive_work_notes("worker").unwrap().is_empty());
}

#[test]
fn mission_rows_enforce_check_constraints() {
    let (_tmp, store) = open();
    // revision must be >= 1.
    let bad_revision = store.conn.execute(
        "INSERT INTO missions (mission_id, name, objective, contract_json, at, status, revision, created_at, created_by)
         VALUES ('ms_a', 'a', '', '{}', 'build', 'active', 0, 0, 'boss')",
        [],
    );
    assert!(bad_revision.is_err());
    // contract_json must be valid JSON.
    let bad_json = store.conn.execute(
        "INSERT INTO missions (mission_id, name, objective, contract_json, at, status, revision, created_at, created_by)
         VALUES ('ms_a', 'a', '', 'not json', 'build', 'active', 1, 0, 'boss')",
        [],
    );
    assert!(bad_json.is_err());
    // mission_events kinds are a closed set.
    store.conn
        .execute(
            "INSERT INTO missions (mission_id, name, objective, contract_json, at, status, revision, created_at, created_by)
             VALUES ('ms_a', 'a', '', '{}', 'build', 'active', 1, 0, 'boss')",
            [],
        )
        .unwrap();
    let bad_event = store.conn.execute(
        "INSERT INTO mission_events (mission_id, seq, kind, payload)
         VALUES ('ms_a', 1, 'explosion', '{}')",
        [],
    );
    assert!(bad_event.is_err());
}

#[test]
fn revision_cas_guard_rejects_stale_writes() {
    let (_tmp, store) = open();
    store.conn
        .execute(
            "INSERT INTO missions (mission_id, name, objective, contract_json, at, status, revision, created_at, created_by)
             VALUES ('ms_a', 'a', '', '{}', 'build', 'active', 1, 0, 'boss')",
            [],
        )
        .unwrap();
    // First writer moves 1 → 2.
    let moved = store
        .conn
        .execute(
            "UPDATE missions SET revision = revision + 1 WHERE mission_id = ?1 AND revision = ?2",
            params!["ms_a", 1],
        )
        .unwrap();
    assert_eq!(moved, 1);
    // A stale writer still carrying revision 1 affects zero rows.
    let stale = store
        .conn
        .execute(
            "UPDATE missions SET revision = revision + 1 WHERE mission_id = ?1 AND revision = ?2",
            params!["ms_a", 1],
        )
        .unwrap();
    assert_eq!(stale, 0);
    let revision: i64 = store
        .conn
        .query_row("SELECT revision FROM missions WHERE mission_id = 'ms_a'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(revision, 2);
}
