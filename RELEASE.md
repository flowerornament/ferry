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

