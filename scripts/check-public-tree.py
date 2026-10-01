#!/usr/bin/env python3
"""Check public source files without printing potentially sensitive matches."""

import ipaddress
from pathlib import Path
import re
import subprocess
import sys
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parent.parent


def public_files():
    if (ROOT / ".git").exists():
        result = subprocess.run(
            ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"],
            cwd=ROOT, check=True, capture_output=True,
        )
        return sorted(set(result.stdout.decode().split("\0")) - {""})
    return sorted(str(p.relative_to(ROOT)) for p in ROOT.rglob("*") if p.is_file())


def main():
    files = public_files()
    problems = []
    patterns = {
        "local user path": re.compile(r"/(?:Users/(?!ForgotPassword/)|home/)[A-Za-z][A-Za-z0-9_.-]*/"),
        "private key": re.compile(r"-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----"),
        "provider token": re.compile(
            r"\b(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{60,}|AKIA[A-Z0-9]{16})\b"
        ),
    }
    private_ranges = [ipaddress.ip_network(n) for n in ("10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16")]
    for name in files:
        path = ROOT / name
        if not path.exists():
            continue
        if path.is_symlink():
            problems.append((name, 0, "symlink needs review"))
            continue
        if (name.startswith(("internal/", "target/", "vendor/", "dev/media/", "dev/volumes/", ".claude/"))
                or path.name in ("CLAUDE.local.md", "signing.env", ".env")
                or path.suffix.lower() in (".p12", ".pfx", ".key")):
            problems.append((name, 0, "local-only file in public tree"))
        data = path.read_bytes()
        if b"\0" in data:
            continue
        text = data.decode("utf-8", errors="replace")
        for kind, pattern in patterns.items():
            for match in pattern.finditer(text):
                problems.append((name, text.count("\n", 0, match.start()) + 1, kind))
        # The scanner's own range definitions are rules, not example addresses.
        if name != "scripts/check-public-tree.py":
            for match in re.finditer(r"\b(?:\d{1,3}\.){3}\d{1,3}\b", text):
                try:
                    address = ipaddress.ip_address(match.group())
                except ValueError:
                    continue
                if any(address in network for network in private_ranges):
                    problems.append((name, text.count("\n", 0, match.start()) + 1, "private IP; use RFC 5737 in examples"))
        if path.suffix == ".md":
            links = list(re.finditer(r"\]\(([^)\s]+)(?:\s+[^)]*)?\)", text))
            links += list(re.finditer(r'(?:src|href)="([^"]+)"', text))
            for match in links:
                target = unquote(match.group(1).strip("<>"))
                parsed = urlsplit(target)
                if parsed.scheme or parsed.netloc or not parsed.path:
                    continue
                resolved = (path.parent / parsed.path).resolve()
                if not resolved.is_relative_to(ROOT) or not resolved.exists():
                    problems.append((name, text.count("\n", 0, match.start()) + 1, "broken local document link"))
    if (ROOT / "AGENTS.md").read_bytes() != (ROOT / "CLAUDE.md").read_bytes():
        problems.append(("CLAUDE.md", 0, "must match AGENTS.md"))
    for name, line, kind in problems:
        print(f"{name}:{line}: {kind}", file=sys.stderr)
    print(f"Public tree: {len(files)} files checked, {len(problems)} problems.")
    return bool(problems)


if __name__ == "__main__":
    sys.exit(main())
