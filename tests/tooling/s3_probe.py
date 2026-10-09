#!/usr/bin/env python3
"""Probe an S3-compatible endpoint with dependency-free SigV4 requests.

Default mode exercises the features snapshot-to-s3 relies on (user metadata,
ranged GET, `If-None-Match: *` creates, multipart complete/abort) under a
unique `probe/<id>/` prefix and prints a JSON report. `--delete-test-object`
instead deletes exactly one object left by an E2E or live test.
"""

import argparse
import datetime as dt
import hashlib
import hmac
import http.client
import json
import os
import threading
import urllib.parse
import uuid
import xml.etree.ElementTree as ET
from collections.abc import Mapping
from contextlib import closing
from pathlib import Path

UNRESERVED = "-_.~"
MULTIPART_PART_SIZE = 5 * 1024 * 1024 + 1  # just above S3's minimum non-final part
CONCURRENT_CREATES = 8
DELETABLE_PREFIXES = ("smoke_", "live-http-")

Response = tuple[int, dict[str, str], bytes]


def _hmac(key: bytes, message: str) -> bytes:
    return hmac.new(key, message.encode(), hashlib.sha256).digest()


def signing_key(secret: str, date_stamp: str, region: str, service: str = "s3") -> bytes:
    key = _hmac(("AWS4" + secret).encode(), date_stamp)
    for part in (region, service, "aws4_request"):
        key = _hmac(key, part)
    return key


def canonical_query(query: str) -> str:
    pairs = urllib.parse.parse_qsl(query, keep_blank_values=True)
    return "&".join(
        f"{urllib.parse.quote(k, safe=UNRESERVED)}={urllib.parse.quote(v, safe=UNRESERVED)}"
        for k, v in sorted(pairs)
    )


def sign(
    method: str,
    path: str,
    query: str,
    headers: Mapping[str, str],
    payload_hash: str,
    *,
    now: dt.datetime,
    region: str,
    access_key: str,
    secret_key: str,
) -> str:
    """Return the SigV4 `Authorization` value; `headers` must include host and x-amz-*."""
    amz_date = now.strftime("%Y%m%dT%H%M%SZ")
    date_stamp = now.strftime("%Y%m%d")
    ordered = sorted((k.lower(), str(v).strip()) for k, v in headers.items())
    signed_headers = ";".join(k for k, _ in ordered)
    canonical_request = "\n".join(
        [
            method,
            path,
            canonical_query(query),
            "".join(f"{k}:{v}\n" for k, v in ordered),
            signed_headers,
            payload_hash,
        ]
    )
    scope = f"{date_stamp}/{region}/s3/aws4_request"
    string_to_sign = "\n".join(
        [
            "AWS4-HMAC-SHA256",
            amz_date,
            scope,
            hashlib.sha256(canonical_request.encode()).hexdigest(),
        ]
    )
    signature = hmac.new(
        signing_key(secret_key, date_stamp, region), string_to_sign.encode(), hashlib.sha256
    ).hexdigest()
    return (
        f"AWS4-HMAC-SHA256 Credential={access_key}/{scope}, "
        f"SignedHeaders={signed_headers}, Signature={signature}"
    )


class SigV4S3Client:
    """Minimal path-style S3 client for plain-HTTP local endpoints."""

    def __init__(self, endpoint: str, region: str, access_key: str, secret_key: str):
        parsed = urllib.parse.urlparse(endpoint)
        if parsed.scheme != "http":
            raise ValueError("Only http:// endpoints are supported for local probe")
        self.host = parsed.hostname or "127.0.0.1"
        self.port = parsed.port or 80
        self.region = region
        self.access_key = access_key
        self.secret_key = secret_key

    def request(
        self,
        method: str,
        path: str,
        query: str = "",
        headers: Mapping[str, str] | None = None,
        body: bytes = b"",
    ) -> Response:
        now = dt.datetime.now(dt.UTC)
        payload_hash = hashlib.sha256(body).hexdigest()
        signed = {k.lower(): v for k, v in (headers or {}).items()}
        signed["host"] = f"{self.host}:{self.port}"
        signed["x-amz-date"] = now.strftime("%Y%m%dT%H%M%SZ")
        signed["x-amz-content-sha256"] = payload_hash
        authorization = sign(
            method,
            path,
            query,
            signed,
            payload_hash,
            now=now,
            region=self.region,
            access_key=self.access_key,
            secret_key=self.secret_key,
        )
        target = path + (f"?{query}" if query else "")
        with closing(http.client.HTTPConnection(self.host, self.port, timeout=20)) as conn:
            conn.request(
                method, target, body=body, headers={**signed, "Authorization": authorization}
            )
            response = conn.getresponse()
            payload = response.read()
            return response.status, {k.lower(): v for k, v in response.getheaders()}, payload


