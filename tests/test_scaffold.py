from __future__ import annotations

import datetime as dt
from pathlib import Path

import pytest

from adrsite.model import load_collection, read_tree
from adrsite.scaffold import new_adr, next_number, slugify

REPO_TEMPLATE = Path(__file__).resolve().parent.parent / "adrs" / "_template"


@pytest.fixture
def adrs(tmp_path: Path) -> Path:
    d = tmp_path / "adrs"
    (d / "_template").mkdir(parents=True)
    for f in REPO_TEMPLATE.iterdir():
        (d / "_template" / f.name).write_text(f.read_text())
    return d


def test_slugify():
    assert slugify("Use 8N1 Framing!") == "use-8n1-framing"
    with pytest.raises(ValueError):
        slugify("!!!")


def test_new_adr_from_template_is_valid(adrs):
    path = new_adr(adrs, 'Use "quoted" framing', author="A. Ham", date=dt.date(2026, 1, 2))
    assert path.name == "0001-use-quoted-framing"
    adr = load_collection(read_tree(adrs)).adrs[path.name]
    assert adr.title == 'Use "quoted" framing'
    assert adr.date == "2026-01-02" and adr.authors == ["A. Ham"]


def test_numbers_increment_and_collisions_fail(adrs):
    (adrs / "0007-existing").mkdir()
    assert next_number(adrs) == 8
    assert new_adr(adrs, "Next").name == "0008-next"
    with pytest.raises(FileExistsError):
        new_adr(adrs, "Next", number=8)
