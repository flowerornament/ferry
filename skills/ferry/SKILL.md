---
name: ferry
description: "Run a repository's recipe at an exact commit on a build machine (or locally in a clean checkout) and read what happened, with Ferry. Use this skill whenever you need to run a project's gate, tests or measurements on another machine (`ferry run … --on <machine>`), when a repository has a ferry.toml or a `just remote …` recipe that wraps ferry, when you must interpret a Ferry receipt, exit code (124, 125, 128+n, 255) or refusal, when a seat is tainted or refuses, when an exclusive/timing run waits for the host lease, or when you must decide whether a red result is your code or the infrastructure."
metadata:
  { "version": "0.1.0", "openclaw": { "emoji": "⛴️", "os": ["darwin", "linux"], "requires": { "bins": ["ferry", "git", "ssh"] } } }
---

# ferry

Ferry runs a recipe the repository declares (`ferry.toml`), at an exact
commit, on a machine that can run it, and writes a receipt saying exactly what
happened. It is the safe way to share build hosts with other agents:
- your run gets a private checkout (a **seat**);
- timing runs get the host alone (the **host lease**);
- whatever a run started is stopped when it ends.

## Run

```bash
ferry recipes                          # recipes at this commit
ferry machines                         # configured machines
ferry run test --on bench              # at your current commit (HEAD / @-)
ferry run test --local                 # clean checkout on this machine
ferry run check --on bench --commit <rev> --timeout-seconds 2400
ferry run bench-perf --on bench --exclusive        # hold the host alone
ferry run test --on bench -- --some-arg            # recipe args after --
ferry run test --on bench --json       # NDJSON events, for tools
```

- **Commit first.** Ferry runs a commit, not your working tree. The commit
  must exist and be described (jj) or committed (git); uncommitted edits are
  not part of the run.
- **One seat per workspace.** `--seat <name>` (default `default`) picks your
  private checkout. Concurrent runs in the same seat take turns; use a
  different seat for parallel work.
- **Always set a timeout** for anything long (`--timeout-seconds`). Without
  one, a hung recipe runs until you stop it.
- **Exclusive waits.** An exclusive run waits for in-flight runs on that
  machine and keeps new ones out. Use it only for measurements. The wait for
  in-flight runs is not bounded by the timeout.

## Read the result

The exit code and the receipt's `outcome` decide what a red result means:

| exit | outcome | what it means | what to do |
|---|---|---|---|
| 0 | `ran-pass` | the recipe passed | — |
| N | `ran-fail` | the recipe failed with code N | your code or test: read the output |
| 124 | `timeout` | `--timeout-seconds` elapsed | look for a hang, or raise the timeout |
| 125 | `refused` | **nothing ran** | read `reason`, which names the next step (below) |
| 128+n | `lost` | a signal stopped it before its result (130 Ctrl-C, 143 TERM, 129 HUP) | rerun if you still want the result |
| 255 | `lost` | infrastructure, not your code (the reason names it: client lost, supervisor died, transport) | rerun once; if it repeats, report it with the receipt id |

**Exit codes only.** A recipe can itself exit 125, so read `outcome` from the
receipt, never the printed text.

```bash
ferry receipt <id>                     # the full receipt (JSON)
```

**Receipt fields that matter:**
- `outcome`, `exit_code`, `reason`;
- `commit`;
- `host_source_rev`, `caller_source_rev`: which Ferry ran on each end;
- `custody`: what the run left behind and what was stopped;
- `lease`: shared or exclusive, and how long it waited;
- `admission`: host disk policy;
- `output_dropped` and `output_delivered`.

**A finished verdict stands.** A pass is never turned into `lost` later;
anything after the verdict is noted in `reason` ("after the verdict: …").

## Refusals (125) and their next step

- **Unmet requirement or unknown machine:** the recipe's `requires` are not
  all in a machine's features. Pick another `--on`, or run `--local` if the
  recipe allows it.
- **Provider capability missing:** the machine runs an older Ferry. Tell the
  operator; do not work around it.
- **Tainted seat:** an earlier run in this seat left processes Ferry could not
  stop or could not prove gone.
  - The refusal names the run, the pids and the exact
    `ferry seat recover <seat>` command, with `--local` for local runs.
  - Check that those processes are gone, then run that command on that
    machine. Recovery never kills.
  - If they are not gone, stop them by exact pid (never `pkill -f`).
  - `--accept-unknown --reason '<why>'` is an explicit operator act. Use it
    only after you have confirmed nothing is left.
- **Owner marker after a supervisor death:** the same procedure,
  `ferry seat recover <seat>`.
- **`cleanup_pending`:** an earlier session's teardown could not be confirmed.
  This is the operator's call: tell the user, and do not delete the marker
  yourself.
- **Disk floor (admission):** the machine is below its operator's free-space
  floor. Tell the operator.
- **Exclusive run refused because a seat is tainted:** recover that seat
  first.

## Waiting lines

`ferry: waiting for lock …: another run holds it` means another run holds
your seat or the host lease. That is normal while another agent's exclusive
measurement runs. Wait, or Ctrl-C (the run is then `lost` 130, and nothing is
left behind).

## Rules

- **Stop runs only through Ferry** (Ctrl-C on your own `ferry run`), or by the
  exact pid it reports. Never `pkill -f`, `killall` or any pattern kill: build
  machines are shared.
- **Services belong to the operator.** A service your recipe needs to outlive
  it (a shared database) must be an operator service, not started by the
  recipe: Ferry stops whatever a run leaves behind.
- **Ferry owns its own files** (`~/.local/state/ferry`, `.ferry/` in the
  repository). Do not edit or delete them by hand; use the commands above.
- **Remote evidence is not landing authority.** A green remote run does not
  certify a repository's own local gate (for example `just land`).
- **Report Ferry problems** with the receipt id and the exit code, to whoever
  maintains Ferry. A `lost` 255 that repeats is a Ferry or transport problem,
  not your code.
