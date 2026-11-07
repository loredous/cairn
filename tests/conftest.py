from __future__ import annotations

import subprocess
from pathlib import Path
from typing import Any

import pytest
import yaml


def prose(title: str = "An ADR", **extra: Any) -> str:
    meta = {"title": title, "date": "2026-11-05", **extra}
    return f"---\n{yaml.safe_dump(meta, sort_keys=False)}---\n\n## Context\n\nBecause.\n"


def decision(slug: str, strength: str | None = "MUST", **extra: Any) -> dict[str, Any]:
    d: dict[str, Any] = {"slug": slug, "title": f"Title of {slug}", "category": "general",
                         "statement": f"Statement of {slug}."}
    if strength is not None:
        d["strength"] = strength
    d.update(extra)
    return d


def adr_files(folder: str, decisions: list[dict[str, Any]], *,
              interpretations: list[dict[str, Any]] | None = None,
              title: str | None = None, body: str = "") -> dict[str, str]:
    files = {
        f"{folder}/prose.md": prose(title or folder) + body,
        f"{folder}/decisions.yaml": yaml.safe_dump(decisions, sort_keys=False),
    }
    if interpretations is not None:
        files[f"{folder}/interpretations.yaml"] = yaml.safe_dump(interpretations, sort_keys=False)
    return files


def write_files(root: Path, files: dict[str, str]) -> None:
    for rel, text in files.items():
        path = root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)


class GitRepo:
    def __init__(self, path: Path):
        self.path = path
        self.adrs = path / "adrs"
        self.adrs.mkdir()
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.name", "Test Author")
        self.git("config", "user.email", "test@example.invalid")
        self.git("config", "commit.gpgsign", "false")

    def git(self, *args: str) -> str:
        return subprocess.run(["git", "-C", str(self.path), *args], check=True,
                              capture_output=True, text=True).stdout

    def write(self, files: dict[str, str]) -> None:
        write_files(self.adrs, files)

    def commit(self, message: str) -> str:
        self.git("add", "-A")
        self.git("commit", "-q", "--allow-empty", "-m", message)
        return self.git("rev-parse", "HEAD").strip()


@pytest.fixture
def repo(tmp_path: Path) -> GitRepo:
    return GitRepo(tmp_path)
