"""Data model, loading, and validation for the ADR collection.

The collection is loaded from a plain mapping of ``relative path -> file text``
(paths relative to the ``adrs/`` directory). That keeps loading independent of
where the files come from: the working tree for builds, or an arbitrary git
commit for the changelog.
"""

from __future__ import annotations

import json
import re
from dataclasses import dataclass, field
from functools import cached_property
from importlib import resources
from pathlib import Path
from typing import Any, Iterable, Mapping

import jsonschema
import yaml

# RFC 2119 requirement levels, strongest obligation first.
STRENGTHS = ("MUST", "MUST_NOT", "SHOULD", "SHOULD_NOT", "MAY")
# An ADR's status is derived, never stored (see status.py): "accepted" once it is
# merged into main, "proposed" before that, and "superseded" when it is accepted
# but none of its decisions remain in force.
ADR_STATUSES = ("proposed", "accepted", "superseded")

ADR_DIR_RE = re.compile(r"^(?P<number>\d{4})-[a-z0-9]+(?:-[a-z0-9]+)*$")
PROSE_FILE = "prose.md"
DECISIONS_FILE = "decisions.yaml"
INTERPRETATIONS_FILE = "interpretations.yaml"


class ValidationError(Exception):
    def __init__(self, errors: list[str]):
        self.errors = errors
        super().__init__("\n".join(errors))


def _schema(name: str) -> dict[str, Any]:
    text = resources.files("adrsite").joinpath("schemas", name).read_text()
    return json.loads(text)


# --------------------------------------------------------------------------- #
# Model
# --------------------------------------------------------------------------- #


@dataclass(frozen=True, order=True)
class DecisionRef:
    adr: str
    decision: str

    def __str__(self) -> str:
        return f"{self.adr}/{self.decision}"


@dataclass
class Decision:
    adr: str
    slug: str
    title: str
    strength: str | None  # None only for rescission records
    statement: str
    rationale: str | None = None
    supersedes: DecisionRef | None = None
    rescinded: bool = False
    tags: list[str] = field(default_factory=list)
    category: str = ""

    @property
    def ref(self) -> DecisionRef:
        return DecisionRef(self.adr, self.slug)

    @property
    def is_rescission(self) -> bool:
        """A record that withdraws ``supersedes`` with no replacement."""
        return self.rescinded

    def as_comparable(self) -> dict[str, Any]:
        """Field values used when diffing two versions of a decision."""
        return {
            "title": self.title,
            "category": self.category,
            "strength": self.strength,
            "statement": self.statement,
            "rationale": self.rationale,
            "supersedes": str(self.supersedes) if self.supersedes else None,
            "rescinded": self.rescinded,
            "tags": list(self.tags),
        }


@dataclass
class Interpretation:
    adr: str
    slug: str
    decision: str
    question: str
    interpretation: str
    date: str | None = None
    errata: bool = False  # a meaning-neutral correction (typo, editorial error)

    def as_comparable(self) -> dict[str, Any]:
        return {
            "decision": self.decision,
            "question": self.question,
            "interpretation": self.interpretation,
            "date": self.date,
            "errata": self.errata,
        }


@dataclass
class ADR:
    id: str  # folder name, e.g. "0001-record-architecture-decisions"
    number: int
    title: str
    date: str | None
    authors: list[str]
    prose: str  # markdown body of prose.md, frontmatter removed
    decisions: list[Decision] = field(default_factory=list)
    interpretations: list[Interpretation] = field(default_factory=list)

    def decision(self, slug: str) -> Decision | None:
        return next((d for d in self.decisions if d.slug == slug), None)

    def interpretations_for(self, slug: str) -> list[Interpretation]:
        return [i for i in self.interpretations if i.decision == slug]


@dataclass
class Collection:
    adrs: dict[str, ADR]
    proposed: frozenset[str] = frozenset()  # ADR ids not yet merged into main

    def is_accepted(self, adr_id: str) -> bool:
        return adr_id not in self.proposed

    def status_of(self, adr: ADR) -> str:
        if not self.is_accepted(adr.id):
            return "proposed"
        if adr.decisions and not any(self.state_of(d) == "active" for d in adr.decisions):
            return "superseded"
        return "accepted"

    def sorted_adrs(self) -> list[ADR]:
        return sorted(self.adrs.values(), key=lambda a: (a.number, a.id))

    def all_decisions(self) -> Iterable[Decision]:
        for adr in self.sorted_adrs():
            yield from adr.decisions

    def decision(self, ref: DecisionRef) -> Decision | None:
        adr = self.adrs.get(ref.adr)
        return adr.decision(ref.decision) if adr else None

    @cached_property
    def superseded_by(self) -> dict[DecisionRef, Decision]:
        """Map of decision -> the decision that supersedes it.

        A supersession only takes effect once the superseding decision's ADR
        is accepted (merged); a merely *proposed* replacement retires nothing.
        """
        result: dict[DecisionRef, Decision] = {}
        for d in self.all_decisions():
            if d.supersedes and self.is_accepted(d.adr):
                result[d.supersedes] = d
        return result

    @cached_property
    def pending_supersession(self) -> dict[DecisionRef, Decision]:
        """Supersessions proposed by ADRs that are not (yet) accepted."""
        result: dict[DecisionRef, Decision] = {}
        for d in self.all_decisions():
            if d.supersedes and not self.is_accepted(d.adr):
                result[d.supersedes] = d
        return result

    def state_of(self, d: Decision) -> str:
        """One of: active, rescission, rescinded, superseded, proposed."""
        if d.is_rescission:
            return "rescission"
        by = self.superseded_by.get(d.ref)
        if by is not None:
            return "rescinded" if by.is_rescission else "superseded"
        if not self.is_accepted(d.adr):
            return "proposed"
        return "active"

    def categories(self) -> list[str]:
        """Categories in order of first appearance (ADR order, then file order)."""
        return list(dict.fromkeys(d.category for d in self.all_decisions()))

    def active_decisions(self) -> list[Decision]:
        return [d for d in self.all_decisions() if self.state_of(d) == "active"]


