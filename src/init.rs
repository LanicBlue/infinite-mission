use anyhow::Result;
use std::path::Path;

pub const EXAMPLE_TEMPLATE: &str = include_str!("templates/example.yaml");

pub fn run() -> Result<()> {
    let workspace = std::env::current_dir()?;
    let dot = workspace.join(".im");

    std::fs::create_dir_all(dot.join("sessions"))?;
    std::fs::create_dir_all(dot.join("templates"))?;
    std::fs::create_dir_all(dot.join("mission-documents"))?;

    let example = dot.join("templates").join("example.yaml");
    if !example.exists() {
        std::fs::write(&example, EXAMPLE_TEMPLATE)?;
    }

    append_if_missing(&workspace.join(".gitignore"), ".im/")?;
    for guide in ["CLAUDE.md", "AGENTS.md", "GEMINI.md"] {
        append_if_missing(
            &workspace.join(guide),
            "## InfiniteMission\n\nThis project uses `im` for multi-agent mission orchestration. Run `im help` for the full guide.\n",
        )?;
    }
    println!("Initialized InfiniteMission workspace at {}", dot.display());
    println!("  - templates:    .im/templates/ (example.yaml is a starter mission template)");
    println!("  - documents:    .im/mission-documents/");
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
