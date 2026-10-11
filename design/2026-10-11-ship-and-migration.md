# Slices 6 and 7: one ship, one closed root

Plan note for M1 slices 6 (ferry-xtx.12) and 7 (ferry-xtx.9), deletion
first. Main is b2756526, src 5985. The ceiling: slices 4–7 end below 5950
net (coordinator, 2026-10-11, after ferry-xtx.24). Inventory: a read-only
sweep of src/ and tests/ on b2756526 (line ranges in §5).

## 1. The change in one paragraph

Slice 6 replaces companion refs and mirrors with one `[ship]` table and one
mechanism. Every shipped object goes to the place's one store
(`store/repo.git`, SPEC §9). It is pinned under one ref scheme and
presented to the checkout in one of two ways: as a ref, or as a URL served
from the store. Slice 7 closes old roots with the `locks/legacy-closed`
sentinel (SPEC §6). It deletes the `host-run` shim and the legacy-worktree
migration, and converges the layout on SPEC §9's names. Each slice removes
more than it adds. Together they bring src to about 5760, below 5950.

## 2. Slice 6: ship (SPEC §5 "objects", §10 `[ship]`)

**Today, two mechanisms doing one job:**

- **Companion refs:** the caller resolves each ref to a commit and pushes it
  into `repo.git` under `refs/ferry/<seat>/<id>/companions/<c>`. The place
  runs `git update-ref` in the seat clone and removes last run's refs, using
  a per-seat manifest (`companion-refs/<seat>.json`).
- **Mirrors:** the caller makes a bare repo per URL (`mirrors/<sha(url)>.git`)
  over raw ssh, pushes the commit, and pins `refs/heads/ferry/<c>` with a
  compare-and-set script. The place writes `<seat>/.ferry/gitconfig`
  (`url.file://….insteadOf=<url>`) and sets `GIT_CONFIG_GLOBAL`.
- **Duplicated between them:** two stores, two keep-alive ref schemes, two
  commit resolvers, two 40-hex checks, two catalog merges, and two receipt
  fields.

**Proposed: one `[ship]` table.**

```toml
[[ship]]              # an extra ref in the checkout
ref = "refs/heads/companion"
from = "main"         # a caller revision, or { remote = "origin", branch = "x" }

[[ship]]              # a dependency served from the place's store
url = "https://github.com/org/dep"
from = "deps/dep"     # a caller-local repository path
commit = "<hex>"      # optional pin; default that path's HEAD
```

- **Caller:**
  - resolves each entry to a commit (one resolver; the tracking-ref rule
    kept);
  - pushes all of them in the one push to `store/repo.git` that already
    carries the main commit, under `refs/ferry/ship/<c>`.
- **Shipping happens after capability gating.** Today mirrors are pushed
  before the `detach` check, so an old place receives objects before its
  refusal. The new order fixes that.
- **Place:**
  - a `ref` entry is `update-ref` in the seat clone, as today. The stale-ref
    manifest goes: refs a run did not ask for are removed by listing the
    clone's refs against the request, with no file to keep.
  - a `url` entry is one `insteadOf` line pointing at `file://<store>`. The
    gitconfig and the `GIT_CONFIG_GLOBAL` reservation stay, since they are
    the isolation that the consumer test
    `environment_hook_cannot_replace_ferry_owned_mirror_configuration`
    proves.
- **Receipt:** a single `ship: [{ref|url, commit, from}]` replaces
  `companion_refs`, `companion_sources` and `mirrors`. Old receipts still
  parse; the old fields stay readable as `Value`.

**Deletes** (inventory §5.1): about 287 src lines.
- caller: `transport::mirror` and `pin_mirror` (69), `push_companions`
  (15), `repository::companion_commit` (38) and the two catalog merges (12);
- place: mirror stores and their checks (19), the companion manifest and
  most of `materialize_refs` (36);
- request, receipt and config fields (about 60);
- the `FERRY_MIRRORS_DIR` export.

**Adds:** the `[ship]` parse and validation (about 40), one resolver (about
30), one push list (about 10), and checkout presentation (about 40). About
120 in all.

**Net: about −165.** Running src about 5820.

**Contract change** (consumer GO: herald-1b, which uses both mechanisms;
murail-4a):
- `ferry.toml` `[companion_refs]` and `[[mirror]]` become `[[ship]]`. Is
  the old syntax refused, or translated for one release? **Q1.**
- The receipt fields change, as above.
- **`FERRY_MIRRORS_DIR` is gone.** Does any consumer read it? **Q2.**
- The served URL's refs: today a mirror serves `refs/heads/ferry/<c>`. From
  one store, a dependency fetch of that ref still works, but the store also
  holds the main repo's refs. A dependency fetcher asking for other heads
  would see them. Is that acceptable, or does each URL need its own
  ref-filtered view (`uploadpack.hideRefs`)? **Q3.**

