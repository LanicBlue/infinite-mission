import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Bridge } from "../bridge.mjs";
import { readConfigFile, validateConfig } from "../lib/config.mjs";

const WS = "/tmp/im-t3-bridge-test-ws";
const MEMBER = { id: "t3-codex", instance: "codex", model: "gpt-5.6-luna", options: undefined, runtimeMode: "full-access", enabled: true };

const ARRIVAL = (missionId, station = "build") =>
  [
    `[station ${station}] mission arrived`,
    `  → Run: im mission show ${missionId} --for t3-codex (then im missions t3-codex)`,
  ].join("\n");

const TIMEOUT = "No new messages (timed out after 600s).";

class ScriptedRunner {
  constructor({
    roster = [],
    receiveScript = [],
    missionShow = () => ({ ok: true, text: "[mission ms_x] T — active" }),
    missionResult = () => ({ ok: false, text: "" }),
    missionEvents = () => ({ ok: true, text: "" }),
    missions = [],
    results = [],
  } = {}) {
    this.rosterSource = roster;
    this.receiveScript = [...receiveScript];
    this.missionShowSource = missionShow;
    this.missionResultSource = missionResult;
    this.missionEventsSource = missionEvents;
    this.missionsSource = missions;
    this.resultsSource = results;
    this.calls = {
      roster: [], join: [], leave: [], receive: [], missionShow: [], missionResult: [], missionEvents: [], missions: [], results: [], workspaces: 0,
    };
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
  async missionResult(workspace, missionId) {
    this.calls.missionResult.push([workspace, missionId]);
    return typeof this.missionResultSource === "function" ? this.missionResultSource(missionId) : this.missionResultSource;
  }
  async missionEvents(workspace, missionId) {
    this.calls.missionEvents.push([workspace, missionId]);
    return typeof this.missionEventsSource === "function" ? this.missionEventsSource(missionId) : this.missionEventsSource;
  }
  async missions(workspace, memberId) {
    this.calls.missions.push([workspace, memberId]);
    return typeof this.missionsSource === "function" ? this.missionsSource(memberId) : this.missionsSource;
  }
  async missionsAt(workspace, memberId) {
    this.calls.missionsAt = this.calls.missionsAt || [];
    this.calls.missionsAt.push([workspace, memberId]);
    const source = typeof this.missionsSource === "function" ? this.missionsSource(memberId) : this.missionsSource;
    return source.map((entry) => (typeof entry === "string" ? { id: entry, station: "build" } : entry));
  }
  async results(workspace, memberId) {
    this.calls.results.push([workspace, memberId]);
    return typeof this.resultsSource === "function" ? this.resultsSource(memberId) : this.resultsSource;
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
    this.hasThreadCalls = [];
    this.failDeliver = false;
    this.threadExists = async () => true;
  }
  async deliver(args) {
    if (this.failDeliver) throw new Error("t3 unreachable");
    this.deliverCalls.push(args);
    return `im-${args.missionId}`;
  }
  async settle(memberId, missionId) {
    this.settleCalls.push([memberId, missionId]);
    return true;
  }
  async hasThread(memberId, missionId) {
    this.hasThreadCalls.push([memberId, missionId]);
    return this.threadExists(memberId, missionId);
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

/** Write a real bridge config file so `configPath` + state derivation work. */
function writeBridgeConfig(dir, members) {
  const configPath = path.join(dir, "t3-bridge.json");
  fs.writeFileSync(
    configPath,
    JSON.stringify({
      t3: { home: "/nonexistent-t3-home-for-tests" },
      members,
      workspaces: [WS],
      rescanSec: 1,
      receiveTimeoutSec: 1,
    }),
  );
  const loaded = readConfigFile(configPath);
  assert.equal(loaded.ok, true, JSON.stringify(loaded.errors));
  return { configPath, config: loaded.config };
}

const ownedInState = (configPath) => {
  const statePath = `${configPath.replace(/\.json$/, "")}.state.json`;
  return JSON.parse(fs.readFileSync(statePath, "utf8")).ownedMembers;
};

const ackedInState = (configPath) => {
  const statePath = `${configPath.replace(/\.json$/, "")}.state.json`;
  return JSON.parse(fs.readFileSync(statePath, "utf8")).ackedResults ?? [];
};

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

test("a rename is cleaned up across a bridge restart (persisted owned members)", async (t) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "im-t3-bridge-state-"));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));

  // Generation 1: the bridge joins t3-old and records it in the state file.
  const first = writeBridgeConfig(dir, [{ id: "t3-old", instance: "codex", model: "gpt-5.6-luna" }]);
  const runnerA = new ScriptedRunner({ roster: [] });
  runnerA.rosterSource = () =>
    runnerA.calls.join.length > 0 ? [{ id: "t3-old", status: "active (1s ago)" }] : [];
  const bridgeA = new Bridge({ runner: runnerA, delivery: new FakeDelivery(), ...first, logger: quiet });
  t.after(() => bridgeA.stop());
  await bridgeA.reconcile();
  assert.deepEqual(runnerA.calls.join, [[WS, "t3-old"]]);
  assert.deepEqual(ownedInState(first.configPath), ["t3-old"]);

  // The rename: the table now says t3-new. A restarted bridge (empty memory)
  // must still archive t3-old via the persisted owned set.
  const second = writeBridgeConfig(dir, [{ id: "t3-new", instance: "codex", model: "gpt-5.6-luna" }]);
  const runnerB = new ScriptedRunner({ roster: [] });
  runnerB.rosterSource = () => [
    { id: "t3-old", status: "active (1s ago)" },
    ...(runnerB.calls.join.length > 0 ? [{ id: "t3-new", status: "active (1s ago)" }] : []),
  ];
  const bridgeB = new Bridge({ runner: runnerB, delivery: new FakeDelivery(), ...second, logger: quiet });
  t.after(() => bridgeB.stop());
  await bridgeB.reconcile();
  assert.deepEqual(runnerB.calls.join, [[WS, "t3-new"]]);
  assert.deepEqual(runnerB.calls.leave, [[WS, "t3-old"]]);
  assert.deepEqual(ownedInState(second.configPath), ["t3-new"]); // responsibility ended

  // Generation 3: the left id must not be re-left nor clobber a manual reuse.
  runnerB.rosterSource = () => [
    { id: "t3-old", status: "active (1s ago)" }, // a human re-joined the freed id
    { id: "t3-new", status: "active (1s ago)" },
  ];
  await bridgeB.reconcile();
  assert.equal(runnerB.calls.leave.length, 1); // t3-old is no longer owned
  assert.deepEqual(ownedInState(second.configPath), ["t3-new"]);
});

