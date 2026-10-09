import argparse
import datetime as dt
import tempfile
import unittest
from pathlib import Path

from s3_probe import load_env_file, resolve_settings, sign, validate_delete_key

# AWS SigV4 documentation example: "GET Object" with a Range header.
AWS_EXAMPLE = {
    "access_key": "AKIAIOSFODNN7EXAMPLE",
    "secret_key": "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
    "region": "us-east-1",
    "now": dt.datetime(2013, 5, 24, tzinfo=dt.UTC),
}
EMPTY_SHA256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"


class SigningTests(unittest.TestCase):
    def test_matches_aws_documentation_example(self):
        headers = {
            "host": "examplebucket.s3.amazonaws.com",
            "range": "bytes=0-9",
            "x-amz-content-sha256": EMPTY_SHA256,
            "x-amz-date": "20130524T000000Z",
        }
        authorization = sign("GET", "/test.txt", "", headers, EMPTY_SHA256, **AWS_EXAMPLE)
        self.assertEqual(
            authorization,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/"
            "aws4_request, SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, "
            "Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41",
        )


class DeleteGuardTests(unittest.TestCase):
    def test_accepts_exact_test_owned_keys(self):
        for key in ("smoke_1/pool/s1/stream.encrypted", "live-http-x/object"):
            with self.subTest(key=key):
                validate_delete_key(key)

    def test_rejects_roots_prefixes_wildcards_and_foreign_keys(self):
        for key in (
            "",
            "/",
            "/smoke_1/object",
            "smoke_1/",
            "smoke_1/*",
            "smoke_1/obj?",
            "backups/object",
            "probe/smoke_1/object",
        ):
            with self.subTest(key=key), self.assertRaises(ValueError):
                validate_delete_key(key)


class SettingsTests(unittest.TestCase):
    def args(self, **values):
        names = ("endpoint", "region", "access_key", "secret_key", "bucket")
        return argparse.Namespace(**{name: values.get(name) for name in names})

    def test_env_file_parsing(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "local_s3.env"
            path.write_text(
                "# comment\n\nexport LOCAL_S3_ENDPOINT='http://127.0.0.1:1'\n"
                'AWS_REGION="eu-west-1"\nnot an assignment\n'
            )
            self.assertEqual(
                load_env_file(path),
                {"LOCAL_S3_ENDPOINT": "http://127.0.0.1:1", "AWS_REGION": "eu-west-1"},
            )

    def test_precedence_is_flag_then_file_then_environment(self):
        file_values = {"LOCAL_S3_ENDPOINT": "http://file", "AWS_REGION": "file-region"}
        environ = {
            "TEST_S3_ENDPOINT": "http://env",
            "AWS_REGION": "env-region",
            "AWS_ACCESS_KEY_ID": "env-access",
            "AWS_SECRET_ACCESS_KEY": "env-secret",
            "TEST_S3_BUCKET": "env-bucket",
        }
        settings = resolve_settings(self.args(region="flag-region"), file_values, environ)
        self.assertEqual(
            settings,
            {
                "endpoint": "http://file",
                "region": "flag-region",
                "access_key": "env-access",
                "secret_key": "env-secret",
                "bucket": "env-bucket",
            },
        )

    def test_bucket_defaults_and_missing_values_stay_empty(self):
        settings = resolve_settings(self.args(), {}, {})
        self.assertEqual(settings["bucket"], "test-bucket")
        self.assertIsNone(settings["endpoint"])


if __name__ == "__main__":
    unittest.main()
