"""Enforce that a decisions.yaml never changes once it exists at a base revision.

Implements 0001-record-architecture-decisions/decisions-immutable: every
``adrs/<folder>/decisions.yaml`` present at ``base`` must still exist,
byte-for-byte identical, in the working tree.
"""

from __future__ import annotations

import subprocess
from pathlib import Path

from .model import DECISIONS_FILE


class GitError(Exception):
    pass


def _git_bytes(repo: Path, *args: str) -> bytes:
    try:
        return subprocess.run(
            ["git", "-C", str(repo), *args], check=True, capture_output=True
        ).stdout
    except subprocess.CalledProcessError as e:
        raise GitError(e.stderr.decode(errors="replace").strip()) from e


def find_violations(repo: Path, adrs_dir: Path, base: str) -> list[str]:
    """Human-readable violations; empty when every frozen file is untouched."""
    repo = repo.resolve()
    adrs_rel = adrs_dir.resolve().relative_to(repo).as_posix()
    _git_bytes(repo, "rev-parse", "--verify", f"{base}^{{commit}}")  # fail fast on bad base
    try:
        listing = _git_bytes(repo, "ls-tree", "-r", "-z", f"{base}:{adrs_rel}")
    except GitError:
        return []  # adrs/ did not exist at base: nothing is frozen yet

    violations: list[str] = []
    for record in filter(None, listing.split(b"\0")):
        meta, _, path = record.decode().partition("\t")
        _mode, kind, blob = meta.split()
        parts = path.split("/")
        if kind != "blob" or len(parts) != 2 or parts[1] != DECISIONS_FILE:
            continue
        if parts[0].startswith(("_", ".")):
            continue
        frozen = _git_bytes(repo, "cat-file", "blob", blob)
        current = adrs_dir / path
        if not current.is_file():
            violations.append(f"{adrs_rel}/{path}: deleted or moved (frozen since {base[:10]})")
        elif current.read_bytes() != frozen:
            violations.append(f"{adrs_rel}/{path}: modified (frozen since {base[:10]})")
    return violations
