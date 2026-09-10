use assert_cmd::Command;
use std::path::Path;
use tempfile::TempDir;

fn im(workspace: &Path) -> Command {
    let mut cmd = Command::cargo_bin("im").unwrap();
    cmd.current_dir(workspace);
    // 测试 HOME：把注册表写进一次性目录，绝不污染真实 ~/.im/workspaces.json
    cmd.env("HOME", test_home());
    cmd
}

fn test_home() -> std::path::PathBuf {
    static HOME: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        let home = std::env::temp_dir().join(format!("im-test-home-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        home
    })
    .clone()
}

/// The console page is a static file driven entirely by `state_json`; this
/// test pins that data contract so UI breakage surfaces as a test failure.
#[test]
fn state_json_exposes_the_console_data_contract() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path();
    im(ws).arg("init").assert().success();
    for id in ["boss", "worker"] {
        im(ws).args(["join", id]).assert().success();
    }
    // Tier seeding goes through the console path (set_agent_tier) — the bare
    // CLI grant retired with the tier ladder.
    {
        let store = im::store::Store::open(&ws.join(".im").join("im.db")).unwrap();
        store
            .set_agent_tier("workspace", "boss", im::records::Tier::Manage)
            .unwrap();
    }
    im(ws)
        .args(["work", "create", "boss", "make", "--executor", "worker"])
        .assert()
        .success();
    im(ws)
        .args(["work", "create", "boss", "approve"])
        .assert()
        .success();
    std::fs::write(
        ws.join(".im").join("templates").join("t.yaml"),
        "schemaVersion: 4\nname: t\nentry: make\nworks:\n  make:\n    completion: {outcomes: [done], terminal: [], feedbackRequiredOn: []}\n    documentRights: {read: [], write: []}\n  approve:\n    completion: {outcomes: [ok], terminal: [ok], feedbackRequiredOn: []}\n    documentRights: {read: [], write: []}\npaths:\n  - {from: make, when: done, to: approve}\n",
    )
    .unwrap();
    im(ws)
        .args([
            "mission",
            "create",
            "boss",
            "--from",
            "approve",
            "--template",
            "t",
            "--key",
            "k1",
        ])
        .assert()
        .success();
    let store = im::store::Store::open(&ws.join(".im").join("im.db")).unwrap();
    let mission_id: String = store
        .conn
        .query_row("SELECT mission_id FROM missions", [], |r| r.get(0))
        .unwrap();
    drop(store);

    // Hop onto the user station so the inbox has a row.
    im(ws)
        .args([
            "mission",
            "submit",
            "worker",
            &mission_id,
            "--revision",
            "1",
            "--outcome",
            "done",
            "--reason",
            "sign-off needed",
        ])
        .assert()
        .success();

    let templates = std::fs::read_dir(ws.join(".im").join("templates"))
        .unwrap()
        .filter_map(|e| {
            let path = e.ok()?.path();
            if path.extension()? == "yaml" {
                path.file_stem()?.to_str().map(String::from)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    let store = im::store::Store::open(&ws.join(".im").join("im.db")).unwrap();
    let workspaces = vec![ws.to_str().unwrap().to_string()];
    let state = im::ui::state_json(&store, ws.to_str().unwrap(), &templates, &workspaces).unwrap();

    // Top-level sections the page renders.
    for key in [
        "workspace",
        "workspaces",
        "registeredWorkspaces",
        "agents",
        "works",
        "missions",
        "outbox",
        "results",
        "inbox",
        "events",
        "templates",
    ] {
        assert!(state.get(key).is_some(), "state_json missing {key}");
    }
    // The managers array retired with the tier ladder.
    assert!(state.get("managers").is_none(), "managers key must be gone");

    // Agents carry their tier instead of a manager bool.
    let agents = state["agents"].as_array().unwrap();
    let boss = agents.iter().find(|a| a["id"] == "boss").expect("boss");
    assert_eq!(boss["tier"].as_str().unwrap(), "manage");
    let worker = agents.iter().find(|a| a["id"] == "worker").expect("worker");
    assert_eq!(worker["tier"].as_str().unwrap(), "execute");
    assert!(
        agents.iter().all(|a| a.get("manager").is_none()),
        "per-agent manager bool must be gone"
    );

    // Works carry executor and a holding count for the stations board.
    let works = state["works"].as_array().unwrap();
    let make = works
        .iter()
        .find(|w| w["work_key"] == "make")
        .expect("make station");
    assert_eq!(make["executor"].as_str().unwrap(), "worker");
    assert_eq!(make["holding"].as_i64().unwrap(), 0, "mailbox moved on");
    assert_eq!(make["outbox"].as_i64().unwrap(), 0);
    assert_eq!(make["unreadResults"].as_i64().unwrap(), 0);
    let approve = works
        .iter()
        .find(|w| w["work_key"] == "approve")
        .expect("user station");
    assert!(approve["executor"].is_null());
    assert_eq!(approve["outbox"].as_i64().unwrap(), 1);
    assert_eq!(approve["unreadResults"].as_i64().unwrap(), 0);
    let outbox = state["outbox"].as_array().unwrap();
    assert_eq!(outbox.len(), 1);
    assert_eq!(outbox[0]["origin_work"].as_str().unwrap(), "approve");
    assert!(state["results"].as_array().unwrap().is_empty());

    // Missions carry at/revision; the inbox row carries the human reason.
    let missions = state["missions"].as_array().unwrap();
    assert_eq!(missions.len(), 1);
    assert_eq!(missions[0]["at"].as_str().unwrap(), "approve");
    assert!(missions[0]["revision"].as_i64().unwrap() >= 2);
    assert_eq!(missions[0]["origin_work"].as_str().unwrap(), "approve");
    let inbox = state["inbox"].as_array().unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0]["mission_id"].as_str().unwrap(), mission_id);
    assert_eq!(inbox[0]["origin_work"].as_str().unwrap(), "approve");
    assert!(
        inbox[0]["reason"].as_str().unwrap().contains("sign-off"),
        "inbox row lost the reason: {}",
        inbox[0]
    );

    // Decision kit: full round trail, per-outcome routes, documents list.
    let rounds = inbox[0]["rounds"].as_array().unwrap();
    assert_eq!(rounds.len(), 1, "one completed round so far: {rounds:?}");
    assert_eq!(rounds[0]["by"].as_str().unwrap(), "worker");
    assert_eq!(rounds[0]["outcome"].as_str().unwrap(), "done");
    assert!(
        rounds[0]["reason"].as_str().unwrap().contains("sign-off"),
        "round trail lost the reason: {rounds:?}"
    );
    let routes = inbox[0]["routes"].as_array().unwrap();
    assert!(
        !routes.is_empty(),
        "approve station has an outcome vocabulary"
    );
    assert!(
        routes
            .iter()
            .all(|r| r["outcome"].is_string() && r["to"].is_string()),
        "every outcome carries its destination: {routes:?}"
    );
    assert!(
        routes.iter().all(|r| r["feedback"].is_boolean()),
        "every outcome declares whether feedback is required: {routes:?}"
    );
    assert!(
        routes
            .iter()
            .any(|r| r["to"].as_str().unwrap().contains("终局")),
        "terminal outcomes are labeled as endings: {routes:?}"
    );
    assert!(
        inbox[0]["documents"].as_array().unwrap().is_empty(),
        "template t declares no documents"
    );

    // Events feed the delivery-history timeline.
    let events = state["events"].as_array().unwrap();
    assert!(
        events.len() >= 2,
        "expected created+routed events, got {events:?}"
    );
}

