#!/usr/bin/env bash
# Laptop half of `just remote` (murail-2qnnz): run one just recipe on a remote
# host at one exact jj commit, stream its output, pass its exit code through,
# and write a receipt and a log under .murail/remote/.
#
# Usage: scripts/ferry-transport.sh <recipe> [rev=@] [host=bench]
#
#   rev    any jj revset naming exactly one commit; refused if conflicted.
#          The commit runs, not the desk's files; `@` carries the desk's edits
#          because jj snapshots it on every command.
#   host   prototype: only `bench`, reached through the `bench` ssh alias
#          (~/.nix-config/home/remotes.nix). FERRY_REMOTE_SSH overrides how
#          it is reached: a full ssh command line ending in the destination.
#
# FERRY_REMOTE_RUNNER=path|devenv (default path) picks how the host runs the
# recipe; see ferry-run.sh for why path is the default today.
#
# Flow:
#   jj commit id ─▶ git push <anchor .git> sha:refs/ferry/<seat>/<sha>
#                ─▶ ssh: ferry-run.sh <seat> <sha> <id> <recipe> <runner> <token>
#                ─▶ .murail/remote/<id>.{log,json}
#
# The runner script is sent from this checkout, not read from the pushed
# commit: the harness is the tool and the commit is the data, so any commit
# (master before this landing, an old bisect point) can run. Its bytes are
# read once; the receipt's runner_sha256 is the digest of exactly what was
# sent, and the host records the digest of what it received.
#
# Every outcome writes a receipt. `outcome` is the authority, because a
# recipe may exit with any code and print any line:
#   ran-pass | ran-fail  the host recorded the recipe's exit code; it passes out
#   refused              a check refused before the recipe ran; exit 125
#   lost                 no host record (ssh failed, interrupted); exit 255,
#                        or 128+signal when this script was signalled
# 125 follows `timeout` and `git bisect run`: the tool itself declined.
set -euo pipefail

ssh_cmd="${FERRY_REMOTE_SSH:-ssh -o BatchMode=yes ${FERRY_HOST:-bench}}"

# Git transport hook: git calls `$GIT_SSH_COMMAND [opts] <host> <command>`;
# drop git's host and options, keep the command, reach the host our way.
if [ "${1:-}" = "--git-ssh" ]; then
    # shellcheck disable=SC2086 # ssh_cmd is a command line by contract
    exec $ssh_cmd "${@: -1}"
fi

recipe="${1:?usage: ferry-transport.sh <recipe> [rev=@] [host=bench]}"
rev="${2:-@}"
host="${3:-bench}"
want="${FERRY_REMOTE_RUNNER:-path}"
desk="$(jj workspace root)"
seat="${desk##*/}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
runner="$here/ferry-run.sh"
out="${FERRY_RECEIPT_DIR:-$desk/.ferry/receipts}"
mkdir -p "$out"
stamp="$(date -u +%Y%m%dT%H%M%SZ)"
nonce="$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')"
token="$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')"
started="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
t0=$(date +%s)
sha="" id="$stamp-$nonce" reason="" facts="" interrupted="" script_sha=""

receipt() { # <outcome> <exit code>
    FACTS="$facts" ID="$id" HOST="$host" SEAT="$seat" REV="$rev" SHA="$sha" \
    RECIPE="$recipe" RC="$2" OUTCOME="$1" REASON="$reason" STARTED="$started" \
    ENDED="$(date -u +%Y-%m-%dT%H:%M:%SZ)" WALL="$(($(date +%s) - t0))" \
    RUNNER_SHA="$script_sha" LOG="$out/$id.log" WANT="$want" TOKEN="$token" \
    INTERRUPTED="$interrupted" python3 - "$out/$id.json" <<'EOF'
import json, os, sys
e = os.environ
facts, _, rustc = e["FACTS"].partition("@@rustc\n")
remote = dict(l.split("\t", 1) for l in facts.splitlines() if "\t" in l)
if remote.get("token") != e["TOKEN"]:
    remote, rustc = {}, ""  # another run's facts, or none: not this run's evidence
remote.pop("token", None)
receipt = {
    "id": e["ID"], "outcome": e["OUTCOME"], "exit_code": int(e["RC"]),
    "host": e["HOST"], "seat": e["SEAT"], "rev": e["REV"], "sha": e["SHA"],
    "recipe": e["RECIPE"], "runner_requested": e["WANT"],
    "laptop_started": e["STARTED"], "laptop_ended": e["ENDED"],
    "wall_seconds": int(e["WALL"]), "runner_sha256": e["RUNNER_SHA"],
    "log": e["LOG"], "remote": remote, "rustc_vV": rustc.strip(),
}
for key, env in (("reason", "REASON"), ("interrupted", "INTERRUPTED")):
    if e[env]:
        receipt[key] = e[env]
with open(sys.argv[1], "w") as f:
    json.dump(receipt, f, indent=2)
EOF
    echo "remote: $1, exit $2 in $(($(date +%s) - t0))s; receipt $out/$id.json" >&2
}
refuse() { reason="$*"; echo "remote: refused: $*" >&2; receipt refused 125; exit 125; }
lost() { reason="$*"; echo "remote: lost: $*" >&2; receipt lost 255; exit 255; }

