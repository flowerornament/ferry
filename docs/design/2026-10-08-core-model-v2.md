# Ferry core model v2, and the roadmap

Status: superseded, 2026-10-08, by `design/2026-10-08-core-model-v3.md` where they differ (user decisions: detached runs with a mandatory timeout; `hold` leaves Ferry). Kept for its review history. Originally proposed 2026-10-08. Owner: the ferry seat. This document has two
jobs: it decides the design, and it orders the work.

- **Supersedes** the unlanded v1 draft (`design: core model v1`, commit
  `ec298586`, kept for its review history). Where the two differ, it also
  supersedes the designs in `design/2026-10-08-coordination-and-dx.md`
  §4–§5. That document stays authoritative for the requirements (§3) and the
  evidence (§1).
- **Constraints:** `PRINCIPLES.md`; the agreements A1–A7 (murail master
  `997c62a5`); the user's decisions on 2026-10-08:
  - stop survivors;
  - ship the P0s now;
  - park P1;
  - portable to macOS hosts; Linux-only solutions are a no-go;
  - files follow lifetime;
  - Quint for formal models, attacked by adversarial reviewers.
- **Inputs:** the two adversarial reviews of the v1 model (reality and rigor,
  2026-10-08; their findings are mapped one by one in §9), the critics' and
  consumers' reviews of P1, and tonight's incidents.

---

## 1. What Ferry is, in one paragraph

Ferry runs a repository's recipe at an exact commit on a place that can run it,
serializes what must not overlap, and records exactly what happened. It owns
three things:

- **exclusion:** locks;
- **custody:** what a run leaves behind;
- **records:** what happened.

Projects own what runs and how. The operator owns host policy, services and
installs. Ferry owns nothing that lives longer than a run, apart from files of
three declared lifetimes (PRINCIPLES 1, 2). Everything else is out of scope
(PRINCIPLES, "What Ferry is not").

## 2. Seven concepts

Each concept has a purpose, a state, actions and an operational principle (OP),
in Jackson's concept design. Concepts never read each other's state;
synchronizations (§3) compose them.

### 2.1 Recipe: what a project allows to run

- **State:** `name → {argv, requires: set, exclusive: bool}`, read from the
  contract at the exact commit (`ferry.toml`, or an adapter's emitted
  contract).
- **Actions:** `declare` (by committing the contract), `list`.
- **OP:** a run of *R* happens only on a place providing everything *R*
  requires, or is refused before any effect.
- **v2 changes:**
  - `requires` is one set, matched against the place's declared features,
    detected facts (`os:*`, `arch:*`) and Ferry capabilities (`ferry:*`).
    `[provider] requires` and `requires = ["exclusive"]` stay accepted
    (ferry-ljc.14).
  - The check applies on **every** route, local included (ferry-ljc.19).

### 2.2 Place: where work happens, and what it truthfully provides

- **State:**
  - name; address (`local`, or an ssh destination);
  - provides (declared ∪ detected ∪ the capabilities of the binary that will
    run the work);
  - policy (`host.toml`);
  - installed generations.
- **Actions:** `describe`, `check`, `install(rev)`, `rollback`. Install and
  rollback are operator acts (PRINCIPLES 4).
- **OP:** a place describes itself through the binary that will run the work.
  `describe` returns the launcher's store path, and the run starts through it
  (pinned launch, ferry-ljc.2). The session leader also starts from that
  generation, not the `current` symlink.
- **Portability:** Linux and macOS places, with the same meaning (§6).

### 2.3 Seat: a warm, isolated working tree

- **State:** `seat → {checkout, cache dir, state: fresh | busy | idle | tainted}`.
  The key is (caller machine, repository, seat name). The default name is the
  caller's workspace name (ferry-ljc.14).
- **Actions:** `checkout(commit)`, `mark(idle | tainted)`, `reclaim(target)`
  (by host policy only).
- **OP:**
  - runs from one workspace reuse one warm checkout, one at a time;
  - different workspaces never share one;
  - a tainted seat refuses until it is recovered (ferry-ljc.16);
  - only an idle seat's targets are ever reclaimed.