test("a corrupt state file is tolerated — the bridge starts with no owned members", async (t) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "im-t3-bridge-state-"));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const { configPath, config } = writeBridgeConfig(dir, [
    { id: "t3-codex", instance: "codex", model: "gpt-5.6-luna" },
  ]);
  fs.writeFileSync(`${configPath.replace(/\.json$/, "")}.state.json`, "{ not json");
  const runner = new ScriptedRunner({ roster: [{ id: "t3-codex", status: "active (1s ago)" }] });
  const bridge = new Bridge({ runner, delivery: new FakeDelivery(), config, configPath, logger: quiet });
  t.after(() => bridge.stop());
  await bridge.reconcile(); // must not throw
  assert.deepEqual(ownedInState(configPath), ["t3-codex"]); // rewritten clean
  assert.equal(runner.calls.leave.length, 0);
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

test("returned result: ended mission is delivered with durable result and events, never submit duty", async () => {
  const missionId = "ms_aaaabbbbccccdddd";
  const runner = new ScriptedRunner({
    roster: [{ id: "t3-codex", status: "active (1s ago)" }],
    receiveScript: [{ code: 0, stdout: ARRIVAL(missionId, "design") }],
    missionShow: () => ({
      ok: true,
      text: `[mission ${missionId}] Ask — ended\n  ended: mission is no longer in the mail stream`,
    }),
    missionResult: () => ({ ok: true, text: '{"disposition":"completed","result":"forty two"}\n' }),
    missionEvents: () => ({ ok: true, text: '#3 mission.ended {"disposition":"completed"}\n' }),
  });
  const delivery = new FakeDelivery();
  const bridge = new Bridge({ runner, delivery, config: makeConfig(), logger: quiet });
  await bridge.reconcile();
  assert.ok(await waitFor(() => delivery.deliverCalls.length === 1));
  const call = delivery.deliverCalls[0];
  assert.equal(call.station, "design");
  assert.match(call.brief, /\[InfiniteMission returned result\]/);
  assert.match(call.brief, /do NOT submit or abandon/i);
  assert.match(call.brief, /\[Durable result\][\s\S]*forty two/);
  assert.match(call.brief, /\[Mission events\][\s\S]*mission\.ended/);
  assert.equal(call.ended, true);
  assert.deepEqual(runner.calls.missionResult, [[WS, missionId]]);
  assert.deepEqual(runner.calls.missionEvents, [[WS, missionId]]);
  assert.equal(bridge.watching.length, 0);
  await bridge.stop();
});

