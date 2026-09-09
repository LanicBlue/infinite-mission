//! The control plane console: a resident local web server over the same
//! static SQLite core. Binds 127.0.0.1 only (external access goes through
//! the gateway in front of it) and runs until stopped — im itself stays a
//! serverless CLI; the console is just a viewer over `.im/im.db` and its
//! lifecycle never affects CLI usage.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const PAGE: &str = include_str!("web/page.html");

/// Fixed default console port so the URL is stable across runs.
const DEFAULT_UI_PORT: u16 = 4600;

/// Workspaces the console may switch between: the global registry
/// (`~/.im/workspaces.json`, maintained by `im init` / `im workspaces`)
/// plus the one the server started in. Switching is restricted to this
/// list, so the endpoint can never be pointed at an arbitrary path.
/// Console workspace selection memory: survives the transient console
/// process (idle exit + launchd respawn) so a restart lands where the user
/// left off, not on the boot cwd.
fn console_state_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("$HOME is not set")?;
    Ok(Path::new(&home).join(".im").join("console-state.json"))
}

fn save_last_workspace(workspace: &Path) -> Result<()> {
    let path = console_state_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        &path,
        serde_json::json!({ "workspace": workspace.display().to_string() }).to_string(),
    )
    .with_context(|| format!("writing {}", path.display()))
}

fn load_last_workspace() -> Option<PathBuf> {
    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(console_state_path().ok()?).ok()?).ok()?;
    raw.get("workspace")?.as_str().map(PathBuf::from)
}

pub fn discover_workspaces(current: &Path) -> Vec<PathBuf> {
    crate::registry::discover(current)
}

