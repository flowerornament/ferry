# Ferry specification

**Normative end state.** The code converges onto this document slice by
slice; `README.md` and `REFERENCE.md` describe the behaviour shipped today. The rationale and its
evidence are in `design/2026-10-09-ferry-minimal.md`, `PRINCIPLES.md` and the
earlier design documents. "Why" lines give the reason in one sentence.

## 1. Purpose

> Run this repository's recipe, at this exact commit (even unpushed), on a
> machine you own, in a warm checkout, alone if asked, and give an honest
> record.

**Why:** CI tools assume a forge, a server and a queue, and Nix builds assume
hermetic inputs. Agents sharing owned machines need exact, warm, serverless
runs. Ferry does only that.

## 2. Boundaries

```
above (repos / CI)   what and when · fan-out · retries · gating · landing · land locks
FERRY                ship · seat · lock · run · record
below (OS/operator)  flock · sessions · systemd scope (opt.) · services · disk reclaim · Nix installs
```

Ferry never schedules, queues, retries, supervises services, sandboxes,
publishes, reclaims disk beyond its own bounds, or proves that nothing
survived (PRINCIPLES). Its own bounds: a seat is retired when its caller's
workspace is gone, when the caller declares it idle, or after a `--once`
run, and a seat's caches are capped (§9).

## 3. Concepts

| Concept | State | Rule | Why |
|---|---|---|---|
| **Recipe** | `name → {argv, needs, exclusive, timeout, wait, artifacts, keep_between_runs}` read from the commit | runs only where `needs ⊆ has`, on every route | the repository decides what may run |
| **Place** | `local` or an ssh destination; `has` = declared (`machines.toml`) ∪ detected ∪ the binary's capabilities; `host.toml` | describes itself through the binary that will run the work | the facts come from the doer |
| **Seat** | `(caller, repo path, name) → checkout + cache` | reset to the commit each run; `keep_between_runs` dirs kept | warm and exact, never shared |
| **Lock** | flock: `seat.<key>`, `host`, `host.gate` | held means busy; the work inherits the seat's fd only, the supervisor alone holds `host` and `host.gate`; release by `close()` | the kernel is the only judge, and a leftover can hold no more than its own seat |
| **Run** | `id → phase, deadline` | detached from the client; ends by its deadline or `stop` | its lifetime is not the connection's |
| **Record** | `<id>.reserved.json` → `<id>.json` | reserved before any effect, final exactly once | one honest account per attempt |

## 4. Interface

```
ferry run <recipe> [--on <place>] [--exclusive] [--seat <name>] [--once] [--commit <rev>]
          [--timeout <dur>] [--wait <dur>] [--detach] [--json] [-- <args>…]
ferry follow <run>                      re-attach: tail the output until the record is final
ferry stop <run> [--other --reason <why>]
ferry record [<run> | last] [--json]
ferry status [--on <place>] [--json]   what the place sees now (read-only)
ferry check  [--on <place>]
ferry seats prune [--on <place>] [--idle <dur>]
                                        retire this caller's seats whose workspace is gone
                                        (and, with --idle, those unused for <dur>)
ferry recipes · ferry places
```

Not shipped yet: `--timeout <dur>` and `--wait <dur>` (today
`--timeout-seconds` and `--wait-seconds`), `--detach`, `ferry check`,
`ferry places` and `record last`. `README.md` lists the commands as built.

**`run` behaviour:**

- **Default:** submit, then follow to the end. It exits with the record's code
  and prints the run id.
- **`--detach`:** submit, print the id, exit 0.
- **SIGINT, SIGTERM or SIGHUP:** request `stop`, then keep following. If the
  stop cannot be delivered, print "not stopped; continues until <deadline>;
  ferry stop <id>" and exit 255.
- **Losing the connection:** reconnect and resume from the offset, until the
  deadline. When that fails, exit 255 with the next step `ferry record <id>`.

