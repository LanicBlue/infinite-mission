#!/usr/bin/env node
// t3-im-bridge: InfiniteMission member(s) whose missions run as T3 threads.
//
// Recipe proven by the DSH bridge (packages/integrations/dsh-im-bridge):
// one `im receive --wait` child per (workspace × member), guard-join against
// id auto-suffixing, a closed-set note parser, mission re-verification via
// `im mission show` before acting, and — crucially — the bridge never
// submits: the T3 agent itself runs `im mission submit` per the duty
// preamble attached to every delivered brief.
//
// T3 side is driven through its public HTTP API only (zero T3 fork):
// project/thread creation, turn start, and settle via /api/orchestration/*.

import path from "node:path";
import { pathToFileURL } from "node:url";
import { readConfigFile, readT3ImMembers, defaultConfigPath } from "./lib/config.mjs";
import { ProcessImRunner } from "./lib/im-cli.mjs";
import { HttpT3Client } from "./lib/t3-client.mjs";
import { Delivery } from "./lib/delivery.mjs";
import { parseReceiveOutput } from "./lib/notes.mjs";
import { sleep } from "./lib/spawn.mjs";

const log = (tag, message) => {
  console.log(`${new Date().toISOString()} [${tag}] ${message}`);
};

/** Console logger with a bounded ring buffer for the admin UI's log tail. */
function teeLogger(ring, limit = 300) {
  return (tag, message) => {
    const line = `${new Date().toISOString()} [${tag}] ${message}`;
    ring.push(line);
    if (ring.length > limit) ring.shift();
    console.log(line);
  };
}

export class Bridge {
  constructor({ runner, delivery, t3 = null, config, configPath = null, logger = log }) {
    this.runner = runner;
    this.delivery = delivery;
    this.t3 = t3 ?? delivery?.t3 ?? null;
    this.config = config;
    this.configPath = configPath;
    this.log = logger;
    // key `${workspace}\0${memberId}` → { controller, running }
    this.loops = new Map();
    // membership-end mutes: the member was archived/removed; do not rejoin
    // or restart its loop for this process lifetime.
    this.muted = new Set();
    // deliveries that failed (e.g. T3 down) and retry on each reconcile
    this.retryQueue = [];
    // member ids this bridge has ensured at least once (leave candidates)
    this.ownedMembers = new Set();
    // delivered threads whose turn we watch once: when the turn reaches a
    // terminal state, settle if the mission left the member's list (the
    // submitter itself never receives im's mission-ended notice).
    this.watching = [];
    this.timer = null;
  }

  key(workspace, memberId) {
    return `${workspace}\0${memberId}`;
  }

  tag(workspace, memberId) {
    return `${path.basename(workspace)}/${memberId}`;
  }

  async start() {
    await this.reconcile();
    this.timer = setInterval(() => {
      this.reconcile().catch((err) => this.log("bridge", `reconcile failed: ${err.message}`));
    }, this.config.rescanSec * 1000);
  }

  async stop() {
    if (this.timer) clearInterval(this.timer);
    for (const entry of this.loops.values()) entry.controller.abort();
  }

