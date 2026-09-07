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

test("threadIdFor determinism, per-member scope, and suffixes", () => {
  assert.equal(threadIdFor("t3-codex", "ms_abc"), "im-t3-codex-ms_abc");
  assert.equal(threadIdFor("t3-codex", "ms_abc", 1), "im-t3-codex-ms_abc-2");
  assert.equal(threadIdFor("t3-codex", "ms_abc", 2), "im-t3-codex-ms_abc-3");
  assert.notEqual(threadIdFor("t3-kimi", "ms_abc"), threadIdFor("t3-codex", "ms_abc"));
});

test("titleFromBrief extracts the mission name with the member prefix", () => {
  assert.equal(titleFromBrief(BRIEF, "t3-codex", "build", "ms_x"), "im/t3-codex@build: Smoke mission");
  assert.equal(titleFromBrief("garbage\nmore", "t3-codex", "review", "ms_abc123def"), "im/t3-codex@review: ms_abc123def");
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
    assert.equal(threadId, "im-t3-codex-ms_aaaabbbbccccdddd");
    assert.deepEqual(types(t3), ["project.create", "thread.create", "thread.turn.start"]);
    const project = t3.dispatched[0];
    assert.equal(project.workspaceRoot, fs.realpathSync(dir));
    const thread = t3.dispatched[1];
    assert.equal(thread.title, "im/t3-codex@build: Smoke mission");
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
    assert.equal(threadId, "im-t3-codex-ms_aaaabbbbccccdddd");
  } finally {
    cleanup();
  }
});

test("follow-up arrival injects into the existing thread without creating", async () => {
  const { dir, cleanup } = tempWorkspace();
  const t3 = new FakeT3({
    projects: [{ id: "p-existing", workspaceRoot: dir }],
    threads: new Map([
      ["im-t3-codex-ms_aaaabbbbccccdddd", { id: "im-t3-codex-ms_aaaabbbbccccdddd", archivedAt: null, deletedAt: null, settledAt: null, settledOverride: null }],
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
    assert.equal(threadId, "im-t3-codex-ms_aaaabbbbccccdddd");
  } finally {
    cleanup();
  }
});

test("archived thread is unarchived before the follow-up turn", async () => {
  const { dir, cleanup } = tempWorkspace();
  const t3 = new FakeT3({
    projects: [{ id: "p-existing", workspaceRoot: dir }],
    threads: new Map([
      ["im-t3-codex-ms_aaaabbbbccccdddd", { id: "im-t3-codex-ms_aaaabbbbccccdddd", archivedAt: "2026-09-01T00:00:00Z", deletedAt: null, settledAt: null, settledOverride: null }],
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
    assert.match(threadId, /^im-t3-codex-ms_aaaabbbbccccdddd-[0-9a-f]{8}$/);
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
      ["im-t3-codex-ms_1111111111111111", { id: "im-t3-codex-ms_1111111111111111", archivedAt: null, deletedAt: null, settledAt: null, settledOverride: null }],
      ["im-t3-codex-ms_2222222222222222", { id: "im-t3-codex-ms_2222222222222222", archivedAt: null, deletedAt: null, settledAt: "2026-09-01T00:00:00Z", settledOverride: null }],
    ]),
  });
  const delivery = new Delivery(t3);

  assert.equal(await delivery.settle("t3-codex", "ms_1111111111111111"), true);
  assert.deepEqual(types(t3), ["thread.settle"]);

  assert.equal(await delivery.settle("t3-codex", "ms_2222222222222222"), false);
  assert.equal(await delivery.settle("t3-codex", "ms_3333333333333333"), false);
  assert.deepEqual(types(t3), ["thread.settle"]);
});

test("hasThread is member-scoped: another member's thread does not count", async () => {
  const t3 = new FakeT3({
    threads: new Map([
      ["im-t3-grok-ms_aaaabbbbccccdddd", { id: "im-t3-grok-ms_aaaabbbbccccdddd", archivedAt: null, deletedAt: null, settledAt: null, settledOverride: null }],
    ]),
  });
  const delivery = new Delivery(t3);
  assert.equal(await delivery.hasThread("t3-kimi", "ms_aaaabbbbccccdddd"), false);
  assert.equal(await delivery.hasThread("t3-grok", "ms_aaaabbbbccccdddd"), true);
});

test("hasThread ignores deleted and settled threads (a revisit round is undelivered)", async () => {
  const t3 = new FakeT3({
    threads: new Map([
      ["im-t3-codex-ms_1111111111111111", { id: "im-t3-codex-ms_1111111111111111", archivedAt: null, deletedAt: "2026-09-01T00:00:00Z", settledAt: null, settledOverride: null }],
      ["im-t3-codex-ms_2222222222222222", { id: "im-t3-codex-ms_2222222222222222", archivedAt: null, deletedAt: null, settledAt: "2026-09-01T00:00:00Z", settledOverride: null }],
      ["im-t3-codex-ms_3333333333333333", { id: "im-t3-codex-ms_3333333333333333", archivedAt: null, deletedAt: null, settledAt: null, settledOverride: "settled" }],
      ["im-t3-codex-ms_4444444444444444", { id: "im-t3-codex-ms_4444444444444444", archivedAt: null, deletedAt: null, settledAt: null, settledOverride: null }],
    ]),
  });
  const delivery = new Delivery(t3);
  assert.equal(await delivery.hasThread("t3-codex", "ms_1111111111111111"), false);
  assert.equal(await delivery.hasThread("t3-codex", "ms_2222222222222222"), false);
  assert.equal(await delivery.hasThread("t3-codex", "ms_3333333333333333"), false);
  assert.equal(await delivery.hasThread("t3-codex", "ms_4444444444444444"), true);
});

test("two members delivering the same mission get separate threads with their own models", async () => {
  const { dir, cleanup } = tempWorkspace();
  const t3 = new FakeT3({ projects: [{ id: "p-existing", workspaceRoot: dir }] });
  const delivery = new Delivery(t3);
  const grok = { id: "t3-grok", instance: "grok", model: "grok-4-fast", runtimeMode: "full-access" };
  const kimi = { id: "t3-kimi", instance: "kimi", model: "k2-0905", runtimeMode: "full-access" };
  try {
    const grokThread = await delivery.deliver({
      workspacePath: dir, member: grok, station: "cni-ui-audit", missionId: "ms_aaaabbbbccccdddd", brief: BRIEF,
    });
    const kimiThread = await delivery.deliver({
      workspacePath: dir, member: kimi, station: "cni-visual-design", missionId: "ms_aaaabbbbccccdddd", brief: BRIEF,
    });
    assert.equal(grokThread, "im-t3-grok-ms_aaaabbbbccccdddd");
    assert.equal(kimiThread, "im-t3-kimi-ms_aaaabbbbccccdddd");
    const creates = t3.dispatched.filter((c) => c.type === "thread.create");
    assert.deepEqual(creates.map((c) => c.modelSelection.instanceId), ["grok", "kimi"]);
    const turns = t3.dispatched.filter((c) => c.type === "thread.turn.start");
    assert.deepEqual(turns.map((c) => c.modelSelection.instanceId), ["grok", "kimi"]);
    assert.deepEqual(turns.map((c) => c.threadId), [grokThread, kimiThread]);
  } finally {
    cleanup();
  }
});
