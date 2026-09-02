use anyhow::{bail, Context, Result};
use fs2::FileExt;
use std::path::{Path, PathBuf};

const DEFAULT_WAIT_TIMEOUT_SECS: u64 = 3_600;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "help".to_string());

    match command.as_str() {
        "init" => cmd_init(args.collect()),
        "join" => {
            let id = args.next().unwrap_or_default();
            if id.is_empty() {
                bail!("Usage: im join <id> [--role <role>]");
            }
            let role = match args.next().as_deref() {
                Some("--role") => args
                    .next()
                    .context("--role requires a value")?,
                None => id.clone(),
                Some(other) => bail!("unknown join flag: {other}"),
            };
            cmd_join(&id, &role)
        }
        "leave" => {
            let id = args.next().unwrap_or_default();
            if id.is_empty() {
                bail!("Usage: im leave <id>");
            }
            cmd_leave(&id)
        }
        "agents" => {
            let show_all = args.next().as_deref() == Some("--all");
            cmd_agents(show_all)
        }
        "roles" => cmd_roles(),
        "send" => cmd_send(args.collect()),
        "receive" => cmd_receive(args.collect()),
        "pending" => cmd_pending(),
        "history" => cmd_history(args.collect()),
        "grant" | "revoke" => {
            let id = args.next().unwrap_or_default();
            if id.is_empty() {
                bail!("Usage: im {command} <agent-id>");
            }
            cmd_grant(&command, &id)
        }
        "operators" => cmd_operators(),
        "work" => cmd_work(args.collect()),
        "template" => cmd_template(args.collect()),
        "mission" => cmd_mission(args.collect()),
        "missions" => {
            let id = args.next().unwrap_or_default();
            if id.is_empty() {
                bail!("Usage: im missions <agent>");
            }
            cmd_missions_for(&id)
        }
        "inbox" => cmd_inbox(),
        "doctor" => cmd_doctor(),
        "setup" => im::setup::cmd(args.next().as_deref()),
        "clean" => cmd_clean(),
        "ui" => cmd_ui(args.collect()),
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(())
        }
        "--version" | "-V" => {
            println!("im {} (infinite-mission)", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        // Unknown commands are treated as role-based joins: `im cto` = `im join cto --role cto`
        other => cmd_join(other, other),
    }
}

fn print_usage() {
    print!("{}", HELP_TEXT);
}

// --- Workspace helpers ---

fn find_workspace() -> Result<PathBuf> {
    let mut dir = std::env::current_dir()?;
    loop {
        if dir.join(".im").exists() {
            return Ok(dir);
        }
        if !dir.pop() {
            bail!("Not an InfiniteMission workspace. Run 'im init' first.");
        }
    }
}

fn open_store(workspace: &Path) -> Result<im::store::Store> {
    im::store::Store::open(&workspace.join(".im").join("im.db"))
}

fn documents_root(workspace: &Path) -> PathBuf {
    workspace.join(".im").join("mission-documents")
}

fn templates_dir(workspace: &Path) -> PathBuf {
    workspace.join(".im").join("templates")
}

fn sessions_dir(workspace: &Path) -> PathBuf {
    workspace.join(".im").join("sessions")
}

fn ensure_agent(store: &im::store::Store, id: &str) -> Result<()> {
    store.require_active_agent(id)
}

/// Displacement detection: the joining session file must match the DB token.
/// In a shared workspace this is a coherence signal, not an authenticator.
fn check_session(workspace: &Path, store: &im::store::Store, agent_id: &str) -> Result<()> {
    store.require_active_agent(agent_id)?;
    let expected = store.get_session_token(agent_id)?;
    let current = std::fs::read_to_string(sessions_dir(workspace).join(agent_id)).ok();
    if let (Some(expected), Some(current)) = (expected.as_deref(), current.as_deref()) {
        if expected != current {
            bail!(
                "Session replaced. Another terminal joined as {agent_id}. \
                 Re-join with a different id (e.g. im join {agent_id}-2 --role <role>)."
            );
        }
    }
    Ok(())
}

// --- Basics ---

fn cmd_init(args: Vec<String>) -> Result<()> {
    let refresh = matches!(args.first().map(String::as_str), Some("--refresh-roles"));
    if !args.is_empty() && !refresh {
        bail!("Usage: im init [--refresh-roles]");
    }
    im::init::run(refresh)
}

