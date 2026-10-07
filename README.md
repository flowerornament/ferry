# Ferry

Run this repo's recipe at this commit on a machine that can run it, and tell
me exactly what happened.

Ferry separates the project from the machine. A project declares recipes and
their required features. You configure machines once. No machine configuration
means local execution; a configured machine missing required features refuses.

## Companion references

Recipes can request exact companion commits under fully qualified host Git refs.
A string source keeps the caller backend's revision syntax. A typed source is
portable across Git, Git worktrees and Git-backed jj:

```toml
[companion_refs]
'refs/remotes/origin/master' = { remote = 'origin', branch = 'master' }
```

This selects the caller's locally known tracking ref, not the local branch or
the remote's current state. Ferry does not fetch or fall back. The tracking ref
must directly name a commit, not a tag object. The caller receipt records the
requested `companion_sources` and resolved `companion_refs`; only those exact
objects are transported and materialized on the host. Old receipts without
`companion_sources` provide no typed tracking provenance.

## Install and run

Ferry installs through Nix on the caller and on every host, from one reviewed
commit, so each receipt can name the source and executable that ran on both
ends (`caller_source_rev`/`caller_exe`, `host_source_rev`/`host_exe`, beside
the binary SHA-256s). `scripts/ferry-install` is the supported procedure for
hosts and for checking the caller; RELEASE.md describes install, upgrade and
rollback. One crate builds one binary: `ferry run` is the caller and
`ferry host-run` receives the request. Git, SSH, and Unix process tools are
required; jj is optional for colocated repositories. A `cargo build` binary
works but reports `source_rev` as `unpinned`.

```
ferry recipes
ferry machines
ferry run smoke --on bench
ferry run test --local --json
ferry receipt <id>
```

There is no published package or forge remote yet. This repository remains
local-only.
Ferry supplies non-interactive SSH (`BatchMode=yes`) and connection sharing
(`ControlMaster=auto`, `ControlPersist=60`) itself, including Git transfers.
Its control sockets live in a private `~/.cache/ferry/ssh` directory.

## Project configuration

Commit `ferry.toml`:

```toml
version = 1

[recipe.test]
command = ["devenv", "shell", "--", "mix", "test"]
requires = ["linux-x86_64"]
routable = true

[recipe.mix-test]
command = ["mix", "test"]
requires = ["linux-x86_64"]
routable = true

[hooks]
prepare = "scripts/ferry-prepare"
env = "scripts/ferry-env"
collect = "scripts/ferry-collect"
```

If recipes already have a catalog, point to it instead of repeating them:

```toml
version = 1
[catalog]
path = ".config/murail-gates.toml"
table = "recipe"
```

Requirements and recipes are read from the named commit. Catalog entries must
provide explicit command argv; Ferry never infers a tool from a recipe name.
Ferry has no Nix-specific environment logic: the repo owns
its command and executable prepare/env hooks.
If an existing catalog has another schema, provide `[catalog] adapter =
"scripts/ferry-catalog"`. The committed executable runs caller-side in fresh
checkouts of the pinned commit and emits JSON `{"version":1,"recipe":{...}}`.
It may also emit a `mirror` list derived from the repo's existing lockfile.
It can declare `preserve = ["_build", "deps", ".devenv"]` for ignored build
directories. Ferry records the list and retains those directories across resets;
tracked files, HEAD and hidden index flags are still checked. Paths must be
literal, ignored directories containing no tracked files, never `.git`, `.jj`
or `.ferry`. The repository's prepare hook makes retained build state current.
Preserved state is a declared cache: a run can depend on earlier runs in the
same seat, not only on the requested commit. Omit preserve for a clean run.
Ferry checks repeatability, validates the entire contract before routing, and
records adapter and emitted-contract digests. The adapter must be a pure reader;
Ferry is not a sandbox that can prevent arbitrary side effects outside its
temporary checkout.
Only explicitly routable recipes go remote. Publishing and certifying recipes
must remain unroutable: Herald's `just land` stays local.

