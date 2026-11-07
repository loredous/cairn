"""Generate Markdown pages (records, handbook, changelog) for Zensical to render.

Every function returns Markdown text. Paths are relative to the staged docs
root, so links between generated pages are ordinary relative ``.md`` links that
Zensical resolves.
"""

from __future__ import annotations

import posixpath
from typing import Any, Iterable

from .changelog import CHANGE_KINDS, Change, Entry
from .model import ADR, Collection, Decision, DecisionRef, Interpretation

ADRS_DIR = "adrs"
HANDBOOK_PAGE = "handbook.md"
CHANGELOG_PAGE = "changelog.md"
RECORDS_INDEX = f"{ADRS_DIR}/index.md"


def adr_page(adr_id: str) -> str:
    return f"{ADRS_DIR}/{adr_id}.md"


def _link(from_page: str, to_page: str, anchor: str | None = None) -> str:
    rel = posixpath.relpath(to_page, posixpath.dirname(from_page) or ".")
    return f"{rel}#{anchor}" if anchor else rel


def _ref_link(from_page: str, ref: DecisionRef) -> str:
    return f"[`{ref}`]({_link(from_page, adr_page(ref.adr), f'd-{ref.decision}')})"


def _indent(text: str, prefix: str = "    ") -> str:
    return "\n".join(prefix + line if line.strip() else "" for line in text.splitlines())


def _quote_title(text: str) -> str:
    return " ".join(text.split()).replace('"', "&quot;")


def _one_line(text: str) -> str:
    return " ".join(str(text).split())


def strength_badge(strength: str | None) -> str:
    if not strength:
        return ""
    cls = strength.lower().replace("_", "-")
    return f'<span class="adr-badge adr-strength adr-strength-{cls}">{strength.replace("_", " ")}</span>'


def state_badge(state: str) -> str:
    if state == "active":
        return '<span class="adr-badge adr-state adr-state-active">in force</span>'
    return f'<span class="adr-badge adr-state adr-state-{state}">{state}</span>'


def status_badge(status: str) -> str:
    return f'<span class="adr-badge adr-status adr-status-{status}">{status}</span>'


def _adr_number(adr: ADR) -> str:
    return f"{adr.number:04d}"


_ACRONYMS = {"adr", "adrs", "ci", "phy", "fec", "arq", "ofdm", "ptt", "kiss", "sdr",
             "api", "pdu", "mcs", "fm", "dcd", "papr", "harq"}


def category_title(category: str) -> str:
    """'physical-layer' -> 'Physical layer', 'adr-format' -> 'ADR format'."""
    words = [w.upper() if w in _ACRONYMS else w for w in category.split("-")]
    words[0] = words[0][:1].upper() + words[0][1:]
    return " ".join(words)


def category_badge(category: str) -> str:
    return f'<span class="adr-badge adr-category-badge">{category_title(category)}</span>'


# --------------------------------------------------------------------------- #
# Decisions
# --------------------------------------------------------------------------- #


def render_interpretation(i: Interpretation, anchor_prefix: str = "") -> str:
    label, admonition = ("Erratum", "bug") if i.errata else ("Interpretation", "question")
    title = label + (f" ({i.date})" if i.date else "")
    body = f"**{_one_line(i.question)}**\n\n{i.interpretation}"
    return f'<span id="i-{anchor_prefix}{i.slug}"></span>\n\n!!! {admonition} "{title}"\n\n{_indent(body)}\n'


def decision_title(d: Decision) -> str:
    """Title that shows the full reference on hover and copies it on click (adr.js)."""
    ref = str(d.ref)
    return (f'<span class="adr-ref" data-ref="{ref}" role="button" tabindex="0" '
            f'aria-label="{_quote_title(d.title)} (copy reference {ref})">{d.title}</span>')