fn cmd_join(id: &str, role: &str) -> Result<()> {
    let workspace = find_workspace()?;
    let store = open_store(&workspace)?;
    let (actual_id, token) = store.register_agent_unique(id, role)?;
    store.touch_agent(&actual_id)?;
    std::fs::create_dir_all(sessions_dir(&workspace))?;
    std::fs::write(sessions_dir(&workspace).join(&actual_id), &token)?;
    if actual_id != id {
        println!("Id '{id}' was taken. Joined as {actual_id} (role: {role}).");
    } else {
        println!("Joined as {actual_id} (role: {role}).");
    }
    match im::roles::load_role(&workspace, role) {
        Ok(prompt) => println!("\n=== Role Instructions ===\n{prompt}"),
        Err(_) => {
            println!("\nNo role file for \"{role}\". Interpret the role autonomously.");
            println!("Known roles: {}", im::roles::list_roles(&workspace).join(", "));
        }
    }
    Ok(())
}

fn cmd_leave(id: &str) -> Result<()> {
    let workspace = find_workspace()?;
    let store = open_store(&workspace)?;
    store.unregister_agent(id)?;
    let session_file = sessions_dir(&workspace).join(id);
    let _ = std::fs::remove_file(&session_file);
    println!("{id} archived. Unread messages were preserved.");
    let stations = store.works_for_executor(id)?;
    if !stations.is_empty() {
        let listing: Vec<String> = stations
            .iter()
            .map(|work| work.work_key.clone())
            .collect();
        println!(
            "Note: {id} still guards {} station(s): {}. An operator should rebind them with \
             `im work set-executor <op> <work-key> <agent>` — missions stay put.",
            listing.len(),
            listing.join(", ")
        );
    }
    Ok(())
}

fn cmd_agents(show_all: bool) -> Result<()> {
    let workspace = find_workspace()?;
    let store = open_store(&workspace)?;
    let operators: std::collections::BTreeSet<String> =
        store.list_operators()?.into_iter().collect();
    let agents = store.list_agents(show_all)?;
    if agents.is_empty() {
        println!("No agents online.");
        return Ok(());
    }
    let now_ts = chrono::Utc::now().timestamp();
    for agent in &agents {
        let status = if agent.status == "archived" {
            "archived".to_string()
        } else {
            match agent.last_seen {
                Some(ts) => {
                    let ago = now_ts - ts;
                    if ago < 60 {
                        format!("active ({}s ago)", ago)
                    } else if ago < 600 {
                        format!("idle ({}m ago)", ago / 60)
                    } else {
                        format!("stale ({}m ago)", ago / 60)
                    }
                }
                None => "unknown".to_string(),
            }
        };
        let operator_tag = if operators.contains(&agent.id) { " [operator]" } else { "" };
        println!("  {} (role: {}) — {status}{operator_tag}", agent.id, agent.role);
    }
    Ok(())
}

fn cmd_roles() -> Result<()> {
    let workspace = find_workspace()?;
    let roles = im::roles::list_roles(&workspace);
    if roles.is_empty() {
        println!("No roles found. Built-ins land via `im init [--refresh-roles]`.");
    } else {
        println!("{}", roles.join(", "));
    }
    Ok(())
}

// --- Messaging ---

struct SendOptions {
    from: String,
    to: String,
    message: String,
    reply_to: Option<i64>,
}

fn cmd_send(args: Vec<String>) -> Result<()> {
    let mut reply_to = None;
    let mut file_path = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--reply-to" => {
                let value = args.get(i + 1).context("--reply-to requires a message id")?;
                reply_to = Some(
                    value
                        .parse()
                        .with_context(|| format!("invalid --reply-to value: {value}"))?,
                );
                i += 2;
            }
            "--file" => {
                file_path = Some(args.get(i + 1).context("--file requires a path or -")?.clone());
                i += 2;
            }
            _ => break,
        }
    }
    let remaining = &args[i..];
    let usage = "Usage: im send [--reply-to <message-id>] [--file <path-or->] <from> <to> <message>";
    let message = if let Some(path) = file_path {
        if remaining.len() != 2 {
            bail!("{usage}");
        }
        if path == "-" {
            use std::io::Read;
            let mut content = String::new();
            std::io::stdin().read_to_string(&mut content)?;
            content
        } else {
            std::fs::read_to_string(&path).with_context(|| format!("failed to read {path}"))?
        }
    } else {
        if remaining.len() < 3 {
            bail!("{usage}");
        }
        remaining[2..].join(" ")
    };
    if message.trim().is_empty() {
        bail!("message content is empty");
    }
    let options = SendOptions {
        from: remaining[0].clone(),
        to: remaining[1].clone(),
        message,
        reply_to,
    };

    let workspace = find_workspace()?;
    let store = open_store(&workspace)?;
    ensure_agent(&store, &options.from)?;
    check_session(&workspace, &store, &options.from)?;
    store.touch_agent(&options.from)?;
    if options.to == "@all" {
        if options.reply_to.is_some() {
            bail!("reply metadata is only supported for direct messages");
        }
        let recipients = store.broadcast_message(&options.from, &options.message)?;
        println!("Broadcast to {} agents: {}", recipients.len(), recipients.join(", "));
    } else {
        store.send_message_checked(&options.from, &options.to, &options.message)?;
        println!("Sent to {}.", options.to);
    }
    Ok(())
}

