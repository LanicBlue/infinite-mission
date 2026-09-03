#!/usr/bin/env python3
"""Codex Desktop lifecycle adapter for InfiniteMission.

Normal IM commands stay on the real `im` CLI. This helper only arms, inspects,
and stops a detached one-shot receive watcher. Runtime state lives outside the
IM workspace under ~/.codex/infinite-mission by default.
"""

from __future__ import annotations

import argparse
import contextlib
import datetime as dt
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import signal
import subprocess
import sys
import time
import uuid
from typing import Any, Callable, Iterator


DEFAULT_TIMEOUT_SECONDS = 21_600
SKILL_NAME = "infinite-mission-codex"
DESKTOP_CODEX = Path("/Applications/ChatGPT.app/Contents/Resources/codex")


class AdapterError(RuntimeError):
    pass


def utc_now() -> str:
    return dt.datetime.now(dt.timezone.utc).isoformat()


def state_root() -> Path:
    configured = os.environ.get("IM_CODEX_STATE_DIR")
    root = Path(configured).expanduser() if configured else Path.home() / ".codex" / "infinite-mission"
    root.mkdir(parents=True, exist_ok=True)
    with contextlib.suppress(OSError):
        root.chmod(0o700)
    return root


def find_workspace(start: Path) -> Path:
    current = start.expanduser().resolve()
    if current.is_file():
        current = current.parent
    while True:
        if (current / ".im").is_dir():
            return current
        if current.parent == current:
            raise AdapterError("Not an InfiniteMission workspace. Run 'im init' first.")
        current = current.parent


def executable(candidate: str | None) -> str | None:
    if not candidate:
        return None
    expanded = str(Path(candidate).expanduser())
    resolved = expanded if "/" in expanded else shutil.which(expanded)
    if resolved and Path(resolved).is_file() and os.access(resolved, os.X_OK):
        return str(Path(resolved).resolve())
    return None


def resolve_im(explicit: str | None) -> str:
    for candidate in (explicit, os.environ.get("IM_CODEX_IM_BIN"), shutil.which("im")):
        resolved = executable(candidate)
        if resolved:
            return resolved
    raise AdapterError("Cannot find the `im` executable. Set IM_CODEX_IM_BIN or add im to PATH.")


