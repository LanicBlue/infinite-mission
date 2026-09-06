import test from "node:test";
import assert from "node:assert/strict";
import { Bridge } from "../bridge.mjs";
import { validateConfig } from "../lib/config.mjs";

const WS = "/tmp/im-t3-bridge-test-ws";
const MEMBER = { id: "t3-codex", instance: "codex", model: "gpt-5.6-luna", options: undefined, runtimeMode: "full-access", enabled: true };

const ARRIVAL = (missionId, station = "build") =>
  [
    `[station ${station}] mission arrived`,
    `  → Run: im mission show ${missionId} --for t3-codex (then im missions t3-codex)`,
  ].join("\n");

const TIMEOUT = "No new messages (timed out after 600s).";

class ScriptedRunner {
  constructor({ roster = [], receiveScript = [], missionShow = () => ({ ok: true, text: "[mission ms_x] T — active" }), missions = [] } = {}) {
    this.rosterSource = roster;
    this.receiveScript = [...receiveScript];
    this.missionShowSource = missionShow;
    this.missionsSource = missions;
    this.calls = { roster: [], join: [], leave: [], receive: [], missionShow: [], missions: [], workspaces: 0 };
  }
  async roster(workspace) {
    this.calls.roster.push(workspace);
    return typeof this.rosterSource === "function" ? this.rosterSource() : this.rosterSource;
  }
  async join(workspace, memberId) {
    this.calls.join.push([workspace, memberId]);
    return { code: 0, stdout: `Joined as ${memberId}.` };
  }
  async leave(workspace, memberId) {
    this.calls.leave.push([workspace, memberId]);
    return `${memberId} archived.`;
  }
  async receive(workspace, memberId, timeoutSec, signal) {
    this.calls.receive.push([workspace, memberId, timeoutSec]);
    await new Promise((resolve) => {
      if (signal?.aborted) return resolve();
      const timer = setTimeout(resolve, 5);
      signal?.addEventListener("abort", () => { clearTimeout(timer); resolve(); }, { once: true });
    });
    const item = this.receiveScript.length ? this.receiveScript.shift() : { code: 0, stdout: TIMEOUT };
    if (item.throw) throw new Error(item.throw);
    return item;
  }
  async missionShow(workspace, missionId, memberId) {
    this.calls.missionShow.push([workspace, missionId, memberId]);
    return typeof this.missionShowSource === "function" ? this.missionShowSource(missionId) : this.missionShowSource;
  }
  async missions(workspace, memberId) {
    this.calls.missions.push([workspace, memberId]);
    return typeof this.missionsSource === "function" ? this.missionsSource(memberId) : this.missionsSource;
  }
  async workspaces() {
    this.calls.workspaces += 1;
    return [WS];
  }
}

class FakeDelivery {
  constructor() {
    this.deliverCalls = [];
    this.settleCalls = [];
    this.failDeliver = false;
  }
  async deliver(args) {
    if (this.failDeliver) throw new Error("t3 unreachable");
    this.deliverCalls.push(args);
    return `im-${args.missionId}`;
  }
  async settle(missionId) {
    this.settleCalls.push(missionId);
    return true;
  }
}

function makeConfig(overrides = {}) {
  const result = validateConfig({
    // Isolate from the real ~/.t3 (whose settings.json may carry imBridge
    // members that would override this test member list).
    t3: { home: "/nonexistent-t3-home-for-tests" },
    members: [{ id: "t3-codex", instance: "codex", model: "gpt-5.6-luna" }],
    workspaces: [WS],
    rescanSec: 1,
    receiveTimeoutSec: 1,
    ...overrides,
  });
  assert.equal(result.ok, true, JSON.stringify(result.errors));
  return result.config;
}

async function waitFor(predicate, timeoutMs = 2000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (predicate()) return true;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  return predicate();
}

const quiet = () => {};

test("guard-join: active member with the exact id never triggers join", async () => {
  const runner = new ScriptedRunner({ roster: [{ id: "t3-codex", status: "active (1s ago)" }] });
  const bridge = new Bridge({ runner, delivery: new FakeDelivery(), config: makeConfig(), logger: quiet });
  await bridge.reconcile();
  assert.equal(runner.calls.join.length, 0);
  assert.ok(await waitFor(() => runner.calls.receive.length >= 1));
  assert.deepEqual(runner.calls.receive[0].slice(0, 2), [WS, "t3-codex"]);
  await bridge.stop();
});