fn cmd_receive(args: Vec<String>) -> Result<()> {
    let mut wait = false;
    let mut timeout_secs = DEFAULT_WAIT_TIMEOUT_SECS;
    let mut i = 1;
    let id = args.first().cloned().unwrap_or_default();
    if id.is_empty() {
        bail!("Usage: im receive <id> [--wait [--timeout <secs>]]");
    }
    while i < args.len() {
        match args[i].as_str() {
            "--wait" => wait = true,
            "--timeout" => {
                let value = args.get(i + 1).context("--timeout requires a value")?;
                timeout_secs = value
                    .parse()
                    .with_context(|| format!("invalid --timeout value: {value}"))?;
                if !wait {
                    bail!("--timeout requires --wait");
                }
                i += 1;
            }
            other => bail!("unknown receive flag: {other}"),
        }
        i += 1;
    }

    let workspace = find_workspace()?;
    {
        let store = open_store(&workspace)?;
        ensure_agent(&store, &id)?;
        check_session(&workspace, &store, &id)?;
        store.touch_agent(&id)?;
    }

    if wait {
        let lock_dir = workspace.join(".im").join("locks");
        std::fs::create_dir_all(&lock_dir)?;
        let lock_file = std::fs::File::create(lock_dir.join(format!("{id}.receive.lock")))?;
        if lock_file.try_lock_exclusive().is_err() {
            bail!(
                "Another `im receive {id} --wait` is already running. \
                 Use `im receive {id}` for a non-blocking check."
            );
        }
        let _guard = lock_file;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
        let mut last_heartbeat = std::time::Instant::now();
        loop {
            let store = open_store(&workspace)?;
            if last_heartbeat.elapsed() >= std::time::Duration::from_secs(30) {
                store.touch_agent(&id)?;
                last_heartbeat = std::time::Instant::now();
            }
            if store.has_unread_messages(&id)? || store.has_unread_work_notes(&id)? {
                let messages = store.receive_messages(&id)?;
                let notes = store.receive_work_notes(&id)?;
                if !messages.is_empty() || !notes.is_empty() {
                    print_messages(&messages, &id);
                    print_notes(&notes, &id);
                    return Ok(());
                }
            }
            if std::time::Instant::now() > deadline {
                println!("No new messages (timed out after {timeout_secs}s).");
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }

    let store = open_store(&workspace)?;
    let messages = store.receive_messages(&id)?;
    let notes = store.receive_work_notes(&id)?;
    if messages.is_empty() && notes.is_empty() {
        println!("No new messages. Run `im receive {id} --wait` to keep listening.");
    } else {
        print_messages(&messages, &id);
        print_notes(&notes, &id);
    }
    Ok(())
}

fn print_messages(messages: &[im::records::MessageRecord], receiver: &str) {
    for msg in messages {
        println!("[from {}] {}", msg.from_agent, msg.content);
        if msg.kind == "mission_ended" {
            if let Some(content) = msg.content.strip_prefix('[') {
                if let Some((mission_id, _)) = content.split_once("] ") {
                    println!("  → History: im mission events {mission_id}");
                }
            }
        }
        println!("  → Reply: im send {receiver} {} \"<your response>\"", msg.from_agent);
    }
}

fn print_notes(notes: &[im::records::WorkNoteRecord], receiver: &str) {
    for note in notes {
        println!("[station {}] {}", note.work_key, note.content);
        if let Some(mission_id) = &note.mission_id {
            println!(
                "  → Run: im mission show {mission_id} --for {receiver} (then im missions {receiver})"
            );
        }
    }
    if !notes.is_empty() {
        println!("  → After processing, run `im receive {receiver} --wait` to continue listening.");
    }
}

fn cmd_pending() -> Result<()> {
    let workspace = find_workspace()?;
    let store = open_store(&workspace)?;
    let messages = store.all_messages(None)?;
    let unread: Vec<_> = messages.iter().filter(|m| !m.read).collect();
    if unread.is_empty() {
        println!("No pending messages.");
        return Ok(());
    }
    for msg in unread {
        println!("  {} -> {}: {}", msg.from_agent, msg.to_agent, msg.content);
    }
    Ok(())
}

fn cmd_history(args: Vec<String>) -> Result<()> {
    let agent = args.first().cloned();
    let workspace = find_workspace()?;
    let store = open_store(&workspace)?;
    for msg in store.all_messages(agent.as_deref())? {
        let stamp = chrono::DateTime::from_timestamp(msg.created_at, 0)
            .map(|ts| ts.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| msg.created_at.to_string());
        let flag = if msg.read { "" } else { " (unread)" };
        println!("[{stamp}] {} -> {}:{flag} {}", msg.from_agent, msg.to_agent, msg.content);
    }
    Ok(())
}

// --- Operators / works / templates ---

fn cmd_grant(command: &str, id: &str) -> Result<()> {
    let workspace = find_workspace()?;
    let store = open_store(&workspace)?;
    if command == "grant" {
        store.grant_operator(id)?;
        println!("Granted operator permission to {id}.");
    } else {
        store.revoke_operator(id)?;
        println!("Revoked operator permission from {id}.");
    }
    Ok(())
}

fn cmd_operators() -> Result<()> {
    let workspace = find_workspace()?;
    let store = open_store(&workspace)?;
    let operators = store.list_operators()?;
    if operators.is_empty() {
        println!("No operators granted. Run `im grant <agent-id>` to grant one.");
    } else {
        println!("{}", operators.join("\n"));
    }
    Ok(())
}

fn flag_value(args: &[String], flag: &str) -> Result<Option<String>> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag {
            return Ok(Some(
                args.get(i + 1)
                    .with_context(|| format!("{flag} requires a value"))?
                    .clone(),
            ));
        }
        i += 1;
    }
    Ok(None)
}

