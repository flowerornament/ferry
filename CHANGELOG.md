# Changelog

All notable changes to `ferry` are documented in this file.

## Unreleased

- **A stalled SSH connection no longer holds the caller.** Every SSH
  connection the caller makes has a connect timeout and keepalives, and the
  replies it needs before submitting a run (`host-info`, file reads) have a
  60 s bound; past it the run is refused (125). Seen as callers waiting
  minutes on a stalled jump host.
- **A failure before submit is a refusal, not a lost run.** Nothing has run
  until the place is asked to, so such a run now ends `refused` (125)
  instead of `lost` (255). The `submitted` receipt is written just before the
  request is sent, so a caller killed earlier leaves no receipt claiming a
  run.

## v0.1.0 - 2026-10-15

The first public release. Ferry runs a repository's recipe at an exact
commit, locally or on a machine over SSH, and records exactly what happened.

- **Exact commits.** A run's commit, even an unpushed one, is pushed to the
  machine's private store and a warm, private checkout (a seat) is reset to
  it. Recipes and their requirements are read from that commit.
- **Detached runs.** A supervisor on the machine owns each run, writes its
  output to a file and keeps one record, final exactly once. A dropped
  connection never ends a run: `ferry follow <id>` re-attaches, `ferry record
  <id>` completes the receipt, and every run has an absolute deadline.
- **Honest outcomes.** The receipt and exit code tell a recipe failure from a
  refusal (125, nothing ran) from a lost run (255, the infrastructure
  failed), and `timeout` (124) and `stopped` (128+signal) apart.
- **Seats and the host lease.** One run at a time per seat (a busy seat
  refuses at once, naming the holder); a machine-wide lease lets an
  `exclusive` run hold the host alone for measurements. Only a run's seat
  lock is inherited by its processes, so a leftover can never close a
  machine to other seats.
- **Custody.** At a run's end its session is stopped; one bounded pass then
  lists, by pid, start and executable, every process still carrying the run's
  `FERRY_ID`, however it left the session, or records `unknown` when it
  cannot read everything (common on macOS).
- **`ferry status`.** A read-only view of a place: disk, live runs and their
  phase, the gate's holder and requesters, seats with their size and last
  run, and processes left by ended runs.
- **Artifacts** are hashed per file on the machine and verified per file by
  the caller, with a status kept apart from the verdict.
- **Host policy and bounded seats:** a disk floor and a per-seat size cap
  (`host.toml`), and `ferry seats prune` for seats whose workspace is gone.
- **Shipped objects** (`[[ship]]`): extra refs, or dependencies served at
  their URL from the run's own store.
- **Packaging:** a Nix flake with a Home Manager module (`programs.ferry`)
  that also links the agent skill; binaries for macOS (aarch64) and Linux
  (x86_64, aarch64).
