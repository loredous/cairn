"""Build the site: stage docs + generated ADR pages, then render with Zensical.

Staged layout (``build/site-src/``), which becomes the site::

    <docs/ contents>         index.md becomes the home page
    adrs/index.md            index of every ADR
    adrs/<folder>.md         one page per ADR
    handbook.md              decisions currently in force
    changelog.md             per-commit changes to the decision record
    stylesheets/adr.css      badge styles

The Zensical config is generated from the committed base ``zensical.toml``,
with docs_dir, site_dir, nav, and the build stamp (commit) filled in.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tomllib
from importlib import resources
from pathlib import Path
from typing import Any

import tomli_w

from . import pages
from .changelog import build_changelog
from .model import Collection

LOCAL_BUILD = "DEVELOP"
STYLESHEET = "stylesheets/adr.css"
SCRIPT = "javascripts/adr.js"
GENERATED = (pages.ADRS_DIR, pages.HANDBOOK_PAGE, pages.CHANGELOG_PAGE, "stylesheets",
             "javascripts")


class BuildError(Exception):
    pass


def detect_commit() -> str:
    """The commit being built: GITHUB_SHA in Actions, DEVELOP for local builds."""
    return os.environ.get("GITHUB_SHA") or LOCAL_BUILD


def build_stamp(commit: str, repo_url: str | None) -> str:
    if commit == LOCAL_BUILD:
        return f"Built from <code>{LOCAL_BUILD}</code>"
    label = f"<code>{commit[:10]}</code>"
    if repo_url:
        label = f'<a href="{repo_url.rstrip("/")}/commit/{commit}">{label}</a>'
    return f"Built from commit {label}"


def stage(collection: Collection, staging: Path, *, docs_dir: Path | None,
          repo: Path, adrs_dir: Path, repo_url: str | None) -> list[Any]:
    """Write all Markdown into ``staging``; returns the generated nav."""
    if staging.exists():
        shutil.rmtree(staging)
    staging.mkdir(parents=True)

    doc_pages: list[str] = []
    if docs_dir and docs_dir.is_dir():
        for name in GENERATED:
            if (docs_dir / name).exists():
                raise BuildError(f"docs/{name} collides with a generated page; rename it")
        shutil.copytree(docs_dir, staging, dirs_exist_ok=True,
                        ignore=shutil.ignore_patterns(".*"))
        doc_pages = sorted(p.relative_to(staging).as_posix() for p in staging.rglob("*.md"))
    if "index.md" not in doc_pages:
        (staging / "index.md").write_text(
            "# Cairn\n\nDocumentation and architecture decisions for the Cairn protocol.\n")

    def write(rel: str, text: str) -> None:
        path = staging / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    write(pages.RECORDS_INDEX, pages.render_records_index(collection))
    for adr in collection.sorted_adrs():
        write(pages.adr_page(adr.id), pages.render_adr(collection, adr))
    write(pages.HANDBOOK_PAGE, pages.render_handbook(collection))
    entries = build_changelog(repo, adrs_dir)
    write(pages.CHANGELOG_PAGE, pages.render_changelog(entries, repo_url=repo_url))
    assets = resources.files("adrsite").joinpath("assets")
    write(STYLESHEET, assets.joinpath("adr.css").read_text())
    write(SCRIPT, assets.joinpath("adr.js").read_text())

    docs_nav = _nested_nav([p for p in doc_pages if p != "index.md"])
    nav: list[Any] = ["index.md"]
    if docs_nav:
        nav.append({"Docs": docs_nav})
    nav.append({"ADRs": [
        {"Handbook": pages.HANDBOOK_PAGE},
        {"Records": [pages.RECORDS_INDEX] +
                    [pages.adr_page(a.id) for a in collection.sorted_adrs()]},
        {"Changelog": pages.CHANGELOG_PAGE},
    ]})
    return nav


def _nested_nav(paths: list[str]) -> list[Any]:
    """Turn ["a.md", "x/b.md", "x/index.md"] into a Zensical nav list."""
    top: list[Any] = []
    groups: dict[str, list[str]] = {}
    for p in paths:
        head, sep, rest = p.partition("/")
        if sep:
            groups.setdefault(head, []).append(rest)
        else:
            top.append(p)
    for folder in sorted(groups):
        children = _nested_nav(groups[folder])
        children = [_prefix(folder, c) for c in children]
        children.sort(key=lambda c: not (isinstance(c, str) and c.endswith("/index.md")))
        top.append({folder.replace("-", " ").title(): children})
    return top


def _prefix(folder: str, item: Any) -> Any:
    if isinstance(item, str):
        return f"{folder}/{item}"
    return {k: [_prefix(folder, c) for c in v] for k, v in item.items()}


def write_config(base_config: Path, out_config: Path, *, docs_dir: Path, site_dir: Path,
                 nav: list[Any], commit: str) -> dict[str, Any]:
    config = tomllib.loads(base_config.read_text())
    project = config.setdefault("project", {})
    stamp = build_stamp(commit, project.get("repo_url"))
    base_copyright = project.get("copyright")
    project["copyright"] = f"{base_copyright} · {stamp}" if base_copyright else stamp
    project["docs_dir"] = os.path.relpath(docs_dir, out_config.parent)
    project["site_dir"] = os.path.relpath(site_dir, out_config.parent)
    project["nav"] = nav
    project["extra_css"] = list(dict.fromkeys([*project.get("extra_css", []), STYLESHEET]))
    project["extra_javascript"] = list(dict.fromkeys([*project.get("extra_javascript", []), SCRIPT]))
    project.setdefault("extra", {})["build_commit"] = commit
    out_config.parent.mkdir(parents=True, exist_ok=True)
    out_config.write_text(tomli_w.dumps(config))
    return config


def _zensical_executable() -> str:
    candidate = Path(sys.executable).parent / "zensical"
    if candidate.exists():
        return str(candidate)
    found = shutil.which("zensical")
    if not found:
        raise BuildError("zensical is not installed; run `uv sync`")
    return found


def build_site(collection: Collection, out_dir: Path, *, repo: Path, adrs_dir: Path,
               docs_dir: Path | None, base_config: Path, build_dir: Path,
               commit: str | None = None) -> None:
    commit = commit or detect_commit()
    repo_url = tomllib.loads(base_config.read_text()).get("project", {}).get("repo_url")
    staging = build_dir / "site-src"
    nav = stage(collection, staging, docs_dir=docs_dir, repo=repo, adrs_dir=adrs_dir,
                repo_url=repo_url)
    config_path = build_dir / "zensical.toml"
    # Zensical requires site_dir to be inside the config's directory, so render
    # into build/site and move the result to out_dir afterwards.
    rendered = build_dir / "site"
    write_config(base_config, config_path, docs_dir=staging, site_dir=rendered,
                 nav=nav, commit=commit)
    if rendered.exists():
        shutil.rmtree(rendered)
    result = subprocess.run([_zensical_executable(), "build", "--clean", "-f", str(config_path)],
                            cwd=build_dir)
    if result.returncode != 0:
        raise BuildError(f"zensical build failed with exit code {result.returncode}")
    if out_dir.resolve() != rendered.resolve():
        if out_dir.exists():
            shutil.rmtree(out_dir)
        shutil.move(rendered, out_dir)
