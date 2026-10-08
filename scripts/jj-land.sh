#!/usr/bin/env bash
# Publish the described jj change: gate exactly it, then push exactly it.
#
# Usage: scripts/jj-land.sh [bookmark] [--dry-run]
#   Without a remote named LAND_REMOTE (default origin), it lands locally only.
#   The bookmark defaults to LAND_BOOKMARK, else `main` if it exists, else `master`.
#
# This is the only publish verb in a jj repository whose gates used to run in
# git hooks: jj runs no hooks, so the gate travels with the publish instead.
# It pins the candidate commit before the gate, runs the gate, checks that
# nothing moved, and only then moves the bookmark to the pinned commit and
# pushes it. It rewrites nothing, so the gated bytes are the published bytes.
# A raw `jj git push` skips the gate; do not use one.
#
# The candidate is `@` when `@` has changes, otherwise `@-`. Everything
# between the bookmark and the candidate is published with it; the gate runs
# on the candidate's tree, which includes them all. It refuses:
#   - nothing to publish (the candidate is empty or already in the bookmark);
#   - any undescribed commit in the range it would publish;
#   - a candidate not on top of the bookmark (rebase first);
#   - a local bookmark that differs from the remote one (fetch first);
#   - `@-` as candidate while `@` holds uncommitted edits;
#   - any change to the working copy or the bookmark while the gate ran.
# The gate must write only ignored paths: a file it leaves behind changes `@`.
# If the push fails, the bookmark moves back, so a later land starts clean.
#
# The gate command is LAND_GATE (default `just check`). --dry-run runs the
# gate and every check, then stops before publishing.
#
# Land lock: a land (not a dry run) holds an exclusive flock on
# <repo>/.jj/land.lock, shared by every workspace of the repository, from
# before the pin until it publishes or refuses. A second land waits for it
# (at most LAND_LOCK_WAIT seconds, default 3600) instead of gating a
# candidate that is about to go stale. The lock is on an open descriptor that
# this script and its gate inherit, so it is released when they exit, crash
# included; a gate that leaves a daemon holding it keeps the lock until that
# daemon exits. flock(1) on Linux, /usr/bin/lockf on macOS.
set -euo pipefail

bookmark=""
dry_run=0
for arg in "$@"; do
  case "$arg" in
    --dry-run) dry_run=1 ;;
    -*) echo "usage: jj-land.sh [bookmark] [--dry-run]" >&2; exit 64 ;;
    *) bookmark="$arg" ;;
  esac
done
if [ -z "$bookmark" ]; then
  bookmark="${LAND_BOOKMARK:-}"
  if [ -z "$bookmark" ]; then
    if [ -n "$(jj log --no-graph -r 'present(main)' -T commit_id 2>/dev/null)" ]; then
      bookmark=main
    else
      bookmark=master
    fi
  fi
fi
gate="${LAND_GATE:-just check}"
remote="${LAND_REMOTE:-origin}"

refuse() { echo "land: refused: $*" >&2; exit 1; }

# Takes the land lock on descriptor 9 (see above). The repository's shared
# store is .jj/repo in the main checkout; a workspace's .jj/repo is a file
# naming it, relative to that workspace's .jj.
land_lock() {
  local jj_dir store lock wait
  jj_dir="$(jj root)/.jj"
  store="$jj_dir/repo"
  if [ -f "$store" ]; then store="$(cd "$jj_dir" && cd "$(cat repo)" && pwd)"; fi
  lock="$(dirname "$store")/land.lock"
  wait="${LAND_LOCK_WAIT:-3600}"
  exec 9>>"$lock"
  if command -v flock >/dev/null 2>&1; then
    flock -n 9 && return 0
    echo "land: another land holds $lock; waiting up to ${wait}s" >&2
    flock -w "$wait" 9 || refuse "the land lock $lock was still held after ${wait}s"
  elif [ -x /usr/bin/lockf ]; then
    /usr/bin/lockf -s -t 0 9 && return 0
    echo "land: another land holds $lock; waiting up to ${wait}s" >&2
    /usr/bin/lockf -s -t "$wait" 9 || refuse "the land lock $lock was still held after ${wait}s"
  else
    refuse "no flock(1) or /usr/bin/lockf to take the land lock"
  fi
}