fn cmd_work(args: Vec<String>) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("create") if args.len() >= 3 => {
            let operator = &args[1];
            let work_key = &args[2];
            let display_name = flag_value(&args[3..], "--display-name")?;
            let executor = flag_value(&args[3..], "--executor")?;
            let prompt = flag_value(&args[3..], "--prompt")?;
            let workspace = find_workspace()?;
            let store = open_store(&workspace)?;
            ensure_agent(&store, operator)?;
            check_session(&workspace, &store, operator)?;
            let key = store.create_work(
                operator,
                work_key,
                &display_name.unwrap_or_default(),
                executor.as_deref(),
                &prompt.unwrap_or_default(),
            )?;
            println!("Created station {key}");
            if executor.is_none() {
                println!("  (user station — bind an executor with im work set-executor)");
            }
            Ok(())
        }
        Some("list") => {
            let workspace = find_workspace()?;
            let store = open_store(&workspace)?;
            for work in store.list_works()? {
                let executor = work.executor.as_deref().unwrap_or("(user)");
                let lifecycle = if work.lifecycle == "retired" { " [retired]" } else { "" };
                let holding: i64 = store.conn.query_row(
                    "SELECT COUNT(*) FROM missions WHERE at = ?1 AND status = 'active'",
                    rusqlite::params![work.work_key],
                    |row| row.get(0),
                )?;
                println!(
                    "  {} — executor: {}, holding: {holding}{lifecycle}",
                    work.work_key, executor
                );
            }
            Ok(())
        }
        Some("set-executor") if args.len() == 4 => {
            let executor_arg = if args[3] == "-" { None } else { Some(args[3].as_str()) };
            let workspace = find_workspace()?;
            let store = open_store(&workspace)?;
            store.set_work_executor(&args[1], &args[2], executor_arg)?;
            println!(
                "Station {} executor → {}",
                args[2],
                executor_arg.unwrap_or("(user station)")
            );
            Ok(())
        }
        Some("set-prompt") if args.len() >= 4 => {
            let workspace = find_workspace()?;
            let store = open_store(&workspace)?;
            store.set_work_prompt(&args[1], &args[2], &args[3..].join(" "))?;
            println!("Station prompt updated.");
            Ok(())
        }
        Some("retire") if args.len() == 3 => {
            let workspace = find_workspace()?;
            let store = open_store(&workspace)?;
            store.retire_work(&args[1], &args[2])?;
            println!("Retired station {}.", args[2]);
            Ok(())
        }
        _ => bail!(
            "Usage: im work <create <op> <work-key> [--display-name <n>] [--executor <agent>] [--prompt <text>]\n             | list | set-executor <op> <work> <agent-or->\n             | set-prompt <op> <work> <text...> | retire <op> <work>>"
        ),
    }
}

