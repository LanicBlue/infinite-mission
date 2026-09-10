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
    register_at(&path, workspace)?;
    mirror_bridge(&[workspace.to_path_buf()], &[]);
    Ok(())
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

/// Remove one registered entry (console action, `im workspaces --remove`).
/// True when it was present. The bridge mirror runs even when the registry
/// didn't hold the entry — the pinned list may still carry it.
pub fn remove(workspace: &Path) -> Result<bool> {
    // Registry entries are physical paths (init stores getcwd(), which
    // resolves symlinks); match caller input against them so `/tmp/x`
    // finds `/private/tmp/x`. Fall back to the raw path for ghosts whose
    // directory no longer exists.
    let target = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let removed = remove_at(&registry_path()?, &target)?;
    mirror_bridge(&[], &[target]);
    Ok(removed)
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
    let dropped = prune_at(&registry_path()?)?;
    mirror_bridge(&[], &dropped);
    Ok(dropped.len())
}

fn prune_at(path: &Path) -> Result<Vec<PathBuf>> {
    let registry = read_raw(path);
    let before = registry.workspaces;
    let kept: Vec<PathBuf> = before
        .iter()
        .filter(|p| p.join(".im").is_dir())
        .cloned()
        .collect();
    let dropped: Vec<PathBuf> = before
        .iter()
        .filter(|p| !kept.contains(p))
        .cloned()
        .collect();
    std::fs::write(
        path,
        serde_json::to_string_pretty(&Registry { workspaces: kept })? + "\n",
    )?;
    Ok(dropped)
}

/// The t3 bridge (plugins/t3) can pin its workspace set as the
/// `workspaces` array in ~/.im/t3-bridge.json instead of following this
/// registry. A pinned list goes stale the moment im registers or drops a
/// workspace — and a workspace missing from it gets no t3 members tending
/// missions — so every registry mutation mirrors itself into it. Best
/// effort by design: no bridge config is a no-op, and a broken one must
/// not fail the im command that triggered the mirror (warn on stderr).
/// `workspaces: null` means the bridge follows the registry live; there is
/// no list to maintain, so it is left untouched.
fn mirror_bridge(added: &[PathBuf], dropped: &[PathBuf]) {
    let Ok(registry_file) = registry_path() else {
        return;
    };
    let Some(config) = registry_file
        .parent()
        .map(|dir| dir.join("t3-bridge.json"))
        .filter(|config| config.is_file())
    else {
        return;
    };
    if let Err(err) = mirror_bridge_at(&config, added, dropped) {
        eprintln!("warning: t3-bridge workspaces not synced: {err:#}");
    }
}

fn mirror_bridge_at(config: &Path, added: &[PathBuf], dropped: &[PathBuf]) -> Result<()> {
    let raw =
        std::fs::read_to_string(config).with_context(|| format!("reading {}", config.display()))?;
    let mut root: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", config.display()))?;
    // 跟随模式（null 或无 workspaces 键）：桥直接读注册表，没有要维护的
    // 清单——静默成功，别把「无需同步」当失败警告刷屏
    let list = match root.get_mut("workspaces") {
        None | Some(serde_json::Value::Null) => return Ok(()),
        Some(value) => value
            .as_array_mut()
            .context("`workspaces` is not an array — leaving the bridge config alone")?,
    };
    if list.iter().any(|value| !value.is_string()) {
        anyhow::bail!("`workspaces` holds a non-string entry — refusing to rewrite it");
    }
    let mut entries: Vec<PathBuf> = list
        .iter()
        .map(|value| PathBuf::from(value.as_str().expect("checked string")))
        .collect();
    for workspace in added {
        if !entries.contains(workspace) {
            entries.push(workspace.clone());
        }
    }
    entries.retain(|entry| !dropped.contains(entry));
    entries.sort();
    entries.dedup();
    *list = entries
        .into_iter()
        .map(|entry| serde_json::Value::from(entry.display().to_string()))
        .collect();
    std::fs::write(config, serde_json::to_string_pretty(&root)? + "\n")?;
    Ok(())
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
        assert_eq!(prune_at(&reg).unwrap().len(), 1); // 只清死条目
        assert_eq!(read_entries(&dir), Vec::<String>::new());
    }

    fn bridge_config(workspaces_json: &str) -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("t3-bridge.json"),
            format!("{{\"receiveTimeoutSec\": 600,\n\"workspaces\": {workspaces_json}}}"),
        )
        .unwrap();
        dir
    }
    fn bridge_entries(dir: &tempfile::TempDir) -> Vec<String> {
        let root: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("t3-bridge.json")).unwrap(),
        )
        .unwrap();
        root["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn bridge_mirror_maintains_only_pinned_lists() {
        let dir = bridge_config("[\"/ws/b\", \"/ws/a\"]");
        let config = dir.path().join("t3-bridge.json");
        // 新增幂等 + 排序稳定，其余字段原样保留
        mirror_bridge_at(&config, &[PathBuf::from("/ws/c")], &[]).unwrap();
        assert_eq!(bridge_entries(&dir), vec!["/ws/a", "/ws/b", "/ws/c"]);
        mirror_bridge_at(&config, &[PathBuf::from("/ws/c")], &[]).unwrap();
        assert_eq!(bridge_entries(&dir), vec!["/ws/a", "/ws/b", "/ws/c"]);
        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(root["receiveTimeoutSec"], 600);
        // 移除：只掉目标；未在册的移除也无害
        mirror_bridge_at(
            &config,
            &[],
            &[PathBuf::from("/ws/a"), PathBuf::from("/nope")],
        )
        .unwrap();
        assert_eq!(bridge_entries(&dir), vec!["/ws/b", "/ws/c"]);
        // 跟随模式（null/缺键）拒绝改写且文件原样——现在是静默 Ok
        let follow = bridge_config("null");
        let follow_config = follow.path().join("t3-bridge.json");
        let before = std::fs::read_to_string(&follow_config).unwrap();
        mirror_bridge_at(&follow_config, &[PathBuf::from("/ws/x")], &[]).unwrap();
        assert_eq!(std::fs::read_to_string(&follow_config).unwrap(), before);
        // 非数组（字符串等畸形）仍拒绝改写
        let weird = bridge_config("\"/ws/only\"");
        let weird_config = weird.path().join("t3-bridge.json");
        let before = std::fs::read_to_string(&weird_config).unwrap();
        assert!(mirror_bridge_at(&weird_config, &[PathBuf::from("/ws/x")], &[]).is_err());
        assert_eq!(std::fs::read_to_string(&weird_config).unwrap(), before);
    }
}
