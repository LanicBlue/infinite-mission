import fs from "node:fs";
import path from "node:path";
import { randomUUID } from "node:crypto";

// Deterministic thread ids make delivery idempotent-ish across bridge
// restarts: the thread for a mission is found by probing `im-<missionId>`
// (then suffixed variants) instead of remembering state.
export function threadIdFor(missionId, attempt = 0) {
  return attempt > 0 ? `im-${missionId}-${attempt + 1}` : `im-${missionId}`;
}

/** `[mission ms_x] NAME — status` → a T3 display title (missionId as fallback). */
export function titleFromBrief(brief, station, missionId) {
  const firstLine = String(brief).split("\n", 1)[0] ?? "";
  const header = firstLine.match(/^\[mission ms_[0-9a-f]{6,64}\] (.*)$/);
  const name = header ? header[1].split(" — ")[0].trim() : "";
  return `im/${station}: ${name || missionId}`;
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
export function dutyPreamble(memberId, workspace) {
  return [
    `You are the InfiniteMission member "${memberId}" in workspace ${workspace}.`,
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

  /** Reuse-or-create the T3 project for an im workspace (matched by real path). */
  async ensureProject(workspacePath) {
    const shell = await this.t3.shell();
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
   * Does a live thread already exist for this mission? Ground truth the
   * reconcile sweep uses for "a round already reached T3" — deterministic
   * thread ids keep the probe stateless across bridge restarts.
   */
  async hasThread(missionId) {
    for (let attempt = 0; attempt < 3; attempt++) {
      const snapshot = await this.t3.threadDetail(threadIdFor(missionId, attempt));
      const thread = snapshot?.thread;
      if (thread && !thread.deletedAt) return true;
    }
    return false;
  }

  /**
   * Resolve where a mission's thread lives. `create` means the base id is
   * free; `followup` means the thread exists (the same mission returned for
   * another round — inject into it rather than forking).
   */
  async #resolveTarget(missionId) {
    for (let attempt = 0; attempt < 3; attempt++) {
      const threadId = threadIdFor(missionId, attempt);
      const snapshot = await this.t3.threadDetail(threadId);
      if (!snapshot?.thread) return { mode: "create", threadId };
      const thread = snapshot.thread;
      if (thread.deletedAt) continue;
      return { mode: "followup", threadId, thread };
    }
    return { mode: "create", threadId: `im-${missionId}-${randomUUID().slice(0, 8)}` };
  }

  /**
   * One mission arrival → one T3 thread turn (creating the thread and its
   * project on first arrival). Returns the thread id used.
   */
  async deliver({ workspacePath, member, station, missionId, brief }) {
    const projectId = await this.ensureProject(workspacePath);
    const target = await this.#resolveTarget(missionId);

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
        text: `${dutyPreamble(member.id, workspacePath)}\n${brief}`,
        attachments: [],
      },
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
      title: titleFromBrief(brief, station, missionId),
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
      const fallback = `im-${missionId}-${randomUUID().slice(0, 8)}`;
      await this.t3.dispatch(make(fallback));
      target.threadId = fallback;
      void err;
    }
  }

  /** Mark the mission's thread settled when the mission ends. Best-effort. */
  async settle(missionId) {
    const target = await this.#resolveTarget(missionId);
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
