#!/usr/bin/env python3
"""Fail if a vendored dylib leaves a symbol for runtime lookup that nothing provides.

mpv links with `-undefined dynamic_lookup`, so a function it references but
never builds (or a framework it forgets to link) is not a link error: it
resolves to null and crashes on first call. This loads the vendored
libraries and the system frameworks they use, then resolves every
"dynamically looked up" symbol the same way dyld would.

Usage: check-dynamic-symbols.py <prefix>/lib
"""

import ctypes
import subprocess
import sys
from pathlib import Path

FRAMEWORKS = [
    "AppKit", "AudioToolbox", "AVFoundation", "Cocoa", "CoreAudio", "CoreFoundation",
    "CoreMedia", "CoreText", "CoreVideo", "Foundation", "IOKit", "IOSurface", "Metal",
    "OpenGL", "QuartzCore", "Security", "VideoToolbox",
]


def main():
    lib_dir = Path(sys.argv[1])
    for name in FRAMEWORKS:
        ctypes.CDLL(f"/System/Library/Frameworks/{name}.framework/{name}", mode=ctypes.RTLD_GLOBAL)
    for name in ("libobjc.A.dylib", "libiconv.2.dylib", "libz.1.dylib", "libbz2.1.0.dylib"):
        ctypes.CDLL(f"/usr/lib/{name}", mode=ctypes.RTLD_GLOBAL)
    dylibs = sorted(p for p in lib_dir.glob("*.dylib") if not p.is_symlink())
    missing = []
    for path in dylibs:
        try:
            ctypes.CDLL(str(path), mode=ctypes.RTLD_GLOBAL)
        except OSError as error:
            missing.append(f"{path.name}: {error}")
    if missing:
        for entry in missing:
            print(f"unresolved: {entry}", file=sys.stderr)
        return 1

    process = ctypes.CDLL(None)
    for path in dylibs:
        listing = subprocess.run(["nm", "-m", str(path)], check=True, capture_output=True, text=True)
        for line in listing.stdout.splitlines():
            if "(dynamically looked up)" not in line:
                continue
            symbol = line.split()[2]
            if not hasattr(process, symbol[1:]):
                missing.append(f"{path.name}: {symbol}")
    for entry in missing:
        print(f"unresolved: {entry}", file=sys.stderr)
    return 1 if missing else 0


if __name__ == "__main__":
    sys.exit(main())
