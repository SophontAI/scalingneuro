#!/usr/bin/env python3
from __future__ import annotations

import hashlib
from pathlib import Path
import sys


def main() -> int:
    if len(sys.argv) != 3:
        return 2
    requirements = Path(sys.argv[1])
    package = Path(sys.argv[2])
    files = sorted(path for path in package.rglob("*.py") if path.is_file())
    if not requirements.is_file() or requirements.is_symlink() or not files:
        return 2
    if any(path.is_symlink() for path in files):
        return 2
    entries = [("requirements.lock", requirements)]
    entries.extend(
        (f"scaling_neuro_processor/{path.relative_to(package).as_posix()}", path)
        for path in files
    )
    digest = hashlib.sha256()
    for logical_name, path in entries:
        name = logical_name.encode("utf-8")
        raw = path.read_bytes()
        digest.update(len(name).to_bytes(4, "big"))
        digest.update(name)
        digest.update(len(raw).to_bytes(8, "big"))
        digest.update(raw)
    print(digest.hexdigest())
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
