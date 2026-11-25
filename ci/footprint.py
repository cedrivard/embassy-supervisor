#!/usr/bin/env python3
"""Measure what each firmware feature costs in flash and RAM.

Builds the demo firmware without default features, once per opt-in feature,
with its default features and with `dns` on top, then prints each build's
growth over its reference build as a Markdown table: flash (`.vector_table` +
`.text` + `.rodata`), `.data` and `.bss`.

`--check` skips the `dns` build and exits 1 when a build grows past its entry
in `BUDGET`. The budgets carry a margin: CI builds with the current stable
toolchain, and sizes move between releases.

Environment: `CARGO` overrides the cargo command (for example
`CARGO="cargo +1.97.1"`). `CARGO_TARGET_DIR` is honoured.
"""

import os
import shlex
import struct
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TARGET_DIR = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
ELF = TARGET_DIR / "thumbv8m.main-none-eabihf" / "release" / "firmware"
CARGO = shlex.split(os.environ.get("CARGO", "cargo"))
FEATURES = [
    "lifecycle-sink",
    "breadcrumb",
    "feed-gate",
    "restart-intensity",
    "enabled-if",
    "task-priority",
]
FLASH = (".vector_table", ".text", ".rodata")
# Ceilings in bytes over the baseline: (flash, `.data` + `.bss`).
BUDGET = {
    "lifecycle-sink": (2688, 320),
    "breadcrumb": (2304, 32),
    "feed-gate": (256, 32),
    "restart-intensity": (640, 112),
    "enabled-if": (1920, 64),
    "task-priority": (2816, 192),
    "default": (10496, 720),
}


def sections(elf: Path) -> dict[str, int]:
    """Section sizes of a little-endian ELF32 file, by name."""
    data = elf.read_bytes()
    if data[:6] != b"\x7fELF\x01\x01":
        raise SystemExit(f"{elf}: not a little-endian ELF32")
    (shoff,) = struct.unpack_from("<I", data, 0x20)
    shentsize, shnum, shstrndx = struct.unpack_from("<HHH", data, 0x2E)

    def header(i: int) -> tuple[int, ...]:
        return struct.unpack_from("<10I", data, shoff + i * shentsize)

    names = header(shstrndx)[4]
    sizes = {}
    for i in range(shnum):
        h = header(i)
        start = names + h[0]
        sizes[data[start : data.index(b"\0", start)].decode()] = h[5]
    return sizes


def build(*args: str) -> tuple[int, int, int]:
    """Build the firmware and return its (flash, .data, .bss) sizes."""
    cmd = CARGO + ["build", "--release", "-p", "firmware", *args]
    print("$", shlex.join(cmd), file=sys.stderr, flush=True)
    subprocess.run(cmd, cwd=ROOT, check=True)
    s = sections(ELF)
    return sum(s[k] for k in FLASH), s[".data"], s[".bss"]


def signed(n: int) -> str:
    return "0" if n == 0 else f"{n:+,}"


def main() -> int:
    check = sys.argv[1:] == ["--check"]
    if sys.argv[1:] and not check:
        print("usage: footprint.py [--check]", file=sys.stderr)
        return 2
    base = build("--no-default-features")
    rows = []
    for f in FEATURES:
        size = build("--no-default-features", "--features", f)
        rows.append((f"`{f}`", f, size, base))
    default = build()
    rows.append(("all six (default)", "default", default, base))
    if not check:
        rows.append(("`dns` (over default)", None, build("--features", "dns"), default))

    print(f"baseline: flash {base[0]:,} B, .data {base[1]:,} B, .bss {base[2]:,} B")
    print("| feature | Δflash | Δ.data | Δ.bss |")
    print("| --- | ---: | ---: | ---: |")
    over = []
    for label, key, size, ref in rows:
        flash, data, bss = (a - b for a, b in zip(size, ref))
        print(f"| {label} | {signed(flash)} | {signed(data)} | {signed(bss)} |")
        if check and key is not None:
            flash_max, ram_max = BUDGET[key]
            if flash > flash_max:
                over.append(f"{key}: flash {signed(flash)} B is over its {flash_max:,} B budget")
            if data + bss > ram_max:
                over.append(f"{key}: RAM {signed(data + bss)} B is over its {ram_max:,} B budget")
    for line in over:
        print(line, file=sys.stderr)
    return 1 if over else 0


if __name__ == "__main__":
    sys.exit(main())
