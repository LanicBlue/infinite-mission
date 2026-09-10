import fs from "node:fs";
import path from "node:path";
import { randomUUID } from "node:crypto";

// Deterministic per-member thread ids: each member gets its own thread for a
// mission (`im-<memberId>-<missionId>`), so the thread's model/instance is
// the member's and watchers never collide across members. Probing the
// deterministic ids (then suffixed variants) replaces remembering state.
export function threadIdFor(memberId, missionId, attempt = 0) {
  return attempt > 0 ? `im-${memberId}-${missionId}-${attempt + 1}` : `im-${memberId}-${missionId}`;
}

/**
 * Whether a thread id belongs to this member's mission: the deterministic
 * base or any of its suffixed variants. Mission ids are fixed-length, so the
 * prefix cannot straddle two missions, and the member id inside it keeps
 * matches member-scoped (`im-t3-glm-…` never matches t3-glm-flash's threads).
 */
export function isMissionThreadId(threadId, memberId, missionId) {
  const base = threadIdFor(memberId, missionId);
  return threadId === base || threadId.startsWith(`${base}-`);
}

/** `[mission ms_x] NAME — status` → a T3 display title (missionId as fallback).
 * The header may sit a few lines down (result briefs open with a marker), so
 * the first lines are scanned but the match stays line-anchored. Shows the
 * member's display name when set — the id in the title is meaningless noise. */
export function titleFromBrief(brief, member, station, missionId) {
  const memberId = typeof member === "string" ? member : member.id;
  const who = typeof member === "string" ? member : (member.displayName?.trim() || member.id);
  const lines = String(brief).split("\n");
  const header = lines
    .slice(0, 5)
    .map((line) => line.match(/^\[mission ms_[0-9a-f]{6,64}\] (.*)$/))
    .find(Boolean);
  const name = header ? header[1].split(" — ")[0].trim() : "";
  return `im/${who}@${station}: ${name || missionId}`;
}

export function modelSelectionOf(member) {
  const selection = { instanceId: member.instance, model: member.model };
  if (member.options !== undefined) selection.options = member.options;
  return selection;
}

/**
 * Duty discipline travels as the message preamble (not a system prompt) —
 * the same contract the DSH bridge proved: the agent itself submits; the
 * bridge never submits on its behalf.
 */
export function dutyPreamble(member, workspace) {
  // Accepts the member object (preferred — carries the display name) or a
  // bare id (tests, legacy callers).
  const memberId = typeof member === "string" ? member : member.id;
  const displayName = typeof member === "string" ? "" : (member.displayName?.trim() ?? "");
  const identity = displayName
    ? `"${memberId}" (display name "${displayName}")`
    : `"${memberId}"`;
  return [
    `You are the InfiniteMission member ${identity} in workspace ${workspace}.`,
    `A mission brief follows below the line. Read it, do the work it asks for in this`,
    `workspace, then close your round by submitting — a round without a submit is a`,
    `failed round:`,
    ``,
    `  1. Re-read the mission at any time:`,
    `       im mission show <missionId> --for ${memberId}`,
    `  2. Mission documents (only if the brief lists them):`,
    `       im mission doc read ${memberId} <missionId> <path>`,
    `       im mission doc write ${memberId} <missionId> --id <docId> --file <path-or->`,
    `  3. Submitting IS the deliverable:`,
    `       im mission submit ${memberId} <missionId> --revision <N> --outcome <permitted> \\`,
    `         [--next-node <station>] [--reason <text>] [--feedback <text>] [--receipts <a,b>]`,
    `     Take --revision and the permitted outcomes from the brief. Attach document`,
    `     receipts from step 2 when the brief requires them.`,
    ``,
    `Never run "im join" or "im receive" — the bridge owns the member identity and`,
    `the listening loop. Run im commands from the workspace root (your current project).`,
    ``,
    `----- mission brief -----`,
  ].join("\n");
}

/**
 * Tail reminder appended AFTER the brief of every duty turn (first arrival
 * and follow-up rounds alike). Follows Hive's compaction-drift field notes:
 * a static head preamble is filtered out as banner noise after a few
 * occurrences, but an XML `<...-system-reminder>` envelope placed at the
 * tail — right before the agent's reply turn — rides recency weighting, and
 * a two-option action menu beats abstract identity restatement. The member
 * and mission ids are pre-bound (the bridge knows both); revision and
 * outcomes stay "from the brief above" because extracting them would mean
 * parsing human-readable show output — the machine-contract line we do not
 * cross again.
 */
