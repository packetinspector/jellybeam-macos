#!/usr/bin/env python3
"""Redact machine paths in native libraries' diagnostic configuration macros."""

from pathlib import Path
import os
import sys

path = Path(sys.argv[1])
macro = sys.argv[2]
root = sys.argv[3]
prefix = f"#define {macro} "
lines = path.read_text().splitlines(keepends=True)
found = False
for index, line in enumerate(lines):
    if line.startswith(prefix):
        lines[index] = line.replace(root, "/jellybeam").replace(os.path.expanduser("~"), "/home")
        found = True
if not found:
    raise SystemExit(f"missing diagnostic configuration macro: {macro}")
path.write_text("".join(lines))