  async #reloadConfig() {
    if (!this.configPath) return;
    const result = readConfigFile(this.configPath);
    if (!result.ok) {
      this.log("bridge", `config reload rejected, keeping last good config: ${result.errors.join("; ")}`);
      return;
    }
    this.config = result.config;
  }

  /**
   * The member table is authored in T3's settings UI (settings.json
   * `imBridge.members`) and wins when present; the bridge config file's own
   * members list is the fallback when that key is absent.
   */
  #resolveMembers() {
    try {
      const fromT3 = readT3ImMembers(this.config.t3.home);
      if (fromT3 !== null) return { members: fromT3, source: "T3 settings (imBridge)" };
    } catch (err) {
      this.log("bridge", `reading T3 imBridge members failed (${err.message}) — using config file list`);
    }
    return { members: this.config.members, source: "bridge config file" };
  }

  async #workspaces() {
    if (this.config.workspaces !== null) return this.config.workspaces;
    try {
      return await this.runner.workspaces();
    } catch (err) {
      this.log("bridge", `im workspaces failed: ${err.message}`);
      return [];
    }
  }

  async reconcile() {
    await this.#reloadConfig();
    const workspaces = await this.#workspaces();
    const { members, source } = this.#resolveMembers();

    const wanted = new Set();
    for (const workspace of workspaces) {
      for (const member of members) {
        if (!member.enabled) continue;
        const key = this.key(workspace, member.id);
        if (this.muted.has(key)) continue;
        wanted.add(key);
        try {
          await this.#ensureMember(workspace, member);
        } catch (err) {
          this.log(this.tag(workspace, member.id), `member setup failed: ${err.message}`);
          continue;
        }
        this.ownedMembers.add(member.id);
        this.#ensureLoop(workspace, member);
      }
    }
    await this.#leaveDisabled(workspaces, members);

    // Loops that are no longer configured (or whose member got muted) stop.
    for (const [key, entry] of [...this.loops]) {
      if (wanted.has(key)) continue;
      if (!entry.controller.signal.aborted) entry.controller.abort();
      this.loops.delete(key);
      this.log(key.replace("\0", "/"), "loop removed (not configured or membership ended)");
    }

    await this.#flushRetries();
    await this.#sweepUndelivered(workspaces, members);
    await this.#pollWatchers();
  }

  /**
   * Disabling (or removing) a member deregisters it in im: `im leave`
   * archives the member and hands its stations back to the user.
   */
  async #leaveDisabled(workspaces, members) {
    const enabledIds = new Set(members.filter((m) => m.enabled).map((m) => m.id));
    const knownIds = new Set([...members.map((m) => m.id), ...this.ownedMembers]);
    for (const workspace of workspaces) {
      let roster;
      try {
        roster = await this.runner.roster(workspace);
      } catch (err) {
        this.log("bridge", `roster check for leave failed: ${err.message}`);
        continue;
      }
      for (const id of knownIds) {
        if (enabledIds.has(id)) continue;
        const entry = roster.find((agent) => agent.id === id);
        if (!entry || entry.status === "archived") continue;
        try {
          await this.runner.leave(workspace, id);
          this.log(this.tag(workspace, id), "member disabled — left im (archived, stations released)");
        } catch (err) {
          this.log(this.tag(workspace, id), `leave failed: ${err.message}`);
        }
      }
    }
  }

  async #ensureMember(workspace, member) {
    const roster = await this.runner.roster(workspace);
    const exact = roster.find((agent) => agent.id === member.id);
    if (exact && exact.status !== "archived") return;
    // Absent OR archived → join reactivates the same id (disable → `im leave`
    // archives it; re-enabling must bring it back).
    await this.runner.join(workspace, member.id);
    const rosterAfter = await this.runner.roster(workspace);
    if (!rosterAfter.some((agent) => agent.id === member.id)) {
      throw new Error("join landed on a suffixed id (im adds -2 on conflict) — resolve the conflict or rename the member");
    }
  }

  #ensureLoop(workspace, member) {
    const key = this.key(workspace, member.id);
    if (this.loops.has(key)) return;
    const controller = new AbortController();
    const entry = { controller, running: true };
    this.loops.set(key, entry);
    this.#loop(workspace, member, entry)
      .catch((err) => {
        this.log(this.tag(workspace, member.id), `loop crashed: ${err.message}`);
      })
      .finally(() => {
        entry.running = false;
      });
  }

  async #loop(workspace, member, entry) {
    const tag = this.tag(workspace, member.id);
    const { controller } = entry;
    this.log(tag, "listening");
    while (!controller.signal.aborted) {
      let result;
      try {
        result = await this.runner.receive(workspace, member.id, this.config.receiveTimeoutSec, controller.signal);
      } catch (err) {
        if (controller.signal.aborted) return;
        this.log(tag, `receive error: ${err.message}`);
        await sleep(this.config.rescanSec * 1000, controller.signal);
        continue;
      }
      if (controller.signal.aborted) return;

      if (result.code !== 0) {
        this.log(tag, `receive exited ${result.code}: ${result.stderr.trim().slice(0, 200)}`);
        await sleep(this.config.rescanSec * 1000, controller.signal);
        continue;
      }

      const notes = parseReceiveOutput(result.stdout);
      if (notes.unknown.length) {
        this.log(tag, `dropped ${notes.unknown.length} unrecognized output line(s)`);
      }

      if (notes.membershipEnd) {
        this.log(tag, "membership ended — loop stopped; restart the bridge after re-joining this member");
        this.muted.add(this.key(workspace, member.id));
        return;
      }

      for (const missionId of notes.ended) {
        try {
          const settled = await this.delivery.settle(member.id, missionId);
          this.log(tag, `mission ${missionId} ended — thread ${settled ? "settled" : "already settled or absent"}`);
        } catch (err) {
          this.log(tag, `settle failed for ${missionId}: ${err.message}`);
        }
      }

      for (const arrival of notes.arrivals) {
        await this.#handleArrival(workspace, member, arrival, tag);
      }
    }
  }

  async #handleArrival(workspace, member, arrival, tag) {
    await this.#tryDeliver(workspace, member, arrival.station, arrival.missionId, tag);
  }

  /**
   * Deliver a mission to T3: the note (or sweep) is only a trigger, so
   * re-read the mission from the authority first. A failed show means the
   * mission moved on — drop it.
   */
  async #tryDeliver(workspace, member, station, missionId, tag) {
    let show;
    try {
      show = await this.runner.missionShow(workspace, missionId, member.id);
    } catch (err) {
      this.log(tag, `mission show threw for ${missionId}@${station}: ${err.message}`);
      return;
    }
    if (!show.ok) {
      this.log(tag, `stale arrival dropped: ${missionId}@${station} (mission show failed)`);
      return;
    }
    try {
      const threadId = await this.delivery.deliver({
        workspacePath: workspace,
        member,
        station,
        missionId,
        brief: show.text,
      });
      this.log(tag, `delivered ${missionId}@${station} → T3 thread ${threadId}`);
      this.watching.push({ workspace, member, missionId, threadId });
    } catch (err) {
      this.log(tag, `delivery failed for ${missionId}@${station}: ${err.message} (queued for retry)`);
      this.retryQueue.push({ workspace, member, station, missionId, attempts: 0 });
    }
  }

  /**
   * Reconcile sweep: a mission parked at an enabled member's station with no
   * live T3 thread never reached T3 — its arrival note was consumed by a
   * delivery that failed past the retry budget, or the bridge restarted
   * around it. Deliver it again; a live thread (deterministic id) is the
   * ground truth that a round already arrived, so delivered missions —
   * including follow-up rounds into their existing threads — are skipped.
   */
  async #sweepUndelivered(workspaces, members) {
    if (!this.t3) return;
    const queued = new Set(this.retryQueue.map((i) => `${i.workspace}\0${i.member.id}\0${i.missionId}`));
    const watched = new Set(this.watching.map((w) => `${w.workspace}\0${w.member.id}\0${w.missionId}`));
    for (const workspace of workspaces) {
      for (const member of members) {
        if (!member.enabled) continue;
        if (this.muted.has(this.key(workspace, member.id))) continue;
        let entries;
        try {
          entries = await this.runner.missionsAt(workspace, member.id);
        } catch (err) {
          this.log(this.tag(workspace, member.id), `sweep: im missions failed (${err.message})`);
          continue;
        }
        for (const { id: missionId, station } of entries) {
          const dedupe = `${workspace}\0${member.id}\0${missionId}`;
          if (queued.has(dedupe) || watched.has(dedupe)) continue;
          let arrived;
          try {
            arrived = await this.delivery.hasThread(member.id, missionId);
          } catch (err) {
            // T3 unreachable — retries own recovery; re-probe next tick.
            this.log("bridge", `sweep aborted (T3 probe failed): ${err.message}`);
            return;
          }
          if (arrived) continue;
          this.log(this.tag(workspace, member.id), `sweep: ${missionId}@${station} has no T3 thread — delivering`);
          await this.#tryDeliver(workspace, member, station, missionId, this.tag(workspace, member.id));
        }
      }
    }
  }

  /**
   * im's mission-ended notice skips the member whose own submit ended the
   * mission, so natural completions never arrive as notes. Watch each
   * delivered thread for one turn-terminal transition, then settle only when
   * the mission has actually left the member's list. One `im missions` call
   * per turn completion — no polling loops beyond the reconcile tick.
   */
  async #pollWatchers() {
    if (!this.watching.length || !this.t3) return;
    const stillWatching = [];
    for (const watch of this.watching) {
      const tag = this.tag(watch.workspace, watch.member.id);
      let state;
      try {
        const snapshot = await this.t3.threadDetail(watch.threadId);
        state = snapshot?.thread?.latestTurn?.state ?? null;
      } catch (err) {
        this.log(tag, `watch probe failed for ${watch.threadId}: ${err.message}`);
        stillWatching.push(watch);
        continue;
      }
      if (state !== "completed" && state !== "error" && state !== "interrupted") {
        stillWatching.push(watch);
        continue;
      }
      let activeMissionIds = [];
      try {
        activeMissionIds = await this.runner.missions(watch.workspace, watch.member.id);
      } catch (err) {
        this.log(tag, `watch: im missions failed (${err.message}) — dropping watch for ${watch.missionId}`);
        continue;
      }
      if (!activeMissionIds.includes(watch.missionId)) {
        try {
          await this.delivery.settle(watch.member.id, watch.missionId);
          this.log(tag, `turn ${state} and mission ${watch.missionId} closed — thread settled`);
        } catch (err) {
          this.log(tag, `settle failed for ${watch.missionId}: ${err.message}`);
        }
      } else {
        this.log(
          tag,
          `turn ${state} but mission ${watch.missionId} is still open — the agent likely skipped its submit; thread left active for a follow-up round`,
        );
      }
    }
    this.watching = stillWatching;
  }

  async #flushRetries() {
    if (!this.retryQueue.length) return;
    const pending = this.retryQueue;
    this.retryQueue = [];
    for (const item of pending) {
      item.attempts += 1;
      if (item.attempts > this.config.maxDeliverAttempts) {
        // Park instead of dropping: a dropped item is a mission stranded at
        // its station (its arrival note was already consumed). The counter
        // resets and later reconciles keep trying — T3 coming back heals it.
        item.attempts = 0;
        this.retryQueue.push(item);
        this.log(
          this.tag(item.workspace, item.member.id),
          `parking ${item.missionId}@${item.station} after ${this.config.maxDeliverAttempts} attempt(s) — stays queued for later reconciles`,
        );
        continue;
      }
      try {
        const show = await this.runner.missionShow(item.workspace, item.missionId, item.member.id);
        if (!show.ok) {
          this.log(this.tag(item.workspace, item.member.id), `retry dropped: ${item.missionId} no longer shows`);
          continue;
        }
        const threadId = await this.delivery.deliver({
          workspacePath: item.workspace,
          member: item.member,
          station: item.station,
          missionId: item.missionId,
          brief: show.text,
        });
        this.log(this.tag(item.workspace, item.member.id), `retry delivered ${item.missionId}@${item.station} → ${threadId}`);
        this.watching.push({ workspace: item.workspace, member: item.member, missionId: item.missionId, threadId });
      } catch (err) {
        this.log(this.tag(item.workspace, item.member.id), `retry ${item.attempts} failed for ${item.missionId}: ${err.message}`);
        this.retryQueue.push(item);
      }
    }
  }

  /** Runtime snapshot for the admin UI. */
  statusSnapshot() {
    const { members, source } = this.#resolveMembers();
    return {
      membersSource: source,
      members: members.map((m) => ({ id: m.id, enabled: m.enabled, instance: m.instance, model: m.model })),
      loops: [...this.loops.entries()].map(([key, entry]) => {
        const [workspace, memberId] = key.split("\0");
        return {
          workspace,
          memberId,
          running: entry.running,
          aborted: entry.controller.signal.aborted,
        };
      }),
      watching: this.watching.map((watch) => ({ missionId: watch.missionId, threadId: watch.threadId })),
      retryQueue: this.retryQueue.map((item) => ({ missionId: item.missionId, attempts: item.attempts })),
      muted: [...this.muted].map((key) => key.replace("\0", "/")),
    };
  }
}

