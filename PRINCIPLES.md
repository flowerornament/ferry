# Ferry design principles

Ferry runs a repository's recipe at an exact commit on a machine that can run
it, serializes what must not overlap, and records exactly what happened.
These principles keep it that small. Every freeze names the ones it touches
(AGENTS.md, Design gate). Changing one is its own reviewed change to this
file.

## Principles

1. **Ownership follows lifetime.** A process belongs to whatever shares its
   lifetime. A run owns only what should end with it.
2. **Ferry owns nothing longer than a run.** No daemons, keep-alives or
   lifecycles beyond an attempt. What persists is files (seats, caches,
   records), repositories, or the operator's host services.
3. **Each layer owns what only it knows.**
   - The project decides what runs and how.
   - The operator decides host policy, services and installs.
   - Ferry decides exclusion, custody and records.
4. **Host acts belong to the operator.** Ferry never installs services or
   changes host configuration. When something is missing, it refuses and
   names the next step.
5. **Safety from mechanism, liveness bounded.** Exclusion rests on kernel
   primitives, never on other processes behaving well. Every wait and hold is
   bounded or interruptible.
6. **Unknown is not empty.** What Ferry cannot prove, it keeps holding or
   refuses, and says so.
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
- **a service manager:** use launchd or systemd (operator);
- **a CI system or gate runner:** repositories own their gates and land laws;
- **a cluster or fleet manager:** machines are declared by the user;
  provisioning is the operator's;
- **a sandbox or security boundary:** runs execute as the user;
- **a deployment or publication tool:** it never pushes to forges;
- **a general cache manager:** only regenerable seat targets, under host
  policy;
- **a quiet-host certifier:** exclusive means "no other Ferry run here", plus
  named observations.

## Worked example

Herald's shared development Postgres outlived the run that started it, and
end-of-run survivor stopping would have stopped it.
- **1, 2:** no run owns it, and Ferry cannot keep it alive.
- **3, 4:** it belongs to the host's service manager, set up by the operator.
- **7:** no exemption keyed on its name.
- **4, 6, until then:** the project's prepare refuses with a next step instead
  of starting it.