#[test]
fn console_can_set_the_first_manage_member_and_delete_one() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().to_path_buf();
    im(&ws).arg("init").assert().success();
    im(&ws).args(["join", "cursor"]).assert().success();

    let store = im::store::Store::open(&ws.join(".im").join("im.db")).unwrap();
    assert!(store.manage_members().unwrap().is_empty());

    // Bootstrap regression: set_tier to manage works with zero manage-tier
    // members — the console is the human.
    let message = im::ui::apply_action(
        &store,
        &serde_json::json!({ "type": "set_tier", "agent": "cursor", "tier": "manage" }),
        &ws,
    )
    .unwrap();
    assert!(
        message.contains("set cursor → manage tier"),
        "got: {message}"
    );
    assert_eq!(
        store.agent_tier("cursor").unwrap(),
        Some(im::records::Tier::Manage)
    );

    // Bootstrap is enough for the rest of the console (create a station —
    // a user station, so the member stays deletable below).
    let created = im::ui::apply_action(
        &store,
        &serde_json::json!({
            "type": "work_create",
            "work": "staging",
            "display_name": "Staging",
            "executor": "-",
            "prompt": ""
        }),
        &ws,
    )
    .unwrap();
    assert!(
        created.contains("station staging created"),
        "got: {created}"
    );

    // Console delete keeps full power over a manage-tier member.
    im::ui::apply_action(
        &store,
        &serde_json::json!({ "type": "delete_agent", "agent": "cursor" }),
        &ws,
    )
    .unwrap();
    assert_eq!(store.agent_tier("cursor").unwrap(), None);
}