## Machines

Create `$XDG_CONFIG_HOME/ferry/machines.toml` (default `~/.config/ferry`):

```toml
[host.bench]
ssh = "bench"
triple = "x86_64-unknown-linux-gnu"
cores = 16
features = ["linux", "x86_64", "quiet"]
```

Ferry chooses the first machine whose features contain every requirement.
`--on` names a configured machine; it does not bypass requirements. SSH aliases
come from your SSH configuration. Ferry does not copy credentials or provision
machines. Features are operator declarations, not hardware measurements or
proof that unrelated host work is absent.

## Hooks

The optional `environment` declaration contains `forwards` (caller variables
sent to the recipe), `watches` (prefixes whose unlisted nonempty variables
refuse before any push), and `local` (variables deliberately left on the
caller). A catalog adapter may emit this declaration from the repo's existing
catalog; it must not also be declared in `ferry.toml`. Receipts record the
selected values in `forwarded_env`, so never include secrets in `forwards`.
The host env hook cannot replace a forwarded value.

Hooks are committed executable paths relative to the exact checkout. They run
inside the owned recipe session: prepare, env, recipe, collect. The env hook
prints a JSON object of environment-variable strings, never shell code.
Ferry reserves `FERRY_*` and `GIT_CONFIG_GLOBAL`; an env hook cannot replace
run identity or the seat-local mirror configuration.
Inputs include `FERRY_SEAT`, `FERRY_COMMIT`, `FERRY_ID`, `FERRY_WORKDIR`
and `FERRY_CACHE_DIR`; collect also receives `FERRY_EXIT_CODE`. Projects own
their database preparation and artifact collection.

`FERRY_CACHE_DIR` is a per-seat directory that Ferry owns
(`<state>/caches/<seat>`, mode 0700). Put regenerable build output there, for
example `CARGO_TARGET_DIR=$FERRY_CACHE_DIR/cargo-target`. Under an explicit
host policy, Ferry may later reclaim named subdirectories of it from idle
seats. It never reclaims anything else.

### Host lease

Every session holds a host-wide lease while it runs. Ordinary runs share it.
`ferry run --exclusive`, or a recipe declaring `requires = ["exclusive"]`,
holds it alone, which is useful for timing measurements:

1. The run first closes a gate, so no new run starts.
2. It waits for in-flight runs to release their shared side.
3. Then it runs alone.

New runs pass the gate briefly before taking their shared side, so once the
exclusive run holds the gate, later runs cannot extend its wait. Before that,
it competes for the gate like any other run: there is no queue. Each receipt records `lease.mode` (`shared`
or `exclusive`) and `lease.wait_ms`. `exclusive` is an execution lock, not a
machine feature: routing ignores it, and receipts list it under
`requirements`.

The lease covers sessions under one state root: one user's Ferry on one host,
or one repository's `.ferry/local`. It does not quiet:

- processes started outside Ferry;
- other users;
- other virtual machines on the same hardware;
- hosts whose Ferry predates the lease. Such a host holds only the exclusive
  side and makes ordinary runs wait for nothing.

The wait for the lease is not bounded by `--timeout-seconds`; interrupt the
caller to give up.

### Seat custody

When a session releases its seat, the receipt's `custody` records what the
session left behind:

- `census` is one of:
  - `empty`: no live descendant of the run's supervisor;
  - `survivors`, with their pids;
  - `unsupported`.
- `record` is one of:
  - `idle`: written only after ran-pass, ran-fail or timeout with an empty
    census;
  - `taint`: survivors were found;
  - `none`.

On Linux the supervisor is a child subreaper. A process that leaves the
recipe's session (`setsid`, double fork, closed descriptors, a scrubbed
environment) is reparented to it rather than to init, so it still counts. A
session deletes its seat's idle record as soon as it holds the seat lock.