test("an active mission whose prompt mentions an ended line is never mistaken for a result", async () => {
  const missionId = "ms_ccccddddeeeeffff";
  const runner = new ScriptedRunner({
    roster: [{ id: "t3-codex", status: "active (1s ago)" }],
    receiveScript: [{ code: 0, stdout: ARRIVAL(missionId, "build") }],
    missionShow: () => ({
      ok: true,
      text: [
        `[mission ${missionId}] Tricky — active`,
        "  prompt: quote this verbatim:",
        "  ended: (a line a station prompt could contain)",
        "  revision: 1",
      ].join("\n"),
    }),
    missionResult: () => ({ ok: true, text: "{}\n" }),
    missionEvents: () => ({ ok: true, text: "" }),
  });
  const delivery = new FakeDelivery();
  const bridge = new Bridge({ runner, delivery, config: makeConfig(), logger: quiet });
  await bridge.reconcile();
  assert.ok(await waitFor(() => delivery.deliverCalls.length === 1));
  const call = delivery.deliverCalls[0];
  assert.equal(call.ended, false);
  assert.doesNotMatch(call.brief, /InfiniteMission returned result/);
  assert.equal(bridge.watching.length, 1);
  await bridge.stop();
});

test("returned result remains deliverable when an older core has no mission result command", async () => {
  const missionId = "ms_bbbbccccddddeeee";
  const runner = new ScriptedRunner({
    roster: [{ id: "t3-codex", status: "active (1s ago)" }],
    receiveScript: [{ code: 0, stdout: ARRIVAL(missionId, "design") }],
    missionShow: () => ({ ok: true, text: `[mission ${missionId}] Ask — ended\n  ended: closed` }),
    missionResult: () => ({ ok: false, text: "unknown command: result" }),
    missionEvents: () => ({ ok: true, text: '#2 round.completed {"result":"legacy answer"}\n' }),
  });
  const delivery = new FakeDelivery();
  const bridge = new Bridge({ runner, delivery, config: makeConfig(), logger: quiet });
  await bridge.reconcile();
  assert.ok(await waitFor(() => delivery.deliverCalls.length === 1));
  assert.doesNotMatch(delivery.deliverCalls[0].brief, /unknown command/);
  assert.match(delivery.deliverCalls[0].brief, /legacy answer/);
  assert.equal(delivery.deliverCalls[0].ended, true);
  assert.equal(bridge.watching.length, 0);
  await bridge.stop();
});

