use anyhow::{Context, Result};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use std::path::Path;

use crate::records::{AgentRecord, MessageRecord};

/// Create the im workspace database schema. Fresh project: no legacy
/// migrations, one canonical shape.
fn schema() -> String {
    r#"
    PRAGMA journal_mode=WAL;
    PRAGMA busy_timeout=5000;

    CREATE TABLE IF NOT EXISTS agents (
        id TEXT PRIMARY KEY,
        role TEXT NOT NULL,
        joined_at INTEGER NOT NULL,
        session_token TEXT,
        last_seen INTEGER,
        status TEXT NOT NULL DEFAULT 'active',
        archived_at INTEGER
    );

    CREATE TABLE IF NOT EXISTS messages (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        from_agent TEXT NOT NULL,
        to_agent TEXT NOT NULL,
        content TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        read INTEGER NOT NULL DEFAULT 0,
        kind TEXT NOT NULL DEFAULT 'note',
        reply_to INTEGER
    );

    CREATE TABLE IF NOT EXISTS operators (
        agent_id TEXT PRIMARY KEY,
        granted_at INTEGER NOT NULL
    );

    -- The workspace IS the project: works and missions hang directly off it.
    -- A stable workspace uuid namespaces mission ids (sha256(ws\0key)).
    CREATE TABLE IF NOT EXISTS workspace_meta (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS works (
        work_key TEXT PRIMARY KEY,
        display_name TEXT NOT NULL DEFAULT '',
        executor TEXT,
        prompt TEXT NOT NULL DEFAULT '',
        lifecycle TEXT NOT NULL DEFAULT 'active',
        created_at INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS missions (
        mission_id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        objective TEXT NOT NULL,
        contract_json TEXT NOT NULL CHECK (json_valid(contract_json)),
        at TEXT,
        status TEXT NOT NULL CHECK (status IN ('active', 'ended')),
        revision INTEGER NOT NULL CHECK (revision >= 1),
        ended_disposition TEXT CHECK (ended_disposition IN ('completed', 'abandoned', 'deleted')),
        ended_by_work TEXT,
        ended_by_iteration INTEGER,
        ended_at INTEGER,
        created_at INTEGER NOT NULL,
        created_by TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_missions_mailbox ON missions (status, at);

    CREATE TABLE IF NOT EXISTS mission_events (
        mission_id TEXT NOT NULL,
        seq INTEGER NOT NULL,
        type TEXT NOT NULL CHECK (type IN ('mission.created', 'mission.round.completed', 'mission.routed', 'mission.ended')),
        payload TEXT NOT NULL CHECK (json_valid(payload)),
        created_at INTEGER NOT NULL,
        PRIMARY KEY (mission_id, seq)
    );

    CREATE TABLE IF NOT EXISTS mission_documents (
        key_hash TEXT PRIMARY KEY,
        mission_id TEXT NOT NULL,
        work_key TEXT NOT NULL,
        document_id TEXT NOT NULL,
        path TEXT NOT NULL,
        content_sha256 TEXT NOT NULL,
        written_by TEXT NOT NULL,
        written_at INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS work_notes (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        work_key TEXT NOT NULL,
        kind TEXT NOT NULL,
        mission_id TEXT,
        content TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        read INTEGER NOT NULL DEFAULT 0
    );
    "#
    .to_string()
}

pub struct Store {
    pub conn: Connection,
}

impl Store {
    /// The stable workspace uuid (created on first open). It namespaces
    /// mission ids the same way PS namespaces them by project id.
    pub fn workspace_id(&self) -> Result<String> {
        if let Some(id) = self
            .conn
            .query_row(
                "SELECT value FROM workspace_meta WHERE key = 'workspace_id'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            return Ok(id);
        }
        let id = format!("ws_{}", uuid::Uuid::new_v4().simple());
        self.conn.execute(
            "INSERT OR IGNORE INTO workspace_meta (key, value) VALUES ('workspace_id', ?1)",
            [&id],
        )?;
        Ok(id)
    }
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open database: {}", path.display()))?;
        conn.execute_batch(&schema())?;
        let _ = conn.execute(
            "UPDATE agents SET status = 'active' WHERE status IS NULL OR status = ''",
            [],
        );
        Ok(Self { conn })
    }

    // --- Agents ---

    pub fn register_agent_unique(&self, requested_id: &str, role: &str) -> Result<(String, String)> {
        let now = chrono::Utc::now().timestamp();
        let candidates = std::iter::once(requested_id.to_string()).chain(
            (2..=99).map(|i| format!("{}-{}", requested_id, i)),
        );
        for candidate in candidates {
            let token = uuid::Uuid::new_v4().to_string();
            let reactivated = self.conn.execute(
                "UPDATE agents
                 SET role = ?2, joined_at = ?3, session_token = ?4, status = 'active', archived_at = NULL
                 WHERE id = ?1 AND status = 'archived'",
                rusqlite::params![candidate, role, now, token],
            )?;
            if reactivated > 0 {
                return Ok((candidate, token));
            }
            let inserted = self.conn.execute(
                "INSERT OR IGNORE INTO agents (id, role, joined_at, session_token, status)
                 VALUES (?1, ?2, ?3, ?4, 'active')",
                rusqlite::params![candidate, role, now, token],
            )?;
            if inserted > 0 {
                return Ok((candidate, token));
            }
        }
        anyhow::bail!("Too many agents with base id '{}'", requested_id);
    }

    pub fn get_session_token(&self, id: &str) -> Result<Option<String>> {
        let token: Option<String> = self
            .conn
            .query_row(
                "SELECT session_token FROM agents WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(token)
    }

    fn agent_status(&self, id: &str) -> Result<Option<String>> {
        let status: Option<String> = self
            .conn
            .query_row("SELECT status FROM agents WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(status)
    }

    pub fn require_active_agent(&self, id: &str) -> Result<()> {
        match self.agent_status(id)?.as_deref() {
            Some("active") => Ok(()),
            Some("archived") => {
                anyhow::bail!("{id} is archived. Re-join with `im join {id}` to reactivate it.")
            }
            Some(_) | None => {
                let names = self.agent_names()?;
                anyhow::bail!("{id} does not exist. Online agents: {}", names.join(", "))
            }
        }
    }

    pub fn agent_names(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM agents WHERE status = 'active' ORDER BY joined_at")?;
        let names = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(names)
    }

    pub fn touch_agent(&self, id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE agents SET last_seen = ?1 WHERE id = ?2",
            params![chrono::Utc::now().timestamp(), id],
        )?;
        Ok(())
    }

    pub fn unregister_agent(&self, id: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        let updated = self.conn.execute(
            "UPDATE agents SET status = 'archived', archived_at = ?2
             WHERE id = ?1 AND status = 'active'",
            params![id, now],
        )?;
        if updated == 1 {
            return Ok(());
        }
        match self.agent_status(id)?.as_deref() {
            Some("archived") => {
                anyhow::bail!("{id} is archived. Re-join with `im join {id}` to reactivate it.")
            }
            Some(_) | None => {
                let names = self.agent_names()?;
                anyhow::bail!("{id} does not exist. Online agents: {}", names.join(", "))
            }
        }
    }

    pub fn list_agents(&self, include_archived: bool) -> Result<Vec<AgentRecord>> {
        let sql = if include_archived {
            "SELECT id, role, joined_at, last_seen, status, archived_at FROM agents ORDER BY joined_at"
        } else {
            "SELECT id, role, joined_at, last_seen, status, archived_at FROM agents WHERE status = 'active' ORDER BY joined_at"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let agents = stmt
            .query_map([], |row| {
                Ok(AgentRecord {
                    id: row.get(0)?,
                    role: row.get(1)?,
                    joined_at: row.get(2)?,
                    last_seen: row.get(3)?,
                    status: row.get(4)?,
                    archived_at: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(agents)
    }

    // --- Messages (freeform layer) ---

    pub fn send_message(&self, from: &str, to: &str, content: &str) -> Result<()> {
        self.send_message_envelope(from, to, content, "note", None)
    }

    pub fn send_message_envelope(
        &self,
        from: &str,
        to: &str,
        content: &str,
        kind: &str,
        reply_to: Option<i64>,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO messages (from_agent, to_agent, content, created_at, read, kind, reply_to)
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6)",
            params![from, to, content, now, kind, reply_to],
        )?;
        Ok(())
    }

    pub fn send_message_checked(&self, from: &str, to: &str, content: &str) -> Result<()> {
        self.require_active_agent(to)?;
        self.send_message(from, to, content)
    }

    pub fn broadcast_message(&self, from: &str, content: &str) -> Result<Vec<String>> {
        let agents = self.agent_names()?;
        let recipients: Vec<_> = agents.into_iter().filter(|a| a != from).collect();
        for to in &recipients {
            self.send_message(from, to, content)?;
        }
        Ok(recipients)
    }

    const MESSAGE_COLUMNS: &'static str =
        "id, from_agent, to_agent, content, created_at, read, kind, reply_to";

    fn map_message_row(row: &rusqlite::Row) -> rusqlite::Result<MessageRecord> {
        Ok(MessageRecord {
            id: row.get(0)?,
            from_agent: row.get(1)?,
            to_agent: row.get(2)?,
            content: row.get(3)?,
            created_at: row.get(4)?,
            read: row.get(5)?,
            kind: row.get(6)?,
            reply_to: row.get(7)?,
        })
    }

    /// Atomically read and mark messages as read using a transaction with an
    /// id fence (a message arriving between SELECT and UPDATE is never
    /// silently marked read).
    pub fn receive_messages(&self, agent_id: &str) -> Result<Vec<MessageRecord>> {
        self.require_active_agent(agent_id)?;
        let tx = self.conn.unchecked_transaction()?;
        let mut stmt = tx.prepare(&format!(
            "SELECT {} FROM messages WHERE to_agent = ?1 AND read = 0 ORDER BY created_at, id",
            Self::MESSAGE_COLUMNS
        ))?;
        let messages: Vec<MessageRecord> = stmt
            .query_map([agent_id], Self::map_message_row)?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);

        if !messages.is_empty() {
            let max_id = messages.iter().map(|msg| msg.id).max().unwrap_or(0);
            tx.execute(
                "UPDATE messages SET read = 1 WHERE read = 0 AND id <= ?1 AND to_agent = ?2",
                params![max_id, agent_id],
            )?;
        }
        tx.commit()?;
        Ok(messages)
    }

    pub fn has_unread_messages(&self, agent_id: &str) -> Result<bool> {
        self.require_active_agent(agent_id)?;
        let has: bool = self.conn.query_row(
            "SELECT COUNT(*) > 0 FROM messages WHERE to_agent = ?1 AND read = 0",
            [agent_id],
            |row| row.get(0),
        )?;
        Ok(has)
    }

    pub fn all_messages(&self, agent_id: Option<&str>) -> Result<Vec<MessageRecord>> {
        let (sql, param): (&str, Vec<&str>) = match agent_id {
            Some(id) => (
                &format!(
                    "SELECT {} FROM messages WHERE from_agent = ?1 OR to_agent = ?1 ORDER BY created_at, id",
                    Self::MESSAGE_COLUMNS
                ),
                vec![id],
            ),
            None => (
                &format!(
                    "SELECT {} FROM messages ORDER BY created_at, id",
                    Self::MESSAGE_COLUMNS
                ),
                vec![],
            ),
        };
        let mut stmt = self.conn.prepare(sql)?;
        let messages = stmt
            .query_map(params_from_iter(param), Self::map_message_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(messages)
    }

    // --- Operators ---

    pub fn grant_operator(&self, agent_id: &str) -> Result<()> {
        self.require_active_agent(agent_id)?;
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT OR REPLACE INTO operators (agent_id, granted_at) VALUES (?1, ?2)",
            params![agent_id, now],
        )?;
        Ok(())
    }

    pub fn revoke_operator(&self, agent_id: &str) -> Result<()> {
        let revoked = self.conn.execute(
            "DELETE FROM operators WHERE agent_id = ?1",
            [agent_id],
        )?;
        if revoked == 0 {
            anyhow::bail!("{agent_id} is not an operator");
        }
        Ok(())
    }

    pub fn list_operators(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT agent_id FROM operators ORDER BY granted_at")?;
        let names = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(names)
    }

    pub fn require_operator(&self, agent_id: &str) -> Result<()> {
        let found: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM operators WHERE agent_id = ?1",
                [agent_id],
                |row| row.get(0),
            )
            .optional()?;
        if found.is_some() {
            return Ok(());
        }
        let operators = self.list_operators()?;
        if operators.is_empty() {
            anyhow::bail!(
                "{agent_id} is not an operator. No operators are granted yet; \
                 a human must run `im grant <agent-id>` first."
            );
        }
        anyhow::bail!(
            "{agent_id} is not an operator. Granted operators: {}",
            operators.join(", ")
        );
    }
}