plain='^[A-Za-z0-9._-]+$'
[[ "$seat" =~ $plain ]] || refuse "seat '$seat' (workspace directory name) is not a plain name"
id="$stamp-$seat-$nonce"
[[ "$host" =~ $plain ]] || refuse "invalid host alias"
[[ "$recipe" =~ ^[A-Za-z_][A-Za-z0-9_-]*$ ]] || refuse "recipe '$recipe' is not a plain recipe name"
[[ "$want" =~ ^(path|devenv)$ ]] || refuse "FERRY_REMOTE_RUNNER '$want' is not path or devenv"

# jj prints warnings (a refused snapshot, say) on stderr; only stdout is ids.
jjerr="$(mktemp "${TMPDIR:-/tmp}/ferry-remote.XXXXXX")"
ids="$(jj log --no-graph -r "$rev" -T 'commit_id ++ "\n"' 2>"$jjerr")" \
    || { e="$(grep -m1 '^Error' "$jjerr" || head -1 "$jjerr")"; rm -f "$jjerr"; refuse "rev '$rev' does not resolve: $e"; }
rm -f "$jjerr"
n="$(grep -c . <<<"$ids" || true)"
[ "$n" = 1 ] || refuse "rev '$rev' names $n commits, not one"
sha="$ids"
id="${FERRY_ID:-$stamp-$seat-${sha:0:12}-$nonce}"
[[ "$id" =~ $plain ]] || refuse "invalid run id"
[ "$(jj log --no-graph -r "$sha" -T 'conflict' 2>/dev/null)" = "false" ] \
    || refuse "commit ${sha:0:12} has conflicts; resolve them before running it"

# Every desk shares the anchor's git store; desk commits are objects there
# even without a ref, so a raw sha pushes.
anchor="$(cd "$desk" && jj git root 2>/dev/null)" || refuse "no colocated git store behind $desk"
git --git-dir="$anchor" cat-file -e "$sha^{commit}" 2>/dev/null \
    || refuse "commit ${sha:0:12} is not in the git store $anchor"

# The exact bytes sent, trailing newline kept; digest them once, now.
script="$(cat "$runner"; printf x)"
script="${script%x}"
script_sha="$(printf %s "$script" | shasum -a 256 | cut -d' ' -f1)"

keeper="" sshpid="" tmp=""
# Cleanup must not fail: under set -e a failing command here would replace
# the exit code.
# shellcheck disable=SC2329,SC2086 # invoked by the trap; pids may be empty
cleanup() { set +e; kill $keeper $sshpid 2>/dev/null; [ -z "$tmp" ] || rm -rf "$tmp"; true; }
trap cleanup EXIT
# shellcheck disable=SC2329 # invoked by the traps below
on_signal() {
    interrupted="$1"
    [ -z "$sshpid" ] || { kill "$sshpid" 2>/dev/null || true; return; }
    reason="laptop got SIG$1 before the run started"
    receipt lost $((128 + $(kill -l "$1")))
    exit $((128 + $(kill -l "$1")))
}
trap 'on_signal HUP' HUP; trap 'on_signal INT' INT; trap 'on_signal TERM' TERM

