# Ferry

Run a recipe at an exact commit on a machine that has the required features.

The source is in a private GitHub repository (`flowerornament/ferry`, remote
`origin`); keep it private until the user says otherwise. `just land` is the
only way anything reaches it. Use jj; do not mutate Herald or Murail from
this checkout.
Current extraction is tracked on Herald's herald-4e6mw bead.

## Check

`just check` runs Rust formatting, Clippy and contract tests. Changes to transport
require real execution controls for exact commits, refusal versus recipe exit,
seat isolation, session stop at the run's end or deadline, and the run
surviving client loss.

## Structure

One Rust crate builds one binary, `ferry`: callers use `ferry run` and its
siblings, and a place answers the `ferry place …` verbs over ssh or a local
exec. `SPEC.md`
is the behavioural target.
Project-specific environment and artifact handling are hooks,
not branches in Ferry. Recipe requirements have one home in the project catalog.

Keep receipts honest: focused controls are not project gates, and remote
evidence does not certify Herald's local `just land`.

## Version control: jj, one workspace per agent

```text
~/code/ferry      colocated checkout (.git + .jj): bd home, the git object store.
                  No agent edits code here.
~/code/ferry-1a   jj workspace: coordinator (reviews, gives GO, installs)
~/code/ferry-1b   jj workspace: implementer
```

Each agent gets its own workspace, so it never snapshots another agent's
half-finished edits. hail names a seat after the jj root, so each workspace
is also a hail address: `hail ferry-1b ask '…'` reaches the agent working
there.

- **Work:** `jj new main`, edit, then `jj describe -m "ferry: subject
  (ferry-xxx)"`. To pick up others' work, run `jj rebase -d main`.
- **Publish:** `just land`. It runs `just check` on your change, together
  with any described commits beneath it, and only then moves `main`. It
  refuses undescribed commits in the range, a stale parent, and any edit made
  while the check ran. One land at a time per repository: it holds
  `.jj/land.lock` (flock, released on exit or crash) from the pin to the
  publish, so a second land waits instead of gating a stale candidate.
  It then pushes `main` to `origin`; if the push fails, `main` moves back.
  When `main` differs from `main@origin` it refuses: run `jj git fetch`, then
  `jj rebase -d main`. Never push by hand (`jj git push`, `git push`): that
  skips the gate. The root commit has an empty committer, so jj refuses to
  push it; it is already on `origin`, so only a new remote meets it. The
  implementer lands after the coordinator's GO; contract changes also need
  the consumer reviewers' GO (murail-2a, murail-4a), by execution.
- **Install:** only landed commits install, through `scripts/ferry-install`
  on hosts and the caller's Nix configuration (RELEASE.md). Announce a window
  to the seats that run Ferry first, and qualify with `just qualify-install`.
- **Trailers:** a SessionStart hook (`scripts/jj-identity`) adds
  `Jj-Workspace:`, `Agent:` and `Session:` to every description. Don't type
  or strip them.
- **Git:** run it only in `~/code/ferry`, and only to read. A workspace has
  no `.git`, and `~/.git` exists, so git in a workspace silently answers
  about your home directory. (`scripts/ferry-install` and `qualify-install`
  use git to read and push commits; run them from `~/code/ferry`.)
- **Processes:** stop a process only by its exact pid, or a process group you
  created and recorded. Never `pkill -f`, `killall` or any name or pattern
  match: this Mac and bench are shared by many agents' seats, and a pattern
  kills theirs.
- **Stale workspace:** when another agent rewrites a commit under your `@`,
  jj refuses until you run `jj workspace update-stale`. Never `jj edit` a
  change that is an ancestor of another workspace's `@`.
- **Recovery:** start with `jj op log`, `jj undo` or `jj op restore`, before
  any destructive file operation. The `jj-ops` skill has the full model.
- **New workspace:** run `just workspace-add ferry-<pair><letter>` in
  `~/code/ferry`. It also points bd and Claude's memory at the main checkout.
  `jj workspace list` is the roster.

## Done means landed

1. `jj describe -m "ferry: subject (ferry-xxx)"`, then `just land`.
2. `bd close <id>`, then `bd dolt push`.

## Design gate

Design questions are decided by `PRINCIPLES.md`. Every freeze description
states:

1. which principles it touches;
2. which core-model concepts and synchronizations it changes;
3. the lock rank of any lock it takes;
4. its fallible points, each with an injection test;
5. the record fields it adds;
6. the capability name of any host behaviour it changes;
7. which environment cells passed (Darwin and Linux Nix check, bench, a Mac host).

A change that needs a principle to bend amends `PRINCIPLES.md` first, in
its own reviewed change.

## Documents

- **Living documents** are at the root in capitals and are kept true to the
  code: README, AGENTS, PRINCIPLES, RELEASE, ARCHITECTURE (where SPEC's
  model lives in the code; later REFERENCE). `SPEC.md` is the normative end state that M1 converges
  on: every slice cites the SPEC sections it implements, and a change to the
  target amends SPEC first, in its own reviewed change. Change them in the same commit as the behaviour they
  describe.
- **Dated design documents** are in `design/` as `YYYY-MM-DD-topic.md`. Each
  references its beads, reviews and evidence. They are amended or superseded
  by a later dated document, never silently rewritten to match the code.
- **Current plan:** `design/2026-10-09-ferry-minimal.md` (one thing done well; deletion-first roadmap), which supersedes `design/2026-10-08-core-model-v3.md` where they differ. v3 and v2 keep the review history (rules A–C, D1–D10, E1–E14). `design/2026-10-08-coordination-and-dx.md` keeps the requirements and evidence.

## Issues

Ferry's own work is tracked with bd (`bd ready`, `bd show <id>`,
`bd update <id> --claim`, `bd close <id>`; full reference: `bd prime`).
Consumer coordination stays on Herald and Murail beads (herald-4e6mw,
herald-v0m46, murail-2qnnz). Issues live in a local Dolt database; sync with
`bd dolt push` / `bd dolt pull` to the file remote, never through git.
`.beads/config.yaml` is tracked config only; keep machine paths out of it.
