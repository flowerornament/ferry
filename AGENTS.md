# Ferry

Run a recipe at an exact commit on a machine that has the required features.

This repository is local-only until the user chooses a forge remote. Never
publish it. Use jj; do not mutate Herald or Murail from this checkout.
Current extraction is tracked on Herald's herald-4e6mw bead.

## Check

`just check` runs Python contracts and Bash syntax checks. Changes to transport
require real execution controls for exact commits, refusal versus recipe exit,
slot isolation, and session cleanup on client loss.

## Structure

`ferry` is the CLI. `libexec/` retains the reviewed client/host transport from
Murail 8ba36cfa. Project-specific environment and artifact handling are hooks,
not branches in Ferry. Recipe requirements have one home in the project catalog.

Keep receipts honest: focused controls are not project gates, and remote
evidence does not certify Herald's local `just land`.