function parseArgs(argv) {
  const configPath = defaultConfigPath();
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--config" && argv[i + 1]) return { configPath: argv[i + 1] };
    if (argv[i].startsWith("--config=")) return { configPath: argv[i].slice("--config=".length) };
  }
  return { configPath };
}

async function main() {
  const { configPath } = parseArgs(process.argv.slice(2));
  const loaded = readConfigFile(configPath);
  if (!loaded.ok) {
    console.error(`t3-im-bridge: invalid config:\n  ${loaded.errors.join("\n  ")}`);
    process.exit(1);
  }
  const { config } = loaded;

  const runner = new ProcessImRunner({ imBin: config.imBin });
  const t3 = new HttpT3Client(config.t3);
  const delivery = new Delivery(t3);
  const ring = [];
  const logger = teeLogger(ring);
  const bridge = new Bridge({ runner, delivery, t3, config, configPath, logger });


  let shuttingDown = false;
  const shutdown = () => {
    if (shuttingDown) return;
    shuttingDown = true;
    log("bridge", "shutting down");
    bridge.stop();
    setTimeout(() => process.exit(0), 1000).unref();
  };
  process.on("SIGTERM", shutdown);
  process.on("SIGINT", shutdown);

  await bridge.start();
  const resolvedMembers = readT3ImMembers(config.t3.home);
  const activeMembers = (resolvedMembers ?? config.members).filter((m) => m.enabled);
  const memberList = activeMembers.map((m) => m.id).join(", ") || "(none)";
  const memberSource = resolvedMembers !== null ? "T3 settings (imBridge)" : "bridge config file";
  const workspaceList =
    config.workspaces !== null ? config.workspaces.join(", ") : "(from `im workspaces` registry)";
  log("bridge", `started — members: ${memberList} [${memberSource}]; workspaces: ${workspaceList}; config: ${configPath}`);
}

const isEntry =
  process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href;

if (isEntry) {
  main().catch((err) => {
    console.error(`t3-im-bridge: fatal: ${err?.stack ?? err}`);
    process.exit(1);
  });
}
