# Bounds on seats (M1 slice 5)

**Status:** approved (§9); built after 4b lands. **Bead:** ferry-xtx.10.
**Implements:** SPEC §9 "Bounds on seats" (2c930bed), §4 `ferry seats
prune`, §6 the `retire` verb, §10 `seat_cap_bytes` and `seats.jsonl`.
**Builds on:** 4a (try-once seat, `lock::Held`) and 4b (admission eviction
deleted, custody as the session stop). It is built after 4b lands. Line
references are to main 2c930bed and to 4a's top, 4e88f5a1.

## 1. The change in one paragraph

Ferry bounds its own seats in two ways, each enforced by the party that can
prove it.

- **Count, by the caller:** each run records its seat in the caller's
  `seats.jsonl`. `ferry seats prune` asks each recorded seat's place to
  `retire` it once the workspace (`repo_path`) is gone. `retire` only tries
  the seat lock and skips a busy seat.
- **Size, by the run:** under its own seat lock, before the checkout, a run
  measures its seat's preserve paths and cache. Over `seat_cap_bytes`, it
  wipes them and runs cold, and the record says `pruned: {bytes, cap}`.

There is no daemon, no second lock protocol, and no cross-seat eviction.

## 2. Count: `seats.jsonl` and `ferry seats prune`

- **Record:** after the place accepts a run (submit replied), the caller
  appends one line to `~/.local/state/ferry/seats.jsonl`:
  `{key, place, repo_path, seat, last_used_ms}`.
  - One `write` of one line, with `O_APPEND`, so concurrent callers never
    interleave.
  - A failure is a note on the receipt, never a refusal: the run is not
    the seat book's.
- **Read:** the latest line per `key` wins; `last_used_ms` orders them.
- **Prune:** `ferry seats prune [--on <place>]`.
  1. For each key whose latest line is not a tombstone, and whose
     `repo_path` does not exist on this machine, call `place retire <key>`.
  2. Print one line per seat: retired (bytes), busy (skipped), or failed
     (why).
  3. For each one retired, append a tombstone `{key, retired_ms}`.

  It never rewrites the file, so a concurrent append is never lost. A
  later run on a retired key appends a new line, which revives it, as it
  should.
- **Scope:** only keys this caller recorded. The key already binds the
  caller's host, repository path and seat name (`transport::seat_key`). A
  caller never names another caller's seat.
- **Exit:** 0 when every candidate was retired or busy. 125 if any place
  refused for another reason. 255 if a place was unreachable; the rest still
  run.

## 3. The `retire <key>` place verb (SPEC §6)

- `lock::try_seat` on `locks/<key>`:
  - **Held:** reply `{refused: busy, why, next}`, naming the lock file's
    run as 4a does (`record::busy`).
  - **Free:** keep it while deleting `slots/<key>` and `caches/<key>`
    (today's layout; slice 7 converges names with SPEC §9's
    `seats/<key>/`). Write `records/retired-<key>.json {key, bytes,
    retired_ms, by}` with `write_new`; a second retire of the same key
    names the earlier one. Reply `{retired, bytes}`.
- **The lock file itself is never deleted.** Unlinking a flock file while
  holding it lets a new run create a fresh file of the same name and lock
  it beside us. The file is a few bytes. **Q2.**
- **Capability:** `retire` in host-info. A caller refuses to prune on a
  place without it (125, naming its `source_rev`, as for `detach`).

## 4. Size: `seat_cap_bytes`

- **When:** in `prepare`, after the seat lock and before `reset_checkout`.
  The run owns the seat (`lock::Held`), and nothing of this run has
  started.
- **What is measured:**
  - the declared `preserve` paths inside `slots/<key>`;
  - `caches/<key>`.

  Source, logs, receipts and records are not measured and never touched
  (SPEC §9).
- **Over the cap:** delete those paths, then continue cold. The record
  gains `pruned: {bytes, cap}` and a note in `reason`. The verdict is the
  recipe's.
- **Absent `seat_cap_bytes`:** no cap, and nothing is measured.
- **Cost:** measuring is a directory walk (`symlink_metadata`, no
  following links) on every run start. For a large `_build` or `deps` that
  is many files. **Q3.**

## 5. What it adds and deletes

- **Adds:** a `seats` module in the caller (record, read, prune, about 90
  lines); `place::retire` (about 50); the cap in `prepare` (about 35); the
  `seats prune` CLI (about 15); `pruned` in the record and receipt (about 6).
  **About +200 src.**
- **Deletes:** nothing of its own. The eviction it replaces is deleted in
  4b, which this note assumes has landed. **Q1.**

## 6. Contract changes (consumer GO: murail-4a, herald-1b)

