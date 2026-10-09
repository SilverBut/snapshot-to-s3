#!/usr/bin/env python3
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import tomllib


PLAN_PATH = Path(".github/release-plan.json")
NOTES_PATH = Path("CHANGELOG.md")
BRANCH_PREFIX = "automation/release-v"
NOTES_MARKER = "<!-- RELEASE_NOTES_NEED_REVIEW -->"
VERSION = re.compile(
    r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)"
    r"(?:-(alpha|beta|pre|rc)\.([1-9]\d*))?$"
)
CHANNELS = {"alpha": 0, "beta": 1, "pre": 2, "rc": 3, None: 4}


def parse_version(value):
    match = VERSION.fullmatch(value) if isinstance(value, str) else None
    if not match:
        raise ValueError(f"unsupported release version: {value!r}")
    major, minor, patch, channel, number = match.groups()
    return (int(major), int(minor), int(patch)), channel, int(number or 0)


def version_key(value):
    core, channel, number = parse_version(value)
    return (*core, CHANNELS[channel], number)


def next_version(current, published, bump, channel="stable"):
    current_core, _, _ = parse_version(current)
    stable = [v for v in published if parse_version(v)[1] is None]
    if bump == "initial":
        if stable:
            raise ValueError("initial is only valid before the first stable release")
        core = current_core
    else:
        if not stable:
            raise ValueError("no stable release exists; choose initial")
        major, minor, patch = parse_version(max(stable, key=version_key))[0]
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
            parse_version(v)[2] for v in published
            if parse_version(v)[:2] == (core, channel)
        ]
        target += f"-{channel}.{max(numbers, default=0) + 1}"
    if target in published:
        raise ValueError(f"{target} is already published")
    if stable and version_key(target) < version_key(current):
        raise ValueError("refusing a prerelease channel downgrade")
    return target


def run(command, root, capture=False):
    return subprocess.run(
        command, cwd=root, check=True, text=True,
        stdout=subprocess.PIPE if capture else None,
    ).stdout


def api(path, root):
    return json.loads(run(["gh", "api", path], root, capture=True))


def release_records(repo, root):
    pages = json.loads(run(
        ["gh", "api", f"repos/{repo}/releases", "--paginate", "--slurp"],
        root, capture=True,
    ))
    return [release for page in pages for release in page]


def published_versions(records):
    versions = []
    for record in records:
        tag = record["tag_name"]
        if not record["draft"] and tag.startswith("v"):
            parse_version(tag[1:])
            versions.append(tag[1:])
    return versions


def package(root):
    with (root / "Cargo.toml").open("rb") as source:
        result = tomllib.load(source)["package"]
    parse_version(result["version"])
    return result["name"], result["version"]


def update_versions(root, target):
    parse_version(target)
    name, _ = package(root)
    manifest_path = root / "Cargo.toml"
    manifest = manifest_path.read_text()
    section = re.search(r"(?ms)^\[package\]\s*\n.*?(?=^\[|\Z)", manifest)
    if not section:
        raise ValueError("missing package table")

    def replace(block):
        changed, count = re.subn(
            r'(?m)^(version\s*=\s*)"[^"]+"',
            lambda match: f'{match[1]}"{target}"', block,
        )
        if count != 1:
            raise ValueError("expected exactly one package version")
        return changed

    manifest_path.write_text(
        manifest[:section.start()] + replace(section[0]) + manifest[section.end():]
    )
    lock_path = root / "Cargo.lock"
    lock = lock_path.read_text()
    original_packages = tomllib.loads(lock)["package"]
    matches = []
    for match in re.finditer(r"(?ms)^\[\[package\]\]\n.*?(?=^\[\[package\]\]|\Z)", lock):
        entry = tomllib.loads(match[0])["package"][0]
        if entry["name"] == name and "source" not in entry:
            matches.append(match)
    if len(matches) != 1:
        raise ValueError("expected exactly one local package in Cargo.lock")
    match = matches[0]
    updated = lock[:match.start()] + replace(match[0]) + lock[match.end():]
    updated_packages = tomllib.loads(updated)["package"]
    before = [p for p in original_packages if p["name"] != name or "source" in p]
    after = [p for p in updated_packages if p["name"] != name or "source" in p]
    if before != after:
        raise ValueError("version update changed external dependencies")
    lock_path.write_text(updated)


