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
| **Recipe** | `name → {argv, requires, exclusive, timeout, wait, artifacts, preserve}` read from the commit | runs only where `requires ⊆ provides`, on every route | the repository decides what may run |
| **Place** | `local` or an ssh destination; `provides` = declared ∪ detected ∪ binary capabilities; `host.toml` | describes itself through the binary that will run the work | the facts come from the doer |
| **Seat** | `(caller, repo path, name) → checkout + cache` | reset to the commit each run; `preserve` dirs kept | warm and exact, never shared |
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
`--timeout-seconds` and `--wait-seconds`), `--detach`, `--once`,
`seats prune --idle`, `ferry check`,
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
- timeout above the host's `max_timeout`;
- capability missing.

## 5. Run lifecycle

```
caller:  check recipe@commit ─refuse─► 125 receipt
         push objects (idempotent) ─► submit{id,request}
place:   reserve record (O_EXCL; a duplicate id → follow the existing run) ─► fork supervisor (setsid, no tty) ─► reply id
super:   seat lock (try; held → refuse busy)
         host lease (shared|exclusive; bounded wait; names holder)
         floor check (host.toml) ─► reset checkout (keep preserve) ─► objects
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
| `retire` | seat `key` | `{retired, bytes}` (under the seat lock, try only) or `{refused: busy}` |
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
  runs/<id>/  request.json output.log artifacts.tar  record (with result.json, result.error and stop.json)
  records/    <id>.reserved.json <id>.json    record (operator retention)
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
    newest `last_used_ms` is older than `<dur>`. Ferry never runs it by
    itself; when to run it is the caller's or the operator's;
  - **one run:** `ferry run --once` declares the seat lives for that run
    only. After the final record, the supervisor, still holding the seat
    lock, retires the seat. The record is final first, so it says `once`,
    never that the delete happened; a delete that fails leaves the seat
    for `status` to show and a later prune to retire.

  Retiring deletes `seats/<key>/` and writes `records/retired-<key>.json`.
  The `retire` verb only tries the seat lock: a busy seat is skipped and
  reported. Records, receipts and `runs/<id>/` are never touched. A caller
  retires only seats it recorded, never another caller's.
- **Size:** at the start of a run, while the run holds the seat lock, if the
  seat's preserve paths plus its cache exceed `seat_cap_bytes`, they are
  wiped and the run starts cold. The record says so (`pruned: {bytes, cap}`).
  Source, logs, receipts and records are never touched.
- **Floor:** below `min_free_bytes` a run refuses with 125, as before. Its
  `next` names `ferry status --on <place>` (seat sizes and last runs) and
  `ferry seats prune --on <place> --idle <dur>`.
  Anything cleverer (shared caches, cross-seat eviction) is the operator's.

**Why:** only the caller knows whether a workspace still exists, and only a
run holds its seat's lock. Each bound is enforced by the party that can
prove it, with no daemon and no second lock protocol.

**Why:** files follow lifetime (PRINCIPLES 1), and nothing lands inside a
working tree.

## 10. Configuration

- **`ferry.toml`** (or the JSON emitted by `[catalog] adapter`): `[recipe.*]`,
  `[hooks]`, `[environment] forwards/watches/local`, `[ship]` extra refs
  (companion refs and mirrors), `[provider] requires`.
- **`machines.toml`** (caller): `[host.<name>] ssh, features`.
- **`host.toml`** (place, operator): `min_free_bytes`, `max_timeout`,
  `seat_cap_bytes`.
- **`seats.jsonl`** (caller state, `~/.local/state/ferry/`): one line per seat
  this caller used: `{key, place, repo_path, seat, last_used_ms}`.
- **Hook environment:** `FERRY_ID FERRY_SEAT FERRY_COMMIT FERRY_WORKDIR
  FERRY_CACHE_DIR FERRY_REQUEST_FILE`, plus `FERRY_EXIT_CODE` for collect. Every hook
  and the recipe see the same `FERRY_ID`, the run's own, collect included
  (it is the §7 marker).

## 11. Guarantees and residues

| Guarantee | Mechanism | Residue (reported) |
|---|---|---|
| exact commit | pushed objects, reset checkout | a session leaver that closed its fds can write into a later run's checkout and the seat's cache (listed as a survivor when it carries the marker); a commit ever shipped on the place is fetchable by hash through a served `[[ship]]` URL even if this run did not declare it (refs are isolated, objects are not) |
| one run per seat | inherited seat flock | work that unlocks its own descriptor |
| exclusive host (among Ferry runs; not a quiet host) | the supervisor's flock on `host` | a survivor of an earlier run (listed in its record); a killed supervisor: its session runs on without `host` until its session owner stops it at the run's timeout or deadline, and the record reconciles to `lost` |
| every run ends | an absolute deadline | a killed supervisor: the record is reconciled as `lost`; the session owner stops its session at the timeout or deadline (TERM, grace, KILL), sparing only itself; what that stop leaves is listed by `ferry status` (no marker pass runs without the supervisor) |
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
    only when its workspace is provably gone (`NotFound`, a listable parent on the
    recorded device). An unreadable or unmounted workspace is kept and
    reported. It never touches another caller's seat, and skips a busy one.
15. **Seat size:** over `seat_cap_bytes`, a run starts cold with `pruned`
    recorded. Nothing outside `seats/<key>/` is ever
    measured or deleted: not through a symbolic link, not through an
    unvalidated preserve path.
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
    `seats/<key>/`. With the retire injected to fail, the record is the
    same and final, the seat stays, `status` lists it, and a later
    `prune --idle 0s` retires it.
20. **Idle seat:** `ferry seats prune --idle <dur>` retires this caller's
    seat last used before `<dur>`, keeps one used since, skips a busy one
    and reports it, and never touches another caller's seat or any record,
    receipt or `runs/<id>/`.