def expect(status: int, allowed: set[int], msg: str) -> None:
    if status not in allowed:
        raise RuntimeError(f"{msg}: status={status}, expected one of {sorted(allowed)}")


def _upload_id(payload: bytes) -> str:
    upload_id = ET.fromstring(payload).findtext(".//{*}UploadId")
    if not upload_id:
        raise RuntimeError("missing multipart upload id")
    return upload_id


def probe_metadata_and_range(client: SigV4S3Client, bucket: str, key: str) -> None:
    body = b"0123456789-meta-body"
    headers = {"x-amz-meta-color": "blue", "content-type": "text/plain"}
    st, _, _ = client.request("PUT", f"/{bucket}/{key}", headers=headers, body=body)
    expect(st, {200}, "put metadata object")
    st, headers, got = client.request("GET", f"/{bucket}/{key}")
    expect(st, {200}, "get metadata object")
    if got != body:
        raise RuntimeError("metadata object content mismatch")
    if headers.get("x-amz-meta-color") != "blue":
        raise RuntimeError("metadata roundtrip mismatch for x-amz-meta-color")
    st, _, got = client.request("GET", f"/{bucket}/{key}", headers={"range": "bytes=2-5"})
    expect(st, {206}, "range read")
    if got != body[2:6]:
        raise RuntimeError("range body mismatch")


