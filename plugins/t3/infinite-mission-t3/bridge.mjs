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

import fs from "node:fs";
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
    // member ids this bridge has joined and not yet left (leave candidates),
    // plus acknowledged Work-origin results. Persisted beside the config
    // (`<config>.state.json`) so renaming or removing a member still archives
    // the old id (`im leave`) after a bridge restart, and so a result whose
    // note was consumed but whose delivery was lost (crash, T3 outage past the
    // retry budget) is found again by the result sweep instead of being lost —
    // the ended mission never reappears in the active-arrival sweep. Null
    // configPath (tests) disables persistence.
    this.statePath = configPath ? `${configPath.replace(/\.json$/, "")}.state.json` : null;
    const persisted = this.#readState();
    this.ownedMembers = new Set(persisted.ownedMembers);
    // `${workspace}\0${memberId}\0${missionId}` of Work-origin results whose
    // delivery is known good (this process delivered them, or a live thread
    // proved a previous process did). The result sweep re-delivers the rest.
    this.ackedResults = new Set(persisted.ackedResults);
    // delivered threads whose turn we watch once: when the turn reaches a
    // terminal state, settle if the mission left the member's list (the
    // submitter itself never receives im's mission-ended notice). Holds
    // member ids only — the member's instance/model is re-read at delivery
    // time, never snapshotted here.
    this.watching = [];
    // Turn-error reopen budget per (member\0mission); in-memory like the watch list.
    this.reopenCounts = new Map();
    // `${workspace}\0${memberId}\0${missionId}` of deliveries currently
    // running — the receive loop, the sweep, and the retry queue can all
    // trigger the same mission concurrently; the first one wins.
    this.inflight = new Set();
    this.reconciling = false;
    this.timer = null;
  }

  key(workspace, memberId) {
    return `${workspace}\0${memberId}`;
  }

  #readState() {
    const empty = { ownedMembers: [], ackedResults: [] };
    if (!this.statePath) return empty;
    try {
      const raw = JSON.parse(fs.readFileSync(this.statePath, "utf8"));
      const strings = (value) =>
        Array.isArray(value) ? value.filter((id) => typeof id === "string" && id.length > 0) : [];
      return { ownedMembers: strings(raw?.ownedMembers), ackedResults: strings(raw?.ackedResults) };
    } catch (err) {
      if (err.code !== "ENOENT") {
        this.log("bridge", `state file unreadable, starting with empty state (${err.message})`);
      }
      return empty;
    }
  }

  #writeState() {
    if (!this.statePath) return;
    try {
      const tmp = `${this.statePath}.tmp`;
      fs.writeFileSync(
        tmp,
        `${JSON.stringify(
          { ownedMembers: [...this.ownedMembers], ackedResults: [...this.ackedResults] },
          null,
          2,
        )}\n`,
      );
      fs.renameSync(tmp, this.statePath);
    } catch (err) {
      this.log("bridge", `state write failed (${err.message}) — archive/ack bookkeeping may be lost on restart`);
    }
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

  /**
   * The member's live definition at delivery time. Loops, the retry queue,
   * and watchers all carry the member id only and resolve through this —
   * editing a member's instance/model in T3's settings applies to the next
   * arrival without restarting the bridge. A member that is gone or disabled
   * resolves to null (reconcile stops its loop within a tick; the sweep
   * heals any arrival dropped in between once the table is readable again).
   */
  #lookupMember(memberId) {
    const { members } = this.#resolveMembers();
    return members.find((member) => member.id === memberId && member.enabled) ?? null;
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
    // One cycle at a time: a slow pass (subprocess spawns per workspace ×
    // member) can outlast the rescan interval, and overlapping passes
    // double-deliver the same parked missions.
    if (this.reconciling) {
      this.log("bridge", "reconcile skipped — previous cycle still running");
      return;
    }
    this.reconciling = true;
    try {
      await this.#reconcileCycle();
    } finally {
      this.reconciling = false;
    }
  }

  async #reconcileCycle() {
    await this.#reloadConfig();
    const workspaces = await this.#workspaces();
    const { members, source } = this.#resolveMembers();

    const wanted = new Set();
    let ownedChanged = false;
    for (const workspace of workspaces) {
      for (const member of members) {
        if (!member.enabled) continue;
        const key = this.key(workspace, member.id);
        if (this.muted.has(key)) continue;
        wanted.add(key);
        let ensured = null;
        try {
          ensured = await this.#ensureMember(workspace, member);
        } catch (err) {
          this.log(this.tag(workspace, member.id), `member setup failed: ${err.message}`);
          continue;
        }
        await this.#syncMemberName(workspace, member, ensured);
        if (!this.ownedMembers.has(member.id)) ownedChanged = true;
        this.ownedMembers.add(member.id);
        this.#ensureLoop(workspace, member);
      }
    }
    const leftCount = await this.#leaveDisabled(workspaces, members);
    if (ownedChanged || leftCount > 0) this.#writeState();

    // Loops that are no longer configured (or whose member got muted) stop.
    for (const [key, entry] of [...this.loops]) {
      if (wanted.has(key)) continue;
      if (!entry.controller.signal.aborted) entry.controller.abort();
      this.loops.delete(key);
      this.log(key.replace("\0", "/"), "loop removed (not configured or membership ended)");
    }

    await this.#flushRetries();
    await this.#sweepUndelivered(workspaces, members);
    await this.#sweepUnackedResults(workspaces, members);
    await this.#pollWatchers();
  }

  /**
   * Disabling (or removing, or renaming away) a member deregisters it in im:
   * `im leave` archives the member and hands its stations back to the user.
   * Returns the number of successful leaves. A left id drops out of the
   * owned set — responsibility ends with the archive, and a human may then
   * reuse the id without the bridge leaving it out from under them; a leave
   * that failed anywhere keeps the id owned so the next tick retries.
   */
  async #leaveDisabled(workspaces, members) {
    const enabledIds = new Set(members.filter((m) => m.enabled).map((m) => m.id));
    const knownIds = new Set([...members.map((m) => m.id), ...this.ownedMembers]);
    const leftIds = new Set();
    const failedIds = new Set();
    let left = 0;
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
          left += 1;
          leftIds.add(id);
          this.log(this.tag(workspace, id), "member disabled — left im (archived, stations released)");
        } catch (err) {
          failedIds.add(id);
          this.log(this.tag(workspace, id), `leave failed: ${err.message}`);
        }
      }
    }
    for (const id of leftIds) {
      if (!failedIds.has(id)) this.ownedMembers.delete(id);
    }
    return left;
  }

  async #ensureMember(workspace, member) {
    const roster = await this.runner.roster(workspace);
    const exact = roster.find((agent) => agent.id === member.id);
    if (exact && exact.status !== "archived") return exact;
    // Absent OR archived → join reactivates the same id (disable → `im leave`
    // archives it; re-enabling must bring it back).
    await this.runner.join(workspace, member.id);
    const rosterAfter = await this.runner.roster(workspace);
    const landed = rosterAfter.find((agent) => agent.id === member.id);
    if (!landed) {
      throw new Error("join landed on a suffixed id (im adds -2 on conflict) — resolve the conflict or rename the member");
    }
    return landed;
  }

  /**
   * Keep im's display label in step with the member table: name edits in T3
   * settings land here within a tick as `im rename` (id-level rename no longer
   * exists — the id is a generated machine key that never changes).
   */
  async #syncMemberName(workspace, member, rosterEntry) {
    if (!rosterEntry) return;
    // "-" is im's clear marker — as a wanted name it could never converge
    // (clear ≠ "-"), so it reads as "no name". Names im would reject (newlines,
    // control characters, oversized) are skipped once, not retried every tick.
    let wanted = member.displayName?.trim() || null;
    if (wanted === "-") wanted = null;
    const invalid =
      wanted !== null &&
      (wanted.length > 64 || /[\x00-\x1f\x7f]/.test(wanted));
    if (invalid) {
      if (!this.warnedNames?.has(wanted)) {
        this.warnedNames ??= new Set();
        this.warnedNames.add(wanted);
        this.log(this.tag(workspace, member.id), `display name rejected by im's shape rules, not synced: ${JSON.stringify(wanted)}`);
      }
      return;
    }
    if ((rosterEntry.name ?? null) === wanted) return;
    try {
      await this.runner.rename(workspace, member.id, wanted);
      this.log(this.tag(workspace, member.id), `display name → ${wanted ?? "(cleared)"}`);
    } catch (err) {
      this.log(this.tag(workspace, member.id), `rename failed: ${err.message}`);
    }
  }

  #ensureLoop(workspace, member) {
    const key = this.key(workspace, member.id);
    if (this.loops.has(key)) return;
    const controller = new AbortController();
    const entry = { controller, running: true };
    this.loops.set(key, entry);
    // The loop owns the identity (member id), not the member definition —
    // instance/model are re-resolved on every delivery via #lookupMember.
    this.#loop(workspace, member.id, entry)
      .catch((err) => {
        this.log(this.tag(workspace, member.id), `loop crashed: ${err.message}`);
      })
      .finally(() => {
        entry.running = false;
      });
  }

  async #loop(workspace, memberId, entry) {
    const tag = this.tag(workspace, memberId);
    const { controller } = entry;
    this.log(tag, "listening");
    while (!controller.signal.aborted) {
      let result;
      try {
        result = await this.runner.receive(workspace, memberId, this.config.receiveTimeoutSec, controller.signal);
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
        this.muted.add(this.key(workspace, memberId));
        return;
      }

      for (const missionId of notes.ended) {
        try {
          const settled = await this.delivery.settle(memberId, missionId);
          this.log(tag, `mission ${missionId} ended — thread ${settled ? "settled" : "already settled or absent"}`);
        } catch (err) {
          this.log(tag, `settle failed for ${missionId}: ${err.message}`);
        }
      }

      for (const arrival of notes.arrivals) {
        await this.#handleArrival(workspace, memberId, arrival, tag);
      }
    }
  }

  async #handleArrival(workspace, memberId, arrival, tag) {
    await this.#tryDeliver(workspace, memberId, arrival.station, arrival.missionId, tag);
  }

  async #deliveryBrief(workspace, missionId, show) {
    // The ended signal is a machine contract, not show-text heuristics: the
    // header's status suffix and any "ended:" line can both be corrupted by
    // mission names and objectives (a multi-line name once turned every
    // result delivery into a duty turn that watch-settled and
    // sweep-redelivered forever). Layered instead: `im mission result`
    // succeeds exactly for ended missions; older cores fall back to the
    // event history, whose `#seq stamp mission.ended` lines are
    // machine-written and cannot be forged through payloads (JSON keeps
    // them single-line).
    let ended = false;
    let result = null;
    let events = null;
    if (typeof this.runner.missionResult === "function") {
      try {
        result = await this.runner.missionResult(workspace, missionId);
        ended = result.ok;
      } catch {
        ended = false; // treat like not-ended; the sweep re-delivers from `im results`
      }
    }
    if (!ended && typeof this.runner.missionEvents === "function") {
      try {
        events = await this.runner.missionEvents(workspace, missionId);
      } catch {
        events = null;
      }
      // Event-kind lines are `#seq <stamp> <kind>` with the payload on its
      // own indented line below — payloads are single-line JSON, so a
      // forged "mission.ended" inside a reason cannot anchor to `#seq`.
      ended =
        events !== null && /^#\d+\s.*mission\.ended$/m.test(String(events.text));
    }
    if (!ended) return { brief: show.text, ended: false };

    // mission_result work notes intentionally share the stable arrival-note
    // envelope; the authoritative ended check above is the distinction.
    // Supply the durable result and history, and make it explicit that no
    // submit duty remains.
    const sections = [
      "[InfiniteMission returned result]",
      "This Mission is already ended. Read the result below; do NOT submit or abandon it.",
      show.text.trimEnd(),
    ];
    if (result && result.ok && result.text.trim()) {
      sections.push("[Durable result]", result.text.trimEnd());
    }
    if (events === null && typeof this.runner.missionEvents === "function") {
      try {
        events = await this.runner.missionEvents(workspace, missionId);
      } catch (err) {
        this.log("bridge", `mission events lookup failed for ${missionId}: ${err.message}`);
        events = { ok: false, text: "" };
      }
    }
    if (events && events.ok && events.text.trim()) {
      sections.push("[Mission events]", events.text.trimEnd());
    }
    return { brief: sections.join("\n\n"), ended: true };
  }

  /**
   * Deliver a mission to T3: the note (or sweep) is only a trigger, so
   * re-read the mission from the authority first. A failed show means the
   * mission moved on — drop it. The member definition is resolved here, at
   * delivery time, so config edits apply without a loop restart; a member
   * that left the table (or was disabled) drops the trigger — the sweep
   * redelivers once it is back. The in-flight check is synchronous, so two
   * concurrent triggers (loop arrival × sweep × retry) cannot both deliver.
   */
  async #tryDeliver(workspace, memberId, station, missionId, tag) {
    const inflightKey = `${workspace}\0${memberId}\0${missionId}`;
    if (this.inflight.has(inflightKey)) {
      this.log(tag, `delivery skipped for ${missionId}@${station}: another delivery is in flight`);
      return;
    }
    this.inflight.add(inflightKey);
    try {
      const member = this.#lookupMember(memberId);
      if (!member) {
        this.log(tag, `delivery skipped for ${missionId}@${station}: member ${memberId} is not enabled in the member table`);
        return;
      }
      let show;
      try {
        show = await this.runner.missionShow(workspace, missionId, memberId);
      } catch (err) {
        this.log(tag, `mission show threw for ${missionId}@${station}: ${err.message}`);
        return;
      }
      if (!show.ok) {
        this.log(tag, `stale arrival dropped: ${missionId}@${station} (mission show failed)`);
        return;
      }
      const { brief, ended } = await this.#deliveryBrief(workspace, missionId, show);
      try {
        const threadId = await this.delivery.deliver({
          workspacePath: workspace,
          member,
          station,
          missionId,
          brief,
          ended,
        });
        this.log(tag, `delivered ${missionId}@${station} → T3 thread ${threadId}`);
        if (ended) {
          // A returned result that reached T3 is done: the mission is ended,
          // so no follow-up round can ever arrive. Ack it (persisted) so the
          // result sweep never re-delivers it.
          this.ackedResults.add(inflightKey);
          this.#writeState();
        } else {
          this.watching.push({ workspace, memberId, missionId, threadId });
        }
      } catch (err) {
        this.log(tag, `delivery failed for ${missionId}@${station}: ${err.message} (queued for retry)`);
        this.retryQueue.push({ workspace, memberId, station, missionId, attempts: 0 });
      }
    } finally {
      this.inflight.delete(inflightKey);
    }
  }

  /**
   * Reconcile sweep: a mission parked at an enabled member's station with no
   * live T3 thread never reached T3 — its arrival note was consumed by a
   * delivery that failed past the retry budget, or the bridge restarted
   * around it. Deliver it again; a live thread (deterministic id) is the
   * ground truth that a round already arrived, so delivered missions —
   * including follow-up rounds into their existing threads — are re-adopted
   * by turn state instead of skipped (#reconcileLiveThread).
   */
  async #sweepUndelivered(workspaces, members) {
    if (!this.t3) return;
    const queued = new Set(this.retryQueue.map((i) => `${i.workspace}\0${i.memberId}\0${i.missionId}`));
    const watched = new Set(this.watching.map((w) => `${w.workspace}\0${w.memberId}\0${w.missionId}`));
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
          if (queued.has(dedupe) || watched.has(dedupe) || this.inflight.has(dedupe)) continue;
          let thread;
          try {
            thread = await this.delivery.liveThread(member.id, missionId);
          } catch (err) {
            // T3 unreachable — retries own recovery; re-probe next tick.
            this.log("bridge", `sweep aborted (T3 probe failed): ${err.message}`);
            return;
          }
          if (thread) {
            await this.#reconcileLiveThread(workspace, member.id, station, missionId, thread, this.tag(workspace, member.id));
            continue;
          }
          this.log(this.tag(workspace, member.id), `sweep: ${missionId}@${station} has no T3 thread — delivering`);
          await this.#tryDeliver(workspace, member.id, station, missionId, this.tag(workspace, member.id));
        }
      }
    }
  }

  /**
   * Result sweep: a mission_result work note is consumed by `im receive`, so
   * a delivery that failed past the retry budget — or a bridge restart around
   * it — loses the only wake: an ended mission never reappears in the
   * active-arrival sweep above. Recover it from the durable side instead:
   * `im results <member>` lists every ended Work-origin mission addressed to
   * the member's duty stations, regardless of whether its note survived.
   * Idempotent by the same rule as the arrival sweep (a live deterministic
   * thread = already delivered) plus the persisted ack set, so re-running is
   * free and a redelivered result cannot loop.
   */
  async #sweepUnackedResults(workspaces, members) {
    if (!this.t3) return;
    if (typeof this.runner.results !== "function") return; // scripted/older runner
    const queued = new Set(this.retryQueue.map((i) => `${i.workspace}\0${i.memberId}\0${i.missionId}`));
    const watched = new Set(this.watching.map((w) => `${w.workspace}\0${w.memberId}\0${w.missionId}`));
    for (const workspace of workspaces) {
      for (const member of members) {
        if (!member.enabled) continue;
        if (this.muted.has(this.key(workspace, member.id))) continue;
        let entries;
        try {
          entries = await this.runner.results(workspace, member.id);
        } catch (err) {
          // Older IM cores have no `im results` — treat like an empty list.
          this.log(this.tag(workspace, member.id), `result sweep: im results unavailable (${err.message})`);
          continue;
        }
        for (const { id: missionId, station } of entries) {
          const dedupe = `${workspace}\0${member.id}\0${missionId}`;
          if (this.ackedResults.has(dedupe) || queued.has(dedupe) || watched.has(dedupe) || this.inflight.has(dedupe)) {
            continue;
          }
          let arrived;
          try {
            arrived = await this.delivery.hasThread(member.id, missionId);
          } catch (err) {
            this.log("bridge", `result sweep aborted (T3 probe failed): ${err.message}`);
            return;
          }
          if (arrived) {
            // Delivered by a previous process life; ack so later ticks stop
            // re-probing T3 for it.
            this.ackedResults.add(dedupe);
            this.#writeState();
            continue;
          }
          this.log(this.tag(workspace, member.id), `result sweep: ${missionId} has no T3 thread — delivering result`);
          await this.#tryDeliver(workspace, member.id, station, missionId, this.tag(workspace, member.id));
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
      const tag = this.tag(watch.workspace, watch.memberId);
      let state;
      try {
        const snapshot = await this.t3.threadDetail(watch.threadId);
        const thread = snapshot?.thread;
        // A thread deleted in the T3 UI (or anywhere else) can never reach a
        // terminal turn state — treat the deletion itself as terminal and let
        // the reconcile sweep redeliver if the mission is still parked.
        if (!thread || thread.deletedAt) {
          this.log(tag, `thread ${watch.threadId} vanished — watch dropped for ${watch.missionId}; sweep will redeliver if still parked`);
          continue;
        }
        state = thread.latestTurn?.state ?? null;
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
        activeMissionIds = await this.runner.missions(watch.workspace, watch.memberId);
      } catch (err) {
        this.log(tag, `watch: im missions failed (${err.message}) — dropping watch for ${watch.missionId}`);
        continue;
      }
      if (!activeMissionIds.includes(watch.missionId)) {
        try {
          await this.delivery.settle(watch.memberId, watch.missionId);
          this.log(tag, `turn ${state} and mission ${watch.missionId} closed — thread settled`);
        } catch (err) {
          this.log(tag, `settle failed for ${watch.missionId}: ${err.message}`);
        }
      } else if (state === "error" && this.config.turnErrorReopen === true) {
        // A turn that died mid-flight (classically: the member's provider or
        // model was switched while the mission was open, and the thread's
        // accumulated history no longer fits the new runtime). Threads are
        // disposable scratch — every delivery re-reads `mission show`, the
        // ledger is the state — so delete the thread and let the reconcile
        // sweep redeliver into a FRESH session on the member's current
        // config.
        await this.#reopenTurnError(tag, watch.memberId, watch.missionId, watch.threadId);
        continue; // watch dropped: the redelivery registers a new watcher
      } else {
        this.log(
          tag,
          `turn ${state} but mission ${watch.missionId} is still open — the agent likely skipped its submit; thread left active for a follow-up round`,
        );
      }
    }
    this.watching = stillWatching;
  }

  /**
   * Turn errored with the mission still open — delete the thread so the
   * reconcile sweep redelivers into a fresh session on the member's current
   * config. Budgeted per (member, mission) so a persistently broken config
   * cannot loop forever; once exhausted, a human fixes the config and deletes
   * the parked thread in T3 (the sweep heals). Shared by the turn watcher and
   * the sweep's re-adoption of restart-orphaned threads.
   */
  async #reopenTurnError(tag, memberId, missionId, threadId) {
    const key = `${memberId}\0${missionId}`;
    const attempt = (this.reopenCounts.get(key) ?? 0) + 1;
    if (attempt > this.config.turnErrorReopenMax) {
      this.log(
        tag,
        `turn error on ${missionId}, reopen budget exhausted (${this.config.turnErrorReopenMax}) — thread left active; fix the member config, then delete the thread in T3 to retry`,
      );
      return false;
    }
    this.reopenCounts.set(key, attempt);
    try {
      await this.t3.deleteThread(threadId);
      this.log(
        tag,
        `turn error on ${missionId} — thread ${threadId} deleted (reopen ${attempt}/${this.config.turnErrorReopenMax}); sweep redelivers into a fresh session`,
      );
      return true;
    } catch (err) {
      this.log(tag, `turn-error reopen failed (${err.message}) — thread left active`);
      return false;
    }
  }

  /**
   * A bridge restart orphans the watchers of live threads: a turn that errors
   * afterwards goes unseen (and unreopened), and a turn that completes is
   * never settled. Whenever the sweep skips over a live thread — "already
   * delivered" — it first re-adopts it by turn state: an errored turn takes
   * the reopen path, an in-flight turn gets a fresh watch, a thread whose
   * turn never landed is re-delivered into, and a completed turn on a parked
   * mission keeps its legacy parked semantics (follow-up round or operator).
   */
  async #reconcileLiveThread(workspace, memberId, station, missionId, thread, tag) {
    const state = thread.latestTurn?.state ?? null;
    if (state === "error" && this.config.turnErrorReopen === true) {
      await this.#reopenTurnError(tag, memberId, missionId, thread.id);
      return;
    }
    if (state === null) {
      // Thread exists but no turn was ever dispatched into it — the round
      // never reached the agent (a crash between thread create and turn
      // start). Inject the brief (followup mode) rather than strand it.
      this.log(tag, `sweep: live thread ${thread.id} for ${missionId} has no turn — re-delivering the round`);
      await this.#tryDeliver(workspace, memberId, station, missionId, tag);
      return;
    }
    if (state !== "completed" && state !== "interrupted") {
      this.watching.push({ workspace, memberId, missionId, threadId: thread.id });
      this.log(tag, `sweep: re-armed watch on ${thread.id} for ${missionId} (turn ${state}; bridge restarted around it)`);
      return;
    }
    // Completed/interrupted with the mission still parked: the agent likely
    // skipped its submit — thread left active for a follow-up round.
  }

  async #flushRetries() {
    if (!this.retryQueue.length) return;
    const pending = this.retryQueue;
    this.retryQueue = [];
    for (const item of pending) {
      if (this.inflight.has(`${item.workspace}\0${item.memberId}\0${item.missionId}`)) {
        // Another path is delivering this mission right now; let it finish —
        // a successful delivery registers the turn watcher itself.
        this.retryQueue.push(item);
        continue;
      }
      item.attempts += 1;
      if (item.attempts > this.config.maxDeliverAttempts) {
        // Park instead of dropping: a dropped item is a mission stranded at
        // its station (its arrival note was already consumed). The counter
        // resets and later reconciles keep trying — T3 coming back heals it.
        item.attempts = 0;
        this.retryQueue.push(item);
        this.log(
          this.tag(item.workspace, item.memberId),
          `parking ${item.missionId}@${item.station} after ${this.config.maxDeliverAttempts} attempt(s) — stays queued for later reconciles`,
        );
        continue;
      }
      // The member definition is re-resolved per attempt, so a retry picks up
      // config edits made since the failure (or drops out when the member
      // left the table — the sweep owns it again then).
      const member = this.#lookupMember(item.memberId);
      if (!member) {
        this.log(this.tag(item.workspace, item.memberId), `retry dropped: member ${item.memberId} is not enabled in the member table`);
        continue;
      }
      try {
        const show = await this.runner.missionShow(item.workspace, item.missionId, item.memberId);
        if (!show.ok) {
          this.log(this.tag(item.workspace, item.memberId), `retry dropped: ${item.missionId} no longer shows`);
          continue;
        }
        const { brief, ended } = await this.#deliveryBrief(item.workspace, item.missionId, show);
        const threadId = await this.delivery.deliver({
          workspacePath: item.workspace,
          member,
          station: item.station,
          missionId: item.missionId,
          brief,
          ended,
        });
        this.log(this.tag(item.workspace, item.memberId), `retry delivered ${item.missionId}@${item.station} → ${threadId}`);
        if (ended) {
          this.ackedResults.add(`${item.workspace}\0${item.memberId}\0${item.missionId}`);
          this.#writeState();
        } else {
          this.watching.push({ workspace: item.workspace, memberId: item.memberId, missionId: item.missionId, threadId });
        }
      } catch (err) {
        this.log(this.tag(item.workspace, item.memberId), `retry ${item.attempts} failed for ${item.missionId}: ${err.message}`);
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