pub fn state_json(
    store: &crate::store::Store,
    workspace: &str,
    templates: &[String],
    workspaces: &[String],
) -> Result<Value> {
    let now_ts = chrono::Utc::now().timestamp();
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
                "status": status,
                "tier": agent.tier.as_str(),
            })
        })
        .collect();

    let inbound = store.inbound_counts()?;
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
                "description": work.description,
                "executor": work.executor,
                "prompt": work.prompt,
                "holding": holding,
                "incoming": inbound.get(&work.work_key).copied().unwrap_or(0),
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

    let mut inbox: Vec<Value> = Vec::new();
    for (mission, _) in store.inbox_missions()? {
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
        // The station's vocabulary so the human can resolve in place,
        // each outcome annotated with where it sends the mission next
        // (mirrors the submit adjudication's route resolution).
        let contract = crate::mission::parse_contract(&mission.contract_json).ok();
        let discipline = contract
            .as_ref()
            .zip(mission.at.as_deref())
            .and_then(|(c, at)| c.works.get(at));
        let (outcomes, terminal, feedback_on): (Vec<String>, Vec<String>, Vec<String>) = discipline
            .map(|d| {
                (
                    d.completion.outcomes.clone(),
                    d.completion.terminal.clone(),
                    d.completion.feedback_required_on.clone(),
                )
            })
            .unwrap_or_default();
        let routes: Vec<Value> = outcomes
            .iter()
            .map(|outcome| {
                let to = if terminal.contains(outcome) {
                    "终局 · 任务完成".to_string()
                } else {
                    let targets: Vec<String> = contract
                        .as_ref()
                        .zip(mission.at.as_deref())
                        .map(|(c, at)| {
                            c.paths
                                .iter()
                                .filter(|edge| {
                                    edge.from == at
                                        && (edge.when == *outcome
                                            || edge.when == crate::contract::ANY)
                                })
                                .map(|edge| edge.to.clone())
                                .collect()
                        })
                        .unwrap_or_default();
                    match targets.len() {
                        0 => "无后续（不可用）".to_string(),
                        1 => format!("→ {}", targets[0]),
                        _ => format!("→ {}（需明选）", targets.join(" / ")),
                    }
                };
                json!({
                    "outcome": outcome,
                    "to": to,
                    // Rejection-style outcomes must carry feedback; the
                    // console modal needs to know to demand it.
                    "feedback": feedback_on.contains(outcome),
                })
            })
            .collect();
        // Full round trail for this mission — the global events feed is
        // capped at 100 across all missions, the decision card must not
        // lose the tail of a long-running one.
        let rounds: Vec<Value> = {
            let mut stmt = store.conn.prepare(
                "SELECT payload, created_at FROM mission_events
                     WHERE mission_id = ?1 AND type = 'mission.round.completed'
                     ORDER BY seq",
            )?;
            let rows = stmt
                .query_map([&mission.mission_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows.into_iter()
                .filter_map(|(payload, ts)| {
                    let payload: Value = serde_json::from_str(&payload).ok()?;
                    let by = payload.get("resolvedBy");
                    Some(json!({
                        "work": payload.get("workKey").and_then(|v| v.as_str()),
                        "iteration": payload.get("iteration").and_then(|v| v.as_i64()),
                        "outcome": payload.get("outcome").and_then(|v| v.as_str()),
                        "by": by.and_then(|r| r.get("executorRef")).and_then(|v| v.as_str()),
                        "plane": by.and_then(|r| r.get("plane")).and_then(|v| v.as_str()),
                        "reason": payload.get("reason").and_then(|v| v.as_str()),
                        "feedback": payload.get("feedback").and_then(|v| v.as_str()),
                        "receipts": payload
                            .get("documentReceipts")
                            .and_then(|v| v.as_array())
                            .map(|a| a.len())
                            .unwrap_or(0),
                        "at": ts,
                    }))
                })
                .collect()
        };
        // Documents written so far — the decision substance (spec,
        // review, …). Content is served by /api/mission-doc. The same
        // path can carry several content-addressed versions (rounds
        // rewrite it); the disk file is the latest one, so the card
        // lists one chip per path, latest write first.
        let mut documents: Vec<Value> = store
            .conn
            .prepare(
                "SELECT document_id, path, work_key, written_by, written_at
                     FROM mission_documents WHERE mission_id = ?1
                     ORDER BY written_at DESC",
            )?
            .query_map([&mission.mission_id], |row| {
                Ok(json!({
                    "id": row.get::<_, String>(0)?,
                    "path": row.get::<_, String>(1)?,
                    "work": row.get::<_, String>(2)?,
                    "by": row.get::<_, String>(3)?,
                    "at": row.get::<_, i64>(4)?,
                }))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut seen_paths: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        documents.retain(|d| {
            let path = d["path"].as_str().unwrap_or_default().to_string();
            seen_paths.insert(path)
        });
        inbox.push(json!({
            "mission_id": mission.mission_id,
            "at": mission.at,
            "name": mission.name,
            "objective": mission.objective,
            "reason": reason,
            "arrived_at": arrived_at,
            "revision": mission.revision,
            "outcomes": outcomes,
            "terminal": terminal,
            "routes": routes,
            "rounds": rounds,
            "documents": documents,
        }));
    }

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
        "workspaces": workspaces,
        "registeredWorkspaces": crate::registry::list_with_liveness()
            .unwrap_or_default()
            .into_iter()
            .map(|(path, live)| json!({ "path": path.display().to_string(), "live": live }))
            .collect::<Vec<_>>(),
        "templates": templates,
        "presets": crate::pipeline::PRESETS
            .iter()
            .map(|p| json!({
                "key": p.key,
                "description": p.description,
                "prompt": p.prompt,
            }))
            .collect::<Vec<_>>(),
        "agents": agents,
        "works": works,
        "missions": missions,
        "inbox": inbox,
        "events": events,
    }))
}

/// The console's acting identity: the first manage-tier member. Manage ⊃
/// publish, so every gated console action stays covered.
fn acting_manager(store: &crate::store::Store) -> Result<String> {
    match store.manage_members()?.into_iter().next() {
        Some(member) => Ok(member),
        None => bail!(
            "no manage-tier member exists yet — set one on this Members page \
             (tier dropdown), then reload"
        ),
    }
}

pub fn apply_action(
    store: &crate::store::Store,
    action: &Value,
    workspace: &Path,
) -> Result<String> {
    let response = apply_action_response(store, action, workspace)?;
    let mut message = response["message"].as_str().unwrap_or_default().to_string();
    if let Some(view) = response.get("runView") {
        message.push('\n');
        message.push_str(&serde_json::to_string_pretty(view)?);
    }
    Ok(message)
}