1. A new command, `ferry seats prune`, and a new capability, `retire`.
2. A seat over `seat_cap_bytes` starts cold, and its record says
   `pruned`. Absent the setting, nothing changes. Murail and Herald
   preserve `_build`, `deps`, `.devenv` and `.cache`, which are exactly what
   a cap would wipe; their operators choose the cap.
3. A new caller state file, `~/.local/state/ferry/seats.jsonl`.

## 7. Design gate (expected)

1. **Principles:** 1 (files follow lifetime: a seat lives while its
   workspace does); 2 (busy means held: retire only tries); 6 (each bound
   is reported: retired, busy, pruned).
2. **Concepts and synchronizations:**
   - caller: run accepted → seat recorded;
   - prune: workspace gone → retire under try-lock → tombstone;
   - run: seat locked → measured → wiped if over → checkout.
3. **Locks:** none new. `retire` takes the seat lock with try only, rank
   1, and holds nothing else.
4. **Fallible points**, each with an injection test:
   - `seats-append`: a note, and the run goes on;
   - `retire-delete`: a partial delete, reported and retried by a later
     prune;
   - `cap-wipe`: a note, and the run goes on with what is there;
   - `retire-record`.
5. **Record fields:** `pruned: {bytes, cap}`, and the `retired-<key>.json`
   record.
6. **Capability:** `retire`.
7. **Cells:** all four, at the freeze.

**Controls** (SPEC §12 gains them with this slice), each with a knockout:
- prune retires a seat whose workspace is gone and spares one that exists;
- a busy seat is skipped and reported;
- prune never names another caller's seat;
- a concurrent append during prune survives;
- a seat over the cap runs cold, with `pruned`, and source untouched;
- the lock file survives retire, and a run started during retire either
  waits out the try or takes the seat after it.

## 8. Questions for the coordinator

- **Q1. Size rule.** Slice 5 now only adds (about +200 src); the deletion
  it carried, admission eviction, moved to 4b. Measure 4b+5 together, the
  way 4a+4b are, or accept slice 5 as additive under M1's total (below
  9848 src+tests at exit)? **Proposal:** measure slice 4 (4a+4b) and
  slice 5 as one stretch. 4b's −900 covers both, with room.
- **Q2. Lock files.** Never deleted by retire, as above, so they
  accumulate one small file per retired key. Accept that, or remove them
  in slice 7 under the new root's sentinel? **Proposal:** accept; slice 7
  may sweep keys with a retired record and no slot.
- **Q3. The cap's measurement.** A walk on every run start, or only when
  the seat's last measured size (kept in the seat book) plus the run count
  suggests it? **Proposal:** walk every time; it is exact and simple.
  Revisit if a consumer's cell slows measurably.
- **Q4. Remote `repo_path`.** `repo_path` is the caller's path, checked on
  the caller's machine, which is right: the workspace is the caller's.
  Confirm that prune never consults the place about workspaces.

## 9. Review (coordinator, 2026-10-11, ferry-xtx.10): approved

- **Q1:** slices 4 and 5 are measured together. 4b's deletion must cover
  4a and 5, so slices 4+5 end below 5950 src. The deletion and its
  replacement were split across slices only by sequence.
- **Q2:** `retire` never unlinks lock files: unlinking while locked is a
  real race. Empty lock files are bytes; slice 7's new root sweeps them.
- **Q3:** not on every run. Walking a 30 GB `target/` costs seconds, and
  growth per run is bounded. A run measures at its start only when the
  seat's last measurement is older than 1 h. The stamp is kept in
  `caches/<key>/`, and the measured bytes are recorded. **Bound:** the cap
  can be exceeded by at most an hour of runs' growth.
- **Q4:** `repo_path` is checked only on the caller. The place never sees
  or judges paths; `retire` takes only a key.
- Also approved: `O_APPEND` and tombstones, never rewriting; prune touching
  only recorded keys; the busy refusal naming the run via `record::busy`.


## 10. As built (2026-10-11, on main 6cd551fa after 4b)

- **Seat book** (`src/seats.rs`): as §2. The caller appends after the place
  accepts the run. A failed append is retried when the run completes, if
  the book does not already hold the seat live; a failure then is a
  `seat book:` note on the final receipt. (An append that failed at
  acceptance was otherwise lost: the final record's `reason` replaces the
  submitted receipt's.)
- **`retire <key>`** (`place::retire`): as §3. It tries the lock once and
  writes its own name, `retiring`, into the lock file while it holds it, so
  a run refused meanwhile is told "seat busy: it is being retired (its
  workspace is gone); try again", and a second prune skips it as busy. The
  lock file is never removed (Q2). Deviation: `retired-<key>.json` is
  `{key, bytes, retired_ms}`, without §3's `by`: the place does not know
  its caller's name, and `retire` takes only a key (Q4).
- **The cap** (`admission::cap`, called in `prepare` after admission): as
  §4, measured at most once an hour (Q3). Deviation: the stamp is
  `caches/.measured-<key>`, beside the seat's cache rather than in it,
  because the wipe deletes `caches/<key>` and would take the stamp with it.