What custody does not see:

- processes the session did not start (pre-existing daemons, `systemd-run`,
  SSH to self, cron);
- other users;
- other hosts;
- a supervisor that dies before release. It records nothing, and its owner
  marker keeps the seat refused until `ferry seat recover`.

A tainted seat stays tainted. Other platforms report `unsupported` and never
record idle.

## Safety and receipts

Remote transport pushes only the exact commit to the machine's private bare
repository under `refs/ferry/<seat>/<sha>`, never to origin or master. A seat
lock protects a detached checkout. HEAD, clean tree, and hidden index flags are
checked before execution. A unique run token binds host facts to client output.
Client loss terminates the recipe session; children creating new sessions can
escape that boundary and require stronger host isolation. This is not a sandbox.
Unconfirmed session cleanup quarantines the host with a `cleanup-pending-*`
marker in its lock directory. Further dispatch refuses until an operator confirms
the session is gone and removes that marker; Ferry never clears it by age.
An execution error after recipe dispatch is `lost`, not a pre-execution refusal.

Local execution uses the same locked, clean detached checkout mechanism, not
the caller's dirty tree.
Receipts live in `.ferry/receipts/`. Machine timings must not be pooled across
platforms. A remote result is evidence, not automatic publication authority.

`--json` emits version-one NDJSON with prepared, output and receipt events.
The host also emits `accepted` and a bound `host-result`. Before acceptance
it may instead emit `refused` with a reason. Output is escaped as data.

### Outcomes and exit codes

Read the receipt's `outcome`, not a recipe's printed text, to tell a Ferry
refusal from a recipe failure. A recipe may itself exit 125.

| outcome    | `ferry run` exits | meaning |
| ---------- | ----------------- | ------- |
| `ran-pass` | 0 | The recipe ran and exited 0. |
| `ran-fail` | the recipe's code | The recipe ran and failed. |
| `timeout`  | 124 | `--timeout-seconds` elapsed; the recipe's session was ended. |
| `refused`  | 125 | Nothing ran. This covers an unknown recipe or route, an unmet requirement, a dirty or unknown commit, and a host that refused before acceptance (including a host older than the caller that rejects the request). |
| `lost`     | 128 + signal, or 255 | The run's fate is unknown or was cut short. When the caller is interrupted, it exits 128 plus the signal it received: 130 for SIGINT, 143 for SIGTERM, 129 for SIGHUP. A signal that arrives before dispatch still writes a receipt and dispatches nothing; the caller first lets an in-flight step, such as a git push, finish. Otherwise, lost means the connection or host failed after dispatch: 255. |

## Limits

SIGKILL of `host-run` leaves the recipe's session running. Its durable ownership
marker keeps that seat refused until recovery; cgroups are the later fix.
After the recipe and collect hook finish, the session leader exits if its
supervisor is gone. Background descendants may still require manual cleanup;
recovery reports their PIDs and never kills them.
The refusal names the internal seat and the exact `ferry seat recover <seat>`
command to run on that host (`--local` for a local run). Recovery takes the seat
lock, compares live session members against the leader's recorded start time,
and clears the marker only when the session is empty. It writes a receipt and
never kills processes. If death happened before the session birth was recorded,
recovery refuses rather than guessing; another logical `--seat` remains usable.

## Development

`just check` runs formatting, strict Clippy, unit and real-CLI controls.
Transport review also requires actual remote execution: unknown/conflicted
commit, tampered checkout, recipe failure, repeated IDs, and client disconnect.
Unit tests alone do not establish those properties. Current v0 work still
includes seat listing/GC, richer evidence and adversarial remote disconnect
controls. Mirrors use seat-local Git configuration, and declared artifact globs
return under `.ferry/runs/<id>/`; filesystem links refuse rather than copy
external bytes. See RELEASE.md for the unpublished release contract.