# --------------------------------------------------------------------------- #
# Loading
# --------------------------------------------------------------------------- #


def read_tree(adrs_dir: Path) -> dict[str, str]:
    """Read the ADR files from disk into a ``relative path -> text`` mapping."""
    files: dict[str, str] = {}
    if not adrs_dir.is_dir():
        return files
    for path in sorted(adrs_dir.glob("*/*")):
        if path.is_file():
            files[path.relative_to(adrs_dir).as_posix()] = path.read_text()
    return files


def split_frontmatter(text: str) -> tuple[dict[str, Any], str]:
    if not text.startswith("---"):
        return {}, text
    parts = re.split(r"^---[ \t]*$", text, maxsplit=2, flags=re.MULTILINE)
    if len(parts) < 3:
        return {}, text
    meta = yaml.safe_load(parts[1]) or {}
    if not isinstance(meta, dict):
        raise ValueError("frontmatter must be a mapping")
    return meta, parts[2].lstrip("\n")


def _as_str(value: Any) -> str | None:
    return None if value is None else str(value)


def load_collection(files: Mapping[str, str], *, strict: bool = True,
                    proposed: Iterable[str] = ()) -> Collection:
    """Build a Collection from a ``relative path -> text`` mapping.

    Folders starting with ``_`` or ``.`` (e.g. ``_template``) are ignored.
    ``proposed`` names the ADRs not yet merged into main (see status.py);
    every other ADR is treated as accepted.

    With ``strict=True`` any problem raises ValidationError listing every
    error found. With ``strict=False`` malformed ADRs are skipped, which is
    what the changelog wants when replaying old commits.
    """
    errors: list[str] = []
    by_dir: dict[str, dict[str, str]] = {}
    for rel, text in files.items():
        parts = rel.split("/")
        if len(parts) != 2 or parts[0].startswith(("_", ".")):
            continue
        by_dir.setdefault(parts[0], {})[parts[1]] = text

    decision_validator = jsonschema.Draft202012Validator(_schema("decisions.schema.json"))
    interp_validator = jsonschema.Draft202012Validator(_schema("interpretations.schema.json"))
    prose_validator = jsonschema.Draft202012Validator(_schema("prose-frontmatter.schema.json"))

    adrs: dict[str, ADR] = {}
    for dirname in sorted(by_dir):
        adr_files = by_dir[dirname]
        adr_errors: list[str] = []
        m = ADR_DIR_RE.match(dirname)
        if not m:
            adr_errors.append(
                f"{dirname}: folder name must look like 0001-lower-kebab-title"
            )

        def check(validator: jsonschema.Draft202012Validator, data: Any, where: str) -> None:
            for err in sorted(validator.iter_errors(data), key=lambda e: list(e.path)):
                loc = "/".join(str(p) for p in err.path) or "(root)"
                adr_errors.append(f"{dirname}/{where}: {loc}: {err.message}")

        # prose.md
        meta: dict[str, Any] = {}
        body = ""
        if PROSE_FILE not in adr_files:
            adr_errors.append(f"{dirname}: missing {PROSE_FILE}")
        else:
            try:
                meta, body = split_frontmatter(adr_files[PROSE_FILE])
            except (yaml.YAMLError, ValueError) as e:
                adr_errors.append(f"{dirname}/{PROSE_FILE}: bad frontmatter: {e}")
            check(prose_validator, _jsonable(meta), PROSE_FILE)

        # decisions.yaml
        raw_decisions: list[dict[str, Any]] = []
        if DECISIONS_FILE not in adr_files:
            adr_errors.append(f"{dirname}: missing {DECISIONS_FILE}")
        else:
            raw_decisions = _load_yaml_list(adr_files[DECISIONS_FILE], f"{dirname}/{DECISIONS_FILE}", adr_errors)
            check(decision_validator, _jsonable(raw_decisions), DECISIONS_FILE)

        # interpretations.yaml (optional)
        raw_interps: list[dict[str, Any]] = []
        if INTERPRETATIONS_FILE in adr_files:
            raw_interps = _load_yaml_list(adr_files[INTERPRETATIONS_FILE], f"{dirname}/{INTERPRETATIONS_FILE}", adr_errors)
            check(interp_validator, _jsonable(raw_interps), INTERPRETATIONS_FILE)

        if adr_errors:
            errors.extend(adr_errors)
            continue

        decisions = [
            Decision(
                adr=dirname,
                slug=d["slug"],
                title=d["title"],
                strength=d.get("strength"),
                statement=d["statement"].strip(),
                rationale=_as_str(d.get("rationale")),
                supersedes=(
                    DecisionRef(d["supersedes"]["adr"], d["supersedes"]["decision"])
                    if d.get("supersedes")
                    else None
                ),
                rescinded=bool(d.get("rescinded", False)),
                tags=list(d.get("tags", [])),
                category=d["category"],
            )
            for d in raw_decisions
        ]
        interpretations = [
            Interpretation(
                adr=dirname,
                slug=i["slug"],
                decision=i["decision"],
                question=i["question"].strip(),
                interpretation=i["interpretation"].strip(),
                date=_as_str(i.get("date")),
                errata=bool(i.get("errata", False)),
            )
            for i in raw_interps
        ]
        adrs[dirname] = ADR(
            id=dirname,
            number=int(m.group("number")),  # type: ignore[union-attr]
            title=meta["title"],
            date=_as_str(meta.get("date")),
            authors=list(meta.get("authors", [])),
            prose=body,
            decisions=decisions,
            interpretations=interpretations,
        )

    collection = Collection(adrs, frozenset(proposed) & frozenset(adrs))
    errors.extend(cross_validate(collection))
    if errors and strict:
        raise ValidationError(errors)
    return collection


