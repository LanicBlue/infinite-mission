//! Mission contracts: station discipline compiled from YAML templates.
//!
//! A mission is one-shot mail — it carries its own configuration and travels
//! between stations. The contract holds that configuration: the entry
//! station, the routing table, the document declarations, and per-station
//! discipline (completion vocabulary + document rights). The station itself
//! owns only identity, executor, and a standing prompt.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const ABANDON: &str = "abandon";
pub const ANY: &str = "any";

fn valid_work_key(key: &str) -> bool {
    !key.is_empty()
        && key.chars().next().unwrap().is_ascii_lowercase()
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !key.contains("--")
        && !key.starts_with('-')
        && !key.ends_with('-')
}

// --- Compiled contract (stored as contract_json on the mission) ---

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Default)]
pub struct Completion {
    pub outcomes: Vec<String>,
    pub terminal: Vec<String>,
    #[serde(rename = "feedbackRequiredOn", default)]
    pub feedback_required_on: Vec<String>,
}

/// Write does NOT imply read — rights are independent lists of document ids.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Default)]
pub struct DocumentRights {
    #[serde(default)]
    pub read: Vec<String>,
    #[serde(default)]
    pub write: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Default)]
pub struct WorkDiscipline {
    pub completion: Completion,
    #[serde(rename = "documentRights", default)]
    pub document_rights: DocumentRights,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct RequireRef {
    #[serde(rename = "workKey")]
    pub work_key: String,
    pub outcome: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Default)]
