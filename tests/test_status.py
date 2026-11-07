from __future__ import annotations

from adrsite.status import merged_adr_ids, proposed_adr_ids, resolve_main_ref
from conftest import adr_files, decision

A1, A2 = "0001-first", "0002-second"


def test_no_main_ref_means_everything_is_proposed(repo, monkeypatch):
    monkeypatch.delenv("CAIRN_MAIN_REF", raising=False)
    repo.write(adr_files(A1, [decision("a")]))
    assert resolve_main_ref(repo.path) is None  # no commits yet
    assert proposed_adr_ids(repo.path, repo.adrs, {A1}) == {A1}


def test_adrs_on_main_are_accepted_and_others_proposed(repo, monkeypatch):
    monkeypatch.delenv("CAIRN_MAIN_REF", raising=False)
    repo.write({**adr_files(A1, [decision("a")]), "_template/decisions.yaml": "[]\n"})
    repo.commit("ADR 1 on main")
    repo.git("checkout", "-q", "-b", "feature")
    repo.write(adr_files(A2, [decision("b")]))
    repo.commit("ADR 2 on a branch")
    assert resolve_main_ref(repo.path) == "main"
    assert merged_adr_ids(repo.path, repo.adrs, "main") == {A1}
    assert proposed_adr_ids(repo.path, repo.adrs, {A1, A2}) == {A2}
    # An explicit ref, or $CAIRN_MAIN_REF, overrides the default.
    assert proposed_adr_ids(repo.path, repo.adrs, {A1, A2}, "feature") == set()
    monkeypatch.setenv("CAIRN_MAIN_REF", "feature")
    assert proposed_adr_ids(repo.path, repo.adrs, {A1, A2}) == set()


def test_origin_main_preferred_over_local_main(repo, monkeypatch):
    monkeypatch.delenv("CAIRN_MAIN_REF", raising=False)
    repo.write(adr_files(A1, [decision("a")]))
    pushed = repo.commit("pushed")
    repo.git("update-ref", "refs/remotes/origin/main", pushed)
    repo.write(adr_files(A2, [decision("b")]))
    repo.commit("local only, not yet on GitHub")
    assert resolve_main_ref(repo.path) == "origin/main"
    assert proposed_adr_ids(repo.path, repo.adrs, {A1, A2}) == {A2}
