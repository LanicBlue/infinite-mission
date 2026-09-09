# infinite-mission-t3

t3-im-bridge — an [InfiniteMission](../../../) plugin that runs im missions as
[T3 Code](https://github.com/pingdotgg/t3code) threads.

**Member table lives in the T3 fork** (branch `im-integration` in
~/projects/t3code-expanded): settings.json `imBridge.members`, edited in
T3's own Settings → IM Bridge page with live instance/model catalogs. The
bridge re-reads it on every reconcile tick (≤60 s). When that key is absent
the bridge config file's `members` list applies (migration fallback). Wire im
stations to members the normal way (`im work set-executor <op> build t3-codex`).

The bridge's own config file (`~/.im/t3-bridge.json`) keeps infrastructure
fields only: imBin, t3 bin/home, timeouts, workspaces, and the admin/status
server. Its web page is **read-only status** (members, loops, watcher, log
tail) — T3 settings is the single writer for members.

```
im CLI children ◄──── t3-im-bridge (this process) ────► T3 HTTP API :3773
receive --wait loops    duty: the T3 agent itself runs     project/thread/turn
per workspace × member  `im mission submit` in-thread      via /api/orchestration/*
```

The design copies the proven DSH bridge recipe
(`deepseek-harness packages/integrations/dsh-im-bridge`): guard-join against
id auto-suffixing, a closed-set note parser (unknown lines are dropped, never
guessed), arrival notes are only triggers — the mission is re-read via
`im mission show --for` before acting — and **the bridge never submits**;
submitting is the agent's deliverable. The PS→T3 integration lesson is baked
in negatively: no T3 fork, no vendored SDK, no network calls on the agent's
tool path (im is a local CLI over SQLite).

## What it does

- **Per (workspace × member) receive loop** — one `im receive --wait` child,
  re-hung after every cycle (delivery and timeout both exit 0).
- **Arrival → thread turn** — ensures a T3 project exists for the im
  workspace (matched by real path), creates the thread
  `im-<memberId>-<missionId>` on first arrival (one thread per member per
  mission), and starts a user turn whose text is the duty preamble followed
  by the verbatim `mission show` brief. The thread's model/instance (and each
  turn's `modelSelection`) is the delivering member's, so a mission routed
  across members never runs on the first member's runtime. A later round of
  the same mission at the same member injects into that member's thread
  (unarchiving if needed; T3 auto-unsettles a settled thread on the new
  turn).
- **Thread settling, two paths** —
  1. *Ended notice*: im tells past round resolvers when someone else ends a
     mission (manage delete, another participant). The bridge settles that
     mission's thread on the note. Note: im deliberately skips the member
     whose own submit ended the mission.
  2. *Turn watcher*: because of that skip, the bridge also watches each
     delivered thread for one turn-terminal transition (completed / error /
     interrupted), then checks `im missions <member>` once — if the mission
     has left the list (the agent's submit closed it), the thread settles;
     if the mission is still open, the agent skipped its duty, and the
     thread stays active for a follow-up round (logged). Threads are never
     deleted. Watch state is in-memory: threads that finish while the
     bridge is down settle on their next ended note or stay
     completed-but-unsettled (cosmetic).
- **Membership end → loop stops** — an archived/removed member is muted for
  the process lifetime; restart the bridge after re-joining.
- **T3 auth** — a bearer token issued offline via `t3 auth session issue`
  (label `im-t3-bridge`, 30-day TTL, auto re-issue on 401/expiry). Revoke
  with `t3 auth session list` / `revoke`.
- **Config hot reload** — the config file is re-read on every reconcile
  (default every 60 s); an invalid file keeps the last good config. Setting a
  member's `enabled: false` stops its loop at the next tick.
- **Delivery retries** — a failed delivery (e.g. T3 down) is retried on each
  reconcile with the mission re-verified, up to `maxDeliverAttempts`; after
  that the mission re-notifies on its next move (exactly-once note semantics).

## Config UI

The bridge serves a member-table admin page on `admin.listen` (default
`127.0.0.1:4770`, loopback-only): edit/add/disable members (instance dropdown
read from the T3 server's settings, model suggestions extracted from the t3
bundle's embedded manifest), watch live loops / watcher / retry state, and
read the log tail. Saving validates, writes the config file, and triggers an
immediate reconcile instead of waiting for the rescan tick.

Routes are prefix-agnostic (`/api/config`, `/api/status`, `/api/catalog`),
so the page works locally at `/` and behind a reverse-proxy path prefix.
Remote exposure on this deployment: frp tunnel (Mac 4770 → ECS 3780) +
caddy route `/t3bridge/*` on the gated launcher site — the card "IM · T3 桥"
on the launcher links to it.

## Setup

1. **Config** — copy `config.example.json` to `~/.im/t3-bridge.json` (or pass
   `--config <path>`) and edit:

   | Key | Meaning |
   | --- | --- |
   | `imBin` | `im` binary; use an absolute path under launchd |
   | `t3.bin` | T3 CLI as a command string or array (e.g. `["node", "…/t3/dist/bin.mjs"]`) |
   | `t3.home` | T3 data dir (default `~/.t3`) — used to find `userdata/server-runtime.json` for origin discovery; `t3.origin` overrides |
   | `workspaces` | `null` = follow the live entries of `im workspaces`; or an explicit array of absolute paths |
   | `members[]` | `{ id, instance, model, options?, runtimeMode?, enabled? }` — `instance` is a T3 provider instance id (e.g. `codex`, `claudeAgent`), `model` a slug from T3's model picker, `runtimeMode` one of `approval-required / auto-accept-edits / auto / full-access` |

2. **im side** — in each workspace, bind stations to members:
   `im work set-executor <op> build t3-codex`. Grant publish tier where the
   pipeline needs it (`im grant`).

3. **Run** — foreground: `node bridge.mjs`. As a service (macOS):

   ```xml
   <?xml version="1.0" encoding="UTF-8"?>
   <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
   <plist version="1.0"><dict>
     <key>Label</key><string>com.lanic.im.t3bridge</string>
     <key>ProgramArguments</key><array>
       <string>/opt/homebrew/bin/node</string>
       <string>/Users/lanic/projects/infinite-mission/plugins/t3/infinite-mission-t3/bridge.mjs</string>
     </array>
     <key>RunAtLoad</key><true/>
     <key>KeepAlive</key><true/>
     <key>StandardOutPath</key><string>/Users/lanic/.im/t3-bridge.log</string>
     <key>StandardErrorPath</key><string>/Users/lanic/.im/t3-bridge.log</string>
   </dict></plist>
   ```

## Semantics & boundaries

- **Single consumer lock** — `im receive --wait` takes a per-member flock.
  Nothing else may wait on the same member id (the duty preamble forbids the
  agent from running `im receive`/`im join`).
- **Revision CAS** — submits must carry the revision from the brief;
  a follow-up round invalidates the old revision. The agent reads it from
  `mission show`, never from memory.
- **Archived members are respected** — if a member id exists but is archived,
  the bridge refuses to rejoin it (a human archived it). Rejoin manually or
  remove it from the config.
- **runtimeMode** — `full-access` (default) runs missions unattended. Pick a
  stricter mode per member if the workspace deserves it.
- **Reconcile sweep** — every reconcile tick re-scans active missions at
  enabled members' stations and re-delivers any that has no live T3 thread
  (deterministic `im-<memberId>-<missionId>` ids are the ground truth, so follow-up
  rounds into existing threads are never re-fired). A delivery that exhausts
  its retry budget is parked in the queue rather than dropped, so a T3
  outage longer than the retry window no longer strands the mission —
  recovery is automatic once T3 is reachable again.
- **Member config is read at delivery time** — loops, the retry queue, and
  watchers carry member ids only; every delivery resolves the member's
  instance/model from the current table, so edits in T3's settings apply to
  the next arrival without a bridge restart. Members that left the table
  (or were disabled) skip delivery until they return.
- **Thread reuse beyond the deterministic ids** — when the deterministic ids
  miss (a deleted base squats its id), the shell snapshot's thread list is
  searched by `im-<memberId>-<missionId>` prefix, so a create-race fallback
  thread (random suffix) is reused instead of forking a new one every round.
  Only an archived random-suffix thread is out of reach.
- **One delivery at a time** — an in-flight set dedupes concurrent triggers
  for the same mission (receive-loop arrival × sweep × retry), and a
  reconcile cycle still running skips the next tick instead of overlapping.

## Tests

```sh
node --test "tests/"*.test.mjs
```

Covers the note parser (closed shapes + garbage), config validation,
delivery decision paths (create / follow-up / unarchive / create-race
fallback / settle), and bridge lifecycle (guard-join, suffix landing,
archived respect, membership-end mute, retry healing + parking, reconcile
sweep, config toggle).
