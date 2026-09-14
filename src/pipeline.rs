//! The delivery pipeline: work-prompt presets seeded by `im init` plus the
//! pipeline mission template. The prompts distill the discipline of
//! matt-skills-with-to-goal (grilling / to-spec / to-goal / spec-executor /
//! code-review) into standing station charters; the routing semantics live
//! entirely in the template — this module adds no new domain mechanics.

use anyhow::Result;

pub const PIPELINE_TEMPLATE: &str = include_str!("templates/pipeline.yaml");

pub struct WorkPreset {
    pub key: &'static str,
    /// One-line charter summary for boards and listings.
    pub description: &'static str,
    pub prompt: &'static str,
}

pub const PRESETS: &[WorkPreset] = &[
    WorkPreset {
        key: "design",
        description: "Grill the conversation into a frozen SPEC; hold the final gate.",
        prompt: include_str!("templates/pipeline/design.md"),
    },
    WorkPreset {
        key: "plan",
        description: "Compile the SPEC into a self-contained GOAL.",
        prompt: include_str!("templates/pipeline/plan.md"),
    },
    WorkPreset {
        key: "build",
        description: "Implement the GOAL; report with an evidence receipt.",
        prompt: include_str!("templates/pipeline/build.md"),
    },
    WorkPreset {
        key: "review",
        description: "Verify the implementation against the GOAL, two evidence axes.",
        prompt: include_str!("templates/pipeline/review.md"),
    },
    // The dev-line station charters. Deliberately mission-template-agnostic:
    // charters carry the station's duty and discipline (source-receipt
    // discipline, ACn accounting, arrival vocabulary comes from each
    // mission's own show) — they never hard-code one pipeline's outcome
    // vocabulary, so any flow contract can route through these stations.
    WorkPreset {
        key: "dev-design",
        description: "对齐用户、冻结 spec(ACn 编号)、终验意图符合性(唯一用户触点,不写代码)",
        prompt: include_str!("templates/dev-stations/design.md"),
    },
    WorkPreset {
        key: "dev-supervisor",
        description: "实施主管两道关:攻方案编译 plan、证据复核放行(独立判断不重复验证)",
        prompt: include_str!("templates/dev-stations/supervisor.md"),
    },
    WorkPreset {
        key: "dev-build",
        description: "通用实现+自测+改动面自报(唯一写入者,每轮 commit+impl@<HEAD短hash> 凭据)",
        prompt: include_str!("templates/dev-stations/build.md"),
    },
    WorkPreset {
        key: "dev-build-ui",
        description: "UI 实现(按 plan,每轮 commit+impl@<HEAD短hash> 凭据,混合任务接力 build)",
        prompt: include_str!("templates/dev-stations/build-ui.md"),
    },
    WorkPreset {
        key: "dev-review-impl",
        description: "功能正确性+测试完整性/退役审查(只读,可并发 ask)",
        prompt: include_str!("templates/dev-stations/review-impl.md"),
    },
    WorkPreset {
        key: "dev-review-impact",
        description: "边界影响与范围符合性审查(只读,可并发 ask)",
        prompt: include_str!("templates/dev-stations/review-impact.md"),
    },
    WorkPreset {
        key: "dev-sec-review",
        description: "安全审查,攻击者视角(只读,触发制)",
        prompt: include_str!("templates/dev-stations/sec-review.md"),
    },
    WorkPreset {
        key: "dev-review-audit",
        description: "审查环编排者+净化站:并发派发三审查、三问核对、决定打回(主 mission 链,只读)",
        prompt: include_str!("templates/dev-stations/review-audit.md"),
    },
    WorkPreset {
        key: "dev-verify",
        description: "必跑验证站:AC 探针+全套件→evidence@HEAD(起跑对账源码凭据,只读执行)",
        prompt: include_str!("templates/dev-stations/verify.md"),
    },
    WorkPreset {
        key: "dev-verify-ui",
        description: "UI 验证站:verify 之后串行,交互+可视化证据(只读执行)",
        prompt: include_str!("templates/dev-stations/verify-ui.md"),
    },
];

pub fn preset(key: &str) -> Option<&'static WorkPreset> {
    PRESETS.iter().find(|p| p.key == key)
}

pub fn preset_keys() -> String {
    PRESETS.iter().map(|p| p.key).collect::<Vec<_>>().join(", ")
}

/// Seed the pipeline stations into a workspace. `im init` runs before any
/// manager exists, so this writes the works table directly (works carry no
/// author). Existing stations are never clobbered; a deleted pipeline
/// station is simply re-seeded on the next `im init`.
pub fn seed_pipeline_works(store: &crate::store::Store) -> Result<Vec<String>> {
    use rusqlite::OptionalExtension;

    let mut notes = Vec::new();
    for preset in PRESETS {
        let key = preset.key;
        let exists: Option<i64> = store
            .conn
            .query_row("SELECT 1 FROM works WHERE work_key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?;
        if exists.is_none() {
            store.conn.execute(
                "INSERT INTO works (work_key, description, executor, prompt, created_at)
                 VALUES (?1, ?2, NULL, ?3, ?4)",
                rusqlite::params![
                    key,
                    preset.description,
                    preset.prompt,
                    chrono::Utc::now().timestamp()
                ],
            )?;
            notes.push(format!("created station {key}"));
        }
    }
    Ok(notes)
}
