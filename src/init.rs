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
    let pipeline = dot.join("templates").join("pipeline.yaml");
    if !pipeline.exists() {
        std::fs::write(&pipeline, crate::pipeline::PIPELINE_TEMPLATE)?;
    }

    // Seed the delivery-pipeline stations (design/plan/build/review + the
    // owner user station). Runs before any manager exists, so seeding writes
    // the works table directly; existing stations are never clobbered.
    let store = crate::store::Store::open(&dot.join("im.db"))?;
    let seeded = crate::pipeline::seed_pipeline_works(&store)?;

    append_if_missing(&workspace.join(".gitignore"), ".im/")?;
    for guide in ["CLAUDE.md", "AGENTS.md", "GEMINI.md"] {
        append_if_missing(
            &workspace.join(guide),
            "## InfiniteMission\n\nThis project uses `im` for multi-agent mission orchestration. Run `im help` for the full guide.\n",
        )?;
    }
    println!("Initialized InfiniteMission workspace at {}", dot.display());
    println!("  - templates:    .im/templates/ (example.yaml, pipeline.yaml)");
    println!("  - documents:    .im/mission-documents/");
    if seeded.is_empty() {
        println!("  - stations:     design/plan/build/review/owner already present (untouched)");
    } else {
        println!("  - stations:     {}", seeded.join(", "));
    }
    println!(
        "  - next:         bind executors — `im work set-executor <manager> <work> <agent>` \
         for design/plan/build/review (unbound stations are user stations: every hop \
         there waits for a manager)"
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
