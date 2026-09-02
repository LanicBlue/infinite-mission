use anyhow::Result;
use std::path::Path;

pub const BUILTIN_ROLES: &[&str] = &["manager", "worker", "inspector"];

pub fn role_prompt(role: &str) -> Option<String> {
    match role {
        "manager" => Some(include_str!("roles/manager.md").to_string()),
        "worker" => Some(include_str!("roles/worker.md").to_string()),
        "inspector" => Some(include_str!("roles/inspector.md").to_string()),
        _ => None,
    }
}

pub const EXAMPLE_TEMPLATE: &str = include_str!("templates/example.yaml");

pub fn run(refresh_roles: bool) -> Result<()> {
    let workspace = std::env::current_dir()?;
    let dot = workspace.join(".im");

    std::fs::create_dir_all(dot.join("roles"))?;
    std::fs::create_dir_all(dot.join("sessions"))?;
    std::fs::create_dir_all(dot.join("templates"))?;
    std::fs::create_dir_all(dot.join("mission-documents"))?;

    for role in BUILTIN_ROLES {
        let path = dot.join("roles").join(format!("{role}.md"));
        if refresh_roles || !path.exists() {
            if let Some(prompt) = role_prompt(role) {
                std::fs::write(&path, prompt)?;
            }
        }
    }

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
    println!("  - roles:        .im/roles/ (manager / worker / inspector)");
    println!("  - templates:    .im/templates/ (example.yaml is a starter mission template)");
    println!("  - documents:    .im/mission-documents/");
    println!("Next: `im setup` installs the /im slash command for your AI tools.");
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