#[test]
fn set_tier_validates_the_tier_enum() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().to_path_buf();
    im(&ws).arg("init").assert().success();
    im(&ws).args(["join", "cursor"]).assert().success();
    let store = im::store::Store::open(&ws.join(".im").join("im.db")).unwrap();

    let err = im::ui::apply_action(
        &store,
        &serde_json::json!({ "type": "set_tier", "agent": "cursor", "tier": "root" }),
        &ws,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("unknown tier"), "got: {err}");
}

#[test]
fn state_json_lists_work_presets_for_the_create_modal() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path();
    im(ws).arg("init").assert().success();
    let store = im::store::Store::open(&ws.join(".im").join("im.db")).unwrap();
    let state = im::ui::state_json(&store, ws.to_str().unwrap(), &[], &[]).unwrap();

    let presets = state["presets"].as_array().unwrap();
    let keys: Vec<&str> = presets.iter().map(|p| p["key"].as_str().unwrap()).collect();
    assert_eq!(keys, vec!["design", "plan", "build", "review"]);
    for preset in presets {
        assert!(
            preset["prompt"]
                .as_str()
                .unwrap()
                .contains("{mission.objective}"),
            "{}",
            preset["key"]
        );
    }
}

/// Resident console child that must never outlive the test: it holds the
/// inherited stdout pipe, so a leak hangs the outer `cargo test` pipeline
/// forever (no idle exit to reap it anymore).
struct UiGuard(std::process::Child);
impl Drop for UiGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn console_workspace_selection_persists_and_validates() {
    // 子进程隔离 HOME：注册表与持久化文件都落临时目录，不碰真实 ~/.im
    let home = tempfile::TempDir::new().unwrap();
    let ws = tempfile::TempDir::new().unwrap();
    let other = tempfile::TempDir::new().unwrap();
    let bin = env!("CARGO_BIN_EXE_im");
    // 切换只认注册表：两个工作区都经 im init 入册（同一测试 HOME）。
    // init 经 current_dir 入册的是真实路径（/private/var/...），TempDir
    // 给的是符号链接形式，POST 必须发 canonicalize 后的路径才对得上。
    let other_canon = std::fs::canonicalize(other.path()).unwrap();
    for dir in [ws.path(), other.path()] {
        let status = std::process::Command::new(bin)
            .env("HOME", home.path())
            .current_dir(dir)
            .arg("init")
            .status()
            .unwrap();
        assert!(status.success(), "im init failed in {}", dir.display());
    }

    // 1) ui 起在 ws，经 API 切到别的已注册工作区 → 状态文件记录选择
    let mut ui = UiGuard(
        std::process::Command::new(bin)
            .env("HOME", home.path())
            .current_dir(ws.path())
            .args(["ui", "--no-open", "--port", "4699"])
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Ok(Some(status)) = ui.0.try_wait() {
            panic!("console exited early ({status}) — is port 4699 busy?");
        }
        if std::net::TcpStream::connect("127.0.0.1:4699").is_ok() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "console did not come up on 4699"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let resp = std::process::Command::new("curl")
        .args([
            "-s",
            "--noproxy",
            "*",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "http://127.0.0.1:4699/api/workspace",
            "-d",
            &format!(r#"{{"path":"{}"}}"#, other_canon.display()),
        ])
        .output()
        .unwrap();
    let body = String::from_utf8_lossy(&resp.stdout);
    assert!(body.contains("\"ok\":true"), "switch failed: {body}");
    let state = std::fs::read_to_string(home.path().join(".im/console-state.json")).unwrap();
    assert!(
        state.contains(other_canon.display().to_string().as_str()),
        "{state}"
    );

    // 2) 指向未注册路径被拒（不会写入）
    let bad = std::process::Command::new("curl")
        .args([
            "-s",
            "--noproxy",
            "*",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "http://127.0.0.1:4699/api/workspace",
            "-d",
            r#"{"path":"/nonexistent/ws"}"#,
        ])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&bad.stdout).contains("not a known workspace"));