/// Preserve the usual ok/message envelope and return the direct first-round
/// handoff as structured data rather than dropping it in the console adapter.
pub fn apply_action_response(
    store: &crate::store::Store,
    action: &Value,
    workspace: &Path,
) -> Result<Value> {
    let kind = action["type"].as_str().context("action needs a `type`")?;
    let mut run_view = None;
    let message: Result<String> = match kind {
        "set_tier" => {
            let agent = action["agent"].as_str().context("`agent` required")?;
            let tier_str = action["tier"].as_str().context("`tier` required")?;
            let tier = crate::records::Tier::parse(tier_str)
                .with_context(|| format!("unknown tier {tier_str:?} (execute|publish|manage)"))?;
            // The console is the human operator, full power — the ONLY manage
            // surface. It must work with zero manage-tier members (bootstrap)
            // and may set manage itself.
            store.set_agent_tier("workspace", agent, tier)?;
            Ok(format!("set {agent} → {tier_str} tier"))
        }
        "prune_workspaces" => {
            let removed = crate::registry::prune()?;
            Ok(format!(
                "pruned {removed} stale workspace entr{}",
                if removed == 1 { "y" } else { "ies" }
            ))
        }
        "remove_workspace" => {
            let target = action["path"].as_str().context("`path` required")?;
            // 与注册表的物理路径对齐（/tmp → /private/tmp），否则符号链接
            // 别名可以绕过下方的当前工作区守卫（registry::remove 内部会
            // canonicalize，守卫必须用同一形态比较——两侧都对齐）
            let target = std::fs::canonicalize(std::path::PathBuf::from(target))
                .unwrap_or_else(|_| std::path::PathBuf::from(target));
            let here = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
            if target == here {
                return Err(anyhow::anyhow!(
                    "当前工作区不能从注册表移除——先切换到别的工作区"
                ));
            }
            let removed = crate::registry::remove(&target)?;
            Ok(if removed {
                format!("removed {target:?} from the registry")
            } else {
                format!("{target:?} was not registered")
            })
        }
        "purge_workspace" => {
            // remove_workspace 的破坏性兄弟：注销 + 删除该工作区的 .im
            // （missions/成员/工位/模板/文档，全部不可恢复）。工作区其余
            // 内容不动。活跃 mission 拒绝——删库不能绕过合同锁纪律。
            let target = action["path"].as_str().context("`path` required")?;
            let raw_target = std::path::PathBuf::from(target);
            // 与注册表的物理路径对齐（/tmp → /private/tmp）：守卫、白名单、
            // 删除三处判定必须落在同一条目上
            let target =
                std::fs::canonicalize(&raw_target).unwrap_or_else(|_| raw_target.clone());
            let here = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
            if target == here {
                return Err(anyhow::anyhow!(
                    "当前工作区不能注销并删除——先切换到别的工作区"
                ));
            }
            // 白名单：只清在册工作区。否则任意路径下的 .im（比如 HOME 下
            // 的 ~/.im 全局状态）都会成为删除目标。
            let known = crate::registry::list_with_liveness()?
                .iter()
                .any(|(entry, _)| *entry == target || *entry == raw_target);
            if !known {
                return Err(anyhow::anyhow!(
                    "refusing: {target:?} 不在注册表——purge 只清在册工作区"
                ));
            }
            let dot = target.join(".im");
            let had_dot = dot.is_dir();
            let db = dot.join("im.db");
            if db.is_file() {
                // Store::open 会顺手建文件——存在才开
                let target_store = crate::store::Store::open(&db)?;
                let active = target_store.active_mission_count()?;
                if active > 0 {
                    return Err(anyhow::anyhow!(
                        "refusing: {target:?} 还有 {active} 个活跃 mission——先结束再删，purge 不能绕过合同锁"
                    ));
                }
            }
            // 先删后注销：删除硬失败时保持未注销（数据可见、可修权限重试），
            // 注销失败只留下可 prune 的死条目——反过来的中间态是孤儿数据。
            // 「不存在」视为已删；其他错误如实上抛。
            match std::fs::remove_dir_all(&dot) {
                Ok(()) => {}
                Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "deletion failed: {e} —— 未注销，.im 数据仍在"
                    ))
                }
            }
            // 复活复查：运行中的 `im receive` 轮询间隔 ≤500ms，会在删除后
            // 的下一个 tick 用 open_store 重建空壳 .im——「先查再等」会在
            // 复活发生前秒退（窗口形同虚设），必须先等一个轮询周期再查，
            // 干净满一个周期才算稳；复活则重删，直到 3 秒窗口耗尽。
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            let poll = std::time::Duration::from_millis(600);
            let mut recreated = false;
            loop {
                std::thread::sleep(poll);
                if !dot.is_dir() {
                    break;
                }
                recreated = true;
                if std::time::Instant::now() > deadline {
                    break;
                }
                match std::fs::remove_dir_all(&dot) {
                    Ok(()) => {}
                    Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        return Err(anyhow::anyhow!(
                            "deletion failed: {e} —— 已删部分数据，注册表未动"
                        ))
                    }
                }
            }
            if dot.is_dir() {
                return Err(anyhow::anyhow!(
                    "a running receive keeps recreating {target:?}/.im —— \
                     数据已清、空壳仍在：稍后重试 purge 或手动删除"
                ));
            }
            let removed = crate::registry::remove(&target)?;
            Ok(match (removed, recreated, had_dot) {
                (true, false, true) => format!("unregistered {target:?} and deleted its .im"),
                (true, false, false) => format!("unregistered {target:?} (no .im to delete)"),
                (_, true, _) => format!(
                    "unregistered {target:?}; its .im was deleted (a running \
                     receive recreated an empty shell — re-deleted)"
                ),
                (false, false, _) => format!("deleted {target:?}/.im (it was not registered)"),
            })
        }
        "delete_agent" => {
            let agent = action["agent"].as_str().context("`agent` required")?;
            // Deleting needs no acting manager — the user may remove any
            // member, manage included — so the notice is attributed to
            // whoever remains.
            let actor = store
                .manage_members()?
                .into_iter()
                .next()
                .unwrap_or_else(|| "workspace".to_string());
            store.delete_agent(&actor, agent)?;
            let session_file = workspace.join(".im").join("sessions").join(agent);
            let _ = std::fs::remove_file(&session_file);
            Ok(format!("deleted member {agent}"))
        }
        "set_executor" => {
            let work = action["work"].as_str().context("`work` required")?;
            let executor = action["executor"].as_str().context("`executor` required")?;
            let executor = if executor == "-" {
                None
            } else {
                Some(executor)
            };
            let acting = acting_manager(store)?;
            store.set_work_executor(&acting, work, executor)?;
            Ok(format!(
                "station {work} executor → {}",
                executor.unwrap_or("(user station)")
            ))
        }
        "mission_create" => {
            let template = action["template"].as_str().context("`template` required")?;
            let key = action["key"].as_str().context("`key` required")?;
            let template_path = workspace
                .join(".im")
                .join("templates")
                .join(format!("{template}.yaml"));
            let bytes = std::fs::read(&template_path)
                .with_context(|| format!("template '{template}' not found"))?;
            let parsed = crate::contract::parse_template(&String::from_utf8_lossy(&bytes))?;
            let acting = acting_manager(store)?;
            let source = crate::mission::TemplateSource {
                template: &parsed,
                path: format!(".im/templates/{template}.yaml"),
                bytes: &bytes,
            };
            let outcome = store.create_mission(
                &acting,
                &source,
                key,
                action["name"].as_str(),
                action["objective"].as_str(),
            )?;
            run_view = outcome.run_view;
            Ok(format!(
                "{} mission {}",
                if outcome.existed {
                    "existing"
                } else {
                    "created"
                },
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
            let submission = crate::mission::RoundSubmission {
                next_node: action["next_node"].as_str(),
                reason: action["reason"].as_str(),
                feedback: action["feedback"].as_str(),
                receipt_ids: &receipts,
            };
            let result = store.submit_mission(&acting, mission, revision, outcome, &submission)?;
            if result.mission_ended {
                Ok(format!(
                    "mission {mission} ended (revision {})",
                    result.revision
                ))
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
            let executor = action["executor"]
                .as_str()
                .context("`executor` required (\"-\" for a user station)")?;
            let executor = if executor == "-" {
                None
            } else {
                Some(executor)
            };
            let acting = acting_manager(store)?;
            store.create_work(
                &acting,
                work,
                action["description"].as_str().unwrap_or(""),
                executor,
                action["prompt"].as_str().unwrap_or(""),
            )?;
            Ok(format!("station {work} created"))
        }
        "work_delete" => {
            let work = action["work"].as_str().context("`work` required")?;
            let acting = acting_manager(store)?;
            store.delete_work(&acting, work)?;
            Ok(format!("station {work} deleted"))
        }
        other => bail!("unknown action type: {other}"),
    };
    let mut response = json!({ "ok": true, "message": message? });
    if let Some(view) = run_view {
        response["runView"] = serde_json::to_value(view)?;
    }
    Ok(response)
}

pub fn run(port: Option<u16>, no_open: bool) -> Result<()> {
    let mut workspace: PathBuf = {
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
    // 重启恢复：上次选中的工作区仍在册（.im 存在）就回到它，否则用启动目录。
    if let Some(last) = load_last_workspace() {
        if last.join(".im").is_dir() {
            workspace = last;
        }
    }
    let port = port.unwrap_or(DEFAULT_UI_PORT);
    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!(
            "port {port} is busy — another `im ui` may already be running; pass --port <N> for a different one"
        ))?;
    let addr = listener.local_addr()?;
    let url = format!("http://{addr}");
    let discovered = discover_workspaces(&workspace);
    println!("im console: {url}");
    println!("workspace: {}", workspace.display());
    println!(
        "switchable workspaces ({}): {}",
        discovered.len(),
        discovered
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "Resident console — stops on Ctrl-C / service stop. im itself stays a serverless CLI."
    );

    if !no_open {
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let _ = std::process::Command::new(opener).arg(&url).spawn();
    }

    // The active workspace is shared mutable state: the console may switch
    // between discovered workspaces at runtime via POST /api/workspace.
    let workspace = Arc::new(Mutex::new(workspace));

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(_) => continue,
        };
        let workspace = Arc::clone(&workspace);
        std::thread::spawn(move || {
            let _ = handle(stream, &workspace);
        });
    }
    Ok(())
}