def render_decision(c: Collection, d: Decision, page: str, *, level: int = 3,
                    show_category: bool = True, anchor_prefix: str = "") -> str:
    state = c.state_of(d)
    hashes = "#" * level
    meta = [strength_badge(d.strength), state_badge(state)]
    if show_category:
        meta.append(category_badge(d.category))
    meta += [f'<span class="adr-badge adr-tag">#{t}</span>' for t in d.tags]
    out = [
        # data-toc-label keeps the sidebar entry plain text (no copy-on-click span).
        f'{hashes} {decision_title(d)} {{ #d-{anchor_prefix}{d.slug} '
        f'data-toc-label="{_quote_title(d.title)}" }}',
        "",
        " ".join(m for m in meta if m),
        "",
        d.statement,
        "",
    ]
    if d.rationale:
        out += ['!!! info "Rationale"', "", _indent(d.rationale), ""]

    lineage: list[str] = []
    if d.supersedes:
        verb = "Rescinds" if d.is_rescission else "Supersedes"
        lineage.append(f"{verb} {_ref_link(page, d.supersedes)}")
    by = c.superseded_by.get(d.ref)
    if by is not None:
        verb = "Rescinded by" if by.is_rescission else "Superseded by"
        lineage.append(f"{verb} {_ref_link(page, by.ref)}")
    pending = c.pending_supersession.get(d.ref)
    if pending is not None:
        verb = "rescinded" if pending.is_rescission else "superseded"
        lineage.append(
            f"Would be {verb} by {_ref_link(page, pending.ref)} "
            f"(ADR is {c.status_of(c.adrs[pending.adr])})"
        )
    if lineage:
        out += [f"- {line}" for line in lineage] + [""]

    for i in c.adrs[d.adr].interpretations_for(d.slug):
        out += [render_interpretation(i, anchor_prefix)]
    return "\n".join(out).rstrip() + "\n"


# --------------------------------------------------------------------------- #
# Pages
# --------------------------------------------------------------------------- #


def render_adr(c: Collection, adr: ADR) -> str:
    page = adr_page(adr.id)
    rows = [("Status", status_badge(c.status_of(adr)))]
    if adr.date:
        rows.append(("Date", adr.date))
    if adr.authors:
        rows.append(("Authors", ", ".join(adr.authors)))
    rows.append(("Folder", f"`adrs/{adr.id}/`"))
    in_force = sum(1 for d in adr.decisions if c.state_of(d) == "active")
    rows.append(("Decisions", f"{len(adr.decisions)} ({in_force} in force)"))

    out = [
        "---",
        f'title: "ADR {_adr_number(adr)}: {_quote_title(adr.title)}"',
        "---",
        "",
        f"# ADR {_adr_number(adr)}: {adr.title}",
        "",
        "| | |",
        "|---|---|",
        *[f"| **{k}** | {v} |" for k, v in rows],
        "",
        adr.prose.strip(),
        "",
        "## Decisions { #decisions }",
        "",
    ]
    if adr.decisions:
        out += [render_decision(c, d, page) for d in adr.decisions]
    else:
        out.append("*This ADR records no normative decisions.*\n")
    return "\n".join(out)


def render_records_index(c: Collection) -> str:
    page = RECORDS_INDEX
    out = [
        "# Records",
        "",
        "Every architecture decision record, including superseded and rejected ones. "
        f"See the [handbook]({_link(page, HANDBOOK_PAGE)}) for only the decisions in force.",
        "",
    ]
    adrs = c.sorted_adrs()
    if not adrs:
        return "\n".join(out + ["*No ADRs yet.*", ""])
    out += [
        "| ADR | Title | Status | Date | Decisions | In force |",
        "|---|---|---|---|--:|--:|",
    ]
    for adr in adrs:
        in_force = sum(1 for d in adr.decisions if c.state_of(d) == "active")
        out.append(
            f"| `{_adr_number(adr)}` | [{adr.title}]({_link(page, adr_page(adr.id))}) "
            f"| {status_badge(c.status_of(adr))} | {adr.date or ''} | {len(adr.decisions)} | {in_force} |"
        )
    return "\n".join(out) + "\n"


def handbook_anchor_prefix(d: Decision) -> str:
    """Handbook anchors include the ADR number, since slugs repeat across ADRs."""
    return d.adr.split("-", 1)[0] + "-"


def _filter_bar(c: Collection, tags: list[str]) -> str:
    """Filter controls; adr.js wires them up (everything shows without JS)."""
    chips = "".join(
        f'<button type="button" class="adr-tag-chip" data-tag="{t}" aria-pressed="false">#{t}</button>'
        for t in tags
    )
    return (
        '<div class="adr-filter" id="adr-filter" hidden>\n'
        '<label class="adr-filter-inforce"><input type="checkbox" id="adr-in-force-only" checked> '
        "Only decisions in force</label>\n"
        f'<div class="adr-filter-tags" role="group" aria-label="Filter by tag">{chips}'
        '<button type="button" class="adr-tag-clear" id="adr-tag-clear" hidden>Clear tags</button></div>\n'
        '<p class="adr-filter-count" id="adr-filter-count" aria-live="polite"></p>\n'
        "</div>\n"
    )


