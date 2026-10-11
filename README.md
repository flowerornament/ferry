# Ferry

Run this repository's recipe at this exact commit, on a machine that can run
it, and record exactly what happened.

Ferry is for agents and people who share build hosts. You declare recipes in
the repository and machines once. Ferry gives each run:

- **The exact commit you named.** No drifting remote checkout: Ferry pushes
  that object and resets a private checkout to it.
- **No collisions.** Each workspace gets its own warm checkout (a *seat*), and
  a host lease keeps timing runs alone on the machine.
- **Nothing left behind.** Processes a run started are stopped when it ends.
  A seat that cannot be proven clean refuses the next run and says why.
- **An honest outcome.** A receipt and an exit code tell a recipe failure
  from a refusal (nothing ran) from a lost run (the infrastructure failed).

Ferry is deliberately small. It is not a scheduler, a CI system, a service
manager, a sandbox, a lock service or a deployment tool. `PRINCIPLES.md` says
what it is and is not, and why.

## Quick start

```sh
# 1. Install (Nix; see RELEASE.md for hosts and rollback)
nix build git+file:///path/to/ferry          # or pin it in your Nix configuration

# 2. Declare recipes in the repository (commit this file)
cat > ferry.toml <<'EOF'
version = 1
[recipe.test]
command = ["cargo", "test", "--locked"]
requires = ["linux"]
routable = true
EOF

# 3. Declare machines once (~/.config/ferry/machines.toml)
cat > ~/.config/ferry/machines.toml <<'EOF'
[host.bench]
ssh = "bench"
features = ["linux", "x86_64"]
EOF

# 4. Run
ferry recipes                    # what this commit allows
ferry machines                   # what you configured
ferry run test --on bench        # remote, at HEAD's commit
ferry run test --local           # this machine, in a clean checkout of the commit
ferry run test --on bench -- --nocapture       # arguments after --
ferry receipt <id>               # what happened
```

With no machines configured, runs are local. A machine that lacks a required
feature refuses rather than running.

## Concepts

| | |
|---|---|
| **Recipe** | A named command (argv) the repository allows to run, with required features. Read from the commit being run. |
| **Machine** | Where work runs: `local`, or an SSH destination with declared features. |
| **Seat** | A private, warm checkout for one workspace (`--seat`, default `default`). One run at a time per seat: a run that finds its seat's lock held is refused at once (125), naming the run that holds it, its age and the next step. |
| **Host lease** | Shared by every run on a machine; held alone by an exclusive run. |
| **Run** | One attempt. It ends with exactly one receipt and one exit code. |
| **Receipt** | `.ferry/receipts/<id>.json`: the commit, the binaries on both ends, the outcome, custody, lease and admission facts. Written at submit with every caller fact (`state: submitted`), then replaced by the final receipt (`state: final`). A refusal before submit is a final receipt. |
| **Host record** | The machine's account of a run (`records/<id>.json` under its state root). The supervisor reserves the id before any effect (`<id>.reserved.json`, with its pid, start time and boot id), so a second submit of the id is refused. It finalizes once, with the reservation's fields plus the run's, linked into place and synced, never replaced. A reservation whose supervisor is provably gone (another boot, a dead pid, another start time) is reconciled to `lost`/255, `supervisor gone (reconciled)`, by its caller or by the next run on its seat; a live supervisor never is. |

## Commands

```
ferry run <recipe> [--on <machine> | --local] [--exclusive] [--seat <name>]
          [--commit <rev>] [--timeout-seconds <n>] [--wait-seconds <n>]
          [--json] [-- <args>…]
ferry follow <id>                   re-attach: the run's output, then its receipt
ferry stop <id> [--other --reason '<why>']
                                    stop your run (another caller's needs a reason)
ferry record <id>                   the receipt, completed once the run is final
                                    (exit 3 while it has no verdict yet)
ferry recipes                       the recipes at this commit
ferry machines                      the configured machines
ferry receipt <id>                  print a receipt
ferry seat recover <seat> [--local] [--accept-unknown --reason '<why>']
                                    clear a seat once its processes are proven gone
```

`ferry place`, `host-info` and `execute-session` are internal: the caller
runs them on the machine. `ferry host-run` only refuses callers from before
detached runs.

### Outcomes and exit codes

Read the receipt's `outcome`, not the recipe's printed text, to tell a Ferry
refusal from a recipe failure. A recipe may itself exit 125.

`ferry record <id>` exits with the final record's code, or 3 while the run
has no verdict yet (`state: submitted`): poll again, or `ferry follow <id>`.
Exit codes are for people. A recipe's own code can be 3, 124, 125 or 255,
so a program decides by the `--json` receipt: `state` first, then
`outcome` (SPEC §4).

