# Custody is the session stop (M1 slice 4b)

**Status:** proposed, for coordinator review. **Bead:** ferry-xtx.11, under
ferry-xtx, carrying ferry-ljc.11's remaining scope. **Implements:** SPEC §7
(custody), §11 "nothing left running", §12 acceptance 9, and the release
half of 12. **Builds on:** slice 4a (frozen at 90a792a4; GO'd on code, not
yet landed). Line references are to 90a792a4. **Plan:**
`design/2026-10-09-ferry-minimal.md` §4 ("Withdrawn from v3") and §7 item 4.
The coordinator's note on ferry-xtx.8 says 4b also gives SPEC §3 Run its own
module, `src/run.rs`. **Size rule:** slice 4 is measured as one unit (4a+4b),
and 4b must take src below 5950, the level before 4a (coordinator,
2026-10-11).

## 1. The change in one paragraph

At the end of a run, Ferry stops the run's session: TERM, a 10 s grace, then
KILL (SPEC §7). It records what it can see: the tier, `session`, and the
session members still alive after the KILL, by pid and start time. It
proves nothing beyond that. There is no census, no subreaper, no taint, no
owner marker, no idle record, no cleanup-pending quarantine, and no `ferry
seat recover`. A process that leaves the session while keeping its lock
descriptors keeps the seat busy, and the next run's refusal names the run it
outlived (4a). A process that leaves and closes its descriptors is outside
Ferry's knowledge: reported where visible, never chased.

## 2. What custody records

```
custody: { tier: "session",
           survivors: [{pid, start}] | "unknown" }
```

- `survivors` are the members of the run's session still alive after the
  KILL, found as today: every pid whose session id is the run's
  (`session_members`, `process_ids`). It is `"unknown"` when the session
  cannot be enumerated, for example when the process table is unreadable.
- SPEC §7's `holds: [lock…]` per survivor is not computed. Naming which
  process holds an inherited flock needs `/proc/<pid>/fdinfo` scans or
  `lsof`, which is census machinery under another name. Instead, the next
  run's busy refusal reports a lock that outlived its run (4a), and the
  record says "survivors". **Q1.**
- `tier: "scope"` (a systemd scope on Linux) is not built here: no code
  exists for it today, and it adds code. **Q2.**
- Release failures are notes, as now: the verdict never changes after it is
  reached (rule B).

## 3. What is deleted (about 900 src lines)

- `src/custody.rs`, except a new `Release { tier, survivors }` of about 20
  lines. Gone: the census, `supervise` (the subreaper), `reap_adopted`,
  taint (paths, `check`, `check_host`, `check_exclusive`, writing), survivor
  stopping (`stop`, `attributed`, `parents`, `send`), `held_locks`, and the
  idle record. About −530.
- `src/seat.rs`: owner markers (`check`, `reserve`, `bind`), `recover`,
  `recover_owner`, `recover_taint`, `recover_hint`. `start_time` moves to
  `record.rs`, where reconcile and `place stop` use it. The file goes.
  About −236.
- `src/main.rs`: `ferry seat recover` and `--accept-unknown`. About −56.
- `src/execution.rs`:
  - `SessionOwner` loses its quarantine and ownership fields, and the
    cleanup-pending file.
  - `prepare` loses the taint checks, `seat::check`, `custody::begin` and the
    cleanup-pending scan.
  - `zombie` goes; it exists only for the subreaper.
  - `run_released` keeps the stop, minus the idle/taint branch.
  - About −55.
- `src/lock.rs`: the `waiting` callback of `acquire_as`, whose only callers
  pass taint checks. About −15.
- Admission eviction, if Q3 says so: about −290.

## 4. What is kept or changed

- **The session stop**, `SessionOwner::cleanup_session`:
  - TERM to every session member, wait up to the grace, KILL, wait.
  - Its grace becomes SPEC §7's 10 s (`STOP_GRACE`), instead of
    `SESSION_CLEANUP`'s 5 s.
  - `TEARDOWN` is recomputed from what remains; the deadline control's
    exact sum follows it.
- **An unconfirmed stop** (members still alive after the KILL) is recorded:
  survivors listed, a note in `reason`, the verdict kept. It no longer
  quarantines the host. Its survivors keep their locks if they hold them,
  so the seat is busy only if they do.
- **`src/run.rs` (a move, measured apart):** the session spawn, signalling,
  the deadline loop and the output copier leave `src/execution.rs` (SPEC §3
  Run). `execution.rs` keeps the request, validation, checkout and hooks.