def render_handbook(c: Collection) -> str:
    page = HANDBOOK_PAGE
    decisions = list(c.all_decisions())
    active = c.active_decisions()
    out = [
        "# Handbook",
        "",
        f"The decisions recorded for Cairn, organized by category. **{len(active)}** "
        f"decision{'s' if len(active) != 1 else ''} {'are' if len(active) != 1 else 'is'} currently in force: "
        "the ADR is accepted (merged into main), the decision is not a rescission record, and no decision in an "
        "accepted ADR supersedes or rescinds it. Uncheck *Only decisions in force* to include "
        "superseded, rescinded, and not-yet-accepted decisions; select tags to narrow further. "
        f"Every ADR is also in the [records]({_link(page, RECORDS_INDEX)}).",
        "",
    ]
    if not decisions:
        return "\n".join(out + ["*No decisions have been recorded yet.*", ""])

    tags = sorted({t for d in decisions for t in d.tags})
    out += [_filter_bar(c, tags), ""]

    counts = {s: sum(1 for d in active if d.strength == s) for s in
              ("MUST", "MUST_NOT", "SHOULD", "SHOULD_NOT", "MAY")}
    out += [" ".join(f"{strength_badge(s)} {n}" for s, n in counts.items() if n), ""]

    for category in c.categories():
        members = [d for d in decisions if d.category == category]
        out += [
            f'<section class="adr-category" data-category="{category}" markdown="1">',
            "",
            f"## {category_title(category)} {{ #cat-{category} }}",
            "",
        ]
        for d in members:
            out += [
                f'<div class="adr-entry" data-state="{c.state_of(d)}" '
                f'data-tags="{" ".join(d.tags)}" markdown="1">',
                "",
                render_decision(c, d, page, show_category=False,
                                anchor_prefix=handbook_anchor_prefix(d)),
                "</div>",
                "",
            ]
        out += ["</section>", ""]
    return "\n".join(out)


def _format_value(v: Any) -> str:
    if v is None:
        return "*none*"
    if isinstance(v, bool):
        return f"`{str(v).lower()}`"
    if isinstance(v, (list, tuple)):
        return ", ".join(f"`{x}`" for x in v) if v else "*none*"
    text = _one_line(v)
    if len(text) > 80 or "`" in text:
        return f"“{text}”"
    return f"`{text}`"


def _change_target(page: str, ch: Change) -> str:
    if ch.subject:
        prefix = "i" if ch.kind.startswith(("interpretation", "erratum")) else "d"
        label = f"{ch.adr}/{ch.subject}"
        if ch.kind.endswith("-removed"):
            return f"`{label}`"
        return f"[`{label}`]({_link(page, adr_page(ch.adr), f'{prefix}-{ch.subject}')})"
    if ch.kind == "adr-removed":
        return f"`{ch.adr}`"
    return f"[`{ch.adr}`]({_link(page, adr_page(ch.adr))})"


def render_changes(changes: Iterable[Change], page: str) -> str:
    lines: list[str] = []
    for ch in changes:
        line = f"- **{CHANGE_KINDS[ch.kind]}** {_change_target(page, ch)}"
        if ch.detail:
            detail = ch.detail
            if ch.kind in ("decision-supersedes", "decision-rescinds") and "/" in detail:
                adr_id, _, slug = detail.partition("/")
                detail = _ref_link(page, DecisionRef(adr_id, slug))
            else:
                detail = _one_line(detail)
            line += f" — {detail}"
        lines.append(line)
        for name, old, new in ch.fields:
            lines.append(f"    - `{name}`: {_format_value(old)} → {_format_value(new)}")
    return "\n".join(lines) + "\n"


def render_changelog(entries: list[Entry], *, repo_url: str | None = None) -> str:
    page = CHANGELOG_PAGE
    out = ["# Changelog", "", "Changes to the decision record, grouped by commit, newest first.", ""]
    if not entries:
        return "\n".join(out + ["*No committed ADR history yet.*", ""])
    for e in entries:
        anchor = f"c-{e.sha[:10]}" if e.sha else "uncommitted"
        out += [f"## {_one_line(e.subject)} {{ #{anchor} }}", ""]
        meta = []
        if e.sha:
            sha = f"`{e.short_sha}`"
            meta.append(f"[{sha}]({repo_url.rstrip('/')}/commit/{e.sha})" if repo_url else sha)
        else:
            meta.append("*not yet committed*")
        if e.author:
            meta.append(e.author)
        if e.date:
            meta.append(e.date[:10])
        out += [" · ".join(meta), "{ .adr-commit-meta }", "", render_changes(e.changes, page)]
    return "\n".join(out)