def notes_section(text, version, reviewed=True):
    parse_version(version)
    heading = re.compile(r"^## \[([^\]]+)\].*$", re.MULTILINE)
    headings = list(heading.finditer(text))
    matches = [(i, h) for i, h in enumerate(headings) if h[1] == version]
    if len(matches) != 1:
        raise ValueError(f"expected one CHANGELOG section for {version}")
    index, heading_match = matches[0]
    end = headings[index + 1].start() if index + 1 < len(headings) else len(text)
    section = text[heading_match.end():end].strip()
    if reviewed:
        if NOTES_MARKER in section:
            raise ValueError("release notes still require maintainer review")
        content = re.sub(r"<!--.*?-->", "", section, flags=re.DOTALL)
        content = re.sub(r"(?m)^#+.*$", "", content).strip()
        if not content:
            raise ValueError("release notes are empty")
    return section


def prepare_notes(text, target):
    if re.search(rf"(?m)^## \[{re.escape(target)}\]", text):
        raise ValueError(f"CHANGELOG already contains {target}")
    unreleased = re.search(r"(?ms)^## \[Unreleased\][^\n]*\n(.*?)(?=^## \[|\Z)", text)
    if unreleased:
        content = unreleased[1].strip()
        remaining = text[:unreleased.start()] + text[unreleased.end():]
    else:
        content = ""
        remaining = text
    if remaining.startswith("# Changelog"):
        remaining = remaining[len("# Changelog"):].lstrip()
    if not content:
        content = (
            "### Added\n<!-- Describe new capabilities. -->\n\n"
            "### Fixed\n<!-- Describe fixes. -->\n\n"
            "### Upgrade notes\n<!-- Describe compatibility and migration. -->\n\n"
            "### Known issues\n<!-- Describe limitations, or state none. -->"
        )
    return (
        f"# Changelog\n\n## [Unreleased]\n\n## [{target}]\n\n"
        f"{NOTES_MARKER}\n\n{content}\n\n{remaining}"
    ).rstrip() + "\n"


def validate_proposal(plan, manifest, lock, changelog):
    parse_version(plan["version"])
    if not re.fullmatch(r"[0-9a-f]{40}", plan["prepared_from"]):
        raise ValueError("invalid prepared_from commit")
    previous = plan["previous_tag"]
    if previous is not None:
        if not previous.startswith("v"):
            raise ValueError("invalid previous release tag")
        parse_version(previous[1:])
    name = manifest["package"]["name"]
    version = manifest["package"]["version"]
    if plan["version"] != version:
        raise ValueError("release proposal and Cargo.toml versions disagree")
    own = [p for p in lock["package"] if p["name"] == name and "source" not in p]
    if len(own) != 1 or own[0]["version"] != version:
        raise ValueError("Cargo.lock local package version disagrees")
    return notes_section(changelog, version)


def check_plan(root):
    path = root / PLAN_PATH
    if not path.exists():
        print("No release proposal is recorded; ordinary CI checks continue")
        return None
    plan = json.loads(path.read_text())
    validate_proposal(
        plan, tomllib.loads((root / "Cargo.toml").read_text()),
        tomllib.loads((root / "Cargo.lock").read_text()), (root / NOTES_PATH).read_text(),
    )
    return plan


