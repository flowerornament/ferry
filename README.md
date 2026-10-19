# Ferry

Run a repository's recipe at an exact commit, on a machine that can run
it, and record exactly what happened.

Ferry is for agents and people who share build hosts. You declare recipes in
the repository and machines once. Ferry gives each run:

- **The exact commit you named.** No drifting remote checkout: Ferry pushes
  that object, even an unpushed one, and resets a private checkout to it.
- **No collisions.** Each workspace gets its own warm checkout (a *seat*), and
  a host lease keeps timing runs alone on the machine.
- **Nothing left running unseen.** A run's session is stopped when it ends.
  Its record lists, by pid, any process that escaped, or says `unknown` when
  it cannot tell (common on macOS).
- **An honest outcome.** A receipt and an exit code tell a recipe failure
  from a refusal (nothing ran) from a lost run (the infrastructure failed).
- **Runs outlive the connection.** A dropped SSH connection never ends a run;
  re-attach with `ferry follow <id>`.

Ferry is deliberately small. It is not a scheduler, a CI system, a service
manager, a sandbox, a lock service or a deployment tool.
[`docs/PRINCIPLES.md`](docs/PRINCIPLES.md) says what it is and is not, and why.

**Status:** early and moving fast. It runs every day on Linux and macOS
build hosts shared by several coding agents. The command line and the
receipt format are versioned (`protocol`, `version`), but expect changes
before 1.0. Tested on Linux x86_64 and macOS aarch64; the flake also builds
for aarch64-linux.

## Install

Ferry is one binary, `ferry`, needed on every machine that calls or runs a
recipe: Linux or macOS, with `git`, and `ssh` on the caller. A caller and its
places should run the same version: a caller refuses a place that lacks a
capability it needs, naming the place's revision. On each remote host, put
`ferry` where `machines.toml` says (`binary`, below).

### Nix with Home Manager

```nix
# flake inputs: the `release` branch always points at the latest tag
ferry = { url = "github:flowerornament/ferry?ref=refs/heads/release"; inputs.nixpkgs.follows = "nixpkgs"; };

# home-manager
imports = [ inputs.ferry.homeManagerModules.default ];
programs.ferry = {
  enable = true;
  skill.enable = true;                                      # link the agent skill
  skill.targets = [ ".agents/skills/ferry" ".claude/skills/ferry" ];
};
```

The flake advertises the `flowerornament` Cachix cache. Builds for
`aarch64-darwin`, `aarch64-linux` and `x86_64-linux` are substituted rather
than compiled; accept the flake config, or add the substituter yourself.

### Other ways

- `nix profile install github:flowerornament/ferry?ref=refs/heads/release`,
  or `nix run github:flowerornament/ferry -- --help`.
- **A release tarball** from GitHub (macOS aarch64, Linux x86_64 and
  aarch64). It holds `ferry` and the agent skill under
  `share/ferry/skills/ferry`.
- `cargo install --locked --git https://github.com/flowerornament/ferry`,
  or `cargo install --path .` from a checkout (Rust 1.99 or newer; the pinned
  toolchain is in `rust-toolchain.toml`).

[`docs/RELEASE.md`](docs/RELEASE.md) covers installing on shared hosts,
upgrades, rollback and cutting a release.

## Quick start

```sh
# 1. Declare recipes in the repository, and commit them: Ferry reads them
#    from the commit it runs, never from the working tree
cat > ferry.toml <<'EOF'
version = 1
[recipe.test]
command = ["cargo", "test", "--locked"]
requires = ["linux"]
routable = true
EOF
git add ferry.toml && git commit -m "Add ferry.toml"

# 2. Declare machines once, on the caller
mkdir -p ~/.config/ferry
cat > ~/.config/ferry/machines.toml <<'EOF'
[host.bench]
ssh = "bench"                       # an alias from your SSH configuration
triple = "x86_64-unknown-linux-gnu"
features = ["linux", "x86_64"]
EOF

# 3. Run
ferry recipes                    # what this commit allows
ferry machines                   # what you configured
ferry run test --on bench        # remote, at your current commit (git HEAD, or jj @-)
ferry run test --local           # this machine, in a clean checkout of the commit
ferry run test --on bench -- --nocapture       # arguments after --
ferry receipt <id>               # what happened
```

With no machines configured, runs are local. A machine that lacks a required
feature refuses rather than running. `--local` runs on this machine without
checking `requires`.

## Concepts

| | |
|---|---|
| **Recipe** | A named command (argv) the repository allows to run, with required features. Read from the commit being run. |
| **Machine** | Where work runs: `local`, or an SSH destination with declared features. |
| **Place** | A machine's Ferry state root, where runs execute: `~/.local/state/ferry/v1` on a host, or the repository's `.ferry/local/v1` for local runs. |
| **Seat** | A private, warm checkout for one workspace (`--seat`, default `default`). One run at a time: a busy seat refuses at once (125), naming the run that holds it. |
| **Host lease** | Shared by every run on a machine; held alone by an exclusive run. |
| **Run** | One attempt. It ends with exactly one receipt and one exit code. |
| **Receipt** | The caller's account, `.ferry/receipts/<id>.json`: the commit, the binaries on both ends, the outcome, custody, lease and admission facts. `state: submitted` until the run is final. |
| **Host record** | The place's account, `records/<id>.json`: reserved before any effect, final exactly once. A run whose supervisor is provably gone is reconciled to `lost` (255). |

## Commands

