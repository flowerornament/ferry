#!/usr/bin/env bash
# Host half of `just remote` (murail-2qnnz): run one recipe at one exact
# commit in this seat's slot, and leave a log and a facts file behind.
#
# Usage (sent over ssh by scripts/ferry-transport.sh, never typed):
#   ferry-run.sh <seat> <sha> <id> <recipe> <path|devenv> <token>
#
# Layout under $FERRY_REMOTE_ROOT (default ~/ferry-remote):
#   repo.git/            bare repo; ferry-transport.sh pushes refs/ferry/<seat>/<sha>
#   slots/<seat>/        one detached worktree per seat (a real checkout, so
#                        changed files get fresh mtimes and Cargo fingerprints
#                        stay correct; `git archive` would stamp commit time)
#   slots/<seat>.lock    flock: one run per seat at a time
#   targets/<seat>/      CARGO_TARGET_DIR, kept across runs
#   logs/<id>.{log,facts,rustc}
#
# The facts file is created once, under the lock, and never reused: an id
# whose facts exist is refused. Its first line is the laptop's token, so the
# laptop trusts only the facts its own run wrote.
#
# The recipe runs in a new session (setsid), and the session is the unit of
# cleanup: nextest puts each test in its own process group, but every group
# stays in the session. This script holds the ssh channel's stdin; EOF there
# means the client is gone, and every process in the session is killed. The
# session is swept again when the recipe ends. A child that starts its own
# session escapes this; only a cgroup would hold it.
#
# Exit: the recipe's own code; 125 for a refusal, with `refused` in the facts
# when the facts file is this run's.
set -uo pipefail

seat="${1:?seat}" sha="${2:?sha}" id="${3:?id}" recipe="${4:?recipe}"
want="${5:?runner}" token="${6:?token}"
root="${FERRY_REMOTE_ROOT:-$HOME/ferry-remote}"
repo="$root/repo.git" slot="$root/slots/$seat" target="$root/targets/$seat"
log="$root/logs/$id.log" facts="$root/logs/$id.facts"
owned=""

fact() { printf '%s\t%s\n' "$1" "$2" >>"$facts"; }
refuse() {
    echo "remote: refused: $*" >&2
    [ -z "$owned" ] || fact refused "$*"
    exit 125
}

[[ "$seat" =~ ^[A-Za-z0-9._-]+$ ]] || refuse "seat '$seat' is not a plain name"
[[ "$sha" =~ ^[0-9a-f]{40}$ ]] || refuse "'$sha' is not a full commit id"
[[ "$id" =~ ^[A-Za-z0-9._-]+$ ]] || refuse "id '$id' is not a plain name"
[[ "$recipe" =~ ^[A-Za-z_][A-Za-z0-9_-]*$ ]] || refuse "recipe '$recipe' is not a plain recipe name"
[[ "$want" =~ ^(path|devenv)$ ]] || refuse "runner '$want' is not path or devenv"
[[ "$token" =~ ^[0-9a-f]{32}$ ]] || refuse "token is not 32 hex digits"
mkdir -p "$root/slots" "$root/targets" "$root/logs"

# Watch the channel from here on. A background job's stdin defaults to
# /dev/null, so the reader takes the channel on an explicit fd; it ends only
# when the ssh client is gone.
exec 8<&0
cat <&8 >/dev/null 2>&1 &
reader=$!
exec 8<&-
trap 'kill "$reader" 2>/dev/null' EXIT

# The recipe's session inherits the lock, so a process that escapes the
# session keeps the slot busy rather than sharing it with the next run.
exec 9>"$root/slots/$seat.lock"
if ! flock -n 9; then
    echo "remote: slot $seat is busy; waiting for its lock" >&2
    until flock -n 9; do
        kill -0 "$reader" 2>/dev/null || refuse "client gone while waiting for the slot lock"
        sleep 1
    done
fi
(set -C; : >"$facts") 2>/dev/null || refuse "run id $id already has a facts file; ids are never reused"
owned=1
fact token "$token"

git --git-dir="$repo" cat-file -e "$sha^{commit}" 2>/dev/null \
    || refuse "commit $sha is not in $repo (push it first)"
if [ ! -e "$slot/.git" ]; then
    git --git-dir="$repo" worktree prune
    git --git-dir="$repo" worktree add --quiet --detach "$slot" "$sha" \
        || refuse "could not create slot $slot"
