#!/usr/bin/env python3
"""Select and publish a release from the accepted release PR's frozen merge commit.

Runs only from the manually dispatched Release workflow on trusted main.
`select` decides what to build; `publish` verifies the built assets, tags the
frozen commit and promotes the draft release. Existing tags and published
releases are never overwritten.
"""

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

from scripts.release.github import api, release_records, run
from scripts.release.notes import NOTES_PATH
from scripts.release.prepare import BRANCH_PREFIX, PLAN_PATH, check_plan, validate_proposal
from scripts.release.version import parse_version

ARCHIVE_NAME = "snapshot-to-s3-linux-x86_64.tar.gz"
CHECKSUMS_NAME = "SHA256SUMS"
CI_GATE_CHECK = "CI Gate"
# GitHub Actions' app id; check runs from any other app cannot satisfy the gate.
GITHUB_ACTIONS_APP_ID = 15368
COMMIT_SHA = re.compile(r"[0-9a-f]{40}")


def output(values: dict[str, str]) -> None:
    """Print step outputs and append them to `$GITHUB_OUTPUT` when present."""
    print(json.dumps(values, indent=2))
    path = os.environ.get("GITHUB_OUTPUT")
    if path:
        with open(path, "a") as file:
            for name, value in values.items():
                file.write(f"{name}={value}\n")


def snapshot(root: Path, sha: str) -> tuple[str, str]:
    """Validate the proposal recorded at `sha`; return its version and notes."""
    if not COMMIT_SHA.fullmatch(sha):
        raise ValueError("invalid release commit")
    plan = json.loads(run(["git", "show", f"{sha}:{PLAN_PATH}"], root, capture=True))
    cargo = tomllib.loads(run(["git", "show", f"{sha}:Cargo.toml"], root, capture=True))
    lock = tomllib.loads(run(["git", "show", f"{sha}:Cargo.lock"], root, capture=True))
    version = plan["version"]
    changelog = run(["git", "show", f"{sha}:{NOTES_PATH}"], root, capture=True)
    notes = validate_proposal(plan, cargo, lock, changelog)
    prepared = plan["prepared_from"]
    if not COMMIT_SHA.fullmatch(prepared):
        raise ValueError("invalid preparation commit")
    run(["git", "merge-base", "--is-ancestor", prepared, sha], root)
    return version, notes


def require_green(repo: str, root: Path, sha: str) -> None:
    """Require the latest GitHub Actions `CI Gate` check on `sha` to have succeeded."""
    data = api(
        f"repos/{repo}/commits/{sha}/check-runs?check_name=CI%20Gate&filter=latest&per_page=100",
        root,
    )
    gates = [
        c
        for c in data["check_runs"]
        if c["name"] == CI_GATE_CHECK and c["app"]["id"] == GITHUB_ACTIONS_APP_ID
    ]
    if not gates:
        raise ValueError("the frozen release commit has no CI Gate")
    latest = max(gates, key=lambda c: c["id"])
    if latest["status"] != "completed" or latest["conclusion"] != "success":
        raise ValueError("frozen commit CI did not succeed; rerun that exact CI before retrying")


def verify_tag(repo: str, root: Path, tag: str, sha: str) -> bool:
    """Return whether `tag` exists; raise if it resolves to any commit but `sha`."""
    refs = api(f"repos/{repo}/git/matching-refs/tags/{tag}", root)
    exact = [r for r in refs if r["ref"] == f"refs/tags/{tag}"]
    if not exact:
        return False
    obj = exact[0]["object"]
    while obj["type"] == "tag":
        obj = api(f"repos/{repo}/git/tags/{obj['sha']}", root)["object"]
    if obj["type"] != "commit" or obj["sha"] != sha:
        raise ValueError("existing release tag points to another commit; never overwrite it")
    return True


def select(root: Path, mode: str) -> None:
    repo = os.environ["GITHUB_REPOSITORY"]
    main = api(f"repos/{repo}", root)["default_branch"]
    if os.environ.get("GITHUB_REF") != f"refs/heads/{main}":
        raise ValueError("release workflows are restricted to the trusted default branch")
    if mode == "build-only":
        ref = os.environ["GITHUB_SHA"]
        sha = run(["git", "rev-parse", "--verify", f"{ref}^{{commit}}"], root, capture=True).strip()
        cargo = tomllib.loads(run(["git", "show", f"{sha}:Cargo.toml"], root, capture=True))[
            "package"
        ]
        parse_version(cargo["version"])
        output(
            {
                "action": "build",
                "sha": sha,
                "version": cargo["version"],
                "publish": "false",
                "tag": f"v{cargo['version']}",
            }
        )
        return
    plan = check_plan(root)
    if plan is None:
        raise ValueError("no release plan exists on main")
    branch = f"{BRANCH_PREFIX}{plan['version']}"
    prs = json.loads(
        run(
            [
                "gh",
                "pr",
                "list",
                "--state",
                "merged",
                "--base",
                main,
                "--head",
                branch,
                "--json",
                "number,mergeCommit",
            ],
            root,
            capture=True,
        )
    )
    if len(prs) != 1:
        raise ValueError("cannot identify exactly one accepted release PR")
    sha = prs[0]["mergeCommit"]["oid"]
    run(["git", "merge-base", "--is-ancestor", sha, f"origin/{main}"], root)
    version, _ = snapshot(root, sha)
    require_green(repo, root, sha)
    verify_tag(repo, root, f"v{version}", sha)
    existing = [r for r in release_records(repo, root) if r["tag_name"] == f"v{version}"]
    if existing and not existing[0]["draft"]:
        print(f"v{version} is already published; assets will not be replaced")
        output({"action": "none"})
        return
    output(
        {"action": "build", "sha": sha, "version": version, "publish": "true", "tag": f"v{version}"}
    )


