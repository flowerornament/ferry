# Detach and output to a file (M1 slices 2 and 3)

**Status:** approved with amendments (§10). **Beads:** ferry-ljc.3 (slice 2), ferry-xtx.5
(slice 3), under ferry-xtx. **Implements:** SPEC §5 (run lifecycle), §6
(protocol), §3 Run, §11 "client independence" and "complete output", §12
acceptance 1, 4, 5, 6 and 12. **Builds on:** ferry-ljc.21 (records, 057f9116):
reserve, finalize once, reconcile. **Plan:** `design/2026-10-09-ferry-minimal.md`
§7 items 2 and 3, designed together because the relay is what ties the
supervisor to its client: remove one and the other has no reason to exist.

## 1. The change in one paragraph

A run's supervisor stops talking to its client. The caller asks the place to
**submit** a request and gets an id back within seconds; the supervisor runs
detached (its own session, no terminal, no client pipes) under one absolute
deadline; the recipe's stdout and stderr go through one pipe to one capped
writer into `runs/<id>/output.log`; the caller **follows** that file by byte
offset, reconnecting from the offset it has after any drop, until the record
is final; then it reads the **record** and writes its receipt. A signalled
caller asks the place to **stop** the run. Nothing the client does, except
`stop`, changes how a run ends.

## 2. Place verbs

One hidden subcommand family, run over ssh or as a local exec with
`--state <root>` (the local route keeps `.ferry/local`):

| Verb | In | Out | Bound |
|---|---|---|---|
| `ferry place submit` | request JSON on stdin | `{"id","pid"}` or `{"refused","why"}`; exit 0 / 125 | seconds |
| `ferry place follow <id> <offset>` | — | `output.log` bytes from `offset`; exits 0 once the record is final and every byte before it is sent | the run |
| `ferry place stop <id> --who <w> --why <y>` | — | `{"accepted"}` or `{"refused","why"}` | seconds |
| `ferry place record <id>` | — | the final record, or the reservation (exit 3: not final) | seconds |

`ferry host-info` stays and gains the capability `detach`.

### submit

1. Read and validate the request (today's `admit`).
2. Spawn `ferry place supervise <id>` **paused**: its own session (`setsid`),
   stdin `/dev/null`, stdout and stderr to `runs/<id>/supervisor.log`, and a
   pipe it blocks on before doing anything.
3. Reserve the id **with the supervisor's pid and start time** (slice 1's
   `record::reserve`). The reservation names the process that will finalize,
   so reconcile judges the right process.