def supports_queue(candidate: str) -> bool:
    try:
        result = subprocess.run(
            [candidate, "queue", "--help"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=10,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    return result.returncode == 0 and "--thread" in result.stdout and "--message" in result.stdout


def resolve_codex(explicit: str | None) -> str:
    candidates = (
        explicit,
        os.environ.get("IM_CODEX_CODEX_BIN"),
        shutil.which("codex"),
        str(DESKTOP_CODEX) if DESKTOP_CODEX.exists() else None,
    )
    checked: set[str] = set()
    for candidate in candidates:
        resolved = executable(candidate)
        if resolved and resolved not in checked:
            checked.add(resolved)
            if supports_queue(resolved):
                return resolved
    raise AdapterError(
        "Cannot find a Codex CLI with `codex queue` support. "
        "Set IM_CODEX_CODEX_BIN or install Codex CLI 0.149.0 or newer."
    )


def state_key(workspace: Path, agent: str) -> str:
    digest = hashlib.sha256(f"{workspace}\0{agent}".encode()).hexdigest()[:20]
    label = re.sub(r"[^A-Za-z0-9_.-]+", "-", agent).strip("-")[:32] or "agent"
    return f"{label}-{digest}"


def state_dir_for(workspace: Path, agent: str) -> Path:
    directory = state_root() / state_key(workspace, agent)
    directory.mkdir(parents=True, exist_ok=True)
    with contextlib.suppress(OSError):
        directory.chmod(0o700)
    return directory


@contextlib.contextmanager
def locked(path: Path, *, blocking: bool = True) -> Iterator[None]:
    path.parent.mkdir(parents=True, exist_ok=True)
    handle = path.open("a+")
    try:
        operation = fcntl.LOCK_EX | (0 if blocking else fcntl.LOCK_NB)
        fcntl.flock(handle.fileno(), operation)
        yield
    finally:
        handle.close()


def read_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text())
    except FileNotFoundError:
        return {}
    except (OSError, json.JSONDecodeError) as exc:
        raise AdapterError(f"Cannot read adapter state {path}: {exc}") from exc
    if not isinstance(value, dict):
        raise AdapterError(f"Invalid adapter state in {path}")
    return value


def write_json(path: Path, value: dict[str, Any]) -> None:
    temporary = path.with_name(f".{path.name}.{os.getpid()}.{uuid.uuid4().hex}.tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")
    with contextlib.suppress(OSError):
        temporary.chmod(0o600)
    os.replace(temporary, path)


def mutate_state(directory: Path, change: Callable[[dict[str, Any]], None]) -> dict[str, Any]:
    with locked(directory / "state.lock"):
        state = read_json(directory / "state.json")
        change(state)
        state["updatedAt"] = utc_now()
        write_json(directory / "state.json", state)
        return state


def pid_alive(pid: Any) -> bool:
    if not isinstance(pid, int) or pid <= 0:
        return False
    try:
        os.kill(pid, 0)
    except OSError:
        return False
    return True


def pid_matches_watcher(pid: Any, directory: Path) -> bool:
    if not pid_alive(pid):
        return False
    try:
        result = subprocess.run(
            ["ps", "-p", str(pid), "-o", "command="],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            timeout=5,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    command = result.stdout
    return (
        result.returncode == 0
        and str(Path(__file__).resolve()) in command
        and "_watch" in command
        and str(directory) in command
    )


def require_thread(explicit: str | None) -> str:
    thread = explicit or os.environ.get("CODEX_THREAD_ID") or os.environ.get("CODEX_SESSION_ID")
    if not thread:
        raise AdapterError(
            "No Codex thread id is available. Run this from Codex Desktop or pass --thread explicitly."
        )
    return thread


def wait_command(agent: str) -> str:
    return shlex.join(["im", "receive", agent, "--wait"])


def command_wait(args: argparse.Namespace) -> int:
    workspace = find_workspace(Path(args.workspace) if args.workspace else Path.cwd())
    agent = args.agent
    thread = require_thread(args.thread)
    im_bin = resolve_im(args.im_bin)
    codex_bin = resolve_codex(args.codex_bin)
    directory = state_dir_for(workspace, agent)
    display_command = wait_command(agent)

    with locked(directory / "state.lock"):
        state_path = directory / "state.json"
        state = read_json(state_path)
        pending_exists = (directory / "pending.json").exists()
        state.update(
            {
                "schemaVersion": 1,
                "agentId": agent,
                "workspace": str(workspace),
                "threadId": thread,
                "imBin": im_bin,
                "codexBin": codex_bin,
                "waitCommand": display_command,
                "autoRenew": True,
                "timeoutSeconds": args.timeout,
                "status": "pending" if pending_exists else "armed",
                "updatedAt": utc_now(),
            }
        )
        if not pending_exists:
            state.pop("lastWaitResult", None)
            state.pop("lastWaitReason", None)
        watch_handle = (directory / "watch.lock").open("a+")
        try:
            fcntl.flock(watch_handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            watch_handle.close()
            write_json(state_path, state)
            print(
                f"Codex watcher already armed for: {display_command} "
                f"(auto-renew enabled); "
                f"updated its Codex target to {thread}."
            )
            return 0

        try:
            log_path = directory / "watcher.log"
            log_fd = os.open(log_path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
            try:
                process = subprocess.Popen(
                    [
                        sys.executable,
                        str(Path(__file__).resolve()),
                        "_watch",
                        "--state-dir",
                        str(directory),
                        "--lock-fd",
                        str(watch_handle.fileno()),
                    ],
                    cwd=workspace,
                    stdin=subprocess.DEVNULL,
                    stdout=log_fd,
                    stderr=subprocess.STDOUT,
                    start_new_session=True,
                    pass_fds=(watch_handle.fileno(),),
                )
            finally:
                os.close(log_fd)
        finally:
            watch_handle.close()
        state["watcherPid"] = process.pid
        state["armedAt"] = utc_now()
        write_json(state_path, state)

    print(f"Armed Codex watcher for: {display_command} (auto-renew enabled).")
    print(f"Target Codex task: {thread}.")
    print("End this Codex turn; the watcher will use `codex queue` when IM returns an event.")
    return 0


def classify_notice(returncode: int, output: str) -> str:
    if returncode != 0:
        return "receive-error"
    if "[membership]" in output:
        return "membership"
    return "arrival"


def durable_notice_path(directory: Path, notice: dict[str, Any]) -> Path:
    notification_id = notice.get("notificationId")
    if not isinstance(notification_id, str) or not re.fullmatch(r"[0-9a-f]{32}", notification_id):
        raise AdapterError("Pending notification has an invalid notificationId.")
    notices = directory / "notices"
    notices.mkdir(parents=True, exist_ok=True)
    with contextlib.suppress(OSError):
        notices.chmod(0o700)
    path = notices / f"{notification_id}.json"
    if not path.exists():
        write_json(path, notice)
    return path


def persist_pending(directory: Path, returncode: int, output: str) -> dict[str, Any]:
    notice = {
        "schemaVersion": 1,
        "notificationId": uuid.uuid4().hex,
        "createdAt": utc_now(),
        "kind": classify_notice(returncode, output),
        "receiveExitCode": returncode,
        "receiveOutput": output,
    }
    durable_notice_path(directory, notice)
    write_json(directory / "pending.json", notice)

    def mark_pending(state: dict[str, Any]) -> None:
        state["status"] = "pending"
        state["notificationId"] = notice["notificationId"]
        state["lastReceiveExitCode"] = returncode
        state["lastWaitResult"] = "ended"
        state["lastWaitReason"] = notice["kind"]

    mutate_state(directory, mark_pending)
    return notice


def auto_renew_after_notice(notice: dict[str, Any]) -> bool:
    if notice.get("kind") == "receive-error":
        return False
    output = notice.get("receiveOutput", "")
    return not (
        notice.get("kind") == "membership"
        and isinstance(output, str)
        and "[membership] you are no longer an active member" in output
    )


def queued_message(state: dict[str, Any], notice: dict[str, Any], notice_path: Path) -> str:
    next_wait = "auto-renewing" if auto_renew_after_notice(notice) else "paused"
    return "\n".join(
        [
            f"${SKILL_NAME}",
            "InfiniteMission notification received by the Codex watcher.",
            "source=im-codex-watcher",
            "protocol=im-codex-wake/v1",
            f"waitCommand={state.get('waitCommand', wait_command(state['agentId']))}",
            "waitResult=ended",
            f"reason={notice['kind']}",
            f"nextWait={next_wait}",
            f"notificationId={notice['notificationId']}",
            f"agentId={state['agentId']}",
            f"workspace={state['workspace']}",
            f"noticePath={notice_path}",
            "Treat this as a wake event and query live IM state before acting.",
        ]
    )


def deliver_pending(directory: Path) -> str:
    pending_path = directory / "pending.json"
    delay = 2
    while pending_path.exists():
        state = read_json(directory / "state.json")
        if state.get("status") == "stop-requested":
            return "stopped"
        notice = read_json(pending_path)
        notice_path = durable_notice_path(directory, notice)
        message = queued_message(state, notice, notice_path)
        try:
            result = subprocess.run(
                [
                    state["codexBin"],
                    "queue",
                    "--thread",
                    state["threadId"],
                    "--message",
                    message,
                ],
                cwd=state["workspace"],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                timeout=30,
                check=False,
            )
            delivery_output = result.stdout.strip()
            returncode = result.returncode
        except (OSError, subprocess.TimeoutExpired) as exc:
            delivery_output = str(exc)
            returncode = 1

        if returncode == 0:
            delivered_path = directory / "last-delivered.json"
            os.replace(pending_path, delivered_path)

            def mark_delivered(current: dict[str, Any]) -> None:
                current["status"] = "delivered"
                current["deliveredAt"] = utc_now()
                current["lastQueueOutput"] = delivery_output
                current.pop("lastError", None)

            mutate_state(directory, mark_delivered)
            return "delivered"

        def mark_retry(current: dict[str, Any]) -> None:
            current["status"] = "pending"
            current["lastError"] = f"codex queue exited {returncode}: {delivery_output}"

        mutate_state(directory, mark_retry)
        time.sleep(delay)
        delay = min(delay * 2, 60)
    return "stopped"


def mark_event_rearmed(directory: Path) -> None:
    def change(state: dict[str, Any]) -> None:
        state["status"] = "armed"
        state["lastAutoRenewedAt"] = utc_now()

    mutate_state(directory, change)


def mark_notice_paused(directory: Path, notice: dict[str, Any]) -> None:
    def change(state: dict[str, Any]) -> None:
        state["status"] = "paused"
        state["watcherPid"] = None
        state["pausedAt"] = utc_now()
        state["pauseReason"] = notice["kind"]

    mutate_state(directory, change)


def command_watch(args: argparse.Namespace) -> int:
    directory = Path(args.state_dir).resolve()
    watch_handle = os.fdopen(args.lock_fd, "a+", closefd=True)

    try:
        def mark_running(state: dict[str, Any]) -> None:
            state["watcherPid"] = os.getpid()
            state["status"] = "pending" if (directory / "pending.json").exists() else "armed"

        mutate_state(directory, mark_running)

        if (directory / "pending.json").exists():
            pending = read_json(directory / "pending.json")
            delivery = deliver_pending(directory)
            if delivery != "delivered":
                return 0
            if not auto_renew_after_notice(pending):
                mark_notice_paused(directory, pending)
                return 0
            mark_event_rearmed(directory)

        while True:
            state = read_json(directory / "state.json")
            if state.get("status") == "stop-requested":
                return 0
            result = subprocess.run(
                [
                    state["imBin"],
                    "receive",
                    state["agentId"],
                    "--wait",
                    "--timeout",
                    str(state["timeoutSeconds"]),
                ],
                cwd=state["workspace"],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                check=False,
            )
            output = result.stdout
            timeout_line = f"No new messages (timed out after {state['timeoutSeconds']}s)."
            if result.returncode == 0 and output.strip() == timeout_line:
                def mark_rearmed(current: dict[str, Any]) -> None:
                    current["status"] = "armed"
                    current["lastTimeoutAt"] = utc_now()

                mutate_state(directory, mark_rearmed)
                continue

            notice = persist_pending(directory, result.returncode, output)
            delivery = deliver_pending(directory)
            if delivery != "delivered":
                return 0
            if not auto_renew_after_notice(notice):
                mark_notice_paused(directory, notice)
                return 0
            mark_event_rearmed(directory)
    finally:
        watch_handle.close()


def state_for_args(args: argparse.Namespace) -> tuple[Path, str, Path]:
    workspace = find_workspace(Path(args.workspace) if args.workspace else Path.cwd())
    return workspace, args.agent, state_dir_for(workspace, args.agent)


def command_status(args: argparse.Namespace) -> int:
    _, _, directory = state_for_args(args)
    state = read_json(directory / "state.json")
    if not state:
        print("No Codex watcher state for this workspace and agent.")
        return 1
    state["watcherAlive"] = pid_matches_watcher(state.get("watcherPid"), directory)
    state["pendingNotice"] = (directory / "pending.json").exists()
    print(json.dumps(state, indent=2, sort_keys=True))
    return 0


def command_stop(args: argparse.Namespace) -> int:
    _, agent, directory = state_for_args(args)
    state = read_json(directory / "state.json")
    if not state:
        print("No Codex watcher state for this workspace and agent.")
        return 1
    pid = state.get("watcherPid")

    def request_stop(current: dict[str, Any]) -> None:
        current["status"] = "stop-requested"

    mutate_state(directory, request_stop)
    if pid_matches_watcher(pid, directory):
        try:
            os.killpg(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass

    def mark_stopped(current: dict[str, Any]) -> None:
        current["status"] = "stopped"
        current["watcherPid"] = None
        current["stoppedAt"] = utc_now()

    mutate_state(directory, mark_stopped)
    print(f"Stopped InfiniteMission watcher for {agent}.")
    return 0


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(
        description="Arm a Codex Desktop wakeup for InfiniteMission without changing IM."
    )
    commands = result.add_subparsers(dest="command", required=True)

    wait = commands.add_parser("wait", help="arm or retarget the one-shot watcher")
    wait.add_argument("agent")
    wait.add_argument("--workspace")
    wait.add_argument("--thread")
    wait.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT_SECONDS)
    wait.add_argument("--im-bin")
    wait.add_argument("--codex-bin")
    wait.set_defaults(handler=command_wait)

    for name, help_text, handler in (
        ("status", "show watcher state", command_status),
        ("stop", "stop the watcher", command_stop),
    ):
        command = commands.add_parser(name, help=help_text)
        command.add_argument("agent")
        command.add_argument("--workspace")
        command.set_defaults(handler=handler)

    watch = commands.add_parser("_watch", help=argparse.SUPPRESS)
    watch.add_argument("--state-dir", required=True)
    watch.add_argument("--lock-fd", type=int, required=True)
    watch.set_defaults(handler=command_watch)
    return result


def main() -> int:
    args = parser().parse_args()
    if getattr(args, "timeout", 1) <= 0:
        print("error: --timeout must be greater than zero", file=sys.stderr)
        return 2
    try:
        return args.handler(args)
    except AdapterError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
