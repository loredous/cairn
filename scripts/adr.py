#!/usr/bin/env python3
"""Manage Cairn ADRs.

    uv run scripts/adr.py validate [FOLDER]        # check every ADR in FOLDER (default: adrs/)
    uv run scripts/adr.py new "Title" [--author NAME]
    uv run scripts/adr.py check-immutable --base REV  # no decisions.yaml at REV was changed
    uv run scripts/adr.py check-approval --repo OWNER/NAME --pr N  # maintainer majority (CI)
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
from pathlib import Path

from adrsite.immutability import GitError, find_violations
from adrsite.model import ValidationError, load_collection, read_tree
from adrsite.scaffold import new_adr
from adrsite.status import adr_ids_in, proposed_adr_ids

REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_ADRS = REPO_ROOT / "adrs"


def cmd_validate(args: argparse.Namespace) -> int:
    folder: Path = args.folder
    if not folder.is_dir():
        print(f"error: {folder} is not a directory", file=sys.stderr)
        return 2
    files = read_tree(folder)
    adr_ids = adr_ids_in(files)
    try:
        proposed = proposed_adr_ids(REPO_ROOT, folder, adr_ids)
    except ValueError:  # folder outside this repo: nothing can be on main
        proposed = adr_ids
    try:
        collection = load_collection(files, proposed=proposed)
    except ValidationError as e:
        print(f"✗ {len(e.errors)} problem(s) in {folder}:", file=sys.stderr)
        for err in e.errors:
            print(f"  - {err}", file=sys.stderr)
        return 1
    n_adrs = len(collection.adrs)
    n_dec = sum(len(a.decisions) for a in collection.adrs.values())
    n_active = len(collection.active_decisions())
    n_prop = len(collection.proposed)
    print(f"✓ {n_adrs} ADR(s) ({n_prop} proposed), {n_dec} decision(s), "
          f"{n_active} in force — all valid")
    return 0


def cmd_check_immutable(args: argparse.Namespace) -> int:
    try:
        violations = find_violations(REPO_ROOT, args.folder, args.base)
    except GitError as e:
        print(f"error: {e}", file=sys.stderr)
        return 2
    if violations:
        print(f"✗ {len(violations)} committed decisions.yaml file(s) changed:", file=sys.stderr)
        for v in violations:
            print(f"  - {v}", file=sys.stderr)
        print("Decisions are immutable once on main; record changes in a new ADR "
              "(see 0001-record-architecture-decisions/decisions-immutable).", file=sys.stderr)
        return 1
    print(f"✓ no committed decisions.yaml changed since {args.base[:10]}")
    return 0


def cmd_check_approval(args: argparse.Namespace) -> int:
    from adrsite.review import check_pull_request

    token = os.environ.get("GITHUB_TOKEN")
    if not token:
        print("error: GITHUB_TOKEN is not set", file=sys.stderr)
        return 2
    verdict = check_pull_request(args.repo, args.pr, token)
    if not verdict.applies:
        print("✓ this pull request does not change any ADR; no maintainer majority needed")
        return 0
    summary = (f"{len(verdict.agreeing)} of {len(verdict.maintainers)} maintainers agree "
               f"({verdict.needed} needed): {', '.join(verdict.agreeing) or 'none'}")
    if verdict.passed:
        print(f"✓ majority reached: {summary}")
        return 0
    print(f"✗ no majority yet: {summary}", file=sys.stderr)
    print(f"  maintainers: {', '.join(verdict.maintainers) or '(none found)'}", file=sys.stderr)
    return 1


def _git_user() -> str | None:
    try:
        out = subprocess.run(["git", "config", "user.name"], capture_output=True, text=True, check=True)
        return out.stdout.strip() or None
    except (subprocess.CalledProcessError, FileNotFoundError):
        return None


def cmd_new(args: argparse.Namespace) -> int:
    try:
        path = new_adr(args.folder, args.title, author=args.author or _git_user())
    except (FileExistsError, FileNotFoundError, ValueError) as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    print(f"Created {path.relative_to(Path.cwd()) if path.is_relative_to(Path.cwd()) else path}")
    for f in sorted(path.iterdir()):
        print(f"  {f.name}")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="adr", description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)

    p = sub.add_parser("validate", help="validate every ADR in a folder")
    p.add_argument("folder", nargs="?", type=Path, default=DEFAULT_ADRS)
    p.set_defaults(func=cmd_validate)

    p = sub.add_parser("new", help="scaffold a new ADR from adrs/_template")
    p.add_argument("title")
    p.add_argument("--author", help="defaults to git config user.name")
    p.add_argument("--folder", type=Path, default=DEFAULT_ADRS, help="ADR folder (default: adrs/)")
    p.set_defaults(func=cmd_new)

    p = sub.add_parser("check-immutable",
                       help="fail if any decisions.yaml present at a base revision was changed")
    p.add_argument("--base", required=True, help="git revision to compare against (e.g. origin/main)")
    p.add_argument("--folder", type=Path, default=DEFAULT_ADRS, help="ADR folder (default: adrs/)")
    p.set_defaults(func=cmd_check_immutable)

    p = sub.add_parser("check-approval",
                       help="fail unless a majority of maintainers agree with a PR that changes ADRs")
    p.add_argument("--repo", required=True, help="OWNER/NAME")
    p.add_argument("--pr", required=True, type=int, help="pull request number")
    p.set_defaults(func=cmd_check_approval)

    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
