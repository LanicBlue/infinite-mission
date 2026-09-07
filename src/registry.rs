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

/// Register a workspace (called by `im init`). Idempotent append-only —
/// cleanup is a deliberate act (`im workspaces --prune` or the console's
/// registry card), never side-effected by init: an init running inside a
/// sandbox that cannot see sibling workspaces must not be able to prune
/// entries it is merely blind to.
pub fn register(workspace: &Path) -> Result<()> {
    let path = registry_path()?;
    register_at(&path, workspace)
}

fn register_at(path: &Path, workspace: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut registry = read_raw(path);
    if !registry.workspaces.iter().any(|p| p == workspace) {
        registry.workspaces.push(workspace.to_path_buf());
    }
    registry.workspaces.sort();
    registry.workspaces.dedup();
    let json = serde_json::to_string_pretty(&registry)?;
    std::fs::write(path, json + "\n").with_context(|| format!("writing {}", path.display()))?;
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

/// Remove one registered entry (console action). True when it was present.
pub fn remove(workspace: &Path) -> Result<bool> {
    remove_at(&registry_path()?, workspace)
}

fn remove_at(path: &Path, workspace: &Path) -> Result<bool> {
    let mut registry = read_raw(path);
    let before = registry.workspaces.len();
    registry.workspaces.retain(|p| p != workspace);
    let removed = before > registry.workspaces.len();
    if removed {
        std::fs::write(path, serde_json::to_string_pretty(&registry)? + "\n")?;
    }
    Ok(removed)
}

/// Drop entries whose directories no longer exist.
pub fn prune() -> Result<usize> {
    prune_at(&registry_path()?)
}

fn prune_at(path: &Path) -> Result<usize> {
    let registry = read_raw(path);
    let before = registry.workspaces.len();
    let kept: Vec<PathBuf> = registry
        .workspaces
        .into_iter()
        .filter(|p| p.join(".im").is_dir())
        .collect();
    let removed = before - kept.len();
    std::fs::write(
        path,
        serde_json::to_string_pretty(&Registry { workspaces: kept })? + "\n",
    )?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_with(entries: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("workspaces.json");
        let list: Vec<PathBuf> = entries.iter().map(PathBuf::from).collect();
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&Registry { workspaces: list }).unwrap(),
        )
        .unwrap();
        dir
    }
    fn read_entries(dir: &tempfile::TempDir) -> Vec<String> {
        let raw: Registry = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("workspaces.json")).unwrap(),
        )
        .unwrap();
        raw.workspaces
            .iter()
            .map(|p| p.display().to_string())
            .collect()
    }

    #[test]
    fn register_appends_without_pruning() {
        let live = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(live.path().join(".im")).unwrap();
        let dead = tempfile::TempDir::new().unwrap();
        let dead_path = dead.path().to_path_buf();
        drop(dead);
        let dir = file_with(&[dead_path.to_str().unwrap()]);
        let reg = dir.path().join("workspaces.json");
        register_at(&reg, live.path()).unwrap();
        // 沙箱盲视/死条目都不许被顺手清掉
        assert_eq!(read_entries(&dir).len(), 2);
        register_at(&reg, live.path()).unwrap(); // 幂等
        assert_eq!(read_entries(&dir).len(), 2);
    }

    #[test]
    fn remove_and_prune_are_deliberate() {
        let live = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(live.path().join(".im")).unwrap();
        let dead = tempfile::TempDir::new().unwrap();
        let dead_path = dead.path().to_path_buf();
        drop(dead);
        let dir = file_with(&[live.path().to_str().unwrap(), dead_path.to_str().unwrap()]);
        let reg = dir.path().join("workspaces.json");
        assert!(remove_at(&reg, live.path()).unwrap());
        assert!(!remove_at(&reg, live.path()).unwrap());
        assert_eq!(prune_at(&reg).unwrap(), 1); // 只清死条目
        assert_eq!(read_entries(&dir), Vec::<String>::new());
    }
}
