---
title: Record architecture decisions
date: 2026-11-05
authors: ["Jeremy Banker"]
---

## Context

Cairn is an amateur radio digital protocol. Its design will be shaped by many
decisions about modulation, framing, addressing, error correction, and
operating practice. Those decisions must be discoverable, reviewable, and
traceable over time, and implementers need one authoritative answer to the
question *"what are the rules right now?"*

Classic prose ADRs capture reasoning well but bury the normative content, so
there is no reliable way to assemble the rules currently in force. A single
living specification has the opposite problem: it is easy to read but loses
the reasoning and history behind each rule.

## Decision

We record architecture decisions as ADRs that pair prose reasoning with
structured, individually addressable decisions. The collection is validated
automatically and published as a static site.

### Format

Each ADR is a folder in `adrs/` named `NNNN-short-kebab-title`, where `NNNN`
is a unique, zero-padded sequence number. The folder contains:

| File | Required | Purpose |
|------|----------|---------|
| `prose.md` | yes | YAML frontmatter (`title`, `date`, `authors`) followed by the narrative sections: Context, Decision, Consequences, Alternatives Considered. |
| `decisions.yaml` | yes | A list of normative decisions. |
| `interpretations.yaml` | no | A list of clarifications of how specific decisions apply. |

An ADR has no stored status. It is **proposed** while it is only on a branch
or in a pull request, and **accepted** once it is on `main`.

A decision looks like this:

```yaml
- slug: frame-length-limit          # stable id, unique within the ADR
  title: Frames are at most 256 bytes
  category: link-layer              # one broad area; organizes the handbook
  strength: MUST                    # MUST | MUST_NOT | SHOULD | SHOULD_NOT | MAY
  statement: >-
    Transmitted frames MUST NOT exceed 256 bytes including the header.
  rationale: >-                     # optional
    Keeps worst-case airtime under the timeout used by digipeaters.
  supersedes:                       # optional
    adr: 0004-framing
    decision: frame-length-limit
  rescinded: false                  # optional; see "Changing decisions"
  tags: [framing]                   # optional; filters the handbook
```

A decision is cited as `<adr-folder>/<slug>`, for example
`0001-record-architecture-decisions/stable-slugs`.

An interpretation answers a question about one decision in the same ADR:

```yaml
- slug: header-counts-toward-limit
  decision: frame-length-limit
  question: Does the FEC trailer count toward the frame length?
  interpretation: No. The limit applies to the frame before FEC encoding.
  date: 2026-11-05                  # optional
```

An interpretation with `errata: true` is an **erratum**: it corrects a typo or
editorial error without changing the decision's meaning.

```yaml
- slug: fix-digipeater-typo
  decision: frame-length-limit
  errata: true
  question: The rationale says "digipeter".
  interpretation: It should read "digipeater".
```

The exact field formats are defined by the JSON Schemas in
`tools/adrsite/schemas/`, and new ADRs are scaffolded from `adrs/_template/`.

### Changing decisions

Once an ADR's `decisions.yaml` is merged into `main` it is frozen: it is never
edited again, not even for typos. Every change happens in a new ADR.

- To **replace** a decision, the new ADR adds a decision whose `supersedes`
  names the old one.
- To **withdraw** a decision with no replacement, the new ADR adds a
  *rescission record*: an entry with `supersedes` naming the old decision and
  `rescinded: true`, whose statement explains why.

```yaml
- slug: drop-frame-length-limit
  title: Rescind the 256-byte frame limit
  category: link-layer
  statement: The limit is removed; frame length is now negotiated.
  supersedes:
    adr: 0004-framing
    decision: frame-length-limit
  rescinded: true
```

Typos and other corrections that do not change a decision's meaning are
recorded as errata in `interpretations.yaml` instead of a new ADR.

A decision is **in force** when its ADR is accepted, it is not a rescission
record, and no decision in an accepted ADR supersedes or rescinds it.
`prose.md` and `interpretations.yaml` remain editable.

### Approval

An ADR, or any change to one, is approved when a majority of the maintainers
(collaborators with the Maintain or Admin role on the GitHub repository) agree
to its pull request and it is merged into `main`. Merging is the approval, so
status is derived rather than stored: whatever is on `main` is accepted. A
required GitHub Actions check counts maintainer approvals on every pull request
that changes an ADR.

### Validation

`scripts/adr.py validate` is the reference validator. It checks every ADR
against the schemas and checks references between them: unique numbers and
slugs, valid `supersedes` targets, no supersession cycles, and interpretations
that point at real decisions. `scripts/adr.py check-immutable` verifies that
no frozen `decisions.yaml` has changed. A GitHub Actions workflow runs both on
every push and pull request that touches `adrs/`.

### Publishing

On every push to `main`, a GitHub Actions workflow builds a static site from
`docs/` and `adrs/` and publishes it to GitHub Pages. The site has a page for
each ADR, a **handbook** of decisions organized by category (filterable to
the decisions in force and by tag), and a **changelog** generated from git history that shows what each commit changed
in the decision record.

## Consequences

- Implementers can read the handbook alone to know the current rules.
- Every rule has a stable identifier that can be used in code comments, test
  names, and discussion.
- History is preserved: superseded and rescinded decisions stay visible and
  link to their replacements, and every change to a rule is its own ADR.
- Even trivial corrections to a decision require a new ADR. This is
  deliberate friction.
- Authors must write discrete, testable statements instead of pure prose,
  which is more work up front.
- The changelog depends on git history, so the publishing workflow must check
  out the full history, not a shallow clone.

## Alternatives Considered

- **A single living specification document.** Easy to read, but loses the
  reasoning behind each choice and makes it hard to see what changed and when.
- **Classic prose-only ADRs (Nygard format).** Captures reasoning, but the
  normative content cannot be extracted or assembled into a handbook.
- **An off-the-shelf static site generator (MkDocs, Hugo).** Renders the prose
  well but understands neither supersession nor the semantic changelog, so the
  hard parts would still need custom code.
