# Ferry minimal: one thing, done well

Status: proposed, 2026-10-09. Owner: the ferry seat.

- **Supersedes** `design/2026-10-08-core-model-v3.md` wherever the two differ. v3
  keeps its diagnosis (§1) and its decisions on connections, output and
  records. Its custody machinery (most of §14) is withdrawn here.
- **User decisions, 2026-10-09:**
  - Ferry is not CI and should do one thing well;
  - "the kernel lock decides busy, and anything that might survive is
    reported" replaces "Ferry proves nothing survived" (PRINCIPLES 6, amended
    in 0703077).

## 1. The one thing

> Run this repository's recipe, at this exact commit (even unpushed), on a
> machine I own, in a warm checkout, alone if I ask, and give me an honest
> record.

CI tools assume a forge, a server and a queue. Nix remote builds assume
hermetic derivations: no warm build state, no services, no timing. Ferry
covers the gap: a serverless, agent-driven, warm and exact "remote
`just <recipe>`". Nothing else.

## 2. Where each responsibility lives

| In Ferry | Down to the OS and operator | Up to repositories and CI |
|---|---|---|
| Ship the exact commit, plus declared extra refs | **Containment strength:** the run's session is stopped at its end everywhere; on Linux, optionally `systemd-run --user --scope` for complete cleanup, reported as a tier | What runs and when, fan-out, retries, gating, landing, land locks |
| A warm seat per workspace, reset to the commit (with declared preserved directories) | **Disk:** Ferry keeps only a floor check that refuses; reclaiming is an operator script or timer | Services and databases (operator), notifications, dashboards, history beyond records |
| A seat lock, and an optional exclusive host lease (flock, inherited by the work) | **Installs:** Nix (as today) | Recipe policy: the adapter or `ferry.toml`, and hooks |
| A deadline-bound run, detached from the connection, with explicit stop | | |
| One record and one output file per run; declared artifacts | | |

## 3. Five concepts

1. **Ship.** The exact commit and its declared extra refs go to the place's
   bare repository. Today's companion refs and mirrors become one mechanism,
   "objects to ship". Publication is idempotent (as today).
2. **Seat.** A checkout per (caller, repository, seat name) and a cache
   directory, reset to the commit, with declared preserved directories kept.
   Its lock is the seat lease.
3. **Lock.** Two flock leases: the seat, then the host (shared, or exclusive
   with its gate). Each is inherited by the run's work and released by
   `close()`.
   - **Held means busy.** That is the only busy signal; there are no markers.
   - **Waits:** the seat wait is 0, refusing with the holding run's id. The
     host wait is bounded.
4. **Run.** A detached supervisor on the place (v3 §3.5) that:
   - takes the locks;
   - reruns the floor check;
   - runs the prepare and env hooks, the recipe, then the collect hook, with
     output to `runs/<id>/output.log`;
   - enforces one absolute deadline;
   - at the end, stops the session (and the scope, where there is one);
   - writes the record.

   A dropped connection never ends a run; its deadline or `ferry stop` does.
5. **Record.** Reserved before any effect, then final once (v3 §3.6 and §14
   E3: the caller picks the id; O_EXCL reservation; a no-replace finalize;
   boot id).
   - It also carries custody: `tier` (`scope` | `session`) and `survivors`,
     the processes still holding a lease or still in the scope after the stop,
     where knowable, otherwise `unknown`.
   - The receipt is a pointer plus a copy.

## 4. What the guarantees rest on

| Guarantee | Mechanism | Reported residue |
|---|---|---|
| exact commit | push of the exact objects; a reset checkout | — |
| one run per seat; exclusive host | flock, inherited by the work | work that unlocks its inherited descriptor |
| every run ends | the supervisor's deadline | a supervisor that is killed: reconciled as lost |
| nothing left running | session stop; systemd scope on Linux (tier `scope`) | outside a scope, a descendant that left the session; the record names it if it still holds a lease, otherwise it is not seen |
| busy is honest | a held seat or host lock | a survivor that closed its descriptors does not keep the seat busy (reported, not chased) |
| connection independence | the detached supervisor | — |
| one honest record | reserve, then final once | — |

**Withdrawn from v3** (the custody machinery):

- the seat marker and its scan for exclusive runs (§14 E2);
- the start barrier and the second deadline enforcer (E1);
- the relock probe as a gate (E4): it remains a diagnostic in the record;
- taint files, `seat recover` and `--accept-unknown`;
- census-driven survivor stopping (the subreaper census): replaced by the
  session stop plus the optional scope;
- admission eviction inside runs: becomes the operator's script, run from a
  timer; the floor check remains.

**Kept from v3:**

- detachment and the mandatory deadline;
- output to a file through one writer with a cap (E8);
- the record cycle (E3);
- stop by owner (E9);
- migration (E10), simplified;
- nesting refused on the caller (E13).

## 5. Interface

```
ferry run <recipe> [--on <place>] [--exclusive] [--detach] [-- <args>…]
ferry stop <run>
ferry record [<run> | last]
ferry status [--on <place>]
ferry check  [--on <place>]
ferry recipes · ferry places
```

Exit codes are unchanged (0, the recipe's code, 124, 125, 128+n, 255).

## 6. Consumers (to review with them before cutting)

**Used today and kept:**

- the catalog adapter (Herald and Murail);
- hooks: prepare, env, collect;
- artifacts (Herald's `trace.json`, Murail's), fetched from the final record;
- preserved directories (Herald: `_build`, `deps`, `.devenv`, `.cache`);
- environment `forwards`, `watches` and `local`;
- companion refs (Herald);
- one mirror (Herald's `ese` dependency): both become "objects to ship";
- the provider capability `admission` (Murail): it now means the floor check,
  and eviction moves to the operator. murail-4a has just activated v2
  eviction, which stays until the operator script replaces it.

**Changes for consumers:**

- **Detached runs and a mandatory timeout:** consumers declare timeouts (as
  v3).
- **No `seat recover`:** a busy seat is a held lock, and the record names the
  holder.
- **Disk reclaiming leaves the run path.**

## 7. Roadmap: deletion first

Each slice should remove more code than it adds; the freeze states the net
lines.

1. **Records and the state root** (reserve, final once, reconcile; the
   one-root layout).
2. **The detached supervisor and deadline:**
   - submit, follow and stop, with the old `host-run` kept as a shim;
   - deletes heartbeats, client-lost verdicts, caller grace and
     `supervisor_grace`.
3. **Output to a file:** deletes the relay (`output.rs`, base64, settle,
   accounting).
4. **Custody to session plus scope:** deletes the census, taints, owner and
   idle records, cleanup-pending and `seat recover`.
5. **Admission:** the floor check only; eviction moves to an operator script
   (`scripts/ferry-reclaim`, the same rules, run from a timer).
6. **Ship:** companion refs and mirrors become one "objects to ship".
7. **Migration:** a sentinel for old roots; the shim; capability gating.

The model shrinks with the design: leases, deadline, record, and a two-tier
custody whose residues are tolerances. It is built when budget allows, with
the same two adversarial reviewers.

## 8. Not doing

- Everything in PRINCIPLES "What Ferry is not".
- Proving absence of survivors.
- Queues, retries, fan-out or scheduling.
- Reclaiming disk inside a run.
- A macOS containment mechanism beyond the session.