- **Location:** every seat, local or host, lives under the user's state root on
  that machine (`<state>/seats/<key>/`). That ends the per-repository
  `.ferry/local` state root (§2.5, one lock space). `.ferry/` in a repository
  keeps only that repository's records.

### 2.4 Run: one attempt, one handle, one verdict

- **State:** `id → {work, place, phase, outcome}`.
  - `work` is `recipe@commit` (isolated, in a seat) or `command@here` (as is,
    the `hold` verb).
  - The phases are `reserved → refused | accepted → waiting → running →
    draining → ended`.
- **Actions:** `start`, `stop(handle)`.
- **OP:**
  - every attempt ends with exactly one record;
  - stopping by its handle ends it as lost/143, custody still accounted;
  - a run inside a Ferry session may not start new work on the same place
    (nesting refusal, §2.5).

### 2.5 Lock: exclusion without people

- **State:** per place, in one lock space (`<state>/locks/`):
  `name → {rank, holders: run → mode, intents, gate}`.
- **Actions:** `acquire` (blocks, interruptible), `try` (never blocks),
  `release`.
- **OP:**
  - while a run holds *L* exclusively, no other run holds *L* (safety, by
    flock only);
  - an exclusive arrival eventually runs once the earlier holders end (liveness,
    by intent markers);
  - blocking acquires happen in increasing rank, enforced at runtime, so waits
    never cycle.

| Lock | Rank | Taken by | Mode |
|---|---|---|---|
| `<user name>` (e.g. `land:<repo>`) | 10 | `hold` | exclusive (default) or shared |
| `install` | 20 | install, rollback (`try` only) | exclusive |
| `seat:<key>` | 30 | recipe runs | exclusive |
| `host` (the lease) | 40 | every run on the place | shared; exclusive for `--exclusive` |

**v2 changes:**

- **Intent markers (liveness without a queue).**
  - An exclusive arrival first writes `locks/<name>.intent/<pid>.json` (pid and
    start time). Shared arrivals defer while a live intent exists.
  - A stale intent (dead pid, or a different start time) is ignored and
    removed, so a crash cannot wedge a lock.
  - Mutual exclusion still rests on flock alone. Intents only affect who goes
    next (PRINCIPLES 5, 6).
  - This fixes the rigor review's starvation finding: before the gate, shared
    arrivals could win it forever.
- **Inheritance.** A run's lock descriptors are inherited by its session leader
  and the work's processes (CLOEXEC cleared), for both holds and recipe runs.
  A dead supervisor cannot free them while work lives (ferry-ljc.17). Because
  survivors are stopped (§2.6), inheritance cannot stall a lock beyond the
  grace period.
- **Nesting refusal.**
  - Inside a Ferry session (`FERRY_ID` set, `FERRY_PLACE=<p>`), a `run` on the
    same place, and a `hold` of a name already held up the process tree
    (`FERRY_HOLDS`), both refuse with 125 and a next step.
  - A run on a different place is allowed: different lock space.
  - Ranks alone cannot see parent→child waits (reality finding 6).
- **One lock space per machine.** The host lease, holds and seat locks share
  `<state>/locks/` on each machine. The local lease becomes machine-wide
  (ferry-ljc.4 folds in here).

### 2.6 Custody: what a run leaves behind

- **State:** `run → {tier, members (pid + start time, recorded), verdict:
  pending | empty | stopped | survivors | unknown}`.
- **Actions:** `adopt`, `track`, `census`, `stop_survivors`, `signal`.
- **OP:** a run's locks are released only after custody ends in `empty` or
  `stopped`. A run never leaves a live process holding its seat.
- **Tiers, reported in every record:**
  - **complete** (Linux): child subreaper plus the ECHILD-only census;
  - **tracked** (macOS): process group, plus inherited descriptors, plus a
    lineage tracker (kqueue `NOTE_FORK`/`NOTE_EXIT`, `proc_listchildpids`)
    (ferry-ljc.12).

