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
    // The dev-line station charters. Deliberately mission-template-agnostic:
    // charters carry the station's duty and discipline (source-receipt
    // discipline, ACn accounting, arrival vocabulary comes from each
    // mission's own show) — they never hard-code one pipeline's outcome
    // vocabulary, so any flow contract can route through these stations.
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

pub fn preset(key: &str) -> Option<&'static WorkPreset> {
    PRESETS.iter().find(|p| p.key == key)
}

pub fn preset_keys() -> String {
    PRESETS.iter().map(|p| p.key).collect::<Vec<_>>().join(", ")
}