struct Request {
    method: String,
    path: String,
    query: String,
    content_type: String,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> Result<Request> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let raw_path = parts.next().unwrap_or_default().to_string();
    let (path, query) = match raw_path.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (raw_path, String::new()),
    };
    let mut content_length = 0usize;
    let mut content_type = String::new();
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let lower = header.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        } else if let Some(value) = lower.strip_prefix("content-type:") {
            content_type = value.trim().to_string();
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    Ok(Request {
        method,
        path,
        query,
        content_type,
        body,
    })
}

fn parse_query(query: &str) -> std::collections::BTreeMap<String, String> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            Some((url_decode(key), url_decode(value)))
        })
        .collect()
}

fn url_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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

fn handle(mut stream: TcpStream, current: &Mutex<PathBuf>) -> Result<()> {
    let request = match read_request(&mut stream) {
        Ok(request) => request,
        Err(_) => return Ok(()),
    };
    let workspace = current.lock().expect("workspace lock poisoned").clone();
    let db_path = workspace.join(".im").join("im.db");

    // 状态变更端点只收 application/json：控制台页面的 fetch 都带这个头，
    // 而 text/plain 是 CORS「简单请求」——跨源恶意页面可以不带预检盲打
    // POST；强制 JSON 让跨源 fetch 必须过预检（本服务无 ACAO，预检必败）。
    if request.method == "POST" && !request.content_type.starts_with("application/json") {
        return respond(
            &mut stream,
            "415 Unsupported Media Type",
            "text/plain; charset=utf-8",
            b"POST endpoints require Content-Type: application/json",
        );
    }

    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") => respond(
            &mut stream,
            "200 OK",
            "text/html; charset=utf-8",
            PAGE.as_bytes(),
        ),
        ("GET", "/api/mission-doc") => {
            // Console-plane document read: the human at a user station is
            // the decider, so this is not gated on station documentRights
            // (that gate is for agent plane). Only declared, written
            // documents are served — no arbitrary file reads.
            // Errors answer 404 with the reason in the body — a dropped
            // connection would surface as a network error in the console
            // page instead of a readable status.
            let served = (|| -> Result<Vec<u8>> {
                let params = parse_query(&request.query);
                let (mission_id, doc_path) = match (
                    params.get("mission").map(String::as_str),
                    params.get("path").map(String::as_str),
                ) {
                    (Some(m), Some(p)) => (m, p),
                    _ => bail!("mission and path query params are required"),
                };
                if doc_path.split('/').any(|seg| seg == ".." || seg.is_empty()) {
                    bail!("invalid document path");
                }
                let store = crate::store::Store::open(&db_path)?;
                let known: Option<String> = store
                    .conn
                    .query_row(
                        "SELECT path FROM mission_documents
                         WHERE mission_id = ?1 AND path = ?2 LIMIT 1",
                        rusqlite::params![mission_id, doc_path],
                        |row| row.get(0),
                    )
                    .ok();
                if known.is_none() {
                    bail!("no document {doc_path:?} on mission {mission_id}");
                }
                // Documents live under a per-mission subdirectory
                // (see write_mission_document).
                let file = workspace
                    .join(".im")
                    .join("mission-documents")
                    .join(mission_id)
                    .join(doc_path);
                Ok(std::fs::read_to_string(&file)
                    .with_context(|| format!("reading {}", file.display()))?
                    .into_bytes())
            })();
            match served {
                Ok(content) => {
                    respond(&mut stream, "200 OK", "text/plain; charset=utf-8", &content)?
                }
                Err(err) => respond(
                    &mut stream,
                    "404 Not Found",
                    "text/plain; charset=utf-8",
                    format!("document unavailable: {err:#}").as_bytes(),
                )?,
            }
            Ok(())
        }
        ("GET", "/api/state") => {
            let store = crate::store::Store::open(&db_path)?;
            let templates = list_templates(&workspace);
            let workspaces: Vec<String> = discover_workspaces(&workspace)
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            let state = state_json(
                &store,
                &workspace.display().to_string(),
                &templates,
                &workspaces,
            )?;
            respond(
                &mut stream,
                "200 OK",
                "application/json",
                serde_json::to_string(&state)?.as_bytes(),
            )
        }
        ("POST", "/api/workspace") => {
            let body: Value = match serde_json::from_slice(&request.body) {
                Ok(body) => body,
                Err(err) => {
                    let err = json!({ "ok": false, "error": format!("invalid JSON body: {err}") });
                    return respond(
                        &mut stream,
                        "400 Bad Request",
                        "application/json",
                        err.to_string().as_bytes(),
                    );
                }
            };
            let target = body["path"].as_str().context("`path` required")?;
            let target_path = PathBuf::from(target);
            // Switching is restricted to discovered workspaces — the endpoint
            // can never be pointed at an arbitrary path.
            let allowed = discover_workspaces(&workspace);
            if !allowed.contains(&target_path) || !target_path.join(".im").is_dir() {
                let err = json!({
                    "ok": false,
                    "error": format!("not a known workspace: {target}"),
                });
                return respond(
                    &mut stream,
                    "400 Bad Request",
                    "application/json",
                    err.to_string().as_bytes(),
                );
            }
            *current.lock().expect("workspace lock poisoned") = target_path.clone();
            if let Err(err) = save_last_workspace(&target_path) {
                eprintln!("im console: cannot persist workspace selection: {err}");
            }
            respond(
                &mut stream,
                "200 OK",
                "application/json",
                json!({ "ok": true, "workspace": target })
                    .to_string()
                    .as_bytes(),
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
            match apply_action_response(&store, &action, &workspace) {
                Ok(response) => respond(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    response.to_string().as_bytes(),
                ),
                Err(err) => respond(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    json!({ "ok": false, "error": err.to_string() })
                        .to_string()
                        .as_bytes(),
                ),
            }
        }
        _ => respond(
            &mut stream,
            "404 Not Found",
            "text/plain; charset=utf-8",
            b"not found",
        ),
    }
}

fn list_templates(workspace: &Path) -> Vec<String> {
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
