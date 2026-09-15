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

/** `[mission ms_x] NAME — status` (legacy) or `# NAME — status` (markdown)
 * → a T3 display title (missionId as fallback). The header may sit a few
 * lines down (result briefs open with a marker), so the first lines are
 * scanned but the match stays line-anchored. Shows the member's display
 * name when set — the id in the title is meaningless noise. */
export function titleFromBrief(brief, member, station, missionId) {
  const memberId = typeof member === "string" ? member : member.id;
  const who = typeof member === "string" ? member : (member.displayName?.trim() || member.id);
  const lines = String(brief).split("\n");
  const found = lines
    .slice(0, 5)
    .map((line) => line.match(/^# (.*)$/) ?? line.match(/^\[mission ms_[0-9a-f]{6,64}\] (.*)$/))
    .find(Boolean);
  const name = found ? found[1].split(" — ")[0].trim() : "";
  return `im/${who}@${station}: ${name || missionId}`;
}

export function modelSelectionOf(member) {
  const selection = { instanceId: member.instance, model: member.model };
  if (member.options !== undefined) selection.options = member.options;
  return selection;
}

/** Render IM core's structured assignment snapshot for an agent consumer.
 * This is presentation only: all mission semantics and adjudication remain
 * in the snapshot/core. Markdown: the delivered turn is read by LLM agents,
 * where semantic lines (bold field labels, backticked ids/commands, one
 * bullet per outcome/document) beat terminal indented fields and 78-col
 * wrapping — and outcomes with their routes are one list, not two.
 *
 * Split per the lifecycle delivery design: `roundBriefFromView` is the
 * per-round block (every-round facts), `freshContextFromView` carries the
 * stable identity facts (mission meta minus revision, objective) that ride
 * only turns starting a history-less provider thread, and
 * `briefFromRunView` keeps rendering the everything-once full form. */
function missionBodyLines(view, { objective }) {
  const lines = [];
  const arrival = view.arrival ?? null;
  if (view.at) {
    const station = [
      `**At station: ${view.at}**（iteration ${view.iteration ?? 1}）`,
      `revision ${view.revision}`,
      view.onDuty ? "**on duty — you hold this station**" : "not on duty",
    ];
    if (arrival?.kind === "route") {
      station.push(
        `arrived from ${arrival.from ?? "?"} on \`${arrival.outcome ?? "?"}\` (#${arrival.eventSeq})`,
      );
    }
    lines.push(station.join(" · "));
    if (arrival && arrival.kind !== "route") {
      if (arrival.kind === "child-result") {
        lines.push(
          `**Child result** — \`${arrival.childMissionId ?? "?"}\` returned \`${arrival.outcome ?? "?"}\` (#${arrival.eventSeq})`,
        );
      } else {
        lines.push(`**Arrival** — from ${arrival.from ?? "?"} (#${arrival.eventSeq})`);
      }
    }
  } else {
    lines.push("**Ended** — mission is no longer in the mail stream");
  }
  if (arrival?.feedback) lines.push(`**Incoming feedback** — ${arrival.feedback}`);
  else if (arrival?.reason) lines.push(`**Incoming reason** — ${arrival.reason}`);
  if (objective && view.objective) lines.push(`**Objective** — ${view.objective}`);
  if (view.currentStep) lines.push(`**Current step** — ${view.currentStep}`);
  if (view.stationCharterSha256 && view.at) {
    lines.push(
      `**Station charter** \`sha256:${shortHash(view.stationCharterSha256)}\` — read: \`im work show ${view.at}\``,
    );
  }

  const outcomes = Array.isArray(view.outcomes) ? [...view.outcomes] : [];
  if (!outcomes.includes("abandon")) outcomes.push("abandon");
  const routeByOutcome = new Map(
    (Array.isArray(view.routes) ? view.routes : []).map((route) => [route.outcome, route]),
  );
  const required = (outcome) => {
    const flags = [];
    if (view.resultRequiredOn?.includes(outcome)) flags.push("`--result`");
    if (view.feedbackRequiredOn?.includes(outcome)) flags.push("`--feedback`");
    return flags.length > 0 ? ` — needs ${flags.join(" & ")}` : "";
  };
  const routed = [...outcomes, ...[...routeByOutcome.keys()].filter((o) => !outcomes.includes(o))];
  if (routed.length > 0) {
    lines.push("**Outcomes → routes**");
    for (const outcome of routed) {
      const route = routeByOutcome.get(outcome);
      if (route?.to?.length) lines.push(`- \`${outcome}\` → ${route.to.join(" | ")}${required(outcome)}`);
      else if (route?.terminal) lines.push(`- \`${outcome}\` — terminal${required(outcome)}`);
      else if (route) lines.push(`- \`${outcome}\` → (none)${required(outcome)}`);
      else lines.push(`- \`${outcome}\`${required(outcome)}`);
    }
  }

  if (Array.isArray(view.children) && view.children.length > 0) {
    lines.push("**Child missions**");
    for (const child of view.children) {
      const detail = child.result ? `result: ${child.result}` : child.reason ? `reason: ${child.reason}` : "";
      lines.push(
        `- \`${child.missionId}\` — ${child.status}${child.outcome ? ` outcome=\`${child.outcome}\`` : ""}${detail ? ` · ${detail}` : ""}`,
      );
    }
  }

  if (Array.isArray(view.documents) && view.documents.length > 0) {
    lines.push("**Documents**");
    for (const document of view.documents) {
      const parts = [
        `\`${document.id}\``,
        document.kind && document.kind !== "file" ? `${document.path} (${document.kind})` : document.path,
      ];
      if (document.receipt) parts.push(`\`${receiptDisplay(document.receipt)}\``);
      parts.push(
        document.mayRead && document.mayWrite
          ? "read-write"
          : document.mayRead
            ? "read-only"
            : document.mayWrite
              ? "write-only"
              : "no access",
      );
      if (document.sourceMissionId) parts.push(`inherited from \`${document.sourceMissionId}\``);
      lines.push(`- ${parts.join(" · ")}`);
    }
  }
  return lines;
}

function metaLine(view) {
  const meta = [`\`${view.missionId}\``];
  if (view.originWork) meta.push(`origin work: ${view.originWork}`);
  if (Array.isArray(view.memberStations) && view.memberStations.length > 0) {
    meta.push(`member stations: ${view.memberStations.join(", ")}`);
  }
  if (view.parent) {
    meta.push(
      `parent \`${view.parent.missionId}\` (round revision ${view.parent.revision}, requested by ${view.parent.requestedByWork})`,
    );
  }
  return meta.join(" · ");
}

/** The everything-once brief: what a history-less provider thread receives
 * (as text) when lifecycle delivery is disabled, and the reference shape the
 * round + freshContext split decomposes. Revision rides the station line. */
export function briefFromRunView(view) {
  return [`# ${view.name} — ${view.status}`, metaLine(view), ...missionBodyLines(view, { objective: true })].join("\n");
}

/** The per-round block: every-round facts only. Stable identity facts
 * (mission meta, objective) travel in freshContextFromView instead. */
export function roundBriefFromView(view) {
  return [`# ${view.name} — ${view.status}`, ...missionBodyLines(view, { objective: false })].join("\n");
}

/** Stable context prepended (by T3's reactor) only when the turn starts a
 * provider thread with no conversation history. Empty string when the view
 * has neither meta facts beyond the mission id nor an objective. */
export function freshContextFromView(view) {
  const lines = [];
  const meta = metaLine(view);
  if (meta !== `\`${view.missionId}\``) lines.push(meta);
  if (view.objective) lines.push(`**Objective** — ${view.objective}`);
  return lines.join("\n");
}

/** Fingerprints shown in the delivered brief carry only a comparable prefix:
 * the working agent never feeds them back (submit takes this station's own
 * doc-write receipts, whose full value is that command's stdout; the charter
 * hash resolves via `im work show`), and the stored full value is always
 * re-readable through the im CLI. */
export function shortHash(hash) {
  return hash.length > 12 ? `${hash.slice(0, 12)}…` : hash;
}

/** Render a document receipt for the delivered brief (see shortHash). */
export function receiptDisplay(receipt) {
  const colon = receipt.indexOf(":");
  if (colon < 0) return shortHash(receipt);
  return `${receipt.slice(0, colon + 1)}${shortHash(receipt.slice(colon + 1))}`;
}

/**
 * Split `im mission events` stdout into `#seq <stamp> <kind>` blocks, each
 * with its (pretty-printed, indented) JSON payload. Malformed blocks are
 * skipped — the header is additive context, never a gate.
 */
function eventBlocks(eventsText) {
  const blocks = [];
  const lines = String(eventsText).split("\n");
  let current = null;
  for (const line of lines) {
    if (/^#\d+\s/.test(line)) {
      if (current) blocks.push(current);
      current = { head: line, json: "" };
    } else if (current !== null) {
      current.json += line;
    }
  }
  if (current) blocks.push(current);
  return blocks
    .map((block) => {
      const start = block.json.indexOf("{");
      const end = block.json.lastIndexOf("}");
      if (start < 0 || end <= start) return null;
      try {
        return { kind: block.head.trim(), payload: JSON.parse(block.json.slice(start, end + 1)) };
      } catch {
        return null;
      }
    })
    .filter(Boolean);
}

/**
 * The arrival header: why the mission is at this station THIS time. Built
 * from the hop facts on the arrival note (from / outcome) plus the mission's
 * own event ledger (how many times it has routed here; the latest
 * round-against-this-station feedback/reason, verbatim). Returns "" when
 * neither source yields a fact (first park, sweep redelivery of a result) —
 * an empty header is better than a guessed one.
 */

export function arrivalHeader({ station, hop, eventsText }) {
  const lines = [];
  const blocks = eventsText ? eventBlocks(eventsText) : [];
  const hopsHere = blocks.filter(
    (block) => block.payload && block.payload.to === station,
  ).length;
  if (hop && hop.from && hop.outcome) {
    lines.push(
      `Arrival: from '${hop.from}' on outcome '${hop.outcome}'` +
        (hopsHere > 0 ? ` — ${hopsHere} time${hopsHere === 1 ? "" : "s"} routed to this station` : ""),
    );
  } else if (hopsHere > 0) {
    lines.push(`Arrival: ${hopsHere} time${hopsHere === 1 ? "" : "s"} routed to this station`);
  }
  // The latest round input addressed to this station's work: feedback from a
  // rejection first (it is the checklist to work through), else the latest
  // round's reason. Events are ordered, so the last match wins.
  let feedback = null;
  let reason = null;
  for (const block of blocks) {
    if (!block.kind.includes("mission.round.completed")) continue;
    const text = typeof block.payload.feedback === "string" && block.payload.feedback.trim()
      ? block.payload.feedback
      : null;
    if (text) feedback = text;
    if (typeof block.payload.reason === "string" && block.payload.reason.trim()) {
      reason = block.payload.reason;
    }
  }
  const input = feedback ?? reason;
  if (input) {
    lines.push(
      feedback ? "Latest round input for you (feedback, verbatim):" : "Latest round input for you (reason, verbatim):",
    );
    for (const line of input.split("\n")) lines.push(`  ${line}`);
  }
  if (lines.length === 0) return "";
  return ["=== ARRIVAL CONTEXT ===", ...lines, "=== END ARRIVAL CONTEXT ===", ""].join("\n");
}

/**
 * Duty discipline travels as the message preamble (not a system prompt) —
 * the same contract the DSH bridge proved: the agent itself submits; the
 * bridge never submits on its behalf. The head is orientation-only:
 * identity, the round-closing duty in one line, and a single pre-bound
 * re-read pointer. Everything actionable — the submit/abandon commands,
 * the permitted-outcomes rule, the child-Mission yield exception, the
 * join/receive ban — lives in the tail reminder that rides every turn on
 * recency; saying it here too as well was banner noise.
 */
export function dutyPreamble(member, missionId) {
  // Accepts the member object (preferred — carries the display name) or a
  // bare id (tests, legacy callers). missionId is pre-bound so the pointer
  // stays copy-pasteable — placeholders invite placeholder submissions. No
  // workspace: the T3 session already runs there (its cwd carries it).
  const memberId = typeof member === "string" ? member : member.id;
  const displayName = typeof member === "string" ? "" : (member.displayName?.trim() ?? "");
  const identity = displayName ? `**${memberId}**（${displayName}）` : `**${memberId}**`;
  return [
    `You are the InfiniteMission member ${identity}. A mission brief follows — do the work it asks for, then close your round; the reminder after the brief owns the rules for replying.`,
    ``,
    `Re-read: \`im mission show ${missionId} --for ${memberId}\` · Documents & full flags: \`im help\``,
    ``,
    `---`,
  ].join("\n");
}

/**
 * Tail reminder appended AFTER the brief of every duty turn (first arrival
 * and follow-up rounds alike). Follows Hive's compaction-drift field notes:
 * a static head preamble is filtered out as banner noise after a few
 * occurrences, but an XML `<...-system-reminder>` envelope placed at the
 * tail — right before the agent's reply turn — rides recency weighting, and
 * a two-option action menu beats abstract identity restatement. Identity
 * stays in the same message's preamble (duty or slim — both open with it),
 * so the tail spends its recency budget purely on the menu. The member and
 * mission ids are pre-bound (the bridge knows both); revision and outcomes
 * stay "from the brief above" because extracting them would mean parsing
 * human-readable show output — the machine-contract line we do not cross
 * again.
 */
export function dutyTailReminder(member, missionId) {
  const memberId = typeof member === "string" ? member : member.id;
  return [
    ``,
    `---`,
    `<im-system-reminder>`,
    `Close the round: \`im mission submit "${memberId}" "${missionId}" --outcome <permitted>\` — permitted outcomes from the brief above. If you cannot proceed: \`im mission abandon "${memberId}" "${missionId}"\`. Active linked child Missions: yield without submitting; IM returns their results to this Mission. Do the work yourself — nested CLI subagent output will not reach the mission. Never run \`im join\` / \`im receive\`.`,
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
 * below the line carries the durable result JSON.
 */
export function resultPreamble(memberId) {
  return [
    `You are the InfiniteMission member \`${memberId}\`.`,
    `A mission you originated has ENDED and its result is addressed to you below the line.`,
    `This is a read-only delivery: do NOT run im mission submit, im mission abandon, or`,
    `im mission cancel for this mission — it is closed and every such command will fail.`,
    `Read the result in the brief, then report the outcome to the`,
    `user in your own words.`,
    ``,
    `Never run "im join" or "im receive" — the bridge owns the member identity and`,
    `the listening loop.`,
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
    return Boolean(await this.liveThread(memberId, missionId));
  }

  /**
   * The live (undeleted, unsettled) thread carrying this member's current
   * round on the mission — what "already delivered" means to the sweep —
   * returned with its snapshot (turn state included) so callers can
   * reconcile a thread whose watcher was lost to a bridge restart.
   */
  async liveThread(memberId, missionId) {
    for (let attempt = 0; attempt < 3; attempt++) {
      const snapshot = await this.t3.threadDetail(threadIdFor(memberId, missionId, attempt));
      const thread = snapshot?.thread;
      if (thread && !thread.deletedAt && !thread.settledAt && thread.settledOverride !== "settled") {
        return thread;
      }
    }
    // The deterministic probes can miss a random-suffix thread (deleted
    // bases squatted their ids); a live one still counts as delivered — a
    // settled one does not, or a revisit round could never find it.
    const reuse = this.#findByPrefix(await this.t3.shell(), memberId, missionId);
    if (reuse && !reuse.settledAt && reuse.settledOverride !== "settled") return reuse;
    return null;
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
   * the result preamble instead of the submit-duty one. `freshContext` (the
   * lifecycle-delivery stable block) rides message.context: T3's provider
   * command reactor prepends it only when the turn starts a provider thread
   * with no conversation history — the bridge never guesses session state.
   */
  async deliver({
    workspacePath,
    member,
    station,
    missionId,
    brief,
    freshContext = null,
    ended = false,
  }) {
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

    const memberId = typeof member === "string" ? member : member.id;
    await this.t3.dispatch({
      type: "thread.turn.start",
      commandId: randomUUID(),
      threadId: target.threadId,
      message: {
        messageId: randomUUID(),
        role: "user",
        text: ended
          ? `${resultPreamble(memberId)}\n${brief}${resultTailReminder(memberId, missionId)}`
          : `${dutyPreamble(member, missionId)}\n${brief}${dutyTailReminder(memberId, missionId)}`,
        ...(freshContext && !ended
          ? { context: { version: 1, records: [], freshContext } }
          : {}),
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