```
ferry run <recipe> [--on <machine> | --local] [--exclusive] [--seat <name>]
          [--once] [--commit <rev>] [--timeout-seconds <n>] [--wait-seconds <n>]
          [--json] [-- <args>…]           (--once: the seat lives for this run only)
ferry follow <id>                   re-attach: the run's output, then its receipt
ferry stop <id> [--other --reason '<why>']
                                    stop your run (another caller's needs a reason)
ferry record <id>                   the receipt, completed once the run is final
                                    (exit 3 while it has no verdict yet)
ferry recipes                       the recipes at this commit
ferry machines                      the configured machines
ferry receipt <id>                  print a receipt
ferry seats prune [--on <machine>] [--idle <dur>]
                                    retire this caller's seats whose workspace
                                    is gone, and with --idle (1d, 12h, 0s)
                                    those unused that long (SPEC §9)
ferry status [--on <machine>] [--json]
                                    what a place sees now: disk, live runs, the
                                    gate, seats, leftovers (default: this
                                    repository's local place); read-only
```

`ferry status` is read-only: it takes no lock and changes nothing. Its
fields are in [docs/REFERENCE.md](docs/REFERENCE.md#ferry-status).

### Outcomes and exit codes

Read the receipt's `outcome`, not the recipe's printed text, to tell a Ferry
refusal from a recipe failure. A recipe may itself exit 125.

`ferry record <id>` exits with the final record's code, or 3 while the run
has no verdict yet (`state: submitted`): poll again, or `ferry follow <id>`.
Exit codes are for people. A recipe's own code can be 3, 124, 125 or 255,
so a program decides by the `--json` receipt: `state` first, then
`outcome` (SPEC §4).

| outcome | exit | meaning |
|---|---|---|
| `ran-pass` | 0 | The recipe ran and exited 0. |
| `ran-fail` | the recipe's code | The recipe ran and failed. |
| `timeout` | 124 | `--timeout-seconds` (or the run's absolute deadline) elapsed; the recipe's session was ended. |
| `refused` | 125 | Nothing ran: unknown recipe or machine, unmet requirement, dirty or unknown commit, busy seat, host policy, or a host that refused before acceptance. |
| `stopped` | 128+signal | `ferry stop`, a signal to the caller, or one to the supervisor came before the recipe's result: 130 for SIGINT, 143 for SIGTERM, 129 for SIGHUP. `stopped_by` names who and why. |
| `lost` | 255 | The run ended without a verdict of its own: its supervisor is gone (reconciled), or something failed after the recipe started. |

A recipe that already reached its result is never turned into `stopped` or
`lost`. Later events (a signal during the collect hook, a failed release) are
noted in `reason` as "after the verdict: …".

A receipt whose `state` is `submitted` claims no verdict (it reads `lost`
255): the caller left, could not deliver its stop, or the place stopped
answering, while the run may go on. `ferry record <id>` completes it from the
place's final record.

## How it works

- **Transport.** The caller pushes the exact commit to the machine's private
  store over SSH and asks the place to run it; the place resets a warm,
  private checkout (the seat) to that commit.
- **Detached runs.** A supervisor on the place owns the run, writes its output
  to a file and keeps one authoritative record. The caller follows the output;
  a dropped connection never ends the run. Every run has an absolute deadline.
- **Locks.** A seat runs one run at a time (a busy seat refuses at once); a
  machine-wide lease lets an exclusive run hold the host alone. The kernel
  holds the locks, and only a run's seat lock is inherited by its processes.
- **Custody.** When a run ends, its session is stopped, and one pass lists any
  process still carrying the run's `FERRY_ID`.

[docs/REFERENCE.md](docs/REFERENCE.md) has the details: project configuration,
artifacts, shipped objects, capabilities, machines and host policy, seat
bounds, hooks, and each mechanism above.

## Limits

- **Not a sandbox.** Runs execute as you, with your permissions.
- **Outside custody:** processes a run did not start, other users and other
  hosts. Descendants that leave the run's session are listed, not stopped;
  one that clears its environment is not listed.
- **A supervisor killed outright** stops nothing. Its run is reconciled to
  `lost`/255 by its caller or the next run on the seat.
  - The inherited seat lock keeps other runs off its seat while its work
    lives, on every platform; the host lease does not outlive it.
- **Remote evidence is not publication authority.** A remote result does not
  certify a repository's own local gate.
- **Timings:** never pool machine timings across platforms.

## Documentation

- [docs/REFERENCE.md](docs/REFERENCE.md): configuration and behaviour in full.
- [docs/SPEC.md](docs/SPEC.md): the normative target, terse.
- [docs/PRINCIPLES.md](docs/PRINCIPLES.md): what Ferry is and is not, and how design
  questions are decided.
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): where SPEC's model lives in the code.
- [docs/RELEASE.md](docs/RELEASE.md): install, upgrade and rollback on shared hosts.
- [docs/design/](docs/design/README.md): dated design records, with their reviews and
  evidence. The rationale is
  [Ferry minimal](docs/design/2026-10-09-ferry-minimal.md).

## Development

```sh
nix develop    # the toolchain and every tool the checks use
just check     # rustfmt, strict Clippy, unit tests and real-CLI contract tests
```

[CONTRIBUTING.md](CONTRIBUTING.md) describes how to propose a change;
[AGENTS.md](AGENTS.md) is the maintainers' full working agreement.

## Contributing

Issues and pull requests are welcome. Read
[`CONTRIBUTING.md`](CONTRIBUTING.md) first: Ferry is small on purpose, and
a change that adds a mechanism is weighed against
[`docs/PRINCIPLES.md`](docs/PRINCIPLES.md). Report security problems privately, as
[`SECURITY.md`](SECURITY.md) describes.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option. Unless you explicitly state
otherwise, any contribution intentionally submitted for inclusion in Ferry
by you, as defined in the Apache-2.0 license, shall be dual licensed as
above, without any additional terms or conditions.
