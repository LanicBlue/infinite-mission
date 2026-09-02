//! Installs the /im slash command for supported AI CLI tools. One logical
//! body, three formats (codex skill, claude/opencode md, gemini toml).

use anyhow::Result;
use std::path::{Path, PathBuf};

pub struct Platform {
    pub name: &'static str,
    pub binary: &'static str,
    pub command_path: &'static str,
    pub format: Format,
}

#[derive(PartialEq, Clone, Copy)]
pub enum Format {
    Codex,
    Markdown,
    Toml,
}

pub const PLATFORMS: &[Platform] = &[
    Platform { name: "claude", binary: "claude", command_path: ".claude/commands/im.md", format: Format::Markdown },
    Platform { name: "gemini", binary: "gemini", command_path: ".gemini/commands/im.toml", format: Format::Toml },
    Platform { name: "codex", binary: "codex", command_path: ".codex/skills/im/SKILL.md", format: Format::Codex },
    Platform { name: "opencode", binary: "opencode", command_path: ".config/opencode/commands/im.md", format: Format::Markdown },
];

const VERSION_MARKER: &str = "im-version:";

fn current_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

pub fn is_installed(binary: &str) -> bool {
    std::env::var("PATH")
        .map(|path| {
            std::env::split_paths(&path).any(|dir| {
                dir.join(binary).exists() || dir.join(format!("{binary}.exe")).exists()
            })
        })
        .unwrap_or(false)
}

const PHASE_BODY: &str = r#"## Phase 1: Setup (once)

1. Parse your join arguments (stated above). First word = your role (any string); optional second word = a custom id. If empty, ask the user which role. Do NOT treat the arguments as a CLI subcommand.
2. Run `im init` (safe to re-run).
3. Run `im agents`. If ALL agents are stale, ask the user whether to `im clean` + `im init` — never clean automatically.
4. Run `im join <id> --role <role>`. The output line "Joined as ..." is your actual id (auto-suffixed on conflict). Follow the printed role instructions.
5. Run `im agents` to see the team.
6. If any command says "Session replaced": re-join under a suffixed id.

## Phase 2: Enter Work Mode (MANDATORY)

**Immediately after setup, run `im receive <your-id> --wait`.** Do not wait for the user.

Missions are one-shot mail: they carry their own contract, travel between stations you guard, and record their own delivery history. You are stateless — everything lives in the mission.

**If your role executes work (worker, reviewer, inspector...):**
1. `im missions <your-id>` — active missions at your stations.
2. `im mission show <ms> --for <your-id>` — prompt, document rights (read/write are separate!), your outcome vocabulary, the routes table, and the revision.
3. Persist results: `im mission doc write <your-id> <ms> --id <docId> --file <path-or->` → prints a receipt `document:<hash>`.
4. Submit: `im mission submit <your-id> <ms> --revision <N> --outcome <outcome> [--next-node <station>] [--reason <text>] [--feedback <text>] [--receipts <document:hash,...>]`
   - The revision comes from the run view; a stale revision tells you the current one — re-read and retry.
   - Routes with 2+ targets REQUIRE --next-node. feedbackRequiredOn outcomes REQUIRE --feedback.
   - `--outcome abandon` is always legal when stuck.
5. `im missions <your-id>` again; when empty, `im receive <your-id> --wait`.

**If your role operates the fleet (manager, lead...):**
1. "not an operator" → ask the human to run `im grant <your-id>`.
2. `im project create <you> <name>` / `im work create <you> <project> <work-key> [--executor <agent>] [--prompt <text>]`
3. Templates in .im/templates/*.yaml (im init writes example.yaml); then `im mission create <you> --project <p> --template <name> --key <unique-key>`
4. `im work list` / `im mission events <ms>` / `im inbox` (user stations waiting for humans).

After acting, run `im receive <your-id>` again; when idle use `im receive <your-id> --wait`.
"#;

const CODEX_BODY: &str = r#"---
name: im
description: "Join InfiniteMission multi-agent mission orchestration. Usage: $im <role> [custom-id]"
---

You are joining an InfiniteMission workspace.

Your join arguments: $ARGUMENTS
"#;

const MD_BODY: &str = r#"---
description: Join InfiniteMission multi-agent mission orchestration. Usage: /im <role> [custom-id]
---

You are joining an InfiniteMission workspace.

Your join arguments: $ARGUMENTS
"#;

const TOML_BODY: &str = r#"description = "Join InfiniteMission multi-agent mission orchestration. Usage: /im <role> [custom-id]"

prompt = """
You are joining an InfiniteMission workspace.

The user's input: {{args}}
"#;

fn command_content(format: Format) -> String {
    let version = current_version();
    match format {
        Format::Codex => format!(
            "<!-- {VERSION_MARKER} {version} -->\n{CODEX_BODY}\n{PHASE_BODY}"
        ),
        Format::Markdown => format!(
            "<!-- {VERSION_MARKER} {version} -->\n{MD_BODY}\n{PHASE_BODY}"
        ),
        Format::Toml => format!(
            "# {VERSION_MARKER} {version}\n{TOML_BODY}\n{PHASE_BODY}\n\"\"\"\n"
        ),
    }
}

pub fn cmd(target: Option<&str>) -> Result<()> {
    match target {
        Some("--list") => {
            println!("Supported platforms:");
            for platform in PLATFORMS {
                let status = if is_installed(platform.binary) { "installed" } else { "not found" };
                println!("  {} ({}: {status})", platform.name, platform.binary);
            }
            Ok(())
        }
        _ => {
            println!("Detecting installed AI tools...");
            let mut installed = 0;
            for platform in PLATFORMS {
                if !is_installed(platform.binary) {
                    continue;
                }
                if let Some(name) = target {
                    if name != platform.name {
                        continue;
                    }
                }
                let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
                let path = home.join(platform.command_path);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&path, command_content(platform.format))?;
                println!("  {} → {}", platform.name, path.display());
                installed += 1;
            }
            if installed == 0 {
                println!("No supported AI tools found.");
            } else {
                println!("Installed /im for {installed} tool(s).");
            }
            Ok(())
        }
    }
}

pub fn cleanup_commands() -> Vec<(String, PathBuf)> {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let mut removed = Vec::new();
    for platform in PLATFORMS {
        let path = home.join(platform.command_path);
        if path.exists() {
            if std::fs::remove_file(&path).is_ok() {
                removed.push((platform.name.to_string(), path));
            }
        }
    }
    removed
}

pub fn diagnose(home: &Path) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    let mut any = false;
    for platform in PLATFORMS {
        if !is_installed(platform.binary) {
            continue;
        }
        any = true;
        let path = home.join(platform.command_path);
        if !path.exists() {
            lines.push(format!(
                "WARN: {name} is installed but /im is not — run im setup",
                name = platform.name
            ));
            continue;
        }
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let current = format!("{VERSION_MARKER} {}", current_version());
        if content.contains(&current) {
            lines.push(format!("OK: {name} /im template is current", name = platform.name));
        } else if content.contains(VERSION_MARKER) {
            lines.push(format!(
                "WARN: {name} /im template is outdated — run im setup",
                name = platform.name
            ));
        } else {
            lines.push(format!(
                "WARN: {name} command file exists without an im-version marker — run im setup",
                name = platform.name
            ));
        }
    }
    if !any {
        lines.push("OK: no supported AI tools detected".to_string());
    }
    Ok(lines)
}
