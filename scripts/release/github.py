"""Thin subprocess wrappers around git and the GitHub CLI."""

import json
import subprocess
from pathlib import Path
from typing import Any


def run(command: list[str], root: Path, capture: bool = False) -> str | None:
    """Run `command` in `root`, failing on a non-zero exit; return stdout if captured."""
    return subprocess.run(
        command,
        cwd=root,
        check=True,
        text=True,
        stdout=subprocess.PIPE if capture else None,
    ).stdout


def api(path: str, root: Path) -> Any:
    return json.loads(run(["gh", "api", path], root, capture=True))


def release_records(repo: str, root: Path) -> list[dict]:
    pages = json.loads(
        run(
            ["gh", "api", f"repos/{repo}/releases", "--paginate", "--slurp"],
            root,
            capture=True,
        )
    )
    return [release for page in pages for release in page]
