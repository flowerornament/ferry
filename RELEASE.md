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