test("guard-join: absent member joins once and verifies the exact id landed", async () => {
  let rosterCall = 0;
  const runner = new ScriptedRunner({
    roster: () => {
      rosterCall += 1;
      return rosterCall <= 1 ? [] : [{ id: "t3-codex", status: "active (1s ago)" }];
    },
  });
  const bridge = new Bridge({ runner, delivery: new FakeDelivery(), config: makeConfig(), logger: quiet });
  await bridge.reconcile();
  assert.equal(runner.calls.join.length, 1);
  assert.ok(await waitFor(() => runner.calls.receive.length >= 1));
  await bridge.stop();
});

test("guard-join: suffixed landing warns and does not open a receive loop", async () => {
  let rosterCall = 0;
  const runner = new ScriptedRunner({
    roster: () => {
      rosterCall += 1;
      return rosterCall <= 1 ? [] : [{ id: "t3-codex-2", status: "active (1s ago)" }];
    },
  });
  const bridge = new Bridge({ runner, delivery: new FakeDelivery(), config: makeConfig(), logger: quiet });
  await bridge.reconcile();
  assert.equal(runner.calls.join.length, 1);
  await new Promise((resolve) => setTimeout(resolve, 150));
  assert.equal(runner.calls.receive.length, 0);
  await bridge.stop();
});

test("archived member is reactivated (disable leaves, re-enable rejoins)", async () => {
  const runner = new ScriptedRunner({ roster: [{ id: "t3-codex", status: "archived" }] });
  const bridge = new Bridge({ runner, delivery: new FakeDelivery(), config: makeConfig(), logger: quiet });
  await bridge.reconcile();
  assert.equal(runner.calls.join.length, 1); // reactivated with the same id
  assert.ok(await waitFor(() => runner.calls.receive.length >= 1));
  await bridge.stop();
});

test("disabling a member leaves im; removing from config does too", async (t) => {
  const runner = new ScriptedRunner({
    roster: () => [
      { id: "t3-codex", status: runner.calls.leave.length === 0 ? "active (1s ago)" : "archived" },
    ],
  });
  const bridge = new Bridge({ runner, delivery: new FakeDelivery(), config: makeConfig(), logger: quiet });
  t.after(() => bridge.stop());
  await bridge.reconcile();
  assert.ok(await waitFor(() => runner.calls.receive.length >= 1));
  // disable → loop stops AND im leave fires
  bridge.config = makeConfig({ members: [{ id: "t3-codex", instance: "codex", model: "m", enabled: false }] });
  await bridge.reconcile();
  assert.equal(bridge.loops.size, 0);
  assert.deepEqual(runner.calls.leave, [[WS, "t3-codex"]]);
  // removal from the list entirely: ownedMembers keeps the leave candidate
  bridge.config = makeConfig({ members: [] });
  await bridge.reconcile();
  assert.equal(runner.calls.leave.length, 1); // already archived → no second leave
});

test("arrival: mission show re-verified, then delivered with the brief", async () => {
  const runner = new ScriptedRunner({
    roster: [{ id: "t3-codex", status: "active (1s ago)" }],
    receiveScript: [{ code: 0, stdout: ARRIVAL("ms_aaaabbbbccccdddd") }],
    missionShow: () => ({ ok: true, text: "[mission ms_aaaabbbbccccdddd] Smoke — active\n  revision: 1" }),
  });
  const delivery = new FakeDelivery();
  const bridge = new Bridge({ runner, delivery, config: makeConfig(), logger: quiet });
  await bridge.reconcile();
  assert.ok(await waitFor(() => delivery.deliverCalls.length === 1));
  const call = delivery.deliverCalls[0];
  assert.equal(call.workspacePath, WS);
  assert.equal(call.member.id, "t3-codex");
  assert.equal(call.station, "build");
  assert.equal(call.missionId, "ms_aaaabbbbccccdddd");
  assert.match(call.brief, /revision: 1/);
  await bridge.stop();
});

