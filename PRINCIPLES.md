# Ferry design principles

Ferry does one thing: it runs a repository's recipe, at an exact commit (even
an unpushed one), on a machine you own, in a warm checkout, alone if you ask,
and gives you an honest record. It serializes only what must not overlap.
These principles keep it that small. Every freeze names the ones it touches
(AGENTS.md, Design gate). Changing one is its own reviewed change to this
file.

## Principles

1. **Ownership follows lifetime, for processes and files.** A run owns only
   what should end with it. Every file Ferry creates has one declared
   lifetime and lives under Ferry's roots:
   - **run** (the run's directory: its request, output and result files):
     kept with its record, never reused by another run;
   - **cache** (seat checkouts, `seats/<seat>/cache`): regenerable. Ferry
     bounds what it created: a seat is retired when its caller's workspace
     is gone, and a seat's caches are wiped at the start of a run, under its
     lock, when they exceed the place's cap. Anything beyond that is host
     policy;
   - **record** (receipts, host records, run output): kept until the
     operator's retention says otherwise.

   Ferry writes nowhere else. Anything else is its creator's to clean up.
2. **Ferry owns nothing longer than a run.** No daemons, keep-alives or
   lifecycles beyond an attempt. What persists is files (seats, caches,
   records), repositories, or the operator's host services. Every run is
   bounded by its deadline. A dropped connection never ends a run; its
   deadline or an explicit stop does.
3. **Each layer owns what only it knows.**
   - The project decides what runs and how.
   - The operator decides host policy, services and installs.
   - Ferry decides exclusion, custody and records.
4. **Host acts belong to the operator.** Ferry never installs services or
   changes host configuration. When something is missing, it refuses and
   names the next step.
5. **Safety from mechanism, liveness bounded.** Exclusion rests on kernel
   primitives, never on other processes behaving well. Each guarantee rests
   on one kernel mechanism or one file. Every wait and hold is bounded or
   interruptible.
6. **Report what you cannot prove; refuse only by mechanism.** A held kernel
   lock is the only "busy"; anything that may have survived a run is named
   in its record, not chased. Ferry does not grow machinery to prove
   absence. Stronger containment comes from the operating system (for
   example a systemd scope), reported as a tier.
7. **Explicit beats inferred.** No behaviour keyed on names, paths or
   heuristics. Exceptions are declared in a contract and recorded.
8. **One honest record per attempt,** including what Ferry did to other
   processes and what it could not prove.
9. **Portable meaning, honest strength.** The same concepts on macOS and
   Linux; any weaker guarantee is reported, not hidden.
10. **A small surface.** A new concept or option must replace one or justify
    itself in the core model. Most users are agents.

## What Ferry is not

Requests in this list go to the layer that owns them, not into Ferry:

- **a scheduler or queue service:** locks in the tool doing the work, nothing
  more;
- **a lock service:** repositories own their land and coordination locks
  (flock or lockf; `scripts/jj-land.sh` is the worked example);
- **a service manager:** use launchd or systemd (operator);
- **a CI system or gate runner:** repositories own their gates and land laws;
  queues, schedules, retries, fan-out, dashboards and artifact stores belong
  to repositories or real CI tools;
- **a process supervisor or containment tool:** a run's session is stopped at
  its end; anything stronger is the operating system's;
- **a cluster or fleet manager:** machines are declared by the user;
  provisioning is the operator's;
- **a sandbox or security boundary:** runs execute as the user;
- **a deployment or publication tool:** it never pushes to forges;
- **a cache manager:** Ferry bounds only its own state (seats retired with
  their caller's workspace; a per-seat cap enforced under the seat lock). It
  never shares, ranks or evicts caches across seats; anything cleverer is
  the operator's. Why: no Ferry state may be left to clean by hand, and only
  Ferry holds the seat lock and only the caller knows whether its workspace
  still exists;
- **a quiet-host certifier:** exclusive means "no other Ferry run here", plus
  named observations.

## Worked example

A project's shared development Postgres outlived the run that started it, and
end-of-run survivor stopping would have stopped it.
- **1, 2:** no run owns it, and Ferry cannot keep it alive.
- **3, 4:** it belongs to the host's service manager, set up by the operator.
- **7:** no exemption keyed on its name.
- **4, 6, until then:** the project's prepare refuses with a next step instead
  of starting it.