fi
cd "$slot" || refuse "slot $slot is missing"
git checkout --quiet --detach --force "$sha" || refuse "checkout of $sha failed"
git clean -ffdxq || refuse "git clean failed in $slot"
head="$(git rev-parse HEAD)"
[ "$head" = "$sha" ] || refuse "slot HEAD is $head, expected $sha"
dirty="$(git status --porcelain --ignored --untracked-files=all)"
[ -z "$dirty" ] || refuse "slot tree is not clean after checkout: $(head -3 <<<"$dirty")"
# assume-unchanged and skip-worktree entries hide edits from status and from
# a forced checkout; a trusted slot has none.
hidden="$(git ls-files -v | grep -v '^H ' || true)"
[ -z "$hidden" ] || refuse "slot index hides files from status: $(head -3 <<<"$hidden") (recover: rm -rf $slot; git --git-dir=$repo worktree prune)"

# Runner: `path` runs just on the tools in ~/.local/bin and ~/.cargo/bin,
# which come first (Debian's /usr/bin/just is older). `devenv` runs
# `devenv shell -- just`, pinning the tools from devenv.lock as on the laptop,
# and falls back to `path` when the shell cannot be entered (network,
# first-run build). Rust comes from rustup either way (rust-toolchain.toml).
# `path` is the default because devenv.nix carries no Linux system libraries
# yet: its pkg-config hides Debian's alsa.pc, so alsa-sys fails to build
# inside the shell (2026-10-07). The facts record which runner ran.
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$HOME/.nix-profile/bin:/nix/var/nix/profiles/default/bin:$PATH"
[ ! -f /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh ] || \
    source /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh
export CARGO_TARGET_DIR="$target"
runner=path
if [ "$want" = devenv ]; then
    if ! command -v devenv >/dev/null 2>&1; then
        refuse "devenv is not installed"
    elif timeout "${FERRY_REMOTE_DEVENV_TIMEOUT:-300}" devenv shell -- true >"$root/logs/$id.devenv" 2>&1; then
        runner=devenv
        fact devenv "$(devenv version 2>/dev/null | head -1)"
    else
        refuse "devenv shell failed (logs/$id.devenv)"
    fi
fi
if [ "$runner" = devenv ]; then wrap=(devenv shell --); else wrap=(); fi
version() { "${wrap[@]}" "$@" 2>/dev/null | head -1; }
fact runner "$runner"
fact prepared "$sha"
fact recipe_executor_sha256_received "$(printf %s "${FERRY_RECIPE_SCRIPT:-}" | sha256sum | cut -d' ' -f1)"
fact recipe_spec_sha256_received "$(printf %s "${FERRY_SPEC:-}" | sha256sum | cut -d' ' -f1)"
fact runner_sha256_received "$(printf %s "$BASH_EXECUTION_STRING" | sha256sum | cut -d' ' -f1)"
fact host "$(hostname)"
fact triple "$(rustc -vV 2>/dev/null | sed -n 's/^host: //p')"
fact just "$(version just --version)"
fact nextest "$(version cargo nextest --version)"
fact cargo_target_dir "$target"
fact slot "$slot"
fact log "$log"
fact loadavg_start "$(cut -d' ' -f1-3 /proc/loadavg)"
fact started "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
rustc -vV >"$root/logs/$id.rustc" 2>&1

echo "remote: $(hostname) slot=$seat sha=${sha:0:12} recipe=$recipe runner=$runner log=$log" >&2
# shellcheck disable=SC2016 # expands in the inner shell
if [ -n "${FERRY_SPEC:-}" ]; then
    command=(python3 -c "${FERRY_RECIPE_SCRIPT:?recipe executor required}")
else
    command=("${wrap[@]}" just "$recipe")
fi
setsid bash -c '"${@:2}" 2>&1 | tee "$1"; exit "${PIPESTATUS[0]}"' recipe "$log" "${command[@]}" &
job=$!
# setsid in a child that leads no group starts the session in place: sid = pid.
sid="$job"
fact session "$sid"
sweep() { # <signal>: every process in the recipe's session
    pkill "-$1" -s "$sid" 2>/dev/null
}

while kill -0 "$job" 2>/dev/null; do
    if ! kill -0 "$reader" 2>/dev/null; then
        echo "remote: client gone; killing session $sid" >>"$log"
        fact lost "client gone"
        sweep TERM
        sleep 5
        sweep KILL
        break
    fi
    sleep 1
done
wait "$job"
rc=$?
sweep TERM
fact ended "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
fact loadavg_end "$(cut -d' ' -f1-3 /proc/loadavg)"
fact exit_code "$rc"
exit "$rc"