| outcome | exit | meaning |
|---|---|---|
| `ran-pass` | 0 | The recipe ran and exited 0. |
| `ran-fail` | the recipe's code | The recipe ran and failed. |
| `timeout` | 124 | `--timeout-seconds` (or the run's absolute deadline) elapsed; the recipe's session was ended. |
| `refused` | 125 | Nothing ran: unknown recipe or machine, unmet requirement, dirty or unknown commit, tainted seat, host policy, or a host that refused before acceptance. |
| `stopped` | 128+signal | `ferry stop`, a signal to the caller, or one to the supervisor came before the recipe's result: 130 for SIGINT, 143 for SIGTERM, 129 for SIGHUP. `stopped_by` names who and why. |
| `lost` | 255 | The run ended without a verdict of its own: its supervisor is gone (reconciled), or something failed after the recipe started. |

A recipe that already reached its result is never turned into `stopped` or
`lost`. Later events (a signal during the collect hook, a failed release) are
noted in `reason` as "after the verdict: …".

A receipt whose `state` is `submitted` claims no verdict (it reads `lost`
255): the caller left, could not deliver its stop, or the place stopped
answering, while the run may go on. `ferry record <id>` completes it from the
place's final record.

## Project configuration

Commit `ferry.toml`:

```toml
version = 1

[recipe.test]
command = ["devenv", "shell", "--", "mix", "test"]
requires = ["linux-x86_64"]
routable = true

[hooks]
prepare = "scripts/ferry-prepare"
env = "scripts/ferry-env"
collect = "scripts/ferry-collect"
```

- **Read from the exact commit.** Recipes and requirements always come from
  the commit being run, never the working tree.
- **Explicit argv.** Ferry never infers a tool from a recipe's name.
- **Only `routable = true` recipes go remote.** Publishing and certifying
  recipes stay unroutable; for example, Herald's `just land` stays local.
- **`exclusive`.** `requires = ["exclusive"]` (or `--exclusive`) holds the host
  lease alone. It is an execution lock, not a machine feature: routing ignores
  it.

### An existing catalog or adapter

If recipes already have a catalog, point to it:

```toml
version = 1
[catalog]
path = ".config/murail-gates.toml"
table = "recipe"
```

If the catalog has another schema, set `[catalog] adapter =
"scripts/ferry-catalog"`.
- **What the adapter emits:** this committed executable runs caller-side in
  fresh checkouts of the pinned commit and prints `{"version":1,"recipe":{...}}`.
  It may also emit:
  - a `mirror` list derived from the repository's lockfile;
  - an `environment` declaration;
  - `preserve`, a list of ignored build directories kept across resets, for
    example `["_build", "deps", ".devenv"]`.
- **Checks:** Ferry checks repeatability, validates the whole contract before
  routing, and records the digests of the adapter and the emitted contract.
- **Purity is your responsibility.** The adapter must be a pure reader; Ferry
  is not a sandbox.

**Preserved directories** are a declared cache:
- a run can depend on earlier runs in the same seat, not only on its commit;
- paths must be literal ignored directories with no tracked files, never
  `.git`, `.jj` or `.ferry`;
- tracked files, HEAD and hidden index flags are still checked;
- omit `preserve` for a clean run.

### Companion references

A recipe can request exact companion commits under fully qualified host Git
refs:

```toml
[companion_refs]
'refs/remotes/origin/master' = { remote = 'origin', branch = 'master' }
```

- **Which commit:** this selects the caller's locally known tracking ref, not
  the local branch or the remote's current state.
- **No fetching:** Ferry does not fetch or fall back, and the ref must name a
  commit directly (not a tag object).
- **A string source** keeps the caller's revision syntax.
- **In the receipt:** `companion_sources` (requested) and `companion_refs`
  (resolved). Only those exact objects are transported.

### Provider capabilities

A project can require capabilities of the Ferry that runs it, so that a
rollback or an older binary on one end cannot silently run without them:

```toml
[provider]
requires = ["admission"]
```

| capability | meaning |
|---|---|
| `args` | recipe arguments after `--` |
| `refusal-event` | typed refusals before acceptance |
| `custody` | seat custody records at release |
| `lease` | the shared/exclusive host lease |
| `admission` | host policy admission (`host.toml`), even when not configured |
| `detach` | detached runs (`ferry place` submit, follow, stop, record); a caller refuses a place without it (125, naming the place's `source_rev`) |

A run is refused (125) before any hook runs if:
- the caller does not know a required capability;
- the host's `host-info` lacks one (a host from before capabilities lists
  none);
- the host refuses the request.

The table may sit in `ferry.toml` or in an adapter's contract, not both. The
receipt records `provider_requires`.

## Machines

`$XDG_CONFIG_HOME/ferry/machines.toml` (default `~/.config/ferry`):

```toml
[host.bench]
ssh = "bench"
triple = "x86_64-unknown-linux-gnu"
cores = 16
features = ["linux", "x86_64", "quiet"]
```

- **Routing:** Ferry chooses the first machine whose features contain every
  requirement. `--on` names a machine; it does not bypass requirements.
- **SSH:** aliases come from your SSH configuration. Ferry copies no
  credentials and provisions nothing.
- **Features are declarations,** not measurements.

### Host policy and admission

A machine's operator may set a disk floor in `~/.config/ferry/host.toml` on
that machine. Each run is admitted under its seat lock, after the host lease,
before checkout and hooks.

```toml
# Version 1: a guard. Below the floor, refuse (125).
version = 1
min_free_bytes = 42949672960

# Version 2: below the floor, first reclaim named targets of idle seats.
version = 2
min_free_bytes = 42949672960
evict = { mode = "idle-seat-targets", targets = ["cargo-target"] }
```

- **No file:** the run proceeds, and the receipt records `not-configured`.
- **A malformed policy refuses** before anything is measured. That covers an
  unknown version or key, `evict` in version 1, and a target that is not a
  plain name.
- **Version 2** removes only `caches/<seat>/<target>` for the listed targets,
  oldest release first, until the floor is met.
- **Which seats are eligible:** a seat whose last release was recorded idle,
  that has no taint or owner marker (an unreadable marker means unknown, and
  the seat is skipped), whose lock can be taken without waiting, and whose idle
  record is unchanged under that lock.
- **Path safety:** every directory down to the target must be a real
  directory on the state root's filesystem, owned by its owner. Removal never
  follows symlinks.
- **Never removed:** checkouts, receipts, records, logs and anything outside
  `<state>/caches`.

If eligible seats run out while still below the floor, the run refuses (125).
The receipt's `admission` records:
- the policy read;
- the floor;
- the filesystem;
- availability before and after;
- each eviction, with its bytes;
- the seats skipped, and why;
- the result.

Availability can change for reasons outside Ferry, so the measured difference
is an observation. There is no collector or timer: reclamation happens only
inside an admitting run.

## Hooks and environment

Hooks are committed executables, with paths relative to the checkout. They run
inside the run's own session, in this order: prepare, env, the recipe, collect.
A run stopped before its session is spawned (a stop while it waits for locks
or prepares its seat) runs none of them, collect included: nothing ran.

- **env** prints a JSON object of environment strings, never shell code.
- **Inputs:** `FERRY_SEAT`, `FERRY_COMMIT`, `FERRY_ID`, `FERRY_WORKDIR`,
  `FERRY_CACHE_DIR`; collect also gets `FERRY_EXIT_CODE`.
- **Reserved:** `FERRY_*` and `GIT_CONFIG_GLOBAL`. A hook cannot replace the
  run's identity or the seat-local mirror configuration.
- **`FERRY_CACHE_DIR`** is a per-seat directory Ferry owns
  (`<state>/caches/<seat>`, mode 0700). Put regenerable build output there,
  for example `CARGO_TARGET_DIR=$FERRY_CACHE_DIR/cargo-target`. Host policy may
  reclaim named subdirectories of it from idle seats, and nothing else.
- **The `environment` declaration** has three lists:
  - `forwards`: caller variables sent to the recipe, recorded in the receipt,
    so never put secrets there;
  - `watches`: prefixes whose unlisted, nonempty variables refuse before any
    push;
  - `local`: variables deliberately kept on the caller.
- **Projects own** their own database preparation and artifact collection. A
  service a run starts and expects to outlive it belongs to the operator's
  service manager (`PRINCIPLES.md`, worked example).

## How a run is kept honest

### Transport

- **Exact objects only.** The caller pushes only the exact commit (and any
  companions and mirrors) to the machine's private bare repository, under
  `refs/ferry/<repo key>-<seat>/<commit>`, never to origin. Two concurrent
  runs of the same commit may race to create that ref. A push that loses the
  race to an identical ref counts as published.
- **One SSH connection per caller.** Ferry sets non-interactive SSH
  (`BatchMode=yes`) itself. Each caller process gets its own control
  connection in `~/.cache/ferry/ssh/<pid>/`, closed when the caller exits, so
  one run never rides on a connection another run's caller owns.
  - If that path would be too long for a Unix socket (a very long HOME), each
    SSH command opens its own connection instead.
- **A run token** binds the host's facts to this caller's receipt.
- **The checkout is checked:** a seat lock protects a detached checkout, and
  HEAD, a clean tree and hidden index flags are checked before execution.
- **Local runs** use the same mechanism, in a clean checkout of the commit
  under `.ferry/local`, not the caller's working tree.

### The host lease

Every run holds a machine-wide lease while it runs. Ordinary runs share it. An
exclusive run:
1. closes a gate, so no new run starts;
2. waits for in-flight runs to release their shared side;
3. runs alone.

There is no queue. Before it holds the gate, an exclusive run competes for it
like any other run. Receipts record `lease.mode`, `wait_ms`, `acquired_ms` and
`released_ms`.

**Scope.** "Exclusive" means no other Ferry run under the same state root:
one user's Ferry on one machine, or one repository's `.ferry/local`. It is not
a quiet-host certificate, and it does not quiet:
- processes started outside Ferry;
- other users;
- other virtual machines on the same hardware;
- Ferry binaries older than the lease.

A run never waits for its seat: a held seat lock refuses at once (125). The
refusal names the run that holds it, or the run that is starting and holds
it, or, when the lock outlived its run, that run ("held after run <id>
ended"). The work inherits the seat lock and the host lease on every
platform, so whatever a run leaves running while holding them keeps them
held. Its waits for the host lease end by its submit plus `--wait-seconds`
(default 30 min), apart from `--timeout-seconds`. Past it the run refuses
(125), naming the run that holds the lease exclusively, or "shared runs".
A place's refusal carries `why` and `next` apart (SPEC §6); the receipt's
reason is `<why>; <next>`.

### Custody: nothing a run starts outlives it

The supervisor is always a freshly spawned `ferry place supervise`, started
by `place submit` in its own session. Having just started, it has no children
of its own, so everything it later finds is the run's.

**At release.** After the collect hook, the receipt's `custody` records a
census of what the run left:
- **`empty`:** no live descendant, so the seat is recorded **idle**;
- **`survivors`:** live descendants remain. Each one, its descendants, and
  any process group one of them leads get SIGTERM, then SIGKILL after 10 s,
  then a second census. If that census is empty, the seat is idle, with
  `custody.stopped` listing what was stopped;
- **still alive, or the census is `unknown`:** the seat is **tainted**;
- **`unsupported`:** see macOS below.

**Release always happens.** Once the session is spawned, every path collects,
censuses, stops and records before the locks drop. If release itself fails,
the verdict stands and `custody.error` says why.

**A tainted seat refuses** new runs (125), naming:
- the run and its survivors (pid and start time);
- the locks they may still hold;
- the command to run: `ferry seat recover <seat>` on that machine (`--local`
  for local runs).

While any seat is tainted, an `--exclusive` run refuses. Any run that needs
the host lease also refuses before its first try in two cases: a taint says
its survivors may hold the lease exclusively or hold the gate, or a taint
cannot be read.

**Recovery never kills.** It clears a taint only when every recorded survivor
is gone, checked by pid and start time. A taint that cannot be proven clear
(no survivors named, or an unreadable record) needs the operator's explicit
`--accept-unknown --reason '<why>'`, which the recover receipt keeps.

**Locks outlive a killed supervisor.** The seat lock and host lease are
inherited by the recipe's processes, on every platform. A killed supervisor
therefore frees them only when the processes that kept them exit, so a new
run on the seat is refused busy instead of starting beside live work.
Release closes descriptors, never `LOCK_UN`.

**Linux and macOS differ:**
- **Linux:** the supervisor is a child subreaper. A process that leaves the
  recipe's session (`setsid`, double fork) is reparented to it, so it is still
  counted and stopped.
- **macOS:** there is no census yet (`unsupported`). Session teardown kills
  the recipe's session, but a descendant that leaves the session escapes.
  If it keeps the inherited locks, the seat stays busy until it exits, and
  the next run's refusal names the run it outlived. The seat is never
  recorded idle. The receipt says so.

### Detached runs, output and the verdict

A run belongs to its place, not to the connection that asked for it:

- **Submit.** The caller sends the request (`place submit`, over SSH or as a
  local exec). The place starts the supervisor paused, reserves the run id
  with the supervisor's pid, then releases it and replies within seconds. A
  retried submit with the same id and token gets the same run back.
- **Follow.** The caller copies `runs/<id>/output.log` from a byte offset until
  the record is final and every byte before it has arrived. After a dropped
  connection it resumes from the bytes it has. If the place does not answer
  for 5 min, the caller exits 255 and its receipt stays `submitted`. Once the
  place has accepted a run, only its final record gives the receipt a
  verdict: any other failure (reading that record, for one) leaves it
  `submitted`, exit 255, with `ferry record <id>` as the next step. A failed
  transfer of the run's artifacts is a note on its verdict.
- **Stop.** The first INT, TERM or HUP of the caller asks the place to stop the
  run (`stopped` 128+signal), and the caller keeps following to the record.
  The first stop decides: a later one, from anyone and with any signal, is
  accepted and changes neither the signal nor `stopped_by`. If the stop cannot
  be delivered, the caller prints `not stopped; continues until its deadline
  in <n> min; ferry stop <id>` and exits 255. A second signal leaves at once
  with 255. Either way the receipt stays `submitted`. A signal that comes
  after the run is final changes nothing: the caller delivers the rest of
  the output and exits with the run's code (a second signal leaves early,
  still with it). A caller killed outright stops nothing: the run goes on to
  its end or its deadline.
- **Deadline.** Every run ends by `submitted + wait + timeout + teardown`
  (`wait` is `--wait-seconds`, 30 min by default, the one bound on all its
  lock waits), which submit returns. A
  request without `--timeout-seconds` gets the place's maximum, 6 h (and none
  may ask for more).

