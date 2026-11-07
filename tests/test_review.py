from __future__ import annotations

from adrsite.review import evaluate, latest_review_states, touches_adrs

ADR_FILES = ["adrs/0003-new/decisions.yaml", "README.md"]


def review(user: str, state: str) -> dict:
    return {"user": {"login": user}, "state": state}


def test_touches_adrs_ignores_template_and_other_files():
    assert touches_adrs(ADR_FILES)
    assert not touches_adrs(["adrs/_template/prose.md", "tools/x.py"])


def test_latest_decisive_review_wins():
    states = latest_review_states([
        review("Ann", "APPROVED"), review("ann", "COMMENTED"),
        review("bob", "APPROVED"), review("bob", "CHANGES_REQUESTED"),
        review("cy", "APPROVED"), review("cy", "DISMISSED"),
    ])
    assert states == {"ann": "APPROVED", "bob": "CHANGES_REQUESTED"}


def test_strict_majority_required_and_author_counts():
    team = ["ann", "bob", "cy", "dee"]  # 4 maintainers: 3 needed
    v = evaluate(team, "ann", [review("bob", "APPROVED")], ADR_FILES)
    assert v.needed == 3 and v.agreeing == ["ann", "bob"] and not v.passed
    v = evaluate(team, "ann", [review("bob", "APPROVED"), review("cy", "APPROVED")], ADR_FILES)
    assert v.passed


def test_non_maintainer_approvals_do_not_count():
    v = evaluate(["ann", "bob", "cy"], "outsider",
                 [review("outsider", "APPROVED"), review("eve", "APPROVED"), review("ann", "APPROVED")],
                 ADR_FILES)
    assert v.agreeing == ["ann"] and not v.passed


def test_sole_maintainer_is_a_majority_of_one():
    assert evaluate(["Loredous"], "loredous", [], ADR_FILES).passed


def test_prs_without_adr_changes_pass():
    v = evaluate(["ann", "bob", "cy"], "outsider", [], ["docs/index.md"])
    assert not v.applies and v.passed
