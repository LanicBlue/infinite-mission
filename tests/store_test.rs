use im::records::Tier;
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
        "workspace_meta",
        "works",
        "missions",
        "mission_events",
        "mission_documents",
        "work_notes",
    ] {
        assert!(names.iter().any(|n| n == expected), "missing table {expected}: {names:?}");
    }
    // The managers table retired with the tier ladder — never create it.
    assert!(
        !names.iter().any(|n| n == "managers"),
        "managers table must not exist: {names:?}"
    );
}

#[test]
fn register_agent_unique_never_overwrites() {
    let (_tmp, store) = open();
    let (first, token_a) = store.register_agent_unique("alice").unwrap();
    let (second, _token_b) = store.register_agent_unique("alice").unwrap();
    assert_eq!(first, "alice");
    assert_eq!(second, "alice-2");
    // Distinct session tokens per join.
    assert_ne!(token_a, _token_b);

    let archived = store.list_agents(true).unwrap();
    assert!(archived.iter().any(|a| a.id == "alice-2"));
}

#[test]
fn receive_consumes_system_notices_exactly_once() {
    let (_tmp, store) = open();
    store.register_agent_unique("alice").unwrap();
    store.register_agent_unique("bob").unwrap();

    store
        .send_message_envelope("workspace", "bob", "granted", "membership", None)
        .unwrap();
    store
        .send_message_envelope("workspace", "bob", "revoked", "membership", None)
        .unwrap();
    let batch = store.receive_messages("bob").unwrap();
    assert_eq!(batch.len(), 2);
    let again = store.receive_messages("bob").unwrap();
    assert!(again.is_empty());
    assert!(!store.has_unread_messages("bob").unwrap());
}

#[test]
fn peer_notes_are_rejected() {
    let (_tmp, store) = open();
    store.register_agent_unique("alice").unwrap();
    store.register_agent_unique("bob").unwrap();
    let err = store
        .send_message_envelope("alice", "bob", "hello", "note", None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("no peer channel"), "got: {err}");
}

#[test]
fn notices_to_archived_identities_are_preserved() {
    let (_tmp, store) = open();
    store.register_agent_unique("bob").unwrap();
    store.set_agent_tier("workspace", "bob", Tier::Manage).unwrap();

    store.unregister_agent("bob").unwrap();
    // The row survives; has_unread still reports it for history consumers.
    let all = store.all_messages(Some("bob")).unwrap();
    assert_eq!(all.len(), 1);
    assert!(all[0].content.contains("tier was set to manage"));
    assert_eq!(all[0].read, false);
}

#[test]
fn tier_ladder_gates_and_refusals() {
    let (_tmp, store) = open();
    for id in ["boss", "worker", "plain"] {
        store.register_agent_unique(id).unwrap();
    }

    // Fresh members sit at execute.
    assert_eq!(store.agent_tier("worker").unwrap(), Some(Tier::Execute));

    // set_agent_tier is the console path: ungated, any tier (manage included).
    store.set_agent_tier("workspace", "boss", Tier::Manage).unwrap();
    assert_eq!(store.agent_tier("boss").unwrap(), Some(Tier::Manage));

    // require_tier follows the linear ladder: manage passes everything,
    // execute passes only itself.
    assert!(store.require_tier("boss", Tier::Manage).is_ok());
    assert!(store.require_tier("worker", Tier::Execute).is_ok());
    let err = store.require_tier("worker", Tier::Publish).unwrap_err().to_string();
    assert!(err.contains("below publish tier"), "got: {err}");
    assert!(err.contains("boss"), "hint names the manage member: {err}");
    let err = store.require_tier("worker", Tier::Manage).unwrap_err().to_string();
    assert!(err.contains("below manage tier"), "got: {err}");

    // grant_publish: manage operator only.
    let err = store
        .grant_publish("plain", "worker")
        .unwrap_err()
        .to_string();
    assert!(err.contains("below manage tier"), "got: {err}");
    store.grant_publish("boss", "worker").unwrap();
    assert_eq!(store.agent_tier("worker").unwrap(), Some(Tier::Publish));
    // Idempotent: granting publish twice is fine.
    store.grant_publish("boss", "worker").unwrap();
    assert_eq!(store.agent_tier("worker").unwrap(), Some(Tier::Publish));

    // revoke_publish refuses anything not sitting at publish.
    let err = store
        .revoke_publish("boss", "plain")
        .unwrap_err()
        .to_string();
    assert!(err.contains("is not at publish tier"), "got: {err}");
    store.revoke_publish("boss", "worker").unwrap();
    assert_eq!(store.agent_tier("worker").unwrap(), Some(Tier::Execute));

    // Manage-tier targets are untouchable from the CLI surface — every
    // operator-tier action on them names the console.
    for action in [
        store.grant_publish("boss", "boss").unwrap_err().to_string(),
        store.revoke_publish("boss", "boss").unwrap_err().to_string(),
        store.delete_member("boss", "boss").unwrap_err().to_string(),
    ] {
        assert!(action.contains("console"), "manage target refused via console hint: {action}");
    }

    // delete_member: manage-tier operator deletes a plain member.
    store.delete_member("boss", "plain").unwrap();
    assert_eq!(store.agent_tier("plain").unwrap(), None);
}

#[test]
fn require_tier_hint_points_to_the_console_when_no_manage_member_exists() {
    let (_tmp, store) = open();
    store.register_agent_unique("lone").unwrap();
    let err = store.require_tier("lone", Tier::Manage).unwrap_err().to_string();
    assert!(err.contains("below manage tier"), "got: {err}");
    assert!(err.contains("console Members page"), "got: {err}");
}

