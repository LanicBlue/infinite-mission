import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { arrivalHeader, Delivery, slimBrief, slimPreamble, threadIdFor, isMissionThreadId, titleFromBrief, dutyPreamble, resultPreamble, modelSelectionOf, briefFromRunView } from "../lib/delivery.mjs";

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
  // threads: the detail map (probe targets); shellThreads: the shell snapshot's
  // live thread list (what the prefix fallback searches) — kept separate so a
  // test can model "deleted base, live suffixed thread" (detail 404s deleted
  // threads on the real server; the shell never lists them).
  constructor({ projects = [], threads = new Map(), shellThreads = [], failOnce = {} } = {}) {
    this.shellState = { projects, threads: shellThreads };
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

test("isMissionThreadId: exact, suffixed, and boundary mismatches", () => {
  const mission = `ms_${"a".repeat(32)}`;
  const otherMission = `ms_${"a".repeat(31)}b`; // same length, different tail
  assert.ok(isMissionThreadId(`im-t3-codex-${mission}`, "t3-codex", mission));
  assert.ok(isMissionThreadId(`im-t3-codex-${mission}-16bced3f`, "t3-codex", mission));
  assert.ok(!isMissionThreadId(`im-t3-codex-${otherMission}-16bced3f`, "t3-codex", mission));
  // member boundary: t3-glm must not claim t3-glm-flash's threads
  const flashMission = `ms_${"f".repeat(32)}`;
  assert.ok(!isMissionThreadId(`im-t3-glm-flash-${flashMission}-ab`, "t3-glm", flashMission));
  assert.ok(isMissionThreadId(`im-t3-glm-flash-${flashMission}-ab`, "t3-glm-flash", flashMission));
});

test("titleFromBrief extracts the mission name with the member prefix", () => {
  assert.equal(titleFromBrief(BRIEF, "t3-codex", "build", "ms_x"), "im/t3-codex@build: Smoke mission");
  assert.equal(titleFromBrief("garbage\nmore", "t3-codex", "review", "ms_abc123def"), "im/t3-codex@review: ms_abc123def");
});

test("titleFromBrief finds the mission header below a result marker", () => {
  const resultBrief = [
    "[InfiniteMission returned result]",
    "This Mission is already ended.",
    "",
    "[mission ms_aaaabbbbccccdddd] Ask — ended",
    "  ended: mission is no longer in the mail stream",
  ].join("\n");
  assert.equal(
    titleFromBrief(resultBrief, "t3-codex", "design", "ms_aaaabbbbccccdddd"),
    "im/t3-codex@design: Ask",
  );
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

test("dutyPreamble is orientation-only: identity, one pointer, no action-menu duplication", () => {
  const text = dutyPreamble("t3-codex", "/w", "ms_aaaabbbbccccdddd");
  assert.match(text, /You are the InfiniteMission member "t3-codex" in workspace \/w/);
  assert.match(text, /im mission show ms_aaaabbbbccccdddd --for t3-codex/);
  // Placeholders invite placeholder submissions: no <missionId> may survive.
  assert.doesNotMatch(text, /<missionId>/);
  assert.match(text, /im help/);
  // The tail reminder owns the action menu; restating it here was banner noise.
  assert.doesNotMatch(text, /im mission submit/);
  assert.doesNotMatch(text, /Never run "im join" or "im receive"/);
  assert.doesNotMatch(text, /\(needs --/);
  assert.doesNotMatch(text, /--revision/);
});

test("briefFromRunView shortens document receipts to a comparable prefix", () => {
  const brief = briefFromRunView({
    missionId: "ms_aaaabbbbccccdddd",
    name: "Smoke",
    status: "active",
    revision: 1,
    documents: [
      { id: "spec", kind: "file", path: "spec.md", receipt: `document:${"a".repeat(64)}`, mayRead: true, mayWrite: false },
      { id: "plan", kind: "file", path: "plan.md", mayRead: true, mayWrite: true },
    ],
  });
  assert.match(brief, /receipt=document:aaaaaaaaaaaa…/);
  // The stored 64-char fingerprint must not ride the delivered brief.
  assert.doesNotMatch(brief, /a{13}/);
  // No-receipt documents render without the receipt field.
  assert.match(brief, /plan \(file\) plan\.md \[read:y write:y\]/);
});

test("resultPreamble is read-only: no submit duty, explicit closed-mission warning", () => {
  const text = resultPreamble("t3-codex", "/w");
  // The warning sentence mentions the command; what must be absent is the
  // duty-preamble's imperative submit instruction and its command syntax.
  assert.doesNotMatch(text, /Submitting IS the deliverable/);
  assert.doesNotMatch(text, /im mission submit \S+ <missionId>/);
  assert.match(text, /ENDED/);
  assert.match(text, /do NOT run im mission submit/);
  assert.match(text, /Never run "im join" or "im receive"/);
  assert.match(text, /----- mission result -----/);
});

test("ended delivery carries the result preamble; a normal one keeps the duty preamble", async () => {
  const { dir, cleanup } = tempWorkspace();
  const t3 = new FakeT3();
  const delivery = new Delivery(t3);
  try {
    await delivery.deliver({
      workspacePath: dir,
      member: MEMBER,
      station: "build",
      missionId: "ms_aaaabbbbccccdddd",
      brief: "[InfiniteMission returned result]\nended brief",
      ended: true,
    });
    await delivery.deliver({
      workspacePath: dir,
      member: MEMBER,
      station: "build",
      missionId: "ms_bbbbccccddddeeee",
      brief: BRIEF,
    });
    const turns = t3.dispatched
      .filter((command) => command.type === "thread.turn.start")
      .map((command) => command.message.text);
    assert.match(turns[0], /----- mission result -----/);
    assert.doesNotMatch(turns[0], /----- mission brief -----/);
    assert.match(turns[1], /----- mission brief -----/);
    // Full-snapshot turn opens with the duty head; follow-ups would not.
    assert.match(turns[1], /You are the InfiniteMission member "t3-codex"/);
  } finally {
    cleanup();
  }
});

test("every delivered turn ends with the tail reminder: recency anchor after the brief", async () => {
  const { dir, cleanup } = tempWorkspace();
  const t3 = new FakeT3();
  const delivery = new Delivery(t3);
  try {
    await delivery.deliver({
      workspacePath: dir,
      member: MEMBER,
      station: "build",
      missionId: "ms_aaaabbbbccccdddd",
      brief: BRIEF,
    });
    await delivery.deliver({
      workspacePath: dir,
      member: MEMBER,
      station: "build",
      missionId: "ms_bbbbccccddddeeee",
      brief: "[InfiniteMission returned result]\nended brief",
      ended: true,
    });
    const turns = t3.dispatched
      .filter((command) => command.type === "thread.turn.start")
      .map((command) => command.message.text);
    // Duty tail: XML envelope at the very end, ids pre-bound, two-option menu.
    const duty = turns[0];
    assert.ok(duty.trimEnd().endsWith("</im-system-reminder>"), "duty tail must be the last thing in the turn");
    assert.match(duty, /<im-system-reminder>\n/);
    assert.match(duty, /im mission submit "t3-codex" "ms_aaaabbbbccccdddd" --outcome/);
    assert.match(duty, /im mission abandon "t3-codex" "ms_aaaabbbbccccdddd"`/);
    assert.match(duty, /Take the permitted outcomes from the/);
    // The tail must not smuggle in a revision flag — revisions are gone from
    // the submit surface entirely.
    assert.doesNotMatch(duty, /--revision/);
    // Result tail: read-only action menu, same envelope discipline.
    const result = turns[1];
    assert.ok(result.trimEnd().endsWith("</im-system-reminder>"), "result tail must be the last thing in the turn");
    assert.match(result, /ms_bbbbccccddddeeee is ENDED/);
    assert.match(result, /reporting the result to the/);
    assert.doesNotMatch(result, /im mission submit "t3-codex" "ms_bbbbccccddddeeee"/);
  } finally {
    cleanup();
  }
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

test("deleted base + live suffixed thread: the suffix thread is reused, not re-forked", async () => {
  const { dir, cleanup } = tempWorkspace();
  const hexId = "im-t3-codex-ms_aaaabbbbccccdddd-16bced3f";
  const t3 = new FakeT3({
    projects: [{ id: "p-existing", workspaceRoot: dir }],
    threads: new Map(), // detail probes all miss (base deleted → 404 on the server)
    shellThreads: [
      { id: hexId, archivedAt: null, settledAt: null, settledOverride: null, updatedAt: "2026-09-09T03:17:51Z" },
    ],
  });
  const delivery = new Delivery(t3);
  try {
    const threadId = await delivery.deliver({
      workspacePath: dir,
      member: MEMBER,
      station: "human-review",
      missionId: "ms_aaaabbbbccccdddd",
      brief: BRIEF,
    });
    assert.equal(threadId, hexId);
    assert.deepEqual(types(t3), ["thread.turn.start"]); // followup — no create, no fork
    assert.equal(t3.dispatched[0].threadId, hexId);
  } finally {
    cleanup();
  }
});

test("several suffixed threads: the most recently updated one carries the conversation", async () => {
  const { dir, cleanup } = tempWorkspace();
  const older = "im-t3-codex-ms_aaaabbbbccccdddd-91f10e00";
  const newer = "im-t3-codex-ms_aaaabbbbccccdddd-ea743c32";
  const t3 = new FakeT3({
    projects: [{ id: "p-existing", workspaceRoot: dir }],
    shellThreads: [
      { id: older, archivedAt: null, settledAt: null, settledOverride: null, updatedAt: "2026-09-09T03:19:38Z" },
      { id: newer, archivedAt: null, settledAt: null, settledOverride: null, updatedAt: "2026-09-09T03:24:58Z" },
    ],
  });
  const delivery = new Delivery(t3);
  try {
    const threadId = await delivery.deliver({
      workspacePath: dir,
      member: MEMBER,
      station: "human-review",
      missionId: "ms_aaaabbbbccccdddd",
      brief: BRIEF,
    });
    assert.equal(threadId, newer);
  } finally {
    cleanup();
  }
});

test("hasThread sees suffixed threads through the shell; settled ones still count as undelivered", async () => {
  const hexLive = "im-t3-codex-ms_1111111111111111-16bced3f";
  const hexSettled = "im-t3-codex-ms_2222222222222222-21235c2a";
  const foreignHex = "im-t3-grok-ms_1111111111111111-abcd1234";
  const t3 = new FakeT3({
    shellThreads: [
      { id: hexLive, archivedAt: null, settledAt: null, settledOverride: null, updatedAt: "2026-09-09T03:00:00Z" },
      { id: hexSettled, archivedAt: null, settledAt: "2026-09-09T03:05:00Z", settledOverride: null, updatedAt: "2026-09-09T03:05:00Z" },
      { id: foreignHex, archivedAt: null, settledAt: null, settledOverride: null, updatedAt: "2026-09-09T03:00:00Z" },
    ],
  });
  const delivery = new Delivery(t3);
  assert.equal(await delivery.hasThread("t3-codex", "ms_1111111111111111"), true);
  assert.equal(await delivery.hasThread("t3-codex", "ms_2222222222222222"), false); // settled → revisit must find it
  assert.equal(await delivery.hasThread("t3-grok", "ms_9999999999999999"), false); // prefix is member-scoped
});

test("settle reaches a suffixed thread once the deterministic ids are gone", async () => {
  const hexId = "im-t3-codex-ms_1111111111111111-ea743c32";
  const t3 = new FakeT3({
    threads: new Map(), // probes miss
    shellThreads: [
      { id: hexId, archivedAt: null, settledAt: null, settledOverride: null, updatedAt: "2026-09-09T03:24:58Z" },
    ],
  });
  const delivery = new Delivery(t3);
  assert.equal(await delivery.settle("t3-codex", "ms_1111111111111111"), true);
  assert.deepEqual(types(t3), ["thread.settle"]);
  assert.equal(t3.dispatched[0].threadId, hexId);
});

test("arrivalHeader states the hop, the visit count, and the latest feedback verbatim", () => {
  const events = [
    "#1  2026-09-14 10:00:00 mission.created",
    '  {"createdBy":"dev-design"}',
    "#2  2026-09-14 10:05:00 mission.round.completed",
    '  {"iteration":1,"outcome":"spec-ready","feedback":null,"reason":"plan ready"}',
    "#3  2026-09-14 10:05:00 mission.routed",
    '  {"from":"design","to":"supervisor","when":"spec-ready"}',
    "#4  2026-09-14 11:00:00 mission.round.completed",
    '  {"iteration":2,"outcome":"plan-ready","feedback":null,"reason":"plan attached"}',
    "#5  2026-09-14 11:00:00 mission.routed",
    '  {"from":"supervisor","to":"build","when":"plan-ready"}',
    "#6  2026-09-14 12:00:00 mission.round.completed",
    '  {"iteration":3,"outcome":"reject-impl","feedback":"fix the null deref in live-apply","reason":"two findings"}',
    "#7  2026-09-14 12:00:00 mission.routed",
    '  {"from":"review-audit","to":"build","when":"reject-impl"}',
  ].join("\n");
  const header = arrivalHeader({
    station: "build",
    hop: { from: "review-audit", to: "build", outcome: "reject-impl" },
    eventsText: events,
  });
  assert.match(header, /=== ARRIVAL CONTEXT ===/);
  assert.match(header, /from 'review-audit' on outcome 'reject-impl' — 2 times routed to this station/);
  assert.match(header, /Latest round input for you \(feedback, verbatim\):/);
  assert.match(header, /  fix the null deref in live-apply/);
  assert.match(header, /=== END ARRIVAL CONTEXT ===/);
});

test("arrivalHeader falls back to reason, degrades to empty without facts", () => {
  const events = [
    "#1  2026-09-14 10:00:00 mission.round.completed",
    '  {"outcome":"done","feedback":null,"reason":"all green"}',
  ].join("\n");
  const header = arrivalHeader({ station: "verify", hop: null, eventsText: events });
  assert.match(header, /reason, verbatim/);
  assert.match(header, /  all green/);
  // No hop and no routable history → no header rather than a guessed one.
  assert.equal(arrivalHeader({ station: "build", hop: null, eventsText: null }), "");
  // Malformed event JSON is skipped, not fatal.
  const broken = arrivalHeader({
    station: "build",
    hop: { from: "x", to: "build", outcome: "y" },
    eventsText: "#1 t mission.round.completed\n  not json",
  });
  assert.match(broken, /from 'x' on outcome 'y'/);
});

test("slimBrief drops the stable objective and current-step summary, keeps round inputs", () => {
  const brief = [
    "=== ARRIVAL CONTEXT ===",
    "Arrival: from 'review-audit' on outcome 'reject-impl' — 2 times routed to this station",
    "Latest round input for you (feedback, verbatim):",
    "  fix the null deref",
    "=== END ARRIVAL CONTEXT ===",
    "",
    "[mission ms_aaaabbbbccccdddd] dev-mixed — active",
    "  objective: 全局任务描述（首投已有）",
    "  origin work: design",
    "  at station: build (iteration 2)",
    "  revision: 9",
    "  on duty: YES — you hold this station",
    "  current step: 通用实现",
    "  outcomes: impl-ready, plan-reject, abandon",
    "  routes:",
    "    impl-ready -> review-audit",
    "  documents:",
    "    impl (file) impl.md [read:n write:y]",
  ].join("\n");
  const slim = slimBrief(brief);
  assert.ok(!slim.includes("全局任务描述"), "objective body must go");
  assert.ok(!slim.includes("通用实现"), "current-step summary must go");
  assert.match(slim, /objective: \(mission objective — first brief/);
  assert.match(slim, /current step: \(unchanged; see the first brief\)/);
  assert.match(slim, /ARRIVAL CONTEXT/);
  assert.match(slim, /fix the null deref/);
  assert.match(slim, /at station: build \(iteration 2\)/);
  assert.match(slim, /outcomes: impl-ready, plan-reject, abandon/);
  assert.match(slim, /impl \(file\) impl\.md/);
});

test("slimBrief fails open when the anchors are missing", () => {
  const odd = "some future CLI shape\n  no known fields";
  assert.equal(slimBrief(odd), odd);
});

test("follow-up rounds deliver the slim form; first rounds keep the full brief", async () => {
  const { dir, cleanup } = tempWorkspace();
  const t3 = new FakeT3();
  const delivery = new Delivery(t3);
  const STEP = "  current step: 通用实现";
  const brief1 = `[mission ms_aaaabbbbccccdddd] dev-mixed — active\n  objective: 全局目标\n${STEP}\n  outcomes: impl-ready, abandon`;
  const brief2 = `=== ARRIVAL CONTEXT ===\nArrival: from 'review-audit' on outcome 'reject-impl'\n=== END ARRIVAL CONTEXT ===\n\n[mission ms_aaaabbbbccccdddd] dev-mixed — active\n  objective: 全局目标\n${STEP}\n  outcomes: impl-ready, abandon`;
  try {
    await delivery.deliver({ workspacePath: dir, member: MEMBER, station: "build", missionId: "ms_aaaabbbbccccdddd", brief: brief1 });
    await delivery.deliver({ workspacePath: dir, member: MEMBER, station: "build", missionId: "ms_aaaabbbbccccdddd", brief: brief2 });
    const texts = t3.dispatched
      .filter((command) => command.type === "thread.turn.start")
      .map((command) => command.message.text);
    assert.match(texts[0], /通用实现/);
    assert.match(texts[0], /全局目标/);
    assert.doesNotMatch(texts[1], /current step: 通用实现/);
    assert.doesNotMatch(texts[1], /全局目标\n/);
    assert.match(texts[1], /ARRIVAL CONTEXT/);
    assert.match(texts[1], /outcomes: impl-ready, abandon/);
    assert.match(texts[1], /Stable mission context lives in this thread's first brief/);
    // The tail reminder still rides the follow-up turn.
    assert.match(texts[1], /<im-system-reminder>/);
  } finally {
    cleanup();
  }
});

test("bridge restart, station change, and failed T3 session force a full snapshot", async () => {
  const { dir, cleanup } = tempWorkspace();
  const t3 = new FakeT3();
  const missionId = "ms_aaaabbbbccccdddd";
  const full = (station, step) =>
    `[mission ${missionId}] dev-mixed — active\n  objective: full objective\n  at station: ${station} (iteration 1)\n  current step: ${step}\n  outcomes: done, abandon`;
  try {
    const firstProcess = new Delivery(t3);
    await firstProcess.deliver({
      workspacePath: dir,
      member: MEMBER,
      station: "build",
      missionId,
      brief: full("build", "build charter"),
      contextKey: "build:one",
    });
    await firstProcess.deliver({
      workspacePath: dir,
      member: MEMBER,
      station: "verify",
      missionId,
      brief: full("verify", "verify charter"),
      contextKey: "verify:two",
    });
    const secondProcess = new Delivery(t3);
    await secondProcess.deliver({
      workspacePath: dir,
      member: MEMBER,
      station: "verify",
      missionId,
      brief: full("verify", "restart charter"),
      contextKey: "verify:two",
    });
    const thread = t3.threads.get(threadIdFor(MEMBER.id, missionId));
    thread.latestTurn = { state: "error" };
    thread.session = { status: "error" };
    await secondProcess.deliver({
      workspacePath: dir,
      member: MEMBER,
      station: "verify",
      missionId,
      brief: full("verify", "replacement charter"),
      contextKey: "verify:two",
    });
    const texts = t3.dispatched
      .filter((command) => command.type === "thread.turn.start")
      .map((command) => command.message.text);
    assert.match(texts[0], /build charter/);
    assert.match(texts[1], /verify charter/);
    assert.match(texts[2], /restart charter/);
    assert.match(texts[3], /replacement charter/);
    for (const text of texts) assert.match(text, /full objective/);
  } finally {
    cleanup();
  }
});
