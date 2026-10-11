# Ferry specification

**Normative end state.** The M1 slices converge the code onto this document;
`README.md` describes the behaviour shipped today. The rationale and its
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
workspace is gone, and a seat's caches are capped (§9).

## 3. Concepts

| Concept | State | Rule | Why |
|---|---|---|---|
| **Recipe** | `name → {argv, requires, exclusive, timeout, wait, artifacts, preserve}` read from the commit | runs only where `requires ⊆ provides`, on every route | the repository decides what may run |
| **Place** | `local` or an ssh destination; `provides` = declared ∪ detected ∪ binary capabilities; `host.toml` | describes itself through the binary that will run the work | the facts come from the doer |
| **Seat** | `(caller, repo path, name) → checkout + cache` | reset to the commit each run; `preserve` dirs kept | warm and exact, never shared |
| **Lock** | flock: `seat.<key>`, `host`, `host.gate` | held means busy; the work inherits the fds; release by `close()` | the kernel is the only judge |
| **Run** | `id → phase, deadline` | detached from the client; ends by its deadline or `stop` | its lifetime is not the connection's |
| **Record** | `<id>.reserved.json` → `<id>.json` | reserved before any effect, final exactly once | one honest account per attempt |

## 4. Interface

```
ferry run <recipe> [--on <place>] [--exclusive] [--seat <name>] [--commit <rev>]
          [--timeout <dur>] [--wait <dur>] [--detach] [--json] [-- <args>…]
ferry follow <run>                      re-attach: tail the output until the record is final
ferry stop <run> [--other --reason <why>]
ferry record [<run> | last] [--json]
ferry status [--on <place>] [--json]
ferry check  [--on <place>]
ferry seats prune [--on <place>]        retire this caller's seats whose workspace is gone
ferry recipes · ferry places
```

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
         end/timeout/stop: stop session (+ scope) ─► collect artifacts ─► final record ─► close locks ─► exit
caller:  follow output.log by offset until record final ─► receipt ─► exit code
```

**Why:** each step either rests on one mechanism or is reported. No step
depends on the client being present.

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
  `source_rev` and the next step (install the M1 build on that place).
  There is no legacy `host-run` shim: a caller is never installed before
  its places, and both move in one announced window (decided 2026-10-10
  on ferry-ljc.3; it supersedes the shim in ferry-minimal §7).
- **Version skew:** old and new binaries never share a state root; the
  sentinel `locks/legacy-closed` makes old binaries refuse.

**Why:** short idempotent calls survive dropped connections; long-lived
streams do not.

## 7. Custody

```
at end:  stop session (TERM → grace 10 s → KILL) ; Linux + systemd: stop the run's scope
record:  custody = { tier: "scope" | "session",
                     survivors: [{pid, start}] | "unknown" }
```

- A survivor that still holds a lock keeps it held, so the seat stays busy,
  and the next run's busy refusal names the run it outlived. The record does
  not attribute locks to survivors: the kernel lock is the evidence, and
  attributing it would mean reading other processes' descriptors.
- A survivor that closed its descriptors is outside Ferry's knowledge: it is
  reported where it is visible and never chased.

**Why:** proving absence cost more machinery than everything else combined
(ten critic rounds). The OS offers stronger containment as a tier.

## 8. Records and receipts

```
records/<id>.reserved.json  {state:reserved, id, token, commit, seat, binary_sha256,
                            supervisor:{pid,start}, boot_id, submitted_ms}
records/<id>.json           reserved fields + {outcome, exit_code, reason, commit, seat,
                            recipe, args, place:{source_rev, exe, sha256}, lease:{mode, wait_ms,
                            acquired_ms, released_ms}, admission:{floor, available}, custody,
                            output:{bytes, capped}, artifacts:[{path, sha256}], stopped_by?, deadline}
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
- **Live supervisor:** a pid with no recorded start time is treated as alive;
  only proof reconciles.

**Why:** reserve-before-effect, final once and reconcile make "zero or two
records" impossible.

## 9. State (per machine, one root)

```
<state>/ (default ~/.local/state/ferry)       lifetime
  locks/      seat.<key> host host.gate       —
  seats/<key>/checkout/ seats/<key>/cache/    cache  (bounded by Ferry: see below)
  runs/<id>/  request.json output.log artifacts/  record
  records/    <id>.reserved.json <id>.json    record (operator retention)
  store/      repo.git                        cache
<repo>/.ferry/receipts/                       record (the caller's)
```

Nothing else is written; Ferry writes nowhere outside these.

**Bounds on seats (Ferry's own state):**

- **Count:** a seat lives while its caller's workspace does. Each run
  appends `{key, place, repo_path, seat}` to the caller's `seats.jsonl`.
  `ferry seats prune` retires, through the place's `retire` verb, each seat
  whose `repo_path` no longer exists. `retire` only tries the seat lock: a
  busy seat is skipped and reported. It deletes the slot and its cache and
  writes `records/retired-<key>.json`. A caller retires only seats it
  recorded, never another caller's.
- **Size:** at the start of a run, while the run holds the seat lock, if the
  seat's preserve paths plus its cache exceed `seat_cap_bytes`, they are
  wiped and the run starts cold. The record says so (`pruned: {bytes, cap}`).
  Source, logs, receipts and records are never touched.
- **Floor:** below `min_free_bytes` a run refuses with 125, as before.
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
  FERRY_CACHE_DIR FERRY_TMP`, plus `FERRY_EXIT_CODE` for collect.

## 11. Guarantees and residues

| Guarantee | Mechanism | Residue (reported) |
|---|---|---|
| exact commit | pushed objects, reset checkout | a session leaver that closed its fds can write into a later run's checkout and the seat's cache (until the scope tier) |
| one run per seat; exclusive host | inherited flock | work that unlocks its own descriptor |
| every run ends | an absolute deadline | a killed supervisor: reconciled as `lost` |
| nothing left running | session stop; scope tier | outside a scope: a session leaver that closed its fds |
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
9. **A survivor holding a lock:** the seat stays busy; the run's record lists
   the survivor, and the next run's refusal names the run it outlived.
10. **A nested run on the same place:** 125.
11. **Old caller with a new place, and a new caller with an old place:** the
    refusal (125, naming the old `source_rev`), never a mixed state root.
12. **Fault injection** (`FERRY_FAIL_AT`, debug builds) at every fallible
    point keeps 1–3.
13. **The cells:** Darwin and Linux Nix checks, bench, a Mac place.
14. **Seat count:** `ferry seats prune` retires a seat only when its
    workspace is provably gone (`NotFound`, a listable parent on the
    recorded device). An unreadable or unmounted workspace is kept and
    reported. It never touches another caller's seat, and skips a busy one.
15. **Seat size:** over `seat_cap_bytes`, a run starts cold with `pruned`
    recorded. Nothing outside `slots/<key>` and `caches/<key>` is ever
    measured or deleted: not through a symbolic link, not through an
    unvalidated preserve path.