    // Console-plane doc read fails closed: unknown mission/doc answers a
    // readable 404, not a dropped connection.
    let doc = std::process::Command::new("curl")
        .args([
            "-s",
            "--noproxy",
            "*",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "http://127.0.0.1:4699/api/mission-doc?mission=ms_none&path=spec.md",
        ])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&doc.stdout),
        "404",
        "missing document must answer HTTP 404"
    );
    let escape = std::process::Command::new("curl")
        .args([
            "-s",
            "--noproxy",
            "*",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "http://127.0.0.1:4699/api/mission-doc?mission=ms_none&path=../im.db",
        ])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&escape.stdout), "404");
    // 守卫 Drop 时收尾；常驻进程没有闲置自退，这里显式确认已可停止
    drop(ui);
}

/// purge 是破坏性动作，注册表/桥镜像在 apply_action 里是进程内直调
/// （读进程 HOME）——这两个测试用互斥量 + 临时 HOME 隔离真实 ~/.im。
/// 这两个测试共享一把锁：都要临时改「进程」HOME（apply_action 在进程
/// 内读注册表），并发互踩环境变量会互相污染。
static PROC_HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 进程内直调测试专用：HOME 不被子命令覆写（继承测试进程已设好的
/// 临时 HOME），注册表与 apply_action 看到的是同一份。
fn im_with_proc_home(workspace: &Path) -> assert_cmd::Command {
    let mut cmd = assert_cmd::Command::cargo_bin("im").unwrap();
    cmd.current_dir(workspace);
    cmd
}

struct RestoreHome(String);
impl Drop for RestoreHome {
    fn drop(&mut self) {
        std::env::set_var("HOME", &self.0);
    }
}

#[test]
fn purge_workspace_guards_and_mirrors_the_bridge_list() {
    let _guard = PROC_HOME_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let home = tempfile::TempDir::new().unwrap();
    let _restore = RestoreHome(std::env::var("HOME").unwrap());
    std::env::set_var("HOME", home.path());

    let console_tmp = tempfile::TempDir::new().unwrap();
    let console_ws = console_tmp.path().to_path_buf();
    im_with_proc_home(&console_ws)
        .arg("init")
        .assert()
        .success();

    let victim_tmp = tempfile::TempDir::new().unwrap();
    let victim = victim_tmp.path().to_path_buf();
    im_with_proc_home(&victim).arg("init").assert().success();

    // 桥配置钉住两个工作区——purge 后镜像必须只剩控制台工作区
    std::fs::create_dir_all(home.path().join(".im")).unwrap();
    let console_phys = std::fs::canonicalize(&console_ws).unwrap();
    let victim_phys = std::fs::canonicalize(&victim).unwrap();
    std::fs::write(
        home.path().join(".im").join("t3-bridge.json"),
        format!(
            "{{\"workspaces\": [\"{}\", \"{}\"]}}",
            console_phys.display(),
            victim_phys.display()
        ),
    )
    .unwrap();

    let store = im::store::Store::open(&console_ws.join(".im").join("im.db")).unwrap();

    // 白名单：不在册的路径拒绝（堵任意路径删 ~/.im 一类目标）
    let stranger = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(stranger.path().join(".im")).unwrap();
    let err = im::ui::apply_action(
        &store,
        &serde_json::json!({ "type": "purge_workspace", "path": stranger.path() }),
        &console_ws,
    )
    .unwrap_err();
    assert!(err.to_string().contains("不在注册表"), "got: {err:#}");

    // 当前工作区拒绝
    let err = im::ui::apply_action(
        &store,
        &serde_json::json!({ "type": "purge_workspace", "path": console_ws }),
        &console_ws,
    )
    .unwrap_err();
    assert!(err.to_string().contains("当前工作区"), "got: {err:#}");

    // 活跃 mission 拒绝（合同锁不可绕过）
    {
        let victim_store = im::store::Store::open(&victim.join(".im").join("im.db")).unwrap();
        victim_store
            .conn
            .execute(
                "INSERT INTO missions (mission_id, name, objective, contract_json, at, status,
                                       revision, created_at, created_by)
                 VALUES ('ms_v','n','o','{}',NULL,'active',1,0,'t')",
                [],
            )
            .unwrap();
    }
    let err = im::ui::apply_action(
        &store,
        &serde_json::json!({ "type": "purge_workspace", "path": victim }),
        &console_ws,
    )
    .unwrap_err();
    assert!(err.to_string().contains("活跃 mission"), "got: {err:#}");

    // 结束 mission 后放行：.im 删除 + 注册表出册 + 桥清单镜像移除
    {
        let victim_store = im::store::Store::open(&victim.join(".im").join("im.db")).unwrap();
        victim_store
            .conn
            .execute("UPDATE missions SET status = 'ended'", [])
            .unwrap();
    }
    let message = im::ui::apply_action(
        &store,
        &serde_json::json!({ "type": "purge_workspace", "path": victim }),
        &console_ws,
    )
    .unwrap();
    assert!(message.contains("unregistered"), "got: {message}");
    assert!(!victim.join(".im").exists(), ".im must be gone");
    let registry: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join(".im").join("workspaces.json")).unwrap(),
    )
    .unwrap();
    let listed: Vec<&str> = registry["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(!listed.contains(&victim_phys.display().to_string().as_str()));
    let bridge: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join(".im").join("t3-bridge.json")).unwrap(),
    )
    .unwrap();
    let pinned: Vec<&str> = bridge["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(pinned, vec![console_phys.display().to_string().as_str()]);
}

