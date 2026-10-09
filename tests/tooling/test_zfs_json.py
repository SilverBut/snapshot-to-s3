import io
import json
import sys
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "e2e" / "lib"))

import zfs_json


def pool(name, health="ONLINE", state="ONLINE"):
    return {
        "name": name,
        "type": "POOL",
        "state": state,
        "properties": {"health": {"value": health}},
    }


def dataset(name, **properties):
    kind = "SNAPSHOT" if "@" in name else "FILESYSTEM"
    return {
        "name": name,
        "type": kind,
        "properties": {key: {"value": value} for key, value in properties.items()},
    }


def run(argv, document):
    stdout, stderr = io.StringIO(), io.StringIO()
    with redirect_stdout(stdout), redirect_stderr(stderr):
        sys.stdin = io.StringIO(json.dumps(document))
        try:
            zfs_json.main(argv)
            code = 0
        except SystemExit as exit_:
            code, message = 1, str(exit_.code)
        finally:
            sys.stdin = sys.__stdin__
    return code, stdout.getvalue() if code == 0 else message


class ZfsJsonTests(unittest.TestCase):
    def test_online_lists_only_fully_online_pools(self):
        document = {
            "pools": {
                "a": pool("a"),
                "b": pool("b", health="DEGRADED"),
                "c": pool("c", state="SUSPENDED"),
                "d": pool("d"),
            }
        }
        self.assertEqual(run(["zpool", "list", "online"], document), (0, "a\nd\n"))

    def test_online_rejects_missing_state(self):
        broken = pool("a")
        del broken["state"]
        code, message = run(["zpool", "list", "online"], {"pools": {"a": broken}})
        self.assertEqual(code, 1)
        self.assertEqual(message, "invalid OpenZFS JSON: missing pool state")

    def test_absent_fails_when_namespace_exists(self):
        document = {"datasets": {"p/x": dataset("p/x")}}
        self.assertEqual(run(["zfs", "list", "absent", "p/y"], document), (0, ""))
        self.assertEqual(
            run(["zfs", "list", "absent", "p/x"], document),
            (1, "invalid OpenZFS JSON: test namespace already exists: p/x"),
        )

    def test_value_prints_validated_property(self):
        document = {"datasets": {"p/x@s1": dataset("p/x@s1", guid="18446744073709551615")}}
        self.assertEqual(
            run(["zfs", "get", "value", "p/x@s1", "guid"], document), (0, "18446744073709551615\n")
        )

    def test_value_rejects_invalid_guids(self):
        for guid in ("0", "012", "18446744073709551616", "-1", "1a"):
            with self.subTest(guid=guid):
                document = {"datasets": {"p/x@s1": dataset("p/x@s1", guid=guid)}}
                self.assertEqual(
                    run(["zfs", "get", "value", "p/x@s1", "guid"], document),
                    (1, "invalid OpenZFS JSON: invalid decimal GUID"),
                )

    def test_value_rejects_malformed_properties(self):
        for value in ("", "a\nb", "a\rb", 5, None):
            with self.subTest(value=value):
                document = {"datasets": {"p/x": dataset("p/x", mounted=value)}}
                self.assertEqual(
                    run(["zfs", "get", "value", "p/x", "mounted"], document),
                    (1, "invalid OpenZFS JSON: missing, empty or malformed property mounted"),
                )

    def test_value_requires_the_dataset_type_implied_by_the_name(self):
        snapshot_as_filesystem = dataset("p/x@s1", guid="1")
        snapshot_as_filesystem["type"] = "FILESYSTEM"
        document = {"datasets": {"p/x@s1": snapshot_as_filesystem}}
        self.assertEqual(
            run(["zfs", "get", "value", "p/x@s1", "guid"], document),
            (1, "invalid OpenZFS JSON: unexpected requested dataset type"),
        )

    def test_rejects_structural_problems(self):
        renamed = dataset("p/x")
        renamed["name"] = "p/other"
        bad_type = dataset("p/x")
        bad_type["type"] = "BOOKMARK"
        for label, argv, document, message in (
            (
                "not a map",
                ["zfs", "list", "absent", "p/y"],
                {"datasets": []},
                "expected a named object map",
            ),
            (
                "key/name",
                ["zfs", "list", "absent", "p/y"],
                {"datasets": {"p/x": renamed}},
                "object map key/name mismatch",
            ),
            (
                "type",
                ["zfs", "list", "absent", "p/y"],
                {"datasets": {"p/x": bad_type}},
                "invalid object type",
            ),
            (
                "pool type",
                ["zpool", "list", "online"],
                {"pools": {"p/x": dataset("p/x")}},
                "invalid object type",
            ),
            ("mode", ["zfs", "list", "bogus"], {"datasets": {}}, "unknown JSON reader mode"),
        ):
            with self.subTest(label):
                self.assertEqual(run(argv, document), (1, "invalid OpenZFS JSON: " + message))

    def test_missing_keys_and_arguments_fail_cleanly(self):
        for argv, document in (
            (["zfs", "get", "value", "p/missing", "guid"], {"datasets": {}}),
            (["zfs", "list", "absent"], {"datasets": {}}),
            (["zpool"], {"pools": {}}),
            (["zfs", "list", "absent", "p/x"], {"pools": {}}),
        ):
            with self.subTest(argv=argv):
                code, message = run(argv, document)
                self.assertEqual(code, 1)
                self.assertTrue(message.startswith("invalid OpenZFS JSON: "), message)


if __name__ == "__main__":
    unittest.main()
