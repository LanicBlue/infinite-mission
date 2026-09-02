//! The mission domain: stations, missions, submit adjudication, documents,
//! events, arrival notes. Semantics follow work-mission-v6.2:
//! - the mission carries its own contract and delivery history;
//! - `revision` is the sole submit CAS token;
//! - the contract IS the permission — there is no off-contract routing;
//! - the executor is a switchable station attribute, never an entity.

use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::contract::{self, MissionContract, RouteRow};
use crate::records::{
    DocumentReceiptRecord, MissionEventRecord, MissionRecord, WorkNoteRecord,
    WorkRecord,
};
use crate::store::Store;

pub const EVENT_CREATED: &str = "mission.created";
pub const EVENT_ROUND: &str = "mission.round.completed";
pub const EVENT_ROUTED: &str = "mission.routed";
pub const EVENT_ENDED: &str = "mission.ended";

fn sha256_hex(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

// --- Works (stations) ---
// The workspace IS the project: stations hang directly off the workspace,
// so there is no project layer anywhere in the domain.

impl Store {
    pub fn create_work(
        &self,
        manager: &str,
        work_key: &str,
        description: &str,
        executor: Option<&str>,
        prompt: &str,
    ) -> Result<String> {
        self.require_manager(manager)?;
        if !contract_work_key(work_key) {
            bail!("work key {work_key:?} must be lowercase kebab-case");
        }
        if let Some(executor_id) = executor {
            self.require_active_agent(executor_id)?;
        }
        let inserted = self.conn.execute(
            "INSERT INTO works (work_key, description, executor, prompt, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![work_key, description, executor, prompt, now()],
        );
        match inserted {
            Ok(_) => {}
            // Only a PK violation means the key is taken; anything else is a
            // real error and must surface as itself (a masked SQL failure
            // here once misreported as "already exists").
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                bail!("work '{work_key}' already exists in this workspace")
            }
            Err(err) => return Err(err.into()),
        }
        Ok(work_key.to_string())
    }

    pub fn get_work(&self, work_key: &str) -> Result<WorkRecord> {
        let work = self
            .conn
            .query_row(
                "SELECT work_key, description, executor, prompt
                 FROM works WHERE work_key = ?1",
                [work_key],
                map_work_row,
            )
            .optional()?;
        match work {
            Some(work) => Ok(work),
            None => bail!("work '{work_key}' does not exist in this workspace"),
        }
    }

    pub fn list_works(&self) -> Result<Vec<WorkRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT work_key, description, executor, prompt
             FROM works ORDER BY work_key",
        )?;
        let works = stmt
            .query_map([], map_work_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(works)
    }

    /// Last-write-wins by design: executor moves are an manager ordering
    /// problem, not a lost-update hazard (the mission revision is the only
    /// submit CAS).
    pub fn set_work_executor(
        &self,
        manager: &str,
        work_key: &str,
        executor: Option<&str>,
    ) -> Result<()> {
        self.require_manager(manager)?;
        self.get_work(work_key)?;
        if let Some(executor_id) = executor {
            self.require_active_agent(executor_id)?;
        }
        self.conn.execute(
            "UPDATE works SET executor = ?1 WHERE work_key = ?2",
            params![executor, work_key],
        )?;
        Ok(())
    }

    pub fn set_work_prompt(&self, manager: &str, work_key: &str, prompt: &str) -> Result<()> {
        self.require_manager(manager)?;
        self.get_work(work_key)?;
        self.conn.execute(
            "UPDATE works SET prompt = ?1 WHERE work_key = ?2",
            params![prompt, work_key],
        )?;
        Ok(())
    }

    pub fn set_work_description(&self, manager: &str, work_key: &str, description: &str) -> Result<()> {
        self.require_manager(manager)?;
        self.get_work(work_key)?;
        self.conn.execute(
            "UPDATE works SET description = ?1 WHERE work_key = ?2",
            params![description, work_key],
        )?;
        Ok(())
    }

    /// Hard delete (PS station-lock semantics): a station referenced by ANY
    /// active mission contract is locked — the reference set is
    /// entry ∪ works keys ∪ path endpoints (a mission's `at` always sits
    /// inside that set, so a live mission can never have its station pulled
    /// out from under it). With the lock in place no soft-delete lifecycle
    /// is needed: un-referenced stations are simply gone, key freed.
    pub fn delete_work(&self, manager: &str, work_key: &str) -> Result<()> {
        self.require_manager(manager)?;
        self.get_work(work_key)?; // bails "does not exist" for unknown keys
        let offenders = self.stations_active_missions(work_key)?;
        if !offenders.is_empty() {
            bail!(
                "work '{work_key}' is locked by active missions {} — a station \
                 referenced by a live contract (entry, works, or path endpoints) \
                 cannot be deleted; end or move those missions first",
                offenders.join(", ")
            );
        }
        let deleted = self
            .conn
            .execute("DELETE FROM works WHERE work_key = ?1", [work_key])?;
        if deleted == 0 {
            bail!("work '{work_key}' does not exist in this workspace");
        }
        // Stale arrival notes must not outlive the station: a same-key
        // successor bound to the same executor would otherwise inherit them.
        self.conn
            .execute("DELETE FROM work_notes WHERE work_key = ?1", [work_key])?;
        Ok(())
    }

    /// Active missions whose contract references this station (the lock set).
    fn stations_active_missions(&self, work_key: &str) -> Result<Vec<String>> {
        Ok(self
            .active_mission_references()?
            .into_iter()
            .filter(|(_, _, referenced)| referenced.contains(work_key))
            .map(|(mission_id, _, _)| mission_id)
            .collect())
    }

    /// Inbound ("en route") counts: active missions whose contract references
    /// a station but are currently parked elsewhere. Held missions don't
    /// count — those are the per-station `holding` numbers.
    pub fn inbound_counts(&self) -> Result<std::collections::BTreeMap<String, usize>> {
        let mut counts = std::collections::BTreeMap::new();
        for (_, at, referenced) in self.active_mission_references()? {
            for key in referenced {
                if at.as_deref() != Some(key.as_str()) {
                    *counts.entry(key).or_insert(0) += 1;
                }
            }
        }
        Ok(counts)
    }

    /// (mission_id, at, referenced stations) for every active mission. The
    /// reference set is entry ∪ works keys ∪ path endpoints (a mission's
    /// `at` always sits inside it).
    fn active_mission_references(
        &self,
    ) -> Result<Vec<(String, Option<String>, BTreeSet<String>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT mission_id, at, contract_json FROM missions WHERE status = 'active'",
        )?;
        let rows: Vec<(String, Option<String>, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<Result<_, _>>()?;
        let mut out = Vec::new();
        for (mission_id, at, contract_json) in rows {
            let contract = parse_contract(&contract_json)?;
            let mut referenced: BTreeSet<String> = BTreeSet::new();
            referenced.insert(contract.entry.clone());
            referenced.extend(contract.works.keys().cloned());
            for edge in &contract.paths {
                referenced.insert(edge.from.clone());
                referenced.insert(edge.to.clone());
            }
            out.push((mission_id, at, referenced));
        }
        Ok(out)
    }

    /// Works whose executor is this agent — the agent's duty stations.
    pub fn works_for_executor(&self, agent_id: &str) -> Result<Vec<WorkRecord>> {
        Ok(self
            .list_works()?
            .into_iter()
            .filter(|work| work.executor.as_deref() == Some(agent_id))
            .collect())
    }
}

