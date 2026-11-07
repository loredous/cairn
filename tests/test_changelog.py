from __future__ import annotations

from adrsite.changelog import build_changelog, diff_collections
from adrsite.model import load_collection
from conftest import adr_files, decision

A1 = "0001-first"
A2 = "0002-second"


def kinds(changes):
    return [(c.kind, c.subject) for c in changes]


def coll(*fs):
    files = {}
    for f in fs:
        files.update(f)
    return load_collection(files)


def test_new_adr_lists_its_decisions_and_interpretations():
    new = coll(adr_files(A1, [decision("a"), decision("b")], interpretations=[
        {"slug": "q", "decision": "a", "question": "?", "interpretation": "!"}]))
    assert kinds(diff_collections(coll(), new)) == [
        ("adr-added", None), ("decision-added", "a"), ("decision-added", "b"),
        ("interpretation-added", "q"),
    ]


def test_supersession_and_rescission_are_called_out():
    old = coll(adr_files(A1, [decision("a"), decision("b")]))
    new = coll(adr_files(A1, [decision("a"), decision("b")]),
               adr_files(A2, [decision("a2", supersedes={"adr": A1, "decision": "a"}),
                              decision("r", None, rescinded=True,
                                       supersedes={"adr": A1, "decision": "b"})]))
    changes = diff_collections(old, new)
    assert kinds(changes) == [
        ("adr-added", None), ("decision-added", "a2"), ("decision-supersedes", "a2"),
        ("decision-rescinds", "r"),
    ]
    assert changes[-1].detail == f"{A1}/b"


def test_title_prose_and_field_changes():
    old = coll(adr_files(A1, [decision("a")]))
    new = coll(adr_files(A1, [decision("a", "SHOULD")], title="Renamed", body="\nMore prose.\n"))
    changes = diff_collections(old, new)
    assert kinds(changes) == [("adr-title", None), ("adr-prose", None), ("decision-changed", "a")]
    assert changes[0].fields == [("title", A1, "Renamed")]
    assert changes[-1].fields == [("strength", "MUST", "SHOULD")]


def test_removals():
    old = coll(adr_files(A1, [decision("a"), decision("b")]), adr_files(A2, [decision("c")]))
    new = coll(adr_files(A1, [decision("a")]))
    assert kinds(diff_collections(old, new)) == [("decision-removed", "b"), ("adr-removed", None)]


def test_no_changes_means_empty_diff():
    c = coll(adr_files(A1, [decision("a")]))
    assert diff_collections(c, c) == []


def test_changelog_from_git_history(repo):
    repo.write(adr_files(A1, [decision("a")]))
    first = repo.commit("Add ADR 1")
    (repo.path / "README").write_text("unrelated")
    repo.commit("Unrelated change")
    repo.write(adr_files(A1, [decision("a")], body="\nClarified context.\n"))
    second = repo.commit("Clarify ADR 1")

    entries = build_changelog(repo.path, repo.adrs)
    assert [e.sha for e in entries] == [second, first]  # newest first, unrelated skipped
    assert entries[0].subject == "Clarify ADR 1"
    assert entries[0].author == "Test Author"
    assert kinds(entries[0].changes) == [("adr-prose", None)]
    assert kinds(entries[1].changes) == [("adr-added", None), ("decision-added", "a")]


def test_uncommitted_changes_appear_first(repo):
    repo.write(adr_files(A1, [decision("a")]))
    repo.commit("Add ADR 1")
    repo.write(adr_files(A2, [decision("b")]))
    entries = build_changelog(repo.path, repo.adrs)
    assert entries[0].sha is None and entries[0].subject == "Uncommitted changes"
    assert not build_changelog(repo.path, repo.adrs, include_worktree=False)[0].sha is None


def test_invalid_historical_commits_do_not_break_changelog(repo):
    repo.write(adr_files(A1, [decision("a", "BOGUS")]))
    repo.commit("Broken ADR")
    repo.write(adr_files(A1, [decision("a")]))
    repo.commit("Fix ADR")
    entries = build_changelog(repo.path, repo.adrs)
    assert entries[0].subject == "Fix ADR"


def test_not_a_git_repo_yields_empty_changelog(tmp_path):
    (tmp_path / "adrs").mkdir()
    assert build_changelog(tmp_path, tmp_path / "adrs") == []


def test_errata_are_reported_as_errata():
    base = adr_files(A1, [decision("a")])
    old = coll(base)
    new = coll(adr_files(A1, [decision("a")], interpretations=[
        {"slug": "e", "decision": "a", "question": "Typo", "interpretation": "Fixed", "errata": True}]))
    assert kinds(diff_collections(old, new)) == [("erratum-added", "e")]