def probe_conditional_create(
    client: SigV4S3Client, bucket: str, existing_key: str, race_key: str
) -> tuple[int, list[int]]:
    """Return the status of a create over `existing_key` and of racing creates."""
    st, _, _ = client.request(
        "PUT", f"/{bucket}/{existing_key}", headers={"if-none-match": "*"}, body=b"new"
    )
    statuses: list[int] = []
    lock = threading.Lock()

    def create(index: int) -> None:
        status, _, _ = client.request(
            "PUT",
            f"/{bucket}/{race_key}",
            headers={"if-none-match": "*"},
            body=f"body-{index}".encode(),
        )
        with lock:
            statuses.append(status)

    threads = [threading.Thread(target=create, args=(i,)) for i in range(CONCURRENT_CREATES)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    return st, sorted(statuses)


def probe_multipart_complete(client: SigV4S3Client, bucket: str, key: str) -> None:
    part1 = b"a" * MULTIPART_PART_SIZE
    part2 = b"b" * (1024 * 1024)
    st, _, payload = client.request("POST", f"/{bucket}/{key}", "uploads=")
    expect(st, {200}, "init multipart")
    upload_id = _upload_id(payload)
    parts = []
    for number, data in ((1, part1), (2, part2)):
        query = urllib.parse.urlencode({"partNumber": str(number), "uploadId": upload_id})
        st, headers, _ = client.request("PUT", f"/{bucket}/{key}", query, body=data)
        expect(st, {200}, f"upload part {number}")
        etag = headers.get("etag", "").strip('"')
        if not etag:
            raise RuntimeError(f"missing etag for part {number}")
        parts.append((number, etag))
    complete = "".join(
        f'<Part><PartNumber>{n}</PartNumber><ETag>"{etag}"</ETag></Part>' for n, etag in parts
    )
    st, _, _ = client.request(
        "POST",
        f"/{bucket}/{key}",
        urllib.parse.urlencode({"uploadId": upload_id}),
        headers={"content-type": "application/xml"},
        body=f"<CompleteMultipartUpload>{complete}</CompleteMultipartUpload>".encode(),
    )
    expect(st, {200}, "complete multipart")
    st, _, full = client.request("GET", f"/{bucket}/{key}")
    expect(st, {200}, "get multipart object")
    if len(full) != len(part1) + len(part2) or full[:4] != b"aaaa" or full[-4:] != b"bbbb":
        raise RuntimeError("multipart complete content mismatch")


def probe_multipart_abort(client: SigV4S3Client, bucket: str, key: str) -> None:
    st, _, payload = client.request("POST", f"/{bucket}/{key}", "uploads=")
    expect(st, {200}, "init abort multipart")
    upload_id = _upload_id(payload)
    query = urllib.parse.urlencode({"partNumber": "1", "uploadId": upload_id})
    st, _, _ = client.request("PUT", f"/{bucket}/{key}", query, body=b"a" * MULTIPART_PART_SIZE)
    expect(st, {200}, "upload abort part")
    st, _, _ = client.request(
        "DELETE", f"/{bucket}/{key}", urllib.parse.urlencode({"uploadId": upload_id})
    )
    expect(st, {204}, "abort multipart")


def run_probe(client: SigV4S3Client, bucket: str) -> dict:
    prefix = f"probe/{uuid.uuid4().hex[:12]}"
    st, _, _ = client.request("PUT", f"/{bucket}")
    expect(st, {200, 409}, "create bucket")
    results: dict = {"create_bucket_status": st}
    probe_metadata_and_range(client, bucket, f"{prefix}/meta.txt")
    results["metadata_roundtrip"] = "ok"
    results["range"] = "ok"
    existing, concurrent = probe_conditional_create(
        client, bucket, f"{prefix}/meta.txt", f"{prefix}/race-key"
    )
    results["conditional_existing_status"] = existing
    results["conditional_concurrent_statuses"] = concurrent
    results["conditional_concurrent_success_count"] = concurrent.count(200)
    probe_multipart_complete(client, bucket, f"{prefix}/multi.bin")
    results["multipart_complete"] = "ok"
    probe_multipart_abort(client, bucket, f"{prefix}/abort.bin")
    results["multipart_abort"] = "ok"
    results["supports_atomic_conditional_create"] = (
        existing in {409, 412} and concurrent.count(200) == 1
    )
    return results


def validate_delete_key(key: str) -> None:
    """Allow only one exact object key under a test-owned first path segment."""
    if not key or key == "/" or key.startswith("/"):
        raise ValueError("delete key must be a non-empty object key (not bucket root)")
    if "*" in key or "?" in key:
        raise ValueError("delete key must be an exact object key (wildcards are not allowed)")
    if key.endswith("/"):
        raise ValueError(
            "delete key ending with '/' looks like a prefix; exact object key required"
        )
    if not key.split("/", 1)[0].startswith(DELETABLE_PREFIXES):
        raise ValueError("delete key first path segment must start with 'smoke_' or 'live-http-'")


def load_env_file(path: Path) -> dict[str, str]:
    """Parse simple `[export ]NAME=value` lines such as local_s3.sh's local_s3.env."""
    values = {}
    for raw in path.read_text().splitlines():
        line = raw.strip().removeprefix("export ")
        if not line or line.startswith("#") or "=" not in line:
            continue
        name, value = line.split("=", 1)
        values[name] = value.strip().strip("'\"")
    return values


# Each setting is the first non-empty value among its CLI flag and these names,
# looked up in --env-file and then the process environment.
SETTINGS: dict[str, tuple[str, ...]] = {
    "endpoint": ("LOCAL_S3_ENDPOINT", "TEST_S3_ENDPOINT"),
    "region": ("AWS_REGION", "AWS_DEFAULT_REGION"),
    "access_key": ("AWS_ACCESS_KEY_ID",),
    "secret_key": ("AWS_SECRET_ACCESS_KEY",),
    "bucket": ("LOCAL_S3_BUCKET", "TEST_S3_BUCKET"),
}


def resolve_settings(
    args: argparse.Namespace, file_values: Mapping[str, str], environ: Mapping[str, str]
) -> dict[str, str | None]:
    settings: dict[str, str | None] = {}
    for name, sources in SETTINGS.items():
        candidates = [getattr(args, name)]
        for source in (file_values, environ):
            candidates += [source.get(variable) for variable in sources]
        settings[name] = next((value for value in candidates if value), None)
    settings["bucket"] = settings["bucket"] or "test-bucket"
    return settings


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Probe local S3 compatibility with stdlib SigV4 requests"
    )
    parser.add_argument("--env-file", help="Path to local_s3.env generated by local_s3.sh")
    parser.add_argument("--endpoint")
    parser.add_argument("--region")
    parser.add_argument("--access-key")
    parser.add_argument("--secret-key")
    parser.add_argument("--bucket")
    parser.add_argument(
        "--delete-test-object",
        metavar="KEY",
        help="Delete exactly one test object key; "
        "first segment must start with smoke_ or live-http-",
    )
    parser.add_argument("--output-json", help="Optional output path for probe result JSON")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> None:
    args = parse_args(argv)
    file_values = load_env_file(Path(args.env_file)) if args.env_file else {}
    settings = resolve_settings(args, file_values, os.environ)
    missing = [name.replace("_", "-") for name, value in settings.items() if not value]
    if missing:
        raise SystemExit(f"Missing required args: {', '.join(missing)}")
    bucket = settings.pop("bucket")
    client = SigV4S3Client(**settings)

    if args.delete_test_object:
        key = args.delete_test_object
        validate_delete_key(key)
        status, _, _ = client.request("DELETE", f"/{bucket}/{key}")
        expect(status, {204}, f"delete object '{key}'")
        print(json.dumps({"deleted_key": key, "status": "ok"}, indent=2))
        return

    results = run_probe(client, bucket)
    if args.output_json:
        Path(args.output_json).write_text(json.dumps(results, indent=2))
    print(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
