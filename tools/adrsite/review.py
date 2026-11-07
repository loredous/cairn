"""Check that a pull request changing ADRs has a majority of maintainers' approval.

Implements 0001-record-architecture-decisions/approval-by-maintainer-majority
and ci-checks-maintainer-majority. Maintainers are the repository's
collaborators with the Maintain or Admin role. A maintainer agrees with a pull
request when their latest review approves it; the pull request's author agrees
by opening it.
"""

from __future__ import annotations

import json
import urllib.request
from dataclasses import dataclass
from typing import Any, Iterable

API = "https://api.github.com"
ADRS_PREFIX = "adrs/"
NON_ADR_PREFIXES = ("adrs/_",)  # e.g. adrs/_template/ is tooling, not an ADR


@dataclass
class Verdict:
    applies: bool  # does the pull request change any ADR?
    maintainers: list[str]
    agreeing: list[str]
    needed: int

    @property
    def passed(self) -> bool:
        return not self.applies or len(self.agreeing) >= self.needed


def touches_adrs(filenames: Iterable[str]) -> bool:
    return any(f.startswith(ADRS_PREFIX) and not f.startswith(NON_ADR_PREFIXES) for f in filenames)


def latest_review_states(reviews: Iterable[dict[str, Any]]) -> dict[str, str]:
    """Each reviewer's latest decisive review state (reviews arrive oldest first)."""
    states: dict[str, str] = {}
    for review in reviews:
        user = (review.get("user") or {}).get("login")
        state = review.get("state")
        if not user:
            continue
        if state in ("APPROVED", "CHANGES_REQUESTED"):
            states[user.lower()] = state
        elif state == "DISMISSED":
            states.pop(user.lower(), None)
    return states


def evaluate(maintainers: Iterable[str], author: str, reviews: Iterable[dict[str, Any]],
             filenames: Iterable[str]) -> Verdict:
    team = sorted({m.lower() for m in maintainers})
    states = latest_review_states(reviews)
    agreeing = [m for m in team if states.get(m) == "APPROVED" or m == author.lower()]
    return Verdict(
        applies=touches_adrs(filenames),
        maintainers=team,
        agreeing=agreeing,
        needed=len(team) // 2 + 1,  # strict majority
    )


# --------------------------------------------------------------------------- #
# GitHub API
# --------------------------------------------------------------------------- #


def _get_all(path: str, token: str) -> list[dict[str, Any]]:
    items: list[dict[str, Any]] = []
    page = 1
    while True:
        sep = "&" if "?" in path else "?"
        req = urllib.request.Request(
            f"{API}{path}{sep}per_page=100&page={page}",
            headers={"Authorization": f"Bearer {token}", "Accept": "application/vnd.github+json",
                     "X-GitHub-Api-Version": "2022-11-28", "User-Agent": "cairn-adr-approval-check"},
        )
        with urllib.request.urlopen(req, timeout=30) as resp:
            batch = json.load(resp)
        if isinstance(batch, dict):  # single object endpoints
            return [batch]
        items.extend(batch)
        if len(batch) < 100:
            return items
        page += 1


def maintainers_of(repo: str, token: str) -> list[str]:
    collaborators = _get_all(f"/repos/{repo}/collaborators?affiliation=all", token)
    return [c["login"] for c in collaborators
            if (c.get("permissions") or {}).get("admin") or (c.get("permissions") or {}).get("maintain")]


def check_pull_request(repo: str, number: int, token: str) -> Verdict:
    [pr] = _get_all(f"/repos/{repo}/pulls/{number}", token)
    files = [f["filename"] for f in _get_all(f"/repos/{repo}/pulls/{number}/files", token)]
    reviews = _get_all(f"/repos/{repo}/pulls/{number}/reviews", token)
    return evaluate(maintainers_of(repo, token), pr["user"]["login"], reviews, files)
