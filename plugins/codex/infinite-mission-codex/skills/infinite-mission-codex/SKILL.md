---
name: infinite-mission-codex
description: Handle InfiniteMission work in Codex Desktop. Use when an IM watcher wakeup invokes this skill, when the user asks this Codex task to process an IM assignment, or when an IM agent should wait for more work. Do not use for human console administration.
---

# InfiniteMission for Codex

Use the existing `im` CLI as the authority for mission state. This plugin adapts
Codex Desktop lifecycle only; it does not replace or modify InfiniteMission.

## On wakeup

1. Treat a queued message from `source=im-codex-watcher` as a wake event, not
   as proof that an assignment is still current. Use its exact `agentId` and
   `workspace` only after they match the local watcher binding.
2. Work from the supplied workspace. Run `im missions <agentId>` and inspect
   each actionable mission with `im mission show <missionId> --for <agentId>`.
   The live mission view, revision, rights, routes, and station prompt are the
   authority. Duplicate or stale wakeups may have no actionable mission.
3. Read and write only the mission documents granted by that live view. Submit
   with the current revision, an allowed outcome, required feedback or reason,
   and every receipt produced by document writes.
4. Do not edit `.im/` directly or infer authority from cached queue text.

The wake envelope uses `protocol=im-codex-wake/v1`. `waitCommand` identifies
the represented IM wait, `waitResult=ended` means that wait returned a real
event, and `reason` is `arrival`, `membership`, or `receive-error`. Normal
six-hour timeouts never produce an envelope because the watcher renews them
internally. `nextWait=auto-renewing` means the watcher is already entering the
next wait; `nextWait=paused` means it stopped for a terminal condition.

If the wakeup reports `reason=receive-error`, inspect the referenced
`noticePath`, diagnose the local CLI/session problem, and report it instead of
silently re-arming.

## Waiting for more work

After the current work is complete, or when the live query shows no actionable
mission, arm the Codex watcher with the script at
`../../scripts/im_codex.py`, resolved relative to this `SKILL.md`:

```bash
python3 <plugin-root>/scripts/im_codex.py wait <agentId>
```

The command captures the current `CODEX_THREAD_ID`, starts or updates one
background watcher for this workspace and agent, and returns immediately. Once
it reports that the watcher is armed, end the Codex turn. Do not poll the
background process and do not run a foreground `im receive --wait`.

The watcher runs the real `im receive <agentId> --wait`, persists its output,
and uses `codex queue` to invoke this skill in the bound Codex task. A normal
six-hour receive timeout is re-armed internally without waking Codex. After a
real notification is delivered, the watcher immediately starts the next wait;
the Codex task does not need to re-arm it after every turn. A receive failure or
terminal membership event is delivered once and pauses the watcher to prevent
a retry storm; re-arm only after fixing that terminal condition.

Use these diagnostic commands only when needed:

```bash
python3 <plugin-root>/scripts/im_codex.py status <agentId>
python3 <plugin-root>/scripts/im_codex.py stop <agentId>
```

Never start multiple waiters for the same workspace and agent. The helper
updates the target task of an existing watcher rather than competing for IM's
exclusive receive lock.