def verify_assets(directory: Path) -> tuple[Path, Path]:
    archive = directory / ARCHIVE_NAME
    checksums = directory / CHECKSUMS_NAME
    files = {p.name for p in directory.iterdir() if p.is_file()}
    if files != {archive.name, checksums.name}:
        raise ValueError("release artifact must contain exactly the archive and SHA256SUMS")
    line = checksums.read_text().strip()
    match = re.fullmatch(rf"([0-9a-f]{{64}})  {re.escape(ARCHIVE_NAME)}", line)
    if not match or hashlib.sha256(archive.read_bytes()).hexdigest() != match[1]:
        raise ValueError("release archive checksum mismatch")
    return archive, checksums


def publish(root: Path, sha: str, directory: Path) -> None:
    repo = os.environ["GITHUB_REPOSITORY"]
    main = api(f"repos/{repo}", root)["default_branch"]
    if os.environ.get("GITHUB_REF") != f"refs/heads/{main}":
        raise ValueError("publication requires trusted main workflow code")
    run(["git", "merge-base", "--is-ancestor", sha, f"origin/{main}"], root)
    version, notes = snapshot(root, sha)
    require_green(repo, root, sha)
    tag = f"v{version}"
    archive, checksums = verify_assets(directory)
    if not verify_tag(repo, root, tag, sha):
        run(
            [
                "gh",
                "api",
                "--method",
                "POST",
                f"repos/{repo}/git/refs",
                "-f",
                f"ref=refs/tags/{tag}",
                "-f",
                f"sha={sha}",
            ],
            root,
        )
    records = [r for r in release_records(repo, root) if r["tag_name"] == tag]
    if records and not records[0]["draft"]:
        print(f"{tag} is already published; nothing will be overwritten")
        return
    with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8") as notes_file:
        notes_file.write(notes)
        notes_file.flush()
        if not records:
            command = [
                "gh",
                "release",
                "create",
                tag,
                "--verify-tag",
                "--draft",
                "--title",
                tag,
                "--notes-file",
                notes_file.name,
            ]
            if parse_version(version)[1] is not None:
                command.append("--prerelease")
            run(command, root)
        else:
            run(["gh", "release", "edit", tag, "--notes-file", notes_file.name], root)
    run(["gh", "release", "upload", tag, str(archive), str(checksums), "--clobber"], root)
    records = [r for r in release_records(repo, root) if r["tag_name"] == tag]
    if len(records) != 1:
        raise ValueError("cannot identify exactly one draft release after asset upload")
    release_path = f"repos/{repo}/releases/{records[0]['id']}"
    release = api(release_path, root)
    if not release["draft"]:
        raise ValueError("release is no longer a draft; publication stopped")
    assets = release["assets"]
    expected = {p.name: p for p in (archive, checksums)}
    if {a["name"] for a in assets} != set(expected):
        raise ValueError("draft contains missing or unexpected release assets")
    for asset in assets:
        path = expected[asset["name"]]
        if asset["size"] != path.stat().st_size:
            raise ValueError("draft asset size mismatch")
        digest = f"sha256:{hashlib.sha256(path.read_bytes()).hexdigest()}"
        if asset.get("digest") != digest:
            raise ValueError("draft asset digest mismatch")
    prerelease = parse_version(version)[1] is not None
    run(
        [
            "gh",
            "release",
            "edit",
            tag,
            "--draft=false",
            "--prerelease" if prerelease else "--prerelease=false",
            "--latest=false" if prerelease else "--latest",
        ],
        root,
    )
    release = api(release_path, root)
    if release["draft"]:
        raise ValueError("release publication was not confirmed")
    print(f"Published {tag} from frozen commit {sha}: {release['html_url']}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    choose = commands.add_parser("select")
    choose.add_argument("--mode", choices=["build-only", "publish"], required=True)
    send = commands.add_parser("publish")
    send.add_argument("--sha", required=True)
    send.add_argument("--assets", type=Path, required=True)
    args = parser.parse_args()
    try:
        if args.command == "select":
            select(Path.cwd(), args.mode)
        else:
            publish(Path.cwd(), args.sha, args.assets.resolve())
    except (ValueError, KeyError, OSError, subprocess.CalledProcessError) as error:
        print(f"Release controller failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