test("result sweep redelivers a consumed-but-undelivered result and acks it exactly once", async (t) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "im-t3-bridge-state-"));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const { configPath, config } = writeBridgeConfig(dir, [
    { id: "t3-codex", instance: "codex", model: "gpt-5.6-luna" },
  ]);
  const missionId = "ms_aaaabbbbccccdddd";
  // The result note was consumed long ago (receive only ever times out) and
  // no thread exists: the note-less restart-loss case the sweep must heal.
  const runner = new ScriptedRunner({
    roster: [{ id: "t3-codex", status: "active (1s ago)" }],
    receiveScript: [{ code: 0, stdout: TIMEOUT }],
    results: () => [{ id: missionId, station: "design", objective: "what is the answer?" }],
    missionShow: () => ({
      ok: true,
      text: `[mission ${missionId}] Ask — ended\n  ended: mission is no longer in the mail stream`,
    }),
    missionResult: () => ({ ok: true, text: '{"disposition":"completed","result":"forty two"}' }),
    missions: [],
  });
  const delivery = new FakeDelivery();
  delivery.threadExists = async () => false;
  const bridge = new Bridge({ runner, delivery, t3: {}, config, configPath, logger: quiet });
  t.after(() => bridge.stop());
  await bridge.reconcile();
  assert.ok(await waitFor(() => delivery.deliverCalls.length === 1));
  assert.equal(delivery.deliverCalls[0].ended, true);
  assert.match(delivery.deliverCalls[0].brief, /forty two/);
  const ackKey = `${WS}\0t3-codex\0${missionId}`;
  assert.deepEqual(ackedInState(configPath), [ackKey]);

  // Idempotent: the next reconcile re-lists the same ended mission but the
  // persisted ack skips it — no second delivery, no second probe.
  await bridge.reconcile();
  await new Promise((resolve) => setTimeout(resolve, 30));
  assert.equal(delivery.deliverCalls.length, 1);
  assert.equal(delivery.hasThreadCalls.length, 1);
  assert.deepEqual(ackedInState(configPath), [ackKey]);
});

test("result sweep acks a live result thread from a previous life without redelivering", async (t) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "im-t3-bridge-state-"));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const { configPath, config } = writeBridgeConfig(dir, [
    { id: "t3-codex", instance: "codex", model: "gpt-5.6-luna" },
  ]);
  const missionId = "ms_bbbbccccddddeeee";
  const runner = new ScriptedRunner({
    roster: [{ id: "t3-codex", status: "active (1s ago)" }],
    results: () => [{ id: missionId, station: "design", objective: "q" }],
    missions: [],
  });
  const delivery = new FakeDelivery(); // threadExists → true: delivered before the restart
  const bridge = new Bridge({ runner, delivery, t3: {}, config, configPath, logger: quiet });
  t.after(() => bridge.stop());
  await bridge.reconcile();
  assert.equal(delivery.deliverCalls.length, 0);
  assert.equal(delivery.hasThreadCalls.length, 1);
  assert.deepEqual(ackedInState(configPath), [`${WS}\0t3-codex\0${missionId}`]);

  // The ack short-circuits: later reconciles stop probing T3 for it.
  await bridge.reconcile();
  assert.equal(delivery.hasThreadCalls.length, 1);
});

test("result sweep on an old core (im results fails) is inert; arrivals still sweep", async (t) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "im-t3-bridge-state-"));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const { configPath, config } = writeBridgeConfig(dir, [
    { id: "t3-codex", instance: "codex", model: "gpt-5.6-luna" },
  ]);
  const missionId = "ms_ccccddddeeeeffff";
  const runner = new ScriptedRunner({
    roster: [{ id: "t3-codex", status: "active (1s ago)" }],
    receiveScript: [{ code: 0, stdout: ARRIVAL(missionId, "build") }],
    // Old core: the command does not exist → non-zero exit.
    results: () => {
      throw new Error("im results t3-codex failed (exit 1): unknown command: results");
    },
    missionShow: () => ({ ok: true, text: "[mission ms_x] T — active\n  revision: 1" }),
    missions: [],
  });
  const delivery = new FakeDelivery();
  const bridge = new Bridge({ runner, delivery, config, configPath, logger: quiet });
  t.after(() => bridge.stop());
  await bridge.reconcile();
  assert.ok(await waitFor(() => delivery.deliverCalls.length === 1));
  assert.equal(delivery.deliverCalls[0].ended, false); // the arrival, not a result
  assert.deepEqual(ackedInState(configPath), []);
  await bridge.stop();
});