**Exit codes** (the record's `outcome`):

| Outcome | Code |
|---|---|
| `ran-pass` | 0 |
| `ran-fail` | the recipe's code |
| `timeout` | 124 |
| `refused` | 125 (nothing ran) |
| `stopped` | 128+signal |
| `lost` | 255 (the supervisor died; reconciled) |

**`record` exit codes:** a final record exits with its outcome's code, as
above. A run with no verdict yet (state `submitted`) exits 3, meaning poll
again or `ferry follow <id>`.

**Why:** a caller can always tell its own code from Ferry's refusal from
infrastructure loss, by the record's `outcome`. Exit codes are for people:
a recipe's own code can coincide with 3, 124, 125 or 255, so a program
decides by `--json` (`state`, then `outcome`), never by the exit code alone.

**`status`:** one read of the place's state root and process table, with no
lock taken and nothing changed. It shows:

- **Disk:** available bytes, the floor and the seat cap.
- **Live runs:** each record that is reserved but not final, with its
  supervisor alive. For each: id, seat, recipe, exclusive or shared, phase
  (below), start and deadline.
- **Seats:** each `seats/<key>/` with its size, its last run and that run's
  outcome. The caller labels a seat with its workspace from its own
  `seats.jsonl`; other callers' seats show their key only.
- **Leftovers:** processes of the place's user that carry a `FERRY_ID` whose
  record under this state root is final, with pid, start time, run, seat and
  command (§7). A marked process with no record here (another state root,
  such as a local run or a test's temporary root) is shown apart as
  `record: none`, not as a leftover. Unreadable processes are counted.
- **Phase** of a live run, from files that already exist: `waiting` (no
  `output.log` yet: locks, admission, checkout, objects), `preparing` (the
  log, no `result.attempted`: hooks), `running` (attempted).
- **The gate.** `host.gate` names the last exclusive run that took it; the
  name is not cleared. `status` calls that run the gate's holder only when
  its reservation's supervisor (pid and start time) is alive; otherwise the
  holder is `unknown`, never a stale name. Every other live exclusive run is
  a `requester`, waiting for the gate or the host. Exclusive means "no other
  Ferry run here", not a quiet host and not the absence of an earlier run's
  leftovers.

It probes no lock: a probe could make a starting run's seat look busy. It is
a report, never a gate input. It stops nothing; a leftover is stopped by
its exact pid, by the operator.

**Why:** "who holds the gate, why is this seat busy, what is idle, what
outlived its run" must each be one command, and the answers already exist
in records, run directories and the marker.

**Refusals (125)** each print `what / why / next`:

- unknown recipe or place;
- unmet requirement;
- unknown or dirty commit;
- seat busy (`run <id>`, its age, then `ferry follow`/`stop`);
- host wait expired (naming the holder);
- below the disk floor;
- nested run on the same place;
- timeout above the host's `max_run_time`;
- capability missing.

## 5. Run lifecycle

```
caller:  check recipe@commit ─refuse─► 125 receipt
         push objects (idempotent) ─► submit{id,request}
place:   reserve record (O_EXCL; a duplicate id → follow the existing run) ─► fork supervisor (setsid, no tty) ─► reply id
super:   seat lock (try; held → refuse busy)
         host lease (shared|exclusive; bounded wait; names holder)
         floor check (host.toml) ─► reset checkout (keeping kept dirs) ─► objects
         hooks: prepare → env → recipe argv+args → collect     stdout+stderr → output.log
         deadline = submit + wait + timeout + grace, absolute, checked every phase
         end/timeout/stop: stop session ─► collect (own session, stopped) ─► marker pass ─► artifacts ─► final record ─► close locks ─► exit
caller:  follow output.log by offset until record final ─► receipt ─► exit code
```

**Why:** each step either rests on one mechanism or is reported. No step
depends on the client being present.

**The verdict is the first terminal fact** the supervisor observes: the
recipe's exit (`ran-pass` or `ran-fail`), a stop (`stopped`) or the deadline
(`timeout`). A later fact never changes it; a stop that arrives after the exit
is a note. A session that ends with none of these before the recipe was
attempted is `refused` (125), and after it `lost` (255), with the session's
own error as the reason when it left one (a session killed from outside
leaves none, and the reason says so).

## 6. Protocol (caller ↔ place, over ssh or a local exec)

| Verb | In | Out |
|---|---|---|
| `describe` | — | `{protocol, source_rev, exe, capabilities, boot_id, os, arch}` |
| `submit` | request JSON (`id`, `token`, `commit`, `seat`, `recipe`, `args`, `env`, `hooks`, `deadline`) | `{id}` or `{refused, why, next}` |
| `follow` | `id`, `offset` | output bytes from that offset; ends when the record is final |
| `retire` | seat `key`; optionally `idle_ms` or `once_id` (neither for a workspace-gone prune: no `used` check) | per seat: `{retired, bytes}` (under a shared host lease and the seat lock, try only), `{refused: exclusive}`, `{refused: busy}`, `{refused: used}` (used since), `{unknown, why}` or `{failed, why}` |
| `stop` | `id`, `who`, `why` | `{accepted}` or `{refused}` |
| `record` | `id` | the final or reserved record |

- **Capabilities:** a new caller uses this protocol only if the place
  advertises `detach`. Otherwise it refuses with 125, naming the place's
  `source_rev` and the next step (install the caller's build on that place).
  There is no legacy `host-run` shim: a caller is never installed before
  its places, and both move in one announced window (decided 2026-10-10
  on ferry-ljc.3; it supersedes the shim in ferry-minimal §7).
- **Version skew:** old and new binaries never share a state root: the
  current root is a new path (`…/ferry/v1`) that earlier binaries never
  read. A new
  caller refuses a place without `detach`/`ship` (125, naming its revision)
  before pushing anything; an old caller meets a place that still answers
  its host-info and is refused at parse (125, naming the place's revision).
- **Caller limits before submit:** short replies from the place are
  bounded and the push has an output watchdog, so a stalled link ends
  in a refusal (125, nothing run, no host record; the caller's receipt
  says refused) rather than a hung caller. Local commands on the caller
  (the repository, the catalog adapter) are not covered:
  - every ssh carries a connect timeout and keepalives;
  - short replies (`host-info`, file reads, the store setup and the
    `ls-remote` check after a failed push) have the reply bound, 60 s;
  - the object push has no wall-clock bound, so a push that keeps
    printing progress is never cut. It is bounded by its progress output:
    the caller runs `git push --progress` in a process group of its own,
    and 60 s without a new byte on its stderr (git's lines or the place's
    `remote:` lines) ends it. The caller stops that group, which it
    recorded, and refuses with "the push printed no progress for 60 s".
    Output is a proxy for progress, not proof of it. Residue, either way:
    a push stuck while something keeps printing (a remote hook's periodic
    text) is not cut, since before submit no run deadline exists yet; real
    work that stays silent for 60 s (on either side) is refused.

**Why:** short idempotent calls survive dropped connections; long-lived
streams do not.

## 7. Custody

```
at end:   stop session (TERM → grace 10 s → KILL)      recipe's, then collect's
          one marker pass: processes carrying FERRY_ID=<this run>
record:   custody = { tier: "session",
                      survivors: [{pid, start, command}] | "unknown" }
```

- **The marker.** Every hook, the recipe and everything they start inherit
  `FERRY_ID=<run id>`; collect gets the same id as the recipe (§10). After
  the session stops, one pass reads the exec-time environment of the place
  user's processes, in-process with no subprocess (`/proc/<pid>/environ`
  on Linux, `sysctl KERN_PROCARGS2` on macOS), and lists those that still
  carry this run's id. That finds a process however it left the session
  (setsid, double fork, re-parented to init or launchd), and needs no setup
  or privilege. `survivors` is that list merged, by pid and start time, with
  any session member still alive after the KILL. The pass reports; it never
  stops or retries.
- **Complete or unknown.** Only a process born at or after the run's start
  can be its descendant; older ones are skipped. An unreadable process is
  also skipped when its parent is neither pid 1 nor a process of the place's
  user, and only when that is observed coherently: the child's parent pid
  and start time, and the parent's uid and start time, are read and still
  match after the read. A parent that cannot be read, has gone, was reused or
  changed during the read leaves the process unreadable, so the pass is
  `"unknown"`. The reasoning: a process the run started, keeping the place
  user's credentials all the way down, has as parent another one it started,
  pid 1, or a same-user subreaper (such as `systemd --user`). This keeps the
  user's own `sshd-session` per login (non-dumpable, its parent root's
  `sshd`) from making every Linux pass unknown. Residue: an unreadable
  escapee whose current parent has another uid is not listed. That covers a
  root-owned subreaper other than pid 1 (some container inits) and any
  credential-changing helper in its ancestry (`sudo`, a setuid program). So
  the list is complete for the run's descendants that kept the user's
  credentials, not proof of all descendants. The list is complete only
  when the pass read the marker of every such live process. If listing
  fails, if a live eligible process's environment cannot be read, or if a
  pid's start time changes while it is read, `survivors` is `"unknown"`,
  never a list, and `custody.error` names what was found and what could not
  be read. A process that exited during the pass is skipped. `"unknown"` is
  a report, never a refusal.
- **Strength by OS (PRINCIPLES 9).** Linux reads any same-user process
  except a non-dumpable one. macOS will not show the environment of Apple's
  own platform binaries (`/bin/sh`, `/bin/sleep`, `git`, the agents launchd
  starts for the user): the read succeeds with argv and no environment, and
  that counts as unreadable, never as "no marker". On a shared Mac some such
  process of the user is born during almost every run, so a Mac place
  records `"unknown"` nearly always. That is the honest Mac strength;
  `status` still lists the leftovers it can read. Acceptance on macOS is
  "listed or `"unknown"`, never `[]`"; on Linux it is exact.
- **Locks.** Only the seat's lock is inherited. A survivor that keeps it
  holds the seat busy, and the next run's refusal names the run it outlived.
  It cannot hold `host` or `host.gate`, so it can never close the place to
  other seats. The cost: a survivor of an exclusive run can overlap the next
  run unseen by the lease. The record lists it.
- **Residue.** A process that drops or rewrites its environment (for
  example `env -i`) carries no marker and is outside Ferry's knowledge;
  `survivors` lists only what the marker shows. A process that also closed
  its fds holds nothing, and the next run is admitted.

**Responsibility for what a run starts:**

| Party | Owns |
|---|---|
| Ferry | the run's session: stop it at the end, the deadline or a stop; list marked survivors in the record and in `status`; let a survivor hold only its own seat |
| The project's committed recipes and hooks | everything they start: it stays in the run's session (no `-d`, `nohup`, `setsid`, `daemon()`), or the project's collect hook stops it; the marker is never removed |
| The operator | a survivor that broke that contract: stop it by exact pid; host policy |

**Why:** proving absence cost more machinery than everything else combined
(ten critic rounds), and containment (a cgroup, a user per run) is
different on each OS and needs host setup. A marker pass works the same on
macOS and Linux, catches the escapes seen so far, and only reports. On
2026-10-13 a stopped run's detached devenv Postgres held its seat and the
shared host lease for 16 hours while its record said `survivors: []`; an
exclusive request then queued behind it closed the shared build host to
every seat. A
stronger OS tier stays possible (PRINCIPLES 6) and is not planned.

## 8. Records and receipts

```
records/<id>.reserved.json  {state:reserved, id, token, commit, seat, binary_sha256,
                            supervisor:{pid,start}, boot_id, submitted_ms}
records/<id>.json           reserved fields + {outcome, exit_code, reason, commit, seat,
                            recipe, args, place:{source_rev, exe, sha256}, lease:{mode, wait_ms,
                            acquired_ms, released_ms}, admission:{floor, available}, custody,
                            output:{bytes, capped}, artifacts:[{path, sha256}], artifacts_error?, stopped_by?, deadline}
```

- **Finalize:** write a temporary file, fsync, `link`/no-replace rename into
  place, fsync the directory, remove the reservation.
- **Reconcile:** a reservation whose boot id differs, whose pid is dead, or
  whose start time does not match is finalized as `lost` ("supervisor gone"),
  exactly once.
- **Receipt** (`<repo>/.ferry/receipts/<id>.json`): written at submit with
  the caller's facts (args, environment, adapter and contract digests, caller
  exe) and `state: submitted` (it reads lost/255, "no verdict yet", until
  final). It is replaced atomically by `state: final`, which carries the final
  record's verdict and evidence fields in the receipt's own schema. A
  receipt with no `state` is final.
- **Artifacts:** the place hashes each collected file into `artifacts`. The
  caller unpacks into an emptied `<repo>/.ferry/runs/<id>/`, verifies every
  file against that list and refuses any file not in it. The receipt carries
  the verified `artifacts:[{path, sha256}]` and `artifacts_status`: `none` (the
  recipe declares none), `fetched` (every declared file is present and
  verified), or `failed` with a kind and path: collection failed, a declared
  pattern matched nothing, the transfer failed, a digest differed, or the
  archive held a missing, unlisted or unsafe entry. The
  status is separate from the verdict: a `ran-pass` with `failed` artifacts
  is still a pass, and a consumer that needs the files reads the status, never
  the reason text or files already on disk.
- **Live supervisor:** a pid with no recorded start time is treated as alive;
  only proof reconciles.

**Why:** reserve-before-effect, final once and reconcile make "zero or two
records" impossible.

## 9. State (per machine, one root)

```
<state>/ (default ~/.local/state/ferry/v1)    lifetime
  locks/      seat.<key> host host.gate       —
  seats/<key>/checkout/ seats/<key>/cache/    cache  (bounded by Ferry: see below)
  seats/<key>/measured                        cache  (the cap's last measurement)
  seats/<key>/used                            cache  (`{ms, id}` of the seat's latest run)
  seats/.retired-<key>-<ms>/                  cache  (a retire's tombstone, until deleted)
  runs/<id>/  request.json output.log artifacts.tar  record (with result.json, result.error and stop.json)
  records/    <id>.reserved.json <id>.json    record (until the host's run-history limit)
  store/      repo.git                        cache
<repo>/.ferry/receipts/                       record (the caller's)
<repo>/.ferry/runs/<id>/                      the run's fetched artifacts (the caller's)
<repo>/.ferry/local/v1/                       the local route's <state>
```

Nothing else is written; Ferry writes nowhere outside these.

**Bounds on seats (Ferry's own state):**

- **Count:** a seat lives as long as its caller says it does. Each run
  appends `{key, place, repo_path, seat, last_used_ms}` to the caller's
  `seats.jsonl`. A seat ends in one of three ways, all declared by the
  caller:
  - **workspace gone:** `ferry seats prune` retires each seat whose
    `repo_path` no longer exists;
  - **idle:** `ferry seats prune --idle <dur>` also retires each seat whose
    newest `last_used_ms` (over its lines with the same `place` and `key`)
    is older than `<dur>`. `last_used_ms` is when this caller last used
    the seat, not a claim that no one else has; the try-lock is the guard
    against live use. A line without `last_used_ms` has an unknown age: the
    seat is kept and reported, never treated as idle;
  - **idle, without remembering:** with `delete_unused_checkouts_after_days
    = <n>` at the top level of the caller's `machines.toml` (a whole number
    of days, at least 1; anything else is a bad `machines.toml`), each
    `ferry run`, once its run is final (or submitted, with `--detach`),
    also runs `prune --idle <n>d` against that run's place, at most once an
    hour per place. The hour is claimed before pruning, under an flock on
    the caller's `pruned.json` (`{<place>: ms}`): a run reads it, and only
    if the place's time is over an hour old writes now and prunes; a run
    that finds the lock held or the time recent skips silently. The lock
    is a sibling `pruned.lock`, so `pruned.json` can be replaced atomically
    under it. After ten seconds it starts no new retire. One already in
    flight is waited for: over ssh up to the ssh reply bound (60 s), on the
    local route until that seat's delete ends. Stopping the wait never
    stops the retire, which the place completes or leaves as a tombstone
    (below). So the caller's exit, after its receipt and `--json` output
    are complete, is delayed by at most ten seconds plus one retire, at
    most once an hour per place. It prints one line on stderr
    naming the place, what it retired and how long it took; it never
    changes the run's exit code (a failing recipe's included) or its
    `--json` output, and a failure is only that line. It is the same
    prune, through the same `retire`, so a busy or recently used seat is
    kept. Without the key nothing prunes by itself;
  - **one run:** `ferry run --once` declares the seat lives for that run
    only. After the final record, the supervisor closes its seat fd (never
    `LOCK_UN`: the lock belongs to the open file description it shares
    with everything that inherited it) and then retires the seat like any
    other, through a fresh try-lock with `once_id` set to its run, after
    its own host lease is released (so an exclusive `--once` run never
    refuses its own retire). An exclusive run waiting at the gate then
    makes it `exclusive`, and the seat waits for a later prune.
    A survivor that inherited the seat's fd still holds the lock (§7), so
    the seat stays, busy, until that process exits and a later prune
    retires it. The record is final first, so it says `once`, never that
    the delete happened; a retire that fails or is skipped leaves the seat
    for `status` to show and a later prune to retire.

  Retiring, under the lock, renames `seats/<key>/` to
  `seats/.retired-<key>-<ms>/` (atomic, same device), writes
  `records/retired-<key>.json`, then deletes the tombstone. A delete that
  fails leaves only the tombstone, which `status` lists and the next prune
  deletes. A tombstone name never replaces an existing one, and `status`,
  the seat walk and prune's list never count, measure or cap a
  `seats/.retired-*` as a seat: they list it only as a tombstone. The key
  itself is gone, so a later run on it starts cold and whole, never on
  half a seat. That run re-creates the seat, and its
  `seats.jsonl` line makes it live again.

  The `retire` verb only tries its locks and never waits. It first passes
  the host's gate and takes a shared `host` lease the way a shared run
  does, by try: if an exclusive run holds the gate or `host`, it is
  refused `exclusive` and nothing is deleted, so no retire, by hand or by
  itself, ever deletes during an exclusive run or jumps ahead of one
  waiting. It then tries the seat lock: a busy seat is skipped and
  reported. This is the opposite of a run's order (seat, then host),
  which is safe only because retire never waits on either: both are
  tries, with no retry that holds `host` while it waits on a seat. Retire
  holds the shared lease through the rename and the tombstone's delete,
  so an exclusive run that arrives meanwhile may wait one seat's delete. Under the lock it checks the seat is still the one the caller
  meant: each run writes `seats/<key>/used` (`{ms, id}`) when it takes the
  seat lock. With `idle_ms`, a seat whose `used.ms` is newer than the
  place's clock minus `idle_ms` is refused `used`. With `once_id`, a seat
  whose `used.id` is not that run is refused `used`. A seat with no `used`
  has not run since this rule shipped, so the caller's age stands; a
  `used` that cannot be read or parsed is `unknown`, and the seat is
  kept. The
  caller reports every seat it considered: retired, busy, used, unknown
  or failed. Records, receipts and `runs/<id>/` are never touched. A caller
  retires only seats it recorded, never another caller's. "This caller" is
  the machine user's `seats.jsonl`, not one workspace: one agent's
  `prune --idle 1d` retires every idle seat that user's caller recorded,
  whoever's workspace it serves.
