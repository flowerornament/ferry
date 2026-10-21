# Ferry reference

Configuration and behaviour in full. [README.md](../README.md) is the
introduction; [SPEC.md](SPEC.md) is the normative target.

- [Project configuration](#project-configuration)
  ([artifacts](#artifacts-spec-8), [catalogs and adapters](#an-existing-catalog-or-adapter),
  [shipped objects](#shipped-objects), [capabilities](#provider-capabilities))
- [Machines](#machines) ([host policy](#host-policy-and-admission),
  [seat bounds](#seat-bounds-spec-9))
- [Hooks and environment](#hooks-and-environment)
- [Version 2 names](#version-2-names) (and reading version 1)
- [How a run is kept honest](#how-a-run-is-kept-honest)
- [`ferry status`](#ferry-status)
- [Development](#development)

## Project configuration

Commit `ferry.toml`:

```toml
version = 2

[recipe.test]
command = ["devenv", "shell", "--", "mix", "test"]
needs = ["linux-x86_64"]     # what the machine must have
may_run_elsewhere = true     # may run on another machine

[hooks]
prepare = "scripts/ferry-prepare"
env = "scripts/ferry-env"
collect = "scripts/ferry-collect"
```

- **Read from the exact commit.** Recipes and requirements always come from
  the commit being run, never the working tree.
- **Explicit argv.** Ferry never infers a tool from a recipe's name.
- **Only `may_run_elsewhere = true` recipes go remote.** Publishing and certifying
  recipes stay unroutable; for example, a repository's `just land` stays
  local.
- **`exclusive`.** `needs = ["exclusive"]` (or `--exclusive`) holds the host
  lease alone. It is an execution lock, not a machine feature: routing ignores
  it.

### Artifacts (SPEC §8)

A recipe's `artifacts = ["out/report.json", "logs/*.txt"]` are collected from
the checkout once the recipe ends or times out, whatever its verdict, and
fetched into `<repo>/.ferry/runs/<id>/`, which is emptied first whatever the
status. A run stopped or refused before its recipe ended collects none, and
its status says so.

- **Bound per file.** The place hashes each file; the caller checks every
  entry: a listed regular file inside the directory, with its listed digest,
  and every listed file present. The receipt's `artifacts` lists what was verified:
  `[{path, sha256}]`.
- **A status apart from the verdict.** `artifacts_status` is `none` (none
  declared), `fetched`, or `{"failed": {kind, path, error}}` with kind
  `collection`, `empty-pattern` (a pattern matched nothing), `transfer`,
  `digest` or `archive` (a missing, unlisted or unsafe entry). A `ran-pass`
  with failed artifacts is still a pass. Read the status, never the reason
  or files already on disk. After a failure nothing is left there, unless
  the earlier directory could not be removed: that is reported as `archive`
  failed, and what remains is never vouched for.
- **Never stale.** A file in a `keep_between_runs` path fails collection: the checkout
  reset keeps those, so it may be an earlier run's.

### An existing catalog or adapter

If recipes already have a catalog, point to it:

```toml
version = 2
[catalog]
path = ".config/gates.toml"   # its entries use requires/routable at either version
table = "recipe"
```

If the catalog has another schema, set `[catalog] adapter =
"scripts/ferry-catalog"`.
- **What the adapter emits:** this committed executable runs caller-side in
  fresh checkouts of the pinned commit and prints `{"version":2,"recipe":{...}}`
  (its own version, checked on its own: either 1 or 2 during the
  compatibility release, whatever `ferry.toml` declares). It may also emit:
  - an `also_send` list (the `[[also_send]]` entries below), for example dependencies
    derived from the repository's lockfile. Entries from the adapter and
    from `ferry.toml` are combined; the same ref or URL declared twice
    refuses;
  - an `environment` declaration;
  - `keep_between_runs`, a list of ignored build directories kept across resets, for
    example `["_build", "deps", ".devenv"]`.
- **Checks:** Ferry checks repeatability, validates the whole contract before
  routing, and records the digests of the adapter and the emitted contract.
- **Purity is your responsibility.** The adapter must be a pure reader; Ferry
  is not a sandbox.

**Preserved directories** are a declared cache:
- a run can depend on earlier runs in the same seat, not only on its commit;
- paths must be literal ignored directories with no tracked files, never
  `.git`, `.jj` or `.ferry`;
- tracked files, HEAD and hidden index flags are still checked;
- omit `keep_between_runs` for a clean run.

### Shipped objects

A run can send more than its commit. Each `[[also_send]]` entry is either an
extra ref in the checkout or a dependency served at its URL:

```toml
[[also_send]]     # an extra ref in the checkout
ref = 'refs/remotes/origin/master'
from = { remote = 'origin', branch = 'master' }   # or a revision: 'master@origin'

[[also_send]]     # a dependency, served from the place at its URL
url = 'https://github.com/org/dep'
path = 'deps/dep' # a caller-local repository
commit = 'abc123…'
```

- **Which commit:** `from` as a table selects the caller's locally known
  tracking ref, not the local branch or the remote's current state; a string
  keeps the caller's revision syntax. Ferry does not fetch or fall back, and
  the ref must name a commit directly (not a tag object).
- **One store:** every object goes to the place's one store with the run's
  commit, only after the place's capabilities are checked (`ship`, besides
  `detach`), and is kept as `refs/ferry/ship/<commit>`, outside the heads a
  seat's clone copies.
- **In the checkout:** a `ref` entry is made in the seat's clone (and a ref
  a later run no longer ships is removed); a `url` entry is served from a
  bare repository of the run's own (it borrows the store's objects and holds
  only this run's declared commits) through the checkout's git configuration
  (`GIT_CONFIG_GLOBAL`), so the recipe fetches it at its URL. A recipe sees
  its commit and its own declared entries, never another project's.
- **In the receipt:** `ship`, one `{ref | url, commit, from}` per entry.

### Provider capabilities

A project can require capabilities of the Ferry that runs it, so that a
rollback or an older binary on one end cannot silently run without them:

```toml
[provider]
needs = ["admission"]
```

| capability | meaning |
|---|---|
| `args` | recipe arguments after `--` |
| `refusal-event` | typed refusals before acceptance |
| `custody` | seat custody records at release |
| `lease` | the shared/exclusive host lease |
| `admission` | host policy admission (`host.toml`), even when not configured |
| `detach` | detached runs (`ferry place` submit, follow, stop, record); a caller refuses a place without it (125, naming the place's `source_rev`) |
| `retire` | the place's `retire` verb for `ferry seats prune`; prune refuses a place without it (125, naming its `source_rev`) |
| `seat-lifetime` | `run --once`, `seats prune --idle`, and a `retire` that checks the seat's latest use, never deletes during an exclusive run, and renames to a tombstone first; a `--once` run or an idle retire refuses a place without it |
| `run-history` | `delete_run_history_after_days` in `host.toml`, and a not-found answer that names it |
| `ship` | `[[also_send]]` requests (`[[ship]]` in version 1) and the one store; a caller refuses a place without it before pushing anything (125, naming its `source_rev`) |
| `status` | the place's `status` verb; `ferry status` refuses a place without it (125, naming its `source_rev`) |

A run is refused (125) before any hook runs if:
- the caller does not know a required capability;
- the host's `host-info` lacks one (a host from before capabilities lists
  none);
- the host refuses the request.

The table may sit in `ferry.toml` or in an adapter's contract, not both. The
receipt records `provider_requires`.

## Machines

`$XDG_CONFIG_HOME/ferry/machines.toml` (default `~/.config/ferry`):

```toml
version = 2
[machine.bench]
ssh = "bench"
platform = "x86_64-unknown-linux-gnu"
has = ["linux", "x86_64", "quiet"]
reserved_for = ["quiet"]   # optional: chosen without --on only for recipes that need one of these
```

- **`binary`** (default `.local/bin/ferry`, relative to the host's home) is
  the path to `ferry` on the host. After `nix profile install`, set
  `binary = ".nix-profile/bin/ferry"`.
- **Routing:** a `may_run_elsewhere` recipe run without `--on` or `--local`
  goes to the first machine, by name, that `has` everything it `needs`.
  A machine with `reserved_for` is skipped in that choice unless the recipe
  needs one of the listed features (one is enough); with no other machine,
  the run is refused naming it (`bench is reserved_for quiet; use --on
  bench`), never sent there. `--on` names a machine: it bypasses a
  reservation, never `may_run_elsewhere` or `needs`. Every `reserved_for`
  name must be in that machine's `has`, or `machines.toml` is refused.
- **SSH:** aliases come from your SSH configuration. Ferry copies no
  credentials and provisions nothing.
- **Features are declarations,** not measurements.

### Host policy and admission

A machine's operator may set its rules in `~/.config/ferry/host.toml` on
that machine. Each run is admitted under its seat lock, after the host lease,
before checkout and hooks.

```toml
version = 2
# Below this much free disk, a run is refused (125).
refuse_runs_below_free_disk = "40 GiB"
# Optional: a seat whose kept directories and cache exceed this starts cold.
max_cache_per_checkout = "20 GiB"
# Optional: the longest timeout a run may ask for, from 1s to 7d (else 6h).
# Checked at submit, and again at admission: the reading under the seat
# lock decides, so a limit lowered while a run waits refuses it.
max_run_time = "6h"
# Optional: each run deletes the history of runs submitted over 7 days ago.
delete_run_history_after_days = 7
```

- **No file:** the run proceeds, and the receipt records `not-configured`.
- **A malformed policy refuses** before anything is measured: an unknown
  version or key.

The receipt's `admission` records the policy read, the floor, the filesystem,
the bytes available, and the result. Availability can change for reasons
outside Ferry, so it is an observation.

### Seat bounds (SPEC §9)

Seats are bounded in count and in size; neither touches source, logs,
receipts or records.

- **Count.** Each accepted run appends one line to the caller's seat book,
  `~/.local/state/ferry/seats.jsonl` (`{key, place, repo_path, seat,
  last_used_ms}`; one `O_APPEND` write, never rewritten). A failed append is
  retried when the run completes, and a failure then is a note. The book is
  the machine user's, not one workspace's: a prune considers every seat
  this user's caller recorded, whichever agent's workspace it serves, and
  never another caller's. A seat ends in one of three ways:
  - **Workspace gone.** `ferry seats prune [--on <machine>]` retires each
    recorded seat whose workspace is provably gone: its name is absent from
    a parent directory that can be listed and is on the device recorded with
    it. Anything else (a denied lookup, a missing parent, an unmounted
    volume) is reported as `kept … unknown` and never retired by this rule.
    So a workspace removed together with its parent directory (say
    `~/tmp/x/` deleted whole) is pruned only by `--idle`.
  - **Idle.** `ferry seats prune --idle <dur>` (`1d`, `12h`, `30m`, `0s`)
    also retires each seat whose newest `last_used_ms` for that place is
    older than `<dur>`. A line without `last_used_ms` has an unknown age and
    is kept. With `delete_unused_checkouts_after_days = <n>` at the top of
    `machines.toml` (a whole number, at least 1; a "checkout" is a seat: the
    place's copy of the repository, with its build cache), every `ferry run`
    on a remote place also prunes that place with `--idle <n>d`, after its
    receipt and output are complete: at most once an hour per place (claimed
    under an flock on `~/.local/state/ferry/pruned.lock`, the time kept in
    `pruned.json`), starting no new retire after 10 s. It prints one line on
    stderr (`ferry: pruned <place>: retired …, kept …, in …`) and never
    changes the run's exit code or `--json` output.
  - **One run.** `ferry run --once` declares the seat lives for that run
    only. Once the run's record is final, its supervisor closes the seat
    lock (never `LOCK_UN`) and retires the seat, checking it is still this
    run's. A process the run left that kept the seat's lock keeps the seat
    busy until it exits; a later prune then retires it. The record says
    `once: true`, never whether the retire happened.

  The place's `retire` verb waits on nothing. It takes a shared host lease
  by try, through the gate, so it is skipped (`exclusive`) while an
  exclusive run holds the host or waits for it, then tries the seat's lock
  (held: skipped, busy). A run refused meanwhile is told the seat "is being
  retired". Under the lock it reads `seats/<key>/used` (`{ms, id}`, written
  by each run when it takes the seat, only inside the state root; a run that
  cannot write it is refused): a seat used within the idle age on
  the place's clock, or by a run other than the `--once` one, is skipped
  (`used since`); a `used` it cannot read keeps the seat (`unknown`). It
  then renames `seats/<key>` to a tombstone `seats/.retired-<key>-<ms>`,
  records `records/retired-<key>.json` (replacing an earlier retire's of
  the same key), deletes the tombstone, and keeps the lock file. A delete
  that fails leaves only the tombstone and is reported failed: the key is
  gone, so the next run on it starts cold and whole; `status` lists the
  tombstone, and the next prune, retiring that key again, deletes it. A
  retire deletes only its own key's tombstones. Prune prints one line per
  seat it considered (retired, already gone when an earlier retire such as a
  `--once` run's took it, skipped busy, used since or exclusive, kept
  unknown, failed) and appends a tombstone line to the book for each one
  retired; a later run on the same workspace revives it. A retirement line
  carries the last use its prune meant, so one acknowledged late, after a
  run had already recreated the seat, never hides it, as long as the
  recreating run's last use reads strictly later than the use the retire
  meant: an equal millisecond, or a caller clock stepped back, can still
  hide it. The other way round, a retire that deleted the
  recreated seat itself (it ran after the run that recreated it) leaves
  the book listing the seat live: the next prune that considers it finds
  it absent and books it gone. Local seats live
  inside their workspace and go with it, so they are not recorded. Exit 0
  when every seat was retired or kept by rule, 125 if a place refused
  otherwise (including a place without the capability it needs, named by
  revision), 255 if a place was unreachable. Below the disk floor, a run's
  refusal names `ferry status --on <place>` and `ferry seats prune --on
  <place> --idle <dur>`.
- **Size.** With `max_cache_per_checkout` set, a run measures its seat's
  kept directories and cache under its seat lock, before the checkout, when the last
  measurement is over an hour old. Only paths proven inside the seat are
  measured or wiped: one behind a symbolic link, or on another device (a
  volume mounted inside the seat), is skipped with a note, and every delete
  proves this again at its own moment. A
  run that has spent its wait budget, or a measurement over 30 s, is not
  measured (a note). Over the cap, they are wiped and the run
  starts cold: the receipt's `pruned` is `{bytes, cap}` and `reason` says so.
  `pruned.bytes` is what was measured over the cap; after a partial wipe
  (a path it could not delete, each a note) less than that was removed.
  The cap can be exceeded by at most an hour of runs' growth. A failed wipe is
  a note, and the run goes on with what is there.
- **Keep `seats/` a real directory** on the state root's disk.
  An operator who moves it to another disk through a symbolic link makes
  every cap and retire on that place refuse: safe, but they never act. Move
  the whole state root instead.

### Run history (SPEC §9)

Without `delete_run_history_after_days`, a place keeps every run's folder
(`runs/<id>/`: output, artifacts) and record (`records/<id>.json`) forever.
With it (a whole number of days, at least 1; anything else refuses the run
as a bad `host.toml`), each run, once its record is final and its locks are
closed, deletes the history of final runs submitted more than that many days
ago (the record's `submitted_ms`, on the place's clock), oldest first:

- **Never beside an exclusive run.** It takes a shared host lease by try,
  as a shared run does. If an exclusive run holds or waits for the place,
  the pass is skipped; an exclusive run that arrives during it waits for
  it (at most 1 s plus one file).
- **One second.** It lists `records/` by name, oldest first by the time a
  name carries (a run id starts with its submit time on the caller's
  clock), and never reads a run named less than `n` days less one hour ago;
  names without a time come last, temporary files before retirement
  records. It reads and deletes entry by entry and stops starting work after
  1 s; the rest waits for the next run. A run's folder goes first, then a
  reservation a crashed finalize left beside its record, then the record,
  each only after the one before succeeded, so a run deleted in part is
  found again and never comes back as `lost`. A run from a caller whose
  clock runs ahead of the place's is deleted that much later. Keep `runs/` a real
  directory on the state root's disk: one behind a symbolic link or on
  another device is never deleted (each pass logs the failure).
- **What stays.** A live supervisor's reservation (an old one whose
  supervisor is gone is reconciled to `lost` first, then ages); a run whose
  recorded survivor is still alive; seats, `store/`, and the caller's
  receipts and fetched artifacts. Survivors listed as `unknown` keep
  nothing. A run folder with neither a record nor a reservation (left by
  hand or by a crash before submit) is never reached, since the pass walks
  `records/`; remove it by hand.
- **Also aged.** `records/retired-<key>.json` by its `retired_ms` (without
  one it stays), the pass accounts by their `ms`, and a crashed write's
  temporary file (`records/.<name>.<pid>.<n>.tmp`) by its modification
  time, never read as a record.
- **Accounted.** A pass that deleted anything writes
  `records/history-<ms>-<run>.json` (`{ms, run, deleted, older_than_days}`,
  `deleted` counting whole runs) and a line in its run's `supervisor.log`.
- **After.** `ferry record <id>` and `ferry follow <id>` on a deleted run say
  `not found (this host keeps run history for <n> days)`, as context: the
  id may never have existed. Your receipt keeps the outcome, exit code and
  digests, and fetched artifacts stay in `<repo>/.ferry/runs/<id>/`; save
  any output you need for longer before then.

## Hooks and environment

Hooks are committed executables, with paths relative to the checkout. They run
in this order: prepare, env, the recipe (in the run's session), then collect
(in its own session, after the recipe's is stopped).
A run stopped before its session is spawned (a stop while it waits for locks
or prepares its seat) runs none of them, collect included: nothing ran.

- **env** prints a JSON object of environment strings, never shell code.
- **Inputs:** `FERRY_SEAT`, `FERRY_COMMIT`, `FERRY_ID`, `FERRY_WORKDIR`,
  `FERRY_CACHE_DIR`, `FERRY_REQUEST_FILE`; collect also gets
  `FERRY_EXIT_CODE`. Every hook and the recipe see the run's own `FERRY_ID`
  (SPEC §10) and request file, collect included.
- **Identity:** the checkout path's basename is not an identity (every
  seat's is `checkout`). A stable per-seat identity is `FERRY_SEAT`;
  `FERRY_ID` is per run.
- **Sockets:** keep Unix sockets out of the checkout. A remote seat's path
  (`~/.local/state/ferry/v1/seats/<key>/checkout`) is about 82 bytes on a
  typical Linux host, and a socket path is limited to 107: put sockets
  under `/tmp` or a short `$XDG_RUNTIME_DIR` path instead.
- **Reserved:** `FERRY_*` and `GIT_CONFIG_GLOBAL`. A hook cannot replace the
  run's identity or the seat-local git configuration that serves shipped
  URLs.
- **`FERRY_CACHE_DIR`** is a per-seat directory Ferry owns
  (`<state>/seats/<seat>/cache`, mode 0700). Put regenerable build output there,
  for example `CARGO_TARGET_DIR=$FERRY_CACHE_DIR/cargo-target`.
- **The `environment` declaration** has three lists:
  - `send`: caller variables sent to the recipe, recorded in the receipt,
    so never put secrets there;
  - `refuse_other_vars_starting_with`: prefixes whose other nonempty
    variables refuse before any push (one listed in `send` or `keep_local`
    never refuses);
  - `keep_local`: variables deliberately kept on the caller.
- **Projects own** their own database preparation and artifact collection. A
  service a run starts and expects to outlive it belongs to the operator's
  service manager (`PRINCIPLES.md`, worked example).

## Version 2 names

Each file declares `version = 2` and uses names a new user can read
(SPEC §10). Sizes are strings with a unit: `"40 GiB"`, `"512 MiB"` (binary),
`"40 GB"` (decimal), case-sensitive; durations a number and unit (`"6h"`,
`"30m"`).

| version 1 | version 2 |
|---|---|
| `requires` (recipe, `[provider]`) | `needs` |
| `routable` | `may_run_elsewhere` |
| `preserve` | `keep_between_runs` |
| `[[ship]]` | `[[also_send]]` |
| `[environment] forwards / watches / local` | `send / refuse_other_vars_starting_with / keep_local` |
| `[host.<name>]` | `[machine.<name>]` |
| `triple` | `platform` |
| `features` | `has` |
| `cores` | removed (routing never read it) |
| `min_free_bytes` (bytes) | `refuse_runs_below_free_disk` (a size) |
| `seat_cap_bytes` (bytes) | `max_cache_per_checkout` (a size) |
| (none; `max_timeout` was never read, and is refused naming the new key) | `max_run_time` (a duration, 1s to 7d) |

- **One release of both.** Ferry reads version 1 and version 2 of each file
  and of an adapter's JSON. `machines.toml` without a `version` is version 1.
- **No mixing.** A version 2 file that holds a version 1 key is refused,
  naming the new key (`min_free_bytes` is version 1: use
  `refuse_runs_below_free_disk`), and so is a size without a unit. Version 2
  keys (`reserved_for`, `max_run_time`) need `version = 2`.
- **A project and its adapter** each declare their own version and are read
  by it; the merged contract is the same either way.
- **What keeps its names:** records, receipts, the request and the protocol
  (`requires`, `routable`, `preserve`, `forwarded_env`, `ship`, …), and a
  catalog file named by `[catalog] path`, which is the project's own data:
  its entries keep `requires` and `routable` (other keys are ignored).
- **Order:** move a file only once every tool that reads it reads version 2.

## How a run is kept honest

### Nesting

A run started from inside a run (its process carries that run's
`FERRY_ID`) on that run's own place is refused (125) before anything is
sent: it would wait on the locks its own run holds. Before shipping, the
caller asks the chosen place for the outer run's record. Only that state
root can answer "reserved, not final" (`record` exits 3) for that id, so it
is the place itself saying it is the same place, whatever the machine is
called. A run to another place, or one started by a process the outer run
left behind after it ended, goes ahead. A process that dropped `FERRY_ID`
is not seen as nested.

### Transport

- **Exact objects only.** The caller pushes only the exact commit (and any
  `[[also_send]]` objects) to the machine's private bare repository, under
  `refs/ferry/<repo key>-<seat>/<commit>`, never to origin. Two concurrent
  runs of the same commit may race to create that ref. A push that loses the
  race to an identical ref counts as published.
- **One SSH connection per caller.** Ferry sets non-interactive SSH
  (`BatchMode=yes`) itself. Each caller process gets its own control
  connection in `~/.cache/ferry/ssh/<pid>/`, closed when the caller exits, so
  one run never rides on a connection another run's caller owns.
  - If that path would be too long for a Unix socket (a very long HOME), each
    SSH command opens its own connection instead.
- **SSH waits.** Each connection gets `ConnectTimeout=10`,
  `ServerAliveInterval=5` and `ServerAliveCountMax=3`, which end a connect
  that never completes and a link that dies. They do not reach a jump host's
  own connection, so the short replies a caller needs before it submits
  (`host-info`, a file read) also have a 60 s wall-clock bound, covering the
  reply's end too: past it the caller stops its own `ssh` and the run is
  refused (125), with nothing run. The store setup and the `ls-remote` check
  after a failed push are short replies under the same bound. The object
  push has no wall-clock bound, since a large push can legitimately take
  long; it has an output watchdog instead. The caller runs `git push
  --progress` (its ssh quiet, `LogLevel=ERROR`) in a process group of its
  own, and 60 s without a new byte of progress output (git's own lines or the
  place's `remote:` lines) ends it: the caller stops that group and the run
  is refused (125), "the push printed no progress for 60 s". Output is a
  proxy for progress, so it errs both ways:
  - a push stuck while something keeps printing (a remote hook, or a
    `ProxyCommand` wrapper with its own periodic output) is not cut;
  - real work silent for 60 s is refused. The likeliest case is the first
    push of a large repository into an empty store: after "Writing objects
    … done", the place's git checks connectivity without printing. A
    retry pushes it all again (a failed push keeps nothing), so the first
    run of a repository that big is best made over a fast link.
  A signal to the caller (Ctrl-C) stops the push at once, as before
  dispatch, and once git exits nothing it left in its group stays (if
  git's state cannot be read, it is reaped first and its group stopped
  after; a live push is never killed for an unreadable state). The
  `ls-remote` check after a failed push runs in the caller's own group, so
  Ctrl-C reaches it directly; on a timeout the caller stops its git, and
  that ssh ends on its pipe or by keepalive (15 s).
- **Submitted means asked.** The receipt is first written, as `submitted`,
  just before the place is asked to run the request. Anything that fails
  before that is a refusal (125), and a caller killed before it leaves no
  receipt claiming a run.
- **A run token** binds the host's facts to this caller's receipt.
- **The checkout is checked:** a seat lock protects a detached checkout, and
  HEAD, a clean tree and hidden index flags are checked before execution.
- **Local runs** use the same mechanism, in a clean checkout of the commit
  under `.ferry/local/v1`, not the caller's working tree.

### The host lease

Every run holds a machine-wide lease while it runs. Ordinary runs share it. An
exclusive run:
1. closes a gate, so no new run starts;
2. waits for in-flight runs to release their shared side;
3. runs alone.

There is no queue. Before it holds the gate, an exclusive run competes for it
like any other run. Receipts record `lease.mode`, `wait_ms`, `acquired_ms` and
`released_ms`, which is when the run's use of the lease ended, stamped just
before its record is final; the locks themselves close after the record
(SPEC §5).

**Scope.** "Exclusive" means no other Ferry run under the same state root:
one user's Ferry on one machine, or one repository's `.ferry/local/v1`. It is not
a quiet-host certificate, and it does not quiet:
- processes started outside Ferry;
- other users;
- other virtual machines on the same hardware;
- Ferry binaries older than the lease.

A run never waits for its seat: a held seat lock refuses at once (125). The
refusal names the run that holds it when the lock file names a live run.
Otherwise it says what it can prove. The lock may have outlived the run it
names ("held after run <id> ended"), or a run that is starting may hold it.
When the seat's live reservations make both possible, it names both: a
reservation shows a run is alive, not that it has the lock. The work
inherits the seat lock, on every platform, so whatever a run leaves running
while holding it keeps the seat busy. It never inherits the host lease: only
the supervisor holds it, so a leftover can never close the place to other
seats (SPEC §3). Its waits for the host lease end by its submit plus `--wait-seconds`
(default 30 min), apart from `--timeout-seconds`. Past it the run refuses
(125), saying what a probe of the lock proves. If it is free now: "released
as the wait ended; retry". If an exclusive probe fails but a shared one then
succeeds, that proves only that no exclusive holder remained: "held by shared
runs, or just released; retry". Otherwise it names the exclusive run that took it, reconciled first:
a live one holds it; an ended one is closing its locks, or an exclusive run
is starting. An error while probing names no holder ("holder unknown"). No
holder's name is ever cleared: the work inherits the seat lock and may
outlive its run.

A run's record is final before its locks close (SPEC §5), on every path: each
lock passes to the supervisor as it is taken, a refusal after that keeps it
until the refusal is final, and a panic in the supervisor is caught so the
record is still written first. If the record cannot be written, the locks
close only when the supervisor exits, and a reconcile then proves the run
lost. So a named run that is still live holds the
lock it names, and a free lock's last holder is always already final.
A place's refusal carries `why` and `next` apart (SPEC §6); the receipt's
reason is `<why>; <next>`.

### Custody: the session stop (SPEC §7)

The supervisor is always a freshly spawned `ferry place supervise`, started
by `place submit` in its own session, and the run's work has a session of
its own.

**At the end of a run,** the run's session (and then the collect hook's) is
stopped: SIGTERM to every member, up to 10 s for them to leave, SIGKILL, and
up to 2 s for them to be gone. Then one **marker pass** reads the
exec-time environment of every process of the place's user born since the
supervisor (`/proc/<pid>/environ` on Linux, `KERN_PROCARGS2` on macOS) and
lists those carrying this run's `FERRY_ID`: every hook, the recipe and what
they start inherit it, however they leave the session (`setsid`, a double
fork). The receipt's `custody` records:
- `tier`: `session`;
- `survivors`: the session's members still alive and the marked processes,
  one entry each, by pid, start time (`null` if it could not be read) and,
  when read, `command` (its `argv[0]` only: arguments may carry secrets); or
  `"unknown"`, when the session could not be listed at the end, an eligible
  process could not be read or changed during the read, or the pass took
  longer than 10 s (or the run's deadline). An unreadable process whose
  parent is neither pid 1 nor a process of the place's user (a login's
  `sshd-session`) cannot be the run's and is skipped. **macOS withholds the
  environment of its own platform binaries** (`/bin/sleep`, `/bin/sh`), so
  on a Mac that runs anything else the result is often `unknown`: never a
  list that might be short;
- `error`: what failed on the way (a signal, a listing), and why the pass was
  unknown, with what it did see. The stop never gives up early, and the
  verdict stands. Only a failed stop is noted in the reason.

The pass reports; it never stops anything. A survivor that keeps the seat's
lock keeps the seat busy until it exits, and the next run's refusal names
the run it outlived ("held after run <id> ended"): stop it by its exact pid.
One that also closed its descriptors holds nothing, and can write into a
later run's checkout or the seat's cache. A process started with a cleared
environment (`env -i`) carries no marker and is not listed (on macOS the
pass then reads it as withheld: `unknown`). What a run
starts is its project's to keep inside the run, or to stop in its collect
hook (SPEC §7). There is no taint, quarantine or `seat recover`.

**The seat lock outlives a killed supervisor.** It is inherited by the
recipe's processes, on every platform. A killed supervisor therefore frees
it only when the processes that kept it exit, so a new run on the seat is
refused busy instead of starting beside live work. The host lease closes
with the supervisor: an exclusive run whose supervisor is killed no longer
keeps other seats out (SPEC §11). Release closes descriptors, never
`LOCK_UN`.

**A killed supervisor's run still ends on time.** The session owner bounds
its session itself. Its watch fires 1 s after the run's timeout or
deadline, whichever is first; and once its supervisor is gone after the
recipe has ended, it stops the session then, as the supervisor would have.
Either way it stops it like the supervisor (TERM, 10 s, KILL), sparing only
itself, and the watch writes no result. When the session cannot be listed,
it signals the hook or recipe it is running instead, and its last act
kills its own process group, ending what those left in it. A process that
moved to a process group of its own is then out of reach, as it is for the
supervisor's stop when the session cannot be listed. Its timeout counts
from its own start, after the locks, so with a live supervisor the
supervisor's stop normally comes first. An owner that is itself killed
leaves its session unbounded. The record reconciles to `lost`. No marker
pass runs without the supervisor, so `ferry status` lists what the stop
leaves.

### Detached runs, output and the verdict

A run belongs to its place, not to the connection that asked for it:

- **Submit.** The caller sends the request (`place submit`, over SSH or as a
  local exec). The place starts the supervisor paused, reserves the run id
  with the supervisor's pid, then releases it and replies within seconds. A
  retried submit with the same id and token gets the same run back.
- **Follow.** The caller copies `runs/<id>/output.log` from a byte offset until
  the record is final and every byte before it has arrived. After a dropped
  connection it resumes from the bytes it has. If the place does not answer
  for 5 min, the caller exits 255 and its receipt stays `submitted`. Once the
  place has accepted a run, only its final record gives the receipt a
  verdict: any other failure (reading that record, for one) leaves it
  `submitted`, exit 255, with `ferry record <id>` as the next step. Artifacts
  have their own status and never change the verdict (see Artifacts).
- **Stop.** The first INT, TERM or HUP of the caller asks the place to stop the
  run (`stopped` 128+signal), and the caller keeps following to the record.
  The first stop decides: a later one, from anyone and with any signal, is
  accepted and changes neither the signal nor `stopped_by`. If the stop cannot
  be delivered, the caller prints `not stopped; continues until its deadline
  in <n> min; ferry stop <id>` and exits 255. A second signal leaves at once
  with 255. Either way the receipt stays `submitted`. A signal that comes
  after the run is final changes nothing: the caller delivers the rest of
  the output and exits with the run's code (a second signal leaves early,
  still with it). A caller killed outright stops nothing: the run goes on to
  its end or its deadline.
- **Deadline.** Every run ends by `submitted + wait + timeout + teardown`
  (`wait` is `--wait-seconds`, 30 min by default, the one bound on all its
  lock waits), which submit returns. A
  request without `--timeout-seconds` gets the place's maximum, 6 h (and none
  may ask for more).

**Output is data, never control.** The recipe's stdout and stderr share one
pipe, so the log keeps the order they were written in. One writer copies its
bytes into the log: no lines, no transcoding, nothing waits on a reader.
- **Cap:** past 1 GiB, output is read and dropped; the record says
  `output: {bytes, capped}`. A failed write is counted (`write_errors`).
- **End:** after the session stop, the writer gets 2 s more. A process outside
  the session that still holds the pipe is not waited for: the reason says
  "output still open", and the log is closed before the record.
- **The caller** writes the bytes exactly as logged, to stdout (to stderr with
  `--json`). A closed stdout or stderr is data: the run goes on.

**What the caller says** (SPEC §4), on stderr, each line starting `ferry: `:
- at submit, `<id> → <place>, seat <seat>, commit <short>`, naming the place
  even when Ferry chose it;
- for a git checkout whose HEAD is the commit run and which has uncommitted
  changes (outside `.ferry`), `uncommitted changes are not in <short>`; a jj
  working copy is itself a commit, so it has none;
- while the run has no output yet, each new wait note of its place, once.
  The supervisor writes `waiting for lock <name>: held by run <id>` after 3 s
  on a host lock; the caller reads it every 2 s with the place's `record`
  verb, which adds the latest note as `waiting` while the run still waits
  (it has no output log yet). It reads only until the run's wait budget
  ends, and it is narration: a read that fails is skipped, and a stalled
  read never holds the caller;
- at the end, the reason when there is one, then `<outcome>, exit <code>,
  run <id>`: the last line is always the verdict.

`ferry stop` says `stop sent to run <id>` or the place's refusal and its
next step. A stopped run's reason names the stop: `stopped: <why> (<who>) with
<signal>`, with `, another caller's run` after `<who>` for `--other`.

**The verdict is decided once,** by the first terminal fact (above), on the
place, and kept in its final record.

**`--json`** emits version-one NDJSON events on stdout and nothing else:
`submitted` (`id`, the supervisor's `pid`, `machine`, `seat`, `commit`),
`waiting` (`note`) for each wait note said, then `receipt`. A reader ignores
an event it does not know.

## `ferry status`

**`status`** reads the place's state root and process table once. It takes
no lock and changes nothing, so it never makes a seat look busy. It prints
one line per fact, sizes in KiB, MiB or GiB and times as ages, leaving out
what is unknown; `--json` gives every field below.
- **disk:** available bytes, the floor and the seat cap from `host.toml`, and
  why admission would refuse now, if it would.
- **runs:** each live run (reserved, not final, its supervisor alive) with its
  seat, recipe, `exclusive`, phase and deadline. Phase comes from the files a
  run has written: `waiting` (locks, admission, checkout), `preparing`
  (hooks), `running` (the recipe started).
- **gate:** `host.gate` keeps the name of the last exclusive run that took it.
  That run is the `holder` only while it is live; otherwise `unknown`, never
  a stale name. Every other live exclusive run is a `requester`.
- **seats:** each seat with the run its lock file last named, that run's
  outcome, the cap's last measurement, and its size from one walk bounded at
  5 s in all (`null` past it). Seats this caller knows from its seat book carry
  their workspace, as does each local seat of this workspace; other callers'
  show their key.
- **leftovers:** processes carrying the `FERRY_ID` of a run that is no longer
  live here, by pid, start, run and `argv[0]`. Those whose run this place does
  not know (another state root) are counted apart under `record_none`, and
  unreadable processes (another user's) are counted in `--json`. Stop a
  leftover by its exact pid.

Internal verbs: `ferry place`, `host-info` and `execute-session` are internal: the caller
runs them on the machine. A caller older than its place is not served (SPEC
§6): callers are never installed before their places.

## Development

- **Real execution required.** Transport changes need controls with real
  execution, not only unit tests: exact commits, refusal versus recipe exit,
  seat isolation, the session stop at the run's end or deadline, and the run
  surviving client loss.
- **Fault injection.** Debug builds read `FERRY_FAIL_AT=<point>[,…]` to
  inject failures at fallible points; release builds never read it.
