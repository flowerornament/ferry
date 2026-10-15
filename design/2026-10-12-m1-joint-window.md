# 2026-10-12: The M1 joint window (runbook, as used)

A dated record, moved here from `RELEASE.md` on 2026-10-15 when the
repository went public. It is the runbook that moved Ferry's places, its
callers and the consumer project Herald to the M1 state root together; the
window ran on 2026-10-12, and the soak that followed ends on 2026-10-19.
Names here are the project's own: `bench` is the shared Linux build host,
Herald and Murail are the consumer projects, and `murail-4a`, `herald-1a`
and similar are reviewing agents' seats. Issue ids (`ferry-8kk.1`,
`ferry-xtx`) are in the project's beads database.

**The M1 state root.** From slice 7, runs keep their state in
`~/.local/state/ferry/v1` (SPEC §9), a fresh root that binaries before M1
never look into; a local run uses `<repo>/.ferry/local/v1`. Old and new
never share a root, by path alone. Every seat starts cold once. Install generations (`installs/`, `current`, `previous` and the
install `repo.git`) stay where they are. After the joint window, an
operator removes the old runtime state once, by hand, on each place:
`~/.local/state/ferry/{locks,slots,caches,runs,records,mirrors,companion-refs}`
(and `.ferry/local/{locks,slots,caches,runs,records,mirrors,companion-refs,repo.git}`
in each repository). Nothing in `v1/` and no install state is touched.
A pre-M1 caller may push objects into the old root's store before the new
place refuses it; this is harmless, since the old root is cleaned by hand
after the window. The checkout path's basename is not an identity: use
`FERRY_SEAT` (stable per seat), not the directory name.

**The M1 joint window (runbook).** M1 changes the place's state root, the
shipping table and the policy file, so places, callers and Herald move
together, in this order. Each step names who acts; Ferry never edits Herald
or Murail.

0. **Preconditions** (all, before announcing):
   - ferry-8kk.1 is done: bench's Herald dev Postgres runs as an operator
     service (systemd user unit with linger), and herald-1a says GO. The
     M1 session stop would otherwise stop the hand-started postmaster.
   - The commit to install is landed on `main`; its cells are green.
   - Herald's held change (7908bf7e: `[[ship]]` instead of
     `[companion_refs]`/`[[mirror]]`, and instance identity from
     `FERRY_SEAT`) has its consumer GO and is ready to land.
   - The window is announced to every seat that runs Ferry, with its
     start, its end and the refusals expected during it (step 3). It asks
     seats not to START runs during the window. Runs already in flight on
     the old binary finish under the old root (their supervisor keeps its
     own binary in memory): nobody stops them.
1. **Policy, before any install** (@flowerornament, a host act on bench; murail-4a,
   which activated version 2, is told): replace
   `~/.config/ferry/host.toml`'s `version = 2` (eviction) with version 1,
   keeping `min_free_bytes`, and add `seat_cap_bytes` if wanted. Pre-M1
   binaries accept version 1, so this changes nothing yet; M1 refuses
   version 2 ("version 2 eviction is retired").
2. **Qualify** (from `~/code/ferry`): `just qualify-install <bench-ssh>
   <commit>`. It runs the whole procedure in a scratch root and never
   touches the live install. A failure ends the window here.
3. **Places first** (from `~/code/ferry`): `scripts/ferry-install host
   <bench-ssh> <commit>`, then `scripts/ferry-install verify-host
   <bench-ssh> <commit>`. Bench is the only place callers reach over ssh
   (the Mac's `machines.toml` lists only `host.bench`); the Mac's place is
   the local route, the caller's own binary, so it moves with step 4. From
   here until step 4, a pre-M1 caller's run ends Lost (255) with nothing
   run: it invokes `host-run`, which M1 removed, and reads the unknown-verb
   error as a lost place (it may push into the old root's store first: see
   above). Expect it and don't retry; the caller moves in step 4. Only the
   other direction is a typed 125: a new caller refuses an old place, naming
   its revision, which is why places go first.
4. **Callers and Herald together:**
   - the Mac caller's Nix configuration pins `<commit>` and is switched;
     `scripts/ferry-install verify-caller <commit>` checks it;
   - in the same step, herald lands 7908bf7e. A pre-M1 caller rejects
     `[[ship]]` and an M1 caller rejects `[companion_refs]`, so Herald's
     config and the caller change at once;
   - Murail needs no change (it uses neither retired key).
5. **Prove it:** one real run per consumer through the new caller and
   place (Herald's and Murail's gates), each receipt naming `<commit>` on
   both ends; every seat starts cold once.
6. **Rollback** (any step): `scripts/ferry-install rollback <ssh>` on a
   place, the previous pin on the caller, and Herald reverts 7908bf7e. A
   rolled-back binary uses the old root, which is still there: so the next
   step waits until rollback is no longer wanted.
7. **After the window** (the operator, by hand, once): remove the old
   runtime state listed above. Nothing in `v1/` and no install state is
   touched.
8. **The 7-day soak** starts at step 5's first green real gate. It is M1's
   done criterion: every run ends in exactly one final record, and no Ferry
   state is left to clean by hand. Its start and end are recorded on
   ferry-xtx.
