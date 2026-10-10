import hashlib
import json
import os
import subprocess
import tempfile
import textwrap
import tomllib
import unittest
from pathlib import Path
from unittest.mock import patch

from scripts.release import prepare as release
from scripts.release import publish as publish_release

MANIFEST = (
    '[package]\nname = "fixture"\nversion = "0.1.0" # preserve comment\n\n'
    '[dependencies]\nbytes = "1"\n'
)
LOCK = """# Keep formatting
version = 4

[[package]]
name = "bytes"
version = "1.12.1"
source = "registry+https://example.invalid"
checksum = "unchanged"

[[package]]
name = "fixture"
version = "0.1.0"
dependencies = ["bytes"]
"""


class ReleaseTests(unittest.TestCase):
    def fixture(self, root):
        (root / "Cargo.toml").write_text(MANIFEST)
        (root / "Cargo.lock").write_text(LOCK)

    def test_initial_and_explicit_bumps(self):
        self.assertEqual(release.next_version("0.1.0", [], "initial"), "0.1.0")
        for bump, expected in (("patch", "1.2.4"), ("minor", "1.3.0"), ("major", "2.0.0")):
            self.assertEqual(release.next_version("1.2.3", ["1.2.3"], bump), expected)
        with self.assertRaises(ValueError):
            release.next_version("0.1.0", [], "patch")
        with self.assertRaises(ValueError):
            release.next_version("1.2.3", ["1.2.3"], "initial")

    def test_prerelease_iteration_and_promotion(self):
        versions = ["0.1.0", "0.2.0-rc.1", "0.2.0-rc.2"]
        self.assertEqual(release.next_version("0.2.0-rc.2", versions, "minor", "rc"), "0.2.0-rc.3")
        self.assertEqual(release.next_version("0.2.0-rc.2", versions, "minor"), "0.2.0")
        with self.assertRaises(ValueError):
            release.next_version("0.2.0-rc.2", versions, "patch")
        with self.assertRaises(ValueError):
            release.next_version("0.2.0-rc.2", versions, "minor", "beta")

    def test_version_validation(self):
        for value in ("1.2", "01.2.3", "1.2.3-rc.0", '1.2.3"; exit', None):
            with self.subTest(value=value), self.assertRaises(ValueError):
                release.parse_version(value)

    def test_updates_only_own_manifest_and_lock_version(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)
            release.update_versions(root, "0.2.0")
            self.assertIn('version = "0.2.0" # preserve comment', (root / "Cargo.toml").read_text())
            data = tomllib.loads((root / "Cargo.lock").read_text())
            self.assertEqual(data["package"][0]["version"], "1.12.1")
            self.assertEqual(data["package"][0]["checksum"], "unchanged")
            self.assertEqual(data["package"][1]["version"], "0.2.0")
            self.assertEqual(data["version"], 4)

    def test_notes_are_unique_reviewed_and_version_scoped(self):
        old = "# Changelog\n\n## [Unreleased]\n\n### Fixed\n- A change.\n\n## [0.0.1]\n\n- Older.\n"
        text = release.prepare_notes(old, "0.1.0")
        self.assertEqual(text.count("- A change."), 1)
        with self.assertRaises(ValueError):
            release.notes_section(text, "0.1.0")
        reviewed = text.replace(release.NOTES_MARKER, "")
        self.assertIn("- A change.", release.notes_section(reviewed, "0.1.0"))
        self.assertNotIn("- Older.", release.notes_section(reviewed, "0.1.0"))
        with self.assertRaises(ValueError):
            release.notes_section("## [0.1.0]\n### Fixed\n<!-- empty -->", "0.1.0")
        with self.assertRaises(ValueError):
            release.notes_section(reviewed + "\n## [0.1.0]\n- Duplicate", "0.1.0")

    def test_generated_notes_are_added_to_marked_release_draft(self):
        text = release.prepare_notes("", "0.1.0", "- Generated user-facing change.")
        self.assertIn(release.NOTES_MARKER, text)
        with self.assertRaises(ValueError):
            release.notes_section(text, "0.1.0")
        reviewed = text.replace(release.NOTES_MARKER, "")
        self.assertEqual(
            release.notes_section(reviewed, "0.1.0"),
            "- Generated user-facing change.",
        )

    def test_prepare_workflow_is_manual_and_default_branch_only(self):
        workflow = (
            Path(__file__).resolve().parents[2] / ".github/workflows/version-bump.yml"
        ).read_text()
        triggers = workflow.split("\non:\n", 1)[1].split("\npermissions:", 1)[0]
        self.assertIn("workflow_dispatch:", triggers)
        for automatic in ("workflow_run", "push", "pull_request", "schedule", "workflow_call"):
            self.assertNotIn(automatic, triggers)
        jobs = workflow.split("\njobs:\n", 1)[1]
        generation, preparation = jobs.split("\n  prepare:\n", 1)
        guard = (
            "if: github.event_name == 'workflow_dispatch' && "
            "github.ref == format('refs/heads/{0}', github.event.repository.default_branch)"
        )
        for job in (generation, preparation):
            self.assertIn(guard, job)
            self.assertIn("ref: ${{ github.sha }}", job)
        self.assertIn("contents: read", generation)
        self.assertIn("pull-requests: read", generation)
        self.assertNotIn(": write", generation)
        self.assertIn("persist-credentials: false", generation)
        self.assertIn("COPILOT_GITHUB_TOKEN: ${{ secrets.COPILOT_GITHUB_TOKEN }}", generation)
        self.assertNotIn("COPILOT_GITHUB_TOKEN", preparation)
        self.assertIn("needs: generate-notes", preparation)
        self.assertIn(
            "RELEASE_NOTES: ${{ needs.generate-notes.outputs.release_notes }}", preparation
        )
        self.assertNotIn("run: ${{", preparation)

    def test_release_pr_uses_changelog_draft_without_legacy_commit_summary(self):
        plan = {"version": "0.1.0", "prepared_from": "a" * 40, "previous_tag": None}
        bodies = []

        def runner(command, root, capture=False):
            self.assertEqual(command[:3], ["gh", "pr", "create"])
            bodies.append(Path(command[command.index("--body-file") + 1]).read_text())

        with patch.object(release, "run", side_effect=runner):
            release._open_release_pr(Path("."), plan, "automation/release-v0.1.0", "main")
        self.assertEqual(len(bodies), 1)
        self.assertIn("Review the generated draft", bodies[0])
        self.assertNotIn("Candidate commits", bodies[0])

    def test_empty_generated_notes_file_fails_before_preparation(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "notes.md"
            path.write_text(" \n")
            with (
                patch(
                    "sys.argv",
                    ["x", "prepare", "--bump", "initial", "--generated-notes-file", str(path)],
                ),
                patch.object(release, "prepare") as prepare,
                patch("sys.stderr") as stderr,
            ):
                self.assertEqual(release.main(), 1)
                prepare.assert_not_called()
                self.assertIn(
                    "generated release notes are empty", stderr.write.call_args_list[0].args[0]
                )

    def test_plan_checks_all_three_version_sources(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)
            path = root / release.PLAN_PATH
            path.parent.mkdir()
            path.write_text(
                json.dumps(
                    {
                        "version": "0.1.0",
                        "previous_tag": None,
                        "prepared_from": "a" * 40,
                    }
                )
            )
            notes = release.prepare_notes("", "0.1.0").replace(
                release.NOTES_MARKER, "- Maintainer reviewed the release."
            )
            (root / "CHANGELOG.md").write_text(notes)
            self.assertEqual(release.check_plan(root)["version"], "0.1.0")
            (root / "Cargo.lock").write_text(LOCK.replace('version = "0.1.0"', 'version = "0.2.0"'))
            with self.assertRaises(ValueError):
                release.check_plan(root)

    def test_green_gate_uses_exact_sha_and_rejects_newer_failure(self):
        gates = [
            {
                "id": 1,
                "name": "CI Gate",
                "app": {"id": 15368},
                "status": "completed",
                "conclusion": "success",
            }
        ]
        with patch.object(publish_release, "api", return_value={"check_runs": gates}) as api:
            publish_release.require_green("fixture/repo", Path("."), "a" * 40)
            self.assertIn("a" * 40, api.call_args.args[0])
            gates.append({**gates[0], "id": 2, "conclusion": "cancelled"})
            with self.assertRaises(ValueError):
                publish_release.require_green("fixture/repo", Path("."), "a" * 40)

    def test_tag_mismatch_never_overwrites(self):
        ref = {"ref": "refs/tags/v0.1.0", "object": {"type": "commit", "sha": "a" * 40}}
        with patch.object(publish_release, "api", return_value=[ref]):
            self.assertTrue(
                publish_release.verify_tag("fixture/repo", Path("."), "v0.1.0", "a" * 40)
            )
            with self.assertRaises(ValueError):
                publish_release.verify_tag("fixture/repo", Path("."), "v0.1.0", "b" * 40)

    def test_asset_digest_and_exact_file_set(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "snapshot-to-s3-linux-x86_64"
            binary.write_bytes(b"fixture binary")
            checksum = root / "SHA256SUMS"
            checksum.write_text(
                f"{hashlib.sha256(binary.read_bytes()).hexdigest()}  {binary.name}\n"
            )
            self.assertEqual(publish_release.verify_assets(root), (binary, checksum))
            binary.write_bytes(b"damaged")
            with self.assertRaises(ValueError):
                publish_release.verify_assets(root)

    def test_binary_assets_reject_missing_extra_and_archive_files(self):
        for damage in ("missing binary", "missing checksums", "extra", "archive", "wrong name"):
            with self.subTest(damage=damage), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                binary = root / "snapshot-to-s3-linux-x86_64"
                binary.write_bytes(b"fixture binary")
                checksums = root / "SHA256SUMS"
                digest = hashlib.sha256(binary.read_bytes()).hexdigest()
                checksums.write_text(f"{digest}  {binary.name}\n")
                if damage == "missing binary":
                    binary.unlink()
                elif damage == "missing checksums":
                    checksums.unlink()
                elif damage == "extra":
                    (root / "unexpected").write_bytes(b"extra")
                elif damage == "archive":
                    binary.rename(root / "snapshot-to-s3-linux-x86_64.tar.gz")
                    checksums.write_text(f"{digest}  snapshot-to-s3-linux-x86_64.tar.gz\n")
                else:
                    checksums.write_text(f"{digest}  snapshot-to-s3\n")
                with self.assertRaises(ValueError):
                    publish_release.verify_assets(root)

    def test_release_workflow_stages_binary_without_tar(self):
        workflow = (
            Path(__file__).resolve().parents[2] / ".github/workflows/release.yml"
        ).read_text()
        self.assertIn('install -m 0755 "$binary" dist/snapshot-to-s3-linux-x86_64', workflow)
        self.assertIn("(cd dist && sha256sum snapshot-to-s3-linux-x86_64 > SHA256SUMS)", workflow)
        self.assertNotIn("tar -", workflow)
        self.assertNotIn(".tar.gz", workflow)
        self.assertIn(
            '[[ "$("$binary" --version)" == "snapshot-to-s3 $EXPECTED_VERSION" ]]', workflow
        )
        self.assertIn('readelf -l "$binary"', workflow)

    def test_publish_is_manual_and_main_only(self):
        with self.assertRaises(SystemExit), patch("sys.argv", ["x", "select", "--mode", "auto"]):
            publish_release.main()
        workflow = (
            Path(__file__).resolve().parents[2] / ".github/workflows/release.yml"
        ).read_text()
        triggers = workflow.split("\non:\n", 1)[1].split("\npermissions:", 1)[0]
        self.assertIn("workflow_dispatch", triggers)
        self.assertNotIn("build_ref:", triggers)
        self.assertNotIn("actions/cache@", workflow)
        for automatic in ("workflow_run", "push", "pull_request", "schedule"):
            self.assertNotIn(automatic, triggers)
        env = {"GITHUB_REPOSITORY": "fixture/repo", "GITHUB_REF": "refs/heads/feature"}
        with (
            patch.dict(os.environ, env),
            patch.object(publish_release, "api", return_value={"default_branch": "main"}),
            patch.object(publish_release, "output") as output,
        ):
            with self.assertRaises(ValueError):
                publish_release.select(Path.cwd(), "publish")
            output.assert_not_called()

    def test_release_build_only_checks_out_verified_dispatch_ancestors(self):
        workflow = (
            Path(__file__).resolve().parents[2] / ".github/workflows/release.yml"
        ).read_text()
        select, build = workflow.split("\n  select:\n", 1)[1].split("\n  build:\n", 1)
        self.assertIn(
            "if: github.event_name == 'workflow_dispatch' && "
            "github.ref == format('refs/heads/{0}', github.event.repository.default_branch)",
            select,
        )
        build = build.split("\n  publish:\n", 1)[0]
        checkout, verification = build.split(
            "      - name: Verify the frozen release belongs to trusted dispatch history\n", 1
        )
        for job in (select, checkout):
            self.assertIn("ref: ${{ github.sha }}", job)
            self.assertIn("fetch-depth: 0", job)
            self.assertIn("persist-credentials: false", job)
        self.assertIn("RELEASE_SHA: ${{ needs.select.outputs.sha }}", verification)
        script = textwrap.dedent(
            verification.split("        run: |\n", 1)[1].split("      - uses:", 1)[0]
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)

            def git(*args):
                return subprocess.run(
                    ["git", *args], cwd=root, check=True, capture_output=True, text=True
                ).stdout.strip()

            git("init", "-b", "main")
            git("config", "user.name", "Fixture")
            git("config", "user.email", "fixture@example.invalid")
            git("commit", "--allow-empty", "-m", "accepted")
            accepted = git("rev-parse", "HEAD")
            git("commit", "--allow-empty", "-m", "dispatch")
            dispatch = git("rev-parse", "HEAD")
            git("checkout", "--orphan", "unmerged")
            git("commit", "--allow-empty", "-m", "unmerged")
            unmerged = git("rev-parse", "HEAD")
            for sha, succeeds in (
                (accepted, True),
                (dispatch, True),
                (unmerged, False),
                ("", False),
            ):
                with self.subTest(sha=sha):
                    git("checkout", "--detach", dispatch)
                    result = subprocess.run(
                        ["bash", "-euo", "pipefail", "-c", script],
                        cwd=root,
                        env={**os.environ, "RELEASE_SHA": sha},
                        capture_output=True,
                    )
                    self.assertEqual(result.returncode == 0, succeeds)
                    self.assertEqual(git("rev-parse", "HEAD"), sha if succeeds else dispatch)

    def test_build_only_requires_main_and_uses_dispatch_commit(self):
        env = {
            "GITHUB_REPOSITORY": "fixture/repo",
            "GITHUB_REF": "refs/heads/feature",
            "GITHUB_SHA": "a" * 40,
        }
        with (
            patch.dict(os.environ, env),
            patch.object(publish_release, "api", return_value={"default_branch": "main"}),
            patch.object(publish_release, "run") as run,
            patch.object(publish_release, "output") as output,
        ):
            with self.assertRaises(ValueError):
                publish_release.select(Path.cwd(), "build-only")
            run.assert_not_called()
            output.assert_not_called()

        env["GITHUB_REF"] = "refs/heads/main"
        with (
            patch.dict(os.environ, env),
            patch.object(publish_release, "api", return_value={"default_branch": "main"}),
            patch.object(
                publish_release,
                "run",
                side_effect=["a" * 40, '[package]\nversion = "0.1.0"\n'],
            ) as run,
            patch.object(publish_release, "output") as output,
        ):
            publish_release.select(Path.cwd(), "build-only")
            self.assertEqual(run.call_args_list[0].args[0][3], "a" * 40 + "^{commit}")
            self.assertEqual(output.call_args.args[0]["sha"], "a" * 40)

    def test_draft_upload_verification_precedes_publication(self):
        for existing, damage in (
            (existing, damage)
            for existing in (False, True)
            for damage in (None, "digest", "size", "missing", "unexpected")
        ):
            with (
                self.subTest(existing=existing, damage=damage),
                tempfile.TemporaryDirectory() as directory,
            ):
                root = Path(directory)
                binary = root / "snapshot-to-s3-linux-x86_64"
                binary.write_bytes(b"verified fixture")
                sums = root / "SHA256SUMS"
                sums.write_text(
                    f"{hashlib.sha256(binary.read_bytes()).hexdigest()}  {binary.name}\n"
                )
                assets = [
                    {
                        "name": p.name,
                        "size": p.stat().st_size,
                        "digest": f"sha256:{hashlib.sha256(p.read_bytes()).hexdigest()}",
                    }
                    for p in (binary, sums)
                ]
                if damage == "digest":
                    assets[0]["digest"] = "sha256:" + "0" * 64
                elif damage == "size":
                    assets[0]["size"] += 1
                elif damage == "missing":
                    assets.pop()
                elif damage == "unexpected":
                    assets.append({**assets[0], "name": "unexpected"})
                calls = []
                draft = {"id": 123, "tag_name": "v0.1.0", "draft": True}

                def api_reply(path, _root, calls=calls, draft=draft, assets=assets):
                    if path == "repos/fixture/repo":
                        return {"default_branch": "main"}
                    if path == "repos/fixture/repo/releases/tags/v0.1.0":
                        raise subprocess.CalledProcessError(1, ["gh", "api", path])
                    self.assertEqual(path, "repos/fixture/repo/releases/123")
                    if any("--draft=false" in command for command in calls):
                        return {"draft": False, "html_url": "https://example.invalid/release"}
                    return {**draft, "assets": assets}

                env = {"GITHUB_REPOSITORY": "fixture/repo", "GITHUB_REF": "refs/heads/main"}
                with (
                    patch.dict(os.environ, env),
                    patch.object(publish_release, "api", side_effect=api_reply) as api,
                    patch.object(
                        publish_release, "snapshot", return_value=("0.1.0", "- Approved.")
                    ),
                    patch.object(publish_release, "require_green"),
                    patch.object(publish_release, "verify_tag", return_value=True),
                    patch.object(
                        publish_release,
                        "release_records",
                        side_effect=[[draft] if existing else [], [draft]],
                    ),
                    patch.object(
                        publish_release,
                        "run",
                        side_effect=lambda command, *_, calls=calls: calls.append(command),
                    ),
                ):
                    if damage:
                        with self.assertRaises(ValueError):
                            publish_release.publish(root, "a" * 40, root)
                    else:
                        publish_release.publish(root, "a" * 40, root)
                created = [c for c in calls if c[:3] == ["gh", "release", "create"]]
                upload = next(
                    i for i, c in enumerate(calls) if c[:3] == ["gh", "release", "upload"]
                )
                self.assertEqual(
                    calls[upload],
                    ["gh", "release", "upload", "v0.1.0", str(binary), str(sums), "--clobber"],
                )
                if existing:
                    self.assertEqual(created, [])
                    edit = next(
                        i
                        for i, c in enumerate(calls)
                        if c[:3] == ["gh", "release", "edit"] and "--notes-file" in c
                    )
                    self.assertLess(edit, upload)
                else:
                    self.assertEqual(len(created), 1)
                    self.assertIn("--draft", created[0])
                    self.assertLess(calls.index(created[0]), upload)
                published = [c for c in calls if "--draft=false" in c]
                self.assertEqual(len(published), 0 if damage else 1)
                self.assertEqual(
                    [call.args[0] for call in api.call_args_list],
                    ["repos/fixture/repo"]
                    + ["repos/fixture/repo/releases/123"] * (1 if damage else 2),
                )

    def test_missing_ambiguous_or_promoted_draft_stops_publication(self):
        draft = {"id": 123, "tag_name": "v0.1.0", "draft": True}
        for records, response, message in (
            ([], None, "cannot identify exactly one draft release"),
            ([draft, {**draft, "id": 456}], None, "cannot identify exactly one draft release"),
            ([draft], {"draft": False}, "release is no longer a draft"),
        ):
            with self.subTest(records=records, response=response):
                env = {"GITHUB_REPOSITORY": "fixture/repo", "GITHUB_REF": "refs/heads/main"}
                with (
                    patch.dict(os.environ, env),
                    patch.object(
                        publish_release,
                        "api",
                        side_effect=[{"default_branch": "main"}, response],
                    ),
                    patch.object(publish_release, "snapshot", return_value=("0.1.0", "Notes")),
                    patch.object(publish_release, "require_green"),
                    patch.object(publish_release, "verify_tag", return_value=True),
                    patch.object(
                        publish_release,
                        "verify_assets",
                        return_value=(Path("snapshot-to-s3-linux-x86_64"), Path("SHA256SUMS")),
                    ),
                    patch.object(publish_release, "release_records", side_effect=[[], records]),
                    patch.object(publish_release, "run") as run,
                ):
                    with self.assertRaisesRegex(ValueError, message):
                        publish_release.publish(Path.cwd(), "a" * 40, Path.cwd())
                    self.assertFalse(
                        any("--draft=false" in call.args[0] for call in run.call_args_list)
                    )

    def test_published_release_is_never_overwritten(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "snapshot-to-s3-linux-x86_64"
            binary.write_bytes(b"fixture")
            (root / "SHA256SUMS").write_text(
                f"{hashlib.sha256(binary.read_bytes()).hexdigest()}  {binary.name}\n"
            )
            env = {"GITHUB_REPOSITORY": "fixture/repo", "GITHUB_REF": "refs/heads/main"}
            with (
                patch.dict(os.environ, env),
                patch.object(publish_release, "api", return_value={"default_branch": "main"}),
                patch.object(publish_release, "snapshot", return_value=("0.1.0", "- Approved.")),
                patch.object(publish_release, "require_green"),
                patch.object(publish_release, "verify_tag", return_value=True),
                patch.object(
                    publish_release,
                    "release_records",
                    return_value=[{"tag_name": "v0.1.0", "draft": False}],
                ),
                patch.object(publish_release, "run") as run,
            ):
                publish_release.publish(root, "a" * 40, root)
                self.assertFalse(
                    any(call.args[0][:2] == ["gh", "release"] for call in run.call_args_list)
                )

    def test_partial_preparation_reuses_branch_and_preserves_manual_notes(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            remote = base / "remote.git"
            seed = base / "seed"
            seed.mkdir()
            self.fixture(seed)

            def git(args, cwd=seed):
                return subprocess.run(
                    ["git", *args],
                    cwd=cwd,
                    check=True,
                    text=True,
                    capture_output=True,
                ).stdout.strip()

            git(["init", "--bare", str(remote)], base)
            git(["init", "-b", "main"])
            git(["config", "user.name", "Fixture"])
            git(["config", "user.email", "fixture@example.invalid"])
            git(["add", "."])
            git(["commit", "-m", "plain commit without conventional syntax"])
            git(["remote", "add", "origin", str(remote)])
            git(["push", "origin", "main"])
            first = base / "first"
            git(["clone", "--branch", "main", str(remote), str(first)], base)
            real_run = release.run
            state = {"prs": [], "creates": 0}

            def runner(command, root, capture=False):
                if command[:3] == ["gh", "pr", "list"]:
                    return json.dumps(state["prs"])
                if command[:3] == ["gh", "pr", "create"]:
                    state["creates"] += 1
                    state["prs"] = [
                        {
                            "number": 1,
                            "headRefName": "automation/release-v0.1.0",
                            "url": "https://example.invalid/pr/1",
                        }
                    ]
                    return ""
                if command[:3] == ["gh", "workflow", "run"]:
                    return ""
                if command[:3] == ["gh", "run", "list"]:
                    return "[]"
                return real_run(command, root, capture)

            env = {
                "GITHUB_REPOSITORY": "fixture/repo",
                "GITHUB_ACTIONS": "true",
                "GITHUB_REF": "refs/heads/main",
            }
            with (
                patch.dict(os.environ, env),
                patch.object(release, "api", return_value={"default_branch": "main"}),
                patch.object(release, "release_records", return_value=[]),
                patch.object(release, "run", side_effect=runner),
            ):
                release.prepare(first, "initial", "stable", False)
                text = (
                    (first / "CHANGELOG.md")
                    .read_text()
                    .replace(
                        release.NOTES_MARKER, "- Handwritten release notes must survive retries."
                    )
                )
                (first / "CHANGELOG.md").write_text(text)
                git(["add", "CHANGELOG.md"], first)
                git(["commit", "-m", "maintainer edits notes"], first)
                git(["push", "origin", "HEAD"], first)
                saved = git(["rev-parse", "HEAD"], first)
                second = base / "second"
                git(["clone", "--branch", "main", str(remote), str(second)], base)
                release.prepare(second, "initial", "stable", False)
                self.assertEqual(git(["rev-parse", "HEAD"], second), saved)
                self.assertEqual((second / "CHANGELOG.md").read_text(), text)
                self.assertEqual(state["creates"], 1)
                self.assertEqual(release.check_plan(second)["version"], "0.1.0")


if __name__ == "__main__":
    unittest.main()
