from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest


SCRIPT = Path(__file__).parents[1] / "scripts" / "im_codex.py"


class ImCodexTest(unittest.TestCase):
    def make_executable(self, path: Path, body: str) -> None:
        path.write_text(f"#!{sys.executable}\n{body}")
        path.chmod(0o755)

    def test_wait_rearms_after_timeout_and_after_delivered_event(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            workspace = root / "workspace"
            workspace.mkdir()
            (workspace / ".im").mkdir()
            state_root = root / "state"
            capture = root / "queued.json"
            receive_count = root / "receive-count"
            renewed = root / "renewed"

            fake_im = root / "im"
            self.make_executable(
                fake_im,
                "import os, pathlib, sys\n"
                "assert sys.argv[1:4] == ['receive', 'worker', '--wait']\n"
                "counter = pathlib.Path(os.environ['RECEIVE_COUNT'])\n"
                "count = int(counter.read_text()) + 1 if counter.exists() else 1\n"
                "counter.write_text(str(count))\n"
                "if count == 1:\n"
                "    print('No new messages (timed out after 1s).')\n"
                "elif count == 2:\n"
                "    print('[station build] No new messages (timed out after 1s). mission ms_test arrived')\n"
                "else:\n"
                "    pathlib.Path(os.environ['RENEWED']).write_text('yes')\n"
                "    import time; time.sleep(10)\n",
            )

            fake_codex = root / "codex"
            self.make_executable(
                fake_codex,
                "import json, os, pathlib, sys\n"
                "if sys.argv[1:] == ['queue', '--help']:\n"
                "    print('queue --thread THREAD --message TEXT')\n"
                "    raise SystemExit(0)\n"
                "pathlib.Path(os.environ['QUEUE_CAPTURE']).write_text(json.dumps(sys.argv[1:]))\n",
            )

            environment = os.environ.copy()
            environment.update(
                {
                    "CODEX_THREAD_ID": "thread-test",
                    "IM_CODEX_STATE_DIR": str(state_root),
                    "IM_CODEX_IM_BIN": str(fake_im),
                    "IM_CODEX_CODEX_BIN": str(fake_codex),
                    "QUEUE_CAPTURE": str(capture),
                    "RECEIVE_COUNT": str(receive_count),
                    "RENEWED": str(renewed),
                }
            )
            try:
                armed = subprocess.run(
                    [sys.executable, str(SCRIPT), "wait", "worker", "--timeout", "1"],
                    cwd=workspace,
                    env=environment,
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    check=False,
                )
                self.assertEqual(armed.returncode, 0, armed.stdout)
                self.assertIn(
                    "Armed Codex watcher for: im receive worker --wait (auto-renew enabled).",
                    armed.stdout,
                )

                deadline = time.monotonic() + 5
                while (not capture.exists() or not renewed.exists()) and time.monotonic() < deadline:
                    time.sleep(0.05)
                self.assertTrue(capture.exists(), "watcher did not invoke codex queue")
                self.assertTrue(renewed.exists(), "watcher did not rearm after queue delivery")
                self.assertEqual(receive_count.read_text(), "3")

                queued = json.loads(capture.read_text())
                self.assertEqual(queued[0:3], ["queue", "--thread", "thread-test"])
                message = queued[queued.index("--message") + 1]
                self.assertIn("$infinite-mission-codex", message)
                self.assertIn("protocol=im-codex-wake/v1", message)
                self.assertIn("waitCommand=im receive worker --wait", message)
                self.assertIn("waitResult=ended", message)
                self.assertIn("reason=arrival", message)
                self.assertIn("nextWait=auto-renewing", message)
                self.assertIn("agentId=worker", message)
                self.assertIn(f"workspace={workspace.resolve()}", message)
                notice_line = next(
                    line for line in message.splitlines() if line.startswith("noticePath=")
                )
                notice_path = Path(notice_line.removeprefix("noticePath="))
                self.assertTrue(notice_path.exists())
                self.assertEqual(json.loads(notice_path.read_text())["kind"], "arrival")
                self.assertNotEqual(notice_path.name, "pending.json")

                status = subprocess.run(
                    [sys.executable, str(SCRIPT), "status", "worker"],
                    cwd=workspace,
                    env=environment,
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    check=False,
                )
                self.assertEqual(status.returncode, 0, status.stdout)
                state = json.loads(status.stdout)
                self.assertEqual(state["status"], "armed")
                self.assertTrue(state["watcherAlive"])
                self.assertTrue(state["autoRenew"])
                self.assertEqual(state["waitCommand"], "im receive worker --wait")
                self.assertEqual(state["lastWaitResult"], "ended")
                self.assertEqual(state["lastWaitReason"], "arrival")
                self.assertFalse(state["pendingNotice"])
            finally:
                subprocess.run(
                    [sys.executable, str(SCRIPT), "stop", "worker"],
                    cwd=workspace,
                    env=environment,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    check=False,
                )

    def test_wait_requires_a_codex_thread(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            workspace = root / "workspace"
            workspace.mkdir()
            (workspace / ".im").mkdir()
            fake_im = root / "im"
            self.make_executable(fake_im, "raise SystemExit(0)\n")
            environment = os.environ.copy()
            environment.pop("CODEX_THREAD_ID", None)
            environment.pop("CODEX_SESSION_ID", None)
            environment["IM_CODEX_STATE_DIR"] = str(root / "state")
            environment["IM_CODEX_IM_BIN"] = str(fake_im)
            result = subprocess.run(
                [sys.executable, str(SCRIPT), "wait", "worker"],
                cwd=workspace,
                env=environment,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                check=False,
            )
            self.assertEqual(result.returncode, 1)
            self.assertIn("No Codex thread id", result.stdout)

    def test_receive_error_queues_once_and_pauses(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            workspace = root / "workspace"
            workspace.mkdir()
            (workspace / ".im").mkdir()
            capture = root / "queued.json"
            receive_count = root / "receive-count"

            fake_im = root / "im"
            self.make_executable(
                fake_im,
                "import os, pathlib\n"
                "counter = pathlib.Path(os.environ['RECEIVE_COUNT'])\n"
                "count = int(counter.read_text()) + 1 if counter.exists() else 1\n"
                "counter.write_text(str(count))\n"
                "print('session replaced')\n"
                "raise SystemExit(7)\n",
            )
            fake_codex = root / "codex"
            self.make_executable(
                fake_codex,
                "import json, os, pathlib, sys\n"
                "if sys.argv[1:] == ['queue', '--help']:\n"
                "    print('queue --thread THREAD --message TEXT')\n"
                "    raise SystemExit(0)\n"
                "pathlib.Path(os.environ['QUEUE_CAPTURE']).write_text(json.dumps(sys.argv[1:]))\n",
            )
            environment = os.environ.copy()
            environment.update(
                {
                    "CODEX_THREAD_ID": "thread-test",
                    "IM_CODEX_STATE_DIR": str(root / "state"),
                    "IM_CODEX_IM_BIN": str(fake_im),
                    "IM_CODEX_CODEX_BIN": str(fake_codex),
                    "QUEUE_CAPTURE": str(capture),
                    "RECEIVE_COUNT": str(receive_count),
                }
            )
            armed = subprocess.run(
                [sys.executable, str(SCRIPT), "wait", "worker", "--timeout", "1"],
                cwd=workspace,
                env=environment,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                check=False,
            )
            self.assertEqual(armed.returncode, 0, armed.stdout)

            deadline = time.monotonic() + 5
            state = {}
            while time.monotonic() < deadline:
                status = subprocess.run(
                    [sys.executable, str(SCRIPT), "status", "worker"],
                    cwd=workspace,
                    env=environment,
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    check=False,
                )
                if status.returncode == 0:
                    state = json.loads(status.stdout)
                    if state.get("status") == "paused":
                        break
                time.sleep(0.05)

            self.assertEqual(state.get("status"), "paused")
            self.assertFalse(state["watcherAlive"])
            self.assertEqual(state["pauseReason"], "receive-error")
            self.assertEqual(receive_count.read_text(), "1")
            queued = json.loads(capture.read_text())
            message = queued[queued.index("--message") + 1]
            self.assertIn("reason=receive-error", message)
            self.assertIn("nextWait=paused", message)

    def test_second_wait_retargets_the_existing_watcher(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            workspace = root / "workspace"
            workspace.mkdir()
            (workspace / ".im").mkdir()
            started = root / "receive-started"

            fake_im = root / "im"
            self.make_executable(
                fake_im,
                "import os, pathlib, time\n"
                "pathlib.Path(os.environ['RECEIVE_STARTED']).write_text('started')\n"
                "time.sleep(10)\n",
            )
            fake_codex = root / "codex"
            self.make_executable(
                fake_codex,
                "import sys\n"
                "if sys.argv[1:] == ['queue', '--help']:\n"
                "    print('queue --thread THREAD --message TEXT')\n"
                "    raise SystemExit(0)\n"
                "raise SystemExit(1)\n",
            )
            environment = os.environ.copy()
            environment.update(
                {
                    "CODEX_THREAD_ID": "thread-one",
                    "IM_CODEX_STATE_DIR": str(root / "state"),
                    "IM_CODEX_IM_BIN": str(fake_im),
                    "IM_CODEX_CODEX_BIN": str(fake_codex),
                    "RECEIVE_STARTED": str(started),
                }
            )
            try:
                first = subprocess.run(
                    [sys.executable, str(SCRIPT), "wait", "worker", "--timeout", "20"],
                    cwd=workspace,
                    env=environment,
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    check=False,
                )
                self.assertEqual(first.returncode, 0, first.stdout)
                deadline = time.monotonic() + 3
                while not started.exists() and time.monotonic() < deadline:
                    time.sleep(0.05)
                self.assertTrue(started.exists(), "first watcher did not start")

                environment["CODEX_THREAD_ID"] = "thread-two"
                second = subprocess.run(
                    [sys.executable, str(SCRIPT), "wait", "worker", "--timeout", "20"],
                    cwd=workspace,
                    env=environment,
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    check=False,
                )
                self.assertEqual(second.returncode, 0, second.stdout)
                self.assertIn("already armed", second.stdout)

                status = subprocess.run(
                    [sys.executable, str(SCRIPT), "status", "worker"],
                    cwd=workspace,
                    env=environment,
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    check=False,
                )
                self.assertEqual(status.returncode, 0, status.stdout)
                self.assertEqual(json.loads(status.stdout)["threadId"], "thread-two")
            finally:
                subprocess.run(
                    [sys.executable, str(SCRIPT), "stop", "worker"],
                    cwd=workspace,
                    env=environment,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    check=False,
                )


if __name__ == "__main__":
    unittest.main()
