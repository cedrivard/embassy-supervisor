#!/usr/bin/env python3
"""Run every embassy-supervisor integration test with exactly its features.

Reads the `[[test]]` entries of `supervisor/Cargo.toml`, groups them by their
sorted `required-features`, and runs one `cargo test` per group with
`--no-default-features --features <list>` (a test without `required-features`
runs with `--no-default-features` alone). Stops at the first failing group.

Environment: `CARGO` overrides the cargo command (for example
`CARGO="cargo +1.97"`), `HOST` the host target (default
`x86_64-unknown-linux-gnu`).
"""

import os
import shlex
import subprocess
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def main() -> int:
    with open(ROOT / "supervisor" / "Cargo.toml", "rb") as f:
        manifest = tomllib.load(f)
    groups: dict[tuple[str, ...], list[str]] = {}
    for test in manifest.get("test", []):
        key = tuple(sorted(test.get("required-features", [])))
        groups.setdefault(key, []).append(test["name"])

    cargo = shlex.split(os.environ.get("CARGO", "cargo"))
    host = os.environ.get("HOST", "x86_64-unknown-linux-gnu")
    for features, names in sorted(groups.items()):
        cmd = cargo + [
            "test",
            "-p",
            "embassy-supervisor",
            "--target",
            host,
            "--no-default-features",
        ]
        if features:
            cmd += ["--features", ",".join(features)]
        for name in names:
            cmd += ["--test", name]
        print(f"::group::features=[{','.join(features) or 'none'}]", flush=True)
        print(" ".join(cmd), flush=True)
        status = subprocess.run(cmd, cwd=ROOT).returncode
        print("::endgroup::", flush=True)
        if status != 0:
            print(f"failed: features=[{','.join(features)}] tests={names}", file=sys.stderr)
            return status
    return 0


if __name__ == "__main__":
    sys.exit(main())
