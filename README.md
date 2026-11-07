# Cairn

An amateur radio digital protocol.

## Repository layout

| Path | Contents |
|------|----------|
| `docs/` | Documentation (Markdown), published to the site |
| `adrs/` | Architecture decision records, one folder per ADR |
| `adrs/_template/` | Template used by `scripts/adr.py new` |
| `scripts/` | Utility scripts (`adr.py`, `build_site.py`) |
| `tools/adrsite/` | Python library for ADR validation, scaffolding, and site generation; JSON Schemas live in `schemas/` |
| `zensical.toml` | Base [Zensical](https://zensical.org) config for the site |
| `tests/` | Tests for the tooling |

## ADRs

Each ADR folder holds a `prose.md` (frontmatter and narrative), a `decisions.yaml`
(structured, citable rules with RFC 2119 strengths), and an optional
`interpretations.yaml`. A `decisions.yaml` is frozen once merged into `main`;
decisions change only through new ADRs that supersede or rescind them. See [ADR 0001](adrs/0001-record-architecture-decisions/prose.md)
for the format, and [docs/writing-adrs.md](docs/writing-adrs.md) for the workflow.

Requires [uv](https://docs.astral.sh/uv/).

```sh
uv run scripts/adr.py new "Title of decision"   # scaffold a new ADR
uv run scripts/adr.py validate                  # validate adrs/
uv run scripts/adr.py check-immutable --base origin/main  # no frozen decisions changed
uv run scripts/build_site.py                    # build the site into _site/ with Zensical
uv run pytest                                   # test the tooling
```

## CI

- **Validate ADRs** (`.github/workflows/validate-adrs.yml`) runs on any change
  to ADRs or their tooling: validation, the immutability check, and tests.
- **ADR approval** (`.github/workflows/adr-approval.yml`) passes only when a
  majority of maintainers (Maintain or Admin role) agree with a pull request
  that changes an ADR. Make `maintainer-majority` a required status check.
- **Publish site** (`.github/workflows/publish-site.yml`) builds the site from
  `docs/` and `adrs/` and deploys it to GitHub Pages on every push to `main`.
  Enable it under *Settings → Pages → Source: GitHub Actions*.

## License

| What | License |
|------|---------|
| Code: `scripts/`, `tools/`, `tests/`, and the reference implementation and simulation as they land | [Apache-2.0](LICENSE) |
| Specification and documentation: `docs/`, `adrs/`, and published test vectors | [CC BY 4.0](LICENSE-CC-BY-4.0) |
| Compression dictionaries, when published | CC0 (SPEC §12.2, §15) |

Copyright 2026 the Cairn contributors. Contributions are accepted under the
license of the files they change (Apache-2.0 §5 for code).

