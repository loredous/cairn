from __future__ import annotations

import tomllib
from pathlib import Path

import pytest

from adrsite import pages
from adrsite.changelog import build_changelog
from adrsite.model import load_collection, read_tree
from adrsite.site import LOCAL_BUILD, BuildError, build_site, build_stamp, detect_commit, stage
from conftest import adr_files, decision

REPO_ROOT = Path(__file__).resolve().parent.parent
A1, A2 = "0001-first", "0002-second"


@pytest.fixture
def collection():
    files = {
        **adr_files(A1, [decision("old", category="link-layer", tags=["framing"]),
                         decision("gone"),
                         decision("keep", "MAY", category="phy", tags=["sync", "framing"])],
                    interpretations=[{"slug": "q", "decision": "keep", "question": "Why?",
                                      "interpretation": "Because."}]),
        **adr_files(A2, [decision("new", "SHOULD", supersedes={"adr": A1, "decision": "old"}),
                         decision("drop", None, rescinded=True,
                                  supersedes={"adr": A1, "decision": "gone"})]),
    }
    return load_collection(files)


def test_adr_page_shows_lineage_and_interpretations(collection):
    md = pages.render_adr(collection, collection.adrs[A1])
    assert md.startswith('---\ntitle: "ADR 0001: 0001-first"')
    assert ('### <span class="adr-ref" data-ref="0001-first/old" role="button" tabindex="0" '
            'aria-label="Title of old (copy reference 0001-first/old)">Title of old</span> '
            '{ #d-old data-toc-label="Title of old" }') in md
    assert "`old`" not in md and "`0001-first/old`" not in md.split("Superseded by")[0]  # no visible slug
    assert "Superseded by [`0002-second/new`](0002-second.md#d-new)" in md
    assert "Rescinded by [`0002-second/drop`](0002-second.md#d-drop)" in md
    assert 'adr-state-superseded' in md and 'adr-state-rescinded' in md
    assert '<span id="i-q"></span>' in md and "**Why?**" in md

    md2 = pages.render_adr(collection, collection.adrs[A2])
    assert "Supersedes [`0001-first/old`](0001-first.md#d-old)" in md2
    assert "Rescinds [`0001-first/gone`](0001-first.md#d-gone)" in md2
    assert "adr-state-rescission" in md2


def test_handbook_groups_by_category_and_marks_state(collection):
    md = pages.render_handbook(collection)
    assert "**2** decisions are currently in force" in md
    # Categories in order of first appearance, each a filterable section.
    assert [line for line in md.splitlines() if line.startswith("## ")] == [
        "## Link layer { #cat-link-layer }", "## General { #cat-general }", "## PHY { #cat-phy }"]
    # Every decision is present (the filter hides non-active ones client-side),
    # with ADR-qualified anchors so equal slugs in different ADRs don't collide.
    for anchor, state in [("0001-old", "superseded"), ("0001-gone", "rescinded"),
                          ("0001-keep", "active"), ("0002-new", "active"), ("0002-drop", "rescission")]:
        assert f"{{ #d-{anchor} data-toc-label=" in md
    assert '<div class="adr-entry" data-state="superseded" data-tags="framing" markdown="1">' in md
    assert 'data-state="active" data-tags="sync framing"' in md
    assert '<span id="i-0001-q"></span>' in md
    assert "(adrs/0001-first.md#d-old)" in md  # lineage links go to the ADR page


def test_handbook_filter_bar_lists_all_tags(collection):
    md = pages.render_handbook(collection)
    assert '<input type="checkbox" id="adr-in-force-only" checked>' in md
    assert md.count('class="adr-tag-chip"') == 2
    assert 'data-tag="framing"' in md and 'data-tag="sync"' in md


def test_category_titles():
    assert pages.category_title("physical-layer") == "Physical layer"
    assert pages.category_title("adr-format") == "ADR format"
    assert pages.category_title("arq") == "ARQ"


def test_records_index(collection):
    md = pages.render_records_index(collection)
    assert "| `0001` | [0001-first](0001-first.md)" in md
    assert "| 3 | 1 |" in md  # 3 decisions, 1 in force


def test_changelog_page(repo):
    repo.write(adr_files(A1, [decision("a")]))
    repo.commit("Add")
    repo.write(adr_files(A1, [decision("a", "SHOULD")], title="Renamed"))
    sha = repo.commit("Rework")
    md = pages.render_changelog(build_changelog(repo.path, repo.adrs, include_worktree=False),
                                repo_url="https://github.com/o/r")
    assert f"## Rework {{ #c-{sha[:10]} }}" in md
    assert f"(https://github.com/o/r/commit/{sha})" in md
    assert "- **Title changed** [`0001-first`](adrs/0001-first.md)" in md
    assert "    - `strength`: `MUST` → `SHOULD`" in md
    assert "- **Decision added** [`0001-first/a`](adrs/0001-first.md#d-a) — MUST: Title of a" in md


