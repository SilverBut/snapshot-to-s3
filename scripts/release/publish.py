#!/usr/bin/env python3
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import tomllib

from scripts.release.prepare import (
    BRANCH_PREFIX, PLAN_PATH, NOTES_PATH, api, check_plan,
    parse_version, release_records, run, validate_proposal,
)


def output(values):
    print(json.dumps(values, indent=2))
    path = os.environ.get("GITHUB_OUTPUT")
    if path:
        with open(path, "a") as file:
            for name, value in values.items():
                file.write(f"{name}={value}\n")


def snapshot(root, sha):
    if not re.fullmatch(r"[0-9a-f]{40}", sha):
        raise ValueError("invalid release commit")
    plan = json.loads(run(["git", "show", f"{sha}:{PLAN_PATH}"], root, capture=True))
    cargo = tomllib.loads(run(["git", "show", f"{sha}:Cargo.toml"], root, capture=True))
    lock = tomllib.loads(run(["git", "show", f"{sha}:Cargo.lock"], root, capture=True))
    version = plan["version"]
    changelog = run(["git", "show", f"{sha}:{NOTES_PATH}"], root, capture=True)
    notes = validate_proposal(plan, cargo, lock, changelog)
    prepared = plan["prepared_from"]
    if not re.fullmatch(r"[0-9a-f]{40}", prepared):
        raise ValueError("invalid preparation commit")
    run(["git", "merge-base", "--is-ancestor", prepared, sha], root)
    return version, notes


def require_green(repo, root, sha):
    data = api(
        f"repos/{repo}/commits/{sha}/check-runs?check_name=CI%20Gate&filter=latest&per_page=100",
        root,
    )
    gates = [
        c for c in data["check_runs"]
        if c["name"] == "CI Gate" and c["app"]["id"] == 15368
    ]
    if not gates:
        raise ValueError("the frozen release commit has no CI Gate")
    latest = max(gates, key=lambda c: c["id"])
    if latest["status"] != "completed" or latest["conclusion"] != "success":
        raise ValueError("frozen commit CI did not succeed; rerun that exact CI before retrying")


def verify_tag(repo, root, tag, sha):
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


def select(root, mode, build_ref):
    repo = os.environ["GITHUB_REPOSITORY"]
    main = api(f"repos/{repo}", root)["default_branch"]
    if mode == "build-only":
        ref = build_ref or os.environ["GITHUB_SHA"]
        if ref.startswith("-") or ".." in ref or not re.fullmatch(r"[A-Za-z0-9._/+~-]+", ref):
            raise ValueError("invalid build reference")
        sha = run(["git", "rev-parse", "--verify", f"{ref}^{{commit}}"], root, capture=True).strip()
        cargo = tomllib.loads(run(["git", "show", f"{sha}:Cargo.toml"], root, capture=True))["package"]
        parse_version(cargo["version"])
        output({"action": "build", "sha": sha, "version": cargo["version"],
                "publish": "false", "tag": f"v{cargo['version']}"})
        return
    if os.environ.get("GITHUB_REF") != f"refs/heads/{main}":
        raise ValueError("publication and draft updates are restricted to trusted main")
    plan = check_plan(root)
    if plan is None:
        raise ValueError("no release plan exists on main")
    branch = f"{BRANCH_PREFIX}{plan['version']}"
    prs = json.loads(run(
        ["gh", "pr", "list", "--state", "merged", "--base", main,
         "--head", branch, "--json", "number,mergeCommit"], root, capture=True,
    ))
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
    output({"action": "build", "sha": sha, "version": version,
            "publish": "true", "tag": f"v{version}"})


def verify_assets(directory):
    archive = directory / "snapshot-to-s3-linux-x86_64.tar.gz"
    checksums = directory / "SHA256SUMS"
    files = set(p.name for p in directory.iterdir() if p.is_file())
    if files != {archive.name, checksums.name}:
        raise ValueError("release artifact must contain exactly the archive and SHA256SUMS")
    line = checksums.read_text().strip()
    match = re.fullmatch(r"([0-9a-f]{64})  snapshot-to-s3-linux-x86_64\.tar\.gz", line)
    if not match or hashlib.sha256(archive.read_bytes()).hexdigest() != match[1]:
        raise ValueError("release archive checksum mismatch")
    return archive, checksums


def publish(root, sha, directory):
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
        run(["gh", "api", "--method", "POST", f"repos/{repo}/git/refs",
             "-f", f"ref=refs/tags/{tag}", "-f", f"sha={sha}"], root)
    records = [r for r in release_records(repo, root) if r["tag_name"] == tag]
    if records and not records[0]["draft"]:
        print(f"{tag} is already published; nothing will be overwritten")
        return
    with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8") as notes_file:
        notes_file.write(notes)
        notes_file.flush()
        if not records:
            command = ["gh", "release", "create", tag, "--verify-tag", "--draft",
                       "--title", tag, "--notes-file", notes_file.name]
            if parse_version(version)[1] is not None:
                command.append("--prerelease")
            run(command, root)
        else:
            run(["gh", "release", "edit", tag, "--notes-file", notes_file.name], root)
    run(["gh", "release", "upload", tag, str(archive), str(checksums), "--clobber"], root)
    assets = api(f"repos/{repo}/releases/tags/{tag}", root)["assets"]
    expected = {p.name: p for p in (archive, checksums)}
    if set(a["name"] for a in assets) != set(expected):
        raise ValueError("draft contains missing or unexpected release assets")
    for asset in assets:
        path = expected[asset["name"]]
        if asset["size"] != path.stat().st_size:
            raise ValueError("draft asset size mismatch")
        digest = f"sha256:{hashlib.sha256(path.read_bytes()).hexdigest()}"
        if asset.get("digest") != digest:
            raise ValueError("draft asset digest mismatch")
    run(["gh", "release", "edit", tag, "--draft=false",
         "--prerelease" if parse_version(version)[1] else "--prerelease=false",
         "--latest=false" if parse_version(version)[1] else "--latest"], root)
    release = api(f"repos/{repo}/releases/tags/{tag}", root)
    if release["draft"]:
        raise ValueError("release publication was not confirmed")
    print(f"Published {tag} from frozen commit {sha}: {release['html_url']}")


def main():
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)
    choose = commands.add_parser("select")
    choose.add_argument("--mode", choices=["build-only", "publish"], required=True)
    choose.add_argument("--build-ref", default="")
    send = commands.add_parser("publish")
    send.add_argument("--sha", required=True)
    send.add_argument("--assets", type=Path, required=True)
    args = parser.parse_args()
    try:
        if args.command == "select":
            select(Path.cwd(), args.mode, args.build_ref)
        else:
            publish(Path.cwd(), args.sha, args.assets.resolve())
    except (ValueError, KeyError, OSError, subprocess.CalledProcessError) as error:
        print(f"Release controller failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