**Controls**, each with a knockout: SPEC §12.11, nothing is shipped before
the capability refusal (the mirror gap above); a `ref` entry and a `url`
entry in one run; stale refs removed without a manifest; the tracking-ref
rule; `GIT_CONFIG_GLOBAL` still Ferry's. The four companion and mirror
tests are rewritten, not dropped.

## 3. Slice 7: migration and layout (SPEC §6, §9, §12.11)

**Deletes:**
- the `host-run` shim (`HostRun` CLI and `place::host_run`, 18). SPEC §6:
  there is no shim; a caller is never installed before its places.
- `migrate_legacy` and the legacy-checkout refusal (52). Its test,
  `legacy_worktree_seats_migrate_under_the_existing_lease`, goes with it.
  An old root is closed instead of migrated.
- small dead or duplicated code, verified by grep:
  - `Machine.cores` is never read (1);
  - the preserve re-validation in prepare (3) and the reserved-environment
    re-check in `execute_session` (7), both already done by `validate`;
  - five copies of the 40-hex check become one helper (−6).

**Adds:**
- `locks/legacy-closed`: created when a new binary first opens a root.
  This needs old binaries to refuse it. Do they already? **Q4.**
- Retire also deletes `caches/.measured-<key>` (found by the sweep; it is
  left behind today).

**Layout convergence (SPEC §9 names):**
- `slots/<key>` → `seats/<key>/checkout`, `caches/<key>` →
  `seats/<key>/cache`, `repo.git` → `store/repo.git`;
- the hourly stamp moves inside `seats/<key>/`, under the cap's own
  exclusion;
- lock names `locks/<key>`, `host-exclusive` and `host-gate` →
  `seat.<key>`, `host` and `host.gate`.

This is a rename, about 0 net in src and about 40 test sites. It needs a
fresh root (Q4): an old root is closed rather than converted. Kept, because
it is the old-binary interlock: `locks/host-exclusive` stays as long as an
old binary could run on the same root. With the sentinel, it can't, so it
goes too.

**Net: about −70.** Running src about 5750.

**Contract changes:** a fresh state root on every place, in the joint
install window. Old roots are closed, never shared (SPEC §12.11). The
remote root, hardcoded as `.local/state/ferry` six times in transport.rs,
goes through `Place`.

**Controls**, each with a knockout:
- §12.11 both ways: an old caller against a new place is refused naming
  its revision, and a new binary refuses a root marked by an old one, or
  marks it;
- the sentinel blocks a mixed root;
- retire leaves nothing of the seat behind.

## 4. Order and ceiling

| After | src | Note |
|---|---|---|
| b2756526 (now) | 5985 | 35 over |
| slice 6 | about 5820 | below 5950 |
| slice 7 | about 5750 | about 200 under |

Slice 6 goes first: it closes the mirror-before-gating gap, and slice 7's
rename is cheaper with one store. Both are estimates from the inventory.
Each freeze states the measured running src.

## 5. Inventory (b2756526)

- **Ship:**
  - config.rs 32–39, 49–53, 69–83, 154–158;
  - repository.rs 128–165, 214–219, 238–243, 265–272;
  - execution.rs 35–38, 65–71, 82–87, 119–124, 284–303, 410–445;
  - transport.rs 208–276, 843–857;
  - main.rs 183–189, 220–222, 234–242, 402–407;
  - receipt.rs 88, 93–96;
  - run.rs 721–724, 734–739.
- **Migration:** main.rs 64–69, 467; place.rs 663–673; execution.rs 271,
  280–282, 362–409.
- **Tests rewritten or removed** (run_contract):
  - `published_companion_*` (two);
  - `companion_refs_are_pinned_and_isolated_between_seats`;
  - `mirrors_are_local_to_the_seat_and_record_the_exact_commit`;
  - `legacy_worktree_seats_migrate_under_the_existing_lease`;
  - `environment_hook_cannot_replace_ferry_owned_mirror_configuration`
    (kept);
  - the `"mirrors": []` request fixtures at three sites.

## 6. Questions for the coordinator

- **Q1:** the old `[companion_refs]` and `[[mirror]]` syntax: refuse with a
  "use `[[ship]]`" message (my default; no translation code), or translate
  for one release?
- **Q2:** `FERRY_MIRRORS_DIR`: does Herald or Murail read it?
- **Q3:** a shared store served for dependency URLs: accept that a fetcher
  sees the main repo's refs, or hide them per URL?
- **Q4:** the sentinel and a fresh root: do the deployed 4a binaries refuse
  `locks/legacy-closed`, or does slice 7 rely on the joint install window
  alone (every binary on a host replaced at once)?

