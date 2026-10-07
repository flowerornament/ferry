"""Execute project declarations inside the transport's owned session."""

import json
import os
import subprocess
import sys
from pathlib import Path


def hook(name, spec, env):
    path = spec.get("hooks", {}).get(name)
    if not path:
        return None
    relative = Path(path)
    if relative.is_absolute() or ".." in relative.parts:
        raise ValueError(f"hook {name} must name a checkout-relative executable")
    return subprocess.run(
        [str(Path.cwd() / relative)],
        env=env,
        check=True,
        capture_output=name == "env",
        text=True,
    )


def main():
    spec = json.loads(os.environ["FERRY_SPEC"])
    env = os.environ.copy()
    env["FERRY_WORKDIR"] = str(Path.cwd())
    hook("prepare", spec, env)
    result = hook("env", spec, env)
    if result:
        overlay = json.loads(result.stdout)
        if not isinstance(overlay, dict) or not all(
            isinstance(k, str)
            and isinstance(v, str)
            and "\0" not in k + v
            and "=" not in k
            for k, v in overlay.items()
        ):
            raise ValueError("env hook must emit a JSON object of environment strings")
        env.update(overlay)
    command = spec["command"] + spec.get("arguments", [])
    if spec.get("runner", "path") == "devenv":
        command = ["devenv", "shell", "--", *command]
    status = subprocess.run(command, env=env, check=False).returncode
    env["FERRY_EXIT_CODE"] = str(status)
    hook("collect", spec, env)
    return status if status >= 0 else 128 - status


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print(f"ferry: hook refused: {error}", file=sys.stderr)
        sys.exit(125)