export function dutyTailReminder(member, missionId) {
  const memberId = typeof member === "string" ? member : member.id;
  return [
    ``,
    `----- end of brief -----`,
    `<im-system-reminder>`,
    `You are InfiniteMission member "${memberId}", on duty for mission ${missionId}.`,
    `Reply by either: (a) \`im mission submit "${memberId}" "${missionId}" --revision <N> --outcome <permitted>\``,
    `when the work is done, or (b) \`im mission abandon "${memberId}" "${missionId}" --revision <N>\``,
    `when you cannot proceed. Take --revision and the permitted outcomes from the`,
    `brief above. Do the work yourself — nested CLI subagent tools bypass this duty`,
    `and their output will not reach the mission. Never run "im join" or "im receive".`,
    `</im-system-reminder>`,
  ].join("\n");
}

/**
 * Tail reminder for a returned Work-origin result: same recency anchoring,
 * but the action menu is read-only — report to the user, never submit.
 */
export function resultTailReminder(memberId, missionId) {
  return [
    ``,
    `----- end of result -----`,
    `<im-system-reminder>`,
    `You are InfiniteMission member "${memberId}". Mission ${missionId} is ENDED — the`,
    `brief above was a read-only result delivery. Reply by reporting the result to the`,
    `user in your own words. Any \`im mission submit/abandon/cancel\` for ${missionId}`,
    `will fail — do not run them.`,
    `</im-system-reminder>`,
  ].join("\n");
}

/**
 * Preamble for a returned Work-origin result: the Mission is already ended,
 * so there is no round to close — the deliverable is reading and relaying
 * the durable result. Deliberately contains no submit instruction; the brief
 * below the line carries the result JSON and the event history.
 */
export function resultPreamble(memberId, workspace) {
  return [
    `You are the InfiniteMission member "${memberId}" in workspace ${workspace}.`,
    `A mission you originated has ENDED and its result is addressed to you below the line.`,
    `This is a read-only delivery: do NOT run im mission submit, im mission abandon, or`,
    `im mission cancel for this mission — it is closed and every such command will fail.`,
    `Read the result and the event history in the brief, then report the outcome to the`,
    `user in your own words.`,
    ``,
    `Never run "im join" or "im receive" — the bridge owns the member identity and`,
    `the listening loop. Run im commands from the workspace root (your current project).`,
    ``,
    `----- mission result -----`,
  ].join("\n");
}

export class Delivery {
  constructor(t3) {
    this.t3 = t3;
  }

