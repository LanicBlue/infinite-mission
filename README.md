# InfiniteMission

**`im` — multi-agent mission orchestration in a static SQLite core.**

No daemon, no server, no background process. Every command is a one-shot CLI
process over a WAL-mode SQLite file in your workspace. The runtime semantics
are the work-mission model: a mission is one-shot mail that carries its own
contract, travels between stations, and records its own delivery history.

```
./install.sh                 # cargo build --release → ~/.local/bin/im
cargo install --path .       # alternative (installs into ~/.cargo/bin)
```

## The model in one screen

- **Workspace** = a directory tree with `.im/` at the root (found by walking
  up, like git). The workspace is the single project: everything lives in
  `.im/im.db` + `.im/mission-documents/`.
- **Agents** self-register with `im join <id>`. A taken id
  auto-suffixes (`alice` → `alice-2`) — identities are never impersonated.
  Agents are stateless and persona-free: all durable state lives in missions,
  and work content arrives with each mission's station prompt.
- **Leaving hands the stations back to the user.** `im leave <id>` archives the
  member and releases every station they held to the user (executor = NULL —
  missions stay parked; the station's prompt, docs and notes never leave the
  station). Reassignment is a deliberate manage-tier act.
- **Member tiers** are a linear ladder `execute ⊂ publish ⊂ manage` on
  `agents.tier`:
  | tier | may … |
  |---|---|
  | execute | work missions at bound stations |
  | publish | + create missions (`im mission create`) |
  | manage | + station ops (`work create/set-* /delete`), resolve/abort
    missions at user stations, user-station documents, `mission end` |
  Manage is **console-only**: set it on the Members page in `im ui` (works
  with zero manage members — the console *is* the human). Publish is granted
  from the CLI by a manage-tier operator (`im grant <op> <target>`), or in
  the console. No CLI verb may set manage or touch a manage-tier member —
  grant, revoke and member delete all refuse manage-tier targets.
- **The workspace IS the project**: one directory tree, one mission space.
  **Stations** (`build`, `review`, `approve`, …) hang directly off the
  workspace. A station has an executor binding (rebindable at any time,
  last-write-wins) — or **no executor**, which makes it a *user station*,
  the human's inbox. Mission ids are namespaced by a per-workspace uuid
  generated at init.
- **Missions** are created from YAML templates. The template compiles into
  an immutable contract: entry station, per-station outcome vocabularies,
  routing paths (`from`/`when`/`to`, optional `iterationPolicy`), document
  declarations, and per-station document rights (read ≠ write).
- **Ownership is the station's, not the agent's.** A mission sits *at* a
  station; only that station's on-duty executor may submit. Rebinding the
  station moves the pointer — the mission, its history and documents stay.
- **Members have no peer channel.** There is no `im send`. Agents are not
  connected to each other; the only mail is a mission sitting at a station.
  `im receive` delivers station arrival notes and membership notices
  (grant / revoke / removed). Feedback travels in the round (`--feedback`,
  `--reason`, documents), not as chat.
- **Submitting** (`im mission submit`) runs full adjudication in one
  transaction: ended → revision CAS → attribution → vocabulary →
  feedback-required → receipts → routing (0/1/2+ edges) → user-station hop
  needs a reason → `round.completed` + `routed` events, revision +1.
  `abandon` is always legal and always terminal.
- **Documents** are content-addressed: same bytes → same receipt, retries
  are free. Receipts submitted with a round prove what the round produced.
- **Events are the history.** `created`, `round.completed`, `routed`,
  `ended` — iteration counts are derived, never stored.
- **Inbox**: missions parked at user stations, with the reason the sender
  had to give. A manage-tier member resolves them.

## Quick start

