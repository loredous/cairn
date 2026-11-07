# Writing ADRs

The ADR format, and how ADRs are validated and published, is itself defined by
ADR 0001 (`adrs/0001-record-architecture-decisions/`). This page is a practical
summary.

## Create an ADR

```sh
uv run scripts/adr.py new "Use 8N1 framing"
```

This creates `adrs/NNNN-use-8n1-framing/` from `adrs/_template/`. Then:

1. Fill in the sections of `prose.md`.
2. Replace the example in `decisions.yaml` with your decisions. Each one needs
   a `slug`, `title`, `category`, `strength` (`MUST`, `MUST_NOT`, `SHOULD`,
   `SHOULD_NOT`, `MAY`), and `statement`. The handbook is organized by
   `category` and can be filtered by `tags`, so reuse existing ones where
   they fit. Get these right before merging: once merged into
   `main`, `decisions.yaml` is frozen.
3. Optionally add an `interpretations.yaml`.
4. Validate it, then open a pull request.

```sh
uv run scripts/adr.py validate          # defaults to adrs/
uv run scripts/adr.py validate path/to/other/adrs
```

Editors with the YAML language server get completion and inline validation
from the `# yaml-language-server: $schema=...` line at the top of each YAML file.

## Change a decision

A `decisions.yaml` that has been merged into `main` is never edited again, not
even for typos. CI rejects any change to one
(`uv run scripts/adr.py check-immutable --base origin/main` runs the same check
locally). Every change goes in a **new ADR**:

- **Fix a typo that doesn't change the meaning:** add an *erratum* to the
  same ADR's `interpretations.yaml` (no new ADR needed).
  ```yaml
  - slug: fix-old-slug-typo
    decision: old-slug
    errata: true
    question: The statement says "recieve".
    interpretation: It should read "receive".
  ```
- **Replace a decision:** add a decision that names it in `supersedes`.
  ```yaml
  - slug: new-rule
    title: The new rule
    category: link-layer
    strength: MUST
    statement: ...
    supersedes:
      adr: 0003-old-adr-folder
      decision: old-slug
  ```
- **Withdraw a decision with no replacement:** add a *rescission record*, an
  entry with `rescinded: true` and `supersedes` naming the old decision.
  `strength` can be omitted.
  ```yaml
  - slug: drop-old-rule
    title: Rescind the old rule
    category: link-layer
    statement: Why the rule no longer applies.
    supersedes:
      adr: 0003-old-adr-folder
      decision: old-slug
    rescinded: true
  ```

Either change takes effect once the new ADR is accepted (merged into `main`).
`prose.md` and `interpretations.yaml` remain editable.

## Approval

There is no status field. An ADR is **proposed** while it lives on a branch or
in a pull request, and **accepted** once it is merged into `main`.

A pull request that changes an ADR can only be merged once a majority of the
maintainers (collaborators with the Maintain or Admin role) agree. Approve the
pull request to agree; if a maintainer opened it, that counts as their
agreement. The **ADR approval** check counts approvals and must pass before
merging.

## Preview the site

The site is built with [Zensical](https://zensical.org). The build script
generates Markdown for the records, handbook, and changelog, stages it with
`docs/` in `build/site-src/`, and renders it using the base config in
`zensical.toml`.

```sh
uv run scripts/build_site.py      # writes _site/
python3 -m http.server -d _site
```

The footer shows the commit the site was built from, or `DEVELOP` for local
builds. Uncommitted ADR changes show up at the top of the changelog as
"Uncommitted changes" so you can preview them before committing. ADRs that are
not yet on `origin/main` show as proposed; pass `--main-ref` to compare
against a different ref.

### Pull request previews

Every pull request also gets a built copy of the site. The **Site preview**
workflow builds it and attaches it to the run as an artifact, and posts a
comment on the pull request with a download link. Download the zip and serve
it the same way:

```sh
unzip site-preview-pr-N.zip -d cairn-preview
python3 -m http.server -d cairn-preview 8000
```

A banner at the top of each page names the pull request and the commit the
preview was built from. Nothing is deployed, and the published site changes
only when a pull request is merged. Pass `--preview-pr N` to
`scripts/build_site.py` to add the same banner to a local build.