fn cmd_template(args: Vec<String>) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("list") => {
            let workspace = find_workspace()?;
            let dir = templates_dir(&workspace);
            let mut names: Vec<String> = std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|entry| {
                    let path = entry.path();
                    if path.extension().map(|e| e == "yaml").unwrap_or(false) {
                        path.file_stem().and_then(|s| s.to_str()).map(String::from)
                    } else {
                        None
                    }
                })
                .collect();
            names.sort();
            if names.is_empty() {
                println!("No templates in .im/templates/. `im init` writes an example.");
            } else {
                println!("{}", names.join("\n"));
            }
            Ok(())
        }
        _ => bail!("Usage: im template list"),
    }
}

// --- Missions ---

fn cmd_mission(args: Vec<String>) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("create") => cmd_mission_create(&args[1..]),
        Some("show") if args.len() >= 2 => cmd_mission_show(&args[1], flag_value(&args[2..], "--for")?.as_deref()),
        Some("submit") => cmd_mission_submit(&args[1..]),
        Some("abandon") => cmd_mission_abandon(&args[1..]),
        Some("events") if args.len() == 2 => cmd_mission_events(&args[1]),
        Some("end") if args.len() >= 3 => cmd_mission_end(&args[1], &args[2], flag_value(&args[3..], "--reason")?),
        Some("doc") => cmd_mission_doc(&args[1..]),
        _ => bail!(
            "Usage: im mission <create <op> --template <name> --key <unique-key> [--name <n>] [--objective <o>]\n             | show <ms> [--for <agent>]\n             | submit <agent> <ms> --revision <N> --outcome <o> [--next-node <station>] [--reason <text>] [--feedback <text>] [--receipts <a,b>]\n             | abandon <agent> <ms> --revision <N> [--reason <text>]\n             | events <ms> | end <op> <ms> [--reason <text>]\n             | doc <read <agent> <ms> <path> | write <agent> <ms> --id <docId> --file <path-or->>"
        ),
    }
}

fn cmd_mission_create(args: &[String]) -> Result<()> {
    let operator = args
        .first()
        .context("Usage: im mission create <op> --template <name> --key <unique-key>")?;
    let template_name = flag_value(&args[1..], "--template")?.context("--template is required")?;
    let idem_key = flag_value(&args[1..], "--key")?.context("--key (idempotency key) is required")?;
    let name = flag_value(&args[1..], "--name")?;
    let objective = flag_value(&args[1..], "--objective")?;

    let workspace = find_workspace()?;
    let store = open_store(&workspace)?;
    ensure_agent(&store, operator)?;
    check_session(&workspace, &store, operator)?;

    let template_path = templates_dir(&workspace).join(format!("{template_name}.yaml"));
    let bytes = std::fs::read(&template_path)
        .with_context(|| format!("template '{template_name}' not found in .im/templates/"))?;
    let template = im::contract::parse_template(&String::from_utf8_lossy(&bytes))?;

    let outcome = store.create_mission(
        operator,
        &template,
        &format!(".im/templates/{template_name}.yaml"),
        &bytes,
        &idem_key,
        name.as_deref(),
        objective.as_deref(),
    )?;
    if outcome.existed {
        println!(
            "Mission {} already exists for this idempotency key (revision unchanged).",
            outcome.mission_id
        );
    } else {
        let mission = store.get_mission(&outcome.mission_id)?;
        println!("Created mission {} — started at station '{}'", outcome.mission_id, mission.at.unwrap_or_default());
        println!("  Next: im mission show {} --for <agent>", outcome.mission_id);
    }
    Ok(())
}