- **Records:** `pruned` is kept in the place record and the receipt as the
  JSON the place wrote, like `custody` and `admission` after 4b's R3, so a
  later shape change can never break an older record.
- **Fallible points:** `seats-append`, `retire-delete`, `retire-record` and
  `cap-wipe`, each with an injection test, and the `retire-pause` debug
  point for the busy and concurrency controls.
- **Controls** (`run_contract`):
  - `seats_prune_retires_a_gone_workspaces_seat_and_spares_a_live_one`
    (the lock file survives; place record and tombstone; a second prune is
    a no-op);
  - `seats_prune_never_names_another_callers_seat`;
  - `a_seat_being_retired_is_busy_and_the_book_keeps_concurrent_appends`
    (a second prune skips it as busy; a run on the seat is refused "being
    retired"; a run's line appended meanwhile survives; the same workspace
    path later revives the seat, and prune spares it);
  - `seats_prune_refuses_a_place_without_retire_naming_its_revision`;
  - `a_failed_retire_is_reported_and_a_later_prune_finishes_it`
    (`retire-delete`, `retire-record`);
  - `a_failed_seat_book_append_is_a_note`;
  - `a_seat_over_its_cap_starts_cold_with_pruned` (not measured again
    within the hour; source untouched);
  - `a_failed_cap_wipe_is_a_note_and_the_run_goes_on`.
- **Review of ccafe4ad** (murail-4a RETURN, critic RETURN; folded into one
  refreeze). Deletion is held to "provably inside, provably dead":
  - **P1-a, the cap followed a symlink ancestor** of a preserve path and
    deleted outside the seat. Now `admission::outside` walks each path from
    the state root, component by component, with `lstat`: only plain names
    and no symbolic link on the way. Anything else, including what cannot
    be inspected, is neither measured nor wiped, with a note. `retire`
    checks its two paths the same way and refuses otherwise. The notes
    prepare leaves reach the record even when the checkout then refuses.
  - **P1-b, prune took any stat error for "gone".** Now gone is proven: the
    name is absent (`NotFound`) from a parent that can be listed and is on
    the device recorded with the seat (`parent_device`, new in the book
    line; it tells an unmounted volume's empty mount point apart). Anything
    else is "kept … unknown", reported, never retired.
  - **S2 (P0), an unvalidated preserve path reached the cap's delete.** Now
    `execution::validate` checks `preserve` at submit and again in prepare,
    before anything touches the disk; and `outside` refuses any component
    that is not a plain name at the point of deletion. Two independent
    checks.
  - **P2, the measurement had no bound.** It is skipped, as a note, when
    the run's wait budget is spent, or once it takes over 30 s (stamped all
    the same, so the next try is an hour away). The wipe of what was
    measured is not separately bounded.
  - **Local seats** are no longer recorded (the coordinator's ruling): they
    live inside their workspace and go with it. Prune's local branch is
    deleted.
  - **Residue:** the containment check and the delete are two steps; a
    process that swaps a directory for a symbolic link between them (a
    leaver of an earlier run, holding no lock) is not excluded. The scope
    tier (ferry-xtx.20) removes leavers.
  - Not taken: moving the cap after `reset_checkout` (the critic's optional
    S1 hardening). The walk is the required fix, and moving it would change
    how `git clean` and the cap act on preserve paths.
- **SPEC §12** gains these controls with this slice (§7); the coordinator
  amends SPEC.

## 11. Hardening before deploy (2026-10-11, ferry-xtx.23)

The critic's P2s on 655170bd (GO), by reading; evidence
`ferry-critic-s23-evidence/5-r2/critic5.rs`. Landed before the joint 4b+5
deploy window.

1. **No mount crossing.** `outside` also requires every component to be on
   the state root's device, so a volume mounted inside a seat is not the
   seat's. The size walk counts nothing on another device, and the delete
   (`admission::remove`, replacing `remove_dir_all`, which crosses mounts)
   refuses to descend into one. A test cannot mount, so the control is a
   unit test with a different device standing in.
2. **The check is repeated at the delete.** `admission::remove` proves
   containment again immediately before each delete, for the cap and for
   `retire`. The window after the measurement (up to 30 s) is now the gap
   inside one call. The remaining residue is §10's: a leaver swapping in a
   link within that gap.
3. **Documented:** a symbolic link for `slots/` or `caches/` (an operator
   moving them to another disk) makes every cap and retire on that place
   refuse, safe but inactive (README, SKILL).
4. **Documented, not built:** a workspace deleted together with its parent
   directory is never pruned ("kept … unknown"); its seat stays until an
   operator removes it. A `--forget` for operator-confirmed retirement
   would be a feature, so it waits for the stop rule's call.

