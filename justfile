check:
    cargo fmt --check
    cargo clippy --all-targets --locked -- -D warnings
    cargo test --locked --no-fail-fast

build:
    nix build --no-link --print-out-paths .

# Publish the described jj change: run `just check` on exactly it, then move main
# and push it to origin (without an origin remote, it lands locally only)
land *args:
    scripts/jj-land.sh {{args}}

# Drill the install procedure for <rev> on <host> in a scratch root (live install untouched)
qualify-install host rev:
    scripts/qualify-install {{host}} {{rev}}

# Add a jj workspace for one more agent (run in the colocated checkout): ../<name>, with bd and Claude memory linked
workspace-add name:
    scripts/workspace-add.sh {{quote(name)}}

# Set the version in Cargo.toml and scaffold the CHANGELOG entry
release-bump version:
    python3 scripts/release.py bump {{quote(version)}}

# Every advertised Nix package output is in the public Cachix cache
release-cache-verify:
    python3 scripts/release.py cache-verify

# Release readiness: versions agree, changelog filled, clean tree, checks, Nix build
release-verify:
    python3 scripts/release.py verify

# Verify the cache, tag vX.Y.Z, push it, move origin/release; GitHub publishes the release
[confirm("This will verify cached Nix outputs, tag, push the tag, force-update origin/release and publish a GitHub release. Continue?")]
release-tag version:
    python3 scripts/release.py tag {{quote(version)}}