fn cmd_mission_show(mission_id: &str, for_agent: Option<&str>) -> Result<()> {
    let workspace = find_workspace()?;
    let store = open_store(&workspace)?;
    if let Some(agent) = for_agent {
        ensure_agent(&store, agent)?;
        let _ = agent;
    }
    let view = store.run_view(mission_id, for_agent)?;
    println!("[mission {}] {} — {}", view.mission_id, view.name, view.status);
    if !view.objective.is_empty() {
        println!("  objective: {}", view.objective);
    }
    match &view.at {
        Some(at) => println!("  at station: {at} (iteration {})", view.iteration.unwrap_or(1)),
        None => println!("  ended: {}", view.ended_note()),
    }
    println!("  revision: {}  ← submit must carry this", view.revision);
    if for_agent.is_some() {
        println!("  on duty: {}", if view.on_duty { "YES — you hold this station" } else { "no (another executor or user station)" });
    }
    if let Some(prompt) = &view.prompt {
        println!("  prompt: {prompt}");
    }
    if !view.outcomes.is_empty() {
        let mut permitted = view.outcomes.clone();
        permitted.push("abandon".to_string());
        println!("  outcomes: {}", permitted.join(", "));
    }
    if !view.terminal.is_empty() {
        println!("  terminal: {}", view.terminal.join(", "));
    }
    if !view.routes.is_empty() {
        println!("  routes:");
        for route in &view.routes {
            let to = if route.to.is_empty() {
                "(none)".to_string()
            } else {
                route.to.join(" | ")
            };
            let terminal = if route.terminal { " [terminal]" } else { "" };
            println!("    {} -> {to}{terminal}", route.outcome);
        }
    }
    if !view.documents.is_empty() {
        println!("  documents:");
        for doc in &view.documents {
            let rights = format!(
                "read:{} write:{}",
                if doc.may_read { "y" } else { "n" },
                if doc.may_write { "y" } else { "n" }
            );
            let receipt = doc
                .receipt
                .as_deref()
                .map(|r| format!(" receipt={r}"))
                .unwrap_or_default();
            println!("    {} ({}) {}{} [{}]", doc.id, doc.kind, doc.path, receipt, rights);
        }
    }
    Ok(())
}

fn cmd_mission_submit(args: &[String]) -> Result<()> {
    let agent = args
        .first()
        .context("Usage: im mission submit <agent> <ms> --revision <N> --outcome <o> [...]")?;
    let mission_id = args.get(1).context("mission id is required")?;
    let revision: i64 = flag_value(&args[2..], "--revision")?
        .context("--revision is required (take it from `im mission show`)")?
        .parse()
        .context("invalid --revision value")?;
    let outcome = flag_value(&args[2..], "--outcome")?.context("--outcome is required")?;
    let next_node = flag_value(&args[2..], "--next-node")?;
    let reason = flag_value(&args[2..], "--reason")?;
    let feedback = flag_value(&args[2..], "--feedback")?;
    let receipts: Vec<String> = flag_value(&args[2..], "--receipts")?
        .map(|raw| raw.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();

    let workspace = find_workspace()?;
    let store = open_store(&workspace)?;
    ensure_agent(&store, agent)?;
    check_session(&workspace, &store, agent)?;
    let outcome = store.submit_mission(
        agent,
        mission_id,
        revision,
        &outcome,
        next_node.as_deref(),
        reason.as_deref(),
        feedback.as_deref(),
        &receipts,
    )?;
    if outcome.mission_ended {
        println!("Mission {} ended (revision {}).", outcome.mission_id, outcome.revision);
    } else {
        println!(
            "Routed {} → {} (iteration {}, revision {}).",
            outcome.mission_id,
            outcome.routed_to.unwrap_or_default(),
            outcome.iteration_at_target.unwrap_or(1),
            outcome.revision
        );
    }
    Ok(())
}

fn cmd_mission_abandon(args: &[String]) -> Result<()> {
    let agent = args.first().context("Usage: im mission abandon <agent> <ms> --revision <N> [--reason <text>]")?;
    let mission_id = args.get(1).context("mission id is required")?;
    let revision: i64 = flag_value(&args[2..], "--revision")?
        .context("--revision is required")?
        .parse()
        .context("invalid --revision value")?;
    let reason = flag_value(&args[2..], "--reason")?;
    let workspace = find_workspace()?;
    let store = open_store(&workspace)?;
    ensure_agent(&store, agent)?;
    let outcome = store.submit_mission(
        agent,
        mission_id,
        revision,
        im::contract::ABANDON,
        None,
        reason.as_deref(),
        None,
        &[],
    )?;
    println!("Mission {} abandoned (revision {}).", outcome.mission_id, outcome.revision);
    Ok(())
}

fn cmd_mission_events(mission_id: &str) -> Result<()> {
    let workspace = find_workspace()?;
    let store = open_store(&workspace)?;
    for event in store.mission_events(mission_id)? {
        let stamp = chrono::DateTime::from_timestamp(event.created_at, 0)
            .map(|ts| ts.format("%m-%d %H:%M:%S").to_string())
            .unwrap_or_default();
        println!("#{:<3} {stamp} {}", event.seq, event.kind);
        println!("     {}", event.payload);
    }
    Ok(())
}

fn cmd_mission_end(operator: &str, mission_id: &str, reason: Option<String>) -> Result<()> {
    let workspace = find_workspace()?;
    let store = open_store(&workspace)?;
    store.delete_mission(operator, mission_id, reason.as_deref())?;
    println!("Mission {mission_id} ended by operator (disposition: deleted).");
    Ok(())
}

fn cmd_mission_doc(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("read") if args.len() == 4 => {
            let workspace = find_workspace()?;
            let store = open_store(&workspace)?;
            ensure_agent(&store, &args[1])?;
            check_session(&workspace, &store, &args[1])?;
            let content = store.read_mission_document(&args[1], &args[2], &args[3], &documents_root(&workspace))?;
            println!("{content}");
            Ok(())
        }
        Some("write") if args.len() >= 3 => {
            let document_id = flag_value(&args[2..], "--id")?.context("--id <documentId> is required")?;
            let file = flag_value(&args[2..], "--file")?.context("--file <path-or-> is required")?;
            let content: Vec<u8> = if file == "-" {
                use std::io::Read;
                let mut buffer = Vec::new();
                std::io::stdin().read_to_end(&mut buffer)?;
                buffer
            } else {
                std::fs::read(&file).with_context(|| format!("failed to read {file}"))?
            };
            let workspace = find_workspace()?;
            let store = open_store(&workspace)?;
            ensure_agent(&store, &args[1])?;
            check_session(&workspace, &store, &args[1])?;
            let receipt = store.write_mission_document(&args[1], &args[2], &document_id, &content, &documents_root(&workspace))?;
            println!("{receipt}");
            Ok(())
        }
        _ => bail!("Usage: im mission doc <read <agent> <ms> <path> | write <agent> <ms> --id <docId> --file <path-or->>"),
    }
}