- **Size:** at the start of a run, while the run holds the seat lock, if the
  seat's kept directories plus its cache exceed `max_cache_per_checkout`, they are
  wiped and the run starts cold. The record says so (`pruned: {bytes, cap}`).
  Source, logs, receipts and records are never touched.
- **Floor:** below `refuse_runs_below_free_disk` a run refuses with 125, as before. Its
  `next` names `ferry status --on <place>` (seat sizes and last runs) and
  `ferry seats prune --on <place> --idle <dur>`.
  Anything cleverer (shared caches, cross-seat eviction) is the operator's.

**Bound on run history (the host's records):**

- A run's history is its run folder (`runs/<id>/`) and its record
  (`records/<id>.json`), with any reservation (`records/<id>.reserved.json`)
  a crashed finalize left beside that final record: it is not live, and left
  alone it would be reconciled into a second, `lost` record once the final
  one is gone. With `delete_run_history_after_days = <n>` in `host.toml` (a
  whole number of days, at least 1; anything else refuses the run as a bad
  `host.toml`), each run, after its record is final and its own locks are
  released (in teardown, after a `--once` retire), deletes the history of
  final runs submitted more than `<n>` days ago (by the host record's own
  `submitted_ms`, on the host's clock). It lists names only and works oldest
  first by the time a name carries (a run id starts with its submit time, on
  the caller's clock, and an account with its `ms`); it reads no run or
  account whose name is less than `<n>` days less one hour old. Names that
  carry no time come after the rest: temporary files first (judged by their
  modification time, never read), then retirement records. The name only
  orders and bounds the work; whether an entry is old is decided by what it
  records. The hour allows for the caller's clock: a pass rereads the runs
  named within that hour of the cutoff, and what it must keep although its
  name is old (live leftovers, live reservations, runs from a caller whose
  clock ran more than an hour behind the host's). Those, and listing the
  names, can still spend a pass's budget: that is a bound on each pass, not
  a promise that every pass deletes something. A run from a caller whose
  clock ran ahead of the host's is deleted that much later. It never delays
  its own recipe or its caller, and never counts against a run's timeout.
- **Never under a timing run:** the pass first passes the host's gate and
  takes a fresh shared `host` lease the way a shared run does, by try,
  never waiting at either, held by the supervisor alone. If an exclusive
  run holds the gate (waiting) or `host` (running), the pass is skipped
  and left to a later run, so it never jumps ahead of an exclusive run. While the pass holds it, no exclusive recipe can start: one
  that arrives waits for the pass, at most its budget plus one file.
- **Old reservations:** a reserved record older than `<n>` days is first
  reconciled by the ordinary rule (§8: another boot, a dead supervisor pid
  or a different start time makes it `lost`); a live supervisor's
  reservation is never touched. A reconciled run then ages like any final
  one, so a killed supervisor's run that nobody read is not kept forever.
- **Live leftovers keep their run:** a final record whose survivors
  (`custody.survivors`) include a process still alive is kept, because its
  history is what names that leftover: a live pid with the same start
  time, or a live pid alone when the start was not recorded (pid reuse
  then keeps a record, never deletes one). A survivor listed as
  `"unknown"` is no reason to keep it, or a macOS host would keep
  everything.
- **Retirement records** (`records/retired-<key>.json`) are not a run's:
  each ages by its own `retired_ms` on the same rule, and one without
  `retired_ms` is kept. A temporary file a crashed write left in
  `records/` (`.<name>.<pid>.<n>.tmp`) ages by its modification time and
  is never read as a record. Without the key, history is kept forever.
  It never touches a live reservation, a seat, `store/`, or a caller's
  receipts and fetched artifacts.
- **Budget:** the pass lists, reads and deletes entry by entry and checks
  one second between entries, then releases its lease. That bounds how much
  work it starts, not the filesystem: one listing or unlink in progress may
  take longer. What is left waits for the next run. A run's folder is
  deleted first, then a reservation left beside its record, then its record,
  each only after the one before it succeeded: a run deleted only in part
  keeps its record and stays eligible next time, and no reservation outlives
  its final record to be reconciled as `lost`. A reader that already has a
  file open (a `follow` in progress) finishes reading it; a new reader of a
  deleted run gets not-found.
- **The account:** the run's record is already final, so a pass that
  deleted anything creates `records/history-<ms>-<run>.json` (`{ms, run,
  deleted, older_than_days}`), never replacing one; it ages by its `ms`
  on the same rule. `deleted` counts whole
  runs only (folder and record both gone). Two passes may race on the
  same files; a file already gone is not an error, and a run counts for
  the pass that removed its record. Afterwards `ferry record <id>` and
  `ferry follow <id>` answer "not found (this host keeps run history for
  `<n>` days)": context, not proof, since the id may never have existed.
- **What is lost with it:** a run's history is the only thing that names
  it on the host. After it is deleted, a seat's last run is no longer
  named, and a marked survivor that was not seen alive at deletion (one
  listed as `"unknown"`) shows in `status` as `record: none` instead of a
  leftover of that run. The caller's receipt keeps the outcome, exit code
  and digests, not the output or the artifacts. The artifacts a recipe
  declares are already fetched to the caller's `<repo>/.ferry/runs/<id>/`
  when the run ends (§8); its output is not, so a caller that needs the
  output longer than `<n>` days saves it (`ferry follow` prints it) before
  then.

**Why:** history only grows when runs happen, so runs can bound it: no
daemon, no timer, nothing left on the host between runs (PRINCIPLES 2).
The host's operator sets the limit, because every caller and project shares
one history and no single project can see the rest. What outlives it is
the caller's: its receipt, the artifacts it fetched and any output it
saved.

**Why:** only the caller knows whether a workspace still exists, and only a
run holds its seat's lock. Each bound is enforced by the party that can
prove it, with no daemon and no second lock protocol.

**Why:** files follow lifetime (PRINCIPLES 1), and nothing lands inside a
working tree.

## 10. Configuration

Three files, each in a different place and owned by a different person.
Every key reads as a plain statement of what it does, so a new user can
read a file without Ferry's vocabulary. Sizes are strings with a unit
(`"40 GiB"`, `"512 MiB"` binary; `"40 GB"` decimal; units are
case-sensitive), durations a number and unit (`"6h"`, `"30m"`). A file
declares `version = 2`. A recipe's `needs` names what the place `has`
(§3): its declared labels, what is detected, and the binary's
capabilities, as `requires` does today; `[provider] needs` names only
the binary's capabilities.

Only the three files' keys are renamed. Records, receipts, the request
and the protocol keep their field names (`requires`, `routable`,
`preserve`, `forwarded_env`, `provides`, …), because every consumer's
receipt parser reads them; they move only with a protocol bump of their
own. A reader who sees `needs` in `ferry.toml` and `requires` in a
receipt is seeing this rule, not a bug.

- **`ferry.toml`** (the project, committed; or the JSON emitted by
  `[catalog] adapter`): what can be run.

  ```toml
  version = 2
  keep_between_runs = ["_build"] # ignored build dirs kept warm, for every recipe
  [recipe.check]
  command = ["just", "check"]
  needs = ["linux-x86_64"]       # what the machine must have (labels unchanged)
  may_run_elsewhere = true       # may run on another machine
  [[also_send]]                  # extra refs or dependencies sent with the commit
  ref = "refs/remotes/origin/main"
  from = "main@origin"
  [environment]
  send = ["MY_FLAG"]             # your variables passed to the recipe
  refuse_other_vars_starting_with = ["MY_"]
                                 # a set, nonempty MY_* variable refuses the run,
                                 # unless it is listed in send or keep_local
  keep_local = ["MY_SECRET"]     # deliberately kept on the caller
  [hooks]                        # prepare, env, collect
  [provider]
  needs = []                     # what the project needs of Ferry itself
  ```
- **`machines.toml`** (the caller, `~/.config/ferry/`): where you can run.

  ```toml
  version = 2
  delete_unused_checkouts_after_days = 1   # §9; a checkout here is a seat:
                                           # a place's copy of the repo, with its build cache
  [machine.bench]
  ssh = "bench"
  platform = "x86_64-unknown-linux-gnu"
  has = ["linux", "x86_64", "quiet"]
  reserved_for = ["quiet"]       # chosen without --on only for recipes that need one of these
  ```

  A run without `--on` goes to the first machine, by name, that has what
  its recipe needs. A machine with `reserved_for` is skipped in that
  choice unless the recipe needs one of the listed features, so a timing
  host is not taken by runs that merely forgot `--on`. One listed feature
  among the recipe's needs is enough. When no other machine can run it,
  such a run is refused ("no configured machine meets requirements"); it
  never falls back to the reserved machine. `--on <machine>`
  still chooses it explicitly; every name in `reserved_for` must also be
  in its `has`, or `machines.toml` is refused.
- **`host.toml`** (each place, its operator, `~/.config/ferry/`): the
  machine's own rules.

  ```toml
  version = 2
  refuse_runs_below_free_disk = "40 GiB"
  max_cache_per_checkout = "15 GiB"
  max_run_time = "6h"
  delete_run_history_after_days = 30
  ```
- **The catalog adapter's JSON** carries its own `version`, checked on its
  own: during the compatibility release a version 2 `ferry.toml` may name
  an adapter that still prints version 1, and the reverse. Each is read by
  its own version's keys, and the two are merged as today (`ship` /
  `also_send` entries combined, a duplicate refusing; `preserve` /
  `keep_between_runs` the project's one list, from either).
- **From version 1:** for one release Ferry reads both versions, of each
  file and of adapter JSON. A version 2 file or JSON that holds a version
  1 key is refused, and the refusal names the new key. Version 1 is
  removed only after Herald and Murail have each moved and given GO by
  execution, not merely at the next version number.
- **Order:** a file another tool also reads moves only after that tool
  reads version 2 (Murail's `remote-hosts.py` reads `machines.toml`
  itself), so a shared file never flips under a reader Ferry does not
  control.
- **Old commits:** once version 1 is removed, an exact commit whose
  `ferry.toml` or adapter is still version 1 no longer runs on the
  current Ferry: it is refused, naming the version. That is the
  compatibility limit; such a commit runs on a Ferry from before the
  removal (an install keeps its previous build). No shim is kept.

  | version 1 | version 2 |
  |---|---|
  | `requires` (recipe, `[provider]`) | `needs` |
  | `routable` | `may_run_elsewhere` |
  | `preserve` | `keep_between_runs` |
  | `[[ship]]` | `[[also_send]]` |
  | `[environment] forwards / watches / local` | `send / refuse_other_vars_starting_with / keep_local` |
  | `[host.<name>]` | `[machine.<name>]` |
  | `triple` | `platform` |
  | `features` | `has` |
  | `cores` | removed (routing never read it) |
  | `min_free_bytes` (a byte count) | `refuse_runs_below_free_disk` (a size) |
  | `seat_cap_bytes` (a byte count) | `max_cache_per_checkout` (a size) |
  | `max_timeout` | `max_run_time` (a duration) |

  Out of this change, named for later: the `--seat` flag and the hook
  environment (`FERRY_SEAT`, `FERRY_CACHE_DIR`), which recipes read.
- **`pruned.json`** (caller state, `~/.local/state/ferry/`): `{<place>: ms}`,
  when this caller's runs last pruned that place by themselves.
- **`seats.jsonl`** (caller state, `~/.local/state/ferry/`): one line per seat
  this caller used: `{key, place, repo_path, seat, last_used_ms}`.
- **Hook environment:** `FERRY_ID FERRY_SEAT FERRY_COMMIT FERRY_WORKDIR
  FERRY_CACHE_DIR FERRY_REQUEST_FILE`, plus `FERRY_EXIT_CODE` for collect. Every hook
  and the recipe see the same `FERRY_ID`, the run's own, collect included
  (it is the §7 marker).

## 11. Guarantees and residues

| Guarantee | Mechanism | Residue (reported) |
|---|---|---|
| exact commit | pushed objects, reset checkout | a session leaver that closed its fds can write into a later run's checkout and the seat's cache (listed as a survivor when it carries the marker); a commit ever shipped on the place is fetchable by hash through a served `[[also_send]]` URL even if this run did not declare it (refs are isolated, objects are not) |
| one run per seat | inherited seat flock | work that unlocks its own descriptor |
| exclusive host (among Ferry runs; not a quiet host) | the supervisor's flock on `host` | a survivor of an earlier run (listed in its record); a killed supervisor: its session runs on without `host` until its session owner stops it at the run's timeout or deadline, and the record reconciles to `lost` |
| every run ends | an absolute deadline | a killed supervisor: the record is reconciled as `lost`; the session owner stops its session at the timeout or deadline (TERM, grace, KILL), sparing only itself; what that stop leaves is listed by `ferry status` (no marker pass runs without the supervisor); when the session cannot be listed, a member that moved to its own process group is out of the owner's and the supervisor's reach alike, and `ferry status` lists it as a leftover when its marker can be read; otherwise status reports it as unreadable or an error, never as absent (§7). A member that cleared its marker is invisible |
| run sessions stopped; marked survivors reported | session stop; the marker pass | a marked survivor is reported and left running (the project's or operator's to stop); a leaver that dropped the marker is not listed; an incomplete pass is `"unknown"` |
| complete output | one capped writer to a file | beyond the cap: `capped` |
| one honest record | reserve, final once, reconcile | — |
| client independence | the detached supervisor | — |

## 12. Acceptance (each invariant has an execution test)

1. **Duplicate submit:** one run.
2. **Crash between reserve and final:** exactly one `lost`.
3. **Two finalizers:** exactly one final record.
4. **A connection dropped mid-run:** the run completes; the caller re-follows,
   and its byte count equals the file's.
5. **Stop:** `stopped` 128+signal; `stopped_by` is set.
6. **Deadline:** `timeout` 124 even with no client; the session is stopped.
7. **Seat busy:** immediate 125 naming the run.
8. **Exclusive:** no other run holds `host` while it executes.
9. **A survivor holding a lock:** a recipe (and, separately, a collect
   hook) starts a child that leaves the session (setsid and double fork,
   re-parented) and keeps its fds. The seat stays busy, and the next run's
   refusal names the run it outlived. The run's record lists the survivor by
   pid and command (on macOS: listed or `"unknown"`), never `[]`. An exclusive run on another seat is admitted
   while it lives: the survivor holds neither `host` nor `host.gate`.
10. **A nested run on the same place:** 125.
11. **Old caller with a new place, and a new caller with an old place:** the
    refusal (125, naming the old `source_rev`), never a mixed state root.
12. **Fault injection** (`FERRY_FAIL_AT`, debug builds) at every fallible
    point keeps 1–3.
13. **The cells:** Darwin and Linux Nix checks, a Linux build host, a Mac
    place.
14. **Seat count:** without `--idle`, `ferry seats prune` retires a seat
    only when its workspace is provably gone (`NotFound`, a listable
    parent on the recorded device). An unreadable or unmounted workspace
    is kept and reported. It never touches another caller's seat, and
    skips a busy one.
15. **Seat size:** over `max_cache_per_checkout`, a run starts cold with `pruned`
    recorded. Nothing outside `seats/<key>/` is ever
    measured or deleted: not through a symbolic link, not through an
    unvalidated `keep_between_runs` path.
16. **A survivor that closed its fds:** the record lists it (marker; on
    macOS listed or `"unknown"`, never `[]`), the
    next run on the seat is admitted, and `status` lists it as a leftover
    until it is stopped. One whose environment was cleared is not listed: the
    residue, asserted so a later change cannot claim it.
16a. **An incomplete pass:** an injected failure to list processes, and a
    live eligible process whose environment cannot be read (Linux: a
    non-dumpable child; macOS: a platform binary started by the recipe),
    each give `survivors: "unknown"` with `custody.error` naming it, never
    `[]`, and the run's outcome is unchanged. So does an unreadable process
    whose parent cannot be read or changes during the read (injected): it is
    never skipped as "not the user's".
17. **Collect's id:** collect sees the recipe's `FERRY_ID`, and a run leaves
    exactly one `runs/<id>/`.
18. **`status`:** with a shared run, two waiting exclusive runs and a
    leftover on the place, it names each: the one holding the gate as its
    holder, the other as a requester. With `host.gate` naming a run whose
    supervisor is gone, the holder is `unknown`. It takes no lock: a run
    submitted during `status` is never refused busy because of it. A Mac
    place and a Linux place give the same schema and meaning (`--json`),
    not the same values.
19. **One-run seat:** `ferry run --once` ends with exactly one final record
    (with `once: true`), `records/retired-<key>.json`, and no
    `seats/<key>/`. With the retire injected to fail before the rename, the
    record is the same and final, the seat stays, `status` lists it, and a later
    `prune --idle 0s` retires it. With an escaped child that inherited the
    seat's fd (the item 9 fixture), the record is final and unchanged, the
    seat stays and is busy, prune skips it while the child lives, and
    retires it once the child has exited.
20. **Idle seat:** `ferry seats prune --idle <dur>` retires this caller's
    seat last used before `<dur>`, keeps one used since, skips a busy one
    and reports it, keeps and reports a seat whose lines have no
    `last_used_ms`, and never touches another caller's seat or any record,
    receipt or `runs/<id>/` (records, output and fetched artifacts stay
    readable after retirement).
21. **Retire against a newer use:** a prune whose list predates a run on
    the seat (injected between the list and the retire) is refused `used`
    and the seat stays, when that run is still inside `idle_ms`: the check
    is the seat's current age, not a version. A `--once` retire that finds another run's `used`
    is refused `used`. With the supervisor's seat fd released by `LOCK_UN`
    instead of close (knockout), item 19's inherited-child case fails, and
    so does one whose child cleared its marker. With the delete injected
    to fail after the rename, only the tombstone remains, `status` lists
    it, the next run on that seat passes cold, and the next prune deletes
    the tombstone.
22. **Run history:** with `delete_run_history_after_days = 1`, a run on a
    host holding a final run from two days ago, a final run from an hour
    ago, a reserved run from two days ago with its supervisor live, and a
    reserved run from two days ago whose supervisor is gone, deletes, after
    its own record is final, the first run's and the reconciled fourth
    run's folders and records, and writes `records/history-<ms>-<run>.json` with
    `deleted: 2`. Its recipe starts with no deletion before it. With the
    pass held mid-delete, an exclusive run submitted then does not start
    its recipe until the pass has released its lease; with an exclusive
    run holding `host`, the pass is skipped. An old
    final run whose recorded survivor is still alive (with or without a
    recorded start) is kept. Without the key nothing is deleted; with `0`
    or `1.5` the run is refused as a bad `host.toml`. A seat, `store/` and
    the caller's receipts are untouched. `ferry record` on a deleted run
    names the setting as context. A retirement record older than a day is
    deleted and a newer one is kept. With the deletion injected to fail
    after the folder, the run's record is unchanged, the record that
    failed stays, the count leaves that run out, and the next pass deletes
    it. Progress: a host holding more runs named under 23 hours old and
    more retirement records (young, or without `retired_ms`) than one pass
    can read, plus a crashed write's temporary file, still has its
    two-day-old final run deleted by the first pass; the temporary file is
    deleted by its age and never read as a record. A final record with a
    stale reservation beside it, deleted with the injection failing
    between the reservation and the record, keeps its final record and is
    never reconciled as `lost`.
23. **Prune without remembering:** with
    `delete_unused_checkouts_after_days = 1` on the caller, a run on a
    place holding this caller's seat unused for two days and one used an
    hour ago retires the first, keeps the second, prints one summary line
    on stderr, and exits with its own code and unchanged `--json`. A
    second run within the hour does not prune, and two runs ending
    together prune once. With the place unreachable or slow, the run's
    exit code and `--json` are its own and no new retire starts after ten
    seconds. With an exclusive run holding the place's gate or `host`,
    retire, by hand or by itself, is refused `exclusive` and deletes
    nothing. Without the key, nothing is retired.
24. **Plain config names:** each of the three files at version 2 is read
    with the §10 keys, sizes and durations. A version 1 file is read the
    same for one release; a version 2 file holding a version 1 key is
    refused naming the new key (`min_free_bytes` → "use
    `refuse_runs_below_free_disk`"), and so is a size without a unit. A
    version 2 `ferry.toml` with a version 1 adapter, and the reverse, run
    with the same merged contract as both at version 1. A variable under
    a `refuse_other_vars_starting_with` prefix that is listed in `send` or `keep_local` does
    not refuse.
25. **Push watchdog:** through a fake ssh, a push that stops producing
    progress output on a live link is refused (125) within the progress bound
    plus slack, with no submit and no host record, and its ssh is gone; a
    push that keeps printing progress for longer than the bound is not cut and
    its run passes; a store setup that never answers is refused within
    the reply bound.
26. **Reserved machine:** with `bench` (`reserved_for = ["quiet"]`) and
    `gates` both able to run a recipe, a run without `--on` of a recipe
    that does not need `quiet` goes to `gates`, one that needs `quiet`
    goes to `bench`, and `--on bench` runs either there. Without `gates`,
    the first is refused, never sent to `bench`. A `reserved_for`
    name missing from that machine's `has` refuses the run as a bad
    `machines.toml`.
