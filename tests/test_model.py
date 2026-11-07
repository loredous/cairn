from __future__ import annotations

import pytest

from adrsite.model import STRENGTHS, DecisionRef, ValidationError, load_collection
from conftest import adr_files, decision, prose

A1 = "0001-first"
A2 = "0002-second"


def load(*file_sets: dict[str, str], proposed=()):
    files: dict[str, str] = {}
    for fs in file_sets:
        files.update(fs)
    return load_collection(files, proposed=proposed)


def errors_for(*file_sets: dict[str, str]) -> list[str]:
    with pytest.raises(ValidationError) as exc:
        load(*file_sets)
    return exc.value.errors


# --- loading -----------------------------------------------------------------


def test_loads_adr_with_decisions_and_interpretations():
    c = load(adr_files(A1, [decision("alpha"), decision("beta", "MAY", tags=["x"])],
                       interpretations=[{"slug": "q1", "decision": "alpha",
                                         "question": "Q?", "interpretation": "A."}]))
    adr = c.adrs[A1]
    assert adr.number == 1 and c.status_of(adr) == "accepted"
    assert [d.slug for d in adr.decisions] == ["alpha", "beta"]
    assert adr.decision("beta").tags == ["x"]
    assert [i.slug for i in adr.interpretations_for("alpha")] == ["q1"]
    assert "## Context" in adr.prose and "---" not in adr.prose


def test_every_rfc2119_strength_is_accepted():
    assert set(STRENGTHS) == {"MUST", "MUST_NOT", "SHOULD", "SHOULD_NOT", "MAY"}
    c = load(adr_files(A1, [decision(f"d{i}", s) for i, s in enumerate(STRENGTHS)]))
    assert [d.strength for d in c.adrs[A1].decisions] == list(STRENGTHS)


def test_underscore_folders_are_ignored():
    c = load(adr_files(A1, [decision("a")]), {"_template/prose.md": "{{title}}"})
    assert list(c.adrs) == [A1]


def test_empty_decisions_file_is_allowed():
    c = load_collection({f"{A1}/prose.md": prose(), f"{A1}/decisions.yaml": "# nothing yet\n"})
    assert c.adrs[A1].decisions == []


# --- structural validation ---------------------------------------------------


@pytest.mark.parametrize("folder", ["1-short", "0001_underscore", "0001-Upper", "0001-"])
def test_bad_folder_names_are_rejected(folder):
    errs = errors_for(adr_files(folder, [decision("a")]))
    assert any("folder name" in e for e in errs)


def test_missing_files_are_reported():
    errs = errors_for({f"{A1}/prose.md": prose()})
    assert any("missing decisions.yaml" in e for e in errs)
    errs = errors_for({f"{A1}/decisions.yaml": "[]"})
    assert any("missing prose.md" in e for e in errs)


def test_unknown_strength_is_rejected():
    errs = errors_for(adr_files(A1, [decision("a", "MIGHT")]))
    assert any("strength" in e and "MIGHT" in e for e in errs)


def test_misspelled_field_is_rejected():
    d = decision("a", superceeds={"adr": A1, "decision": "b"})
    errs = errors_for(adr_files(A1, [d]))
    assert any("superceeds" in e for e in errs)


def test_strength_required_for_ordinary_decisions():
    errs = errors_for(adr_files(A1, [decision("a", strength=None)]))
    assert any("'strength' is a required property" in e for e in errs)


def test_stored_status_is_rejected():
    errs = errors_for({f"{A1}/prose.md": prose(status="accepted"), f"{A1}/decisions.yaml": "[]"})
    assert any("status" in e and "Additional properties" in e for e in errs)


def test_all_errors_reported_together():
    errs = errors_for(adr_files(A1, [decision("a", "NOPE")]), adr_files("bad", [decision("b")]))
    assert len(errs) >= 2


# --- cross-reference validation ----------------------------------------------


def test_duplicate_numbers_and_slugs():
    errs = errors_for(adr_files(A1, [decision("a"), decision("a")]),
                      adr_files("0001-again", [decision("b")]))
    assert any("duplicate decision slug 'a'" in e for e in errs)
    assert any("ADR number 0001 already used" in e for e in errs)


def test_interpretation_must_reference_decision_in_same_adr():
    errs = errors_for(adr_files(A1, [decision("a")], interpretations=[
        {"slug": "q", "decision": "missing", "question": "?", "interpretation": "!"}]))
    assert any("unknown decision 'missing'" in e for e in errs)


def test_supersedes_must_resolve():
    errs = errors_for(adr_files(A1, [decision("a", supersedes={"adr": A2, "decision": "zzz"})]))
    assert any("supersedes unknown decision" in e for e in errs)


def test_cannot_supersede_self():
    errs = errors_for(adr_files(A1, [decision("a", supersedes={"adr": A1, "decision": "a"})]))
    assert any("cannot supersede itself" in e for e in errs)


def test_decision_superseded_at_most_once():
    errs = errors_for(
        adr_files(A1, [decision("a")]),
        adr_files(A2, [decision("b", supersedes={"adr": A1, "decision": "a"}),
                       decision("c", supersedes={"adr": A1, "decision": "a"})]),
    )
    assert any("already superseded" in e for e in errs)


def test_supersession_cycle_detected():
    errs = errors_for(
        adr_files(A1, [decision("a", supersedes={"adr": A2, "decision": "b"})]),
        adr_files(A2, [decision("b", supersedes={"adr": A1, "decision": "a"})]),
    )
    assert any("cycle" in e for e in errs)


def test_rescinded_requires_supersedes():
    errs = errors_for(adr_files(A1, [decision("a", rescinded=True)]))
    assert any("supersedes" in e for e in errs)