fn cmd_missions_for(agent: &str) -> Result<()> {
    let workspace = find_workspace()?;
    let store = open_store(&workspace)?;
    ensure_agent(&store, agent)?;
    let missions = store.missions_for_executor(agent)?;
    if missions.is_empty() {
        println!("No active missions at your stations.");
        println!("  Waiting on: im receive {agent} --wait");
        return Ok(());
    }
    for mission in &missions {
        let iteration = store
            .standing_iteration(mission, mission.at.as_deref().unwrap_or_default())?
            .unwrap_or(1);
        println!(
            "[mission {}] at {} (iteration {}, revision {})",
            mission.mission_id,
            mission.at.as_deref().unwrap_or("?"),
            iteration,
            mission.revision
        );
        println!("  {}", mission.name);
        println!("  → im mission show {} --for {agent}", mission.mission_id);
    }
    Ok(())
}

fn cmd_inbox() -> Result<()> {
    let workspace = find_workspace()?;
    let store = open_store(&workspace)?;
    let entries = store.inbox_missions()?;
    if entries.is_empty() {
        println!("Inbox empty — no missions parked at user stations.");
        return Ok(());
    }
    for (mission, _station) in &entries {
        let reason = store
            .mission_events(&mission.mission_id)?
            .iter()
            .rev()
            .find_map(|event| {
                let payload: serde_json::Value = serde_json::from_str(&event.payload).ok()?;
                payload["reason"].as_str().map(String::from)
            })
            .unwrap_or_else(|| "(no reason given)".to_string());
        println!(
            "[mission {}] waiting at user station '{}' — {reason}",
            mission.mission_id,
            mission.at.as_deref().unwrap_or("?")
        );
        println!("  → im mission show {}", mission.mission_id);
        println!(
            "  → resolve it: im mission submit <operator> {} --revision {} --outcome <outcome>",
            mission.mission_id,
            mission.revision
        );
    }
    Ok(())
}

// --- Maintenance ---