**v2 changes:**

- **Stop survivors** (user decision; ferry-ljc.22).
  1. After the work and the collect hook end, census.
  2. Survivors attributed to this run get TERM to their pids and groups,
     a grace period (10 s), then KILL, then a reap and a new census.
  3. `empty` → seat idle, with the record noting `custody.stopped=[…]`.
  4. Otherwise `survivors` or `unknown` → seat tainted, and the locks are kept
     until the processes are gone or an operator stops them.
  
  Only processes custody attributes to this run are signalled. Never names or
  paths (PRINCIPLES 7).
- **Recorded members.** The supervisor appends each member it learns of (pid
  plus start time) to the run's owner marker as it goes.
  - After a supervisor death, recovery is **not** automatic: an unrecorded
    escapee cannot be ruled out (PRINCIPLES 6; reality finding 3).
  - The refusal names the next step: `ferry stop <dead run>`. That signals the
    recorded members, confirms they are gone, and records an explicit
    `recovered` (ferry-ljc.3).
- **Order:** the collect hook runs before the census (Herald's requirement),
  and a collect failure is recorded while cleanup still runs.
- **Residues, documented:**
  - **macOS:** a descendant that forks and has its parent exit before the
    tracker enrols it, and that leaves the group and closes every descriptor,
    escapes.
  - **Any platform:** after a supervisor death, an escapee that was never
    recorded.

### 2.7 Record: one honest account per attempt

- **State:** `id → {kind: run | hold | stop | install, state: reserved | final,
  work, place, locks[], custody, admission, outcome, exit_code, reason, next,
  evidence: [host record id]}`.
- **Actions:** `reserve`, `finalize` (exactly once), `reconcile`, `read`,
  `list`.
- **OP:** every attempt has exactly one record, and it becomes final exactly
  once.
- **v2 changes** (reality finding 9; ferry-ljc.21):
  - **Reserve first.** The caller reserves the record before any effect, and
    finalizes it at the end.
  - **The host's record is evidence,** referenced by id, not a second record.
  - **A record left `reserved`** (caller SIGKILLed) is finalized as lost by
    `reconcile`, once the supervisor is provably gone (pid plus start time).
    `status` and the next run on the seat call it.
  - **Finalization is idempotent** and never blocks the release of locks.
  - **A diagnostic failure never changes the outcome** (P1 C4).
- **Schema:** fields are only added, and an absent field means unknown.

## 3. Synchronizations

```
S1  start(work, place)        reserve record; Recipe.requires ⊆ Place.provides else finalize(refused, next)
S2  start remote              Place.describe → pinned launcher; host-run and session leader from that generation
S3  accepted                  nesting check (FERRY_ID / FERRY_HOLDS) → Lock.acquire in rank order, intents first
S4  Lock(host) held           Place.admit(policy) → Seat.reclaim(idle, by try) → still below → finalize(refused, next)
S5  work + collect ended      Custody.census → stop_survivors → empty|stopped: release locks (stamp while held),
                              Seat.mark(idle), finalize ; survivors|unknown: Seat.mark(tainted), keep locks
S6  stop(handle)              ownership check (own workspace, else --other --reason) → Custody.signal → S5
S7  install(rev)              Lock.try(install) → build → check → flip ; --quiesce adds Lock(host, excl)
S8  any failure after spawn   recorded; never skips S5   (fault injection tests every fallible point)
S9  seat has owner marker     supervisor alive → wait/refuse ; dead → refuse, next: ferry stop <run> (explicit recovery)
S10 seat tainted              refuse, next: ferry stop <run> / recover
S11 heartbeat silent > 30 s   host treats the client as lost → stop the work → S5 (ferry-ljc.18)
S12 record reserved, owner gone   reconcile → finalize(lost, reconciled)
```

S8 and S5 together are what the v1 bugs violated (P1 F1/C1, ferry-ljc.16/.21).
The refactor (§7) implements them once, in one Custodian, instead of at each
call site.

**Amendment 2026-10-08 (P0 critic rounds 3–4, ferry-ljc.22).** Every round-3
and round-4 finding was one class: an output failure or a late event steering
control flow or the verdict. S8 is refined by three rules:

- **A. Data never controls; control has one channel.**
  - Control is signals, the host's stdin heartbeat or EOF (client loss), and
    the timeout.
  - Output (the recipe's stdout and stderr, and events) is data. A closed or
    failed output stream is never a stop: it is dropped, counted and
    recorded.
- **B. The verdict is decided once, by the first terminal fact.**
  - Once the recipe has a result, the run is ran-pass, ran-fail or timeout.
    Later client loss, signals or release errors are recorded diagnostics.
  - Lost is reached only when control stopped the run before a result. Its
    code is 128+signal if a signal was caught, else 255, with the cause
    named.
  - One pure function maps the facts to (outcome, exit code, reason) on
    every route.
- **C. The supervisor never blocks on data or starves on it.**
  - Output goes through a bounded channel to one writer. The supervisor loop
    drains with a bound per tick and checks stop, timeout and the result
    every tick.
  - The final emit is bounded, and the record on disk is the truth.

In the model (§8), output is not an action that can change `phase`.

## 4. Interface

```
ferry run <recipe> [--on <place>] [--exclusive] [-- <args>…]    recipe at the exact commit, in a seat
ferry hold <lock> [--shared] -- <command>…                      your directory, as is, under <lock>
ferry stop <run>                                                yours; --other --reason for anyone else's
ferry status [--on <place>]                                     runs, locks, seats, disk, installed revisions
ferry record [<run> | last]          (alias: receipt)           what happened; `ferry records` lists
ferry check [--on <place>]           (alias: doctor)            is this place usable for this repository?
ferry recipes · ferry places (alias: machines) · ferry init
```

**Removed from the surface:**
- `seat recover`: becomes the `next:` step `ferry stop <run>`;
- `--seat`: advanced; the default is the workspace;
- `--commit`: advanced;
- `--repo-land`: the default for `hold land`;
- `--json`: everywhere, documented once.

**Output:**
- one summary line in record vocabulary;
- waiting lines naming the holders;
- refusals as `what / why / next`;
- exit codes as in the README table.

**Vocabulary:** user-facing text says "this machine" and the place name.
"Caller" and "host" appear only in the reference.

Interactive commands: `hold` refuses when stdin is a terminal (125, next: make
the command non-interactive). There is no terminal hand-off (decision from P1
C2).

## 5. Ownership

The end-to-end argument, plus PRINCIPLES 3:

| Function | Owner |
|---|---|
| what runs, how; gates; land rebase and conflict law | repository |
| host policy, services (e.g. Herald's shared Postgres), installs, retention of records | operator |
| exclusion, custody, records, describing a place, the install procedure tool | Ferry |

## 6. Portability

| Mechanism | Linux | macOS |
|---|---|---|
| locks, intents, inheritance | flock, files | flock, files |
| custody tier | complete (subreaper, ECHILD census) | tracked (kqueue lineage) |
| process start time | `/proc/<pid>/stat` | `proc_pidinfo` |
| disk, load | statvfs, `/proc/loadavg` | statvfs, `sysctl vm.loadavg` |
| install procedure | Ferry's own primitives (`ferry hold install`, rename, `host-info` hashes) | same |

The environment matrix is a gate: the Darwin and Linux Nix check sandboxes,
bench via the dev shell, and a Mac host (ferry-ljc.12, ferry-oln.1).

## 7. Implementation shape

Three types implement the concepts' laws once:

| Type | Responsibilities |
|---|---|
| `Lock` | rank checked at runtime; intents; inheritance; ledger entry (diagnostic) |
| `Custodian` | tiers; S5, S8 and S11; stop_survivors |
| `Record` | reserve, finalize and reconcile |

`run`, `hold`, `stop` and install are compositions of these.

A fault-injection harness (`FERRY_FAIL_AT=<point>`, debug builds only)
enumerates every fallible point after a resource is taken. One test loops over
the points and asserts the invariants: no early release, exactly one record,
the correct exit code.

## 8. Formal model (Quint)

`formal/ferry.qnt`, written from this document by a modeller who is not its
author. It has one section per concept, and one `run` test per synchronization
S1–S12.

**Reviewer fixes, built in from the start:**
- **Independent truth.** Ghost state for flock holders and for each run's start
  generation, so invariants never share definitions with the guards they check.
- **Wider bounds:** two user locks, two shared holders of the host lease,
  2 seats, and the install lock.
- **More processes:** escapees that fork, and escape while draining.
- **Runs that restart,** so liveness can fail.
- **Deadlock and liveness** checked with crashes off; safety checked with
  crashes on.
- **Every surviving v1 mutant** is kept as a mutant test that must fail.

**Invariants:**
- MutualExclusion, over physical holders, independently defined;
- NoEarlyRelease;
- SeatAlone;
- ExclusiveQuiet (no other run's live process while an exclusive run
  executes);
- NoReclaimLive;
- RankOrder;
- OneRecord, as the record state machine;
- PinnedLaunch, against the start generation;
- RequirementsHold.

**Temporal properties:**
- EveryRunEnds;
- LocksEventuallyFree;
- ExclusiveEventuallyRuns, given intents and fairness.

**Checking:**
- `quint test` for the per-synchronization tests;
- `quint run` simulation at wide bounds;
- `quint verify --backend=tlc` for exhaustive checks at narrow bounds.

The toolchain is pinned in `formal/flake.nix` (quint, jdk). Model-based trace
replay against the binary comes with the refactor (§7).

**Acceptance:** the two adversarial reviewers (reality and rigor) find no
surviving mutant and no unmodelled reality gap that matters for an invariant.
Anything accepted as residue is listed here.

### 8.1 Amendment 2026-10-08: model round 2 (decisions)

The first Quint model (scratchpad `quint.SAlG`, not yet in `formal/`) and its two
adversarial reviews (reality, rigor) did **not** meet the acceptance bar.
- Reality review: the model is not yet normative. It models the plan, not the
  code, and its liveness rests on fairness that reality lacks.
- Rigor review: 10 of 30 new mutants survive, and several checks are vacuous.

The reports are kept with the model's history (ferry-ljc.10). These decisions
answer the modeller's F1–F15 and both reviews, and they bind the next model
and the M1 refactor.

| # | Decision | Answers |
|---|---|---|
| D1 | Inside a Ferry session, every acquire on the same place is a **try**: if contended, refuse with 125 and a next step. A re-hold of a name already held up the tree refuses. Nesting stays usable when uncontended, and cannot deadlock (checked: no deadlock in the reality review's X4b). "Reuse the parent's lease" still deadlocks (X4a). | F1, R7, F15 |
| D2 | **Keepers are the kernel or a durable marker, never a lingering process.** The owner marker (while running) and the taint (after survivors or unknown) name every lock the run holds (seat, host mode, gate, user locks). Every acquirer checks markers before its first try and while waiting. The §2.5 sentence "inheritance cannot stall a lock beyond the grace period" is withdrawn. After a supervisor death or survivors, the named locks refuse until `ferry stop` or recovery, an explicit operator act (PRINCIPLES 4, 6). | F2, F3, R2, rigor 2 |
| D3 | **TaintRespected** is an invariant: no work starts in a seat whose last custody ended in survivors or unknown, until recovered. | F4, rigor 4 |
| D4 | **Liveness is claimed only under stated assumptions:** the recipe ends, its timeout fires, or it is stopped; the client is up or closed (a silence deadline is S11, ljc.18). An operator's stop may fail on stuck processes. No fairness is assumed on a recipe exiting. | R3, rigor 6 |
| D5 | **Exclusive arrivals go oldest live intent first.** The intent file names its arrival time, pid and start time, so two exclusive landers cannot starve each other. This is files only, crash-safe, and not a queue service. | rigor 6, R8 |
| D6 | **Two records, each a state machine.** The caller's receipt and the host record are each reserved before any effect (pid, start time, token) and finalized once, by atomic rename. The caller never takes an outcome from a reserved record. Invariants: RecordsAgree (both final implies the same outcome, or the receipt names the disagreement) and RecordHonest (the outcome matches what happened). | R4, F7, rigor 8 |
| D7 | **Non-interference:** no step of one run changes another run's client, transport or supervisor. Code: each caller owns its SSH connection (2f7ac8ea), and the local host-run gets its own process group. | R5 |
| D8 | **The model labels every configuration as-coded or planned.** macOS as coded (no census) is a configuration expected to fail, and the records say `unsupported` (PRINCIPLES 9). Named residues: work operating on inherited descriptors (`LOCK_UN`, lock conversion; release stays `close()`); work delegated to existing daemons; a state root on a network filesystem (refused); a stopped supervisor; an unrecorded escapee after `ferry stop <dead run>`. | R1, R6, R9–R11, F11 |
| D9 | **Progress properties:** permitted work eventually runs, and shared holders can run concurrently, so a model that refuses or serializes everything fails. | rigor 3 |
| D10 | S9 refuses; there is no "supervisor alive → wait" (it is unreachable once the quarantine check follows lock acquisition). S5 order: mark, then release, then finalize. Intents are on the host lease and user locks; `--quiesce` writes one. The local route's provides are the caller binary's capabilities. A rank refusal is 125. A refused stop gets a record (kind `stop`). Pinned launch refuses with 125 if the described store path is gone. A nested Ferry supervisor is TERMed first by its parent's stop_survivors and releases on TERM. | F6, F8–F10, F12–F14 |

**Model work for round 3:**
- **Configurations:**
  - crash-on liveness (EveryRunSettles, RecordEventuallyFinal);
  - installer liveness (`cfg_pin_live`, `cfg_quiesce_live`, InstallEventuallyDone), with `cfg_pin` reworked so a `--quiesce` install overlaps a live run;
  - two shared user-lock holders;
  - equal-rank plans;
  - a restart that overlaps an escapee;
  - exclusive against exclusive.
- **Tolerances:** no `hidden` on Linux, and `closedfd` only for the slot that closed its own descriptor.
- **Census:** an imperfect census (it may say unknown) instead of an oracle.
- **Invariants:**
  - restate MutualExclusion over the kernel flock table;
  - drop invariants that only restate a guard, or mark them as such.
- **Tests and docs:** fix the S4 and S12 tests and the README.
- **Acceptance:** both reviewers re-run their mutants, and none survives.

## 9. Review findings → decisions

### Reality review

| # | Finding | Decision |
|---|---|---|
| 1 | tainted seat released and reused | S10 refuse plus stop survivors (ljc.16, ljc.22) |
| 2 | sessions don't inherit lock fds | §2.5 inheritance (ljc.17) |
| 3 | S9 auto-recover checks only the session | recovery is explicit, via `ferry stop` (§2.6) |
| 4 | daemons plus keep-holding stall the machine | stop survivors, so every hold is bounded (§2.6) |
| 5 | deadlock and liveness never checked | §8 (no crash for liveness; temporal properties) |
| 6 | nested run or hold deadlocks | nesting refusal (§2.5) |
| 7 | code launches through the symlink; the session uses `current_exe` | pinned launch for host-run and the session leader (§2.2, ljc.2) |
| 8 | requirements skipped on local paths | S1 on every route (ljc.19) |
| 9 | zero or two records; Finish not atomic | the record state machine (§2.7, ljc.21) |
| 10 | no heartbeat deadline | S11 (ljc.18) |
| 11 | three lock roots | one lock space per machine (§2.5) |
| 12 | the quarantine check races the lease wait | quarantine checked after all locks are held (ljc.20) |
| 13 | crash between reserve and bind | the record and owner marker are reserved before any spawn; a marker without a session is recovered by `ferry stop` (ljc.20) |
| 14 | a failed post-flip verify; two install locks | flip only after the verify passes (already so); one install lock, `ferry hold install` (ljc.13) |

### Rigor review

| # | Finding | Decision |
|---|---|---|
| 1 | crash masks deadlock | §8 |
| 2 | shared and u1 exclusion vacuous | wider bounds, §8 |
| 3 | circular physical model | ghost flock, §8 |
| 4 | PinnedLaunch a tautology | ghost start generation, §8 |
| 5 | WaiterProgress untestable; exclusive starvation | restart runs, plus intents (§2.5) |
| 6 | taint unreachable | stop_survivors and taint modelled, §8 |
| 7 | RankOrder unstated | invariant plus out-of-rank plans, §8 |
| 8 | reclaim try-lock untested | release and idle as separate steps, §8 |
| 9 | tracked census ignores fd holders | §2.6: empty requires no other holder of the description |
| 10 | S4, S6, unknown and install not modelled | all are in v2's model |

## 10. Roadmap

Each milestone ships through the design gate (AGENTS.md) and the pipeline:
freeze → own critic → consumer review if the contract changes → land →
qualify → install.

### M0 Safety now (P0)

- **ljc.16** refuse a tainted seat;
- **ljc.17** inherited lock fds;
- **ljc.22** stop survivors.

Built and reviewed now. **Bench install** waits for Herald's Postgres to run
outside custody: an operator service (herald-qmdox, herald-1a's setup).

### M1 Core v2 (the rigor pass)

1. this document;
2. the Quint model, with adversarial round 2 (ljc.10);
3. one Lock, Custodian and Record, plus fault injection (ljc.11);
4. records as a state machine (ljc.21);
5. heartbeat deadline (ljc.18);
6. requirements on every route, and requires unification (ljc.19, ljc.14);
7. one state root and lock space per machine (ljc.4, without load);
8. pinned launch (ljc.2);
9. nesting refusal and intents (new);
10. quarantine order and reserve-before-spawn (ljc.20);
11. `FERRY_TMP` (c3t.13);
12. the dev shell (oln.1).

### M2 Coordination

1. holds on the new core (ljc.1 resumes);
2. the lock ledger and waiting lines (ljc.8);
3. `ferry stop` with explicit recovery (ljc.3);
4. land adoption: Ferry, Murail, Herald (ljc.5, ljc.6, ljc.7);
5. `status` (c3t.10);
6. retire hand holds after a measured clean day (ljc.9).

### M3 Legibility

1. help text (c3t.5), summary line (c3t.6), records UX (c3t.7);
2. what/why/next (c3t.8);
3. `check` (c3t.9), `init` (c3t.11), tables (c3t.12);
4. interface consolidation (ljc.15);
5. REFERENCE.md (c3t.1), README (c3t.2), ARCHITECTURE.md (new);
6. the skill plus Home Manager module (c3t.4);
7. the seat path budget (c3t.3).

### M4 Portability and evidence

1. the macOS tracked tier and Mac host cell (ljc.12);
2. the portable install procedure (ljc.13);
3. load observations and Ferry-owned host facts (ljc.4 load part, jx8);
4. argv follow-ups (4mw).

### Outside Ferry, tracked here

- the `~/.nix-config` pin commit (nrl.1, user);
- bench home hygiene (fod, all seats);
- Herald's Postgres as an operator service (herald-qmdox);
- v2 eviction activation (9zp.1, murail-4a and the user).

### Parked

Retention beyond admission eviction (eon): the operator's retention of records
is a policy question for later, and the default stays "never implicitly".

### Done (records of the night)

These carry over and stay: exit codes, custody (v1), lease, admission v1,
capabilities, the install procedure (ki5).

## 11. Deliberately not doing

- a scheduler, queue service or FIFO fairness: intents give liveness, not
  order;
- keep-alives or service management;
- cgroups or any Linux-only custody mechanism;
- terminal hand-off for interactive holds;
- automatic recovery after a supervisor death;
- cleaning files Ferry did not create;
- an optimistic land inside Ferry (repositories may build one on `hold`).
