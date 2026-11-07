"""Build a per-commit changelog of ADR changes from git history.

For every commit that touches ``adrs/``, the collection is loaded as it was at
that commit and at its first parent, and the two are diffed semantically
(decisions added, changed, superseded, rescinded, etc.) rather than textually.
"""

from __future__ import annotations

import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from .model import ADR, Collection, load_collection, read_tree


@dataclass
class Change:
    kind: str  # see CHANGE_KINDS
    adr: str
    subject: str | None = None  # decision/interpretation slug, when relevant
    detail: str | None = None
    fields: list[tuple[str, Any, Any]] = field(default_factory=list)  # (name, old, new)


CHANGE_KINDS = {
    "adr-added": "ADR added",
    "adr-removed": "ADR removed",
    "adr-title": "Title changed",
    "adr-prose": "Prose edited",
    "decision-added": "Decision added",
    "decision-removed": "Decision removed",
    "decision-changed": "Decision changed",
    "decision-supersedes": "Supersedes",
    "decision-rescinds": "Rescinds",
    "decision-rescinded": "Decision rescinded",
    "decision-reinstated": "Decision reinstated",
    "interpretation-added": "Interpretation added",
    "interpretation-removed": "Interpretation removed",
    "interpretation-changed": "Interpretation changed",
    "erratum-added": "Erratum added",
    "erratum-removed": "Erratum removed",
    "erratum-changed": "Erratum changed",
}


@dataclass
class Entry:
    sha: str | None  # None for uncommitted working-tree changes
    author: str
    date: str
    subject: str
    changes: list[Change]

    @property
    def short_sha(self) -> str:
        return self.sha[:10] if self.sha else "working tree"


# --------------------------------------------------------------------------- #
# Diffing
# --------------------------------------------------------------------------- #


def diff_collections(old: Collection, new: Collection) -> list[Change]:
    changes: list[Change] = []
    for adr_id in sorted(set(old.adrs) | set(new.adrs)):
        a, b = old.adrs.get(adr_id), new.adrs.get(adr_id)
        if a is None and b is not None:
            changes.append(Change("adr-added", adr_id, detail=b.title))
            changes.extend(_diff_adr(_empty_like(b), b, include_meta=False))
        elif b is None and a is not None:
            changes.append(Change("adr-removed", adr_id, detail=a.title))
        elif a is not None and b is not None:
            changes.extend(_diff_adr(a, b, include_meta=True))
    return changes


def _empty_like(adr: ADR) -> ADR:
    return ADR(adr.id, adr.number, adr.title, adr.date, [], "", [], [])


def _diff_adr(a: ADR, b: ADR, *, include_meta: bool) -> list[Change]:
    out: list[Change] = []
    if include_meta:
        if a.title != b.title:
            out.append(Change("adr-title", b.id, fields=[("title", a.title, b.title)]))
        if a.prose.strip() != b.prose.strip():
            out.append(Change("adr-prose", b.id))

    old_d = {d.slug: d for d in a.decisions}
    new_d = {d.slug: d for d in b.decisions}
    for slug in [d.slug for d in b.decisions] + sorted(set(old_d) - set(new_d)):
        od, nd = old_d.get(slug), new_d.get(slug)
        if od is None and nd is not None:
            if nd.is_rescission and nd.supersedes:
                out.append(Change("decision-rescinds", b.id, slug, detail=str(nd.supersedes)))
                continue
            out.append(Change("decision-added", b.id, slug, detail=f"{nd.strength}: {nd.title}"))
            if nd.supersedes:
                out.append(Change("decision-supersedes", b.id, slug, detail=str(nd.supersedes)))
        elif nd is None and od is not None:
            out.append(Change("decision-removed", b.id, slug, detail=od.title))
        elif od is not None and nd is not None:
            ov, nv = od.as_comparable(), nd.as_comparable()
            if not od.rescinded and nd.rescinded:
                out.append(Change("decision-rescinded", b.id, slug))
            elif od.rescinded and not nd.rescinded:
                out.append(Change("decision-reinstated", b.id, slug))
            if nd.supersedes and ov["supersedes"] != nv["supersedes"]:
                out.append(Change("decision-supersedes", b.id, slug, detail=str(nd.supersedes)))
            changed = [
                (k, ov[k], nv[k])
                for k in ov
                if ov[k] != nv[k] and k not in ("rescinded",)
            ]
            if changed:
                out.append(Change("decision-changed", b.id, slug, fields=changed))

    old_i = {i.slug: i for i in a.interpretations}
    new_i = {i.slug: i for i in b.interpretations}
    for slug in [i.slug for i in b.interpretations] + sorted(set(old_i) - set(new_i)):
        oi, ni = old_i.get(slug), new_i.get(slug)
        if oi is None and ni is not None:
            kind = "erratum" if ni.errata else "interpretation"
            out.append(Change(f"{kind}-added", b.id, slug, detail=f"on '{ni.decision}': {ni.question}"))
        elif ni is None and oi is not None:
            kind = "erratum" if oi.errata else "interpretation"
            out.append(Change(f"{kind}-removed", b.id, slug))
        elif oi is not None and ni is not None:
            ov, nv = oi.as_comparable(), ni.as_comparable()
            changed = [(k, ov[k], nv[k]) for k in ov if ov[k] != nv[k]]
            if changed:
                kind = "erratum" if ni.errata else "interpretation"
                out.append(Change(f"{kind}-changed", b.id, slug, fields=changed))
    return out