```bash
mkdir my-project && cd my-project
im init                       # workspace + example/pipeline templates +
                              # the pipeline stations (design/plan/build/review)

im join boss                  # in each agent's terminal — no persona needed
im join worker
im ui                         # set boss → manage on the Members page
                              # (manage is console-only)

# as boss — first thing: bind executors to the seeded stations
# (an unbound station is a user station: every hop there waits for a
#  manage-tier member)
im work set-executor boss design <agent>
im work set-executor boss plan <agent>
im work set-executor boss build worker
im work set-executor boss review <agent>

# a pipeline mission — grill→spec happens in the design agent's session
# conversation with you; when it settles, the design agent starts delivery:
im mission create <design-agent> --template pipeline --key v1 \
    --objective "ship the widget"          # the distilled intent

# as the design agent (first round: park the frozen spec):
im receive <design-agent>          # arrival note
im mission show <ms> --for <design-agent>   # interpolated charter, rights, vocabulary, revision
im mission doc write <design-agent> <ms> --id spec --file -   # → receipt
im mission submit <design-agent> <ms> --revision 1 --outcome spec-ready \
    --receipts document:<hash>

# as worker (at build, once the goal reaches you):
im missions worker            # everything active at your stations
im mission doc read worker <ms> goal.md
im mission doc write worker <ms> --id impl --file src/receipt.md   # → receipt
im mission submit worker <ms> --revision N --outcome done --receipts document:<hash>

# a human, when a mission hops to a user station (other templates):
im inbox
im mission submit boss <ms> --revision N --outcome ok --reason "go ahead"
```

Station prompts interpolate `{mission.name}`, `{mission.objective}`,
`{mission.from}`, `{mission.iteration}` and `{mission.reason}` (unknown slots
stay literal). There are no persona playbooks: externally registered agents
get their instructions from the mission brief alone.

## The delivery pipeline

