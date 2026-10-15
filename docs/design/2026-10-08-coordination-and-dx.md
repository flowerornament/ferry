# Ferry: coordination without people, and a usable surface

Status: proposed, 2026-10-08. Owner: the ferry seat (coordinator, `ferry-1a`), with the implementer in `ferry-1b`.
Epics: `ferry-ljc` (no hand coordination), `ferry-c3t` (general product and DX), `ferry-nrl` (install), `ferry-oln` (honest outcomes), `ferry-9zp` (admission).
Source of the request: the user via murail-2a, hail `1008T004154-60f4`. The design shape was approved by murail-2a (`1008T004616-87a1`) and herald-1a (`1008T004627-0216`), and tightened by herald-1b (`1008T004631-0c1c`, `1008T004542-969a`) and murail-4a (`1008T004724-c479`). The working agreements are in murail master `997c62a5`, `.design/implementation/specs/2026-10-08-working-agreements.md` (A1 to A7).

This spec covers two things:

- removing every hand-coordinated window (landing, installing, measuring) from the seats' working day;
- making Ferry legible to a newcomer.

It treats each item on its own (sections 4 to 6) and then all of them together (section 7: locks, custody, receipts, capabilities, compatibility and environments as one system). Section 8 orders the work, and section 9 maps it to beads.

---

## 1. Why

On 2026-10-07/08 about eight agent seats shared one laptop and one build host (`bench`). Whenever two of them could interfere, a seat acted as a manual lock over hail. Murail-2a's census in the working-agreements spec counts the cost:

- 667 messages to 2a, 68% of them progress fyi;
- 50 review round-trips;
- 14 window requests;
- 33 stale holds.

Concrete incidents from that night:

| Incident | Evidence |
|---|---|
| Landings raced `master`. Each loser re-ran 2–6 min of gates ("master advanced"). | murail-5xjeq; murail-4a `1008T004838-c6be` (a land completed 5 gates in 492 s, then refused) |
| Ferry installs needed broadcast minutes, because a run whose `host-info` preceded a flip and whose `host-run` followed it reported lost. | windows at 22:04Z, 22:21Z, 23:29Z, 00:20Z; README "host binary changed during run" |
| A host upgrade changed process parentage under a project hook; it was rolled back at 23:38Z. | ferry-9zp.2; herald-1a `1007T233552-0c12`; fix `660c187c` |
| A quiet laptop window for a timing run never reached its load target. | murail-kyx2h.1; `.design/research/2026-10-07-laptop-quiet-window.md` (murail) |
| A pattern `pkill` risked other seats' processes, twice. | ferry AGENTS.md, Processes; memory `kill-by-pid-only` |
| Disk filled on bench, and seats were swept by hand. | murail-98y2f; now `host.toml` v1 is live and v2 is prepared |

The principle (agreement A1): **whatever must be serialized takes a lock inside the tool that performs the effect.** The lock releases itself when its holder dies (flock), and it records every wait and refusal in a receipt. No seat holds another by message. There is no scheduler, daemon, timer or service.

## 2. Ferry today (baseline, `main` = `fac305a0`)

```
caller (Mac)  ferry run ──ssh──► host (bench)  ferry host-run ─► execute-session ─► recipe
```