# --------------------------------------------------------------------------- #
# Git plumbing
# --------------------------------------------------------------------------- #


def _git(repo: Path, *args: str) -> str:
    return subprocess.run(
        ["git", "-C", str(repo), *args],
        check=True,
        capture_output=True,
        text=True,
    ).stdout


def _is_git_repo_with_commits(repo: Path) -> bool:
    try:
        _git(repo, "rev-parse", "--verify", "HEAD")
        return True
    except (subprocess.CalledProcessError, FileNotFoundError):
        return False


class _TreeLoader:
    """Loads collections at commits, cached by the git tree id of ``adrs/``."""

    def __init__(self, repo: Path, adrs_rel: str):
        self.repo = repo
        self.adrs_rel = adrs_rel.strip("/")
        self._cache: dict[str, Collection] = {}

    def at(self, rev: str | None) -> Collection:
        if rev is None:
            return Collection({})
        try:
            tree = _git(self.repo, "rev-parse", f"{rev}:{self.adrs_rel}").strip()
        except subprocess.CalledProcessError:
            return Collection({})  # adrs/ did not exist at this commit
        if tree not in self._cache:
            files: dict[str, str] = {}
            listing = _git(self.repo, "ls-tree", "-r", "-z", "--name-only", tree)
            for rel in filter(None, listing.split("\0")):
                if rel.count("/") == 1:
                    files[rel] = _git(self.repo, "cat-file", "blob", f"{tree}:{rel}")
            self._cache[tree] = load_collection(files, strict=False)
        return self._cache[tree]


def build_changelog(repo: Path, adrs_dir: Path, *, include_worktree: bool = True) -> list[Entry]:
    """Changelog entries, newest first."""
    if not _is_git_repo_with_commits(repo):
        return []
    adrs_rel = adrs_dir.resolve().relative_to(repo.resolve()).as_posix()
    loader = _TreeLoader(repo, adrs_rel)

    log = _git(
        repo, "log", "--first-parent", "-z",
        "--format=%H%x1f%P%x1f%an%x1f%aI%x1f%s", "--", adrs_rel,
    )
    entries: list[Entry] = []
    for record in filter(None, log.split("\0")):
        sha, parents, author, date, subject = record.strip("\n").split("\x1f", 4)
        parent = parents.split()[0] if parents else None
        changes = diff_collections(loader.at(parent), loader.at(sha))
        if changes:
            entries.append(Entry(sha, author, date, subject, changes))

    if include_worktree:
        head = loader.at("HEAD")
        current = load_collection(read_tree(adrs_dir), strict=False)
        changes = diff_collections(head, current)
        if changes:
            entries.insert(0, Entry(None, "", "", "Uncommitted changes", changes))
    return entries
