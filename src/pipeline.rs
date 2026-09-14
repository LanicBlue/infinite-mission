//! Station charter presets. The runtime source of truth is the workspace's
//! `.im/presets/*.md` files — one file per preset, a `description:` YAML
//! front-matter line followed by the charter body — so editing a file is
//! instantly live for every reader (CLI and console alike, no restarts).
//! The compiled-in table below is only the seed stock `im init` materializes
//! into a fresh workspace. The charters are deliberately
//! mission-template-agnostic: they carry the station's duty and discipline
//! (source-receipt discipline, ACn accounting) and never hard-code one
//! pipeline's outcome vocabulary — any flow contract can route through them.

use anyhow::Result;
use std::path::Path;

pub const PIPELINE_TEMPLATE: &str = include_str!("templates/pipeline.yaml");

pub struct WorkPreset {
    pub key: &'static str,
    /// One-line charter summary for boards and listings.
    pub description: &'static str,
    pub prompt: &'static str,
}

/// Seed stock only; the live preset list is read from `.im/presets/`.
pub const PRESETS: &[WorkPreset] = &[
    WorkPreset {
        key: "design",
        description: "对齐用户、冻结 spec(ACn 编号)、终验意图符合性(唯一用户触点,不写代码)",
        prompt: include_str!("templates/dev-stations/design.md"),
    },
    WorkPreset {
        key: "supervisor",
        description: "实施主管两道关:攻方案编译 plan、证据复核放行(独立判断不重复验证)",
        prompt: include_str!("templates/dev-stations/supervisor.md"),
    },
    WorkPreset {
        key: "build",
        description: "通用实现+自测+改动面自报(唯一写入者,每轮 commit+impl@<HEAD短hash> 凭据)",
        prompt: include_str!("templates/dev-stations/build.md"),
    },
    WorkPreset {
        key: "build-ui",
        description: "UI 实现(按 plan,每轮 commit+impl@<HEAD短hash> 凭据,混合任务接力 build)",
        prompt: include_str!("templates/dev-stations/build-ui.md"),
    },
    WorkPreset {
        key: "review-impl",
        description: "功能正确性+测试完整性/退役审查(只读,可并发 ask)",
        prompt: include_str!("templates/dev-stations/review-impl.md"),
    },
    WorkPreset {
        key: "review-impact",
        description: "边界影响与范围符合性审查(只读,可并发 ask)",
        prompt: include_str!("templates/dev-stations/review-impact.md"),
    },
    WorkPreset {
        key: "sec-review",
        description: "安全审查,攻击者视角(只读,触发制)",
        prompt: include_str!("templates/dev-stations/sec-review.md"),
    },
    WorkPreset {
        key: "review-audit",
        description: "审查环编排者+净化站:并发派发三审查、三问核对、决定打回(主 mission 链,只读)",
        prompt: include_str!("templates/dev-stations/review-audit.md"),
    },
    WorkPreset {
        key: "verify",
        description: "必跑验证站:AC 探针+全套件→evidence@HEAD(起跑对账源码凭据,只读执行)",
        prompt: include_str!("templates/dev-stations/verify.md"),
    },
    WorkPreset {
        key: "verify-ui",
        description: "UI 验证站:verify 之后串行,交互+可视化证据(只读执行)",
        prompt: include_str!("templates/dev-stations/verify-ui.md"),
    },
];

/// A preset as read from disk: owned strings, one per `.im/presets/*.md`.
#[derive(Clone)]
pub struct FilePreset {
    pub key: String,
    pub description: String,
    pub prompt: String,
}

/// Materialize the seed stock into a fresh workspace. Existing files are
/// never clobbered — an edited preset is the workspace's own from then on.
pub fn seed_preset_files(presets_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(presets_dir)?;
    for preset in PRESETS {
        let path = presets_dir.join(format!("{}.md", preset.key));
        if !path.exists() {
            std::fs::write(&path, format_preset(preset.description, preset.prompt))?;
        }
    }
    Ok(())
}

/// The on-disk format: one `description:` front-matter line, then the body.
pub fn format_preset(description: &str, prompt: &str) -> String {
    format!("---\ndescription: {description}\n---\n{prompt}\n")
}

/// Parse one preset file. Files without front-matter keep an empty
/// description; unparsable names are skipped by the caller, not guessed at.
fn parse_preset_file(path: &Path) -> Option<FilePreset> {
    let key = path.file_stem()?.to_str()?.to_string();
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let (description, prompt) = match text.strip_prefix("---\n") {
        Some(rest) => match rest.split_once("\n---\n") {
            Some((head, body)) => {
                let description = head
                    .lines()
                    .find_map(|line| line.strip_prefix("description:"))
                    .map(|d| d.trim().to_string())
                    .unwrap_or_default();
                (description, body.to_string())
            }
            None => (String::new(), text),
        },
        None => (String::new(), text),
    };
    Some(FilePreset {
        key,
        description,
        prompt: prompt.trim_end().to_string(),
    })
}


/// Overwrite the user-level stock (`~/.im/templates/`, `~/.im/presets/`)
/// with the compiled-in templates and charters. Run by the installer (and
/// manually after editing your own stock defaults is NOT advised — the next
/// refresh wins; long-lived customization belongs in workspace files).
/// Returns how many files were written.
pub fn stock_refresh(templates_dir: &Path, presets_dir: &Path) -> Result<usize> {
    use crate::init::BUILTIN_TEMPLATES;
    let mut written = 0;
    std::fs::create_dir_all(templates_dir)?;
    for (file, template) in BUILTIN_TEMPLATES {
        std::fs::write(templates_dir.join(file), template)?;
        written += 1;
    }
    std::fs::create_dir_all(presets_dir)?;
    for preset in PRESETS {
        std::fs::write(
            presets_dir.join(format!("{}.md", preset.key)),
            format_preset(preset.description, preset.prompt),
        )?;
        written += 1;
    }
    Ok(written)
}

/// Read the workspace's live presets, sorted by file name (the seed stock's
/// canonical order is alphabetical by design). A missing directory reads as
/// empty — callers surface the known-stocks hint, not a crash.
pub fn read_presets(presets_dir: &Path) -> Result<Vec<FilePreset>> {
    let mut presets = Vec::new();
    let entries = match std::fs::read_dir(presets_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(presets),
        Err(err) => return Err(err.into()),
    };
    let mut paths: Vec<_> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    paths.sort();
    for path in paths {
        if let Some(preset) = parse_preset_file(&path) {
            presets.push(preset);
        }
    }
    Ok(presets)
}