#[test]
fn remove_workspace_refuses_symlink_alias_of_current_workspace() {
    let _guard = PROC_HOME_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let home = tempfile::TempDir::new().unwrap();
    let _restore = RestoreHome(std::env::var("HOME").unwrap());
    std::env::set_var("HOME", home.path());

    let console_tmp = tempfile::TempDir::new().unwrap();
    let console_ws = console_tmp.path().to_path_buf();
    im_with_proc_home(&console_ws)
        .arg("init")
        .assert()
        .success();
    let store = im::store::Store::open(&console_ws.join(".im").join("im.db")).unwrap();

    // 别名（符号链接）指向当前工作区：守卫必须在 canonicalize 之后比较，
    // 否则 registry::remove 内部的 canonicalize 会命中物理条目、静默注销当前工作区
    #[cfg(unix)]
    std::os::unix::fs::symlink(&console_ws, console_tmp.path().join("alias")).unwrap();
    let err = im::ui::apply_action(
        &store,
        &serde_json::json!({
            "type": "remove_workspace",
            "path": console_tmp.path().join("alias")
        }),
        &console_ws,
    )
    .unwrap_err();
    assert!(err.to_string().contains("当前工作区"), "got: {err:#}");
    let registry: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join(".im").join("workspaces.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(registry["workspaces"].as_array().unwrap().len(), 1);
}

#[test]
fn workspaces_remove_rejects_flags_as_paths() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path();
    im(ws).arg("init").assert().success();
    im(ws)
        .args(["workspaces", "--remove", "--prune"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--remove requires a path"));
}

#[test]
fn purge_redeletes_shell_recreated_like_a_running_receiver() {
    let _guard = PROC_HOME_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let home = tempfile::TempDir::new().unwrap();
    let _restore = RestoreHome(std::env::var("HOME").unwrap());
    std::env::set_var("HOME", home.path());

    let console_tmp = tempfile::TempDir::new().unwrap();
    let console_ws = console_tmp.path().to_path_buf();
    im_with_proc_home(&console_ws)
        .arg("init")
        .assert()
        .success();
    let victim_tmp = tempfile::TempDir::new().unwrap();
    let victim = victim_tmp.path().to_path_buf();
    im_with_proc_home(&victim).arg("init").assert().success();

    let store = im::store::Store::open(&console_ws.join(".im").join("im.db")).unwrap();

    // 复活源：模拟 im receive 的 open_store——目录一旦消失就在 ≤100ms 内
    // 重建空壳，1.2s 后「退出」（真 receive 遇空库查不到成员即退出）。
    // 复查循环必须先等一个轮询周期再查才能接住它。
    let dot = victim.join(".im");
    let mimic = std::thread::spawn(move || {
        let stop = std::time::Instant::now() + std::time::Duration::from_millis(1200);
        while std::time::Instant::now() < stop {
            if !dot.is_dir() {
                let _ = std::fs::create_dir_all(&dot);
                let _ = std::fs::write(dot.join("im.db"), b"");
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    });

    let message = im::ui::apply_action(
        &store,
        &serde_json::json!({ "type": "purge_workspace", "path": victim }),
        &console_ws,
    )
    .unwrap();
    mimic.join().unwrap();
    assert!(
        message.contains("recreated an empty shell — re-deleted"),
        "got: {message}"
    );
    assert!(!victim.join(".im").exists(), "final state must be clean");
}

/// The console page keeps the five-pane IA and only adds Work-origin / Ask
/// controls. This is a static interaction contract: selectors the page
/// actually binds, not a rendered browser session.
#[test]
fn console_page_keeps_five_pane_ia_and_ask_controls() {
    let page = include_str!("../src/web/page.html");
    assert!(
        page.contains("[\"inbox\", \"📥 收件箱\"")
            && page.contains("[\"works\", \"🛠 工位\"")
            && page.contains("[\"missions\", \"📋 任务\""),
        "task-face nav must keep inbox|works|missions"
    );
    for pane in ["members", "settings"] {
        assert!(
            page.contains(&format!("data-nav=\"{pane}\"")),
            "missing nav pane {pane}"
        );
    }
    assert!(
        page.contains("let pane = \"inbox\""),
        "default pane must remain inbox"
    );
    assert!(
        !page.contains("data-nav=\"ask\"") && !page.contains("data-nav=\"results\""),
        "Ask/results must not become new top-level panes"
    );
    for field in ["origin-work", "target-work", "question", "key"] {
        assert!(
            page.contains(&format!("data-f=\"{field}\"")),
            "Ask modal missing field {field}"
        );
    }
    assert!(page.contains("data-act=\"open-ask-create\""));
    assert!(page.contains("data-act=\"ask-create\""));
    assert!(page.contains("data-act=\"mission-create\""));
    assert!(page.contains("data-act=\"result-ack\""));
    assert!(page.contains("来源工位（from）"));
    assert!(page.contains("目标工位（to）"));
    assert!(page.contains("问题（question）"));
    assert!(page.contains("aria-required=\"true\""));
    assert!(page.contains("id=\"modal-status\""));
    assert!(page.contains("role=\"alert\""));
    assert!(page.contains("aria-labelledby=\"modal-title\""));
    assert!(
        page.contains("type: \"result_ack\""),
        "ack must post result_ack"
    );
    let ack_idx = page.find("case \"result-ack\"").expect("result-ack handler");
    let ack_slice = &page[ack_idx..ack_idx + 120];
    assert!(
        !ack_slice.contains("mission_submit"),
        "result ack must not submit: {ack_slice}"
    );
}

#[test]
fn console_origin_ask_and_result_ack_do_not_consume_notes() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().to_path_buf();
    im(&ws).arg("init").assert().success();
    for id in ["boss", "worker"] {
        im(&ws).args(["join", id]).assert().success();
    }
    {
        let store = im::store::Store::open(&ws.join(".im").join("im.db")).unwrap();
        store
            .set_agent_tier("workspace", "boss", im::records::Tier::Manage)
            .unwrap();
    }
    im(&ws)
        .args(["work", "create", "boss", "make", "--executor", "worker"])
        .assert()
        .success();
    im(&ws)
        .args(["work", "create", "boss", "desk"])
        .assert()
        .success();
    std::fs::write(
        ws.join(".im").join("templates").join("t.yaml"),
        "schemaVersion: 4\nname: t\nentry: make\nworks:\n  make:\n    completion: {outcomes: [done], terminal: [done], feedbackRequiredOn: []}\n    documentRights: {read: [], write: []}\npaths: []\n",
    )
    .unwrap();

    let store = im::store::Store::open(&ws.join(".im").join("im.db")).unwrap();

    let missing = im::ui::apply_action(
        &store,
        &serde_json::json!({ "type": "mission_create", "template": "t", "key": "no-origin" }),
        &ws,
    )
    .unwrap_err()
    .to_string();
    assert!(
        missing.contains("originWork"),
        "mission_create must require originWork, got: {missing}"
    );
    let empty = im::ui::apply_action(
        &store,
        &serde_json::json!({
            "type": "mission_create",
            "template": "t",
            "key": "empty-origin",
            "originWork": ""
        }),
        &ws,
    )
    .unwrap_err()
    .to_string();
    assert!(
        empty.contains("originWork"),
        "empty originWork must be rejected, got: {empty}"
    );

    let created = im::ui::apply_action(
        &store,
        &serde_json::json!({
            "type": "mission_create",
            "template": "t",
            "key": "from-desk",
            "originWork": "desk",
            "name": "from desk"
        }),
        &ws,
    )
    .unwrap();
    assert!(created.contains("created"), "got: {created}");

    let asked = im::ui::apply_action(
        &store,
        &serde_json::json!({
            "type": "ask_create",
            "originWork": "desk",
            "targetWork": "make",
            "question": "What ships first?",
            "key": "q-ui"
        }),
        &ws,
    )
    .unwrap();
    assert!(asked.contains("created ask"), "got: {asked}");

    let ask_id: String = store
        .conn
        .query_row(
            "SELECT mission_id FROM missions WHERE name = 'ask' ORDER BY created_at DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    im(&ws)
        .args([
            "mission",
            "submit",
            "worker",
            &ask_id,
            "--revision",
            "1",
            "--outcome",
            "answered",
            "--result",
            "the inbox round-trip",
        ])
        .assert()
        .success();

    let state = im::ui::state_json(&store, ws.to_str().unwrap(), &["t".into()], &[]).unwrap();
    let desk = state["works"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["work_key"] == "desk")
        .expect("desk");
    assert_eq!(desk["unreadResults"].as_i64().unwrap(), 1);
    let results = state["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["acked"], false);
    assert_eq!(results[0]["work_key"], "desk");
    assert!(
        results[0]["summary"]
            .as_str()
            .unwrap()
            .contains("inbox round-trip"),
        "inbox return area lost the result: {}",
        results[0]
    );
    let missions = state["missions"].as_array().unwrap();
    let ask_row = missions
        .iter()
        .find(|m| m["mission_id"] == ask_id)
        .expect("ask mission");
    assert_eq!(ask_row["origin_work"].as_str().unwrap(), "desk");

    // list-without-consume: a second snapshot must still see the unread note.
    let again = im::ui::state_json(&store, ws.to_str().unwrap(), &["t".into()], &[]).unwrap();
    assert_eq!(again["results"][0]["acked"], false);
    let unread: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM work_notes WHERE kind = 'mission_result' AND read = 0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(unread, 1);

    store
        .conn
        .execute(
            "INSERT INTO work_notes (work_key, kind, mission_id, content, created_at, read)
             VALUES ('desk', 'mission_arrived', ?1, 'keep other notes', 1, 0)",
            [&ask_id],
        )
        .unwrap();
    let note_id = results[0]["note_id"].as_i64().unwrap();
    let revision_before: i64 = store
        .conn
        .query_row(
            "SELECT revision FROM missions WHERE mission_id = ?1",
            [&ask_id],
            |row| row.get(0),
        )
        .unwrap();
    let ack = im::ui::apply_action(
        &store,
        &serde_json::json!({ "type": "result_ack", "noteId": note_id }),
        &ws,
    )
    .unwrap();
    assert!(ack.contains("acknowledged"), "got: {ack}");

    let result_read: i64 = store
        .conn
        .query_row(
            "SELECT read FROM work_notes WHERE id = ?1 AND kind = 'mission_result'",
            [note_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(result_read, 1);
    let keep_unread: i64 = store
        .conn
        .query_row(
            "SELECT read FROM work_notes WHERE content = 'keep other notes'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(keep_unread, 0, "ack must not consume other notes");
    let other_kinds_acked: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM work_notes WHERE kind != 'mission_result' AND read = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        other_kinds_acked, 0,
        "ack must only mark mission_result notes"
    );
    let status: String = store
        .conn
        .query_row(
            "SELECT status FROM missions WHERE mission_id = ?1",
            [&ask_id],
            |row| row.get(0),
        )
        .unwrap();
    let revision_after: i64 = store
        .conn
        .query_row(
            "SELECT revision FROM missions WHERE mission_id = ?1",
            [&ask_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status, "ended");
    assert_eq!(
        revision_before, revision_after,
        "ack must not mission_submit"
    );

    let after = im::ui::state_json(&store, ws.to_str().unwrap(), &["t".into()], &[]).unwrap();
    assert_eq!(after["results"][0]["acked"], true);
    let desk_after = after["works"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["work_key"] == "desk")
        .unwrap();
    assert_eq!(desk_after["unreadResults"].as_i64().unwrap(), 0);
}
