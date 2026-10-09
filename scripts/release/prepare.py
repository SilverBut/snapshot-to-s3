#!/usr/bin/env python3
"""Validate the recorded release proposal, or prepare a release PR from trusted main.

`check` runs in ordinary CI. `prepare` runs only from the manually dispatched
Prepare Release workflow; it pushes `automation/release-v<version>` and opens
or reuses one release PR. Nothing is published here.
"""

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

from scripts.release.github import api, release_records, run
from scripts.release.manifest import is_local_package, package, update_versions
from scripts.release.notes import (
    NOTES_MARKER,
    NOTES_PATH,
    notes_section,
    prepare_notes,
    read_notes,
)
from scripts.release.version import (
    latest_stable,
    next_version,
    parse_version,
    published_versions,
    version_key,
)

# Re-exported for callers and tests that address the controller as one module.
__all__ = [
    "BRANCH_PREFIX",
    "NOTES_MARKER",
    "NOTES_PATH",
    "PLAN_PATH",
    "api",
    "check_plan",
    "next_version",
    "notes_section",
    "parse_version",
    "prepare",
    "prepare_notes",
    "release_records",
    "run",
    "update_versions",
    "validate_proposal",
]

PLAN_PATH = Path(".github/release-plan.json")
BRANCH_PREFIX = "automation/release-v"
BOT_NAME = "github-actions[bot]"
BOT_EMAIL = "41898282+github-actions[bot]@users.noreply.github.com"
COMMIT_TRAILER = "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
NEXT_STEPS = (
    "Edit the CHANGELOG version section in the PR, remove the review marker, "
    "wait for CI, merge with a merge commit, then run Release in publish mode.\n"
)


def validate_proposal(plan: dict, manifest: dict, lock: dict, changelog: str) -> str:
    """Check a release plan against the manifests; return its reviewed notes."""
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
    own = [p for p in lock["package"] if is_local_package(p, name)]
    if len(own) != 1 or own[0]["version"] != version:
        raise ValueError("Cargo.lock local package version disagrees")
    return notes_section(changelog, version)


def check_plan(root: Path) -> dict | None:
    path = root / PLAN_PATH
    if not path.exists():
        print("No release proposal is recorded; ordinary CI checks continue")
        return None
    plan = json.loads(path.read_text())
    validate_proposal(
        plan,
        tomllib.loads((root / "Cargo.toml").read_text()),
        tomllib.loads((root / "Cargo.lock").read_text()),
        (root / NOTES_PATH).read_text(),
    )
    return plan


def _plan_release(root: Path, repo: str, bump: str, channel: str, source_sha: str) -> dict:
    """Choose the next version from published releases and the main manifest."""
    records = release_records(repo, root)
    if any(r["draft"] and r["tag_name"].startswith("v") for r in records):
        raise ValueError(
            "an unfinished draft release exists; run Release in publish mode before bumping"
        )
    versions = published_versions(records)
    _, current = package(root)
    if (root / PLAN_PATH).exists():
        accepted = json.loads((root / PLAN_PATH).read_text())
        if accepted["version"] == current and current not in versions:
            raise ValueError("main contains an unfinished release; run Release in publish mode")
    if versions and current != max(versions, key=version_key):
        raise ValueError("main version differs from the last published release")
    target = next_version(current, versions, bump, channel)
    latest = latest_stable(versions)
    print(f"Preparing {current} -> {target}; source={source_sha}; branch={BRANCH_PREFIX}{target}")
    return {
        "version": target,
        "previous_tag": f"v{latest}" if latest else None,
        "prepared_from": source_sha,
    }


def _commit_release_branch(
    root: Path, plan: dict, branch: str, generated_notes: str | None = None
) -> None:
    """Create or resume the release branch and push the proposal commit.

    A resumed branch keeps its maintainer-edited notes and only merges main.
    """
    target = plan["version"]
    remote = run(["git", "ls-remote", "origin", f"refs/heads/{branch}"], root, capture=True)
    run(["git", "config", "user.name", BOT_NAME], root)
    run(["git", "config", "user.email", BOT_EMAIL], root)
    if remote.strip():
        run(["git", "fetch", "origin", f"refs/heads/{branch}"], root)
        run(["git", "switch", "-c", branch, "FETCH_HEAD"], root)
        managed = json.loads((root / PLAN_PATH).read_text())
        if managed["version"] != target:
            raise ValueError("existing release branch has a different proposal")
        run(["git", "merge", "--no-edit", plan["prepared_from"]], root)
    else:
        run(["git", "switch", "-c", branch], root)
        (root / NOTES_PATH).write_text(
            prepare_notes(read_notes(root), target, generated_notes)
        )
        update_versions(root, target)
    (root / PLAN_PATH).parent.mkdir(exist_ok=True)
    (root / PLAN_PATH).write_text(json.dumps(plan, indent=2) + "\n")
    run(["git", "add", "--", "Cargo.toml", "Cargo.lock", str(NOTES_PATH), str(PLAN_PATH)], root)
    changed = subprocess.run(["git", "diff", "--cached", "--quiet"], cwd=root).returncode
    if changed == 1:
        run(["git", "commit", "-m", f"Prepare release v{target}", "-m", COMMIT_TRAILER], root)
    elif changed != 0:
        raise ValueError("cannot inspect staged release changes")
    run(["git", "push", "origin", f"HEAD:refs/heads/{branch}"], root)