# shellcheck disable=SC2086,SC2016 # ssh_cmd is a command line; $R expands on the host
$ssh_cmd 'R=$HOME/ferry-remote; mkdir -p "$R/slots" "$R/targets" "$R/logs"; [ -d "$R/repo.git" ] || git init --quiet --bare "$R/repo.git"' </dev/null \
    || lost "cannot reach $host"

# Two runs of one sha race to create its ref; the loser finds the commit
# already there, which is all a run needs.
echo "remote: push ${sha:0:12} -> $host refs/ferry/$seat/$sha" >&2
GIT_SSH_COMMAND="$(printf %q "$here/ferry-transport.sh") --git-ssh" GIT_SSH_VARIANT=ssh \
    git --git-dir="$anchor" push --quiet --no-verify \
    "ssh://$host/~/ferry-remote/repo.git" "$sha:refs/ferry/$seat/$sha" </dev/null \
    || $ssh_cmd "git --git-dir=\"\$HOME/ferry-remote/repo.git\" cat-file -e $sha^{commit}" </dev/null \
    || lost "push of ${sha:0:12} to $host failed"

# The channel's stdin stays open while this script and its caller live: the
# host kills the recipe's session at EOF. The keeper exits within a second of
# either one's death, however it dies (kill -9 of `just` included), and
# removes the fifos, so nothing outlives it. ssh runs in the
# background so a TERM or INT trap fires at once instead of after the run.
tmp="$(mktemp -d "${TMPDIR:-/tmp}/ferry-remote.XXXXXX")"
mkfifo "$tmp/in" "$tmp/out"
self=$$ caller=$PPID
(while kill -0 "$self" 2>/dev/null && kill -0 "$caller" 2>/dev/null; do sleep 1; done; rm -rf "$tmp") >"$tmp/in" &
keeper=$!
tee "$out/$id.log" <"$tmp/out" &
teer=$!

remote_cmd="bash -c $(printf %q "$script") remote-run"
remote_env="env"
for key in FERRY_SPEC FERRY_RECIPE_SCRIPT FERRY_SEAT FERRY_COMMIT FERRY_RECIPE FERRY_ID; do
    remote_env+=" $(printf %q "$key=${!key:-}")"
done
remote_cmd="$remote_env $remote_cmd"
for arg in "$seat" "$sha" "$id" "$recipe" "$want" "$token"; do
    remote_cmd+=" $(printf %q "$arg")"
done
# shellcheck disable=SC2086
$ssh_cmd "$remote_cmd" <"$tmp/in" >"$tmp/out" 2>&1 &
sshpid=$!
rc=0
wait "$sshpid" || rc=$?
while kill -0 "$sshpid" 2>/dev/null; do wait "$sshpid" || rc=$?; done
wait "$teer" || true

# shellcheck disable=SC2086
facts="$($ssh_cmd "cat \"\$HOME/ferry-remote/logs/$id.facts\" 2>/dev/null; echo '@@rustc'; cat \"\$HOME/ferry-remote/logs/$id.rustc\" 2>/dev/null" </dev/null 2>/dev/null || true)"
fact() { awk -F'\t' -v k="$1" '$1 == k { sub(/^[^\t]*\t/, ""); print; exit }' <<<"${facts%%@@rustc*}"; }

if [ "$(fact token)" != "$token" ]; then
    facts=""
fi
code="$(fact exit_code)"
if [ -n "$interrupted" ]; then
    reason="laptop got SIG$interrupted"
    sig=$(kill -l "$interrupted")
    receipt lost $((128 + sig))
    exit $((128 + sig))
elif [ -n "$(fact lost)" ]; then
    lost "the host saw the client go and killed the recipe's session"
elif [ -n "$code" ]; then
    if [ "$code" = 0 ]; then receipt ran-pass 0; else receipt ran-fail "$code"; fi
    exit "$code"
elif [ -n "$(fact refused)" ]; then
    reason="$(fact refused)"
    receipt refused 125
    exit 125
elif [ "$rc" = 125 ]; then
    reason="host refused before it owned a facts file (see log)"
    receipt refused 125
    exit 125
else
    lost "no host record of the run (ssh exit $rc)"
fi
