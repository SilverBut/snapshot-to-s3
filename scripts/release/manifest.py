"""Read and rewrite the package version in Cargo.toml and Cargo.lock."""

import re
import tomllib
from pathlib import Path

from scripts.release.version import parse_version

PACKAGE_TABLE = re.compile(r"(?ms)^\[package\]\s*\n.*?(?=^\[|\Z)")
LOCK_ENTRY = re.compile(r"(?ms)^\[\[package\]\]\n.*?(?=^\[\[package\]\]|\Z)")
VERSION_LINE = re.compile(r'(?m)^(version\s*=\s*)"[^"]+"')


def package(root: Path) -> tuple[str, str]:
    """Return the validated `(name, version)` of the root package."""
    with (root / "Cargo.toml").open("rb") as source:
        result = tomllib.load(source)["package"]
    parse_version(result["version"])
    return result["name"], result["version"]


def is_local_package(entry: dict, name: str) -> bool:
    return entry["name"] == name and "source" not in entry


def _replace_version(block: str, target: str) -> str:
    changed, count = VERSION_LINE.subn(lambda match: f'{match[1]}"{target}"', block)
    if count != 1:
        raise ValueError("expected exactly one package version")
    return changed


def update_versions(root: Path, target: str) -> None:
    """Set the package version in both files, preserving formatting and comments.

    Cargo.lock is rewritten textually; the result must leave every external
    dependency record unchanged.
    """
    parse_version(target)
    name, _ = package(root)
    manifest_path = root / "Cargo.toml"
    manifest = manifest_path.read_text()
    section = PACKAGE_TABLE.search(manifest)
    if not section:
        raise ValueError("missing package table")
    manifest_path.write_text(
        manifest[: section.start()]
        + _replace_version(section[0], target)
        + manifest[section.end() :]
    )

    lock_path = root / "Cargo.lock"
    lock = lock_path.read_text()
    original_packages = tomllib.loads(lock)["package"]
    matches = [
        match
        for match in LOCK_ENTRY.finditer(lock)
        if is_local_package(tomllib.loads(match[0])["package"][0], name)
    ]
    if len(matches) != 1:
        raise ValueError("expected exactly one local package in Cargo.lock")
    match = matches[0]
    updated = lock[: match.start()] + _replace_version(match[0], target) + lock[match.end() :]
    updated_packages = tomllib.loads(updated)["package"]
    before = [p for p in original_packages if not is_local_package(p, name)]
    after = [p for p in updated_packages if not is_local_package(p, name)]
    if before != after:
        raise ValueError("version update changed external dependencies")
    lock_path.write_text(updated)