def _load_yaml_list(text: str, where: str, errors: list[str]) -> list[dict[str, Any]]:
    try:
        data = yaml.safe_load(text)
    except yaml.YAMLError as e:
        errors.append(f"{where}: invalid YAML: {e}")
        return []
    if data is None:
        return []
    return data


def _jsonable(data: Any) -> Any:
    """YAML parses bare dates into datetime.date; the schemas expect strings."""
    if isinstance(data, dict):
        return {k: _jsonable(v) for k, v in data.items()}
    if isinstance(data, list):
        return [_jsonable(v) for v in data]
    if hasattr(data, "isoformat"):
        return data.isoformat()
    return data


def cross_validate(c: Collection) -> list[str]:
    """Checks that span files or ADRs: uniqueness and reference integrity."""
    errors: list[str] = []
    numbers: dict[int, str] = {}
    for adr in c.sorted_adrs():
        if adr.number in numbers:
            errors.append(f"{adr.id}: ADR number {adr.number:04d} already used by {numbers[adr.number]}")
        numbers.setdefault(adr.number, adr.id)

        seen: set[str] = set()
        for d in adr.decisions:
            if d.slug in seen:
                errors.append(f"{adr.id}/{DECISIONS_FILE}: duplicate decision slug '{d.slug}'")
            seen.add(d.slug)

        seen_i: set[str] = set()
        for i in adr.interpretations:
            if i.slug in seen_i:
                errors.append(f"{adr.id}/{INTERPRETATIONS_FILE}: duplicate interpretation slug '{i.slug}'")
            seen_i.add(i.slug)
            if adr.decision(i.decision) is None:
                errors.append(
                    f"{adr.id}/{INTERPRETATIONS_FILE}: interpretation '{i.slug}' references "
                    f"unknown decision '{i.decision}'"
                )

    supersessors: dict[DecisionRef, Decision] = {}
    for d in c.all_decisions():
        if d.rescinded and not d.supersedes:
            errors.append(
                f"{d.ref}: 'rescinded: true' requires 'supersedes' naming the decision being rescinded"
            )
        if not d.supersedes:
            continue
        target = d.supersedes
        if target == d.ref:
            errors.append(f"{d.ref}: a decision cannot supersede itself")
            continue
        target_decision = c.decision(target)
        if target_decision is None:
            errors.append(f"{d.ref}: supersedes unknown decision '{target}'")
            continue
        if target_decision.is_rescission:
            errors.append(
                f"{d.ref}: cannot supersede rescission record '{target}'; "
                "to reinstate a rule, add it as a new decision"
            )
        if target in supersessors:
            errors.append(
                f"{d.ref}: '{target}' is already superseded or rescinded by {supersessors[target].ref}"
            )
        supersessors[target] = d

    # Detect supersession cycles (a -> b -> a).
    for start in supersessors:
        seen_refs = {start}
        cur = c.decision(start)
        while cur and cur.supersedes:
            if cur.supersedes in seen_refs:
                errors.append(f"{start}: supersession cycle detected")
                break
            seen_refs.add(cur.supersedes)
            cur = c.decision(cur.supersedes)
    return sorted(set(errors))
