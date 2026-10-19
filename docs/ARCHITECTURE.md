# Architecture

How SPEC.md's model maps onto the code. SPEC says what must hold; this says
where it is done. Keep both true to the code, in the same commit as the
change.

## One run, end to end

```
caller (ferry run)                                place (ferry place <verb>, over ssh or a local exec)
──────────────────                                ───────────────────────────────────────────────────
main ── config, repository                         
  │     resolve recipe@commit, route (§10)         
  │     refuse 125 before any effect               
  ▼                                                
transport::ship ── push commit + [[ship]] ──────►  store/repo.git
transport::run_on ── submit{request} ──────────►  place::submit
  │                                                  record::reserve (O_EXCL)   ── records/<id>.reserved.json
  │                                                  fork supervisor (setsid)
  │                                                place::supervise
  │                                                  execution::prepare
  │                                                    lock::try_seat ── locks/seat.<key>   (refuse busy, named)
  │                                                    lock::HostLease ── locks/host[.gate] (bounded wait)
  │                                                    admission::admit / cap (§9 floor, seat cap)
  │                                                    reset checkout, present [[ship]]   ── seats/<key>/checkout
  │                                                  run::run_released (deadline absolute, §5)
  │                                                    session owner: prepare → env → recipe → (result.json | result.error)
  │                                                      own watch: stops its session at timeout/deadline (no result); unsupervised: stops it at the end
  │                                                    copy output ── runs/<id>/output.log (capped)
  │                                                    end/timeout/stop → custody: stop session (§7)
  │                                                    collect hook, own session, stopped ── runs/<id>/collect.*
  │                                                    custody::mark: one marker pass (FERRY_ID) → survivors
  │                                                    artifacts (§8)  ── runs/<id>/artifacts.tar
  │                                                  run::verdict: first terminal fact (§5)
  │                                                  record::finalize (once)   ── records/<id>.json
  │                                                  close locks; --once retire; history::delete_old (§9, shared host try)
transport::follow ◄── output.log by offset ──────  place::follow
transport::complete ◄── final record ───────────  place::print_record
  │   artifacts::fetch, verify per file
  ▼
receipt::save ── <repo>/.ferry/receipts/<id>.json (submitted → final)
```

A dropped caller changes nothing on the place: the supervisor runs to the
final record, and `ferry record <id>` completes the receipt later.

## Modules

| Module | Owns | SPEC |
|---|---|---|
| `main` | The CLI; builds a caller's receipt; maps any error to 125. | §4 |
| `config` | `ferry.toml`, `machines.toml`, catalogs; routing; path and env rules. | §10 |
| `repository` | Exact commits from git or jj; committed declarations; git helpers. | §2, §10 |
| `transport` | The caller's side: ssh options, ship, submit, follow, stop, record, retire. | §6 |
| `place` | The place's verbs and the supervisor; the place record (`HostResult`). | §5, §6 |
| `execution` | The request; prepare (locks, admission, checkout reset, ship); debug fault points. | §5, §9 |
| `run` | The session: spawn with the seat lock inherited, deadline loop, output copier, verdict. | §3, §5 |
| `custody` | What a session stop and the marker pass left alive, as recorded evidence; the one `unsafe` call (macOS `KERN_PROCARGS2`). | §7 |
| `lock` | Seat lock (inherited by the work) and host lease (shared or exclusive, the supervisor's) as flock files. | §3, §5 |
| `record` | Reserve, finalize once, reconcile a lost supervisor; busy-holder naming. | §8 |
| `receipt` | The caller's receipt, digests, run ids, capabilities. | §8 |
| `admission` | Host policy: disk floor, per-seat cap, and every delete in a seat (contained, same device). | §9 |
| `names` | Version 2's plain config names: a version 2 file renamed to the version 1 names the structs read, sizes and durations converted, version 1 keys in it refused naming the new ones. Deleted with version 1. | §10 |
| `history` | The host's run history bound: after a run's record is final, delete the runs, retirement records and accounts older than `delete_run_history_after_days`. | §9 |
| `seats` | The caller's seat book, `ferry seats prune [--idle]`, and the hourly prune a run makes by itself. | §9 |
| `status` | `ferry status`: one read-only view of a place (disk, live runs, the gate, seats, leftovers). | §4 |
| `artifacts` | Collect and hash at the place; fetch and verify at the caller. | §8 |
| `limits` | Every timing and size bound, shared with the tests. | §5 |

## Rules the code keeps

- **One record per run.** Only `record::finalize` writes a final record, and
  only once; a lost supervisor is reconciled to `lost` exactly once.
- **The verdict is the first terminal fact** (`run::verdict`): a later fact
  is a note, never a new verdict.
- **The seat lock lives as long as the work.** The session inherits it; the
  host lease stays with the supervisor, so a leftover holds no more than its
  seat. Release is `close()`, after the final record.
- **Refuse before effect.** A refusal (125) happens before the recipe runs;
  the only deletes before that are the seat cap's, recorded as `pruned`. A
  seat's contents are deleted only by `admission::remove`, which re-proves
  containment and never crosses a device.
- **Records parse across versions.** Fields a place writes in a shape that
  may change (`custody`, `admission`, `pruned`, `artifacts`) are kept as the
  JSON it wrote, and read where they are used.
- **Every fallible point has a fault point** (`execution::fail_at`, debug
  builds only) and a control that injects it.
