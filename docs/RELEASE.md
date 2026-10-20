# Release contract

How Ferry is versioned, released, installed, upgraded and rolled back.
Releases are tags `vX.Y.Z` on `main`, with notes from `CHANGELOG.md`,
binaries for each advertised target, and Nix outputs in the
`flowerornament` Cachix cache. The `release` branch always points at the
latest tag. There is no crates.io package.

Cargo.toml owns the version. Nix reads it rather than keeping another version
variable. Cargo.lock and rust-toolchain.toml pin source and compiler inputs.
The package installs one binary and verifies its version and help output.

Before a public release, every advertised target must build and pass its
controls, its installed binary must identify the same version, and its Nix
output must be confirmed present in its binary cache. Probe cache presence
with negative lookup caching disabled: a locally built output is not
evidence that another machine can substitute it.

Tagging and moving a release pointer are separate explicit publication acts,
after version, changelog, clean-tree, checks, package and cache verification.
Runtime Ferry neither provisions hosts nor performs these publication acts.

Packaging exposes a normal Nix package/app and an optional Home Manager
module (`programs.ferry`, with the agent skill) for installation. It must not duplicate
project recipes or bake any project's data into the binary. Target declarations
must agree across packaging, binary releases and the installer; unsupported
targets are not advertised merely because the source compiles on one machine.

## Cutting a release

From the colocated checkout (the one with `.git`):

1. `just release-bump X.Y.Z` sets the version in `Cargo.toml` and
   `Cargo.lock` and scaffolds the `CHANGELOG.md` entry. Fill it in, describe
   the change, and land it (`just land`) like any other change.
2. Wait for the **CI** and **Nix Cache** workflows on that commit. Nix Cache
   builds every advertised system and pushes it to Cachix (it needs the
   `CACHIX_AUTH_TOKEN` repository secret).
3. `just release-verify`: versions agree, the changelog entry is filled, the
   tree is clean, `just check` passes and the Nix build reports the version.
4. `just release-tag X.Y.Z`: checks that every advertised output is in the
   cache, tags `vX.Y.Z`, pushes the tag, and moves `release` to it. The
   **Release** workflow then publishes the GitHub release with the changelog
   entry as its notes and attaches the binaries.

Only a landed commit on `main` is tagged. A tag is a publication act of its
own; it installs nothing anywhere.

## Install, upgrade and rollback

One reviewed, landed commit is installed through Nix on both ends. Nothing is
copied by hand.

**Caller.** The machine's Nix configuration takes ferry as a flake input:
`github:flowerornament/ferry?ref=refs/heads/release` to follow releases, or
`github:flowerornament/ferry?rev=<commit>` (or
`git+file:///path/to/ferry?rev=<commit>`) to pin one commit. It installs
`packages.<system>.default`; with the Cachix substituter accepted, released
outputs are downloaded rather than built.

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

**State root.** Runs keep their state in `~/.local/state/ferry/v1` on a
place (SPEC §9); a local run uses `<repo>/.ferry/local/v1`. Install
generations (`installs/`, `current`, `previous` and the install `repo.git`)
live beside it under `~/.local/state/ferry`. A change of state layout gets a
new root, so old and new binaries never share one; the move to `v1` is
recorded in [`design/2026-10-12-m1-joint-window.md`](design/2026-10-12-m1-joint-window.md).

**Activation.** Activation is one atomic rename under `install.lock`, and
runs already in flight finish on the binary they started with. A run whose
`host-info` precedes the rename and whose `submit` follows it runs on the
new binary, and its record names that binary, but its capability and
protocol checks were made against the old one. After a forward install that
is harmless. After a rollback, or a release that drops a capability, a run
checked by one binary is executed by another, and a protocol change is
refused only after the run has run. Pinned launch (`ferry-ljc.2`) closes
that gap. Install places before their callers: a new caller refuses an old
place with a typed 125 that names its revision, while an old caller may
meet a new place it cannot read.

**What this guarantees and what it does not.**

- The source commit and the flake's locked inputs are fixed by commit id and
  `narHash`. The first build on a host fetches those inputs. Rollback needs
  no network, because each generation's output stays rooted.
- A run keeps the executable its supervisor started with, and its record
  names that binary (`binary_sha256`), so an install during a run is never
  mis-attributed. A caller refuses a place without `detach` (125, naming the
  place's `source_rev`): install places before or with their callers.
- A run submitted across an activation is checked by the old binary and
  executed by the new one (see **Activation**).
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

