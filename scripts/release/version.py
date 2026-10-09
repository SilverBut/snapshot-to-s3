"""Release version parsing, ordering and next-version selection."""

import re
from collections.abc import Iterable, Sequence

VERSION = re.compile(
    r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)"
    r"(?:-(alpha|beta|pre|rc)\.([1-9]\d*))?$"
)
# Precedence of prerelease channels; `None` is a stable release.
CHANNELS: dict[str | None, int] = {"alpha": 0, "beta": 1, "pre": 2, "rc": 3, None: 4}

Core = tuple[int, int, int]


def parse_version(value: object) -> tuple[Core, str | None, int]:
    """Return `(core, channel, number)`, raising ValueError for unsupported forms."""
    match = VERSION.fullmatch(value) if isinstance(value, str) else None
    if not match:
        raise ValueError(f"unsupported release version: {value!r}")
    major, minor, patch, channel, number = match.groups()
    return (int(major), int(minor), int(patch)), channel, int(number or 0)


def version_key(value: str) -> tuple[int, int, int, int, int]:
    core, channel, number = parse_version(value)
    return (*core, CHANNELS[channel], number)


def is_stable(value: str) -> bool:
    return parse_version(value)[1] is None


def latest_stable(published: Iterable[str]) -> str | None:
    stable = [v for v in published if is_stable(v)]
    return max(stable, key=version_key) if stable else None


def next_version(current: str, published: Sequence[str], bump: str, channel: str = "stable") -> str:
    current_core, _, _ = parse_version(current)
    latest = latest_stable(published)
    if bump == "initial":
        if latest is not None:
            raise ValueError("initial is only valid before the first stable release")
        core = current_core
    else:
        if latest is None:
            raise ValueError("no stable release exists; choose initial")
        major, minor, patch = parse_version(latest)[0]
        if bump == "patch":
            core = (major, minor, patch + 1)
        elif bump == "minor":
            core = (major, minor + 1, 0)
        elif bump == "major":
            core = (major + 1, 0, 0)
        else:
            raise ValueError(f"invalid bump type: {bump}")
    if core < current_core:
        raise ValueError("refusing a version downgrade")
    target = ".".join(map(str, core))
    if channel != "stable":
        if channel not in CHANNELS or channel is None:
            raise ValueError(f"invalid channel: {channel}")
        numbers = [
            parse_version(v)[2] for v in published if parse_version(v)[:2] == (core, channel)
        ]
        target += f"-{channel}.{max(numbers, default=0) + 1}"
    if target in published:
        raise ValueError(f"{target} is already published")
    if latest is not None and version_key(target) < version_key(current):
        raise ValueError("refusing a prerelease channel downgrade")
    return target


def published_versions(records: Iterable[dict]) -> list[str]:
    """Versions of non-draft `v*` GitHub releases; malformed tags are rejected."""
    versions = []
    for record in records:
        tag = record["tag_name"]
        if not record["draft"] and tag.startswith("v"):
            parse_version(tag[1:])
            versions.append(tag[1:])
    return versions
