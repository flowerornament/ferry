# Ferry design principles

These decide design questions before anyone has to argue them case by case.
Every freeze says which principles it touches (AGENTS.md, Design gate).
Amending a principle means amending this file in its own reviewed change.

1. **Ownership follows lifetime.** A process belongs to whatever shares its
   lifetime. Anything a run starts that should end with the run is the run's.
   Anything meant to outlive a run belongs to an owner with that longer
   lifetime, never to the run that happened to start it.

2. **Ferry owns nothing longer than a run.** Ferry is not a service manager:
   no daemons, no keep-alives, no lifecycles beyond an attempt. What persists
   between runs is files (seats, caches, records), repositories, or host
   services owned by the operator.

3. **Each layer owns what only it knows** (the end-to-end argument):
   - the project decides what runs and how (recipes, hooks, gates, rebase and
     conflict law);
   - the operator decides host policy, host services and installs;
   - Ferry decides exclusion, custody and records.

4. **Host acts belong to the operator.** Ferry never installs services or
   changes host configuration. When something it needs is missing, it refuses
   and names the next step.

5. **Safety from mechanism, liveness bounded.** Exclusion rests on kernel
   primitives (flock, process lineage), never on other processes behaving
   well. Every wait and every hold is bounded or interruptible.

6. **Unknown is not empty.** When Ferry cannot prove something (that
   processes are gone, that a seat is idle, that a host is current), it keeps
   holding or refuses, and says so. It never infers absence.

7. **Explicit beats inferred.** No behaviour keyed on process names, paths or
   heuristics. Exceptions are declared in a contract (the project contract or
   the host policy) and recorded.

8. **One honest record per attempt.** Every attempt ends in exactly one
   record. The record says what happened, including what Ferry did to other
   processes and what it could not prove.

9. **Portable meaning, honest strength.** Every concept means the same on
   macOS and Linux. Where a guarantee is weaker on one platform, records and
   capabilities say so.

10. **A small surface.** Few concepts and few options; most users are agents.
    A new user-facing concept must replace one or justify itself in the core
    model (`docs/specs/2026-10-08-core-model.md`).

## Worked example: a shared service started by a run

Herald's machine-level development Postgres is shared by every seat on a host,
but its Linux adapter started it inside whichever run first found it down.
Ferry's end-of-run survivor stopping (ferry-ljc.22) would have stopped it.

- **Principles 1 and 2:** it outlives runs, so no run owns it and Ferry cannot
  keep it alive. That rules out a keep-alive declaration.
- **Principles 3 and 4:** its owner is the host's service manager (launchd on
  macOS, systemd on Linux), and setting that up is the operator's act.
- **Principle 7:** Ferry's survivor policy gets no exemption keyed on its
  name or path.
- **Principles 4 and 6, until the service exists:** the project's prepare
  refuses with a next step (start the service) instead of starting it inside
  a run.
