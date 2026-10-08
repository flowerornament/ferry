# Ferry core model v3: a run is a bounded, detached job held by the kernel

Status: proposed, 2026-10-08. Owner: the ferry seat.

- **Supersedes** `design/2026-10-08-core-model-v2.md` wherever the two differ.
  v2 stays the record of the review history: §3's amendment (rules A–C), §8.1
  (D1–D10), and §9's findings table. Its roadmap M1–M4 is replaced by §11 here.
- **User decisions, 2026-10-08:**
  - a run's lifetime is decoupled from the client connection: a detached
    supervisor, a mandatory timeout, an explicit stop;
  - `ferry hold` and user-named locks leave Ferry, and repositories own their
    land locks.
- **Constraints:** `PRINCIPLES.md` (amended alongside this document, §12);
  portable to macOS hosts; Quint models attacked by reality and rigor
  reviewers before any claim.

---

## 1. Why a v3

Ferry was turning into a small operating system. The evidence is where its bugs
lived. Every serious defect from 2026-10-07 to 2026-10-08 sat at one of three
seams. At each one, Ferry promised something that depends on a thing it does
not own.

| Seam | What Ferry promised | Machinery it grew | Evidence |
|---|---|---|---|
| The client connection | a run's fate follows its client | heartbeats; EOF means stop; client-lost verdicts; caller grace and kill; local process groups; `tostop` handling; Connection drop logic | P0 critic rounds 3–8 (ferry-ljc.22); shared-ControlMaster losses (ferry-w4u.1); a 0-byte host record (ferry-ljc.21) |
| The output pipes | every byte relayed live and accounted for | `output.rs`, backpressure, settle (stall, cap, drain), base64, escaping, exact counting, one locked log | P0 rounds 5–10, all of them |
| Process bookkeeping | Ferry knows and records what a run left behind | owner markers, taint files, idle records, cleanup-pending, subtree records, keepers, reconcile | model reviews R2, X5, X7, X8 (v2 §8.1); ferry-ljc.16, ferry-ljc.20 |

v3 does not add care at each seam. It stops standing in them. **Each guarantee
rests on one kernel mechanism or one file, and Ferry declines the rest** (§7).

## 2. What Ferry is (unchanged in spirit)

Ferry runs a repository's recipe at an exact commit, on a place that can run
it. It keeps the runs that must not overlap apart, and it records exactly what
happened. It owns three things:

- **leases:** the seat and the host lease;
- **the release of a run:** stopping what the run started;
- **the record:** what happened.

The project owns what runs and how. The operator owns host policy, services and
installs. Repositories own their land laws and land locks.

## 3. Six concepts

Concepts never read each other's state. The synchronizations in §4 compose
them.

### 3.1 Recipe: what a project allows to run

- **State:** `name → {argv, requires, exclusive, timeout, wait}`, read from the
  contract at the exact commit.
- **New:** `timeout` is mandatory in effect. A recipe may declare one; the
  default is 60 min; host policy may cap it (`max_timeout`). `wait` bounds how
  long a run waits for its leases (default: the timeout).
- **OP:** a run of R happens only on a place providing everything R requires,
  on every route, the local one included. Otherwise it is refused before any
  effect.

### 3.2 Place: where work happens

- **State:** name; address (`local`, or an ssh destination); provides
  (declared, detected, and the capabilities of the binary that will run the
  work); policy (`host.toml`); installed generations.
- **Actions:** `describe`, `check`; `install` and `rollback` are operator acts.
- **OP:** a place describes itself through the binary that will run the work,
  and the run starts from that generation (pinned launch).

### 3.3 Seat: a warm working tree for one workspace

- **State:** `seat → {checkout, cache, marker: none | busy{run, supervisor pid, start}}`.
- **OP:**
  - one run at a time per seat (the seat lease);
  - the checkout is reset to the exact commit at every run;
  - a busy marker whose supervisor is gone means the seat refuses until the
    run is stopped (`ferry stop <run>`). This is the only marker Ferry keeps
    (§7).

### 3.4 Lease: exclusion by the kernel

