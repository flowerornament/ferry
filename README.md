# Ferry

Run this repo's recipe at this commit on a machine that can run it, and tell
me exactly what happened.

Ferry separates the project from the machine. A project declares recipes and
their required features. You configure machines once. No machine configuration
means local execution; a configured machine missing required features refuses.

## Companion references

Recipes can request exact companion commits under fully qualified host Git refs.
A string source keeps the caller backend's revision syntax. A typed source is
portable across Git, Git worktrees and Git-backed jj:

```toml
[companion_refs]
'refs/remotes/origin/master' = { remote = 'origin', branch = 'master' }
```

This selects the caller's locally known tracking ref, not the local branch or
the remote's current state. Ferry does not fetch or fall back. The tracking ref
must directly name a commit, not a tag object. The caller receipt records the
requested `companion_sources` and resolved `companion_refs`; only those exact
objects are transported and materialized on the host. Old receipts without
`companion_sources` provide no typed tracking provenance.

## Install and run

Ferry installs through Nix on the caller and on every host, from one reviewed
commit, so each receipt can name the source and executable that ran on both
ends (`caller_source_rev`/`caller_exe`, `host_source_rev`/`host_exe`, beside
the binary SHA-256s). `scripts/ferry-install` is the supported procedure for
hosts and for checking the caller; RELEASE.md describes install, upgrade and
rollback. One crate builds one binary: `ferry run` is the caller and
`ferry host-run` receives the request. Git, SSH, and Unix process tools are
required; jj is optional for colocated repositories. A `cargo build` binary
works but reports `source_rev` as `unpinned`.

```
ferry recipes
ferry machines
ferry run smoke --on bench
ferry run test --local --json
ferry receipt <id>
```

There is no published package or forge remote yet. This repository remains
local-only.
Ferry supplies non-interactive SSH (`BatchMode=yes`) and connection sharing
(`ControlMaster=auto`, `ControlPersist=60`) itself, including Git transfers.
Its control sockets live in a private `~/.cache/ferry/ssh` directory.

## Project configuration

Commit `ferry.toml`:

```toml
version = 1

[recipe.test]
command = ["devenv", "shell", "--", "mix", "test"]
requires = ["linux-x86_64"]
routable = true

[recipe.mix-test]
command = ["mix", "test"]
requires = ["linux-x86_64"]
routable = true

[hooks]
prepare = "scripts/ferry-prepare"
env = "scripts/ferry-env"
collect = "scripts/ferry-collect"
```

If recipes already have a catalog, point to it instead of repeating them:

```toml
version = 1
[catalog]
path = ".config/murail-gates.toml"
table = "recipe"
```

Requirements and recipes are read from the named commit. Catalog entries must
provide explicit command argv; Ferry never infers a tool from a recipe name.
Ferry has no Nix-specific environment logic: the repo owns
its command and executable prepare/env hooks.
If an existing catalog has another schema, provide `[catalog] adapter =
"scripts/ferry-catalog"`. The committed executable runs caller-side in fresh
checkouts of the pinned commit and emits JSON `{"version":1,"recipe":{...}}`.
It may also emit a `mirror` list derived from the repo's existing lockfile.
It can declare `preserve = ["_build", "deps", ".devenv"]` for ignored build
directories. Ferry records the list and retains those directories across resets;
tracked files, HEAD and hidden index flags are still checked. Paths must be
literal, ignored directories containing no tracked files, never `.git`, `.jj`
or `.ferry`. The repository's prepare hook makes retained build state current.
Preserved state is a declared cache: a run can depend on earlier runs in the
same seat, not only on the requested commit. Omit preserve for a clean run.
Ferry checks repeatability, validates the entire contract before routing, and
records adapter and emitted-contract digests. The adapter must be a pure reader;
Ferry is not a sandbox that can prevent arbitrary side effects outside its
temporary checkout.
Only explicitly routable recipes go remote. Publishing and certifying recipes
must remain unroutable: Herald's `just land` stays local.