## 7. Review (coordinator, 2026-10-11): approved as a plan; build 6 first

- **Q1:** refuse the old syntax, with no translation code: "[companion_refs]/[[mirror]] are retired: use [[ship]] (see README)". The same applies to the catalog adapter's JSON `mirror` and `companion_refs` keys.
- **Q2:** Murail uses neither mechanism. Herald uses both:
  - `ferry.toml` `[companion_refs]` `"refs/remotes/origin/master" = "master@origin"`;
  - `bin/ferry-catalog` emits `mirror: [{url, path: "deps/ese", commit}]` from mix.lock.

  Neither reads `FERRY_MIRRORS_DIR`. Ferry never edits Herald; herald-1b prepares the Herald side for the joint window. The mapping:
  - (a) `[companion_refs] "refs/remotes/origin/master" = "master@origin"` → `[[ship]] ref = "refs/remotes/origin/master"`, `from = "master@origin"`;
  - (b) the adapter's `mirror: [{url, path, commit}]` → `ship: [{url, path, commit}]`, the same three fields;
  - (c) `deps/ese` is still served at the same URL and commit, from the store.

  Since Herald's ref comes from `ferry.toml` and its URL from the adapter, entries from both are combined. Only the same `ref` or `url` declared twice is refused, replacing today's "two owners" rule for these keys.
- **Q3:** the shared store is accepted, with no hideRefs. The only fetcher is the run's own recipe, as the same user. Every shipped commit is namespaced: `refs/heads/ferry/ship/<commit>`. It sits under `heads` because a dependency clone fetches only heads; that is why mirrors pinned `refs/heads/ferry/<c>`. The name contains the commit, so a second push of the same commit is a no-op, and the compare-and-set pin script goes.
- **Q4:** the deployed binaries predate the sentinel, so slice 7 puts the M1 root at a fresh path, writes `locks/legacy-closed` in the old root at first start, and leaves the old root for a one-time operator cleanup (RELEASE).
- **Confirmed:** the mirror-before-capability-check gap is a bug, fixed in 6 with a control: an old place receives no objects before its refusal. Retire deletes `caches/.measured-<key>` (7). Each freeze states the running src.
- **Implementation note:** the stale-ref manifest moves into the seat clone's own git config (`ferry.ref`, multi-valued), not a listing. A shipped ref may sit in the clone's own namespace (Herald's is `refs/remotes/origin/master`), so a listing cannot tell Ferry's refs from git's. The list then lives and dies with the seat, and `companion-refs/<seat>.json` goes.

## 8. Slice 6 as built (2026-10-11)

- **Config:** `[[ship]]` is an untagged list of `{ref, from}` (`from` a
  revision or `{remote, branch}`) or `{url, path, commit}`. The retired
  `companion_refs` and `mirror` keys parse as ignored values only so that
  they refuse with the new form, in `ferry.toml` and in the adapter's JSON
  (serde cannot combine `flatten` with `deny_unknown_fields`, so each
  struct carries the two keys).
- **Caller:** one resolver (`repository::ship`) returns each object, the
  caller repository holding it, and its receipt line; it refuses a ref or
  URL declared twice. `transport::ship` pushes every object to the one
  store under `refs/heads/ferry/ship/<commit>`, after the capability
  checks; the local route fetches with the same refspec.
- **Place:** `present_ship` makes `ref` entries with `update-ref` and lists
  them in the seat clone's own config (`ferry.ref`, `--local`), removing
  those a later run does not ship; `url` entries add an `insteadOf` value
  (`--add`, since all URLs now share one store) to the checkout's
  `.ferry/gitconfig`, which `GIT_CONFIG_GLOBAL` points at.
- **Deleted:** `transport::mirror`, `pin_mirror` (and its compare-and-set
  script), `push_companions`, the per-URL bare stores and their remote
  `git init` over raw ssh, the companion manifest file, `materialize_refs`,
  `Mirrored`, `FERRY_MIRRORS_DIR`, the two-owners rules for these keys.
- **Size:** src 5985 → 5965 (−20), not the −165 the plan estimated. The
  inventory counted the 287 lines the two mechanisms occupied but
  underestimated their replacement (the typed entries, the resolver with its
  duplicate rule, and `present_ship`, after rustfmt). Slice 7's estimate is
  about −70, which would bring src to about 5895, below 5950; slice 7 is
  measured, not estimated, at its freeze.
- **Receipt:** `mirrors`, `companion_refs` and `companion_sources` are
  replaced by `ship`; an older receipt still parses (its old keys are
  ignored, not carried).
- **A test that never tested it:** the old mirror test checked stdout for
  the dependency commit, which the receipt JSON always contains; the
  recipe's output is on stderr with `--json`. Its replacement checks
  stderr for the served ref.