# One commit id for a revset, or empty when it names none. The first call in
# an observation snapshots the working copy, so later calls see current files.
rev() { jj log --no-graph -r "$1" -T 'commit_id ++ "\n"' 2>/dev/null | head -1; }

observe() {
  at="$(rev '@')"
  if [ -n "$(rev '@ & ~empty()')" ]; then
    candidate="$at"
  else
    candidate="$(rev '@- & ~empty()')"
  fi
  target="$(rev "present($bookmark)")"
  remote_tip="$(rev "present($bookmark@$remote)")"
}

[ "$dry_run" = 1 ] || land_lock
observe
[ -n "$candidate" ] || refuse "nothing to publish: @ and @- are empty"
[ -n "$target" ] || refuse "bookmark $bookmark does not exist"
[ -n "$(rev "$candidate & ::$bookmark")" ] &&
  refuse "nothing to publish: ${candidate:0:12} is already in $bookmark"
[ -n "$(rev "$bookmark & ::$candidate")" ] ||
  refuse "$bookmark is not an ancestor of ${candidate:0:12}; run: jj rebase -d $bookmark"
undescribed="$(rev "$bookmark..$candidate & description(exact:'')")"
[ -z "$undescribed" ] ||
  refuse "${undescribed:0:12} has no description; run: jj describe -r ${undescribed:0:12} -m \"area: subject\""
if [ -n "$remote_tip" ] && [ "$remote_tip" != "$target" ]; then
  refuse "$bookmark differs from $bookmark@$remote; run: jj git fetch, then jj rebase -d $bookmark"
fi
if [ "$candidate" != "$at" ] && [ -n "$(jj diff --from "$candidate" --to @ --summary)" ]; then
  refuse "@ holds edits on top of ${candidate:0:12}; describe them into the change or move them off"
fi

pinned_candidate="$candidate" pinned_at="$at" pinned_target="$target" pinned_remote="$remote_tip"
count="$(jj log --no-graph -r "$bookmark..$candidate" -T '"x\n"' | grep -c x || true)"
echo "land: pinned ${pinned_candidate:0:12} for $bookmark (on ${pinned_target:0:12}, $count commit(s)); gate: $gate"

if ! bash -c "$gate"; then
  refuse "gate failed ($gate); nothing published"
fi

observe
if [ "$at" != "$pinned_at" ] || [ "$candidate" != "$pinned_candidate" ]; then
  jj diff --from "$pinned_at" --to "$at" --summary >&2 || true
  refuse "the working copy changed while the gate ran (above); land again"
fi
[ "$target" = "$pinned_target" ] && [ "$remote_tip" = "$pinned_remote" ] ||
  refuse "$bookmark moved while the gate ran; run: jj git fetch, jj rebase -d $bookmark, land again"

if [ "$dry_run" = 1 ]; then
  echo "land: ADMITTED ${pinned_candidate:0:12} -> $bookmark (dry run; nothing published)"
  exit 0
fi

jj bookmark move "$bookmark" --to "$pinned_candidate"
# Local-only until a forge remote is chosen: without one, landing moves the
# bookmark and stops.
if ! jj git remote list | awk '{print $1}' | grep -qx "$remote"; then
  echo "land: LANDED ${pinned_candidate:0:12} -> $bookmark (no remote $remote; nothing pushed)"
  exit 0
fi
if ! jj git push --remote "$remote" --bookmark "$bookmark"; then
  jj bookmark move "$bookmark" --to "$pinned_target" --allow-backwards
  refuse "push failed; $bookmark moved back to ${pinned_target:0:12}; land again"
fi
echo "land: PUBLISHED ${pinned_candidate:0:12} -> $bookmark"