test("a failed result delivery retries and the retry success acks and persists", async (t) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "im-t3-bridge-state-"));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const { configPath, config } = writeBridgeConfig(dir, [
    { id: "t3-codex", instance: "codex", model: "gpt-5.6-luna" },
  ]);
  const missionId = "ms_ddddceeeffff0001";
  const runner = new ScriptedRunner({
    roster: [{ id: "t3-codex", status: "active (1s ago)" }],
    receiveScript: [{ code: 0, stdout: ARRIVAL(missionId, "design") }],
    results: () => [],
    missionShow: () => ({
      ok: true,
      text: `[mission ${missionId}] Ask — ended\n  ended: mission is no longer in the mail stream`,
    }),
    missionResult: () => ({ ok: true, text: '{"disposition":"completed","result":"late answer"}' }),
    missions: [],
  });
  const delivery = new FakeDelivery();
  delivery.failDeliver = true; // first delivery attempt fails → retry queue
  const bridge = new Bridge({ runner, delivery, config, configPath, logger: quiet });
  t.after(() => bridge.stop());
  await bridge.reconcile();
  await new Promise((resolve) => setTimeout(resolve, 30));
  assert.equal(delivery.deliverCalls.length, 0);
  assert.deepEqual(ackedInState(configPath), []); // failure must not ack

  delivery.failDeliver = false;
  await bridge.reconcile();
  assert.ok(await waitFor(() => delivery.deliverCalls.length === 1));
  assert.equal(delivery.deliverCalls[0].ended, true);
  assert.match(delivery.deliverCalls[0].brief, /late answer/);
  assert.deepEqual(ackedInState(configPath), [`${WS}\0t3-codex\0${missionId}`]);
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
  assert.deepEqual(delivery.settleCalls[0], ["t3-codex", "ms_1111111111111111"]);
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
  assert.deepEqual(delivery.settleCalls, [["t3-codex", "ms_aaaabbbbccccdddd"]]);
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

test("sweep delivers a station mission that has no T3 thread, once", async (t) => {
  const runner = new ScriptedRunner({
    roster: [{ id: "t3-codex", status: "active (1s ago)" }],
    missionShow: () => ({ ok: true, text: "[mission ms_dddddddddddddddd] Swept — active\n  revision: 3" }),
    missions: [{ id: "ms_dddddddddddddddd", station: "audit" }],
  });
  const delivery = new FakeDelivery();
  delivery.threadExists = async () => false;
  const bridge = new Bridge({ runner, delivery, t3: {}, config: makeConfig(), logger: quiet });
  t.after(() => bridge.stop());
  await bridge.reconcile();
  assert.equal(delivery.deliverCalls.length, 1);
  assert.equal(delivery.deliverCalls[0].missionId, "ms_dddddddddddddddd");
  assert.equal(delivery.deliverCalls[0].station, "audit");
  assert.equal(bridge.watching.length, 1);
  await bridge.reconcile(); // already watching → no duplicate
  assert.equal(delivery.deliverCalls.length, 1);
});

test("sweep skips missions whose thread already exists (follow-up rounds included)", async (t) => {
  const runner = new ScriptedRunner({
    roster: [{ id: "t3-codex", status: "active (1s ago)" }],
    missions: [{ id: "ms_aaaabbbbccccdddd", station: "build" }],
  });
  const delivery = new FakeDelivery(); // threadExists → true by default
  const bridge = new Bridge({ runner, delivery, t3: {}, config: makeConfig(), logger: quiet });
  t.after(() => bridge.stop());
  await bridge.reconcile();
  assert.equal(delivery.hasThreadCalls.length, 1);
  assert.equal(delivery.deliverCalls.length, 0);
});