def _open_release_pr(root: Path, plan: dict, branch: str, main: str) -> None:
    target, source_sha, previous = plan["version"], plan["prepared_from"], plan["previous_tag"]
    start = f"{previous}..{source_sha}" if previous else source_sha
    commits = run(
        ["git", "log", start, "--no-merges", "--format=- %s (%h)", "-n", "100"],
        root,
        capture=True,
    )
    body = (
        f"Prepare **v{target}** from `{source_sha}`.\n\n"
        "Review the generated draft in this PR's CHANGELOG version section and remove "
        "the review marker. "
        "CI validates the version, notes and all infrastructure tests. "
        "After CI passes, merge with a merge commit, then run Actions → Release "
        "in publish mode. Nothing is published automatically.\n\n"
        f"### Candidate commits (not approved release notes)\n\n{commits}"
    )
    with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8") as file:
        file.write(body)
        file.flush()
        run(
            [
                "gh",
                "pr",
                "create",
                "--base",
                main,
                "--head",
                branch,
                "--label",
                "release",
                "--title",
                f"Release v{target}",
                "--body-file",
                file.name,
            ],
            root,
        )


def _ensure_release_ci(root: Path, branch: str) -> None:
    """Dispatch CI for the release head unless a run is active or already green."""
    head = run(["git", "rev-parse", "HEAD"], root, capture=True).strip()
    checks = json.loads(
        run(
            [
                "gh",
                "run",
                "list",
                "--workflow",
                "ci.yml",
                "--branch",
                branch,
                "--commit",
                head,
                "--json",
                "status,conclusion",
                "--limit",
                "10",
            ],
            root,
            capture=True,
        )
    )
    if any(r["status"] != "completed" or r["conclusion"] == "success" for r in checks):
        print("Current release head already has active or successful CI")
    else:
        run(["gh", "workflow", "run", "ci.yml", "--ref", branch], root)


def _write_step_summary(plan: dict, branch: str) -> None:
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as file:
            file.write(
                f"## Prepared release v{plan['version']}\n\n"
                f"Source: `{plan['prepared_from']}`\n\nBranch: `{branch}`\n\n" + NEXT_STEPS
            )


def prepare(
    root: Path,
    bump: str,
    channel: str,
    dry_run: bool,
    generated_notes: str | None = None,
) -> None:
    repo = os.environ["GITHUB_REPOSITORY"]
    main = api(f"repos/{repo}", root)["default_branch"]
    if not dry_run and (
        os.environ.get("GITHUB_ACTIONS") != "true"
        or os.environ.get("GITHUB_REF") != f"refs/heads/{main}"
    ):
        raise ValueError("release preparation writes are restricted to trusted main")
    source_sha = run(["git", "rev-parse", "HEAD"], root, capture=True).strip()
    plan = _plan_release(root, repo, bump, channel, source_sha)
    target = plan["version"]
    branch = f"{BRANCH_PREFIX}{target}"
    if dry_run:
        print(json.dumps(plan, indent=2))
        print(prepare_notes(read_notes(root), target, generated_notes))
        return
    prs = json.loads(
        run(
            [
                "gh",
                "pr",
                "list",
                "--state",
                "open",
                "--base",
                main,
                "--label",
                "release",
                "--json",
                "number,headRefName,url",
            ],
            root,
            capture=True,
        )
    )
    if any(p["headRefName"] != branch for p in prs):
        raise ValueError("another release PR is open; finish or close it first")
    _commit_release_branch(root, plan, branch, generated_notes)
    if prs:
        print(f"Preserved existing notes and release PR: {prs[0]['url']}")
    else:
        _open_release_pr(root, plan, branch, main)
    _ensure_release_ci(root, branch)
    _write_step_summary(plan, branch)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("check", help="validate .github/release-plan.json if present")
    create = commands.add_parser("prepare", help="open or update the release PR")
    create.add_argument("--bump", choices=["initial", "patch", "minor", "major"], required=True)
    create.add_argument(
        "--channel", choices=["stable", "alpha", "beta", "pre", "rc"], default="stable"
    )
    create.add_argument("--dry-run", action="store_true")
    create.add_argument("--generated-notes-file", type=Path)
    args = parser.parse_args()
    root = Path.cwd()
    try:
        if args.command == "check":
            check_plan(root)
        else:
            generated_notes = (
                args.generated_notes_file.read_text()
                if args.generated_notes_file
                else None
            )
            prepare(root, args.bump, args.channel, args.dry_run, generated_notes)
    except (ValueError, KeyError, OSError, subprocess.CalledProcessError) as error:
        print(f"Release preparation failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
