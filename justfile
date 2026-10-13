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

# Add a jj workspace for one more agent (run in ~/code/ferry): ../<name>, with bd and Claude memory linked
workspace-add name:
    scripts/workspace-add.sh {{quote(name)}}
