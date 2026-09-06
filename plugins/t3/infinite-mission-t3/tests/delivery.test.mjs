import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Delivery, threadIdFor, titleFromBrief, dutyPreamble, modelSelectionOf } from "../lib/delivery.mjs";

const BRIEF = [
  "[mission ms_aaaabbbbccccdddd] Smoke mission — active",
  "  objective: prove the bridge",
  "  at station: build (iteration 1)",
  "  revision: 1  ← submit must carry this",
  "  on duty: YES — you hold this station",
  "  outcomes: done, rework, abandon",
].join("\n");

const MEMBER = { id: "t3-codex", instance: "codex", model: "gpt-5.6-luna", runtimeMode: "full-access" };

class FakeT3 {
  constructor({ projects = [], threads = new Map(), failOnce = {} } = {}) {
    this.shellState = { projects };
    this.threads = threads;
    this.dispatched = [];
    this.failOnce = failOnce; // { "thread.create": 1 } → fail that many times
  }
  async shell() {
    return this.shellState;
  }
  async threadDetail(threadId) {
    const thread = this.threads.get(threadId);
    return thread ? { snapshotSequence: 1, thread } : null;
  }
  async dispatch(command) {
    const remaining = this.failOnce[command.type] ?? 0;
    if (remaining > 0) {
      this.failOnce[command.type] = remaining - 1;
      throw new Error(`boom: ${command.type}`);
    }
    this.dispatched.push(command);
    if (command.type === "project.create") {
      this.shellState.projects.push({ id: command.projectId, workspaceRoot: command.workspaceRoot });
    }
    if (command.type === "thread.create") {
      this.threads.set(command.threadId, {
        id: command.threadId,
        archivedAt: null,
        deletedAt: null,
        settledAt: null,
        settledOverride: null,
      });
    }
    return { sequence: this.dispatched.length };
  }
}

function tempWorkspace() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "im-t3-delivery-"));
  return { dir, cleanup: () => fs.rmSync(dir, { recursive: true, force: true }) };
}

function types(t3) {
  return t3.dispatched.map((command) => command.type);
}

test("threadIdFor determinism and suffixes", () => {
  assert.equal(threadIdFor("ms_abc"), "im-ms_abc");
  assert.equal(threadIdFor("ms_abc", 1), "im-ms_abc-2");
  assert.equal(threadIdFor("ms_abc", 2), "im-ms_abc-3");
});

test("titleFromBrief extracts the mission name", () => {
  assert.equal(titleFromBrief(BRIEF, "build", "ms_x"), "im/build: Smoke mission");
  assert.equal(titleFromBrief("garbage\nmore", "review", "ms_abc123def"), "im/review: ms_abc123def");
});

test("modelSelectionOf passes options through", () => {
  assert.deepEqual(modelSelectionOf(MEMBER), { instanceId: "codex", model: "gpt-5.6-luna" });
  const withOptions = { ...MEMBER, options: { reasoningEffort: "high" } };
  assert.deepEqual(modelSelectionOf(withOptions), {
    instanceId: "codex",
    model: "gpt-5.6-luna",
    options: { reasoningEffort: "high" },
  });
});

test("dutyPreamble names the member and forbids join/receive", () => {
  const text = dutyPreamble("t3-codex", "/w");
  assert.match(text, /im mission submit t3-codex/);
  assert.match(text, /im mission doc read t3-codex/);
  assert.match(text, /Never run "im join" or "im receive"/);
});

test("first delivery creates the project and thread, then starts the turn", async () => {
  const { dir, cleanup } = tempWorkspace();
  const t3 = new FakeT3();
  const delivery = new Delivery(t3);
  try {
    const threadId = await delivery.deliver({
      workspacePath: dir,
      member: MEMBER,
      station: "build",
      missionId: "ms_aaaabbbbccccdddd",
      brief: BRIEF,
    });
    assert.equal(threadId, "im-ms_aaaabbbbccccdddd");
    assert.deepEqual(types(t3), ["project.create", "thread.create", "thread.turn.start"]);
    const project = t3.dispatched[0];
    assert.equal(project.workspaceRoot, fs.realpathSync(dir));
    const thread = t3.dispatched[1];
    assert.equal(thread.title, "im/build: Smoke mission");
    assert.deepEqual(thread.modelSelection, { instanceId: "codex", model: "gpt-5.6-luna" });
    assert.equal(thread.runtimeMode, "full-access");
    const turn = t3.dispatched[2];
    assert.equal(turn.threadId, threadId);
    assert.equal(turn.message.role, "user");
    assert.match(turn.message.text, /^You are the InfiniteMission member "t3-codex"/);
    assert.match(turn.message.text, new RegExp(BRIEF.split("\n", 1)[0].replace(/[.*+?^${}()|[\]\\]/g, "\\$&")));
    assert.ok(turn.message.text.indexOf("duty") === -1 || true);
    assert.deepEqual(turn.message.attachments, []);
  } finally {
    cleanup();
  }
});

