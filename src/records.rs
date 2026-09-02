use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AgentRecord {
    pub id: String,
    pub role: String,
    pub joined_at: i64,
    pub last_seen: Option<i64>,
    pub status: String,
    pub archived_at: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MessageRecord {
    pub id: i64,
    pub from_agent: String,
    pub to_agent: String,
    pub content: String,
    pub created_at: i64,
    pub read: bool,
    pub kind: String,
    pub reply_to: Option<i64>,
}

/// A station: an addressable executor slot. All discipline (completion
/// vocabulary, document rights) is mission-contract-owned; the station keeps
/// identity, the on-duty executor, and a standing prompt.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WorkRecord {
    pub work_key: String,
    pub display_name: String,
    /// Current on-duty executor agent id; NULL means a user station.
    pub executor: Option<String>,
    pub prompt: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MissionRecord {
    pub mission_id: String,
    pub name: String,
    pub objective: String,
    /// Immutable compiled contract (contract::MissionContract as JSON).
    pub contract_json: String,
    /// Station currently holding the mission; NULL once ended.
    pub at: Option<String>,
    pub status: String,
    /// The sole submit CAS token — bumped on every transition.
    pub revision: i64,
    pub ended_disposition: Option<String>,
    pub ended_by_work: Option<String>,
    pub ended_by_iteration: Option<i64>,
    pub ended_at: Option<i64>,
    pub created_at: i64,
    pub created_by: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MissionEventRecord {
    pub seq: i64,
    pub kind: String,
    pub payload: String,
    pub created_at: i64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DocumentReceiptRecord {
    pub key_hash: String,
    pub mission_id: String,
    pub work_key: String,
    pub document_id: String,
    pub path: String,
    pub content_sha256: String,
    pub written_by: String,
    pub written_at: i64,
}

/// An arrival notification pinned to a station. Notes belong to the station:
/// whoever is currently bound receives them, so an identity change never
/// strands unread notifications.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WorkNoteRecord {
    pub id: i64,
    pub work_key: String,
    pub kind: String,
    pub mission_id: Option<String>,
    pub content: String,
    pub created_at: i64,
}