test("sweep aborts cleanly when the T3 probe fails", async (t) => {
  const runner = new ScriptedRunner({
    roster: [{ id: "t3-codex", status: "active (1s ago)" }],
    missions: [{ id: "ms_aaaabbbbccccdddd", station: "build" }],
  });
  const delivery = new FakeDelivery();
  delivery.threadExists = async () => {
    throw new Error("connection refused");
  };
  const bridge = new Bridge({ runner, delivery, t3: {}, config: makeConfig(), logger: quiet });
  t.after(() => bridge.stop());
  await bridge.reconcile(); // must not throw
  assert.equal(delivery.deliverCalls.length, 0);
});

test("exhausted retries park in the queue instead of dropping; healing delivers and watches", async (t) => {
  const runner = new ScriptedRunner({
    roster: [{ id: "t3-codex", status: "active (1s ago)" }],
    receiveScript: [{ code: 0, stdout: ARRIVAL("ms_cccccccccccccccc") }],
    missionShow: () => ({ ok: true, text: "[mission ms_cccccccccccccccc] Parked — active" }),
    missions: [],
  });
  const delivery = new FakeDelivery();
  delivery.failDeliver = true;
  const bridge = new Bridge({ runner, delivery, t3: {}, config: makeConfig({ maxDeliverAttempts: 1 }), logger: quiet });
  t.after(() => bridge.stop());
  await bridge.reconcile();
  assert.ok(await waitFor(() => runner.calls.missionShow.length === 1)); // arrival consumed, delivery failed
  await bridge.reconcile(); // attempt 1 of 1 → fails again, stays queued
  assert.equal(bridge.retryQueue.length, 1);
  await bridge.reconcile(); // over budget → parked, still queued
  assert.equal(bridge.retryQueue.length, 1);
  assert.equal(bridge.retryQueue[0].attempts, 0);
  delivery.failDeliver = false;
  await bridge.reconcile(); // parked item retries → delivers
  assert.equal(bridge.retryQueue.length, 0);
  assert.equal(delivery.deliverCalls.length, 1);
  assert.equal(bridge.watching.length, 1); // retry delivery gets a turn watcher too
});

test("loop deliveries resolve the member at delivery time — model edits apply without a loop restart", async (t) => {
  const runner = new ScriptedRunner({
    roster: [{ id: "t3-codex", status: "active (1s ago)" }],
    receiveScript: [{ code: 0, stdout: ARRIVAL("ms_1111111111111111") }, { code: 0, stdout: ARRIVAL("ms_2222222222222222") }],
    missionShow: (missionId) => ({ ok: true, text: `[mission ${missionId}] Smoke — active` }),
    missions: [],
  });
  const delivery = new FakeDelivery();
  const bridge = new Bridge({ runner, delivery, config: makeConfig(), logger: quiet });
  t.after(() => bridge.stop());
  await bridge.reconcile();
  assert.ok(await waitFor(() => delivery.deliverCalls.length === 1));
  assert.equal(delivery.deliverCalls[0].member.model, "gpt-5.6-luna");
  // Edit the member's instance/model while the loop keeps running — the next
  // arrival must carry the new selection, not the loop-start snapshot.
  bridge.config = makeConfig({ members: [{ id: "t3-codex", instance: "zcode", model: "builtin:bigmodel-coding-plan/GLM-5.3-Flash" }] });
  assert.ok(await waitFor(() => delivery.deliverCalls.length === 2));
  assert.equal(delivery.deliverCalls[1].member.instance, "zcode");
  assert.equal(delivery.deliverCalls[1].member.model, "builtin:bigmodel-coding-plan/GLM-5.3-Flash");
});