| Area | Where | What it does today |
|---|---|---|
| Run pipeline | `src/main.rs` `dispatch`, `src/transport.rs` `remote`/`local`/`host_run`, `src/execution.rs` `prepare`/`run_session`/`execute_session` | Exact commit pushed to the host's bare repo; typed events; heartbeats; one receipt per attempt |
| Outcomes | README "Outcomes and exit codes" | ran-pass 0, ran-fail (the recipe's code), timeout 124, refused 125, lost 128+signal or 255; typed pre-acceptance refusal (`refused` event) |
| Seat lock | `execution::prepare` → `locks/<seat>` | One session per seat |
| Host lease | `execution::HostLease`, `locks/host-gate` + `locks/host-exclusive` | Shared for every run; `--exclusive` / `requires=["exclusive"]` runs alone; acquired/released stamped |
| Custody | `src/custody.rs` | Linux child subreaper; `reap_adopted` during a session; at release the census `waitpid` ECHILD → idle, StillAlive → taint, error → unknown |
| Admission | `src/admission.rs`, `~/.config/ferry/host.toml` | v1 floor guard (live on bench); v2 evicts idle `caches/<seat>/<target>` (not activated) |
| Capabilities | `receipt::CAPABILITIES`, `[provider] requires` | `args`, `refusal-event`, `custody`, `lease`, `admission`; checked by the caller, by `host-info` and by `host-run` |
| Install | `scripts/ferry-install`, `scripts/qualify-install`, RELEASE.md | Nix generations under `installs/<rev>` (GC roots), `current`/`previous`, flock'd, verified, rollback |
| Holds (P1, in review) | `src/hold.rs` (freeze `6ed4216e`) | `ferry hold`, `ferry locks`; RETURNed (F1–F3, R1), see §4.1 |

Live: bench host `26ec25b5`; Mac caller `936d1157` (identical source except a test). Receipt `1791419884993-a798091a` joins both.

## 3. Requirements

These are murail-2a's seven, numbered as in `1008T004154-60f4`, with the reviewers' tightenings.

- **R1 Landing.** Publications to one repository serialize themselves. Rebase onto the newest trunk happens inside the serialization, gates run once on the exact new tree, and a conflict refuses to its owner. The guarantee is cooperative: only publishers that take the same lock are covered (herald-1b, murail-4a).
- **R2 Install.** Installing or rolling back Ferry needs no announcement. No run is lost to a flip, and in-flight runs keep the executable they started with (murail-4a: retry only a typed mismatch proven before acceptance; never re-dispatch an accepted or lost run).
- **R3 Measure.** A measurement gets exclusivity by asking. Its receipt proves which Ferry runs were excluded and records named load observations. Nothing outside Ferry is claimed quiet (herald-1b, murail-4a).
- **R4 Disk.** Done for v1. v2 activation belongs to murail-4a and the user; it is out of this spec except where it interacts.
- **R5 Mid-flight change.** An upgrade or rollback never changes what an in-flight or queued run is promised without the receipt saying so.
- **R6 Ambient environment.** Forbidden environment is refused at admission. For routed runs this already holds (Murail's catalog watches `CARGO_BUILD_`, murail-4a `1008T005710-fb77`). Local land is Murail-owned (murail-l5ngh). **No Ferry change.**
- **R7 Process control.** No tool ever needs a pattern kill. Every run and hold is stoppable by its own handle, and only by its owner unless explicitly overridden (agreement A7).

The user added a cross-cutting requirement (2026-10-08): **D. Ferry should be a general product with documented interfaces**, so a newcomer can learn it from config to daily operations, and it ships a skill like hail, anneal and peat do.

---

## 4. Coordination, item by item (`ferry-ljc`)

### 4.1 P1: holds (`ferry-ljc.1`, in review)

**Interface:** `ferry hold <name>|--repo-land [--exclusive (default)|--shared] [--json] -- <cmd...>`, and `ferry locks [--json]`.

**Semantics as built (`6ed4216e`):**

- **Gate.** The lock reuses the host lease's two-flock gate, factored as `execution::gated()`. Once an exclusive waiter holds the gate, later arrivals cannot extend its wait. There is no FIFO claim and no "nobody starves" claim.
- **Custody.** The lock descriptor is inherited by the child (CLOEXEC cleared). The hold calls `custody::supervise()` before spawning the child in its own process group, and forwards INT/TERM/HUP to that group. After the child exits it keeps holding until the census is Empty (on Linux) and the group is gone.
- **Diagnostics.** Sidecars (`holds/<name>.d/<pid>.json`) and `ferry locks` are diagnostics, not authority. A dead pid or a mismatched start time marks a sidecar stale.
- **Identity.** `--repo-land` gives `land:<store-dir>-<12 hex of the canonical .jj/repo store>`, the same name from every workspace of one repository.
- **Ordering.** A hold refuses (125) inside a Ferry session (`FERRY_ID` set): holds are outer, the host lease is inner.

**Returned, to amend before GO:**

| # | Finding | Required change | Control |
|---|---|---|---|
| F1 | A post-spawn sidecar write failure (EISDIR) returns via `?` and releases the lock while the child lives (murail-4a, executed on bench, `1008T012347-4457`) | After spawn nothing may skip the wait and census drain. Diagnostics are best-effort; other errors are recorded and keep the lock held (unknown) | 4a's repro as a contract test; knockout: `?` restored → RED |
| F2 | `released_ms` is stamped after the descriptors drop | Stamp it while the lock is still held, just before dropping it (as `HostLease::release`) | serialize test: `released_ms(A) <= acquired_ms(B)` |
| F3 | Census `Unknown` has no control | A debug-build-only injection (`cfg(debug_assertions)`, `FERRY_TEST_CENSUS_UNKNOWN`) | hold stays held; receipt records `unknown` |
| R1h | Pre-admission refusals (bad name, `FERRY_ID`) write no receipt (herald-1b, `1008T012405-2b82`) | Every refusal authors a receipt: stored, and printed with `--json`; no lock, no command | exact JSON plus a stored file for both cases |
| C1 | Same class as F1 at `hold.rs` :400/:406/:446, including a draining variant where a command chmods its sidecar directory and leaves a survivor: the hold exits 0 with custody null (own critic, executed on Mac and bench, verdict on ferry-ljc.1) | Covered by F1's fix; the draining path too | critic test `critic_sidecar_failure_after_spawn_does_not_abandon_the_command`, adopted |
| C2 | **Interactive commands hang.** The child runs in its own process group without the terminal, so a prompt (a `jj describe` editor, an ssh host-key prompt, sudo, a pager) stops it with SIGTTIN, and the hold holds forever (critic, `script -q /dev/null ferry hold t4 -- sh -c 'read x'`) | When stdin is a terminal, make the child's group the foreground process group (`tcsetpgrp`, with SIGTTOU ignored in the hold) and restore the hold's group afterwards. Wait with WUNTRACED: a stopped child is reported on stderr and in the receipt, and is never mistaken for a running one | a pty test: a child reading stdin receives input and exits; knockout: no `tcsetpgrp` → it hangs (bounded) → RED |
| C3 | **No control exercises the Linux subreaper for holds.** Knockout K1 (`supervise()` removed) leaves the suite green on bench | Adopt the critic's test: a `setsid` descendant that closes every descriptor above 2 and outlives the command must keep the hold held until it exits (Linux) | `critic_census_keeps_lock_for_setsid_descendant_without_descriptor`; knockout K1 → RED |
| C4 | Descriptors drop at the end of the `Ok(held)` arm, before `released_ms`, the receipt, sidecar removal and `--json` output; a receipt-write failure after a successful command exits 125 with the lock already free. The comment at :371 is wrong | One release sequence: stamp → write receipt (best-effort, warn) → remove sidecar → drop the descriptors last. A diagnostic failure never changes the exit code of a command that ran | the ordering assertion (F2), plus a receipts-dir failure that keeps the child's exit code |

**Done when:**

- the amendment covering F1–F3, R1h and C1–C4 is frozen;
- my own critic (A2) and the consumer reviewers (murail-2a, murail-4a, herald-1b) give GO by execution;
- it is landed, qualified with `qualify-install`, and installed.

### 4.2 Adoption of holds (`ferry-ljc.5`, `.6`, `.7`)

- **Ferry's own land** (`scripts/jj-land.sh`): wrap the pinned candidate, gate, bookmark move and push in `ferry hold --repo-land --exclusive -- …`.
  - The script's existing moved-trunk refusal stays as a fail-closed assertion, which becomes unreachable when every publisher holds the lock.
  - If `ferry hold` is unavailable (an older caller), it prints a warning and proceeds unlocked. That covers only the bootstrap window, and the warning names it.
- **Murail** (murail-4a): the lock goes inside canonical `cargo xtask land`, not only around `just land`. Rebase only under the lock; refuse conflicts; gate the exact new tree through push.
- **Herald** (herald-1b): every `just land` (normal and fan-in) holds `land:herald`. Rebase inside the hold; keep the moved-master refusal as an assertion.
- **Retiring hand holds** (`ferry-ljc.9`): after adoption, verify with murail's `scripts/hail-census.py` that no land, install or measure window appears in hail for a working day. Then delete the corresponding rules from the seats' AGENTS.md files (herald-1b: "delete only after the controls prove the boundary").

### 4.3 P2: installs without windows (`ferry-ljc.2`, redesigned)

Taking both reviews together, the original "install holds the exclusive host lease" is not needed for correctness, and its wait can last as long as the longest run (herald-1b, murail-4a). The race behind every install window is narrower: the caller asks `host-info` (which binary?), and later starts `host-run` through the **`~/.local/bin/ferry` symlink**, which may have flipped in between.

**Design: a pinned launcher.**

1. `host-info` gains `launcher`: the canonical path of the Nix store wrapper it was started through (`/nix/store/<hash>-ferry-<v>/bin/ferry`, the wrapper's own path, not `.ferry-wrapped`). The new capability is `pinned-launch`.
2. When the host advertises `pinned-launch`, the caller starts `host-run` through `launcher` instead of the configured `binary`. A flip between `host-info` and `host-run` can then no longer change the executable. The existing post-run check ("host binary changed during run", `transport::accept_result`) stays as a backstop.
3. `host-run` reports its `binary_sha256` in `accepted`. If it differs from `host-info` (which should be impossible under 2), the caller treats it as lost with the existing reason: it is never retried after acceptance (murail-4a).
4. **Retention.** `scripts/ferry-install` never deletes a generation. A store path a caller pinned stays rooted while it is `current` or `previous`. Pruning older generations is a separate, explicit operator command (out of scope), and it must refuse while a session runs under that generation.
5. **Quiesce (opt-in).** `scripts/ferry-install host <ssh> <rev> --quiesce` runs the flip under the host lease held exclusively, using `ferry host-lease --exclusive -- <flip>`, a new hidden subcommand that takes the host lease and runs a command. Use it only when an upgrade changes on-disk state formats. The receipt-free flip is otherwise instantaneous.
6. **Bootstrap.** The first install of a `pinned-launch` binary happens without it, because the caller pins only when the host advertises it. Callers learn per host from `host-info`, so mixed fleets are safe.
7. **Docs.** Delete "announce a window" from AGENTS.md and RELEASE.md once 1–3 are installed on both ends.

**Controls:**

- an ssh-shim host whose `current` flips between `host-info` and `host-run` (scripted in the shim): the run passes on the pinned binary, and its receipt shows that binary;
- knockout (the caller uses the configured `binary`): lost "host binary changed";
- an old host without the capability: unchanged behaviour;
- `--quiesce` waits for a running shared session (WouldBlock probe), then flips.

**R5:** an in-flight run keeps its executable (POSIX). A queued run starts on whatever is current, and its receipt names `host_source_rev`, `host_exe` and capabilities; a project that needs a behaviour requires it (`[provider] requires`) and is refused, not downgraded.

### 4.4 P3: stopping by handle (`ferry-ljc.3`)

`ferry stop <run-id|prefix|hold:<name>> [--reason <text>] [--other]`

- **Run (local or remote).** Find the receipt for the id: machine, seat, request. On the host (directly, or via ssh `ferry host-stop <id>`), find the session's owner record (`owners/<seat>.json`, which gains the supervisor pid and start time). Then:
  1. **Prefer the supervisor.** Send SIGTERM to the supervisor pid if its start time matches. The supervisor runs its normal teardown (session group TERM then KILL, collection, custody release, receipt) and records the outcome lost, 143, reason "stopped by <workspace>: <reason>".
  2. **Supervisor gone.** If the supervisor is already gone (owner marker sticky), signal the session group by session id plus birth time (`seat::recover`'s check), then tell the user to run `ferry seat recover`.
  3. **Recycled processes.** A stale handle (pid recycled, start time differs) refuses and never signals.
- **Hold.** Signal the hold process (sidecar pid plus start time). It forwards to its group and drains as usual.
- **Ownership (A7).** The request and the owner record carry the caller's workspace (`jj workspace root` name, or the git top-level name). `ferry stop` refuses a run or hold started from another workspace unless `--other --reason …`. The stopped run's receipt records who stopped it and why. Agent seats can then end their own work freely, and ending someone else's is an explicit, recorded act. The README tells a hung holder's neighbours to ask, or to use `--other` with a reason.
- **Capability:** `stop` (the host must implement `host-stop`).
- **Exit codes of `ferry stop`:** 0 signalled and confirmed ended; 125 refused (not found, not yours without `--other`, stale handle); 1 signalled but not confirmed within the grace period (prints the next step).
- **Controls:**
  - local and remote stop end a sleeping recipe: the receipt reads lost 143 "stopped by …";
  - a stale handle (a recorded start time altered) refuses and signals nothing;
  - another workspace refuses without `--other`;
  - a hold stop releases after its drain;
  - knockout: skip the start-time check, and the stale-handle control goes RED.

### 4.5 P4: one lease per machine, and load observations (`ferry-ljc.4`; absorbs part of `ferry-jx8`)

- **Machine-wide lease.** Local sessions today take the lease under the repository's `.ferry/local/locks`, so `--local --exclusive` excludes only that repository. Move the **lease files only** (`host-gate`, `host-exclusive`) to the user's state root on the machine (`~/.local/state/ferry/locks/`). That is where host sessions already take theirs, so local runs and host runs on one machine share one lease. Seats, slots, caches, idle and taint records stay per repository: equal seat names in different repositories never collide (murail-4a).
- **Load observations.** At acquire and at release, the lease record gains `load: {acquired: {loadavg_1, loadavg_5, loadavg_15, cpus, at_ms}, released: {...}}`. The sources are `/proc/loadavg` on Linux and `sysctl vm.loadavg` on macOS. These are two named samples, not proof of sustained quiet; the README says so. A measurement that needs an interval bound observes it itself.
- **Host facts.** Ferry records only host facts it owns: os, arch, cpus, kernel release, ferry rev. Tool versions (`rustc -vV`, etc.) are project facts, recorded by a project's hooks or recipe. `ferry-jx8` is narrowed accordingly.
- **Controls:**
  - two repositories' local runs, one `--exclusive`: they exclude each other (log order);
  - equal seat names in two repositories do not share slots;
  - both load samples are present and ordered;
  - knockout (lease under the per-repo root): the cross-repo exclusion goes RED.

### 4.6 The lock ledger (`ferry-ljc.8`)

**Visibility, for every lock.** Today waits on the seat lock and the host lease are silent: a run looks hung. Generalize P1's sidecars into one **ledger** used by every Ferry lock (hold, seat, host lease, install). The ledger is `locks/<lock>.d/<pid>.json` = {pid, start, workspace, purpose, run id, mode, state: waiting|holding|draining, since}. It is best-effort and diagnostic only, like P1's.

- Every wait prints one line naming the current holders, e.g. `waiting for the host lease (exclusive) held by murail-4b check-jit, run …a798, since 00:41:12Z`. It prints again once per minute while waiting.
- Receipts record `waited_behind` for each lock waited on (seat, lease, hold).
- `ferry status` (§5.5) reads the ledger.
- **Controls:**
  - a run waiting on another's seat prints the holder's workspace and run id;
  - a stale ledger entry (dead pid) never blocks and is shown as stale;
  - ledger write failure never affects locking (knockout: make the write fatal, and a lock test goes RED).

---

## 5. Developer experience, item by item (`ferry-c3t`)

The walkthrough that produced these was given to the user on 2026-10-08. The guiding model, for README and skill alike:

```
ferry = make target + ssh + exact commit + a receipt
three files:  ferry.toml (project)   machines.toml (you)   host.toml (host operator)
three verbs:  run                    look                  fix
```

### 5.1 Help text everywhere (`ferry-c3t.5`)

**Today:** every subcommand and flag except `--exclusive` shows a blank description (`ferry --help`, `ferry run --help`).

**Change:** add clap doc comments to all commands, arguments and flags. `ferry --help` lists commands grouped as run / look / fix / host / install.

**Control:** a test asserts that no visible command or flag has an empty help string. It walks the clap `Command` tree in a unit test.

### 5.2 Human output in receipt vocabulary (`ferry-c3t.6`)

**Today:** `ferry: RanPass:  (receipt 1791…)`. That is Rust `Debug` formatting, with a stray colon and the reason missing.

**Change:** one summary line on stderr, in the receipt's own words:

```
ferry: ran-pass  smoke @ 603debb on bench · 12.3s · seat default · lease shared (waited 0s) · receipt …ef74112e
ferry: refused   unknown recipe "nosuch" · next: ferry recipes · receipt …5a23d2f3
```

The `--json` event stream is unchanged.

**Control:** golden tests for each outcome's line.

### 5.3 Receipts you can read (`ferry-c3t.7`)

- `ferry receipt last | <prefix> | <id>`: a sectioned human rendering by default:
  - outcome and exit code;
  - commit, recipe, args;
  - machine, with caller and host source and executable;
  - lease, admission, custody;
  - reason and next.
  
  `--json` prints the stored JSON verbatim.
- `ferry receipts [-n N] [--json]`: a table of recent runs (time, outcome, recipe, machine, seat, duration, id prefix).
- An ambiguous prefix refuses (125) and lists the candidates.
- **Compatibility:** consumers read `.ferry/receipts/*.json` files directly, and that file format is unchanged. Before switching `ferry receipt`'s default output, confirm that no consumer script parses the command's stdout (grep Murail's and Herald's scripts). If any does, it moves to `--json` first.

### 5.4 Refusals that say what to do (`ferry-c3t.8`)

One format everywhere, adopted from Herald's runtime `next_action` lines:

```
ferry: refused (125): <what>
  why:  <the fact that caused it>
  next: <one command or action>
```

Receipts gain `next` (string or null). Every refusal site sets it:
- seat cleanup pending → `ferry seat recover <seat> [--on <host>]`;
- unknown recipe → `ferry recipes`;
- host lacks a capability → `scripts/ferry-install host <ssh> <rev>` or `ferry doctor --on <host>`;
- below the host floor → `ferry status --on <host>`;
- … (the full table lives in the reference doc).

**Control:** every refusal-producing test asserts that `next` is non-null. Knockout: drop one `next`, and the test goes RED.

### 5.5 `ferry status [--on <host>]` (`ferry-c3t.10`; absorbs `ferry locks`)

One screen answering "what is going on":
- the installed source revision (caller and host);
- runs in flight (from the ledger and owner records);
- host lease holders and waiters;
- holds;
- seats with idle, taint or cleanup-pending state;
- disk against the floor, and the `host.toml` version.

For a remote host it uses a hidden `ferry host-status` that returns JSON (capability `status`). `ferry locks` remains as an alias for the holds part. Depends on the ledger (§4.6).

### 5.6 `ferry doctor [--on <host>]` (`ferry-c3t.9`)

Checks with a verdict and a next step each. Exit code 0 means healthy, 1 means problems found.

- `machines.toml` parses, with no unknown keys;
- ssh reachable (BatchMode);
- host `host-info`: binary, revision, capabilities;
- caller and host revision drift;
- `host.toml` parses (version, floor);
- disk against the floor;
- any `cleanup-pending` marker;
- the seat path budget: warn when `<state>/slots/<24-hex>-<seat>/` plus a typical socket suffix exceeds 100 bytes (ferry-c3t.3; Herald's Postgres 107-byte socket refusal, receipt `1791417477524-a788b9fc`);
- on the caller, the `~/.nix-config` pin matches the running caller (`scripts/ferry-install verify-caller` logic).

### 5.7 `ferry init`, plus tables for `recipes` and `machines` (`ferry-c3t.11`, `.12`)

- `ferry init` writes a minimal `ferry.toml`, offering the recipes found in the justfile (`just --summary`) as candidates, each `routable = false` until chosen. It refuses if a `ferry.toml` exists.
- `ferry recipes` and `ferry machines` print tables by default and JSON with `--json`. The same consumer check applies as in §5.3.

### 5.8 Documentation, and the skill (`ferry-c3t.1`, `.2`, `.4`)

- **Reference** (`REFERENCE.md`, c3t.1): `ferry.toml` and the adapter contract; `machines.toml`; `host.toml`; the hook environment (`FERRY_*`); the receipt schema with field meanings and since-which-revision; JSON events; outcomes and exit codes; capabilities; the state layout; the lock taxonomy and ordering (§7.1); and the refusal → next table.
- **README** (c3t.2): restructured as the model, the three files, the three verbs, a worked example outside Herald and Murail, then "advanced" (companion refs, mirrors, custody, admission, capabilities, holds) linking to the reference.
- **Skill** (c3t.4), the same pattern as hail, anneal and peat:
  - `skills/ferry/SKILL.md`, with front matter name, description and metadata (version, os, requires bins `ferry ssh git`);
  - `nix/home-manager.nix` exporting `homeManagerModules.default` with `programs.ferry.{enable, skill.enable, skill.targets}`, plus the flake's `skillsDir`;
  - `~/.nix-config/home/ferry.nix` enabling it (this replaces the ferry line in `packages/nix/cli.nix`).
  
  The skill covers when to use `ferry run`, reading receipts, every refusal and its next step, holds, status, stop, and what never to do:
  - run a raw `jj git push`;
  - kill by pattern;
  - install outside `scripts/ferry-install`;
  - take a hold inside a session.
  
  It is written after 5.1–5.5 so it documents shipped commands.

---

## 6. Platform and environment items

### 6.1 One development environment (`ferry-oln.1`, renamed)

- **Problem.** Bench's `test` recipe runs `cargo test` without `jj`, so `published_companion_uses_tracking_ref_on_git_jj_and_worktree_callers` fails outside Nix, which has made "41/42" normal. Tonight's Linux-only failures differed from the Nix check sandbox: no `setsid`, `env -i` without `sleep`, and no `ps` in the Darwin sandbox.
- **Change.**
  - Add `devShells.default` to `flake.nix`: the toolchain from `rust-toolchain.toml`, plus `git`, `jujutsu`, `python3`, `inetutils`, and `procps`/`util-linux` on Linux. It sets `GIT_CEILING_DIRECTORIES` to the parent of the checkout (the `~/.git` trap, AGENTS.md).
  - `ferry.toml`'s `test` recipe becomes `nix develop -c cargo test --locked`.
  - Add a `check` recipe (`nix develop -c just check`) so pedantic clippy also runs on Linux.
- **Rule for tests.** A test that needs a platform facility asserts its presence and fails loudly (exit 97 style), or is `cfg`-scoped with a one-line reason. It never silently tests nothing (`936d1157`, `83e3acf5`).

### 6.2 Remaining open items (kept, not redesigned here)

- `ferry-nrl.1`: commit the ferry pin in `~/.nix-config`. This is the user's call: their repository, with unrelated WIP in `home/remotes.nix`.
- `ferry-4mw`: args in the host record; a NUL-refusal control; a boundary-showing remote args control.
- `ferry-9zp.1`: v2 activation is murail-4a's and the user's.
- `ferry-eon`: retention beyond admission is parked (no authority).
- `ferry-c3t.3`: the seat path budget, folded into the reference (§5.8) and `doctor` (§5.6).
- Capabilities wording (murail-4a `1007T234148-7e24`): "only ever added" becomes "a binary lists exactly what it implements"; a removed implementation drops its name.

---

## 7. Together: the system view

### 7.1 Lock taxonomy and the ordering rule

| Lock | Scope (state root) | Taken by | Mode | Blocking? |
|---|---|---|---|---|
| `hold:<name>` | user, one machine | `ferry hold` (land, any serialization) | excl/shared | yes |
| `install.lock` | host install root | `scripts/ferry-install` | excl | no (refuses) |
| `locks/<seat>` | session machine | `prepare` | excl | yes |
| `locks/host-gate` + `host-exclusive` (the lease) | session machine (§4.5: one per machine) | `prepare`, `host-lease` (§4.3) | shared/excl | yes |
| victim `locks/<seat>` | host | admission eviction | try only | no |

**The rule:** acquire in the order hold → install → seat → lease. Non-blocking tries (eviction, the install lock) never wait, so they cannot deadlock.

How it is enforced:
- a hold refuses inside a session (`FERRY_ID`, §4.1);
- `ferry host-lease` refuses inside a session;
- `--quiesce` installs take install → lease, and a run holds seat → lease, so no cycle exists;
- a holder of `hold:land:<repo>` may start runs (seat → lease): the order is respected.

The reference documents this table. Any new lock must declare its place.

### 7.2 Custody everywhere

Holds, sessions and (later) stop all rely on one custody model: subreaper, `reap_adopted`, and the ECHILD-only census, with `Unknown` and taint as fail-closed states. There is no second supervisor (murail-4a `1008T004724-c479`). Interactions:
- **Stop (§4.4)** asks the supervisor to tear down, so the custody release still runs and the receipt records it.
- **A hold whose command runs `ferry run`:** the hold's census sees the `ferry run` caller as its child. Remote sessions are on another machine, and their custody is the host's.
- **The Darwin limit:** there is no census, so only the inherited descriptor guarantees a hold. The README states the residue: a descendant that closes every descriptor and leaves the group escapes.

### 7.3 Receipts as the single record

- Run receipts (repo `.ferry/receipts/`) and hold receipts (user `holds/receipts/`) share a `kind` field: `run`, `hold`, `stop`. `ferry receipts` lists both, with `--kind`.
- Every wait (seat, lease, hold) writes `waited_behind` and `wait_ms`. Every refusal writes `reason` and `next`.
- **Schema rule:** fields are only added; an absent field means unknown. `protocol` stays 1 while requests and events stay additive. The reference records, per field, the revision that introduced it.

### 7.4 Capabilities and compatibility

New host behaviours get capability names:
- `pinned-launch` (§4.3);
- `stop` (§4.4);
- `status` (§5.5);
- `machine-lease` (§4.5; informational).

Each binary lists exactly what it implements. Callers adapt per host: pin only if `pinned-launch`; offer stop only if `stop`.

Request fields stay omitted when empty, so an older host refuses only requests that actually need the new behaviour. Its refusal comes back as refused 125 before acceptance, never as lost.

### 7.5 Installation and qualification as one pipeline

```
freeze → own critic (A2) → [consumer review by execution if the contract changes] → just land
       → qualify-install (scratch root, both ends, Nix sandboxes Darwin + Linux)
       → install (after P2: no window; with --quiesce only for state-format changes)
       → verify-caller / verify-host → receipt join recorded on the bead
```

Holds make `just land` safe to run concurrently. P2 makes installs safe to run any time. Together they remove every window from this pipeline.

### 7.6 Test environments are part of the contract

Nix check phases (Darwin and Linux) and bench via the dev shell (§6.1) must run the same tests with the same tools. Platform-scoped tests say why. Knockouts run on the platform where the behaviour exists. A test that cannot exercise its claim fails rather than passes (§6.1 rule).

### 7.7 Ownership boundaries

| Owner | Owns |
|---|---|
| **Ferry** | locks, leases, holds, custody, admission, receipts, capabilities, the install procedure, stop, status, doctor, and the docs and skill for all of it |
| **Repositories** (Murail, Herald, Ferry's own) | what runs under a hold: candidate selection, rebase, conflict law, gate population, forbidden environment, hooks |
| **Operator** (the user) | `machines.toml`, `host.toml` (v1/v2), the Nix configuration, deletion authority, host activation |

---

## 8. Order of work

| Wave | Work | Unblocks |
|---|---|---|
| **0 (now)** | P1 amendment (F1–F3, R1h) → critic → consumers → land → qualify → install. §6.1 dev shell and test recipe (small, independent) | adoption; trustworthy bench runs |
| **1** | §4.2 adoption: ferry's own land, then Murail's and Herald's (by their owners). §5.1 help, §5.2 summary line, §5.3 receipts | R1 met; the first DX gains |
| **2** | §4.3 P2 pinned launcher (+ `host-lease`, `--quiesce`). §5.4 refusal why/next | R2/R5 met; the install window rule deleted |
| **3** | §4.6 ledger → §5.5 status → §5.6 doctor. §4.4 P3 stop | R7 met; "what's going on" without hail |
| **4** | §4.5 P4 machine lease + load. §5.7 init and tables. §5.8 reference → README → skill and HM module | R3 met; newcomer-ready |
| **5** | §4.2 retire the hand holds after a measured clean day | A1 fully realized |

Each item ships independently through §7.5. No item changes the shape of another's interface without amending this spec.

## 9. Bead map

| Bead | Item | Depends on |
|---|---|---|
| ferry-ljc.1 | P1 holds (amend) | n/a |
| ferry-ljc.5 | ferry's own land adopts holds | ljc.1 installed |
| ferry-ljc.6 | Murail land adoption (murail-4a; tracked here) | ljc.1 installed |
| ferry-ljc.7 | Herald land adoption (herald-1b; tracked here) | ljc.1 installed |
| ferry-ljc.2 | P2 pinned launcher, host-lease, --quiesce | n/a |
| ferry-ljc.8 | lock ledger and waiting lines | ljc.1 |
| ferry-ljc.3 | P3 ferry stop | ljc.8 |
| ferry-ljc.4 | P4 machine-wide lease and load observations | ljc.8 |
| ferry-ljc.9 | retire hand holds (measured) | ljc.5, ljc.6, ljc.7, ljc.2 |
| ferry-oln.1 | dev shell and test/check recipes | n/a |
| ferry-c3t.5 | help text | ljc.1 |
| ferry-c3t.6 | summary line | ljc.1 |
| ferry-c3t.7 | receipts UX | c3t.6 |
| ferry-c3t.8 | refusal why/next | c3t.6 |
| ferry-c3t.10 | status | ljc.8 |
| ferry-c3t.9 | doctor | c3t.10 |
| ferry-c3t.11 | init | c3t.5 |
| ferry-c3t.12 | recipes/machines tables | c3t.5 |
| ferry-c3t.1 | reference doc | c3t.8, ljc.3 |
| ferry-c3t.2 | README restructure | c3t.1 |
| ferry-c3t.4 | skill + HM module + nix-config | c3t.2, c3t.10 |
| ferry-nrl.1 | the nix-config pin commit (user) | n/a |

## 10. Decisions taken here (and alternatives rejected)

1. **Locks, not a scheduler.** A scheduler would add a service, a single point of failure and an authority over ordering that nobody asked for. Flock gives crash-release for free. The research graph's only applicable claim (FFS advisory locking, moderate evidence) supports cooperative advisory locks among cooperating tools; nothing in it argues for central scheduling.
2. **A pessimistic land lock first.** murail-2a's optimistic variant (gate outside the lock, push under it if trunk is unchanged, else rebase and re-gate) cuts waits under low contention. It can be added inside a repository's land later, without changing Ferry.
3. **P2: pin the launcher, not quiesce by default.** Pinning closes the actual race at no waiting cost. Quiesce remains for state-format changes.
4. **`ferry stop` refuses others' runs without `--other --reason`.** This keeps A7 true by default and makes overrides auditable.
5. **One lease per machine.** This is simpler than per-repo leases with a cross-repo layer, and it matches what "exclusive" means to a measurement.
6. **Sidecars and the ledger are diagnostics.** The flock decides. A ledger failure never changes locking.
7. **Human output by default, JSON on request.** This applies only after confirming that consumers read the files rather than stdout.
8. **R6 needs no Ferry work.** Watched environment already refuses routed runs early. Local land is the repository's.