### Provider capabilities

A project can require capabilities of the Ferry that runs it. This keeps a
rollback, or an older binary on one end, from silently running without them:

```toml
[provider]
requires = ["admission"]
```

The table may sit in `ferry.toml` or in an adapter's emitted contract, but not
both. Capabilities are only ever added:

| capability | meaning |
| ---------- | ------- |
| `args` | recipe arguments after `--` |
| `refusal-event` | typed refusals before acceptance |
| `custody` | seat custody records at release |
| `lease` | the shared/exclusive host lease |
| `admission` | host policy admission (`host.toml`), even when not configured |

A run is refused (125), before any hook runs, in each of these cases:

- **Unknown capability:** the project requires a capability the caller does
  not provide, such as a misspelled name.
- **Host lacks it:** the host's `host-info` does not list every required
  capability. A host from before capabilities lists none. The receipt still
  names that host's binary.
- **Host refuses the request:** `host-run` refuses a request requiring
  something it lacks.

A caller from before this table rejects it as an unknown key. The receipt
records `provider_requires`.

## Machines

Create `$XDG_CONFIG_HOME/ferry/machines.toml` (default `~/.config/ferry`):

```toml
[host.bench]
ssh = "bench"
triple = "x86_64-unknown-linux-gnu"
cores = 16
features = ["linux", "x86_64", "quiet"]
```

Ferry chooses the first machine whose features contain every requirement.
`--on` names a configured machine; it does not bypass requirements. SSH aliases
come from your SSH configuration. Ferry does not copy credentials or provision
machines. Features are operator declarations, not hardware measurements or
proof that unrelated host work is absent.

### Host policy and admission

A host's operator may set a disk floor in `$XDG_CONFIG_HOME/ferry/host.toml`
(default `~/.config/ferry/host.toml`) on the machine that runs sessions. Each
session is admitted under its seat lock, after the host lease, before
checkout and hooks:

```toml
# Version 1: a guard. Below the floor, refuse (125).
version = 1
min_free_bytes = 42949672960

# Version 2: below the floor, first reclaim named targets of idle seats.
version = 2
min_free_bytes = 42949672960
evict = { mode = "idle-seat-targets", targets = ["cargo-target"] }
```

- **No file:** the run proceeds, and the receipt records the policy as
  `not-configured`, with no floor claimed.
- **Malformed policy:** a malformed file, another version, an unknown key, an
  `evict` block in version 1, or a target that is not a plain name refuses
  before anything is measured.
- **Version 2 eviction:** below the floor, version 2 removes only
  `caches/<seat>/<target>` for the listed targets. It works through seats
  oldest release first, until the floor is met.
- **Eligible seats:** a seat is eligible only if all of these hold:
  - it has a custody `idle` record;
  - it has no taint and no owner marker, and both are confirmed absent (an
    unreadable marker means unknown, and the seat is skipped);
  - its lock can be taken without waiting;
  - its idle record is unchanged under that lock.
- **Path checks:** every directory from the state root down to the target must
  be a real directory, and every entry inside the target must be on the state
  root's filesystem and owned by the state root's owner. A symlinked target, or a mount
  inside one, makes the seat ineligible. Removal never follows symlinks.
- **Never removed:** checkouts, receipts, records, logs, other cache
  contents, seats without an idle record, and anything outside
  `<state>/caches`.

If eligible seats run out while still below the floor, the run is refused
(125). The evictions already made stand.

The receipt and the host result carry `admission`, as far as the decision got:

| field | meaning |
| ----- | ------- |
| `policy` | `{path, sha256, version}` of the file as read, or `"not-configured"` |
| `floor_bytes` | the floor; null when not configured or refused before reading it |
| `filesystem` | `{path, device}` of the state root that was measured |
| `available_before`, `available_after` | bytes available to this user (statvfs), measured |
| `evicted` | for each removed target: `seat`, `released_ms`, exact `path`, `logical_bytes` (sum of file lengths), `allocated_bytes` (allocated blocks, each inode once), and the measured `available_before`/`available_after` around it |
| `skipped` | eligible-looking seats passed over: `locked`, `owners`, `tainted`, `no_idle_record`, `changed`, `unsafe_path`, and `unknown` (a custody marker could not be read) |
| `result` | `admitted` or `refused` |

Measured availability can move for reasons outside Ferry. The difference
across a removal is an observation, not a claim about what that removal freed.
There is no collector, timer or service: reclamation happens only inside an
admitting session.

## Hooks

The optional `environment` declaration contains `forwards` (caller variables
sent to the recipe), `watches` (prefixes whose unlisted nonempty variables
refuse before any push), and `local` (variables deliberately left on the
caller). A catalog adapter may emit this declaration from the repo's existing
catalog; it must not also be declared in `ferry.toml`. Receipts record the
selected values in `forwarded_env`, so never include secrets in `forwards`.
The host env hook cannot replace a forwarded value.

Hooks are committed executable paths relative to the exact checkout. They run
inside the owned recipe session: prepare, env, recipe, collect. The env hook
prints a JSON object of environment-variable strings, never shell code.
Ferry reserves `FERRY_*` and `GIT_CONFIG_GLOBAL`; an env hook cannot replace
run identity or the seat-local mirror configuration.
Inputs include `FERRY_SEAT`, `FERRY_COMMIT`, `FERRY_ID`, `FERRY_WORKDIR`
and `FERRY_CACHE_DIR`; collect also receives `FERRY_EXIT_CODE`. Projects own
their database preparation and artifact collection.

`FERRY_CACHE_DIR` is a per-seat directory that Ferry owns
(`<state>/caches/<seat>`, mode 0700). Put regenerable build output there, for
example `CARGO_TARGET_DIR=$FERRY_CACHE_DIR/cargo-target`. Under an explicit
host policy, Ferry may later reclaim named subdirectories of it from idle
seats. It never reclaims anything else.

### Host lease

Every session holds a host-wide lease while it runs. Ordinary runs share it.
`ferry run --exclusive`, or a recipe declaring `requires = ["exclusive"]`,
holds it alone, which is useful for timing measurements:

1. The run first closes a gate, so no new run starts.
2. It waits for in-flight runs to release their shared side.
3. Then it runs alone.

New runs pass the gate briefly before taking their shared side, so once the
exclusive run holds the gate, later runs cannot extend its wait. Before that,
it competes for the gate like any other run: there is no queue. Each receipt records `lease.mode` (`shared`
or `exclusive`) and `lease.wait_ms`. `exclusive` is an execution lock, not a
machine feature: routing ignores it, and receipts list it under
`requirements`.

`exclusive` means no other live Ferry session under the same state root: one
user's Ferry on one host, or one repository's `.ferry/local`. It is not a
quiet-host certificate. A measurement should still account for host load and
custody. It does not quiet:

- descendants that outlived an earlier session's supervisor. They hold no
  lease; the seat's custody taint or owner marker is the evidence for them;
- processes started outside Ferry;
- other users;
- other virtual machines on the same hardware;
- runs by a Ferry that predates the lease. An older binary takes the
  `locks/host-exclusive` file only for its own exclusive runs, so old and new
  exclusive runs still exclude each other. Older ordinary runs take no lease
  at all. Install the lease on every caller and host before relying on it.

An exclusive run waits for the longest in-flight run under its state root.
That wait is not bounded by `--timeout-seconds`; interrupt the caller to give
up. The lease record also carries `acquired_ms` and `released_ms`, so a timing
consumer can compare the window it held with its own load observations.

### Seat custody

When a session releases its seat, the receipt's `custody` records what the
session left behind:

- `census`, taken after the collect hook, is one of:
  - `empty`: no live descendant of the run's supervisor;
  - `survivors`, with their pids;
  - `unknown`: the census could not be taken;
  - `unsupported`.
