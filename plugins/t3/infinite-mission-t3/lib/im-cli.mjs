import { runCapture } from "./spawn.mjs";

// The whole im surface the bridge needs. Tests substitute a scripted runner
// with the same method shapes; the bridge never spawns im directly.

const ROSTER_LINE = /^  (.+?) — (.+?) \[(execute|publish|manage)\]$/;
const WORKSPACE_LINE = /^  ([✓✗]) (.+)$/;
const MISSIONS_LINE = /^\[mission (ms_[0-9a-f]{6,64})\] at (\S+)/;
const RESULTS_LINE = /^(ms_[0-9a-f]{6,64})  from:(\S+)  (.*)$/;

export class ProcessImRunner {
  constructor({ imBin = "im" } = {}) {
    this.imBin = imBin;
  }

  #run(args, workspace, { signal } = {}) {
    return runCapture(this.imBin, args, { cwd: workspace, signal });
  }

  /** @returns {Promise<Array<{ id: string, status: string }>>} */
  async roster(workspace) {
    const res = await this.#run(["agents", "--all"], workspace);
    if (res.code !== 0) {
      throw new Error(`im agents failed (exit ${res.code}): ${res.stderr.trim().slice(0, 200)}`);
    }
    const agents = [];
    for (const line of res.stdout.split("\n")) {
      const match = line.match(ROSTER_LINE);
      if (match) agents.push({ id: match[1], status: match[2] });
    }
    return agents;
  }

  async join(workspace, memberId) {
    return this.#run(["join", memberId], workspace);
  }

  /** Archive the member and release its stations (self-service). */
  async leave(workspace, memberId) {
    const res = await this.#run(["leave", memberId], workspace);
    if (res.code !== 0) {
      throw new Error(`im leave ${memberId} failed (exit ${res.code}): ${res.stderr.trim().slice(0, 200)}`);
    }
    return res.stdout;
  }

  /**
   * One blocking receive cycle. Delivery and timeout both exit 0; a non-zero
   * exit is a real failure the caller logs and backs off from.
   */
  async receive(workspace, memberId, timeoutSec, signal) {
    return this.#run(
      ["receive", memberId, "--wait", "--timeout", String(Math.max(1, Math.trunc(timeoutSec)))],
      workspace,
      { signal },
    );
  }

  /** @returns {Promise<{ ok: boolean, text: string }>} */
  async missionShow(workspace, missionId, memberId) {
    const res = await this.#run(["mission", "show", missionId, "--for", memberId], workspace);
    return { ok: res.code === 0, text: res.stdout };
  }

  /** Durable terminal result for a Work-origin mission (newer IM cores). */
  async missionResult(workspace, missionId) {
    const res = await this.#run(["mission", "result", missionId], workspace);
    return { ok: res.code === 0, text: res.stdout };
  }

  /** Append-only mission history; also available on older IM cores. */
  async missionEvents(workspace, missionId) {
    const res = await this.#run(["mission", "events", missionId], workspace);
    return { ok: res.code === 0, text: res.stdout };
  }

  /** Active missions at the member's stations, with the station key. */
  async #missionsEntries(workspace, memberId) {
    const res = await this.#run(["missions", memberId], workspace);
    if (res.code !== 0) {
      throw new Error(`im missions ${memberId} failed (exit ${res.code}): ${res.stderr.trim().slice(0, 200)}`);
    }
    const entries = [];
    for (const line of res.stdout.split("\n")) {
      const match = line.match(MISSIONS_LINE);
      if (match) entries.push({ id: match[1], station: match[2] });
    }
    return entries;
  }

  /** Active mission ids at the member's stations. */
  async missions(workspace, memberId) {
    const entries = await this.#missionsEntries(workspace, memberId);
    return entries.map((entry) => entry.id);
  }

  /** Active missions as `{ id, station }` — the reconcile sweep's input. */
  async missionsAt(workspace, memberId) {
    return this.#missionsEntries(workspace, memberId);
  }

  /**
   * Ended Work-origin results addressed to the member's duty stations, as
   * `{ id, station, objective }` — the result sweep's input. Older IM cores
   * without the command exit non-zero (the caller treats that as empty).
   */
  async results(workspace, memberId) {
    const res = await this.#run(["results", memberId], workspace);
    if (res.code !== 0) {
      throw new Error(`im results ${memberId} failed (exit ${res.code}): ${res.stderr.trim().slice(0, 200)}`);
    }
    const entries = [];
    for (const line of res.stdout.split("\n")) {
      const match = line.match(RESULTS_LINE);
      if (match) entries.push({ id: match[1], station: match[2], objective: match[3] });
    }
    return entries;
  }

  /** Live workspace paths from the global registry. */
  async workspaces() {
    const res = await this.#run(["workspaces"], process.cwd());
    if (res.code !== 0) {
      throw new Error(`im workspaces failed (exit ${res.code}): ${res.stderr.trim().slice(0, 200)}`);
    }
    const paths = [];
    for (const line of res.stdout.split("\n")) {
      const match = line.match(WORKSPACE_LINE);
      if (match && match[1] === "✓") paths.push(match[2]);
    }
    return paths;
  }
}
