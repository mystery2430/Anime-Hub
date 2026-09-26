#!/usr/bin/env python3
"""Relax the release profile in Cargo.toml for a low-RAM build.

Called by `scripts/build.sh` when `TAURI_LOW_MEMORY=1`. Only the two settings
that drive peak memory during linking are touched; the original file is
restored by the shell script's trap, so this edit never needs to be committed.
"""

from __future__ import annotations

import re
import sys


def relax(text: str) -> str:
    """Turn off LTO and widen codegen units inside `[profile.release]`."""
    out, in_release = [], False
    for line in text.splitlines(keepends=True):
        if re.match(r"^\[profile\.release\]\s*$", line):
            in_release = True
            out.append(line)
            continue
        if in_release and re.match(r"^\[", line):
            in_release = False
        if in_release:
            if re.match(r"^lto = true\s*$", line):
                out.append("lto = false\n")
                continue
            if re.match(r"^codegen-units = 1\s*$", line):
                out.append("codegen-units = 16\n")
                continue
        out.append(line)
    return "".join(out)


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: _relax_profile.py <path/to/Cargo.toml>", file=sys.stderr)
        return 2
    path = sys.argv[1]
    with open(path, encoding="utf-8") as fh:
        original = fh.read()
    updated = relax(original)
    if updated == original:
        print("warning: no release profile settings were changed", file=sys.stderr)
    with open(path, "w", encoding="utf-8") as fh:
        fh.write(updated)
    return 0


if __name__ == "__main__":
    sys.exit(main())