- `stopped`: the survivors this release stopped, and `after`: the census
  taken after stopping them.
- `record` is one of:
  - `idle`: written only after ran-pass, ran-fail or timeout, when the
    census, or the census after stopping, is empty;
  - `taint`: survivors remain after stopping, or the census is unknown;
  - `none`.

**Survivors are stopped.** A run owns what it started, and nothing it
started outlives it. At release, each survivor the census attributes to this
run gets SIGTERM, together with all of its descendants, and so does any
process group one of them leads. For 10 seconds the census is retaken, and
descendants adopted meanwhile get SIGTERM too. Then whatever remains gets
SIGKILL, and a final census is taken. Survivors are this supervisor's
unreaped children, so their pids cannot have been reused. A deeper
descendant is signalled only while its start time still matches.

Release always happens. Once the session is spawned, every path runs the
collect hook, then the census and stop, then records custody, before the
locks are dropped. That holds even when a hook, the session or collection
fails.

The supervisor is always a freshly spawned `ferry host-run`: over SSH for a
remote run, and as a child of the caller for `--local`, on the repository's
`.ferry/local` state, over the same protocol. Having just started, it has no
children of its own, so nothing outside the run can be adopted and
attributed to it. A wrapper's background job, or that job's orphans, stays
the wrapper's. The local host-run runs in its own process group, so a
signal or kill aimed at the caller's group (a harness, a terminal) does
not reach it. It learns of a lost caller from its heartbeat. Killing that host-run (its pid is in the `accepted` event)
is killing the supervisor. Killing the caller instead ends its heartbeats,
so host-run stops the work and releases.

If release itself fails (a record cannot be written), the work's verdict
and exit code stand. `custody.error` says why, custody counts as unknown,
and the seat is tainted if that can still be written.
A service a run starts and expects to outlive it belongs to the operator's
service manager, not to the run (PRINCIPLES.md, worked example).

On Linux the supervisor is a child subreaper. A process that leaves the
recipe's session (`setsid`, double fork, closed descriptors, a scrubbed
environment) is reparented to it rather than to init, so it still counts. A
session deletes its seat's idle record as soon as it holds the seat lock.

What custody does not see:

- processes the session did not start (pre-existing daemons, `systemd-run`,
  SSH to self, cron);
- other users;
- other hosts;
- a supervisor that dies before release. It records nothing and stops
  nothing, and its owner marker keeps the seat refused until
  `ferry seat recover`.

**A tainted seat refuses** new runs (125) with the run and pids that tainted
it, before waiting for the seat lock and again while waiting, since a
survivor may still hold that lock. The next step is to stop those
processes, then run `ferry seat recover <seat>` on the host that ran it.
The refusal names the command, the host, and your `--seat`. A taint record
names every process in the signalled subtree, by pid and start time, so
recovery checks them all. It also names the locks those processes may still
hold: the seat, and the host lease in the run's mode (an exclusive run also
held the gate). While any seat is tainted, an `--exclusive` run refuses. A
run that needs the lease also refuses, before its first try, when a taint
says its survivors may hold it exclusively or hold the gate, or when a
taint cannot be read. Any run that waits more than 3 seconds for a lock prints one
line saying so.

Recovery never kills. It refuses while the seat lock is held, and clears the
taint only when every recorded survivor is gone, checked by pid and start
time. Some taints cannot be proven clear: one that names no survivors
(because the census was unknown or attributed no pids), or a record that
cannot be read. For those, the operator confirms nothing is left and runs
`ferry seat recover <seat> --accept-unknown --reason '<why>'`. That reason
is kept in the recover receipt. A free seat lock shows only that no
process still holds it. A survivor that closed its descriptors may still
live, so this is the operator's call, not Ferry's.

**Other platforms** report `unsupported`: there is no census, so nothing is
stopped at release and the seat is never recorded idle. The session teardown
still kills every member of the recipe's session. A descendant that leaves
the session (`setsid`, a double fork) escapes there, and this is reported
rather than hidden.

