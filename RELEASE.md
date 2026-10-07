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

Packaging should expose a normal Nix package/app and an optional Home Manager
module for installation and machine configuration. It must not duplicate
project recipes or bake Herald/Murail data into the binary. Target declarations
must agree across packaging, binary releases and the installer; unsupported
targets are not advertised merely because the source compiles on one machine.

## Install, upgrade and rollback

One reviewed, landed commit is installed through Nix on both ends. Nothing is
copied by hand, and nothing reaches a forge.

**Caller.** The machine's Nix configuration takes ferry as a flake input
pinned by commit, `git+file:///path/to/ferry?rev=<commit>`, and installs
`packages.<system>.default`. Upgrading means changing that pin through the
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
without rebuilding. `verify-host <ssh> [commit]` checks the active install.
One `flock` serializes every activation and rollback. Only full commit ids
reachable from `main` install on the live root.

**What this guarantees and what it does not.**

- The source commit and the flake's locked inputs are fixed by commit id and
  `narHash`. The first build on a host fetches those inputs. Rollback needs
  no network, because each generation's output stays rooted.
- Running sessions keep the executable they started with. A run whose
  `host-info` precedes a flip and whose `host-run` follows it is reported
  lost ("host binary changed during run"), never mis-attributed.
- Activating on a shared host still needs an announced window.
- Builds from before receipts carried the source revision report it as
  unknown. A receipt without these fields says nothing about provenance.

**Qualification.** `just qualify-install <ssh> <commit>` runs the whole
procedure in a scratch root on the host, without touching the live install.
It adopts the live generation, installs `<commit>`, and requires a receipt
that joins both ends to `<commit>` and to the hashes of the executables that
ran. It rolls back and forward. It requires two refusals: an install of a
build without the source join, and a second activation while another holds
the lock.