test("stale arrival (mission show fails) is dropped, never delivered", async () => {
  const runner = new ScriptedRunner({
    roster: [{ id: "t3-codex", status: "active (1s ago)" }],
    receiveScript: [{ code: 0, stdout: ARRIVAL("ms_gone") }],
    missionShow: () => ({ ok: false, text: "error: no such mission" }),
  });
  const delivery = new FakeDelivery();
  const bridge = new Bridge({ runner, delivery, config: makeConfig(), logger: quiet });
  await bridge.reconcile();
  await new Promise((resolve) => setTimeout(resolve, 150));
  assert.equal(delivery.deliverCalls.length, 0);
  assert.ok(await waitFor(() => runner.calls.receive.length >= 2)); // loop survived
  await bridge.stop();
});

test("membership-end stops the loop and reconcile does not resurrect it", async () => {
  const runner = new ScriptedRunner({
    roster: [{ id: "t3-codex", status: "active (1s ago)" }],
    receiveScript: [
      { code: 0, stdout: "[membership] you are no longer an active member (removed or archived) — stopping the listener.\n" },
    ],
  });
  const bridge = new Bridge({ runner, delivery: new FakeDelivery(), config: makeConfig(), logger: quiet });
  await bridge.reconcile();
  assert.ok(await waitFor(() => runner.calls.receive.length === 1));
  await new Promise((resolve) => setTimeout(resolve, 150));
  assert.equal(runner.calls.receive.length, 1); // stopped, filler never consumed
  await bridge.reconcile();
  await new Promise((resolve) => setTimeout(resolve, 150));
  assert.equal(runner.calls.receive.length, 1); // muted — no resurrection
  await bridge.stop();
});

test("mission ended → settle called; failed delivery is retried on the next reconcile", async () => {
  const runner = new ScriptedRunner({
    roster: [{ id: "t3-codex", status: "active (1s ago)" }],
    receiveScript: [
      {
        code: 0,
        stdout: [
          "[from workspace] [ms_1111111111111111] mission ended: deleted (outcome)",
          "  → History: im mission events ms_1111111111111111",
          ARRIVAL("ms_aaaabbbbccccdddd"),
        ].join("\n"),
      },
    ],
    missionShow: (missionId) =>
      missionId === "ms_aaaabbbbccccdddd"
        ? { ok: true, text: "[mission ms_aaaabbbbccccdddd] Smoke — active" }
        : { ok: false, text: "" },
  });
  const delivery = new FakeDelivery();
  delivery.failDeliver = true;
  const bridge = new Bridge({ runner, delivery, config: makeConfig(), logger: quiet });
  await bridge.reconcile();
  assert.ok(await waitFor(() => delivery.settleCalls.length === 1));
  assert.equal(delivery.settleCalls[0], "ms_1111111111111111");
  assert.ok(await waitFor(() => runner.calls.missionShow.filter(([_, ms]) => ms === "ms_aaaabbbbccccdddd").length === 1));

  // First delivery failed → queued. Heal on the next reconcile.
  delivery.failDeliver = false;
  await bridge.reconcile();
  assert.ok(await waitFor(() => delivery.deliverCalls.length === 1));
  assert.equal(delivery.deliverCalls[0].missionId, "ms_aaaabbbbccccdddd");
  await bridge.stop();
});

test("disabled member never gets a loop; removing from config aborts a running loop", async () => {
  const runner = new ScriptedRunner({ roster: [{ id: "t3-codex", status: "active (1s ago)" }] });
  const bridge = new Bridge({ runner, delivery: new FakeDelivery(), config: makeConfig(), logger: quiet });
  await bridge.reconcile();
  assert.ok(await waitFor(() => runner.calls.receive.length >= 1));
  bridge.config = makeConfig({ members: [] });
  await bridge.reconcile();
  assert.equal(bridge.loops.size, 0);
  const before = runner.calls.receive.length;
  await new Promise((resolve) => setTimeout(resolve, 100));
  assert.equal(runner.calls.receive.length, before); // aborted, no more receive
  await bridge.stop();
});