- **State:** per place, `<state>/locks/`, two locks only:
  - `seat.<key>`, exclusive;
  - the host lease `host` plus its `host.gate`: shared for every run,
    exclusive for an `exclusive` recipe.
- **Order:** seat, then host. There are no ranks, no user locks, and no intents
  beyond the gate.
- **Inheritance:** lease descriptors are inherited by the run's work, so a
  dead supervisor cannot free them while the work lives. Release is `close()`,
  never `LOCK_UN`.
- **Waiting:** bounded by the recipe's `wait`. While waiting, the run prints a
  line naming the holder (Linux: `/proc/locks`; macOS: the holder's run id from
  its seat marker or the lease's run file). Refusal on expiry is 125.
- **OP:** while a lease is held exclusively, no other run holds it. That rests
  on flock alone: "held" is whatever the kernel says.

### 3.5 Run: one attempt, detached and bounded

- **State:** `id → {work, place, phase, deadline}`. The phases are
  `submitted → waiting → running → releasing → final`.
- **Actions:** `submit`, `follow`, `stop`.
- **OP:**
  - every run ends by its deadline (the wait bound plus the timeout, plus the
    stop grace), whatever happens to any client;
  - a dropped connection never stops a run;
  - stopping is explicit;
  - a run inside a Ferry run on the same place refuses with 125.
- **Release** (formerly the Custody concept, now a step of the run):
  1. After the work and the collect hook, stop what the run started: its
     session and process groups everywhere, plus, on Linux, the subreaper
     census and TERM, 10 s, KILL.
  2. Whatever still holds a lease descriptor keeps that lease held (the
     kernel), and the record names it.
  3. If custody is unknown (no census on this platform and something may
     remain, or a stop failed), the busy marker stays, and the seat refuses
     until `ferry stop`.

### 3.6 Record: one authoritative account per run

- **State:** on the place, `records/<id>.reserved.json` (id, token, supervisor
  pid and start time, submitted time), then `records/<id>.json`, final. Each is
  written by atomic rename.
- **Actions:**
  - `reserve` before any effect;
  - `finalize` once;
  - `reconcile`: a reservation whose supervisor is provably gone (pid plus
    start time) is finalized as lost/255.
- **The caller's receipt** (`.ferry/receipts/<id>.json`) is a pointer: id,
  place, commit, plus a copy of the final record once the caller has read it
  (now, or later through `ferry record <id>`). The host record is the truth.
- **Schema:** fields are only added; an absent field means unknown.

## 4. Synchronizations

```
S1  submit(recipe, place)   requires ⊆ provides (every route) else refuse 125; push the exact commit (idempotent)
S2  submit on the place     reserve record {pid,start} → fork a detached supervisor (own session, no tty,
                            stdio to the run dir) from the described generation → print the id
S3  supervisor              nesting check (FERRY_RUN on the same place → 125) → seat lease → seat marker check
                            (busy, supervisor gone → 125, next: ferry stop) → write busy marker → host lease
                            (gate) → admission → checkout
S4  work                    recipe argv + args; stdout and stderr → runs/<id>/output.log (kernel-written);
                            the deadline is enforced by the supervisor
S5  release                 collect hook (appends to output.log, bounded) → stop what the run started → census
                            (Linux) → clear the marker if custody is known → finalize the record → close the
                            leases → exit
S6  follow(id)              tail output.log from an offset until the record is final; a pure reader; reconnecting
                            resumes from the offset
S7  stop(id)                signal the supervisor (pid+start from the reservation) → it stops the work →
                            S5 → lost 128+signal, with who stopped it; a dead supervisor's run is reconciled
                            and its marker cleared
S8  any failure after spawn recorded; never skips S5 (fault injection at every fallible point)
S9  reservation, supervisor gone   reconcile → final lost/255 (by status, record, or the next run on the seat)
S10 install(rev)            install-lock try → build → check → flip (operator act; --quiesce takes the host lease exclusively)
```

What disappears from v2:

- **S11** (heartbeat deadline): there is no client to time out.
- **Rules C and D** (output relay and accounting): the log is a file.
- **Intents, ranks, user locks, and the nesting of holds.**
- **Taint, owner, idle and cleanup-pending records,** replaced by one seat
  marker.

## 5. Interface

```
ferry run <recipe> [--on <place>] [--detach] [-- <args>…]   submit, then follow; prints the id
                                                           Ctrl-C asks: stop the run (y) or detach (n)
ferry stop <run>                                           explicit stop; records who stopped it
ferry record [<run> | last]                                the final record (waits while it runs)
ferry status [--on <place>]                                runs, leases and holders, seats, disk, installs
ferry check  [--on <place>]                                is this place usable for this repository?
ferry recipes · ferry places
```

**Exit codes are unchanged:**

| Code | Meaning |
|---|---|
| 0 | pass |
| N | the recipe's own code |
| 124 | timeout |
| 125 | refused, nothing ran |
| 128+signal | stopped |
| 255 | lost: the supervisor died, reconciled |

`run` exits with the code of the final record. If it can no longer follow, it
exits 255 with the next step `ferry record <id>`, and the run continues.

**Internal verbs on the place:** `ferry submit` (reads the request; prints the
id), `ferry follow <id> [--from <offset>]`, `ferry supervise <id>`.

## 6. State on a machine

The same layout on every machine; there is no per-repository `.ferry/local`
lock space.

```
<state>/                                   default ~/.local/state/ferry
  locks/   seat.<key>  host  host.gate                     flock only
  seats/<key>/  marker.json  checkout/  cache/             cache lifetime (marker: run lifetime)
  runs/<id>/    request.json  output.log  supervisor.log   output and logs: record lifetime
  records/      <id>.reserved.json  <id>.json              record lifetime
  store/        repo.git  mirrors/  companion-refs/        cache lifetime
  installs/<rev>  current  previous  install.lock          operator acts
~/.config/ferry/host.toml                                  operator policy (floor, eviction, max_timeout)
<repo>/.ferry/receipts/<id>.json                           the caller's pointers
```

## 7. Each guarantee, its one mechanism, its residue

| Guarantee | Mechanism | Residue (reported, not hidden) |
|---|---|---|
| exact commit | git push of the exact object; checkout reset with index checks | none known |
| one run per seat | flock on `seat.<key>`, inherited by the work | work that unlocks its inherited descriptor (`LOCK_UN`) |
| exclusive host | flock gate plus `host`, inherited | work delegated to existing daemons (nix-daemon, docker); other users |
| every run ends | supervisor deadline (wait + timeout + grace) | a supervisor that is itself stopped (SIGSTOP) or killed: reconciled as lost |
| nothing left running | session and process-group stop; Linux subreaper census | macOS: an escapee into a new session that closed its descriptors; any platform: after a supervisor crash, an escapee that closed its descriptors (the seat marker refuses until `ferry stop`) |
| output is complete | the kernel writes the file | output written after release by a survivor not stopped |
| one honest record | reserve, then final once by rename; reconcile by pid and start time | none known |
| connection independence | detached supervisor (own session, no tty) | none |

## 8. Portability

Everything above uses POSIX only: fork, setsid, flock, rename, signals. Linux
adds the subreaper census, which strengthens "nothing left running". macOS
gets the same meaning with the residue in §7, recorded as `custody:
session-only`; the kqueue "tracked" tier remains a later strengthening. The
environment matrix stays a gate: the Darwin and Linux Nix check sandboxes,
bench, and a Mac host.

## 9. What v3 deletes

The size of the deletion is the point of v3.

- **From the caller:** heartbeats; `converse`/`read_stream` event parsing for
  output; the record fallback; the Connection grace and kill; the local
  process group; `supervisor_grace`.
- **From the host:** the stdin reader thread; `output.rs` (writer, queues,
  settle, drain, gate, counts); the output events (base64, escaping); verdict
  branches for client loss; owner, taint, idle and cleanup-pending files; the
  taint checks threaded through the lock waits.
- **From the design:** S11, rules C and D, intents, ranks, user locks, the
  nesting rules for holds, RecordsAgree.

**What stays from P0 (ferry-ljc.22):** stopping survivors (subreaper census,
TERM, grace, KILL), lock inheritance, the verdict-once rule (B), fault
injection (`FERRY_FAIL_AT`), and the controls that exercise them.

## 10. Formal model (round 4)

Derived from round 3 (`scratchpad/quint.r3`):

- **Remove:** client links, output, user locks, intents, the nesting of holds,
  and the two-records agreement.
- **Add:** the detached supervisor and its deadline; the single seat marker;
  the record reserve, final and reconcile cycle; explicit stop.
- **Keep:** the kernel flock table as ghost truth; the census as imperfect;
  the residues of §7 as tolerances, each labelled; as-coded configurations
  expected to fail.

The model should be markedly smaller. Its reviewers are the same reality and
rigor agents, with the same acceptance bar (v2 §8): no surviving mutant, and no
unmodelled reality gap that matters for an invariant.

**Invariants:**

- SeatAlone;
- ExclusiveQuiet;
- MutualExclusion (over the kernel table);
- NoReleaseWhileWorkLives (the kernel, or the marker);
- OneRecord (reserved, then final once);
- RecordHonest;
- PinnedLaunch;
- RequirementsHold;
- NoNestedRun.

**Temporal properties:**

- EveryRunEnds (by its deadline, with no fairness assumed of clients or
  recipes);
- LeasesEventuallyFree (assuming the work ends, which the deadline and the
  stop ensure, except for the §7 residues);
- PermittedEventuallyRuns.

## 11. Roadmap (replaces v2 §10)

- **M0, safety now:** the P0 stack lands after the consumer reviews (ac753768),
  then installs on the Mac; bench installs after ferry-8kk.1. No further work on
  its connection and output paths.
- **M1, v3 core** (one reviewed change per item, each through the design gate):
  1. this document and its model (round 4) with the adversarial reviews;
  2. records (reserve, final by rename, reconcile) and one state root per
     machine (§6);
  3. the detached supervisor: `submit`, `supervise`, `follow`, `stop`; the
     mandatory timeout and wait bound; pinned launch;
  4. output to a file; following by offset;
  5. leases: seat and host, the holder named while waiting, inheritance,
     release by close; the nesting refusal;
  6. release: the stop, the census, the seat marker; requirements on every
     route;
  7. migration: read the old state once, write the new layout, and refuse
     mixed versions on one state root (lock files renamed so binaries that
     cannot read markers never share them);
  8. the dev shell for the recipes (ferry-oln.1).
- **M2, legibility:** `status` and `record` that a person can read; the
  what/why/next refusal format; `check`; `init`; REFERENCE.md, ARCHITECTURE.md,
  README; the skill.
- **M3, portability and evidence:** the macOS tracked tier and a Mac host
  cell; the portable install procedure; host facts.
- **Dropped with `hold`:** the coordination epic's holds, ledger, waiting lines
  and land adoption. Repositories own land locks; Ferry's `jj-land.sh` is the
  worked example.

## 12. Principles touched

Amended in the same landing, as its own change:

- **Principle 2** gains: "A dropped connection never ends a run; its deadline
  or an explicit stop does."
- **Principle 5** gains: "Each guarantee rests on one kernel mechanism or one
  file."
- **"What Ferry is not"** gains: "a lock service: repositories own their land
  and coordination locks."

## 13. Open questions

1. **Ctrl-C default.** Should `ferry run` ask "stop or detach?", or should
   Ctrl-C stop and a second Ctrl-C detach? Agents never see a prompt, so for
   them Ctrl-C (SIGINT) stops and SIGHUP detaches.
2. **Log size.** A file can grow without bound for a chatty, long run. Options:
   leave it to the disk floor (admission), or set an `RLIMIT_FSIZE`-style cap
   policy. It is a record-lifetime file, so this is the operator's retention
   question.
3. **Stream separation.** One merged `output.log` keeps order. Should stderr
   be marked, for example with a second file `stderr.log` written by a `tee`?
   The default here is one file.
4. **Abandoned runs.** A dead agent's run continues to its deadline. Should
   `status` flag runs with no follower for a long time, or is the deadline
   enough?
5. **Herald and Murail adapters.** They consume receipts and output events
   today. The migration needs their review (contract change): receipts become
   pointers to final records, and output is the file.