4. Write `runs/<id>/request.json` (only after the reservation: a duplicate can
   never overwrite a live run's request).
5. Release the supervisor (write one byte to its pipe) and reply.

On a refusal at 3 the paused child is killed by its exact pid and reaped;
nothing ran. A crash of `submit` between 3 and 5 leaves a paused supervisor
whose pipe is closed: it reads EOF and exits without effect, and its
reservation reconciles to lost (dead pid). **Duplicate id:** the same token
(a caller retrying a submit whose reply it lost) replies with the existing
`{"id","pid"}` and the caller follows it (SPEC §5 "dup id → follow
existing"); another token is refused, as in slice 1.

### supervise

Today's `host_run` after acceptance, minus the client:

- locks, floor check, reset, hooks, recipe, collect, custody release,
  artifacts, `record::finalize` (unchanged order);
- **one absolute deadline** `deadline_ms = submitted_ms + timeout + teardown`,
  where `timeout` is the request's, or the place's `max_timeout` from
  `host.toml` when the request names none (default 6 h; no consumer declares
  a timeout today, so the default keeps them unchanged). Checked in every
  phase that waits: the host lease wait, prepare, the session loop, collect.
  Past it: stop the session, verdict `timeout` 124. Recorded as `deadline_ms`.
- **stop:** the supervisor's stop flag is set by SIGTERM/SIGINT/SIGHUP (as
  today) and by nothing else. There is no heartbeat reader.

### follow

Reads `output.log` from `offset`, writes the bytes to stdout, polls for growth
every 100 ms. It exits 0 when the record is final **and** it has sent every
byte up to the size the file had when the record became final (the writer has
closed the log before finalizing, see §3). It holds no lock and changes
nothing, so any number of followers may come and go.

### stop

Reads the reservation; refuses unless the run is reserved (not final) and its
supervisor is the live process named there (pid **and** start time, the
reconcile test inverted). Writes `runs/<id>/stop.json` `{who, why, at_ms}`
with `create_new` (the first stop wins; later ones are accepted and change
nothing), then sends SIGTERM to that pid. The verdict is `stopped`, exit
128 + 15, with `stopped_by` copied from `stop.json` (SPEC §12.5).

## 3. Output: one pipe, one writer, one file

- Each session (recipe, then collect) gets **one** pipe as both stdout and
  stderr, so the file keeps the order the recipe wrote in. (Separate streams
  cannot be ordered across pipes anyway.)
- One copier thread per session reads the pipe and appends raw bytes to
  `runs/<id>/output.log`. No lines, no splitting, no UTF-8 handling, no
  base64, no queue, no delivery accounting.
- **Cap** `OUTPUT_CAP` (1 GiB): bytes past it are read and discarded, so the
  recipe never blocks; the record says `output: {bytes, capped}`.
- **End:** the copier ends at EOF. After the session stop, a survivor outside
  the session may still hold the pipe: the copier gets `DRAIN` (2 s) more,
  then stops reading and the record notes `output still open after the
  session stop` (a reported residue, SPEC §11). The log is closed before
  `finalize`, which is what lets `follow` end.
- A failed write is counted, and reading goes on (the recipe is never
  blocked); the record notes it.

The caller writes followed bytes to its stdout. With `--json`, stdout carries
only `submitted` and `receipt` events and the bytes go to stderr.

## 4. Caller

`transport::local` and `transport::remote` become one `run_on(place)`, where a
`Place` only knows how to build a verb's command (ssh with today's options, or
a local exec with `--state`). Pushes stay route-specific and happen first.

1. `submit` (bounded, retried once on a transport error; a retry is safe:
   same id and token → follow existing).
2. `follow` from offset 0; on any transport error, reconnect from the bytes
   received so far, with backoff, for as long as the place answered within
   the last `UNREACHABLE` (5 min). Past that the receipt is `lost` 255 with
   `the place stopped answering; the run may go on: ferry record <id>`.
