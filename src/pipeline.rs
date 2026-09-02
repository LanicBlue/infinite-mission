//! The delivery pipeline: work-prompt presets seeded by `im init` plus the
//! pipeline mission template. The prompts distill the discipline of
//! matt-skills-with-to-goal (grilling / to-spec / to-goal / spec-executor /
//! code-review) into standing station charters; the routing semantics live
//! entirely in the template — this module adds no new domain mechanics.

use anyhow::Result;

pub const PIPELINE_TEMPLATE: &str = include_str!("templates/pipeline.yaml");

pub struct WorkPreset {
    pub key: &'static str,
    pub display_name: &'static str,
    pub prompt: &'static str,
}

pub const PRESETS: &[WorkPreset] = &[
    WorkPreset {
        key: "design",
        display_name: "Design",
        prompt: include_str!("templates/pipeline/design.md"),
    },
    WorkPreset {
        key: "plan",
        display_name: "Plan",
        prompt: include_str!("templates/pipeline/plan.md"),
    },
    WorkPreset {
        key: "build",
        display_name: "Build",
        prompt: include_str!("templates/pipeline/build.md"),
    },
    WorkPreset {
        key: "review",
        display_name: "Review",
        prompt: include_str!("templates/pipeline/review.md"),
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
/// author). Existing stations are never clobbered: active → untouched,
/// retired → reactivated (a retired pipeline key would otherwise make every
/// pipeline mission create fail — there is no user-facing unretire escape
/// hatch at init time), with the preset prompt filled only if the station's
/// prompt is empty.
pub fn seed_pipeline_works(store: &crate::store::Store) -> Result<Vec<String>> {
    use rusqlite::OptionalExtension;

    let mut notes = Vec::new();
    for preset in PRESETS {
        let key = preset.key;
        let existing: Option<(String, String)> = store
            .conn
            .query_row(
                "SELECT lifecycle, prompt FROM works WHERE work_key = ?1",
                [key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match existing {
            None => {
                store.conn.execute(
                    "INSERT INTO works (work_key, display_name, executor, prompt, lifecycle, created_at)
                     VALUES (?1, ?2, NULL, ?3, 'active', ?4)",
                    rusqlite::params![
                        key,
                        preset.display_name,
                        preset.prompt,
                        chrono::Utc::now().timestamp()
                    ],
                )?;
                notes.push(format!("created station {key}"));
            }
            Some((lifecycle, existing_prompt)) if lifecycle != "active" => {
                let fill_prompt = existing_prompt.trim().is_empty();
                store.conn.execute(
                    "UPDATE works SET lifecycle = 'active', prompt = CASE WHEN ?1 THEN ?2 ELSE prompt END
                     WHERE work_key = ?3",
                    rusqlite::params![fill_prompt, preset.prompt, key],
                )?;
                notes.push(format!("reactivated station {key}"));
            }
            Some(_) => {}
        }
    }
    Ok(notes)
}
