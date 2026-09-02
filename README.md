# InfiniteMission

**`im` — multi-agent mission orchestration in a static SQLite core.**

No daemon, no server, no background process. Every command is a one-shot CLI
process over a WAL-mode SQLite file in your workspace. The runtime semantics
are the work-mission model: a mission is one-shot mail that carries its own
contract, travels between stations, and records its own delivery history.

```
cargo install --path .   # binary: im
```

## The model in one screen

- **Workspace** = a directory tree with `.im/` at the root (found by walking
  up, like git). The workspace is the single project: everything lives in
  `.im/im.db` + `.im/mission-documents/`.
- **Agents** self-register with `im join <id> --role <role>`. A taken id
  auto-suffixes (`alice` → `alice-2`) — identities are never impersonated.
  Agents are stateless: all durable state lives in missions.
- **Operators** are humans you trust: `im grant <id>`. Operators create
  stations and missions.
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
  had to give. An operator resolves them.

## Quick start

```bash
mkdir my-project && cd my-project
im init                       # workspace, roles, example template, /im setup

im join boss --role manager   # in each agent's terminal
im join worker --role worker
im grant boss                 # a human runs this: boss is now an operator

# as boss:
im work create boss build --executor worker
im work create boss review --executor reviewer
im work create boss approve          # user station — no executor
im mission create boss --template example --key v1

# as worker:
im receive worker             # arrival note: a mission landed at your station
im missions worker            # everything active at your stations
im mission show <ms> --for worker   # prompt, document rights, vocabulary, routes, revision
im mission doc write worker <ms> --id impl --file src/impl.md   # → receipt
im mission submit worker <ms> --revision 1 --outcome done \
    --receipts document:<hash>

# a human, when a mission hops to a user station:
im inbox
im mission submit boss <ms> --revision 2 --outcome ok
```

Slash commands install for Claude Code, Gemini CLI, Codex and OpenCode via
`im setup` (already run by `im init`): `/im worker`, `/im boss`, … teach the
role loop to the hosting agent.

## Commands

```
Workspace   im init | agents | leave | roles | setup | doctor | clean | ui
Messaging   im send <from> <to|@all> <text> | receive <id> [--wait] | pending | history
Operators   im grant|revoke <agent> | operators
Stations    im work create|list|set-executor|set-prompt|retire
Templates   im template list           (.im/templates/*.yaml)
Missions    im mission create|show|events|end
            im mission submit <agent> <ms> --revision N --outcome O
                                 [--next-node] [--reason] [--feedback] [--receipts]
            im mission abandon <agent> <ms> --revision N [--reason]
            im mission doc read <agent> <ms> <path>
            im mission doc write <agent> <ms> --id <docId> --file <path|->
            im missions <agent>        # active missions at your stations
Inbox       im inbox                   # missions waiting at user stations
Console     im ui                      # ephemeral browser console (localhost)
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

`im ui` starts an ephemeral localhost console (random port, opens the
browser, exits after 5 idle minutes): stations board with rebind, missions
with revision/at/disposition, the user inbox with hop reasons, delivery
history timeline, and operator actions (grant/revoke, create mission from
template, end mission).

## Attribution

The no-daemon substrate (workspace discovery, agent registry, session
tokens, message lock/wait loop, slash-command installation, ephemeral UI
server) is derived from [squad](https://github.com/mco-org/squad) (MIT).
The mission semantics follow the work-mission mailbox model. MIT license.
