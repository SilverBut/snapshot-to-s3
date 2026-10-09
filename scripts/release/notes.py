"""CHANGELOG.md release-notes sections."""

import re
from pathlib import Path

from scripts.release.version import parse_version

NOTES_PATH = Path("CHANGELOG.md")
# Inserted into every prepared section; publishing refuses notes that still contain it.
NOTES_MARKER = "<!-- RELEASE_NOTES_NEED_REVIEW -->"
NOTES_TEMPLATE = (
    "### Added\n<!-- Describe new capabilities. -->\n\n"
    "### Fixed\n<!-- Describe fixes. -->\n\n"
    "### Upgrade notes\n<!-- Describe compatibility and migration. -->\n\n"
    "### Known issues\n<!-- Describe limitations, or state none. -->"
)
SECTION_HEADING = re.compile(r"^## \[([^\]]+)\].*$", re.MULTILINE)
UNRELEASED = re.compile(r"(?ms)^## \[Unreleased\][^\n]*\n(.*?)(?=^## \[|\Z)")


def notes_section(text: str, version: str, reviewed: bool = True) -> str:
    """Return the body of the single `## [version]` section.

    With `reviewed`, the section must be free of the review marker and contain
    something other than comments and headings.
    """
    parse_version(version)
    headings = list(SECTION_HEADING.finditer(text))
    matches = [(i, h) for i, h in enumerate(headings) if h[1] == version]
    if len(matches) != 1:
        raise ValueError(f"expected one CHANGELOG section for {version}")
    index, heading = matches[0]
    end = headings[index + 1].start() if index + 1 < len(headings) else len(text)
    section = text[heading.end() : end].strip()
    if reviewed:
        if NOTES_MARKER in section:
            raise ValueError("release notes still require maintainer review")
        content = re.sub(r"<!--.*?-->", "", section, flags=re.DOTALL)
        content = re.sub(r"(?m)^#+.*$", "", content).strip()
        if not content:
            raise ValueError("release notes are empty")
    return section


def prepare_notes(text: str, target: str) -> str:
    """Move Unreleased content (or a template) into a new marked `target` section."""
    if re.search(rf"(?m)^## \[{re.escape(target)}\]", text):
        raise ValueError(f"CHANGELOG already contains {target}")
    unreleased = UNRELEASED.search(text)
    if unreleased:
        content = unreleased[1].strip()
        remaining = text[: unreleased.start()] + text[unreleased.end() :]
    else:
        content = ""
        remaining = text
    if remaining.startswith("# Changelog"):
        remaining = remaining[len("# Changelog") :].lstrip()
    if not content:
        content = NOTES_TEMPLATE
    return (
        f"# Changelog\n\n## [Unreleased]\n\n## [{target}]\n\n"
        f"{NOTES_MARKER}\n\n{content}\n\n{remaining}"
    ).rstrip() + "\n"


def read_notes(root: Path) -> str:
    path = root / NOTES_PATH
    return path.read_text() if path.exists() else ""