pub struct Carry {
    #[serde(
        rename = "feedbackFrom",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub feedback_from: Option<String>,
    #[serde(default)]
    pub documents: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct PathEdge {
    pub from: String,
    pub when: String,
    pub to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub carry: Option<Carry>,
    #[serde(
        rename = "iterationPolicy",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub iteration_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<RequireRef>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct DocumentDeclaration {
    pub id: String,
    pub kind: String, // "file" | "collection"
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct MissionContract {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<TemplateProvenance>,
    pub entry: String,
    pub paths: Vec<PathEdge>,
    pub documents: Vec<DocumentDeclaration>,
    pub works: BTreeMap<String, WorkDiscipline>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct TemplateProvenance {
    pub path: String,
    pub digest: String,
}

// --- Template file shape (.im/templates/*.yaml) ---

#[derive(Debug, Deserialize)]
pub struct MissionTemplate {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub objective: Option<String>,
    pub entry: String,
    #[serde(default)]
    pub paths: Vec<PathEdge>,
    #[serde(default)]
    pub documents: Vec<DocumentDeclaration>,
    pub works: BTreeMap<String, TemplateWork>,
}

#[derive(Debug, Deserialize)]
pub struct TemplateWork {
    #[serde(default)]
    pub completion: Completion,
    #[serde(default, rename = "documentRights")]
    pub document_rights: DocumentRights,
}

pub fn parse_template(text: &str) -> Result<MissionTemplate> {
    let template: MissionTemplate =
        serde_yaml::from_str(text).context("template is not valid YAML for the expected schema")?;
    if template.schema_version != 4 {
        bail!(
            "unsupported template schemaVersion {} (expected 4)",
            template.schema_version
        );
    }
    Ok(template)
}

pub fn digest_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Compile a parsed template into the immutable contract carried by the
/// mission. Structural self-validation only (vocabulary, paths, documents);
/// station existence is checked against the project at create time.
pub fn compile(
    template: &MissionTemplate,
    template_path: &str,
    template_bytes: &[u8],
) -> Result<MissionContract> {
    let contract = MissionContract {
        template: Some(TemplateProvenance {
            path: template_path.to_string(),
            digest: digest_hex(template_bytes),
        }),
        entry: template.entry.clone(),
        paths: template.paths.clone(),
        documents: template.documents.clone(),
        works: template
            .works
            .iter()
            .map(|(key, work)| {
                (
                    key.clone(),
                    WorkDiscipline {
                        completion: work.completion.clone(),
                        document_rights: work.document_rights.clone(),
                    },
                )
            })
            .collect(),
    };
    validate_contract(&contract)?;
    Ok(contract)
}

pub fn validate_contract(contract: &MissionContract) -> Result<()> {
    if !valid_work_key(&contract.entry) {
        bail!(
            "entry work key {:?} is not a valid work key",
            contract.entry
        );
    }
    if !contract.works.contains_key(&contract.entry) {
        bail!(
            "entry station {:?} has no discipline in this contract",
            contract.entry
        );
    }

    let declared: std::collections::BTreeSet<&str> =
        contract.documents.iter().map(|d| d.id.as_str()).collect();

    for (key, discipline) in &contract.works {
        if !valid_work_key(key) {
            bail!("work key {key:?} is not valid (lowercase kebab-case)");
        }
        if discipline
            .completion
            .outcomes
            .contains(&ABANDON.to_string())
        {
            bail!("work {key}: outcome \"abandon\" is reserved and cannot be declared");
        }
        if discipline.completion.outcomes.is_empty() {
            bail!("work {key}: completion vocabulary is empty");
        }
        for terminal in &discipline.completion.terminal {
            if !discipline.completion.outcomes.contains(terminal) {
                bail!("work {key}: terminal outcome {terminal:?} is not in the vocabulary");
            }
        }
        for requires_feedback in &discipline.completion.feedback_required_on {
            if !discipline.completion.outcomes.contains(requires_feedback) {
                bail!("work {key}: feedback-required outcome {requires_feedback:?} is not in the vocabulary");
            }
        }
        for list_name in ["read", "write", "evidence"] {
            let list = match list_name {
                "read" => &discipline.document_rights.read,
                "write" => &discipline.document_rights.write,
                _ => &discipline.document_rights.evidence,
            };
            for id in list {
                if !declared.contains(id.as_str()) {
                    bail!("work {key}: documentRights.{list_name} references undeclared document {id:?}");
                }
            }
        }
    }

    for edge in &contract.paths {
        if !contract.works.contains_key(&edge.from) {
            bail!(
                "path from {:?}: station has no discipline in this contract",
                edge.from
            );
        }
        if !contract.works.contains_key(&edge.to) {
            bail!(
                "path to {:?}: station has no discipline in this contract",
                edge.to
            );
        }
        if edge.when == ABANDON {
            bail!(
                "path {:?} -> {:?}: \"abandon\" is reserved",
                edge.from,
                edge.to
            );
        }
        let source = &contract.works[&edge.from].completion;
        if edge.when != ANY && !source.outcomes.contains(&edge.when) {
            bail!(
                "path from {:?}: when {:?} is not in the source vocabulary",
                edge.from,
                edge.when
            );
        }
        if source.terminal.contains(&edge.when) {
            bail!(
                "path from {:?}: terminal outcome {:?} must not have out-edges",
                edge.from,
                edge.when
            );
        }
        if let Some(policy) = &edge.iteration_policy {
            if policy != "increment" && policy != "first" {
                bail!(
                    "path from {:?}: iterationPolicy must be increment|first",
                    edge.from
                );
            }
        }
        for require in &edge.requires {
            if !contract.works.contains_key(&require.work_key) {
                bail!(
                    "path from {:?}: requires references unknown station {:?}",
                    edge.from,
                    require.work_key
                );
            }
            let required = &contract.works[&require.work_key].completion;
            if !required.outcomes.contains(&require.outcome) {
                bail!(
                    "path from {:?}: requires outcome {:?} is not in {}'s vocabulary",
                    edge.from,
                    require.outcome,
                    require.work_key
                );
            }
        }
        if let Some(carry) = &edge.carry {
            for id in &carry.documents {
                if !declared.contains(id.as_str()) {
                    bail!(
                        "path from {:?}: carry references undeclared document {id:?}",
                        edge.from
                    );
                }
            }
            if let Some(from) = &carry.feedback_from {
                if !contract.works.contains_key(from) {
                    bail!(
                        "path from {:?}: carry.feedbackFrom references unknown station {from:?}",
                        edge.from
                    );
                }
            }
        }
    }

    for document in &contract.documents {
        if document.id.is_empty() || document.id.len() > 128 {
            bail!("document id {:?} must be 1..=128 chars", document.id);
        }
        if document.kind != "file" && document.kind != "collection" {
            bail!("document {:?}: kind must be file|collection", document.id);
        }
        let path = &document.path;
        if path.is_empty() {
            bail!("document {:?}: path is empty", document.id);
        }
        if path.contains('\0') {
            bail!("document {:?}: path contains NUL", document.id);
        }
        let segments: Vec<&str> = path.split('/').collect();
        for segment in &segments {
            if segment.is_empty() || *segment == "." || *segment == ".." {
                bail!("document {:?}: path has empty or dot segments", document.id);
            }
        }
        match document.kind.as_str() {
            "file" => {
                if path.ends_with('/') {
                    bail!("document {:?}: file paths never end in /", document.id);
                }
                if document.index.is_some() {
                    bail!(
                        "document {:?}: only collections may declare an index",
                        document.id
                    );
                }
            }
            _ => {
                if !path.ends_with('/') {
                    bail!(
                        "document {:?}: collection paths always end in /",
                        document.id
                    );
                }
                if let Some(index) = &document.index {
                    if index != "README.md" {
                        bail!("document {:?}: index must be README.md", document.id);
                    }
                }
            }
        }
    }

    Ok(())
}

/// Per-outcome routing projection for a station (the submit-time view):
/// `to` with 2+ targets means a submit must choose nextNode explicitly;
/// terminal outcomes end the mission; abandon is always appended.
pub fn routes_for(contract: &MissionContract, work_key: &str) -> Vec<RouteRow> {
    let discipline = contract.works.get(work_key);
    let mut rows = Vec::new();
    let outcomes = discipline
        .map(|d| d.completion.outcomes.clone())
        .unwrap_or_default();
    for outcome in outcomes {
        let to: Vec<String> = contract
            .paths
            .iter()
            .filter(|edge| edge.from == work_key && (edge.when == outcome || edge.when == ANY))
            .map(|edge| edge.to.clone())
            .collect();
        let terminal = discipline
            .map(|d| d.completion.terminal.contains(&outcome))
            .unwrap_or(false);
        rows.push(RouteRow {
            outcome,
            to,
            terminal,
        });
    }
    rows.push(RouteRow {
        outcome: ABANDON.to_string(),
        to: Vec::new(),
        terminal: true,
    });
    rows
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct RouteRow {
    pub outcome: String,
    pub to: Vec<String>,
    pub terminal: bool,
}
