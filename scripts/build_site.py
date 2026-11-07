#!/usr/bin/env python3
"""Build the static site from docs/ and adrs/ with Zensical.

    uv run scripts/build_site.py [--out _site] [--commit SHA]

The footer shows the commit the site was built from: $GITHUB_SHA in GitHub
Actions, otherwise DEVELOP. --preview-pr N adds a banner marking the site as a
preview of pull request N.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from adrsite.model import ValidationError, load_collection, read_tree
from adrsite.site import BuildError, build_site
from adrsite.status import adr_ids_in, proposed_adr_ids, resolve_main_ref

REPO_ROOT = Path(__file__).resolve().parent.parent


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--out", type=Path, default=REPO_ROOT / "_site")
    parser.add_argument("--adrs", type=Path, default=REPO_ROOT / "adrs")
    parser.add_argument("--docs", type=Path, default=REPO_ROOT / "docs")
    parser.add_argument("--config", type=Path, default=REPO_ROOT / "zensical.toml",
                        help="base Zensical config")
    parser.add_argument("--build-dir", type=Path, default=REPO_ROOT / "build",
                        help="where staged Markdown and the effective config are written")
    parser.add_argument("--commit", help="commit to show in the footer (default: $GITHUB_SHA or DEVELOP)")
    parser.add_argument("--preview-pr", type=int, metavar="N",
                        help="mark the site as a preview of pull request N")
    parser.add_argument("--main-ref", help="ref whose ADRs count as accepted "
                        "(default: $CAIRN_MAIN_REF, origin/main, then main)")
    args = parser.parse_args(argv)

    files = read_tree(args.adrs)
    adr_ids = adr_ids_in(files)
    proposed = proposed_adr_ids(REPO_ROOT, args.adrs, adr_ids, args.main_ref)
    ref = resolve_main_ref(REPO_ROOT, args.main_ref)
    print(f"ADRs on {ref or '(no main ref yet)'} are accepted; "
          f"{len(proposed)} proposed", file=sys.stderr)
    try:
        collection = load_collection(files, proposed=proposed)
    except ValidationError as e:
        print("ADRs failed validation; run scripts/adr.py validate for details:", file=sys.stderr)
        for err in e.errors:
            print(f"  - {err}", file=sys.stderr)
        return 1
    try:
        build_site(collection, args.out, repo=REPO_ROOT, adrs_dir=args.adrs, docs_dir=args.docs,
                   base_config=args.config, build_dir=args.build_dir, commit=args.commit,
                   preview_pr=args.preview_pr)
    except BuildError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    print(f"Built site ({len(collection.adrs)} ADR(s)) into {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