**Locks outlive a killed supervisor, on Linux.** There the seat lock and the
host lease are inherited by the session and the recipe. If the supervisor
(`host-run`, remote or local) is killed, they stay held until the
recipe's processes that kept them exit, and a new run waits instead of
starting beside a live recipe. A survivor that kept them also keeps them,
until it is stopped at release or, if it cannot be, the seat is tainted.
Elsewhere, survivors cannot be stopped, so the locks are not inherited:
a killed supervisor frees them while the recipe lives. The dead
supervisor's owner marker still refuses that seat until recovery, but an
`--exclusive` run on another seat is not kept out.

## Safety and receipts

Remote transport pushes only the exact commit to the machine's private bare
repository under `refs/ferry/<seat>/<sha>`, never to origin or master. A seat
lock protects a detached checkout. HEAD, clean tree, and hidden index flags are
checked before execution. A unique run token binds host facts to client output.
Client loss terminates the recipe session; children creating new sessions can
escape that boundary and require stronger host isolation. This is not a sandbox.
Unconfirmed session cleanup quarantines the host with a `cleanup-pending-*`
marker in its lock directory. Further dispatch refuses until an operator confirms
the session is gone and removes that marker; Ferry never clears it by age.
An execution error after recipe dispatch is `lost`, not a pre-execution refusal.

Local execution uses the same locked, clean detached checkout mechanism, not
the caller's dirty tree.
Receipts live in `.ferry/receipts/`. Machine timings must not be pooled across
platforms. A remote result is evidence, not automatic publication authority.

