# Changelog

All notable changes to `ferry` are documented in this file.

## Unreleased

- **`needs` is checked on every route.** `--local` now refuses (125) a
  recipe whose `needs` this machine lacks, naming what is missing: a recipe
  that needs `linux-x86_64`, run `--local` on a Mac, is refused instead of
  running on macOS. The same holds for a recipe that may not run elsewhere
  and for a caller with no machines.
  A place has what it detects (OS, architecture, and both), its Ferry's
  capabilities, and on a machine its declared `has`; a remote run is
  checked again on what the place reports before anything is sent, which
  replaces the old architecture-only check. Refusals that held a parser's
  line breaks are now one line.
- **A run inside a run on the same place is refused.** A `ferry run` started
  from a recipe or hook (it carries the run's `FERRY_ID`) toward that run's
  own place used to wait on the locks its own run held, until its wait ran
  out, or to start beside it; it is now refused at once (125), naming the
  outer run. A run to another place still goes ahead.

## v0.1.6 - 2026-10-21

- **Small readability fixes.** A busy seat gives its holder's age in seconds
  under two minutes ("submitted 40 s ago", not "0 min ago"), and
  `ferry recipes` shell-quotes each argument, so
  `sh -c 'echo hi; exit 17'` reads as it runs.

## v0.1.5 - 2026-10-21

- **The caller says what is happening.** A run prints where it went
  (`ferry: <id> → bench, seat default, commit <short>`), that uncommitted
  changes are not in the commit run, what a quiet run waits for and who
  holds it, and ends with `ferry: <outcome>, exit <code>, run <id>` after its
  reason. `--json` adds a `waiting` event and `machine`, `seat` and `commit`
  to `submitted`; every other JSON shape and every exit code is unchanged.
- **Plainer refusals.** An unknown run id, commit or repository, and a
  `ferry.toml` that does not parse, say so (the file and its commit named)
  instead of an OS or git error. A stopped run's reason names who stopped
  it. `ferry stop` answers in a sentence. A wait that ran out says seconds
  and how to stop another caller's run.
- **Easier to read.** `ferry recipes` and `ferry machines` print one line per
  entry in version 2's names; `ferry status` prints sizes and ages and
  leaves unknowns out, and labels this workspace's local seats. Every
  command and option has help; `ferry run --help` gives the default commit
  and the exit codes. `ferry follow` on a finished run replays its output.
- **Removed:** `ferry receipt` (use `ferry record`, which completes the
  receipt and says when the run is not final). `--on` with `--local` is now
  a usage error (exit 2) instead of a refused run with a receipt.
- **Fixed:** the place's `record` could answer "not found" for a run that
  became final between its two reads.

## v0.1.4 - 2026-10-20

- **Config files read in plain words.** `ferry.toml`, `machines.toml` and
  `host.toml` take `version = 2`, whose keys say what they do: `needs`,
  `may_run_elsewhere`, `keep_between_runs`, `[[also_send]]`,
  `send` / `refuse_other_vars_starting_with` / `keep_local`,
  `[machine.<name>]` with `platform` and `has`, and
  `refuse_runs_below_free_disk = "40 GiB"` instead of a byte count. Version 1
  files are still read for this release; a version 2 file holding a version 1
  key is refused, naming the new one. `host.toml` gains `max_run_time`, the
  longest timeout a run may ask for.
- **A reserved machine.** `reserved_for = ["quiet"]` in a version 2
  `machines.toml` keeps a machine out of the automatic choice unless a
  recipe needs one of those features, so a timing host is not taken by runs
  that forgot `--on`. `--on` still chooses it.

- **A late prune no longer hides a recreated seat.** If a run recreated a
  seat while a prune's retirement of it was still on its way back, the
  caller's book took the late retirement as the latest word, and later
  prunes never considered the new seat. A retirement now counts only if it
  meant the newest use the book holds. If the retire instead deleted the
  recreated seat, the book lists it live until the next prune finds it
  gone.

## v0.1.3 - 2026-10-19

- **A stalled push no longer holds the caller.** The caller's object push
  now has an output watchdog: 60 s without new progress output (git's own,
  or the place's `remote:` lines) stops the push and refuses the run (125),
  "the push printed no progress for 60 s". A large push that keeps printing
  progress is never cut. The store setup now has the same 60 s reply bound
  as `host-info`. Before, a place's git that stopped reading held the caller
  forever, with no record.

## v0.1.2 - 2026-10-19

- **Run history no longer grows forever.** With
  `delete_run_history_after_days = <n>` in a place's `host.toml`, each run,
  after its record is final, deletes the folders and records of runs
  submitted more than `n` days ago, for at most 1 s and never beside an
  exclusive run. A run whose leftover process is still alive is kept, and
  `ferry record` on a deleted run names the setting. Without the key nothing
  changes. Seen as 2,740 run folders (5.7 GB) on one shared host after a
  week.

## v0.1.1 - 2026-10-19

- **Seats no longer pile up.** `ferry seats prune --idle <dur>` also
  retires this caller's seats unused for that long, and `ferry run --once`
  retires its seat once the run's record is final. With
  `delete_unused_checkouts_after_days = <n>` in `machines.toml`, every run
  prunes its place's seats unused for `n` days by itself, at most once an
  hour per place, adding one line to stderr. Retire now never deletes during
  an exclusive run, checks under the seat lock that the seat was not used
  since, and renames the seat away before deleting it, so a failed delete
  never leaves half a seat. `ferry status` lists such leftovers as
  tombstones, and a refusal below the disk floor names what to run. Seen as
  97 of 104 seats on one shared host idle for over a day.

- **A killed supervisor no longer leaves its recipe unbounded.** The session
  owner stops its own session 1 s after the run's timeout or deadline
  (TERM, 10 s, KILL), so a supervisor that is SIGKILLed or OOM-killed no
  longer leaves the recipe running until it exits. A recipe that ends
  after its supervisor was killed no longer leaves the processes it started
  running either. The record still reconciles to `lost`.
- **An env hook that leaves its output open fails the run.** If the
  `[hooks] env` hook exits while something it started still holds its
  stdout or stderr, the run now fails 2 s later with "the stage's output
  still open", instead of waiting forever. The prepare hook and the recipe
  are not captured, so they are unaffected.

- **Fewer `unknown` survivors on busy Linux hosts.** The marker pass reads a
  process it could not read again every 100 ms for up to 1 s, instead of
  once: a large build process still tearing down its memory as it exits no
  longer makes the run's survivors `unknown`. One still unreadable after the
  second counts, as before (3.4% of runs on one shared host read `unknown`
  before this change).

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
