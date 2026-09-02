//! The control plane console: an ephemeral local web server over the same
//! static SQLite core. Binds 127.0.0.1 only, opens the browser, exits on
//! Ctrl-C or after 5 idle minutes — nothing to keep alive.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

const PAGE: &str = include_str!("web/page.html");
const IDLE_TIMEOUT_SECS: u64 = 5 * 60;

pub fn state_json(store: &crate::store::Store, workspace: &str, templates: &[String]) -> Result<Value> {
    let now_ts = chrono::Utc::now().timestamp();
    let managers = store.list_managers()?;
    let agents: Vec<Value> = store
        .list_agents(true)?
        .into_iter()
        .map(|agent| {
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
            json!({
                "id": agent.id,
                "role": agent.role,
                "status": status,
                "manager": managers.contains(&agent.id),
            })
        })
        .collect();

    let works: Vec<Value> = store
        .list_works()?
        .into_iter()
        .map(|work| {
            let holding: i64 = store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM missions WHERE at = ?1 AND status = 'active'",
                    rusqlite::params![work.work_key],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            json!({
                "work_key": work.work_key,
                "display_name": work.display_name,
                "executor": work.executor,
                "prompt": work.prompt,
                "lifecycle": work.lifecycle,
                "holding": holding,
            })
        })
        .collect();

    let missions: Vec<Value> = store
        .conn
        .prepare(
            "SELECT mission_id, name, objective, at, status, revision,
                    ended_disposition, created_at, created_by
             FROM missions ORDER BY created_at DESC",
        )?
        .query_map([], |row| {
            Ok(json!({
                "mission_id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "objective": row.get::<_, String>(2)?,
                "at": row.get::<_, Option<String>>(3)?,
                "status": row.get::<_, String>(4)?,
                "revision": row.get::<_, i64>(5)?,
                "ended_disposition": row.get::<_, Option<String>>(6)?,
                "created_at": row.get::<_, i64>(7)?,
                "created_by": row.get::<_, String>(8)?,
            }))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let inbox: Vec<Value> = store
        .inbox_missions()?
        .into_iter()
        .map(|(mission, _)| {
            // The hop reason is the human's decision context — surface it
            // (it rides on the round that performed the hop).
            let reason: Option<String> = store
                .conn
                .query_row(
                    "SELECT payload FROM mission_events
                     WHERE mission_id = ?1 AND type = 'mission.round.completed'
                     ORDER BY seq DESC LIMIT 1",
                    [&mission.mission_id],
                    |row| row.get::<_, String>(0),
                )
                .ok()
                .and_then(|payload| serde_json::from_str::<Value>(&payload).ok())
                .and_then(|v| v.get("reason").and_then(|r| r.as_str()).map(String::from));
            // When the mailbox arrived at this station (for aging display).
            let arrived_at: Option<i64> = store
                .conn
                .query_row(
                    "SELECT created_at FROM mission_events
                     WHERE mission_id = ?1 AND type = 'mission.routed'
                     ORDER BY seq DESC LIMIT 1",
                    [&mission.mission_id],
                    |row| row.get(0),
                )
                .ok();
            // The station's vocabulary so the human can resolve in place.
            let (outcomes, terminal): (Vec<String>, Vec<String>) =
                crate::mission::parse_contract(&mission.contract_json)
                    .ok()
                    .and_then(|contract| {
                        mission
                            .at
                            .as_deref()
                            .and_then(|at| contract.works.get(at))
                            .map(|d| {
                                (
                                    d.completion.outcomes.clone(),
                                    d.completion.terminal.clone(),
                                )
                            })
                    })
                    .unwrap_or_default();
            json!({
                "mission_id": mission.mission_id,
                "at": mission.at,
                "name": mission.name,
                "objective": mission.objective,
                "reason": reason,
                "arrived_at": arrived_at,
                "revision": mission.revision,
                "outcomes": outcomes,
                "terminal": terminal,
            })
        })
        .collect();

    let events: Vec<Value> = store
        .conn
        .prepare(
            "SELECT mission_id, seq, type, payload, created_at FROM mission_events
             ORDER BY rowid DESC LIMIT 100",
        )?
        .query_map([], |row| {
            Ok(json!({
                "mission_id": row.get::<_, String>(0)?,
                "seq": row.get::<_, i64>(1)?,
                "type": row.get::<_, String>(2)?,
                "payload": row.get::<_, String>(3)?,
                "created_at": row.get::<_, i64>(4)?,
            }))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(json!({
        "workspace": workspace,
        "managers": managers,
        "templates": templates,
        "agents": agents,
        "works": works,
        "missions": missions,
        "inbox": inbox,
        "events": events,
    }))
}

fn acting_manager(store: &crate::store::Store) -> Result<String> {
    match store.list_managers()?.into_iter().next() {
        Some(manager) => Ok(manager),
        None => bail!(
            "no manager is granted yet — run `im grant <agent-id>` in a terminal first, \
             then reload this page"
        ),
    }
}

fn apply_action(
    store: &crate::store::Store,
    action: &Value,
    workspace: &PathBuf,
) -> Result<String> {
    let kind = action["type"].as_str().context("action needs a `type`")?;
    match kind {
        "grant" | "revoke" => {
            let agent = action["agent"].as_str().context("`agent` required")?;
            if kind == "grant" {
                store.grant_manager(agent)?;
                Ok(format!("granted manager to {agent}"))
            } else {
                store.revoke_manager(agent)?;
                Ok(format!("revoked manager from {agent}"))
            }
        }
        "delete_agent" => {
            let agent = action["agent"].as_str().context("`agent` required")?;
            store.delete_agent(agent)?;
            let session_file = workspace.join(".im").join("sessions").join(agent);
            let _ = std::fs::remove_file(&session_file);
            Ok(format!("deleted member {agent}"))
        }
        "set_executor" => {
            let work = action["work"].as_str().context("`work` required")?;
            let executor = action["executor"].as_str().context("`executor` required")?;
            let executor = if executor == "-" { None } else { Some(executor) };
            let acting = acting_manager(store)?;
            store.set_work_executor(&acting, work, executor)?;
            Ok(format!("station {work} executor → {}", executor.unwrap_or("(user station)")))
        }
        "mission_create" => {
            let template = action["template"].as_str().context("`template` required")?;
            let key = action["key"].as_str().context("`key` required")?;
            let template_path = workspace.join(".im").join("templates").join(format!("{template}.yaml"));
            let bytes = std::fs::read(&template_path)
                .with_context(|| format!("template '{template}' not found"))?;
            let parsed = crate::contract::parse_template(&String::from_utf8_lossy(&bytes))?;
            let acting = acting_manager(store)?;
            let outcome = store.create_mission(
                &acting,
                &parsed,
                &format!(".im/templates/{template}.yaml"),
                &bytes,
                key,
                action["name"].as_str(),
                action["objective"].as_str(),
            )?;
            Ok(format!(
                "{} mission {}",
                if outcome.existed { "existing" } else { "created" },
                outcome.mission_id
            ))
        }
        "mission_end" => {
            let mission = action["mission"].as_str().context("`mission` required")?;
            let acting = acting_manager(store)?;
            store.delete_mission(&acting, mission, action["reason"].as_str())?;
            Ok(format!("ended mission {mission}"))
        }
        "mission_submit" => {
            let mission = action["mission"].as_str().context("`mission` required")?;
            let revision = action["revision"].as_i64().context("`revision` required")?;
            let outcome = action["outcome"].as_str().context("`outcome` required")?;
            let receipts: Vec<String> = action["receipts"]
                .as_array()
                .map(|list| {
                    list.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let acting = acting_manager(store)?;
            let result = store.submit_mission(
                &acting,
                mission,
                revision,
                outcome,
                action["next_node"].as_str(),
                action["reason"].as_str(),
                action["feedback"].as_str(),
                &receipts,
            )?;
            if result.mission_ended {
                Ok(format!("mission {mission} ended (revision {})", result.revision))
            } else {
                Ok(format!(
                    "mission {mission} → {} (revision {})",
                    result.routed_to.as_deref().unwrap_or("?"),
                    result.revision
                ))
            }
        }
        "work_create" => {
            let work = action["work"].as_str().context("`work` required")?;
            let executor = action["executor"].as_str().context("`executor` required (\"-\" for a user station)")?;
            let executor = if executor == "-" { None } else { Some(executor) };
            let acting = acting_manager(store)?;
            store.create_work(
                &acting,
                work,
                action["display_name"].as_str().unwrap_or(""),
                executor,
                action["prompt"].as_str().unwrap_or(""),
            )?;
            Ok(format!("station {work} created"))
        }
        "work_retire" => {
            let work = action["work"].as_str().context("`work` required")?;
            let acting = acting_manager(store)?;
            store.retire_work(&acting, work)?;
            Ok(format!("station {work} retired"))
        }
        other => bail!("unknown action type: {other}"),
    }
}

pub fn run(port: Option<u16>, no_open: bool) -> Result<()> {
    let workspace: PathBuf = {
        let mut dir = std::env::current_dir()?;
        loop {
            if dir.join(".im").exists() {
                break;
            }
            if !dir.pop() {
                bail!("Not an InfiniteMission workspace. Run 'im init' first.");
            }
        }
        dir
    };
    let listener = TcpListener::bind(("127.0.0.1", port.unwrap_or(0)))
        .with_context(|| "failed to bind a local port")?;
    let addr = listener.local_addr()?;
    let url = format!("http://{addr}");
    println!("im console: {url}");
    println!("workspace: {}", workspace.display());
    println!("Ctrl-C stops the server; it also exits after {IDLE_TIMEOUT_SECS}s with no requests.");

    if !no_open {
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        let _ = std::process::Command::new(opener).arg(&url).spawn();
    }

    let last_request = Arc::new(AtomicU64::new(now_secs()));
    {
        let last_request = Arc::clone(&last_request);
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(15));
            if now_secs().saturating_sub(last_request.load(Ordering::Relaxed)) > IDLE_TIMEOUT_SECS {
                println!("im console: idle timeout, exiting.");
                std::process::exit(0);
            }
        });
    }

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(_) => continue,
        };
        let last_request = Arc::clone(&last_request);
        let workspace = Arc::clone(&Arc::new(workspace.clone()));
        std::thread::spawn(move || {
            last_request.store(now_secs(), Ordering::Relaxed);
            let _ = handle(stream, &workspace);
        });
    }
    Ok(())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> Result<Request> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts
        .next()
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or_default()
        .to_string();
    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    Ok(Request { method, path, body })
}

fn respond(stream: &mut TcpStream, status: &str, content_type: &str, body: &[u8]) -> Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

fn handle(mut stream: TcpStream, workspace: &PathBuf) -> Result<()> {
    let request = match read_request(&mut stream) {
        Ok(request) => request,
        Err(_) => return Ok(()),
    };
    let db_path = workspace.join(".im").join("im.db");

    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") => respond(&mut stream, "200 OK", "text/html; charset=utf-8", PAGE.as_bytes()),
        ("GET", "/api/state") => {
            let store = crate::store::Store::open(&db_path)?;
            let templates = list_templates(workspace);
            let state = state_json(&store, &workspace.display().to_string(), &templates)?;
            respond(
                &mut stream,
                "200 OK",
                "application/json",
                serde_json::to_string(&state)?.as_bytes(),
            )
        }
        ("POST", "/api/action") => {
            let action: Value = match serde_json::from_slice(&request.body) {
                Ok(action) => action,
                Err(err) => {
                    let err = json!({ "error": format!("invalid JSON body: {err}") });
                    return respond(
                        &mut stream,
                        "400 Bad Request",
                        "application/json",
                        err.to_string().as_bytes(),
                    );
                }
            };
            let store = crate::store::Store::open(&db_path)?;
            match apply_action(&store, &action, workspace) {
                Ok(message) => respond(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    json!({ "ok": true, "message": message }).to_string().as_bytes(),
                ),
                Err(err) => respond(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    json!({ "ok": false, "error": err.to_string() }).to_string().as_bytes(),
                ),
            }
        }
        _ => respond(&mut stream, "404 Not Found", "text/plain; charset=utf-8", b"not found"),
    }
}

fn list_templates(workspace: &PathBuf) -> Vec<String> {
    let dir = workspace.join(".im").join("templates");
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
    names
}