`--json` emits version-one NDJSON with prepared, output and receipt events.
The host also emits `accepted` (with the supervisor's `pid`) and a bound
`host-result`. Before acceptance it may instead emit `refused` with a
reason. Output is escaped as data: an `output` event carries the line as
`text`, and a line that is not UTF-8 also as `base64`. Without `--json`
the caller writes those exact bytes, so output is byte-exact either way.

After acceptance three rules hold:

- **Output is data, never control.** Recipe output and events travel to
  the client through one writer thread and a bounded queue.
  - A slow reader slows the recipe through backpressure, never the
    supervisor.
  - A stream that cannot be written, such as `ferry run … | head -1` or a
    closed stderr, is dropped and its lost lines counted in the receipt's
    `output_dropped` (`stdout`, `stderr`, or `host` for lines the host could
    not deliver). The run goes on to its own end or its timeout.
  - Live delivery is bounded and counted; the output log is the complete
    account. Every line the recipe wrote is in the host's output log
    (`.ferry/local/runs/<id>/output.log` for a local run). Each line is
    counted once: delivered (`output_delivered`) or dropped
    (`output_dropped`). The host reports how many lines it read, and lines
    this caller never received count as `host`.
  - Lines longer than 1 MiB are split at 1 MiB. A split, and a final line
    without a newline, each arrive with an added newline; the output log
    has the same lines.
  - A line whose JSON escaping would more than double it (NUL and other
    control bytes) travels as `base64` only. Callers from before `base64`
    print nothing for it.
  - Only signals, the client's heartbeat ending (stdin EOF: "client lost"),
    and the timeout stop a run.
- **The verdict is decided once, by the first terminal fact.**
  - A recipe that reached its result keeps ran-pass, ran-fail or timeout,
    with its own code or 124. Anything after it (a lost client, a signal
    during the collect hook, a failed release) is noted in `reason` as
    "after the verdict: …".
  - A run is lost only when a stop came first: 128 plus the signal when one
    was caught, else 255. The reason always names the cause ("caller
    received SIGINT", "client lost: heartbeat EOF").
  - A failed diagnostic is recorded in `reason` and leaves the outcome and
    exit code as they were. This covers writing the collection result, the
    artifact digest, the host record and the output log.
- **The supervisor never waits on data, and termination wins.** Its loop
  checks the recipe's result, stop and the timeout every tick, in that
  order.
  - After custody release it reads the output to the end and delivers it.
    It stops after 15 seconds without progress, after 60 seconds in all, or
    on a stop, whichever comes first, and counts what was left.
  - A consumer slower than that gets drops, counted in `output_dropped`.
    The output log still has every line.
  - A survivor that writes to the output cannot keep a run alive past the
    cap.
  - Then the supervisor writes its record, with its line count, and sends
    `host-result` last. Its events are always attempted, even when output
    was given up. If `host-result` does not reach the client within 5
    seconds, or arrives truncated, the caller reads the verdict from the
    host record (locally, or over SSH with connection bounds). It accepts
    only a final record, and the receipt says so.
  - Both queues are bounded at 8 MiB as well as in lines.

If the local caller fails before the run ends, it closes the supervisor's
input (its heartbeats stop). It then gives the supervisor longer than its
worst-case stop: session teardown, the collect hook, stopping survivors and
the final emit. Only after that does it kill the supervisor, and it says so.

Debug builds take `FERRY_FAIL_AT=<point>[,<point>…]` (`collection-write`,
`artifact-digest`, `output-write`, `run-panic`, `session-cleanup`,
`stop-survivors`) to inject those failures in
controls. Release builds never read it.

### Outcomes and exit codes

Read the receipt's `outcome`, not a recipe's printed text, to tell a Ferry
refusal from a recipe failure. A recipe may itself exit 125.

| outcome    | `ferry run` exits | meaning |
| ---------- | ----------------- | ------- |
| `ran-pass` | 0 | The recipe ran and exited 0. |
| `ran-fail` | the recipe's code | The recipe ran and failed. |
| `timeout`  | 124 | `--timeout-seconds` elapsed; the recipe's session was ended. |
| `refused`  | 125 | Nothing ran. This covers an unknown recipe or route, an unmet requirement, a dirty or unknown commit, and a host that refused before acceptance (including a host older than the caller that rejects the request). |
| `lost`     | 128 + signal, or 255 | The run's fate is unknown or was cut short. When the caller is interrupted, it exits 128 plus the signal it received: 130 for SIGINT, 143 for SIGTERM, 129 for SIGHUP. A signal that arrives before dispatch still writes a receipt and dispatches nothing; the caller first lets an in-flight step, such as a git push, finish. Otherwise, lost means a stop came before the recipe's result, and the host lost its client or failed after dispatch: 255. The reason names the cause. A recipe that already reached its result is never lost; later events are noted. |

## Limits

SIGKILL of `host-run` leaves the recipe's session running. On Linux it keeps
the seat lock and host lease it inherited until it exits; elsewhere they are
freed at once. Its durable ownership marker
then keeps that seat refused until recovery; cgroups are the later fix.
After the recipe and collect hook finish, the session leader exits if its
supervisor is gone. Background descendants may still require manual cleanup;
recovery reports their PIDs and never kills them.
The refusal names the internal seat and the exact `ferry seat recover <seat>`
command to run on that host (`--local` for a local run). Recovery takes the seat
lock, compares live session members against the leader's recorded start time,
and clears the marker only when the session is empty. It writes a receipt and
never kills processes. If death happened before the session birth was recorded,
recovery refuses rather than guessing; another logical `--seat` remains usable.

## Development

`just check` runs formatting, strict Clippy, unit and real-CLI controls.
Transport review also requires actual remote execution: unknown/conflicted
commit, tampered checkout, recipe failure, repeated IDs, and client disconnect.
Unit tests alone do not establish those properties. Current v0 work still
includes seat listing/GC, richer evidence and adversarial remote disconnect
controls. Mirrors use seat-local Git configuration, and declared artifact globs
return under `.ferry/runs/<id>/`; filesystem links refuse rather than copy
external bytes. See RELEASE.md for the unpublished release contract.