def prepare(root, bump, channel, dry_run):
    repo = os.environ["GITHUB_REPOSITORY"]
    info = api(f"repos/{repo}", root)
    main = info["default_branch"]
    if not dry_run and (
        os.environ.get("GITHUB_ACTIONS") != "true"
        or os.environ.get("GITHUB_REF") != f"refs/heads/{main}"
    ):
        raise ValueError("release preparation writes are restricted to trusted main")
    source_sha = run(["git", "rev-parse", "HEAD"], root, capture=True).strip()
    records = release_records(repo, root)
    if any(r["draft"] and r["tag_name"].startswith("v") for r in records):
        raise ValueError("an unfinished draft release exists; retry it before bumping")
    versions = published_versions(records)
    _, current = package(root)
    if (root / PLAN_PATH).exists():
        accepted = json.loads((root / PLAN_PATH).read_text())
        if accepted["version"] == current and current not in versions:
            raise ValueError("main contains an unfinished release; use Retry Release")
    if versions and current != max(versions, key=version_key):
        raise ValueError("main version differs from the last published release")
    target = next_version(current, versions, bump, channel)
    stable = [v for v in versions if parse_version(v)[1] is None]
    previous = f"v{max(stable, key=version_key)}" if stable else None
    plan = {"version": target, "previous_tag": previous, "prepared_from": source_sha}
    branch = f"{BRANCH_PREFIX}{target}"
    print(f"Preparing {current} -> {target}; source={source_sha}; branch={branch}")
    if dry_run:
        print(json.dumps(plan, indent=2))
        old = (root / NOTES_PATH).read_text() if (root / NOTES_PATH).exists() else ""
        print(prepare_notes(old, target))
        return
    prs = json.loads(run(
        ["gh", "pr", "list", "--state", "open", "--base", main, "--label", "release",
         "--json", "number,headRefName,url"], root, capture=True,
    ))
    if any(p["headRefName"] != branch for p in prs):
        raise ValueError("another release PR is open; finish or close it first")
    remote = run(["git", "ls-remote", "origin", f"refs/heads/{branch}"], root, capture=True)
    run(["git", "config", "user.name", "github-actions[bot]"], root)
    run(["git", "config", "user.email", "41898282+github-actions[bot]@users.noreply.github.com"], root)
    if remote.strip():
        run(["git", "fetch", "origin", f"refs/heads/{branch}"], root)
        run(["git", "switch", "-c", branch, "FETCH_HEAD"], root)
        managed = json.loads((root / PLAN_PATH).read_text())
        if managed["version"] != target:
            raise ValueError("existing release branch has a different proposal")
        run(["git", "merge", "--no-edit", source_sha], root)
    else:
        run(["git", "switch", "-c", branch], root)
        old = (root / NOTES_PATH).read_text() if (root / NOTES_PATH).exists() else ""
        (root / NOTES_PATH).write_text(prepare_notes(old, target))
        update_versions(root, target)
    (root / PLAN_PATH).parent.mkdir(exist_ok=True)
    (root / PLAN_PATH).write_text(json.dumps(plan, indent=2) + "\n")
    run(["git", "add", "--", "Cargo.toml", "Cargo.lock", str(NOTES_PATH), str(PLAN_PATH)], root)
    changed = subprocess.run(["git", "diff", "--cached", "--quiet"], cwd=root).returncode
    if changed == 1:
        run(["git", "commit", "-m", f"Prepare release v{target}", "-m",
             "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"], root)
    elif changed != 0:
        raise ValueError("cannot inspect staged release changes")
    run(["git", "push", "origin", f"HEAD:refs/heads/{branch}"], root)
    if not prs:
        start = f"{previous}..{source_sha}" if previous else source_sha
        commits = run(
            ["git", "log", start, "--no-merges", "--format=- %s (%h)", "-n", "100"],
            root, capture=True,
        )
        body = (
            f"Prepare **v{target}** from `{source_sha}`.\n\n"
            "Edit this PR's CHANGELOG version section and remove the review marker. "
            "CI validates the version, notes and all infrastructure tests. "
            "After CI succeeds this draft is marked ready automatically. "
            "Merge with a merge commit to authorize publication.\n\n"
            f"### Candidate commits (not approved release notes)\n\n{commits}"
        )
        with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8") as file:
            file.write(body)
            file.flush()
            run(["gh", "pr", "create", "--draft", "--base", main, "--head", branch,
                 "--label", "release", "--title", f"Release v{target}",
                 "--body-file", file.name], root)
    else:
        print(f"Preserved existing notes and release PR: {prs[0]['url']}")
    head = run(["git", "rev-parse", "HEAD"], root, capture=True).strip()
    checks = json.loads(run(
        ["gh", "run", "list", "--workflow", "ci.yml", "--branch", branch,
         "--commit", head, "--json", "status,conclusion", "--limit", "10"],
        root, capture=True,
    ))
    if any(r["status"] != "completed" or r["conclusion"] == "success" for r in checks):
        print("Current release head already has active or successful CI")
    else:
        run(["gh", "workflow", "run", "ci.yml", "--ref", branch], root)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as file:
            file.write(
                f"## Prepared release v{target}\n\n"
                f"Source: `{source_sha}`\n\nBranch: `{branch}`\n\n"
                "Edit the CHANGELOG version section in the draft PR, remove the review marker, "
                "then wait for CI and merge with a merge commit.\n"
            )


def main():
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("check")
    create = commands.add_parser("prepare")
    create.add_argument("--bump", choices=["initial", "patch", "minor", "major"], required=True)
    create.add_argument("--channel", choices=["stable", "alpha", "beta", "pre", "rc"], default="stable")
    create.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()
    root = Path.cwd()
    try:
        if args.command == "check":
            check_plan(root)
        else:
            prepare(root, args.bump, args.channel, args.dry_run)
    except (ValueError, KeyError, OSError, subprocess.CalledProcessError) as error:
        print(f"Release preparation failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
