"""Create a new ADR folder from ``adrs/_template``."""

from __future__ import annotations

import datetime as dt
import re
from pathlib import Path

from .model import ADR_DIR_RE

TEMPLATE_DIR = "_template"


def slugify(title: str) -> str:
    slug = re.sub(r"[^a-z0-9]+", "-", title.lower()).strip("-")
    if not slug:
        raise ValueError(f"cannot derive a folder name from title {title!r}")
    return slug


def next_number(adrs_dir: Path) -> int:
    numbers = [
        int(m.group("number"))
        for p in adrs_dir.iterdir()
        if p.is_dir() and (m := ADR_DIR_RE.match(p.name))
    ] if adrs_dir.is_dir() else []
    return max(numbers, default=0) + 1


def new_adr(adrs_dir: Path, title: str, *, author: str | None = None,
            date: dt.date | None = None, number: int | None = None) -> Path:
    """Copy the template into ``NNNN-slug/``, filling placeholders. Returns the new folder."""
    template = adrs_dir / TEMPLATE_DIR
    if not template.is_dir():
        raise FileNotFoundError(f"template folder not found: {template}")
    number = number or next_number(adrs_dir)
    target = adrs_dir / f"{number:04d}-{slugify(title)}"
    if target.exists():
        raise FileExistsError(f"{target} already exists")

    values = {
        "title": title.replace('"', '\\"'),
        "number": f"{number:04d}",
        "date": (date or dt.date.today()).isoformat(),
        "author": (author or "").replace('"', '\\"'),
    }
    target.mkdir(parents=True)
    for src in sorted(template.iterdir()):
        if src.is_file():
            text = src.read_text()
            for key, value in values.items():
                text = text.replace("{{" + key + "}}", value)
            (target / src.name).write_text(text)
    return target
