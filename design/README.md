# Design records

Dated design documents, kept as written. Each records a decision at the time
it was made: the problem, the options, the reviews, and the evidence. They
are amended or superseded by a later dated document, never rewritten to match
the code, so an older one can describe behaviour Ferry no longer has.

For how Ferry works today, read [`SPEC.md`](../SPEC.md) (the normative
target), [`README.md`](../README.md) (what ships) and
[`ARCHITECTURE.md`](../ARCHITECTURE.md) (where it lives in the code).
[`PRINCIPLES.md`](../PRINCIPLES.md) decides design questions.

**Reading notes.** These were written while Ferry served two consumer
projects, Herald and Murail, on a shared Linux build host named `bench`, and
were reviewed by several coding agents. Names like `ferry-1a`, `murail-4a`
or `herald-1b` are those agents' seats; ids like `ferry-xtx.8` are issues in
the project's [beads](https://github.com/steveyegge/beads) tracker; "M1" is
the milestone that moved Ferry to detached runs and the `v1` state root.

| Date | Document | Status |
|---|---|---|
| 2026-10-08 | [Coordination without people, and a usable surface](2026-10-08-coordination-and-dx.md) | Requirements and evidence; the plan parts are superseded |
| 2026-10-08 | [Core model v2, and the roadmap](2026-10-08-core-model-v2.md) | Superseded by v3; kept for its review history |
| 2026-10-08 | [Core model v3: a run is a bounded, detached job](2026-10-08-core-model-v3.md) | Superseded in part by *Ferry minimal* |
| 2026-10-09 | [Ferry minimal: one thing, done well](2026-10-09-ferry-minimal.md) | The current plan and rationale |
| 2026-10-10 | [Detach and output to a file](2026-10-10-detach-and-output-file.md) | Built (M1 slices 2 and 3) |
| 2026-10-11 | [Leases minimal](2026-10-11-leases-minimal.md) | Built (M1 slice 4a); the lease inheritance changed later (SPEC §3) |
| 2026-10-11 | [Custody is the session stop](2026-10-11-custody-session.md) | Built (M1 slice 4b); extended by the marker pass (SPEC §7) |
| 2026-10-11 | [Bounds on seats](2026-10-11-seat-bounds.md) | Built (M1 slice 5) |
| 2026-10-11 | [One ship, one closed root](2026-10-11-ship-and-migration.md) | Built (M1 slices 6 and 7) |
| 2026-10-12 | [The M1 joint window](2026-10-12-m1-joint-window.md) | The install runbook, as used |
