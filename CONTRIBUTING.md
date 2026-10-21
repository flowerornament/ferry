# Contributing to Ferry

Thanks for your interest. Ferry is small on purpose: it does one thing (run
a repository's recipe at an exact commit, on a machine that can run it, and
record exactly what happened) and leaves scheduling, retries, containment
and disk policy to the projects and operators around it. Read
[`docs/PRINCIPLES.md`](docs/PRINCIPLES.md) before proposing a new mechanism; a change
that deletes code or makes an existing record more honest is the easiest
kind to accept.

## Reporting a problem

Open a GitHub issue with:

- the command you ran and the `ferry --version` on each end (the receipt
  names both binaries: `caller_source_rev`, `host_source_rev`);
- the receipt (`ferry record <id>`), with anything
  private removed;
- what you expected, and what happened instead.

A refusal (exit 125) prints what, why and the next step; please include it.

## Making a change

1. Enter the dev shell: `nix develop` (toolchain, jj, git, python3, and
   procps and util-linux on Linux).
2. Make the change, with a test that fails without it. Behaviour changes
   need a real-CLI control in `tests/`, not only a unit test; a new fallible
   point needs a `FERRY_FAIL_AT` injection and a control for it.
3. Update the living documents in the same change: `README.md` or
   `docs/REFERENCE.md` for anything a user sees, `docs/SPEC.md` if the target
   changes (amend it first), and `docs/ARCHITECTURE.md` if a module's job
   changes.
4. Run `just check`. Code that is Linux-only is never compiled on a Mac, so
   run the check on Linux too if you touch it.
5. Open a pull request that says what the change deletes or adds, which
   `docs/PRINCIPLES.md` points it touches, and which checks you ran where.

[`AGENTS.md`](AGENTS.md) is the full working agreement that the maintainers
and their coding agents follow (jj workspaces, the design gate, reviews by
execution). You don't need jj to send a pull request.

## License

By contributing, you agree that your contribution is dual licensed under the
[MIT license](LICENSE-MIT) and the [Apache License, Version
2.0](LICENSE-APACHE), as described in the [README](README.md#license).
