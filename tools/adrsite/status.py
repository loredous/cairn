"""Derive ADR status from git: an ADR is accepted once it is on main.

Implements 0001-record-architecture-decisions/status-derived-from-main. "Main"
is the first of these that exists: an explicit ref, $CAIRN_MAIN_REF,
``origin/main`` (the repo on GitHub), then the local ``main`` branch. With no
main ref at all (a brand-new repo) nothing has been merged, so every ADR is
proposed.
"""

from __future__ import annotations

import os
import subprocess
from pathlib import Path
from typing import Iterable

from .model import ADR_DIR_RE

DEFAULT_MAIN_REFS = ("origin/main", "main")


def _git(repo: Path, *args: str) -> str | None:
    try:
        return subprocess.run(["git", "-C", str(repo), *args], check=True,
                              capture_output=True, text=True).stdout
    except (subprocess.CalledProcessError, FileNotFoundError):
        return None


def adr_ids_in(files: Iterable[str]) -> set[str]:
    """ADR folder names in a read_tree() mapping (skips _template and the like)."""
    return {rel.split("/", 1)[0] for rel in files if ADR_DIR_RE.match(rel.split("/", 1)[0])}


def resolve_main_ref(repo: Path, explicit: str | None = None) -> str | None:
    candidates = [explicit] if explicit else [os.environ.get("CAIRN_MAIN_REF"), *DEFAULT_MAIN_REFS]
    for ref in filter(None, candidates):
        if _git(repo, "rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}") is not None:
            return ref
    return None


def merged_adr_ids(repo: Path, adrs_dir: Path, ref: str) -> set[str]:
    """ADR folders present (with a decisions.yaml) in ``ref``'s tree."""
    adrs_rel = adrs_dir.resolve().relative_to(repo.resolve()).as_posix()
    listing = _git(repo, "ls-tree", "-r", "-z", "--name-only", ref, "--", f"{adrs_rel}/")
    if not listing:
        return set()
    ids = set()
    for path in filter(None, listing.split("\0")):
        parts = Path(path).relative_to(adrs_rel).parts
        if len(parts) == 2 and parts[1] == "decisions.yaml" and ADR_DIR_RE.match(parts[0]):
            ids.add(parts[0])
    return ids


def proposed_adr_ids(repo: Path, adrs_dir: Path, adr_ids: Iterable[str],
                     main_ref: str | None = None) -> set[str]:
    """The subset of ``adr_ids`` not yet merged into main."""
    ref = resolve_main_ref(repo, main_ref)
    merged = merged_adr_ids(repo, adrs_dir, ref) if ref else set()
    return set(adr_ids) - merged