fn cmd_doctor() -> Result<()> {
    let workspace = find_workspace()?;
    let store = open_store(&workspace)?;
    let home = PathBuf::from(std::env::var("HOME").context("HOME not set")?);
    for line in im::setup::diagnose(&home)? {
        println!("{line}");
    }

    // Archived agents still guarding stations.
    let stations = store.list_works()?;
    let archived: std::collections::BTreeSet<String> = store
        .list_agents(true)?
        .into_iter()
        .filter(|agent| agent.status == "archived")
        .map(|agent| agent.id)
        .collect();
    let orphaned: Vec<String> = stations
        .iter()
        .filter(|work| work.executor.as_ref().map(|e| archived.contains(e)).unwrap_or(false))
        .map(|work| format!("{} (held by {})", work.work_key, work.executor.clone().unwrap_or_default()))
        .collect();
    if orphaned.is_empty() {
        println!("OK: no archived agents guarding stations");
    } else {
        for entry in &orphaned {
            println!("WARN: archived executor still on duty: {entry} — rebind with im work set-executor");
        }
    }

    // Missions parked at retired stations.
    let retired: std::collections::BTreeSet<String> = stations
        .iter()
        .filter(|work| work.lifecycle == "retired")
        .map(|work| work.work_key.clone())
        .collect();
    let stranded = store.list_stranded_missions()?;
    if stranded.is_empty() {
        println!("OK: no active missions stranded at retired stations {}", if retired.is_empty() { String::new() } else { format!("(retired: {})", retired.len()) });
    } else {
        for mission_id in &stranded {
            println!("WARN: active mission {mission_id} sits at a retired station — route it or end it");
        }
    }
    Ok(())
}

fn cmd_clean() -> Result<()> {
    let workspace = find_workspace()?;
    let dot = workspace.join(".im");
    for name in ["im.db", "im.db-wal", "im.db-shm"] {
        let path = dot.join(name);
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
    }
    let sessions = dot.join("sessions");
    if sessions.exists() {
        for entry in std::fs::read_dir(&sessions)?.flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let notes_dir = dot.join("mission-documents");
    if notes_dir.exists() {
        std::fs::remove_dir_all(&notes_dir)?;
        std::fs::create_dir_all(&notes_dir)?;
    }
    println!("Cleaned InfiniteMission state (roles/templates kept).");
    Ok(())
}

fn cmd_ui(args: Vec<String>) -> Result<()> {
    let mut port: Option<u16> = None;
    let mut no_open = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                let value = args.get(i + 1).context("--port requires a value")?;
                port = Some(value.parse().with_context(|| format!("invalid --port value: {value}"))?);
                i += 2;
            }
            "--no-open" => no_open = true,
            other => bail!("unknown ui flag: {other}"),
        }
        i += 1;
    }
    im::ui::run(port, no_open)
}

const HELP_TEXT: &str = r#"im — InfiniteMission: multi-agent mission orchestration on a static core

No daemon, no server: every command is a one-shot process over `.im/im.db`.
A mission is one-shot mail — it carries its own contract, travels between
stations, and records its own delivery history.

COMMANDS
  im init [--refresh-roles]                 Initialize workspace (.im/)
  im join <id> [--role <role>]              Join as agent (role defaults to id)
  im leave <id>                             Archive agent
  im agents [--all]                         List agents ([operator] tags)
  im roles                                  List role files
  im send [--reply-to <id>] [--file <p-or->] <from> <to> <message>
                                             Send a note (@all broadcasts)
  im receive <id> [--wait] [--timeout N]    Check inbox + station arrival notes
  im pending / im history [agent]           Unread / full message views

Operators (humans run `im grant <agent-id>` once)
  im grant|revoke <agent> / im operators
  im work create <op> <work-key> [--display-name <n>] [--executor <agent>] [--prompt <text>]
  im work list / set-executor <op> <work> <agent-or-> / set-prompt <op> <work> <text> / retire <op> <work>
  im template list                          Mission templates in .im/templates/

Missions (PS semantics)
  im mission create <op> --template <name> --key <unique-key> [--name] [--objective]
  im mission show <ms> [--for <agent>]      Run view: prompt/rights/routes/revision
  im missions <agent>                       Active missions at your stations
  im mission submit <agent> <ms> --revision N --outcome <o>
       [--next-node <station>] [--reason <t>] [--feedback <t>] [--receipts <document:hash,...>]
  im mission abandon <agent> <ms> --revision N [--reason <t>]
  im mission events <ms>                    Delivery history
  im mission end <op> <ms> [--reason <t>]   Operator delete
  im mission doc read <agent> <ms> <path>
  im mission doc write <agent> <ms> --id <docId> --file <path-or->
  im inbox                                  Missions parked at user stations

Maintenance
  im setup [platform] / im doctor / im clean / im ui [--port N] [--no-open]

QUICK START
  1. im init && im join boss --role manager && im grant boss   (human grants once)
  2. im work create boss build --executor worker
     im work create boss review --executor inspector
  3. Write .im/templates/example-like.yaml, then:
     im mission create boss --template example-like --key v1
  5. Worker: im missions worker → im mission show <ms> --for worker
     → im mission doc write worker <ms> --id impl --file -
     → im mission submit worker <ms> --revision 1 --outcome done
  6. im inbox shows anything waiting for a human decision.
"#;