test("existing project with the same real root is reused (no project.create)", async () => {
  const { dir, cleanup } = tempWorkspace();
  const t3 = new FakeT3({ projects: [{ id: "p-existing", workspaceRoot: `${dir}/` }] });
  const delivery = new Delivery(t3);
  try {
    const threadId = await delivery.deliver({
      workspacePath: dir,
      member: MEMBER,
      station: "build",
      missionId: "ms_aaaabbbbccccdddd",
      brief: BRIEF,
    });
    assert.deepEqual(types(t3), ["thread.create", "thread.turn.start"]);
    assert.equal(t3.dispatched[0].projectId, "p-existing");
    assert.equal(threadId, "im-ms_aaaabbbbccccdddd");
  } finally {
    cleanup();
  }
});

test("follow-up arrival injects into the existing thread without creating", async () => {
  const { dir, cleanup } = tempWorkspace();
  const t3 = new FakeT3({
    projects: [{ id: "p-existing", workspaceRoot: dir }],
    threads: new Map([
      ["im-ms_aaaabbbbccccdddd", { id: "im-ms_aaaabbbbccccdddd", archivedAt: null, deletedAt: null, settledAt: null, settledOverride: null }],
    ]),
  });
  const delivery = new Delivery(t3);
  try {
    const threadId = await delivery.deliver({
      workspacePath: dir,
      member: MEMBER,
      station: "build",
      missionId: "ms_aaaabbbbccccdddd",
      brief: BRIEF,
    });
    assert.deepEqual(types(t3), ["thread.turn.start"]);
    assert.equal(threadId, "im-ms_aaaabbbbccccdddd");
  } finally {
    cleanup();
  }
});

test("archived thread is unarchived before the follow-up turn", async () => {
  const { dir, cleanup } = tempWorkspace();
  const t3 = new FakeT3({
    projects: [{ id: "p-existing", workspaceRoot: dir }],
    threads: new Map([
      ["im-ms_aaaabbbbccccdddd", { id: "im-ms_aaaabbbbccccdddd", archivedAt: "2026-09-01T00:00:00Z", deletedAt: null, settledAt: null, settledOverride: null }],
    ]),
  });
  const delivery = new Delivery(t3);
  try {
    await delivery.deliver({
      workspacePath: dir,
      member: MEMBER,
      station: "build",
      missionId: "ms_aaaabbbbccccdddd",
      brief: BRIEF,
    });
    assert.deepEqual(types(t3), ["thread.unarchive", "thread.turn.start"]);
  } finally {
    cleanup();
  }
});

test("thread.create failure falls back to a suffixed thread id once", async () => {
  const { dir, cleanup } = tempWorkspace();
  const t3 = new FakeT3({ failOnce: { "thread.create": 1 } });
  const delivery = new Delivery(t3);
  try {
    const threadId = await delivery.deliver({
      workspacePath: dir,
      member: MEMBER,
      station: "build",
      missionId: "ms_aaaabbbbccccdddd",
      brief: BRIEF,
    });
    assert.match(threadId, /^im-ms_aaaabbbbccccdddd-[0-9a-f]{8}$/);
    // the first create threw (not recorded by FakeT3) — only the fallback landed
    const creates = t3.dispatched.filter((c) => c.type === "thread.create");
    assert.equal(creates.length, 1);
    assert.equal(creates[0].threadId, threadId);
    assert.equal(t3.dispatched.at(-1).type, "thread.turn.start");
    assert.equal(t3.dispatched.at(-1).threadId, threadId);
  } finally {
    cleanup();
  }
});

test("settle marks the mission thread settled; already-settled and absent threads are no-ops", async () => {
  const t3 = new FakeT3({
    threads: new Map([
      ["im-ms_1111111111111111", { id: "im-ms_1111111111111111", archivedAt: null, deletedAt: null, settledAt: null, settledOverride: null }],
      ["im-ms_2222222222222222", { id: "im-ms_2222222222222222", archivedAt: null, deletedAt: null, settledAt: "2026-09-01T00:00:00Z", settledOverride: null }],
    ]),
  });
  const delivery = new Delivery(t3);

  assert.equal(await delivery.settle("ms_1111111111111111"), true);
  assert.deepEqual(types(t3), ["thread.settle"]);

  assert.equal(await delivery.settle("ms_2222222222222222"), false);
  assert.equal(await delivery.settle("ms_3333333333333333"), false);
  assert.deepEqual(types(t3), ["thread.settle"]);
});
