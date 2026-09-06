//! The global workspace registry: one JSON file at `~/.im/workspaces.json`
//! holding absolute paths of every workspace created by `im init`. The
//! console uses it as the switch allowlist; nothing else scans the disk.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Default)]
struct Registry {
    workspaces: Vec<PathBuf>,
}

fn registry_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("$HOME is not set")?;
    let dir = Path::new(&home).join(".im");
    Ok(dir.join("workspaces.json"))
}

fn read_raw(path: &Path) -> Registry {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Register a workspace (called by `im init`). Idempotent; keeps the list
/// sorted and prunes entries whose directories no longer exist.
pub fn register(workspace: &Path) -> Result<()> {
    let path = registry_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut registry = read_raw(&path);
    registry.workspaces.retain(|p| p.join(".im").is_dir());
    if !registry.workspaces.iter().any(|p| p == workspace) {
        registry.workspaces.push(workspace.to_path_buf());
    }
    registry.workspaces.sort();
    registry.workspaces.dedup();
    let json = serde_json::to_string_pretty(&registry)?;
    std::fs::write(&path, json + "\n").with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Every registered workspace whose directory still exists, plus `current`
/// (the console always offers the workspace it started in).
pub fn discover(current: &Path) -> Vec<PathBuf> {
    let mut found = vec![current.to_path_buf()];
    let registry = read_raw(&registry_path().unwrap_or_else(|_| PathBuf::from("/nonexistent")));
    for p in registry.workspaces {
        if p.join(".im").is_dir() && !found.contains(&p) {
            found.push(p);
        }
    }
    found.sort();
    found.dedup();
    found
}

/// All registered entries with a liveness flag, for `im workspaces`.
pub fn list_with_liveness() -> Result<Vec<(PathBuf, bool)>> {
    let registry = read_raw(&registry_path()?);
    Ok(registry
        .workspaces
        .into_iter()
        .map(|p| {
            let live = p.join(".im").is_dir();
            (p, live)
        })
        .collect())
}

/// Drop entries whose directories no longer exist.
pub fn prune() -> Result<usize> {
    let path = registry_path()?;
    let registry = read_raw(&path);
    let before = registry.workspaces.len();
    let kept: Vec<PathBuf> = registry
        .workspaces
        .into_iter()
        .filter(|p| p.join(".im").is_dir())
        .collect();
    let removed = before - kept.len();
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&Registry { workspaces: kept })? + "\n",
    )?;
    Ok(removed)
}