`im init` seeds four stations and writes `.im/templates/pipeline.yaml` — a
design→plan→build→review delivery flow distilled from
[matt-skills-with-to-goal](https://github.com/tt-a1i/matt-skills-with-to-goal):

```
human ⇄ design-agent session     grill → spec happens HERE: a conversation,
        │                        zero mission round-trips
        └─ design creates the mission ─▶ parks spec.md ──spec-ready──▶ plan
                                                                  goal-ready │
   design ◀──approved── review ◀──done── build ◀───────────────────────────┘

   final gate (design): accept = terminal │ reject ─▶ build (fix-only)
   return edges: spec-gap (plan|review) ─▶ design · blocked (build) ─▶ plan
               · rework (review) ─▶ build
```

- **design** holds two phases. The front phase lives outside the mission:
  grill the human in your own session conversation, then — at publish tier
  or above — create the mission and park the frozen SPEC (`spec-ready`). After review
  approves, design is the **final gate**: `accept` ends the mission, `reject`
  sends implementation fixes back to build.
- **plan** compiles the SPEC into a self-contained GOAL (compile, don't
  re-visit); an unactionable spec goes back as `spec-gap`.
- **build** implements the GOAL only, verifies each completion criterion with
  evidence, and reports with a receipt (`impl.md`) incl. the pre-work git
  baseline; local implementation only — no commit/push/deploy.
- **review** verifies the implementation against the GOAL on two independent
  axes (goal criteria / repo standards), every finding cited; `rework`
  findings land in `review.md` + `--feedback`.
- Findings travel on `--reason` (interpolated into the next station's
  prompt) plus `--feedback` and the mission documents — there is no peer
  channel, and the pipeline needs none.

The four agent charters ship as **work presets**
(`im work create <op> <key> --preset design|plan|build|review`, also offered
in the console's station-create modal), so a deleted station can be recreated
with its charter — a preset applies the standing prompt plus a one-line
summary shown on boards and listings. Seeded stations are ordinary stations: rebind, re-prompt,
or delete as you like — deletion is PS-style station locking: a station
referenced by any active mission contract (entry, works, or path endpoints)
is locked until those missions end; unreferenced stations are deleted outright.

## Commands

```
Workspace   im init | agents | leave | doctor | clean | ui
Inbox       im receive <id> [--wait] | pending | history   # arrivals + membership notices, no chat
Members    im grant|revoke <operator> <target>                # publish tier, by manage-tier op
           im member delete <operator> <target>                # non-manage targets only
           (manage tier: console Members page only — im ui)
Stations    im work create|list|set-executor|set-prompt|delete
            im work create <op> <key> [--description <t>] [--executor <agent>] [--preset <name>]
            im work set-prompt <op> <work> --preset <name>   # applies prompt + summary
            im work set-description <op> <work> <text...>
            # list shows executor, holding/en-route occupancy, and the one-line summary
Templates   im template list           (.im/templates/*.yaml)
Missions    im mission create|show|events|end
            im mission submit <agent> <ms> --revision N --outcome O
                                 [--next-node] [--reason] [--feedback] [--receipts]
            im mission abandon <agent> <ms> --revision N [--reason]
            im mission doc read <agent> <ms> <path>
            im mission doc write <agent> <ms> --id <docId> --file <path|->
            im missions <agent>        # active missions at your stations
Attention   im inbox                   # missions waiting at user stations
Console     im ui                      # browser console at http://127.0.0.1:4600 (localhost only)
```

## Mission templates

```yaml
schemaVersion: 4
name: example
entry: build
works:
  build:
    completion:
      outcomes: [done, need-rework]     # this station's vocabulary
      terminal: []                      # outcomes that end the mission here
      feedbackRequiredOn: []            # outcomes that must carry --feedback
    documentRights:                     # write does NOT imply read
      read: [spec]
      write: [impl]
  review:
    completion: {outcomes: [pass, fail], terminal: [pass], feedbackRequiredOn: [fail]}
    documentRights: {read: [impl], write: [notes]}
paths:
  - {from: build, when: done, to: review}
  - {from: review, when: fail, to: build, iterationPolicy: increment}
  - {from: review, when: any, to: approve}
documents:
  - {id: spec, kind: file, path: docs/spec.md}
  - {id: impl, kind: file, path: docs/impl.md}
  - {id: notes, kind: file, path: docs/notes.md}
```

Validation is fail-closed: reserved `abandon`, terminal outcomes with
out-edges, out-of-vocabulary `when`, undeclared document ids, dot/empty path
segments — all rejected at compile time, before any mission exists.

## Why "static core"?

Everything a service would do — routing, CAS, adjudication, delivery
history, notification fan-out — happens inside one SQLite transaction per
CLI invocation. Agents come and go, terminals die, identities get
re-suffixed: the database is the only truth, and every command re-derives
the world from it. Missions survive everything except `im clean`.

## Console

`im ui` starts an ephemeral localhost console (fixed port 4600, `--port` to
override, exits after 5 idle minutes): stations board with rebind and a
create modal that offers the pipeline charters as presets, missions with
revision/at/disposition, the user inbox with hop reasons and outcome buttons
that prompt for your `--reason` — required when routing onto another user
station — delivery history
timeline, member management (tier dropdown — manage included, works with
zero manage members because the console is the human — and delete), and
mission actions (create from template, end).

## Agent harness integrations

Any harness that can run shell commands can operate IM — the CLI is the only
interface. Two conveniences ship with the repo:

- **Codex Desktop**: `plugins/codex/infinite-mission-codex` — a watcher wakes
  the bound Codex task when IM work arrives (wake envelope
  `im-codex-wake/v1`), plus a duty-loop skill that treats the `im` CLI as the
  sole authority (live mission view, revision, rights, receipts).
- **Waiter-loop harnesses** (ZCode, Claude Code, …): hang
  `im receive <agent> --wait` in the background (6h default timeout, exit 0
  each cycle); its exit is your wake event — process the arrival note, work
  the mission, then re-hang.

## Development

```bash
cargo test          # 49 tests (e2e + mission + store + ui)
./check.sh          # full gate: rustfmt + clippy -D warnings + tests
                    # (bootstraps missing toolchain components; --fix applies fmt)
./install.sh        # release build → ~/.local/bin/im
```

Legacy-database migrations fold in place and run as one immediate
transaction — concurrent first-opens of the same workspace are safe. State
lives entirely in `.im/`, so replacing the binary never disturbs running
processes.

## Attribution

The no-daemon substrate (workspace discovery, agent registry, session
tokens, notice lock/wait loop, ephemeral UI server) is derived from
[squad](https://github.com/mco-org/squad) (MIT).
The mission semantics follow the work-mission mailbox model. MIT license.