test("receive failure does not kill the loop (backs off, re-hangs)", async (t) => {
  const runner = new ScriptedRunner({
    roster: [{ id: "t3-codex", status: "active (1s ago)" }],
    receiveScript: [{ code: 1, stdout: "", stderr: "boom" }, { throw: "spawn failed" }],
  });
  const bridge = new Bridge({ runner, delivery: new FakeDelivery(), config: makeConfig({ rescanSec: 1 }), logger: quiet });
  t.after(() => bridge.stop());
  await bridge.reconcile();
  // two scripted failures each back off 1s, then the filler timeout keeps it alive
  assert.ok(await waitFor(() => runner.calls.receive.length >= 3, 5000));
});

test("watcher settles the thread when the turn ends and the mission left the member's list", async (t) => {
  const runner = new ScriptedRunner({
    roster: [{ id: "t3-codex", status: "active (1s ago)" }],
    receiveScript: [{ code: 0, stdout: ARRIVAL("ms_aaaabbbbccccdddd") }],
    missionShow: () => ({ ok: true, text: "[mission ms_aaaabbbbccccdddd] Smoke — active" }),
    missions: [],
  });
  const delivery = new FakeDelivery();
  const fakeT3 = {
    async threadDetail() {
      return { snapshotSequence: 1, thread: { id: "im-ms_aaaabbbbccccdddd", latestTurn: { state: "completed" }, settledAt: null, settledOverride: null } };
    },
  };
  const bridge = new Bridge({ runner, delivery, t3: fakeT3, config: makeConfig(), logger: quiet });
  t.after(() => bridge.stop());
  await bridge.reconcile();
  assert.ok(await waitFor(() => delivery.deliverCalls.length === 1));
  assert.equal(bridge.watching.length, 1);
  await bridge.reconcile(); // poll watchers
  assert.deepEqual(delivery.settleCalls, ["ms_aaaabbbbccccdddd"]);
  assert.equal(bridge.watching.length, 0);
});

test("watcher leaves the thread active when the mission is still open (duty skipped)", async (t) => {
  const runner = new ScriptedRunner({
    roster: [{ id: "t3-codex", status: "active (1s ago)" }],
    receiveScript: [{ code: 0, stdout: ARRIVAL("ms_aaaabbbbccccdddd") }],
    missionShow: () => ({ ok: true, text: "[mission ms_aaaabbbbccccdddd] Smoke — active" }),
    missions: ["ms_aaaabbbbccccdddd"],
  });
  const delivery = new FakeDelivery();
  const fakeT3 = {
    async threadDetail() {
      return { snapshotSequence: 1, thread: { id: "im-ms_aaaabbbbccccdddd", latestTurn: { state: "completed" }, settledAt: null, settledOverride: null } };
    },
  };
  const bridge = new Bridge({ runner, delivery, t3: fakeT3, config: makeConfig(), logger: quiet });
  t.after(() => bridge.stop());
  await bridge.reconcile();
  assert.ok(await waitFor(() => delivery.deliverCalls.length === 1));
  await bridge.reconcile();
  assert.equal(delivery.settleCalls.length, 0); // no settle — mission still open
  assert.equal(bridge.watching.length, 0); // watch consumed either way
});

test("watcher keeps watching while the turn is still running", async (t) => {
  const runner = new ScriptedRunner({
    roster: [{ id: "t3-codex", status: "active (1s ago)" }],
    receiveScript: [{ code: 0, stdout: ARRIVAL("ms_aaaabbbbccccdddd") }],
    missionShow: () => ({ ok: true, text: "[mission ms_aaaabbbbccccdddd] Smoke — active" }),
    missions: [],
  });
  const delivery = new FakeDelivery();
  const fakeT3 = {
    async threadDetail() {
      return { snapshotSequence: 1, thread: { id: "im-ms_aaaabbbbccccdddd", latestTurn: { state: "running" }, settledAt: null, settledOverride: null } };
    },
  };
  const bridge = new Bridge({ runner, delivery, t3: fakeT3, config: makeConfig(), logger: quiet });
  t.after(() => bridge.stop());
  await bridge.reconcile();
  assert.ok(await waitFor(() => delivery.deliverCalls.length === 1));
  await bridge.reconcile();
  assert.equal(delivery.settleCalls.length, 0);
  assert.equal(bridge.watching.length, 1); // still watching
});