  #sameRoot(a, b) {
    const norm = (p) => {
      let target = p;
      try {
        target = fs.realpathSync(p);
      } catch {
        // keep the literal path when it does not resolve
      }
      return target.replace(/\/+$/, "");
    };
    return norm(a) === norm(b);
  }

  /**
   * The mission's most recently updated live thread beyond the three
   * deterministic ids, found by id prefix in the shell snapshot's thread
   * list. This is how a create-race fallback (random suffix) thread stays
   * findable — without it every round after its creation would fork yet
   * another one. The shell lists non-deleted threads only; archived ones are
   * legitimate followup targets (deliver unarchives). Residual blind spot:
   * an archived random-suffix thread is absent from the shell and unreachable.
   */
  #findByPrefix(shell, memberId, missionId) {
    const matches = (shell?.threads ?? [])
      .filter((thread) => isMissionThreadId(thread.id, memberId, missionId))
      .toSorted((a, b) => String(a.updatedAt ?? "").localeCompare(String(b.updatedAt ?? "")));
    return matches.at(-1) ?? null;
  }

  /** Reuse-or-create the T3 project for an im workspace (matched by real path). */
  async ensureProject(shell, workspacePath) {
    const existing = (shell.projects ?? []).find((project) =>
      this.#sameRoot(project.workspaceRoot, workspacePath),
    );
    if (existing) return existing.id;
    const root = fs.realpathSync(workspacePath);
    const projectId = randomUUID();
    await this.t3.dispatch({
      type: "project.create",
      commandId: randomUUID(),
      projectId,
      title: path.basename(root),
      workspaceRoot: root,
      createdAt: new Date().toISOString(),
    });
    return projectId;
  }

  /**
   * Does a live thread already exist for this member's round on this
   * mission? Ground truth the reconcile sweep uses for "the current round
   * already reached T3". A settled thread does not count: T3 auto-unsettles
   * on activity, so a revisit round injects right back into it.
   */
  async hasThread(memberId, missionId) {
    for (let attempt = 0; attempt < 3; attempt++) {
      const snapshot = await this.t3.threadDetail(threadIdFor(memberId, missionId, attempt));
      const thread = snapshot?.thread;
      if (thread && !thread.deletedAt && !thread.settledAt && thread.settledOverride !== "settled") {
        return true;
      }
    }
    // The deterministic probes can miss a random-suffix thread (deleted
    // bases squatted their ids); a live one still counts as delivered — a
    // settled one does not, or a revisit round could never find it.
    const reuse = this.#findByPrefix(await this.t3.shell(), memberId, missionId);
    return Boolean(reuse && !reuse.settledAt && reuse.settledOverride !== "settled");
  }

  /**
   * Resolve where a mission's thread lives. `create` means the id is free;
   * `followup` means the thread exists (the same mission returned for
   * another round — inject into it rather than forking). A settled thread is
   * still a valid followup target: the server un-settles it on the next turn.
   * Detail 404 covers both "never existed" and "deleted" — and a deleted
   * base is exactly when a create-race fallback thread (random suffix) may
   * live on, so every miss checks the shell before deciding to create.
   */
  async #resolveTarget(memberId, missionId, shell = null) {
    for (let attempt = 0; attempt < 3; attempt++) {
      const threadId = threadIdFor(memberId, missionId, attempt);
      const snapshot = await this.t3.threadDetail(threadId);
      if (snapshot?.thread) {
        const thread = snapshot.thread;
        if (thread.deletedAt) continue;
        return { mode: "followup", threadId, thread };
      }
      const reuse = this.#findByPrefix(shell ?? (await this.t3.shell()), memberId, missionId);
      if (reuse) return { mode: "followup", threadId: reuse.id, thread: reuse };
      return { mode: "create", threadId };
    }
    return { mode: "create", threadId: `im-${memberId}-${missionId}-${randomUUID().slice(0, 8)}` };
  }

  /**
   * One mission arrival → one T3 thread turn (creating the thread and its
   * project on first arrival). Returns the thread id used. `ended` marks a
   * returned Work-origin result: the brief is read-only, so the turn carries
   * the result preamble instead of the submit-duty one.
   */
  async deliver({ workspacePath, member, station, missionId, brief, ended = false }) {
    // One shell fetch serves both the project lookup and the prefix search
    // in #resolveTarget (the fallback path for deleted deterministic ids).
    const shell = await this.t3.shell();
    const projectId = await this.ensureProject(shell, workspacePath);
    const target = await this.#resolveTarget(member.id, missionId, shell);

    if (target.mode === "create") {
      await this.#createThread({ target, projectId, member, station, missionId, brief });
    } else if (target.thread.archivedAt) {
      await this.t3.dispatch({
        type: "thread.unarchive",
        commandId: randomUUID(),
        threadId: target.threadId,
      });
    }

    await this.t3.dispatch({
      type: "thread.turn.start",
      commandId: randomUUID(),
      threadId: target.threadId,
      message: {
        messageId: randomUUID(),
        role: "user",
        text: ended
          ? `${resultPreamble(member.id, workspacePath)}\n${brief}${resultTailReminder(member.id, missionId)}`
          : `${dutyPreamble(member, workspacePath)}\n${brief}${dutyTailReminder(member, missionId)}`,
        attachments: [],
      },
      // Each turn restates the member's selection, so the runtime identity
      // is carried by the turn itself, not just the thread's creation.
      modelSelection: modelSelectionOf(member),
      runtimeMode: member.runtimeMode,
      interactionMode: "default",
      createdAt: new Date().toISOString(),
    });
    return target.threadId;
  }

  async #createThread({ target, projectId, member, station, missionId, brief }) {
    const make = (threadId) => ({
      type: "thread.create",
      commandId: randomUUID(),
      threadId,
      projectId,
      title: titleFromBrief(brief, member, station, missionId),
      modelSelection: modelSelectionOf(member),
      runtimeMode: member.runtimeMode,
      interactionMode: "default",
      branch: null,
      worktreePath: null,
      createdAt: new Date().toISOString(),
    });
    try {
      await this.t3.dispatch(make(target.threadId));
    } catch (err) {
      // The probe said free but creation lost a race (or the id stream was
      // reused) — retry once on an unguessable suffix instead of dropping.
      const fallback = `im-${member.id}-${missionId}-${randomUUID().slice(0, 8)}`;
      await this.t3.dispatch(make(fallback));
      target.threadId = fallback;
      void err;
    }
  }

  /** Mark the member's thread for the mission settled. Best-effort. */
  async settle(memberId, missionId) {
    const target = await this.#resolveTarget(memberId, missionId);
    if (target.mode !== "followup") return false;
    const { thread } = target;
    if (thread.settledAt || thread.settledOverride === "settled") return false;
    await this.t3.dispatch({
      type: "thread.settle",
      commandId: randomUUID(),
      threadId: target.threadId,
    });
    return true;
  }
}
