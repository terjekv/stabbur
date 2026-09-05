#!/usr/bin/env python3
"""Require Docker's dependency-cache manifests to match Cargo workspace members."""

from __future__ import annotations

import re
import tomllib
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
MANIFEST = ROOT / "Cargo.toml"
DOCKERFILE = ROOT / "Dockerfile"


def main() -> None:
    cargo = tomllib.loads(MANIFEST.read_text(encoding="utf-8"))
    expected = {
        member.rstrip("/") + "/Cargo.toml"
        for member in cargo["workspace"]["members"]
        if member != "."
    }
    dockerfile = DOCKERFILE.read_text(encoding="utf-8")
    copied = set(
        re.findall(
            r"^COPY (crates/[^ ]+/Cargo\.toml) ", dockerfile, flags=re.MULTILINE
        )
    )
    if copied != expected:
        missing = sorted(expected - copied)
        extra = sorted(copied - expected)
        raise SystemExit(
            f"Docker workspace manifest copies differ: missing={missing}, extra={extra}"
        )
    print(f"Dockerfile copies all {len(expected)} workspace crate manifests")


if __name__ == "__main__":
    main()
