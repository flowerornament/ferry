# Leases minimal (M1 slice 4a)

**Status:** approved with one amendment (§8). **Bead:** ferry-xtx.8, under
ferry-xtx. **Implements:** SPEC §3 Lock, §5 (`seat lock (try; held → refuse
busy)`, `host lease (… bounded wait; names holder)`), §4 refusal "seat busy",
§11 "one run per seat; exclusive host", §12 acceptance 7 and 8. **Builds on:**
slices 2+3 (top 003d838c, frozen and GO'd on code, not yet landed). All
line references below are to 003d838c. **Plan:**
`design/2026-10-09-ferry-minimal.md` §3.3 (Lock) and §7, with the
coordinator's note on ferry-xtx.8 (2026-10-10): 4a gives SPEC's Lock its own
module, `src/lock.rs`.

## 1. The change in one paragraph

A seat is busy exactly when its lock is held, and a run that finds it held
is refused at once (125), naming the holding run, its age and the next step.
It never waits for a seat. Only the host lease waits, bounded by
`--wait-seconds`, and a wait that expires names its holder (as today). Lock
acquisition moves out of `src/execution.rs` into `src/lock.rs`, which has no
knowledge of custody. The work inherits its lock descriptors on every
platform, so whatever outlives a run while still holding them keeps the
seat busy, on a Mac place too.

## 2. What changes

### Seat: try once, refuse busy (SPEC §12.7)

Today the seat lock goes through `acquire_as` (execution.rs:255-318). It
waits up to `request.wait()` (30 min by default), polls every 50 ms and warns
after 3 s. In 4a it is one `try_lock_exclusive`:

- **Free:** take it and write the run's id into the lock file, as today.
  Run ids are already unique.
- **Held:** read the holder's id from the file, and its age from that run's
  record (`records/<id>.reserved.json` or `<id>.json`, `submitted_ms`). Refuse
  with 125:
  - live holder: `seat <name> busy: run <id>, submitted <n> min ago; ferry
    follow <id> or ferry stop <id>`;
  - holder already final, so something it started still holds the lock:
    `seat <name> busy: held after run <id> ended (<outcome>); ferry record
    <id>`. Naming those processes is §12.9, slice 4b; 4a only says the lock
    outlived its run.
  - unreadable or empty holder: `seat <name> busy: held by an unknown run`.

**Why no wait:** a seat belongs to one caller workspace. A busy seat means
that caller already has a run, and waiting behind it hides a duplicate.
Queues are above Ferry (M1 goal). The caller's choice is `--seat <other>`,
`follow` or `stop`.

The seat file is never cleared at release. A stale id in a lock nobody holds
is harmless, because `try` succeeds. While the lock is held, the id names
the run whose work holds it, which is what §12.9 needs.

### Host lease: unchanged behaviour, new home

Shared takes the gate shared, then `host` shared, and drops the gate.
Exclusive takes the gate exclusive and then `host` exclusive, and keeps both.
The wait is bounded by `request.wait()`, and an expired wait names its
holder. Both are as in 003d838c. The exclusive holder still writes its id
and clears it before release (execution.rs:319-329). Shared takers never
write, so a cleared file correctly reads as "shared runs".

### `src/lock.rs`

It owns the Lock concept and nothing else:

```
pub fn seat(root, key, id) -> Result<Held, Busy>     // try once
pub fn host(root, exclusive, id, wait, stop, waiting) -> Result<Lease>
pub struct Held(File)           // a held descriptor; release = drop/close
pub struct Lease { held: Vec<Held>, record: LeaseRecord }   // today's HostLease + Lease
pub fn inherit(files) -> Restore   // clear FD_CLOEXEC for the spawn, then restore
```

Moved from execution.rs: `acquire_as`, `HostLease`, `Lease` (the record
fields `mode`, `wait_ms`, `acquired_ms`, `released_ms`, unchanged),
`close_on_exec`, `inherited`, the clear and restore around `spawn_session`,
and the unit test
`a_lock_is_released_only_when_every_descriptor_is_closed`. That is about
260 lines (execution.rs:119-129, 255-411, 420-445 in part, 705-715, 770-780,
961-987, 1293-1315).

The one custody seam is `waiting`: the closure that rechecks taint while
waiting for the host (execution.rs:385, 393). It stays a parameter of
`lock::host`, so lock.rs does not import custody. 4b deletes taint and,
with it, the parameter. The seat has no wait, so it has no seam.

### Inheritance on every platform

`inherited` (execution.rs:770-780) gives the work its lock descriptors only
on Linux. SPEC §3 says "the work inherits the fds", and §11 rests "one run
per seat" on the inherited flock. In 4a, the work inherits on macOS too: a
flock belongs to the open file description, which fork and exec share on
both kernels. Consequence: on a Mac place, a process that outlives its run
while holding the descriptor keeps the seat busy, and the refusal says so,
where today the next run starts beside it. Question Q2.

## 3. What is deleted

- The seat wait: polling, the 3 s "waiting for lock" warning on the seat,
  and the seat's `waiting` taint hook (execution.rs:424-432).
- The Linux-only branch in `inherited`.
- `request.wait()` no longer applies to the seat. It still bounds the host
  wait and is part of the deadline (unchanged).

Not deleted here, because they are custody (slice 4b, per §7 item 4):
`owners/<seat>.json` and `seat::check`, `locks/cleanup-pending-*`,
`taint/<seat>.json` and its three checks, `idle/<seat>.json`, `seat
recover`, and the census.

**Net production lines: about −40 to −60.** The move is about 260 lines out
and about 240 in; the deletions are the seat wait and the platform branch;
the busy refusal adds about 25. The freeze will state the moves separately
from the deletions (coordinator's note), by measuring the move commit and
the behaviour commit apart.

## 4. Commits

1. **Move, no behaviour change:** create `src/lock.rs`, move the code
   listed, and change execution.rs to call it. The gates pass unchanged,
   and the freeze states it as moved lines.
2. **Behaviour:** seat try-once with the busy refusal; inheritance on every
   platform; README (seat busy), SKILL.md (a busy seat refuses: use another
   `--seat`, or follow or stop the named run), and SPEC citations.
3. **Controls:** tests for §12.7 and §12.8, and the changed tests (§6).

## 5. Contract changes (consumer GO: murail-4a, herald-1b; murail-2a if @flowerornament says so)

1. A busy seat is refused at once, 125, naming the run, its age and the next
   step. Today it waits up to `--wait-seconds`. A consumer that relied on
   the wait (two runs queued on one seat) must use two seats, or queue above
   Ferry.
2. On a Mac place, a process that outlives its run while holding the seat
   lock keeps the seat busy (Q2).

Exit codes and receipt fields are unchanged.

## 6. Design gate

1. **Principles:** 2 (held means busy: the kernel is the only judge); 5
   (every wait bounded: the seat's is 0); 6 (a lock outliving its run is
   reported, not chased).
2. **Concepts and synchronizations:** Lock gets its module. Seat: try or
   refuse. Host: as before. Run inherits both.
3. **Lock ranks** (unchanged, now written down): seat (1) < host.gate (2) <
   host (3). A rank is taken only while holding lower ranks. The seat is
   never waited for. The only wait is for the host, while holding the seat,
   and it is bounded. Admission's reclaim and `seat recover` take a
   seat with try only (admission.rs:259-270, seat.rs:99-114) and hold
   nothing else.
4. **Fallible points:** `seat-lock` (try or read the holder fails → refuse,
   never run) and `host-lock`, as FERRY_FAIL_AT points, each with an
   injection test.
5. **Record fields:** none. The refusal's reason names the holder; `lease`
   is unchanged.
6. **Capability:** none new. The caller cannot tell the difference, because
   the refusal is the supervisor's.
7. **Cells:** Darwin and Linux Nix, bench, a Mac place, at the freeze.

**Controls (SPEC §12), each with a knockout:**

- §12.7 `a_busy_seat_refuses_at_once_naming_the_run`: run A holds the seat,
  quiet, waiting for a file. Run B on the same seat exits 125 within a
  bound (well under `--wait-seconds`). Its reason names A's id, an age, and
  `ferry follow`. Then A is released and ends ran-pass. Knockout: wait
  instead of try → RED (bounded).
- `a_lock_held_after_its_run_keeps_the_seat_busy`, on both kernels: a recipe
  leaves a child that keeps its descriptor (setsid, `sleep`) past the run's
  end. The next run on that seat refuses with "held after run <id> ended".
  The test stops the child by its recorded pid. Knockout: no inheritance on
  macOS → RED on the Mac cell.
- §12.8 stays `an_exclusive_run_waits_for_in_flight_runs_and_keeps_new_ones_out`,
  unchanged (lock file names are unchanged; see Q1).
- Injection: `seat-lock` refuses and starts nothing.

**Tests that change:** `dead_supervisor_refuses_seat_reuse_until_the_recorded_session_is_empty`
(run_contract 718-747: "a new run on the seat waits for it" becomes "is
refused busy"); custody_contract's `next_run_blocks`/`run_blocks` users
(130-158; tests at 186, 266, 600, 639) where the blocked run is on the same
seat. Each is checked one by one: a test whose point is custody keeps its
point, and only "waits 5 s" becomes "refused busy".

## 7. Questions for the coordinator

- **Q1. Lock file names.** SPEC §3 and §9 name them `seat.<key>`, `host`
  and `host.gate`; the code uses `<key>`, `host-exclusive` and
  `host-gate`. The deployed binary (3d24d8a1) uses the same state root,
  `~/.local/state/ferry`, and there is no old-root sentinel yet
  (`locks/legacy-closed` is slice 7). A rename in 4a would let a run of the
  old binary, still in flight during the install window, and a run of the
  new one hold "the same" seat under two names. **Proposal:** 4a keeps
  today's names, and the rename lands with slice 7's sentinel. Alternative:
  amend SPEC §3/§9 to today's names (its own reviewed change). Your call.
- **Q2. Inheritance on macOS.** It makes §11 true on a Mac place, at the
  cost that a session leaver holding the descriptor makes the seat busy
  until it exits. Without `seat recover` (4b deletes it), the operator stops
  it by pid. **Proposal:** yes, as SPEC says. The Mac caller stays at
  3d24d8a1 until the joint window, so no Mac place changes before then.
- **Q3. Order with 4b.** 4a lands before 4b, so for one slice the taint
  checks still run around a try-once seat lock. A tainted seat still
  refuses with its own reason before the lock is tried (execution.rs:420).
  Nothing in 4a depends on 4b. Proposal: as written.

## 8. Review (coordinator, 2026-10-11, ferry-xtx.8): approved with one amendment

**Answers to §7:**

- **Q1:** keep today's lock file names in 4a. Names and layout converge with
  slice 7's sentinel (ferry-xtx.9), together with bench's actual layout
  (`slots/<key>`, `caches/<key>`), which also differs from SPEC §9's
  `seats/<key>/`. Why: renaming a held lock beside a running old binary
  splits one seat into two; a renamed root with a sentinel cannot.
- **Q2:** yes, inheritance on macOS, as SPEC says. A Mac seat kept busy by a
  leaver is the truth, and the refusal says so.
- **Q3:** yes, 4a before 4b.

**Amendment A1: the holder-id race.** The id is written after the lock is
taken and never cleared at release. Between a new holder's lock and its
write, a refused run reads the previous run's id. That run is final, so the
refusal would say "held after run <prev> ended", blaming a finished run for
a live one.

- **Fix:** if the named holder is final, or the file is empty, re-read once
  after a short bounded delay (200 ms). Say "held after run <id> ended" only
  if it is still that final run. Otherwise name the new id, or say "held by
  a run that is starting".
- **Control:** a FERRY_FAIL_AT point, `seat-lock-write`, pauses between lock
  and write, with a refused run during the pause. The refusal does not
  claim the previous run ended. Plus its knockout.
- **Why:** P6, report what you can prove. A refusal naming the wrong run
  sends the caller to `ferry record` on a finished run instead of waiting.

**Process:**

- Contract change 1 (a busy seat refuses at once instead of waiting) goes
  to the consumers (murail-4a, herald-1b) at the 4a freeze. Their GO on 2+3
  does not cover it.
- Build 4a after 2+3 lands, on the landed main.

**Added from the 2+3 critic (2026-10-11):** SPEC §6's refusal shape,
`{refused, why, next}`, lands in 4a. Today `submit` replies
`{"refused": "<text>"}` and the place's refusals are one string. In 4a the
seat-busy refusal (and the place's other refusals, mechanically) reply with
`why` and `next` as separate fields: for example, `why: "seat <name> busy:
run <id>, submitted <n> min ago"`, `next: "ferry follow <id> or ferry stop
<id>"`. The caller prints both and keeps the receipt's `reason` as
`<why>; <next>`. This is a protocol change: the caller reads `why`/`next`
when present and falls back to `refused`, so places and callers can move in
the same window.


## 9. As built (2026-10-11)

- **Busy, by state, not timing (replaces A1's re-read; accepted by the
  coordinator).** The synchronization it rests on: every run reserves its
  record (`records/<id>.reserved.json`, naming its seat) before it touches
  the seat lock, and the supervisor reconciles the seat's reservations
  (`record::reconcile_seat`) before it tries the lock. So when the lock is
  held, every other reservation for the seat is a live run. This proves only
  that a lock holder has a reservation, never that a reservation holds the
  lock: a live run may not have tried it yet. Hence:
  - the lock file names a live reservation: that run holds the seat
    (`seat busy: run <id>, submitted <n> min ago; ferry follow <id> or
    ferry stop <id>`);
  - it names a final run, and no reservation is live: the lock outlived
    that run (`held after run <id> ended (<outcome>) by something it
    started; ferry record <id>`);
  - it names a final run, and a reservation is live: both are possible,
    and both are named (`held after run <id> ended … , or by a run that is
    starting (<ids>)`). A live reservation proves the run is alive, not that
    it holds the lock: it may not have tried it yet. **Correction after the
    freeze:** 90a792a4 named the live reservation alone as the holder;
    murail-4a and herald-1b each showed a finished run's leftover holding
    the lock while a contender had reserved but not tried it (RETURN,
    2026-10-11);
  - it names nothing: a run that is starting, if one is live, or an unknown
    process.
- **Controls:** `a_busy_seat_refuses_at_once_naming_the_run` (§12.7);
  `a_run_refused_while_the_holder_starts_names_both_possible_holders`;
  `record::tests::a_reserved_contender_does_not_prove_it_holds_an_ended_runs_lock`
  (herald-1b's deterministic reproduction, adapted);
  `a_starting_contender_is_not_named_as_a_finished_runs_lock_holder` (macOS;
  murail-4a's reproduction, with a FERRY_FAIL_AT=seat-try pause before the
  lock is tried)
  (FERRY_FAIL_AT=seat-lock-write holds the window open);
  `a_lock_held_after_its_run_keeps_the_seat_busy` (macOS; on Linux the census
  stops such a leaver until 4b); `a_host_refusal_before_acceptance…` now
  checks the place's `why; next`.
- **Size:** src +16 for the move, +87 for the behaviour. Slice 4 is measured
  as one unit: 4b must take src below 5950, the level before 4a (coordinator,
  2026-10-11).
- **SPEC §6:** `refuse(why, next)` replies `{refused, why, next}`; `refused`
  stays for older callers, and the caller joins `why; next` when both are
  present.
- **Host holder, by state (critic L2, 2026-10-11).** `HostLease`'s drop no
  longer clears the exclusive holder's name: the work inherits the lock on
  every platform, so the lock outlives the clearing, and waiters named
  "shared runs" after a full wait. An expired host wait names, by
  `record::host_holder`:
  - a live exclusive run (`ferry follow/stop`);
  - an ended one, "or by shared runs";
  - or shared runs (an empty name).

  Control: `a_host_held_after_an_exclusive_run_names_that_run`.
- **Concurrent refusals (critic L1):**
  `runs_refused_together_behind_a_leaver_each_name_the_ended_run`.
- **Next step for a lock that outlived its run:** "stop what run <id> left
  running (its processes in this seat's checkout)". 4b's survivors
  `{pid, start}` make it exact.
- **The end order is load-bearing (SPEC §5; murail-4a's RETURN on
  7f870775).** The supervisor had closed its locks before finalizing its
  record, the reverse of SPEC §5. That created a window: lock free, file
  naming A, A's reservation live, so a refusal named A while B held the
  lock. The seat lock and host lease are now held until after
  `record::finalize`, on every path, from the moment `prepare` returns
  them; a failed finalize leaves them to close when the supervisor exits,
  and the run reconciles to lost. With that order:
  - a named run whose reservation is live (after reconcile) holds the
    lock;
  - a named run that is final means a leftover of it holds the lock, or a
    run that is starting;
  - the named run is reconciled first (critic L3), so a killed supervisor's
    run is lost before it is named.

  Controls: `a_run_holds_its_seat_until_its_record_is_final`,
  `an_exclusive_run_holds_the_host_until_its_record_is_final` (both with a
  FERRY_FAIL_AT=pre-finalize pause), and
  `a_dead_exclusive_run_is_reconciled_before_it_is_named` (macOS).
  `a_failed_reconcile_leaves_the_receipt_submitted_until_the_next_reader`
  now also shows the seat taking the next run once the supervisor whose
  finalize failed has exited.
- **Shared probe (critic L4).** If `try_lock_shared` on the waited lock
  succeeds, only shared runs hold it: "held by shared runs", alone.
  Otherwise the named exclusive run, live or ended, as above. Control:
  `a_host_held_by_shared_runs_says_so_alone`.
- **Every path (herald-1b's and murail-4a's RETURN on 785949a0).** 785949a0
  held the locks from the moment `prepare` returned Ok. On a refusal inside
  `prepare` after the seat was taken (owner or cleanup-pending checks, the
  host wait, admission, the repository), they still dropped before finalize.
  Now `supervise` owns `lock::Held`, and `prepare` stores each lock in it
  the moment it is taken. A panic anywhere in the supervisor's body is
  caught (`catch_unwind`) and recorded as "supervisor panicked", so `held`
  survives to finalize; `panic=abort` would remove `run_released`'s
  backstop, which custody needs until 4b. A stop during a wait is an `Err`
  from `prepare`, with `held` intact. Controls:
  - `a_prepare_refusal_keeps_its_acquired_seat_until_finalization`
    (herald-1b's 08211dcd);
  - `a_host_wait_refusal_keeps_its_seat_until_finalization`;
  - `a_supervisor_panic_keeps_its_locks_until_finalization`
    (FERRY_FAIL_AT=supervise-panic).
- **What the probes prove (murail-4a's P1 and P2).** A shared probe succeeds
  on a free lock too, and an open error proves nothing. An expired host
  wait now asks, in order:
  - exclusive succeeds: the lock is free now ("released as the wait ended;
    retry"; 125 kept, the budget is spent);
  - shared succeeds: shared holders and no exclusive one ("held by shared
    runs");
  - otherwise exclusive: the named run, by state, as above;
  - any error: "holder unknown (<error>)".

  The probe fd closes at once; it never counts as acquiring. Controls:
  `a_host_lock_freed_as_the_wait_ends_says_so` and
  `a_host_lock_that_cannot_be_probed_names_no_holder` (FERRY_FAIL_AT=host-probe
  pauses before the probe).
- **A partly taken host lease (critic on 785949a0).** An exclusive run that
  takes the gate and then gives up waiting for the host lock kept the gate
  only in a local, so the gate closed before the refusal was final.
  `lock::Held.partial` now owns each host lock as it is taken. A shared
  run's gate is released once its lease is held, by design. Control:
  `a_partly_taken_host_lease_keeps_its_gate_until_finalization`.
- **Residue: a finalize that stalls on disk holds the locks with no bound.**
  Accepted (critic, coordinator): a disk stall stalls everything on the
  place anyway, and the record cannot be final without the disk. Closing
  the locks first would bring back the false-holder window that SPEC §5's
  order removes.
- **`lease.released_ms`** is when the run's use of the lease ended, stamped
  just before the record is final. The descriptors close after the record.
- **Two probes are two observations (herald-1b on 4e88f5a1).** Exclusive
  fails, then the holder releases, then shared succeeds on a free lock, so
  "held by shared runs" could be false. A shared success now says only
  what it proves: "no exclusive holder at the probe: held by shared runs,
  or just released; retry". Control: herald-1b's
  `exclusive_only_contention_never_proves_shared_holders` (11cfa73e).
- **A failed finalize (murail-4a on 4e88f5a1).** `drop(held)` followed a
  finalize error, so the locks closed while the supervisor lived with a
  live reservation. A successor could take the seat, and a third caller
  would name the old run. Now, in one place: `Ok` → `drop(held)`; `Err` →
  warn, then `process::exit` with `held` still open, so it closes with the
  process. The invariant reads: locks close only after the record is final,
  or at process death. Control:
  `a_failed_finalize_keeps_the_locks_until_the_supervisor_exits`, with
  FERRY_FAIL_AT finalize-link and the new finalize-failed pause. The
  existing finalize-link control (the seat is freed once the supervisor
  exits) still passes.
- **Interim residue, Linux only, until 4b (critic on 4e88f5a1).**
  `admission::reclaim`, and `ferry seat recover`, briefly take an idle
  seat's lock. With the seat tried once, a run that starts in that moment is
  refused, and named as "held after run X ended" (X being the last run that
  wrote the file), though the holder is reclaim. 4b deletes both, and
  nothing deploys between 4a and 4b, so there is no code here. Slice 5's
  `retire` writes its own name into the lock file, and busy then says "seat
  being retired" (ferry-xtx.10, with a control).