- **Fault injection, ferry-ljc.11's remaining scope (v3 S8):** any failure
  after spawn is recorded and never skips release. Each fallible point of
  release gets a FERRY_FAIL_AT point and a control. The verdict stands and
  the record is final:
  - `session-term`: the TERM fails; KILL still runs.
  - `session-kill`: survivors are recorded, with a note.
  - `collect`: as today.
  - `finalize-link` and `fsync`: as today.

  v2's "runtime rank check" is not carried: v3 removed ranks (§4: "no
  intents, ranks, user locks"), and lock order is written down in 4a's note
  §6.

## 5. Tests

- `tests/custody_contract.rs` goes, except
  `locks_reach_the_recipe_but_not_the_supervisors_other_children`, which
  moves to run_contract. Its `run_blocks` helper with a fixed 5 s bound goes
  with it; it flaked once on bench under load.
- run_contract:
  - Rewritten for the new custody:
    - `unconfirmed_cleanup_quarantines_the_host_before_dispatch` becomes "an
      unconfirmed stop is recorded and quarantines nothing".
    - `dead_supervisor_refuses_seat_reuse…` loses its owner-marker and
      `seat recover` steps.
  - Deleted: `a_normal_release_records_the_seat_idle…`,
    `an_escaped_descendant_is_stopped_at_release…`, `a_blind_diagnostic…`,
    `a_tainted_seat_refuses_until_recovery…`, and
    `a_recipe_can_empty_a_process_group_it_killed` (subreaper zombies).
  - Kept: `a_term_ignoring_survivor_is_killed_after_the_grace_period`, now
    "a session member that ignores TERM is killed after 10 s", and
    `collect_runs_before_survivors_are_stopped…`.
- stream_contract:
  - `a_failed_teardown_after_the_verdict_is_a_note` drops `cleanup_pending`.
  - `a_taint_that_may_hold_the_host_refuses_before_any_wait` goes.
  - `a_survivor_holding_the_output_pipe…` moves from `stop-survivors` to
    `session-kill`.
- New controls, each with a knockout:
  - §12.9, a survivor holding a lock: a recipe's setsid child keeps the seat
    lock. The record lists it as a survivor, and the next run is refused,
    naming the run it outlived. Both kernels: with the census gone, Linux
    behaves like the Mac.
  - A session member that ignores TERM is KILLed after the grace and
    recorded as stopped, not as a survivor.
  - Each release injection point leaves a final record with its verdict
    and a note.

## 6. Contract changes (consumer GO: murail-4a, herald-1b)

1. `ferry seat recover` is gone. A busy seat is a held lock; the refusal
   names the run, and the operator stops a leftover process by pid.
2. Taint refusals are gone, as are the owner-marker and cleanup-pending
   refusals. Survivors are reported in the record, never as a refusal.
3. The receipt's `custody` changes shape: `{tier, survivors}` replaces
   `{census, record, stopped, after, error}`.
4. Linux no longer stops processes that leave the session; they are
   reported if still in it, and keep the seat busy if they hold its lock.
   The scope tier (Q2) is what would restore stopping them.
5. Admission eviction, depending on Q3.

## 7. Design gate (expected)

1. **Principles:** 1 (files follow lifetime: four marker kinds gone); 2
   (held means busy: the only busy signal); 6 (residues reported, not
   chased); rule B (release failures are notes).
2. **Concepts and synchronizations:** custody becomes the session stop plus
   a record. Run gets its module. Release never skips the stop or the
   record.
3. **Locks:** none new; ranks as 4a.
4. **Fallible points:** `session-term` and `session-kill` are new; collect,
   finalize-link and fsync are kept. Each has an injection test.