def test_cannot_supersede_a_rescission_record():
    errs = errors_for(
        adr_files(A1, [decision("a")]),
        adr_files(A2, [decision("r", None, rescinded=True, supersedes={"adr": A1, "decision": "a"})]),
        adr_files("0003-third", [decision("x", supersedes={"adr": A2, "decision": "r"})]),
    )
    assert any("cannot supersede rescission record" in e for e in errs)


def test_non_strict_mode_skips_broken_adrs():
    c = load_collection({**adr_files(A1, [decision("a")]), **adr_files(A2, [decision("b", "NOPE")])},
                        strict=False)
    assert list(c.adrs) == [A1]


# --- decision state / handbook -----------------------------------------------


def test_plain_decisions_in_accepted_adr_are_active():
    c = load(adr_files(A1, [decision("a"), decision("b")]))
    assert [d.slug for d in c.active_decisions()] == ["a", "b"]


def test_decisions_in_proposed_adrs_are_not_in_force():
    c = load(adr_files(A1, [decision("a")]), adr_files(A2, [decision("b")]), proposed={A2})
    assert c.status_of(c.adrs[A2]) == "proposed"
    assert c.state_of(c.adrs[A2].decision("b")) == "proposed"
    assert [d.slug for d in c.active_decisions()] == ["a"]


def test_unknown_proposed_ids_are_ignored():
    c = load(adr_files(A1, [decision("a")]), proposed={"0099-not-here"})
    assert c.proposed == frozenset()


def test_adr_with_no_decisions_in_force_shows_superseded():
    c = load(adr_files(A1, [decision("a")]),
             adr_files(A2, [decision("b", supersedes={"adr": A1, "decision": "a"})]))
    assert c.status_of(c.adrs[A1]) == "superseded"
    assert c.status_of(c.adrs[A2]) == "accepted"


def test_supersession_retires_old_decision():
    c = load(adr_files(A1, [decision("a"), decision("keep")]),
             adr_files(A2, [decision("b", supersedes={"adr": A1, "decision": "a"})]))
    assert c.state_of(c.decision(DecisionRef(A1, "a"))) == "superseded"
    assert c.superseded_by[DecisionRef(A1, "a")].slug == "b"
    assert [str(d.ref) for d in c.active_decisions()] == [f"{A1}/keep", f"{A2}/b"]


def test_proposed_supersession_does_not_take_effect():
    c = load(adr_files(A1, [decision("a")]),
             adr_files(A2, [decision("b", supersedes={"adr": A1, "decision": "a"})]), proposed={A2})
    a = c.decision(DecisionRef(A1, "a"))
    assert c.state_of(a) == "active"
    assert c.pending_supersession[a.ref].slug == "b"


def test_rescission_record_withdraws_target_and_is_not_itself_in_force():
    c = load(adr_files(A1, [decision("a"), decision("keep")]),
             adr_files(A2, [decision("r", None, rescinded=True, supersedes={"adr": A1, "decision": "a"})]))
    assert c.state_of(c.decision(DecisionRef(A1, "a"))) == "rescinded"
    assert c.state_of(c.decision(DecisionRef(A2, "r"))) == "rescission"
    assert [d.slug for d in c.active_decisions()] == ["keep"]


def test_supersession_chain_leaves_only_the_newest_active():
    c = load(adr_files(A1, [decision("a")]),
             adr_files(A2, [decision("b", supersedes={"adr": A1, "decision": "a"})]),
             adr_files("0003-third", [decision("c", supersedes={"adr": A2, "decision": "b"})]))
    assert [str(d.ref) for d in c.active_decisions()] == ["0003-third/c"]


def test_real_adrs_are_valid():
    from pathlib import Path

    from adrsite.model import read_tree

    repo_adrs = Path(__file__).resolve().parent.parent / "adrs"
    c = load_collection(read_tree(repo_adrs))
    assert "0001-record-architecture-decisions" in c.adrs


def test_errata_flag_loads_and_defaults_false():
    c = load(adr_files(A1, [decision("a")], interpretations=[
        {"slug": "q", "decision": "a", "question": "?", "interpretation": "!"},
        {"slug": "e", "decision": "a", "question": "Typo", "interpretation": "Fixed", "errata": True},
    ]))
    assert [(i.slug, i.errata) for i in c.adrs[A1].interpretations] == [("q", False), ("e", True)]


def test_errata_must_be_boolean():
    errs = errors_for(adr_files(A1, [decision("a")], interpretations=[
        {"slug": "e", "decision": "a", "question": "?", "interpretation": "!", "errata": "yes"}]))
    assert any("errata" in e for e in errs)


def test_category_is_required_and_ordered_by_first_appearance():
    errs = errors_for({f"{A1}/prose.md": prose(),
                       f"{A1}/decisions.yaml": "- {slug: a, title: A, strength: MUST, statement: S}\n"})
    assert any("'category' is a required property" in e for e in errs)
    c = load(adr_files(A1, [decision("a", category="zeta"), decision("b", category="alpha"),
                            decision("c", category="zeta")]))
    assert c.categories() == ["zeta", "alpha"]


def test_tags_allow_dotted_version_labels():
    c = load(adr_files(A1, [decision("a", tags=["v0.1", "v1.x", "link-layer"])]))
    assert c.adrs[A1].decision("a").tags == ["v0.1", "v1.x", "link-layer"]
    for bad in (["V1"], ["v1..x"], ["-x"], ["a_b"]):
        errs = errors_for(adr_files(A1, [decision("a", tags=bad)]))
        assert any("tags" in e or "does not match" in e for e in errs), bad