def test_build_stamp(monkeypatch):
    monkeypatch.delenv("GITHUB_SHA", raising=False)
    assert detect_commit() == LOCAL_BUILD
    monkeypatch.setenv("GITHUB_SHA", "abc123def4567890")
    assert detect_commit() == "abc123def4567890"
    assert build_stamp(LOCAL_BUILD, "https://x") == "Built from <code>DEVELOP</code>"
    assert build_stamp("abc123def4567890", "https://g/o/r") == (
        'Built from commit <a href="https://g/o/r/commit/abc123def4567890"><code>abc123def4</code></a>')


def test_stage_builds_nav_and_rejects_collisions(tmp_path, collection):
    docs = tmp_path / "docs"
    (docs / "guide").mkdir(parents=True)
    (docs / "index.md").write_text("# Home\n")
    (docs / "SPEC.md").write_text("# Spec\n")
    (docs / "guide" / "index.md").write_text("# Guide\n")
    (docs / "guide" / "setup.md").write_text("# Setup\n")
    nav = stage(collection, tmp_path / "out", docs_dir=docs, repo=tmp_path,
                adrs_dir=tmp_path / "adrs", repo_url=None)
    assert nav == [
        "index.md",
        {"Docs": ["SPEC.md", {"Guide": ["guide/index.md", "guide/setup.md"]}]},
        {"ADRs": [
            {"Handbook": "handbook.md"},
            {"Records": ["adrs/index.md", "adrs/0001-first.md", "adrs/0002-second.md"]},
            {"Changelog": "changelog.md"},
        ]},
    ]
    (docs / "handbook.md").write_text("# clash\n")
    with pytest.raises(BuildError, match="collides"):
        stage(collection, tmp_path / "out", docs_dir=docs, repo=tmp_path,
              adrs_dir=tmp_path / "adrs", repo_url=None)


def test_full_zensical_build(tmp_path, collection):
    docs = tmp_path / "docs"
    docs.mkdir()
    (docs / "index.md").write_text("# Home\n\nSee [the guide](guide.md).\n")
    (docs / "guide.md").write_text("# Guide\n")
    out = tmp_path / "site"
    build_site(collection, out, repo=tmp_path, adrs_dir=tmp_path / "adrs", docs_dir=docs,
               base_config=REPO_ROOT / "zensical.toml", build_dir=tmp_path / "build",
               commit="0123456789abcdef")

    effective = tomllib.loads((tmp_path / "build" / "zensical.toml").read_text())["project"]
    assert effective["extra"]["build_commit"] == "0123456789abcdef"

    home = (out / "index.html").read_text()
    assert "Built from commit" in home and "0123456789" in home
    assert "Switch to dark mode" in home and 'data-md-component="search"' in home
    adr = (out / "adrs" / A1 / "index.html").read_text()
    assert 'id="d-old"' in adr and 'adr-strength-may' in adr
    assert "!!!" not in adr and "{ #" not in adr
    handbook = (out / "handbook" / "index.html").read_text()
    assert 'id="d-0001-keep"' in handbook and 'id="d-0001-old"' in handbook
    assert 'class="adr-entry" data-state="superseded"' in handbook  # md_in_html kept the wrapper
    assert 'id="adr-in-force-only"' in handbook and "javascripts/adr.js" in handbook
    assert 'data-ref="0001-keep"' not in handbook and 'data-ref="0001-first/keep"' in handbook
    import re
    toc_links = re.findall(r'class="md-nav__link">(.*?)</a>', handbook, re.S)
    assert toc_links and not any("adr-ref" in link for link in toc_links)  # sidebar stays plain
    assert 'id="cat-phy"' in handbook
    assert (out / "javascripts" / "adr.js").exists()
    assert (out / "changelog" / "index.html").exists()
    assert (out / "stylesheets" / "adr.css").exists()


def test_real_repo_stages_cleanly(tmp_path):
    c = load_collection(read_tree(REPO_ROOT / "adrs"))
    nav = stage(c, tmp_path / "s", docs_dir=REPO_ROOT / "docs", repo=REPO_ROOT,
                adrs_dir=REPO_ROOT / "adrs", repo_url=None)
    assert nav[0] == "index.md"


def test_errata_render_as_erratum_callout():
    c = load_collection(adr_files(A1, [decision("a")], interpretations=[
        {"slug": "e", "decision": "a", "question": "Says recieve", "interpretation": "receive",
         "errata": True, "date": "2026-11-05"}]))
    md = pages.render_adr(c, c.adrs[A1])
    assert '<span id="i-e"></span>\n\n!!! bug "Erratum (2026-11-05)"' in md
    assert "**Says recieve**" in md
