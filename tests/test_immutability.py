from __future__ import annotations

import pytest

from adrsite.immutability import GitError, find_violations
from conftest import adr_files, decision

A1 = "0001-first"


def test_untouched_decisions_pass(repo):
    repo.write(adr_files(A1, [decision("a")]))
    base = repo.commit("Add ADR")
    repo.write(adr_files("0002-new", [decision("b")]))  # new ADRs are fine
    (repo.adrs / A1 / "prose.md").write_text((repo.adrs / A1 / "prose.md").read_text() + "\nedit\n")
    (repo.adrs / A1 / "interpretations.yaml").write_text(
        "- {slug: q, decision: a, question: '?', interpretation: '!'}\n")
    assert find_violations(repo.path, repo.adrs, base) == []


def test_modified_decisions_fail(repo):
    repo.write(adr_files(A1, [decision("a")]))
    base = repo.commit("Add ADR")
    path = repo.adrs / A1 / "decisions.yaml"
    path.write_text(path.read_text() + "# just a comment\n")
    [v] = find_violations(repo.path, repo.adrs, base)
    assert f"{A1}/decisions.yaml: modified" in v


def test_deleted_or_renamed_adr_fails(repo):
    repo.write(adr_files(A1, [decision("a")]))
    base = repo.commit("Add ADR")
    (repo.adrs / A1).rename(repo.adrs / "0001-renamed")
    [v] = find_violations(repo.path, repo.adrs, base)
    assert "deleted or moved" in v


def test_template_is_not_frozen(repo):
    repo.write({"_template/decisions.yaml": "[]\n"})
    base = repo.commit("Template")
    (repo.adrs / "_template" / "decisions.yaml").write_text("# changed\n")
    assert find_violations(repo.path, repo.adrs, base) == []


def test_base_without_adrs_dir_has_nothing_frozen(repo):
    (repo.path / "README").write_text("x")
    base = repo.commit("Initial")
    repo.write(adr_files(A1, [decision("a")]))
    assert find_violations(repo.path, repo.adrs, base) == []


def test_bad_base_revision_raises(repo):
    repo.commit("Initial")
    with pytest.raises(GitError):
        find_violations(repo.path, repo.adrs, "does-not-exist")
