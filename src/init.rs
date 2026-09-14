use anyhow::Result;
use std::path::Path;

pub const EXAMPLE_TEMPLATE: &str = include_str!("templates/example.yaml");
pub const DEV_GENERAL_TEMPLATE: &str = include_str!("templates/dev-general.yaml");
pub const DEV_UI_TEMPLATE: &str = include_str!("templates/dev-ui.yaml");
pub const DEV_MIXED_TEMPLATE: &str = include_str!("templates/dev-mixed.yaml");

/// Built-in templates every fresh workspace starts with: the `example`
/// starter plus the three delivery-line contracts (general / UI / mixed).
/// Templates are inert until referenced — stations are created by the
/// workspace's owners, never seeded here.
const BUILTIN_TEMPLATES: [(&str, &str); 4] = [
    ("example.yaml", EXAMPLE_TEMPLATE),
    ("dev-general.yaml", DEV_GENERAL_TEMPLATE),
    ("dev-ui.yaml", DEV_UI_TEMPLATE),
    ("dev-mixed.yaml", DEV_MIXED_TEMPLATE),
];

pub fn run() -> Result<()> {
    let workspace = std::env::current_dir()?;
    let dot = workspace.join(".im");

    std::fs::create_dir_all(dot.join("sessions"))?;
    std::fs::create_dir_all(dot.join("templates"))?;
    std::fs::create_dir_all(dot.join("mission-documents"))?;

    for (file, template) in BUILTIN_TEMPLATES {
        let path = dot.join("templates").join(file);
        if !path.exists() {
            std::fs::write(&path, template)?;
        }
    }
    // Live station-charter presets: files are the runtime source of truth
    // (CLI and console read them on every use — edits need no restarts).
    crate::pipeline::seed_preset_files(&dot.join("presets"))?;

    let store = crate::store::Store::open(&dot.join("im.db"))?;
    drop(store);

    crate::registry::register(&workspace)?;
    append_if_missing(&workspace.join(".gitignore"), ".im/")?;
    for guide in ["CLAUDE.md", "AGENTS.md", "GEMINI.md"] {
        append_if_missing(
            &workspace.join(guide),
            "## InfiniteMission\n\nThis project uses `im` for multi-agent mission orchestration. Run `im help` for the full guide.\n",
        )?;
    }
    println!("Initialized InfiniteMission workspace at {}", dot.display());
    println!("  - templates:    .im/templates/ (example, dev-general, dev-ui, dev-mixed)");
    println!("  - presets:      .im/presets/ (station charters, editable — no restart needed)");
    println!("  - documents:    .im/mission-documents/");
    println!(
        "  - next:         `im join <id>`, create stations with `im work create` (a manager \
         seeds them via `im work set-prompt`), then `im mission create --template <name>`"
    );
    Ok(())
}

fn append_if_missing(path: &Path, text: &str) -> Result<()> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    if existing.contains(text.trim()) {
        return Ok(());
    }
    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push('\n');
    content.push_str(text);
    std::fs::write(path, content)?;
    Ok(())
}