**Output is data, never control.** The recipe's stdout and stderr share one
pipe, so the log keeps the order they were written in. One writer copies its
bytes into the log: no lines, no transcoding, nothing waits on a reader.
- **Cap:** past 1 GiB, output is read and dropped; the record says
  `output: {bytes, capped}`. A failed write is counted (`write_errors`).
- **End:** after the session stop, the writer gets 2 s more. A process outside
  the session that still holds the pipe is not waited for: the reason says
  "output still open", and the log is closed before the record.
- **The caller** writes the bytes exactly as logged, to stdout (to stderr with
  `--json`). A closed stdout or stderr is data: the run goes on.

**The verdict is decided once,** by the first terminal fact (above), on the
place, and kept in its final record.

**`--json`** emits version-one NDJSON events: `submitted` (with the
supervisor's pid), then `receipt`.

## Limits

- **Not a sandbox.** Runs execute as you, with your permissions.
- **Outside custody:** processes a run did not start, other users and other
  hosts. On macOS, also descendants that leave the session.
- **A supervisor killed outright** stops nothing. Its run is reconciled to
  `lost`/255 by its caller or the next run on the seat.
  - Its owner marker keeps that seat refused until `ferry seat recover`.
  - The inherited locks keep other runs out while its work lives, on every
    platform.
- **Unconfirmed session cleanup** writes a `cleanup-pending-*` marker. Further
  runs under that state root refuse until an operator confirms the session is
  gone and removes the marker; Ferry never clears it by age.
- **Remote evidence is not publication authority.** A remote result does not
  certify a repository's own local gate.
- **Timings:** never pool machine timings across platforms.

## Development

```sh
nix develop          # the toolchain and every tool the checks use (jj, git, util-linux on Linux)
just check           # rustfmt, strict Clippy, unit tests and real-CLI contract tests
just land            # gate exactly the described change, then move main (one land at a time)
```

- **Real execution required.** Transport changes need controls with real
  execution, not only unit tests: exact commits, refusal versus recipe exit,
  seat isolation, and session cleanup on client loss.
- **Fault injection.** Debug builds read `FERRY_FAIL_AT=<point>[,…]` to
  inject failures at fallible points; release builds never read it.
- **Process conventions.** `AGENTS.md` describes them: one jj workspace per
  agent, the design gate, and stopping processes only by exact pid.
- **No forge.** This repository is local-only; there is no published package
  or forge remote.

## Design and direction

- `PRINCIPLES.md`: what Ferry is and is not, and how design questions are
  decided.
- `design/`: dated design documents, with their reviews and evidence.
- **Target:** `SPEC.md` (normative end state, terse). **Rationale:**
  `design/2026-10-09-ferry-minimal.md`: Ferry does one
  thing (an exact commit's recipe on your machine, warm, alone if asked, with
  an honest record) and pushes containment strength to the OS and scheduling
  to repositories and CI. A run becomes a bounded, detached job:
  - a dropped connection never ends a run; its mandatory timeout or
    `ferry stop` does;
  - output is written to a file on the machine;
  - leases are held by the kernel;
  - there is one authoritative record per run.

  This README describes Ferry as built today; it will change as v3 lands.
- `RELEASE.md`: install, upgrade and rollback.