test("retries resolve the member at retry time — a config edit heals with the new selection", async (t) => {
  const runner = new ScriptedRunner({
    roster: [{ id: "t3-codex", status: "active (1s ago)" }],
    receiveScript: [{ code: 0, stdout: ARRIVAL("ms_3333333333333333") }],
    missionShow: () => ({ ok: true, text: "[mission ms_3333333333333333] Parked — active" }),
    missions: [],
  });
  const delivery = new FakeDelivery();
  delivery.failDeliver = true;
  const bridge = new Bridge({ runner, delivery, t3: {}, config: makeConfig(), logger: quiet });
  t.after(() => bridge.stop());
  await bridge.reconcile();
  assert.ok(await waitFor(() => runner.calls.missionShow.length === 1)); // failed → queued
  delivery.failDeliver = false;
  bridge.config = makeConfig({ members: [{ id: "t3-codex", instance: "zcode", model: "GLM-5.3-Flash" }] });
  await bridge.reconcile();
  assert.equal(delivery.deliverCalls.length, 1);
  assert.equal(delivery.deliverCalls[0].member.model, "GLM-5.3-Flash"); // not the failure-time snapshot
});

test("arrival for a member that left the table is skipped, not delivered with a stale snapshot", async (t) => {
  const runner = new ScriptedRunner({
    roster: [{ id: "t3-codex", status: "active (1s ago)" }],
    receiveScript: [{ code: 0, stdout: ARRIVAL("ms_4444444444444444") }],
    missionShow: () => ({ ok: true, text: "[mission ms_4444444444444444] Smoke — active" }),
    missions: [],
  });
  const delivery = new FakeDelivery();
  const bridge = new Bridge({ runner, delivery, config: makeConfig(), logger: quiet });
  t.after(() => bridge.stop());
  await bridge.reconcile();
  assert.ok(await waitFor(() => runner.calls.receive.length >= 1));
  // Member disabled between reconciles: the arrival lands before the loop is
  // removed — it must be skipped before even re-reading the mission.
  bridge.config = makeConfig({ members: [{ id: "t3-codex", instance: "codex", model: "gpt-5.6-luna", enabled: false }] });
  assert.ok(await waitFor(() => runner.calls.receive.length >= 2)); // arrival batch consumed
  assert.equal(runner.calls.missionShow.length, 0); // skipped before the authority re-read
  assert.equal(delivery.deliverCalls.length, 0);
});

test("a delivery in flight absorbs the concurrent triggers: loop arrival, sweep, and a second reconcile", async (t) => {
  const MISSION = "ms_5555555555555555";
  const runner = new ScriptedRunner({
    roster: [{ id: "t3-codex", status: "active (1s ago)" }],
    receiveScript: [{ code: 0, stdout: ARRIVAL(MISSION) }],
    missionShow: () => ({ ok: true, text: `[mission ${MISSION}] Smoke — active` }),
    missions: [{ id: MISSION, station: "build" }], // the sweep sees the same parked mission
  });
  const delivery = new FakeDelivery();
  delivery.threadExists = async () => false; // sweep would deliver it
  let releaseDelivery;
  const gate = new Promise((resolve) => (releaseDelivery = resolve));
  const innerDeliver = delivery.deliver.bind(delivery);
  delivery.deliver = async (args) => {
    const threadId = await innerDeliver(args);
    await gate; // hold the first delivery while the other triggers fire
    return threadId;
  };
  const bridge = new Bridge({ runner, delivery, t3: {}, config: makeConfig(), logger: quiet });
  t.after(() => bridge.stop());

  const firstCycle = bridge.reconcile(); // sweep delivers, blocks on the gate
  assert.ok(await waitFor(() => delivery.deliverCalls.length === 1));
  // While the delivery is in flight: the receive loop's arrival for the same
  // mission must skip, and an overlapping reconcile tick must not double it.
  await bridge.reconcile();
  releaseDelivery();
  await firstCycle;
  assert.ok(await waitFor(() => runner.calls.receive.length >= 2)); // the loop consumed its arrival and re-hung
  assert.equal(delivery.deliverCalls.length, 1); // exactly one delivery, one thread
  assert.equal(bridge.watching.length, 1);
});