fn contract_work_key(key: &str) -> bool {
    !key.is_empty()
        && key.chars().next().unwrap().is_ascii_lowercase()
        && key.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !key.contains("--")
        && !key.starts_with('-')
        && !key.ends_with('-')
}

fn map_work_row(row: &rusqlite::Row) -> rusqlite::Result<WorkRecord> {
    Ok(WorkRecord {
        work_key: row.get(0)?,
        description: row.get(1)?,
        executor: row.get(2)?,
        prompt: row.get(3)?,
    })
}

// --- Missions ---

#[derive(Debug, Serialize)]
pub struct MissionCreateOutcome {
    pub mission_id: String,
    pub existed: bool,
}

impl Store {
    /// Mission id is derived from the idempotency key namespaced by the
    /// workspace uuid: the same key always lands on the same mission.
    /// Creation only walks templates.
    pub fn create_mission(
        &self,
        manager: &str,
        template: &contract::MissionTemplate,
        template_path: &str,
        template_bytes: &[u8],
        idempotency_key: &str,
        name_override: Option<&str>,
        objective_override: Option<&str>,
    ) -> Result<MissionCreateOutcome> {
        self.require_manager(manager)?;
        let contract = contract::compile(template, template_path, template_bytes)?;

        // Station reference validation: every referenced station must exist
        // and be active — referenced stations are editable but not deletable
        // for as long as this mission lives.
        let mut referenced: BTreeSet<String> = BTreeSet::new();
        referenced.insert(contract.entry.clone());
        for key in contract.works.keys() {
            referenced.insert(key.clone());
        }
        for edge in &contract.paths {
            referenced.insert(edge.from.clone());
            referenced.insert(edge.to.clone());
        }
        let mut unknown = Vec::new();
        for key in &referenced {
            match self.get_work(key) {
                Ok(_) => {}
                Err(_) => unknown.push(key.clone()),
            }
        }
        if !unknown.is_empty() {
            let known: Vec<String> = self
                .list_works()?
                .into_iter()
                .map(|work| work.work_key)
                .collect();
            bail!(
                "template references unknown stations: {} — known in this workspace: {}",
                unknown.join(", "),
                known.join(", ")
            );
        }

        let name = name_override
            .or(template.name.as_deref())
            .unwrap_or("mission");
        let objective = objective_override
            .or(template.objective.as_deref())
            .unwrap_or("");
        let workspace_id = self.workspace_id()?;
        let mission_id = format!("ms_{}", &sha256_hex(&[&workspace_id, idempotency_key])[..32]);
        let created = now();

        let tx = self.conn.unchecked_transaction()?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO missions (
                mission_id, name, objective, contract_json,
                at, status, revision, created_at, created_by
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'active', 1, ?6, ?7)",
            params![
                mission_id,
                name,
                objective,
                serde_json::to_string(&contract)?,
                contract.entry,
                created,
                manager
            ],
        )?;
        if inserted == 0 {
            return Ok(MissionCreateOutcome {
                mission_id,
                existed: true,
            });
        }
        append_event(
            &tx,
            &mission_id,
            EVENT_CREATED,
            json!({
                "missionId": mission_id,
                "entry": contract.entry,
                "name": name,
                "objective": objective,
                "template": {"path": template_path, "digest": contract.template.as_ref().map(|t| t.digest.clone()).unwrap_or_default()},
            }),
            created,
        )?;
        insert_arrival_note(&tx, &contract.entry, &mission_id, &format!("[{mission_id}] {name}"), created)?;
        tx.commit()?;
        Ok(MissionCreateOutcome {
            mission_id,
            existed: false,
        })
    }

    pub fn get_mission(&self, mission_id: &str) -> Result<MissionRecord> {
        let mission = self
            .conn
            .query_row(
                "SELECT mission_id, name, objective, contract_json, at, status,
                        revision, ended_disposition, ended_by_work, ended_by_iteration,
                        ended_at, created_at, created_by
                 FROM missions WHERE mission_id = ?1",
                [mission_id],
                map_mission_row,
            )
            .optional()?;
        match mission {
            Some(mission) => Ok(mission),
            None => bail!("mission '{mission_id}' does not exist"),
        }
    }

    pub fn mission_events(&self, mission_id: &str) -> Result<Vec<MissionEventRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, type, payload, created_at FROM mission_events
             WHERE mission_id = ?1 ORDER BY seq",
        )?;
        let events = stmt
            .query_map([mission_id], |row| {
                Ok(MissionEventRecord {
                    seq: row.get(0)?,
                    kind: row.get(1)?,
                    payload: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(events)
    }

    /// Iteration is derived from routed events, never stored: scan newest
    /// first for the first delivery to this station.
    pub fn standing_iteration(&self, mission: &MissionRecord, work_key: &str) -> Result<Option<i64>> {
        let events = self.mission_events(&mission.mission_id)?;
        for event in events.iter().rev() {
            if event.kind != EVENT_ROUTED {
                continue;
            }
            let payload: serde_json::Value = serde_json::from_str(&event.payload)?;
            if payload["to"] == serde_json::json!(work_key) {
                return Ok(payload["iteration"].as_i64().or(Some(1)));
            }
        }
        let contract = parse_contract(&mission.contract_json)?;
        if contract.entry == work_key {
            Ok(Some(1))
        } else {
            Ok(None)
        }
    }

    /// Missions at any station this agent currently guards (listMyRuns).
    pub fn missions_for_executor(&self, agent_id: &str) -> Result<Vec<MissionRecord>> {
        let works = self.works_for_executor(agent_id)?;
        let mut missions = Vec::new();
        for work in &works {
            let mut stmt = self.conn.prepare(
                "SELECT mission_id, name, objective, contract_json, at, status,
                        revision, ended_disposition, ended_by_work, ended_by_iteration,
                        ended_at, created_at, created_by
                 FROM missions
                 WHERE at = ?1 AND status = 'active'
                 ORDER BY created_at",
            )?;
            let rows = stmt
                .query_map(params![work.work_key], map_mission_row)?
                .collect::<Result<Vec<_>, _>>()?;
            missions.extend(rows);
        }
        Ok(missions)
    }

    /// The human attention plane: missions currently parked at user stations.
    pub fn inbox_missions(&self) -> Result<Vec<(MissionRecord, WorkRecord)>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.mission_id, m.name, m.objective, m.contract_json, m.at, m.status,
                    m.revision, m.ended_disposition, m.ended_by_work, m.ended_by_iteration,
                    m.ended_at, m.created_at, m.created_by, w.executor
             FROM missions m
             JOIN works w ON w.work_key = m.at
             WHERE m.status = 'active' AND w.executor IS NULL
             ORDER BY m.created_at",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((map_mission_row(row)?, map_work_row_by_prefix(row)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // --- Submit adjudication ---

    /// The full PS adjudication order, committed in one transaction. The
    /// mission revision is the sole CAS token; attribution requires the
    /// submitter to be the station's on-duty executor; the contract is the
    /// only routing authority.
    pub fn submit_mission(
        &self,
        agent_id: &str,
        mission_id: &str,
        expected_revision: i64,
        outcome: &str,
        next_node: Option<&str>,
        reason: Option<&str>,
        feedback: Option<&str>,
        receipt_ids: &[String],
    ) -> Result<SubmitOutcome> {
        self.require_active_agent(agent_id)?;
        let mission = self.get_mission(mission_id)?;
        if mission.status == "ended" {
            bail!("Mission has already ended (disposition: {})",
                mission.ended_disposition.unwrap_or_else(|| "unknown".to_string()));
        }
        let at = mission.at.clone().context("active mission has no station")?;
        if expected_revision != mission.revision {
            bail!(
                "Mission state was superseded (current revision: {}); re-read the mission and retry",
                mission.revision
            );
        }
        let work = self.get_work(&at)?;
        let user_station = work.executor.is_none();
        if user_station {
            // A user station is the human's mailbox: an manager resolves it.
            self.require_manager(agent_id).with_context(|| {
                format!(
                    "station '{at}' is a user station — missions parked there are resolved by a manager"
                )
            })?;
        } else if work.executor.as_deref() != Some(agent_id) {
            bail!(
                "Mission belongs to another executor — station '{at}' is currently held by {}",
                work.executor.as_deref().unwrap_or("?")
            );
        }
        let contract = parse_contract(&mission.contract_json)?;
        let discipline = contract
            .works
            .get(&at)
            .with_context(|| format!("station '{at}' has no discipline in this mission's contract"))?;

        // Receipts: minted at THIS station, inside its write rights.
        let mut frozen_receipts = Vec::new();
        for receipt_id in receipt_ids {
            let key_hash = receipt_id
                .strip_prefix("document:")
                .unwrap_or(receipt_id.as_str());
            let receipt = self
                .get_receipt(key_hash)?
                .with_context(|| format!("receipt {receipt_id} does not resolve"))?;
            if receipt.mission_id != mission.mission_id {
                bail!("receipt {receipt_id} was minted for another mission");
            }
            if receipt.work_key != at {
                bail!("receipt {receipt_id} was minted at another station");
            }
            if !discipline.document_rights.write.contains(&receipt.document_id) {
                bail!(
                    "receipt {receipt_id} covers document {:?} which station '{at}' may not write",
                    receipt.document_id
                );
            }
            frozen_receipts.push(json!({"documentId": receipt.document_id, "keyHash": key_hash}));
        }

        let created = now();

        // abandon: always legal on an open round, never carries a next node.
        if outcome == contract::ABANDON {
            if next_node.is_some() {
                bail!("abandon does not accept a next node");
            }
            if let Some(reason_text) = reason {
                if reason_text.trim().is_empty() {
                    bail!("reason must be non-empty when given");
                }
            }
            return self.end_mission_tx(&mission, "abandoned", Some(&at), outcome, agent_id, reason, &frozen_receipts, user_station);
        }

        if !discipline.completion.outcomes.iter().any(|o| o == outcome) {
            let mut permitted = discipline.completion.outcomes.clone();
            permitted.push(contract::ABANDON.to_string());
            bail!(
                "outcome '{outcome}' is not in station '{at}' vocabulary; permitted: {}",
                permitted.join(", ")
            );
        }
        if discipline.completion.feedback_required_on.iter().any(|o| o == outcome) {
            let feedback_text = feedback.unwrap_or("");
            if feedback_text.trim().is_empty() {
                bail!("outcome '{outcome}' requires non-empty feedback");
            }
        }
        if let Some(reason_text) = reason {
            if reason_text.len() > 2000 || reason_text.trim().is_empty() {
                bail!("reason must be 1..=2000 chars when given");
            }
        }

        let terminal = discipline.completion.terminal.iter().any(|o| o == outcome);
        let edges: Vec<&contract::PathEdge> = contract
            .paths
            .iter()
            .filter(|edge| edge.from == at && (edge.when == outcome || edge.when == contract::ANY))
            .collect();

        if terminal {
            if next_node.is_some() {
                bail!("terminal outcome '{outcome}' does not accept a next node");
            }
            return self.end_mission_tx(&mission, "completed", Some(&at), outcome, agent_id, reason, &frozen_receipts, user_station);
        }

        let chosen = match next_node {
            None => match edges.len() {
                0 => bail!(
                    "outcome '{outcome}' has no contract continuation — choose a terminal outcome instead"
                ),
                1 => edges[0],
                _ => {
                    let candidates: Vec<String> =
                        edges.iter().map(|edge| edge.to.clone()).collect();
                    bail!(
                        "outcome '{outcome}' has several contract continuations — the next node must be chosen explicitly (candidates: {})",
                        candidates.join(", ")
                    );
                }
            },
            Some(node) => match edges.iter().find(|edge| edge.to == node) {
                Some(edge) => *edge,
                None => {
                    let candidates: Vec<String> =
                        edges.iter().map(|edge| edge.to.clone()).collect();
                    bail!(
                        "next node '{node}' is not a contract continuation of this mission (candidates: {})",
                        candidates.join(", ")
                    );
                }
            },
        };

        // requires admission: every referenced round must have happened.
        for require in &chosen.requires {
            let satisfied = self.round_completed(&mission.mission_id, &require.work_key, &require.outcome)?;
            if !satisfied {
                bail!(
                    "edge to '{}' requires {} to have completed with outcome '{}' first",
                    chosen.to, require.work_key, require.outcome
                );
            }
        }

        // A hop onto a user station requires a reason for the human.
        let destination = self.get_work(&chosen.to)?;
        if destination.executor.is_none() && reason.map(|r| r.trim().is_empty()).unwrap_or(true) {
            bail!("routing to user station '{}' requires a non-empty --reason", chosen.to);
        }

        let target_standing = self.standing_iteration(&mission, &chosen.to)?.unwrap_or(0);
        let iteration_at_target = if chosen.iteration_policy.as_deref() == Some("increment") {
            target_standing + 1
        } else {
            std::cmp::max(target_standing, 1)
        };

        let tx = self.conn.unchecked_transaction()?;
        let round_payload = json!({
            "workKey": at,
            "iteration": self.standing_iteration(&mission, &at)?.unwrap_or(1),
            "outcome": outcome,
            "resolvedBy": {"executorRef": agent_id, "plane": if user_station { "manager" } else { "agent" }},
            "reason": reason,
            "feedback": feedback,
            "documentReceipts": frozen_receipts,
        });
        append_event(&tx, &mission.mission_id, EVENT_ROUND, round_payload, created)?;
        let new_revision = mission.revision + 1;
        let updated = tx.execute(
            "UPDATE missions SET at = ?1, revision = ?2 WHERE mission_id = ?3 AND revision = ?4",
            params![chosen.to, new_revision, mission.mission_id, mission.revision],
        )?;
        if updated != 1 {
            bail!("Mission state was superseded (current revision: {}); re-read and retry", mission.revision + 1);
        }
        append_event(
            &tx,
            &mission.mission_id,
            EVENT_ROUTED,
            json!({
                "from": at,
                "when": outcome,
                "to": chosen.to,
                "iteration": iteration_at_target,
                "revision": new_revision,
            }),
            created,
        )?;
        insert_arrival_note(
            &tx,
            &chosen.to,
            &mission.mission_id,
            &format!("[{}] {} → {} (round: {})", mission.mission_id, at, chosen.to, outcome),
            created,
        )?;
        tx.commit()?;

        Ok(SubmitOutcome {
            mission_id: mission.mission_id.clone(),
            routed_to: Some(chosen.to.clone()),
            iteration_at_target: Some(iteration_at_target),
            revision: new_revision,
            mission_ended: false,
        })
    }

    fn round_completed(&self, mission_id: &str, work_key: &str, outcome: &str) -> Result<bool> {
        let events = self.mission_events(mission_id)?;
        Ok(events.iter().any(|event| {
            if event.kind != EVENT_ROUND {
                return false;
            }
            let payload: serde_json::Value = serde_json::from_str(&event.payload).unwrap_or_default();
            payload["workKey"] == serde_json::json!(work_key) && payload["outcome"] == serde_json::json!(outcome)
        }))
    }

    fn end_mission_tx(
        &self,
        mission: &MissionRecord,
        disposition: &str,
        by_work: Option<&str>,
        outcome: &str,
        by_agent: &str,
        reason: Option<&str>,
        frozen_receipts: &[serde_json::Value],
        operator_plane: bool,
    ) -> Result<SubmitOutcome> {
        let created = now();
        let iteration = match by_work {
            Some(work_key) => self.standing_iteration(mission, work_key)?.unwrap_or(1),
            None => 1,
        };
        let tx = self.conn.unchecked_transaction()?;
        append_event(
            &tx,
            &mission.mission_id,
            EVENT_ROUND,
            json!({
                "workKey": by_work,
                "iteration": iteration,
                "outcome": outcome,
                "resolvedBy": {"executorRef": by_agent, "plane": if operator_plane { "manager" } else { "agent" }},
                "reason": reason,
                "feedback": null,
                "documentReceipts": frozen_receipts,
            }),
            created,
        )?;
        let new_revision = mission.revision + 1;
        let updated = tx.execute(
            "UPDATE missions SET at = NULL, status = 'ended', revision = ?1,
                    ended_disposition = ?2, ended_by_work = ?3, ended_by_iteration = ?4, ended_at = ?5
             WHERE mission_id = ?6 AND revision = ?7",
            params![
                new_revision,
                disposition,
                by_work,
                iteration,
                created,
                mission.mission_id,
                mission.revision
            ],
        )?;
        if updated != 1 {
            bail!("Mission state was superseded; re-read and retry");
        }
        append_event(
            &tx,
            &mission.mission_id,
            EVENT_ENDED,
            json!({
                "disposition": disposition,
                "outcome": outcome,
                "byWork": by_work,
                "byIteration": iteration,
            }),
            created,
        )?;
        tx.commit()?;

        // mission.ended fan-out: a workspace notice, not a peer message.
        // Past round resolvers hear the end even after the mailbox has moved.
        let participants = self.mission_participants(&mission.mission_id)?;
        for participant in participants {
            if participant != by_agent {
                let _ = self.send_message_envelope(
                    "workspace",
                    &participant,
                    &format!(
                        "[{}] mission ended: {} ({})",
                        mission.mission_id, disposition, outcome
                    ),
                    "mission_ended",
                    None,
                );
            }
        }
        Ok(SubmitOutcome {
            mission_id: mission.mission_id.clone(),
            routed_to: None,
            iteration_at_target: None,
            revision: new_revision,
            mission_ended: true,
        })
    }

    pub fn mission_participants(&self, mission_id: &str) -> Result<Vec<String>> {
        let events = self.mission_events(mission_id)?;
        let mut seen = BTreeSet::new();
        for event in events {
            if event.kind != EVENT_ROUND {
                continue;
            }
            let payload: serde_json::Value = serde_json::from_str(&event.payload)?;
            if let Some(resolver) = payload["resolvedBy"]["executorRef"].as_str() {
                seen.insert(resolver.to_string());
            }
        }
        Ok(seen.into_iter().collect())
    }

    /// Manager delete: disposition=deleted, not attributed to any round.
    pub fn delete_mission(&self, manager: &str, mission_id: &str, reason: Option<&str>) -> Result<()> {
        self.require_manager(manager)?;
        let mission = self.get_mission(mission_id)?;
        if mission.status == "ended" {
            bail!("Mission has already ended");
        }
        let created = now();
        let tx = self.conn.unchecked_transaction()?;
        let new_revision = mission.revision + 1;
        let updated = tx.execute(
            "UPDATE missions SET at = NULL, status = 'ended', revision = ?1,
                    ended_disposition = 'deleted', ended_at = ?2
             WHERE mission_id = ?3 AND revision = ?4",
            params![new_revision, created, mission.mission_id, mission.revision],
        )?;
        if updated != 1 {
            bail!("Mission state was superseded; re-read and retry");
        }
        append_event(
            &tx,
            &mission.mission_id,
            EVENT_ENDED,
            json!({
                "disposition": "deleted",
                "outcome": null,
                "byWork": null,
                "byIteration": null,
                "reason": reason,
            }),
            created,
        )?;
        tx.commit()?;
        Ok(())
    }

    // --- Work notes (arrival notifications) ---

    pub fn has_unread_work_notes(&self, agent_id: &str) -> Result<bool> {
        self.require_active_agent(agent_id)?;
        let has: bool = self.conn.query_row(
            "SELECT COUNT(*) > 0
             FROM work_notes n
             JOIN works w ON w.work_key = n.work_key
             WHERE w.executor = ?1 AND n.read = 0",
            [agent_id],
            |row| row.get(0),
        )?;
        Ok(has)
    }

    pub fn receive_work_notes(&self, agent_id: &str) -> Result<Vec<WorkNoteRecord>> {
        self.require_active_agent(agent_id)?;
        let tx = self.conn.unchecked_transaction()?;
        let mut stmt = tx.prepare(
            "SELECT n.id, n.work_key, n.kind, n.mission_id, n.content, n.created_at
             FROM work_notes n
             JOIN works w ON w.work_key = n.work_key
             WHERE w.executor = ?1 AND n.read = 0
             ORDER BY n.created_at, n.id",
        )?;
        let notes: Vec<WorkNoteRecord> = stmt
            .query_map([agent_id], |row| {
                Ok(WorkNoteRecord {
                    id: row.get(0)?,
                    work_key: row.get(1)?,
                    kind: row.get(2)?,
                    mission_id: row.get(3)?,
                    content: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        if !notes.is_empty() {
            let max_id = notes.iter().map(|note| note.id).max().unwrap_or(0);
            tx.execute(
                "UPDATE work_notes SET read = 1 WHERE read = 0 AND id <= ?1 AND EXISTS (
                     SELECT 1 FROM works w
                     WHERE w.work_key = work_notes.work_key
                       AND w.executor = ?2)",
                params![max_id, agent_id],
            )?;
        }
        tx.commit()?;
        Ok(notes)
    }
}

impl RunView {
    pub fn ended_note(&self) -> String {
        // Reconstructed from the events is overkill here; the caller prints
        // disposition context via the events command. Keep it simple.
        "mission is no longer in the mail stream".to_string()
    }
}

#[derive(Debug, Serialize)]
pub struct SubmitOutcome {
    pub mission_id: String,
    pub routed_to: Option<String>,
    pub iteration_at_target: Option<i64>,
    pub revision: i64,
    pub mission_ended: bool,
}

fn map_mission_row(row: &rusqlite::Row) -> rusqlite::Result<MissionRecord> {
    Ok(MissionRecord {
        mission_id: row.get(0)?,
        name: row.get(1)?,
        objective: row.get(2)?,
        contract_json: row.get(3)?,
        at: row.get(4)?,
        status: row.get(5)?,
        revision: row.get(6)?,
        ended_disposition: row.get(7)?,
        ended_by_work: row.get(8)?,
        ended_by_iteration: row.get(9)?,
        ended_at: row.get(10)?,
        created_at: row.get(11)?,
        created_by: row.get(12)?,
    })
}

fn map_work_row_by_prefix(row: &rusqlite::Row) -> rusqlite::Result<WorkRecord> {
    // Row layout: mission columns 0..12, then works executor(13). Only the
    // executor is needed here; other station facts are re-read by callers.
    Ok(WorkRecord {
        work_key: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
        description: String::new(),
        executor: row.get(13)?,
        prompt: String::new(),
    })
}

pub fn parse_contract(contract_json: &str) -> Result<MissionContract> {
    serde_json::from_str(contract_json).context("stored contract failed to parse")
}

fn append_event(
    tx: &rusqlite::Transaction,
    mission_id: &str,
    kind: &str,
    payload: serde_json::Value,
    created: i64,
) -> Result<()> {
    let next: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM mission_events WHERE mission_id = ?1",
        [mission_id],
        |row| row.get(0),
    )?;
    tx.execute(
        "INSERT INTO mission_events (mission_id, seq, type, payload, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![mission_id, next, kind, payload.to_string(), created],
    )?;
    Ok(())
}

fn insert_arrival_note(
    tx: &rusqlite::Transaction,
    work_key: &str,
    mission_id: &str,
    content: &str,
    created: i64,
) -> Result<()> {
    tx.execute(
        "INSERT INTO work_notes (work_key, kind, mission_id, content, created_at, read)
         VALUES (?1, 'mission_arrived', ?2, ?3, ?4, 0)",
        params![work_key, mission_id, content, created],
    )?;
    Ok(())
}

// --- Documents ---

impl Store {
    /// Write a declared document. Content addressing makes retries free: the
    /// same bytes always land on the same receipt. The file lands first, the
    /// notary row follows.
    pub fn write_mission_document(
        &self,
        agent_id: &str,
        mission_id: &str,
        document_id: &str,
        content: &[u8],
        documents_root: &Path,
    ) -> Result<String> {
        self.require_active_agent(agent_id)?;
        let mission = self.get_mission(mission_id)?;
        if mission.status != "active" {
            bail!("Mission is not active at a station (mailbox has moved on or ended)");
        }
        let at = mission.at.context("active mission has no station")?;
        let contract = parse_contract(&mission.contract_json)?;
        let discipline = contract
            .works
            .get(&at)
            .context("current station has no discipline in this mission")?;
        let declaration = contract
            .documents
            .iter()
            .find(|d| d.id == document_id)
            .with_context(|| format!("document id {document_id:?} is not declared by the mission contract"))?;
        if !discipline.document_rights.write.contains(&declaration.id) {
            bail!(
                "station '{at}' may not write document {:?} (not in its documentRights.write)",
                declaration.id
            );
        }
        let work = self.get_work(&at)?;
        self.require_doc_on_duty(agent_id, &work, "write")?;

        let content_sha = {
            let digest = Sha256::digest(content);
            digest.iter().map(|b| format!("{b:02x}")).collect::<String>()
        };
        let key_hash = sha256_hex(&[mission_id, &declaration.path, &content_sha]);

        let file_path: PathBuf = documents_root
            .join(mission_id)
            .join(&declaration.path)
            .to_path_buf();
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        std::fs::write(&file_path, content)
            .with_context(|| format!("failed to write {}", file_path.display()))?;
        self.conn.execute(
            "INSERT OR IGNORE INTO mission_documents
                (key_hash, mission_id, work_key, document_id, path, content_sha256, written_by, written_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![key_hash, mission_id, at, declaration.id, declaration.path, content_sha, agent_id, now()],
        )?;
        Ok(format!("document:{key_hash}"))
    }

    pub fn read_mission_document(
        &self,
        agent_id: &str,
        mission_id: &str,
        path: &str,
        documents_root: &Path,
    ) -> Result<String> {
        self.require_active_agent(agent_id)?;
        let mission = self.get_mission(mission_id)?;
        if mission.status != "active" {
            bail!("Mission is not active at a station (mailbox has moved on or ended)");
        }
        let at = mission.at.context("active mission has no station")?;
        let contract = parse_contract(&mission.contract_json)?;
        let discipline = contract
            .works
            .get(&at)
            .context("current station has no discipline in this mission")?;
        let declaration = contract
            .documents
            .iter()
            .find(|d| d.path == path)
            .with_context(|| format!("no document declared at path {path:?}"))?;
        if !discipline.document_rights.read.contains(&declaration.id) {
            bail!(
                "station '{at}' may not read document {:?} (not in its documentRights.read)",
                declaration.id
            );
        }
        let work = self.get_work(&at)?;
        self.require_doc_on_duty(agent_id, &work, "read")?;
        let file_path = documents_root.join(mission_id).join(path);
        std::fs::read_to_string(&file_path)
            .with_context(|| format!("document bytes not found at {}", file_path.display()))
    }

    /// Document access follows the station's on-duty rule: the bound
    /// executor, or — at a user station — any manager (mirroring run_view's
    /// on-duty projection and submit's manager resolution of user stations).
    fn require_doc_on_duty(
        &self,
        agent_id: &str,
        work: &WorkRecord,
        action: &str,
    ) -> Result<()> {
        match work.executor.as_deref() {
            None => self.require_manager(agent_id).with_context(|| {
                format!(
                    "document {action}s at user station '{}' are resolved by a manager",
                    work.work_key
                )
            }),
            Some(executor) if executor == agent_id => Ok(()),
            Some(executor) => bail!(
                "Document {action}s belong to the station's on-duty executor ({executor})"
            ),
        }
    }

    pub fn get_receipt(&self, key_hash: &str) -> Result<Option<DocumentReceiptRecord>> {
        let receipt = self
            .conn
            .query_row(
                "SELECT key_hash, mission_id, work_key, document_id, path, content_sha256, written_by, written_at
                 FROM mission_documents WHERE key_hash = ?1",
                [key_hash],
                |row| {
                    Ok(DocumentReceiptRecord {
                        key_hash: row.get(0)?,
                        mission_id: row.get(1)?,
                        work_key: row.get(2)?,
                        document_id: row.get(3)?,
                        path: row.get(4)?,
                        content_sha256: row.get(5)?,
                        written_by: row.get(6)?,
                        written_at: row.get(7)?,
                    })
                },
            )
            .optional()?;
        Ok(receipt)
    }
}

// --- Run view (the agent-facing projection) ---

#[derive(Debug, Serialize)]
pub struct RunView {
    pub mission_id: String,
    pub name: String,
    pub objective: String,
    pub at: Option<String>,
    pub status: String,
    pub revision: i64,
    pub iteration: Option<i64>,
    /// Rendered station prompt (read-time fusion of station text + mission facts).
    pub prompt: Option<String>,
    pub on_duty: bool,
    pub outcomes: Vec<String>,
    pub terminal: Vec<String>,
    pub routes: Vec<RouteRow>,
    pub documents: Vec<DocumentResolvedRow>,
}

#[derive(Debug, Serialize)]
pub struct DocumentResolvedRow {
    pub id: String,
    pub kind: String,
    pub path: String,
    pub may_read: bool,
    pub may_write: bool,
    pub receipt: Option<String>,
}

impl Store {
    pub fn run_view(&self, mission_id: &str, for_agent: Option<&str>) -> Result<RunView> {
        let mission = self.get_mission(mission_id)?;
        let contract = parse_contract(&mission.contract_json)?;
        let (station_prompt, on_duty, discipline, iteration, routes) = match &mission.at {
            Some(at) => {
                let work = self.get_work(at)?;
                let discipline = contract.works.get(at);
                let iteration = self.standing_iteration(&mission, at)?;
                let reason = self.last_round_reason(&mission.mission_id)?;
                let from = self.last_routed_from(&mission.mission_id)
                    .unwrap_or_else(|| mission.created_by.clone());
                let prompt = if work.prompt.is_empty() {
                    None
                } else {
                    Some(interpolate_prompt(
                        &work.prompt,
                        &InterpolationContext {
                            name: &mission.name,
                            objective: &mission.objective,
                            from: &from,
                            iteration,
                            reason: reason.as_deref(),
                        },
                    ))
                };
                let on_duty = for_agent
                    .map(|agent| {
                        if work.executor.is_none() {
                            // User station: any manager is on duty.
                            self.list_managers()
                                .map(|ops| ops.iter().any(|op| op == agent))
                                .unwrap_or(false)
                        } else {
                            work.executor.as_deref() == Some(agent)
                        }
                    })
                    .unwrap_or(false);
                let routes = contract::routes_for(&contract, at);
                (prompt, on_duty, discipline.cloned(), iteration, routes)
            }
            None => (None, false, None, None, Vec::new()),
        };

        let (outcomes, terminal) = discipline
            .as_ref()
            .map(|d| (d.completion.outcomes.clone(), d.completion.terminal.clone()))
            .unwrap_or_default();

        let mut documents = Vec::new();
        for declaration in &contract.documents {
            let receipt: Option<String> = self
                .conn
                .query_row(
                    "SELECT key_hash FROM mission_documents
                     WHERE mission_id = ?1 AND document_id = ?2
                     ORDER BY written_at DESC, key_hash DESC LIMIT 1",
                    params![mission.mission_id, declaration.id],
                    |row| row.get(0),
                )
                .optional()?;
            let (may_read, may_write) = match &discipline {
                Some(d) => (
                    d.document_rights.read.contains(&declaration.id),
                    d.document_rights.write.contains(&declaration.id),
                ),
                None => (false, false),
            };
            documents.push(DocumentResolvedRow {
                id: declaration.id.clone(),
                kind: declaration.kind.clone(),
                path: declaration.path.clone(),
                may_read,
                may_write,
                receipt: receipt.map(|hash| format!("document:{hash}")),
            });
        }

        Ok(RunView {
            mission_id: mission.mission_id,
            name: mission.name,
            objective: mission.objective,
            at: mission.at,
            status: mission.status,
            revision: mission.revision,
            iteration,
            prompt: station_prompt,
            on_duty,
            outcomes,
            terminal,
            routes,
            documents,
        })
    }

    fn last_round_reason(&self, mission_id: &str) -> Result<Option<String>> {
        let events = self.mission_events(mission_id)?;
        for event in events.iter().rev() {
            if event.kind == EVENT_ROUND {
                let payload: serde_json::Value = serde_json::from_str(&event.payload)?;
                return Ok(payload["reason"].as_str().map(String::from));
            }
        }
        Ok(None)
    }

    fn last_routed_from(&self, mission_id: &str) -> Option<String> {
        let events = self.mission_events(mission_id).ok()?;
        for event in events.iter().rev() {
            if event.kind == EVENT_ROUTED {
                let payload: serde_json::Value = serde_json::from_str(&event.payload).ok()?;
                return payload["from"].as_str().map(String::from);
            }
        }
        None
    }
}

pub struct InterpolationContext<'a> {
    pub name: &'a str,
    pub objective: &'a str,
    pub from: &'a str,
    pub iteration: Option<i64>,
    pub reason: Option<&'a str>,
}

/// Unknown slots stay literal; `from` falls back to the creator for
/// never-routed missions; `iteration` renders as 1 when unset.
pub fn interpolate_prompt(prompt: &str, context: &InterpolationContext) -> String {
    prompt
        .replace("{mission.name}", context.name)
        .replace("{mission.objective}", context.objective)
        .replace("{mission.from}", context.from)
        .replace(
            "{mission.iteration}",
            &context.iteration.unwrap_or(1).to_string(),
        )
        .replace("{mission.reason}", context.reason.unwrap_or(""))
}