#[test]
fn work_notes_are_consumed_only_by_the_bound_executor() {
    let (_tmp, store) = open();
    store.register_agent_unique("worker").unwrap();
    store.register_agent_unique("inspector").unwrap();
    store.conn
        .execute_batch(
            "INSERT INTO works (work_key, description, executor, prompt, created_at)
                 VALUES ('build', '', 'worker', '', 0);
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

#[test]
fn legacy_managers_fold_into_the_manage_tier() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("im.db");
    {
        // A pre-tier database: agents without the tier column, a managers
        // table holding a1/a2, plain member a3.
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE agents (
                id TEXT PRIMARY KEY,
                role TEXT NOT NULL DEFAULT '',
                joined_at INTEGER NOT NULL,
                session_token TEXT,
                last_seen INTEGER,
                status TEXT NOT NULL DEFAULT 'active',
                archived_at INTEGER
            );
             CREATE TABLE managers (
                agent_id TEXT PRIMARY KEY,
                granted_at INTEGER NOT NULL
             );
             CREATE TABLE workspace_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO agents (id, joined_at) VALUES ('a1', 1), ('a2', 2), ('a3', 3);
             INSERT INTO managers (agent_id, granted_at) VALUES ('a1', 10), ('a2', 20);",
        )
        .unwrap();
    }
    let store = Store::open(&db_path).unwrap();
    let tiers: Vec<String> = store
        .conn
        .prepare("SELECT tier FROM agents ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(tiers, vec!["manage", "manage", "execute"]);
    let managers: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'managers'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(managers, 0, "the managers table must be dropped");
    let has_tier: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('agents') WHERE name = 'tier'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_tier, 1, "the tier column must exist");
    // Reopening is idempotent.
    drop(store);
    let store = Store::open(&db_path).unwrap();
    assert_eq!(store.agent_tier("a1").unwrap(), Some(Tier::Manage));
}

#[test]
fn membership_actions_deliver_inbox_notices() {
    let (_tmp, store) = open();
    for id in ["boss", "worker"] {
        store.register_agent_unique(id).unwrap();
    }
    store.set_agent_tier("workspace", "boss", Tier::Manage).unwrap();
    store.grant_publish("boss", "worker").unwrap();
    store.revoke_publish("boss", "worker").unwrap();
    store.delete_agent("workspace", "worker").unwrap();

    // The notices outlive the deleted agent row (messages carry no FK).
    let notices: Vec<(String, String)> = store
        .conn
        .prepare(
            "SELECT from_agent, content FROM messages
             WHERE to_agent = 'worker' AND kind = 'membership' ORDER BY id",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    // (The set_tier notice went to boss, not worker — only worker's stream
    // is asserted here.)
    assert_eq!(
        notices,
        vec![
            (
                "boss".to_string(),
                "[membership] you were granted publish-tier permission.".to_string()
            ),
            (
                "boss".to_string(),
                "[membership] your publish-tier permission was revoked.".to_string()
            ),
            (
                "workspace".to_string(),
                "[membership] you were removed from this workspace.".to_string()
            ),
        ]
    );
}

#[test]
fn legacy_works_table_gains_the_description_column() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("im.db");
    {
        // A pre-description database: works without the summary column.
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE works (
                work_key TEXT PRIMARY KEY,
                display_name TEXT NOT NULL DEFAULT '',
                executor TEXT,
                prompt TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL
            );
            INSERT INTO works (work_key, executor, prompt, created_at)
             VALUES ('legacy', NULL, 'charter', 0);",
        )
        .unwrap();
    }
    let store = im::store::Store::open(&db_path).unwrap();
    let has_description: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('works') WHERE name = 'description'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_description, 1, "migration must add the description column");
    let summary: String = store
        .conn
        .query_row("SELECT description FROM works WHERE work_key = 'legacy'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(summary, "", "backfilled descriptions start empty");
}

#[test]
fn concurrent_opens_migrate_a_legacy_db_safely() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("im.db");
    {
        // Pre-tier fixture: one legacy manager (a1), one plain member (a2).
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE agents (
                id TEXT PRIMARY KEY,
                role TEXT NOT NULL DEFAULT '',
                joined_at INTEGER NOT NULL,
                session_token TEXT,
                last_seen INTEGER,
                status TEXT NOT NULL DEFAULT 'active',
                archived_at INTEGER
            );
             CREATE TABLE managers (agent_id TEXT PRIMARY KEY, granted_at INTEGER NOT NULL);
             CREATE TABLE workspace_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO agents (id, joined_at) VALUES ('a1', 1), ('a2', 2);
             INSERT INTO managers (agent_id, granted_at) VALUES ('a1', 10);",
        )
        .unwrap();
    }

    // Four opens racing the migration: the immediate transaction must
    // serialize them — every open succeeds, and the fold+drop happens
    // exactly once (the latecomers re-probe and skip).
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
    let mut handles = Vec::new();
    for _ in 0..4 {
        let path = db_path.clone();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            Store::open(&path).map(|_| ())
        }));
    }
    for handle in handles {
        handle.join().unwrap().unwrap();
    }

    let check = Store::open(&db_path).unwrap();
    let managers_tables: i64 = check
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'managers'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(managers_tables, 0, "the fold must drop the managers table");
    let tier: String = check
        .conn
        .query_row("SELECT tier FROM agents WHERE id = 'a1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(tier, "manage");
}