3. `record`: the final record fills the receipt (slice 1's mapping).
4. **Signals:** the first INT/TERM/HUP sends `stop` (who = caller host and
   pid, why = the signal) and keeps following; the run ends by the stop, and
   the receipt is `stopped` 128 + n. A second signal leaves at once: `lost`
   128 + n, `left while the run was stopping; ferry record <id>`.
5. A caller killed outright leaves the run to finish by itself; its submitted
   receipt reads `lost` 255 (slice 1) and `ferry record <id>` shows the
   verdict.

New user commands (SPEC §4): `ferry stop <id>` and `ferry record <id>`. Both
find the place from the receipt's `machine`.

## 5. What is deleted

Production code, measured on main 6c4aa9c6 (lines):

| Item | Lines | Replaced by |
|---|---|---|
| `output.rs` (queue, writer thread, admit, settle, flush, counters) | 310 | copier, ~35 |
| `transport::converse`, `Connection`, heartbeat thread | 121 | follow loop, ~70 |
| `read_stream`, `HostStream`, `spawn_writer`, `offer` | 188 | — |
| `note_delivery`, `supervisor_grace`, `event`, `terminal_signals` | 45 | — |
| `local` + `remote` (two copies of the same flow) | 217 | `run_on`, ~90 |
| `host_run` | 117 | `submit` ~45, `supervise` ~60 |
| `execution::read_output`, `LineReader`, `relay`, `escaped_len`, relay tests | 177 | — |
| the settle block in `run_released`, `limits` delivery constants | 32 | `OUTPUT_CAP`, `UNREACHABLE` |
| receipt `output_dropped`, `output_delivered` | 8 | `output {bytes, capped}` |

Additions: the verbs (~150 with `follow` and `stop`), deadline (~25), `ferry
stop` and `ferry record` (~40).

**Net production lines: about −600** (−1,215 deleted, ~+600 added; the freeze
states the measured number). **Tests:** about 20 relay controls in
`stream_contract.rs` (~900 lines: stalled, slow and closed consumers, line
splitting, base64, queue bounds, tostop, delivery counts) lose their subject;
the new controls (§7) are ~350. **Net test lines: about −550.**

## 6. Contract changes (consumer GO needed: murail-2a, murail-4a, herald-1b)

Herald and Murail call plain `ferry run` and read the exit code and
`.ferry/receipts/`; neither reads `--json` events, `output_dropped`,
`output_delivered` or `output_lines` (checked in herald-1a and murail-2a
sources). What they will see:

1. A caller signal stops the run explicitly, and a **killed** caller no
   longer stops it: the run goes on to its end or deadline.
2. stderr arrives on stdout, in order (with `--json`: on stderr).
3. Receipts: `output: {bytes, capped}` replaces `output_dropped` and
   `output_delivered`; new `deadline_ms`, `stopped_by`; outcome `stopped`.
4. `--json` events: `submitted` and `receipt` only.
5. Every run has a deadline (the place's `max_timeout` by default).

## 7. Design gate

1. **Principles:** 1 (files follow lifetime: `runs/<id>/`), 3 (the place
   describes itself: `detach`), 6 (report residues: output still open,
   unreachable place), 8 (one record; reservation names the supervisor).
2. **Concepts and synchronizations:** Run becomes detached (§3.4); submit →
   reserve-with-supervisor-pid → release; follow ends on final; stop by owner
   (pid + start). Deleted synchronizations: heartbeat, client loss as a stop,
   delivery settle, final emit.
3. **Locks:** none new. The seat lock and host lease are unchanged.
4. **Fallible points (FERRY_FAIL_AT), each with an injection test:**
   `submit-spawn`, `submit-release` (crash after reserve: reconciles lost,
   nothing ran), `follow-drop` (follow exits after its first chunk: the caller
   reconnects, and its byte count equals the file's), `stop-write`,
   `output-write` (kept; now the copier), plus slice 1's four.
5. **Record fields:** `deadline_ms`, `output {bytes, capped}`, `stopped_by`;
   outcome `stopped`.
6. **Capability:** `detach` (host-info).
7. **Cells:** Darwin and Linux Nix checks, bench, a Mac place.

**Controls (SPEC §12):**
- 1: duplicate submit, same token → one run, two followers; another token → 125.
- 4: `follow-drop` → the run completes, the caller re-follows, byte count = file size.
- 5: `ferry stop` → `stopped` 128+15, `stopped_by` set; a caller SIGINT →
  `stopped` 128+2.
- 6: deadline with the caller SIGKILLed → `timeout` 124 in the record; the
  session is stopped.
- Caller killed → the run completes; `ferry record <id>` is final.
- Cap: a recipe writing past `OUTPUT_CAP` (lowered in debug builds) →
  `capped`, the run is not blocked.
- A survivor holding the output pipe cannot keep the run past `DRAIN`.

## 8. Migration (decision needed)

The roadmap kept `host-run` as a shim for old callers. A shim must speak the
old event protocol (output events with base64, heartbeats, host-result), which
is the code this slice deletes; keeping it would make the slice net-positive
until slice 7. **Proposal:** no shim. A new caller requires `detach` from the
place and otherwise refuses 125 naming the install
(`install ferry ≥ <rev> on <place>`); the new binary's `host-run` refuses an
old caller with a `refused` event saying the same. Callers and places are
upgraded together in one announced install window (RELEASE.md), which is how
installs already happen. Slice 7 keeps only the old-root sentinel.

## 9. Open questions for the coordinator

1. Accept §8 (no shim; install window + `detach` gate)?
2. The new outcome `stopped` (SPEC §12.5) versus today's `lost` 128 + n.
3. Default `max_timeout` (proposed 6 h) and `OUTPUT_CAP` (1 GiB).
4. One slice or two freezes: detach first (with the relay still writing the
   file), then the copier? One freeze is net-negative, while detach alone is
   not, because the relay stays.

## 10. Review (coordinator, 2026-10-11, ferry-ljc.3): approved with amendments

**Answers to §9:**
1. **No shim.** A new caller refuses an old place, so no caller is installed
   before its places. bench's host install waits on ferry-8kk.1, so these
   slices may land on main and deploy only in one joint window once 8kk.1
   clears; the Mac caller stays at 3d24d8a1 until then.
2. **Outcome `stopped`:** yes; the exit code stays 128 + n. The stop carries
   the caller's signal, so a `^C` is 130 as before.
3. **Defaults:** `max_timeout` 6 h as a transition, tightened once consumers
   declare timeouts (ferry-8kk.2); `OUTPUT_CAP` 1 GiB.
4. **One freeze,** as a reviewable stack of commits landed together; the
   freeze states the measured net lines.

**Amendments:**
- **A1. Lock waits are bounded apart from the timeout.** Each lock wait (seat
  and host lease) is bounded by `wait` (`--wait-seconds`, default 30 min) and
  refuses 125 naming the holder: an exclusive holder writes its run id into
  the lock file and clears it before release. The deadline is
  `submitted + wait + timeout + teardown`.
- **A2. Stop by owner.** `stop` is allowed for the run's own caller (the
  request's seat key); anyone else needs `--other --reason`, recorded in
  `stop.json`.
- **A3. An unreachable place leaves the receipt `submitted`** (reading `lost`
  255, no verdict claimed) with the note; the caller exits 255, and `ferry
  record <id>` later finalizes it. The same holds for a second caller signal
  and for a submit that gave no reply.
- **A4. `ferry follow <id>`** re-attaches from any terminal, through the same
  code as `run`.
- **A5. Controls:** the wait bound (an exclusive holder → 125 naming it, a
  deadline recorded); a stop from another seat without `--other` refused;
  unreachable → `submitted`, then `ferry record` makes it final.

## 11. Adversarial critic (2026-10-11, on 003d838c): returned, fixed

Findings with reproductions (`/private/tmp/claude-501/ferry-critic-s23-evidence/`),
each now a control with a knockout:

- **C1:** one transient failure after the place accepted a run made the
  receipt final `lost` (remote) or `refused` (local) for good. Now, once the
  run is accepted, only the place's final record gives a verdict. Any other
  failure leaves the receipt `submitted`, exit 255, with `ferry record <id>`.
  A failed artifact transfer is a note on the verdict.
- **C3:** each lock wait had its own `wait` budget, so three waits could
  carry a run about 60 min past its deadline. Now all of a run's lock waits
  end by submitted + wait, so the deadline holds by construction.
- **C2:** a second stop's signal replaced the first's in the verdict. Now the
  first stop decides: its signal is recorded in `stop.json`, and a later stop
  only re-sends that signal.
- **C6:** a signal after the run was final printed "not stopped", exited 255
  and cut the output short. Now the follow delivers every byte, and the
  caller exits with the run's code.
- **T1:** the §12.6 control was ended by its timeout, not the deadline. A
  debug-only `FERRY_DEADLINE_MS` now makes the deadline bind.
- Also: a stop that arrives while the run prepares now spawns nothing.

**Residues, accepted (reported, not chased):**

- `ferry stop` has a pid-reuse window between its liveness check (pid and
  start time) and the kill.
- Stop ownership trusts the caller's `--seat` key: places run as one user,
  so the owner check is advisory, not a security boundary.
- The stored submitted receipt keeps "submitted; no verdict yet"; the
  caller's next step is on stderr and in the `--json` event (R5).

**Not here:** the exit code of a signal before dispatch (lost 128+n against
SPEC §4's lost 255) is ferry-xtx.14, a consumer-visible decision. SPEC §6's
`{refused, why, next}` goes to slice 4a.
