# Ferry: guide for contributors and coding agents

Ferry runs a repository's recipe at an exact commit on a machine that has
the required features, and records what happened. This file is the working
agreement for anyone changing Ferry, human or agent. `CLAUDE.md` links here.
Start with [`README.md`](README.md) for what Ferry does. This is how the
maintainers and their coding agents work; an outside contributor needs only
[`CONTRIBUTING.md`](CONTRIBUTING.md).

## Check

`just check` runs rustfmt, strict Clippy (`-D warnings`, pedantic) and every
test: unit tests and real-CLI contract tests (`tests/`). Run it inside
`nix develop`, which provides the toolchain and the tools the tests use
(git, jj, python3, and procps and util-linux on Linux).

Changes to transport or custody need real execution controls, not only unit
tests: exact commits, refusal versus recipe exit, seat isolation, the
session stop at the run's end or deadline, and the run surviving client
loss. Linux-only code is never compiled on a Mac: run the same check on a
Linux machine (`ferry run check --on <linux-host> --commit <rev>` does
exactly that) and in a Darwin Nix build
(`nix build --no-link -L "git+file://$PWD?rev=<rev>"`).

## Structure

One Rust crate builds one binary, `ferry`. Callers use `ferry run` and its
siblings; a place answers the hidden `ferry place …` verbs over SSH or a
local exec. [`docs/SPEC.md`](docs/SPEC.md) is the behavioural target, and
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) maps it onto the modules.
Project-specific environment and artifact handling are hooks, not branches
in Ferry. Recipe requirements have one home: the project's catalog.

Keep receipts honest: a focused control is not a project's gate, and a
remote result does not certify a repository's own local gate.

## Version control: jj, one workspace per agent

Ferry is developed with [jj](https://jj-vcs.github.io/jj/) on a colocated
checkout (`.git` and `.jj`). When several agents work at once, each gets its
own jj workspace, so it never snapshots another agent's half-finished edits.
One agent coordinates (reviews and gives GO, runs installs), others
implement. The colocated checkout itself holds the issue database and the git
object store; nobody edits code there.

- **Work:** `jj new main`, edit, then `jj describe -m "ferry: subject
  (ferry-xxx)"`. To pick up others' work, `jj rebase -d main`.
- **Freeze:** a change under review is frozen: run `jj new` before editing
  again, so a reviewer's view never moves under them. Never run jj in a
  workspace while a check runs there (a snapshot changes what is checked).
- **Land:** `just land`. It runs `just check` on your change, together with
  any described commits beneath it, and only then moves `main` and pushes it
  to `origin`. It refuses undescribed commits in the range, a stale parent,
  `main` differing from `main@origin` (run `jj git fetch`, then
  `jj rebase -d main`), and any edit made while the check ran. If the push
  fails, `main` moves back. One land at a time per repository
  (`.jj/land.lock`). Never push by hand: that skips the gate.
- **Review:** an implementer lands after the coordinator's GO. Each freeze
  lists every control it claims with `grep -c 'fn <name>'` run at the
  frozen commit.
- **Critics and knockouts:** before a freeze, an adversarial review of the
  diff, and a knockout per new control: undo the fix in a separate copy of
  the tree, with its own `CARGO_TARGET_DIR`, and show the control fail.
- **Trailers:** a SessionStart hook (`scripts/jj-identity`) adds
  `Jj-Workspace:`, `Agent:` and `Session:` to every description. Don't type
  or strip them.
- **Git:** only read with git, and only in the colocated checkout. A jj
  workspace has no `.git`, so git there may answer about an enclosing
  repository instead. (`scripts/ferry-install` and `qualify-install` read
  and push commits with git; run them from the colocated checkout.)
- **Stale workspace:** when another agent rewrites a commit under your `@`,
  jj refuses until `jj workspace update-stale`. Never `jj edit` a change that
  is an ancestor of another workspace's `@`.
- **Recovery:** start with `jj op log`, `jj undo` or `jj op restore`, before
  any destructive file operation.
- **New workspace:** `just workspace-add <name>` in the colocated checkout.
  It also points the issue database and Claude's memory at the main
  checkout. `jj workspace list` is the roster.

## Shared machines

Ferry's build hosts are shared by many agents.

- **Stop a process only by its exact pid**, or a process group you created
  and recorded. Never `pkill -f`, `killall` or any name or pattern match: a
  pattern kills other agents' work.
- **Freeing disk, retrying, queueing and containment** are operator and
  project decisions, not Ferry's (see `docs/PRINCIPLES.md`). Ask before deleting
  anything on a shared host.

## Done means landed

1. `jj describe -m "ferry: subject (ferry-xxx)"`, then `just land`.
2. Close the issue (`bd close <id>`), then `bd dolt push`.

## Design gate

Design questions are decided by [`docs/PRINCIPLES.md`](docs/PRINCIPLES.md). Every
freeze description states:

1. which principles it touches;
2. which core-model concepts and synchronizations it changes;
3. the lock rank of any lock it takes;
4. its fallible points, each with an injection test (`FERRY_FAIL_AT`);
5. the record fields it adds;
6. the capability name of any host behaviour it changes;
7. which environment cells passed: the Mac `just check`, a Darwin Nix build,
   and the check on a Linux host.

It also says what it deletes. Ferry grows only when the deletion-first
alternatives are worse, and the freeze says by how many lines.

A change that needs a principle to bend amends `docs/PRINCIPLES.md` first, in its
own reviewed change.

## Documents

- **Living documents** are kept true to the code: at the root README,
  CHANGELOG, CONTRIBUTING, SECURITY and AGENTS; in `docs/` REFERENCE,
  PRINCIPLES, ARCHITECTURE and RELEASE. `docs/SPEC.md` is the normative
  target: every change cites the SPEC sections it
  implements, and a change to the target amends SPEC first, in its own
  reviewed change. Change living documents in the same commit as the
  behaviour they describe.
- **Dated design documents** are in [`docs/design/`](docs/design/README.md) as
  `YYYY-MM-DD-topic.md`. Each references its issues, reviews and evidence.
  They are amended or superseded by a later dated document, never silently
  rewritten to match the code.

## Issues

Ferry's own work is tracked with [beads](https://github.com/steveyegge/beads)
(`bd ready`, `bd show <id>`, `bd update <id> --claim`, `bd close <id>`; full
reference: `bd prime`). The issue database is local Dolt, synced with
`bd dolt push` / `bd dolt pull`, never through git; `.beads/config.yaml` is
tracked config only, so keep machine paths out of it. Outside contributors
use GitHub issues.
