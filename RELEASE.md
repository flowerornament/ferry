# Release contract

Ferry is local-only today. This describes the release boundary learned from
Hail and Peat; it does not claim a published package or populated cache.

Cargo.toml owns the version. Nix reads it rather than keeping another version
variable. Cargo.lock and rust-toolchain.toml pin source and compiler inputs.
The package installs one binary and verifies its version and help output.

Before a public release, every advertised target must build and pass its
controls, its installed binary must identify the same version, and its Nix
output must be confirmed present in Cachix. Probe cache presence with negative
lookup caching disabled, as Hail does. A locally built output is not evidence
that another machine can substitute it.

Tagging and moving a release pointer are separate explicit publication acts,
after version, changelog, clean-tree, checks, package and cache verification.
Do not create a forge remote, upload a cache or publish a release without the
user choosing that destination. Runtime Ferry neither provisions hosts nor
performs these publication acts.

Packaging exposes a normal Nix package/app and an optional Home Manager
module (`programs.ferry`, with the agent skill) for installation. It must not duplicate
project recipes or bake Herald/Murail data into the binary. Target declarations
must agree across packaging, binary releases and the installer; unsupported
targets are not advertised merely because the source compiles on one machine.

## Install, upgrade and rollback

One reviewed, landed commit is installed through Nix on both ends. Nothing is
copied by hand, and nothing reaches a forge.

**Caller.** The machine's Nix configuration takes ferry as a flake input
pinned by commit, `git+file:///path/to/ferry?rev=<commit>`, and installs
`packages.<system>.default`.

The flake also exports `homeManagerModules.default`, `programs.ferry`:

- `enable` installs the package;
- `skill.enable` symlinks the agent skill (`skills/ferry`) to each of
  `skill.targets`, for example
  `[ ".agents/skills/ferry" ".claude/skills/ferry" ]`.

`skillsDir` points at the same tree for skill syncing. Upgrading means changing that pin through the
normal configuration workflow. `scripts/ferry-install verify-caller <commit>`
only checks the result. The first `ferry` on PATH must resolve into the Nix
store and report `<commit>`. The executable it runs must hash to the SHA-256
it reports. The configuration's `flake.lock` must pin `<commit>`.

**Host.** `scripts/ferry-install host <ssh> <commit>` installs one generation:

1. It pushes the commit into the host's bare repository, under
   `refs/ferry-install/<commit>`. That ref keeps the commit alive for as long
   as the generation exists.
2. It builds the commit with Nix into `installs/<commit>`, an indirect GC root.
3. It checks the new build before activating anything.
4. It flips `current` by rename and keeps the old target as `previous`.
5. It checks again through `~/.local/bin/ferry`, which is a fixed link to
   `current/bin/ferry` and is the `binary` default in `machines.toml`.

Failure at any step leaves the working generation active, and a failed check
after the flip flips back. `rollback <ssh>` swaps `current` and `previous`
without rebuilding. `verify-host <ssh> [commit]` checks the active install and
changes nothing.

A host still in the old single-link layout (`installed`) is adopted by the
next install as its own step:

1. The old generation is rooted under `installs/`.
2. `current` and the launcher are pointed at it.
3. The launcher must still run the same binary.
4. Only then is the old link removed.

So even a failed first install leaves a working launcher.
One `flock` serializes every activation and rollback. Only full commit ids
reachable from `main` install on the live root.

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
1. **Policy, before any install** (Morgan, a host act on bench; murail-4a,
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
   here until step 4, a pre-M1 caller is refused with 125 naming
   the place's revision (it may push into the old root's store first: see
   above). A new caller refuses an old place, which is why places go first.
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

**What this guarantees and what it does not.**

- The source commit and the flake's locked inputs are fixed by commit id and
  `narHash`. The first build on a host fetches those inputs. Rollback needs
  no network, because each generation's output stays rooted.
- A run keeps the executable its supervisor started with, and its record
  names that binary (`binary_sha256`), so an install during a run is never
  mis-attributed. A caller refuses a place without `detach` (125, naming the
  place's `source_rev`): install places before or with their callers.
- Activating on a shared host still needs an announced window.
- Builds from before receipts carried the source revision report it as
  unknown. A receipt without these fields says nothing about provenance.

**Qualification.** `just qualify-install <ssh> <commit>` runs the whole
procedure in a private scratch root on the host, made with `mktemp -d`. It
removes that root only after a pass and never touches the live install.
It adopts the live generation, installs `<commit>`, and requires a receipt
that joins both ends to `<commit>` and to the hashes of the executables that
ran. It rolls back and forward. It requires these refusals:

- verification of the legacy layout, without changing it;
- a first install of a build without the source join, which must leave the
  launcher on the same binary;
- the same install after a good one;
- a second activation while a confirmed holder owns the lock.

