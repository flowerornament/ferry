# Ferry

Run a recipe at an exact commit on a machine that has the required features.

This repository is local-only until the user chooses a forge remote. Never
publish it. Use jj; do not mutate Herald or Murail from this checkout.
Current extraction is tracked on Herald's herald-4e6mw bead.

## Check

`just check` runs Rust formatting, Clippy and contract tests. Changes to transport
require real execution controls for exact commits, refusal versus recipe exit,
slot isolation, and session cleanup on client loss.

## Structure

One Rust crate builds one binary: `ferry run` and `ferry host-run`. Murail's
reviewed transport 8ba36cfa is a behavioral spec, not a second implementation.
Project-specific environment and artifact handling are hooks,
not branches in Ferry. Recipe requirements have one home in the project catalog.

Keep receipts honest: focused controls are not project gates, and remote
evidence does not certify Herald's local `just land`.

## Version Control

This project uses jj, colocated with git. The `main` bookmark is the only
landed history; jj records edits continuously, so there is nothing to stage.
When more than one agent writes, each gets its own jj workspace
(`jj workspace add --name ferry-1a ../ferry-1a`); no agent then edits code in
this colocated checkout.

- Work: `jj new main`, edit, `jj describe -m "ferry: subject"`. Pick up
  others' landings with `jj rebase -d main`.
- Land: `just land` runs `just check` on exactly the described change, then
  moves `main` to it. It refuses undescribed work, a stale parent, and any
  edit made while the check ran. With no `origin` remote it pushes nothing;
  never add a remote or run `jj git push` yourself.
- Every description gains `Jj-Workspace:`, `Agent:` and `Session:` trailers
  from `scripts/jj-identity`, which a SessionStart hook runs. Do not type or
  strip them.
- Git runs only in this checkout, and only to read. A jj workspace has no
  `.git`, and `~/.git` exists, so git there silently answers about the home
  directory. Never run a mutating git command.
- Staleness: when another agent rewrites a commit under your `@`, run
  `jj workspace update-stale`. Never `jj edit` an ancestor of another
  workspace's `@`.
- Recovery starts at `jj op log`, `jj undo` and `jj op restore`, before any
  destructive file operation. The `jj-ops` skill holds the jj model.

## Completion

`jj describe -m "ferry: subject"`, then `just land`. Work is not complete
until `just land` succeeds.
