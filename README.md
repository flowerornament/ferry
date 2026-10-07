# Ferry

Run a recipe at an exact commit on a machine that can run it. Stream its
output, return its exit code, and keep a receipt.

Ferry separates the project from the machine. A project declares recipes and
their required features. You configure machines once. No machine configuration
means local execution; a configured machine missing required features refuses.

## Install and run

Python 3.12+, Bash 5, jj, Git and SSH are required on the client. Remote hosts
need Python, Git, Bash, flock, setsid, and the recipe's tools. Ferry's transport
is extracted from Murail's reviewed `8ba36cfa` implementation.

```
~/code/ferry/ferry recipes
~/code/ferry/ferry machines
~/code/ferry/ferry run test
~/code/ferry/ferry run mix-test --on bench --commit @ -- path/to/test.exs
~/code/ferry/ferry run test --local --json
~/code/ferry/ferry receipt <id>
```

Add this directory to PATH or use its absolute executable path. There is no
published package or forge remote yet. `--dry-run` resolves the commit and
route without pushing or running anything.

## Project configuration

Commit `ferry.toml`:

```toml
version = 1
runner = "devenv"

[recipe.test]
command = ["mix", "test"]
requires = ["linux", "x86_64"]
routable = true

[recipe.mix-test]
command = ["mix", "test"]
requires = ["linux", "x86_64"]
routable = true

[hooks]
prepare = "scripts/ferry-prepare"
env = "scripts/ferry-env"
collect = "scripts/ferry-collect"
```

If recipes already have a catalog, point to it instead of repeating them:

```toml
version = 1
runner = "path"
[catalog]
path = ".config/murail-gates.toml"
table = "recipe"
```

Each catalog entry supplies `requires`, `routable`, and an optional argv
`command` (default `just <recipe>`). Recipe runner metadata in another catalog
is not an environment choice; `environment = "path" | "devenv"` overrides the
project environment. Requirements and recipes are read from the named commit.
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

Hooks are committed executable paths relative to the exact checkout. They run
inside the owned recipe session: prepare, env, recipe, collect. The env hook
prints a JSON object of environment-variable strings, never shell code.
Inputs include `FERRY_SEAT`, `FERRY_COMMIT`, `FERRY_RECIPE`, `FERRY_ID`, and
`FERRY_WORKDIR`; collect also receives `FERRY_EXIT_CODE`. Projects own their
database preparation, cache homes, and artifact collection.

## Safety and receipts

Remote transport pushes only the exact commit to the machine's private bare
repository under `refs/ferry/<seat>/<sha>`, never to origin or master. A seat
lock protects a detached checkout. HEAD, clean tree, and hidden index flags are
checked before execution. A unique run token binds host facts to client output.
Client loss terminates the recipe session; children creating new sessions can
escape that boundary and require stronger host isolation. This is not a sandbox.

Local execution extracts a fresh archive of the exact commit, not the caller's
dirty tree. It does not claim remote HEAD/index verification or a warm checkout.
Receipts live in `.ferry/receipts/`. Machine timings must not be pooled across
platforms. A remote result is evidence, not automatic publication authority.

`--json` emits version-one NDJSON: accepted, routed, prepared, output, exited,
receipt, or refused. Output is escaped as data. Recipe exits pass through;
125 means refusal and 124 is reserved for timeout. Read the receipt's outcome,
not a recipe's printed text, to distinguish tool refusal from recipe failure.

## Development

`just check` runs pure routing/catalog controls and Bash syntax checks.
Transport review also requires actual remote execution: unknown/conflicted
commit, tampered checkout, recipe failure, repeated IDs, and client disconnect.
Unit tests alone do not establish those properties.