5. **Record fields:** `custody` changes shape (§6.3).
6. **Capability:** custody's shape changed. A new caller reads `{tier,
   survivors}`, an old one reads unknown fields as absent. Install happens
   in one window, as for 2+3, so no capability is needed. Q4.
7. **Cells:** all four, at the freeze.

## 8. Questions for the coordinator

- **Q1. `holds`.** Record survivors without per-lock attribution, and
  amend SPEC §7 to drop `holds`? (The busy refusal covers the lock case.)
  Or compute holds on Linux from `/proc/<pid>/fdinfo`? **Proposal:** drop
  it; amend SPEC §7 first.
- **Q2. The scope tier.** Build it here, or as its own slice later?
  **Proposal:** later, as its own bead. 4b is the deletion slice, and the
  scope adds code and a systemd dependency to test on bench.
- **Q3. Admission eviction** reads only idle, owner and taint records, all
  deleted here, so v2 eviction would never find a candidate. murail-4a
  activated it. Options:
  - (a) delete eviction here, and slice 5's operator script replaces it;
  - (b) give eviction a new candidate source until slice 5: a seat whose
    lock is free, aged by its last final record (about +30);
  - (c) keep the idle record alone until slice 5.

  **Proposal:** (b). It keeps murail-4a's eviction working, and slice 5
  deletes it together with the rest of eviction. Needs murail-4a's view.
- **Q4.** Custody's shape changes in the receipt. Is the one-window
  install enough, or do you want a capability name for it?

## 9. Review (coordinator, 2026-10-11, ferry-xtx.11): approved for building after 4a lands

- **Q1:** drop `holds`. SPEC §7 and §12.9 were amended at 874b2445. Survivors
  are `{pid, start}`. A survivor that holds a lock shows up as the next run's
  busy refusal, naming the run it outlived.
- **Q2:** the scope tier comes later, as its own bead. 4b is the deletion
  slice.
- **Q3:** option (a). Admission eviction is deleted here, and slice 5's
  operator reclaim script replaces it. Option (b) would add about 30 lines
  of interim code that slice 5 deletes again, and nothing deploys between
  4b and 5. **Constraint (on ferry-xtx.10):** no build containing 4b
  deploys unless slice 5's reclaim script is qualified on bench in the same
  window. The installed host (26ec25b5) keeps its v2 eviction until then.
  murail-4a, who activated eviction, is told.
- **Q4:** the one-window install is enough, so no capability name. The
  receipt's custody shape change goes to the consumers at the 4b freeze.

## 10. As built (2026-10-11)

- **Custody record:** `custody: {tier: "session", survivors: [{pid,
  start}] | "unknown", error?}`. `start` is a `u64`, because serde cannot
  buffer a `u128` through an untagged enum. The new §12.9 control caught
  this as a parse failure on the first run. The receipt keeps custody as
  JSON, so stored receipts of the old shape still load.
- **Session stop** (`run::SessionOwner::stop`): TERM to every member, up to
  `STOP_GRACE` (10 s), KILL, then up to `KILL_GRACE` (2 s), killing again
  whatever is left. The members left are the survivors, without any whose
  start can no longer be read: it has gone (review R6). The stop never gives
  up early (review R4): every member is signalled and each error kept; a
  failed listing signals the leader's process group instead; the KILL and
  the leader's reaping always follow. The errors are the custody `error`,
  and the survivors are `"unknown"` only when the final listing fails.
  Debug points:
  - `session-term`: the TERM fails; KILL still runs;
  - `session-kill`: the KILL is skipped, so a TERM-ignoring member survives;
  - `session-list`: every listing in the stop fails;
  - `session-signal-one`: the stop's first KILL to a member fails, once.
- **Old records always parse** (review R3): a place record keeps `custody`
  and `admission` as the JSON its place wrote, so a run finalized by the 4a
  binary still completes under `ferry record`.
  `SESSION_CLEANUP` is gone; `TEARDOWN` is recomputed from the two stops.
- **Deleted:**
  - the census, subreaper and `reap_adopted`;
  - taint, owner and idle records, and cleanup-pending;
  - `seat.rs` (its `start_time` moved to `record.rs`) and `ferry seat
    recover`;
  - the taint hooks in `lock.rs` (`acquire_as` lost its `waiting`);
  - admission eviction. A version 2 policy refuses with "keep
    `min_free_bytes` as version 1", and the receipt's `admission` is now
    `{policy, floor_bytes, filesystem, available, result}`;
  - `custody_contract.rs`. Its lock-inheritance test moved to
    `stream_contract`, witnessed by a `git` wrapper (Linux), since the
    census's `ps` is gone.
- **Only runs take seat locks** (besides the probes, which close at once).
  4a's interim reclaim and `seat recover` residue is gone with them.
- **`src/run.rs`** (a separate commit, no behaviour change; +24 lines of
  header and imports): the session owner and stop, session listing and
  signals, the run and collect sessions with the deadline, the copier, the
  verdict, and `execute_session`.
- **4a's leaver controls run on both kernels now:** Linux no longer stops a
  setsid leaver either.
- **Size:** src 6260 after 4a → 5126 after the behaviour → 5150 after the
  move: 4b −1110, so slices 4a+4b come to −800. Slice 5's estimated +200
  leaves slices 4–5 near 5350, below 5950.
- **Residue (review R5):** a session leaver that closed its descriptors
  holds no lock, so the next run takes the seat while it lives. It can then
  write into that run's checkout after `reset_checkout`'s integrity check,
  and into the seat's cache. Ferry neither sees nor reports it. The scope
  tier (ferry-xtx.20) is its cure. Proposed SPEC §11 row, replacing the
  `exact commit` row (for the coordinator to amend):

  | exact commit | pushed objects, reset checkout | a session leaver that closed its fds can write into a later run's checkout and the seat's cache (until the scope tier) |
