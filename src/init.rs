use anyhow::{Context, Result};
use std::path::Path;

pub const EXAMPLE_TEMPLATE: &str = include_str!("templates/example.yaml");
pub const DEV_GENERAL_TEMPLATE: &str = include_str!("templates/dev-general.yaml");
pub const DEV_UI_TEMPLATE: &str = include_str!("templates/dev-ui.yaml");
pub const DEV_MIXED_TEMPLATE: &str = include_str!("templates/dev-mixed.yaml");

/// Built-in templates every fresh workspace starts with: the `example`
/// starter plus the three delivery-line contracts (general / UI / mixed).
/// Templates are inert until referenced — stations are created by the
/// workspace's owners, never seeded here.
pub const BUILTIN_TEMPLATES: [(&str, &str); 4] = [
    ("example.yaml", EXAMPLE_TEMPLATE),
    ("dev-general.yaml", DEV_GENERAL_TEMPLATE),
    ("dev-ui.yaml", DEV_UI_TEMPLATE),
    ("dev-mixed.yaml", DEV_MIXED_TEMPLATE),
];

/// The user-level stock directory: `~/.im`. The installer refreshes
/// `~/.im/templates/` and `~/.im/presets/` from the compiled-in stock
/// (`im stock refresh`); `im init` then copies from there into the
/// workspace, so a user-curated stock is what new workspaces inherit.
pub fn stock_dir() -> Result<std::path::PathBuf> {
    let home = std::env::var_os("HOME").context("$HOME is not set")?;
    Ok(Path::new(&home).join(".im"))
}

pub fn run() -> Result<()> {
    let workspace = std::env::current_dir()?;
    let dot = workspace.join(".im");

    std::fs::create_dir_all(dot.join("sessions"))?;
    std::fs::create_dir_all(dot.join("templates"))?;
    std::fs::create_dir_all(dot.join("mission-documents"))?;
    std::fs::create_dir_all(dot.join("presets"))?;

    // Seed any missing stock file from the compiled-in stock (a never-
    // refreshed install still works), then copy stock → workspace. Missing
    // files only — an existing file (stock or workspace) is never clobbered.
    let stock = stock_dir()?;
    let stock_templates = stock.join("templates");
    let stock_presets = stock.join("presets");
    for (file, builtin) in BUILTIN_TEMPLATES {
        let stock_file = stock_templates.join(file);
        if !stock_file.exists() {
            std::fs::create_dir_all(&stock_templates)?;
            std::fs::write(&stock_file, builtin)?;
        }
        let path = dot.join("templates").join(file);
        if !path.exists() {
            std::fs::copy(&stock_file, &path)?;
        }
    }
    // Station-charter presets: live files are the runtime source of truth
    // (CLI and console read them on every use — edits need no restarts).
    crate::pipeline::seed_preset_files(&dot.join("presets"))?;
    std::fs::create_dir_all(&stock_presets)?;
    for entry in std::fs::read_dir(&stock_presets)? {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "md") {
            let name = path.file_name().context("stock preset without a name")?;
            let target = dot.join("presets").join(name);
            if !target.exists() {
                std::fs::copy(&path, &target)?;
            }
        }
    }

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
    println!("  - templates:    .im/templates/ (from ~/.im/templates/ — the user stock)");
    println!("  - presets:      .im/presets/ (from ~/.im/presets/ — editable, no restart needed)");
    println!("  - documents:    .im/mission-documents/");
    println!(
        "  - next:         `im join <id>`, create stations with `im work create --preset <key>`, then `im mission create --template <name>`"
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
