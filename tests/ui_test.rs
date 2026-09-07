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
    let approve = works
        .iter()
        .find(|w| w["work_key"] == "approve")
        .expect("user station");
    assert!(approve["executor"].is_null());

    // Missions carry at/revision; the inbox row carries the human reason.
    let missions = state["missions"].as_array().unwrap();
    assert_eq!(missions.len(), 1);
    assert_eq!(missions[0]["at"].as_str().unwrap(), "approve");
    assert!(missions[0]["revision"].as_i64().unwrap() >= 2);
    let inbox = state["inbox"].as_array().unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0]["mission_id"].as_str().unwrap(), mission_id);
    assert!(
        inbox[0]["reason"].as_str().unwrap().contains("sign-off"),
        "inbox row lost the reason: {}",
        inbox[0]
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
            "http://127.0.0.1:4699/api/workspace",
            "-d",
            r#"{"path":"/nonexistent/ws"}"#,
        ])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&bad.stdout).contains("not a known workspace"));
    // 守卫 Drop 时收尾；常驻进程没有闲置自退，这里显式确认已可停止
    drop(ui);
}
