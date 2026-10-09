#!/usr/bin/env python3
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
from pathlib import Path


def _sign(key: bytes, msg: str) -> bytes:
    return hmac.new(key, msg.encode(), hashlib.sha256).digest()


def _signing_key(secret: str, date_stamp: str, region: str, service: str = "s3") -> bytes:
    k_date = _sign(("AWS4" + secret).encode(), date_stamp)
    k_region = hmac.new(k_date, region.encode(), hashlib.sha256).digest()
    k_service = hmac.new(k_region, service.encode(), hashlib.sha256).digest()
    return hmac.new(k_service, b"aws4_request", hashlib.sha256).digest()


class SigV4S3Client:
    def __init__(self, endpoint: str, region: str, access_key: str, secret_key: str):
        parsed = urllib.parse.urlparse(endpoint)
        if parsed.scheme != "http":
            raise ValueError("Only http:// endpoints are supported for local probe")
        self.host = parsed.hostname or "127.0.0.1"
        self.port = parsed.port or 80
        self.region = region
        self.access_key = access_key
        self.secret_key = secret_key

    def request(self, method: str, path: str, query: str = "", headers=None, body: bytes = b""):
        if headers is None:
            headers = {}
        if isinstance(body, str):
            body = body.encode()
        t = dt.datetime.now(dt.UTC)
        amz_date = t.strftime("%Y%m%dT%H%M%SZ")
        date_stamp = t.strftime("%Y%m%d")
        payload_hash = hashlib.sha256(body).hexdigest()

        headers = {k.lower(): v for k, v in headers.items()}
        headers["host"] = f"{self.host}:{self.port}"
        headers["x-amz-date"] = amz_date
        headers["x-amz-content-sha256"] = payload_hash

        qp = urllib.parse.parse_qsl(query, keep_blank_values=True)
        canonical_query = "&".join(
            f"{urllib.parse.quote(k, safe='-_.~')}={urllib.parse.quote(v, safe='-_.~')}"
            for k, v in sorted(qp)
        )
        canonical_headers = "".join(f"{k}:{str(v).strip()}\n" for k, v in sorted(headers.items()))
        signed_headers = ";".join(k for k, _ in sorted(headers.items()))

        canonical_request = "\n".join([
            method,
            path,
            canonical_query,
            canonical_headers,
            signed_headers,
            payload_hash,
        ])

        credential_scope = f"{date_stamp}/{self.region}/s3/aws4_request"
        string_to_sign = "\n".join([
            "AWS4-HMAC-SHA256",
            amz_date,
            credential_scope,
            hashlib.sha256(canonical_request.encode()).hexdigest(),
        ])

        signature = hmac.new(
            _signing_key(self.secret_key, date_stamp, self.region),
            string_to_sign.encode(),
            hashlib.sha256,
        ).hexdigest()

        auth = (
            f"AWS4-HMAC-SHA256 Credential={self.access_key}/{credential_scope}, "
            f"SignedHeaders={signed_headers}, Signature={signature}"
        )

        send_headers = dict(headers)
        send_headers["Authorization"] = auth

        conn = http.client.HTTPConnection(self.host, self.port, timeout=20)
        target = path + (("?" + query) if query else "")
        conn.request(method, target, body=body, headers=send_headers)
        resp = conn.getresponse()
        resp_body = resp.read()
        resp_headers = {k.lower(): v for k, v in resp.getheaders()}
        conn.close()
        return resp.status, resp_headers, resp_body


def expect(status: int, allowed: set[int], msg: str):
    if status not in allowed:
        raise RuntimeError(f"{msg}: status={status}, expected one of {sorted(allowed)}")


def run_probe(client: SigV4S3Client, bucket: str):
    results = {}
    run_id = uuid.uuid4().hex[:12]
    meta_key = f"probe/{run_id}/meta.txt"
    race_key = f"probe/{run_id}/race-key"
    multi_key = f"probe/{run_id}/multi.bin"
    abort_key = f"probe/{run_id}/abort.bin"

    st, _, _ = client.request("PUT", f"/{bucket}")
    expect(st, {200, 409}, "create bucket")
    results["create_bucket_status"] = st

    body = b"0123456789-meta-body"
    st, _, _ = client.request(
        "PUT",
        f"/{bucket}/{meta_key}",
        headers={"x-amz-meta-color": "blue", "content-type": "text/plain"},
        body=body,
    )
    expect(st, {200}, "put metadata object")

    st, headers, got = client.request("GET", f"/{bucket}/{meta_key}")
    expect(st, {200}, "get metadata object")
    if got != body:
        raise RuntimeError("metadata object content mismatch")
    if headers.get("x-amz-meta-color") != "blue":
        raise RuntimeError("metadata roundtrip mismatch for x-amz-meta-color")
    results["metadata_roundtrip"] = "ok"

    st, _, got = client.request("GET", f"/{bucket}/{meta_key}", headers={"range": "bytes=2-5"})
    expect(st, {206}, "range read")
    if got != body[2:6]:
        raise RuntimeError("range body mismatch")
    results["range"] = "ok"

    st, _, _ = client.request("PUT", f"/{bucket}/{meta_key}", headers={"if-none-match": "*"}, body=b"new")
    results["conditional_existing_status"] = st

    statuses = []
    lock = threading.Lock()

    def worker(i: int):
        status, _, _ = client.request(
            "PUT",
            f"/{bucket}/{race_key}",
            headers={"if-none-match": "*"},
            body=f"body-{i}".encode(),
        )
        with lock:
            statuses.append(status)

    threads = [threading.Thread(target=worker, args=(i,)) for i in range(8)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    results["conditional_concurrent_statuses"] = sorted(statuses)
    results["conditional_concurrent_success_count"] = sum(1 for s in statuses if s == 200)

    part1 = b"a" * (5 * 1024 * 1024 + 1)
    part2 = b"b" * (1024 * 1024)

    st, _, payload = client.request("POST", f"/{bucket}/{multi_key}", "uploads=")
    expect(st, {200}, "init multipart")
    upload_id = ET.fromstring(payload).findtext(".//{*}UploadId")
    if not upload_id:
        raise RuntimeError("missing multipart upload id")

    parts = []
    for part_number, data in ((1, part1), (2, part2)):
        q = urllib.parse.urlencode({"partNumber": str(part_number), "uploadId": upload_id})
        st, h, _ = client.request("PUT", f"/{bucket}/{multi_key}", q, body=data)
        expect(st, {200}, f"upload part {part_number}")
        etag = h.get("etag", "").strip('"')
        if not etag:
            raise RuntimeError(f"missing etag for part {part_number}")
        parts.append((part_number, etag))

    complete_xml = "<CompleteMultipartUpload>" + "".join(
        f"<Part><PartNumber>{n}</PartNumber><ETag>\"{etag}\"</ETag></Part>" for n, etag in parts
    ) + "</CompleteMultipartUpload>"

    st, _, _ = client.request(
        "POST",
        f"/{bucket}/{multi_key}",
        urllib.parse.urlencode({"uploadId": upload_id}),
        headers={"content-type": "application/xml"},
        body=complete_xml.encode(),
    )
    expect(st, {200}, "complete multipart")

    st, _, full = client.request("GET", f"/{bucket}/{multi_key}")
    expect(st, {200}, "get multipart object")
    if len(full) != len(part1) + len(part2) or full[:4] != b"aaaa" or full[-4:] != b"bbbb":
        raise RuntimeError("multipart complete content mismatch")
    results["multipart_complete"] = "ok"

    st, _, payload = client.request("POST", f"/{bucket}/{abort_key}", "uploads=")
    expect(st, {200}, "init abort multipart")
    upload_id2 = ET.fromstring(payload).findtext(".//{*}UploadId")
    q = urllib.parse.urlencode({"partNumber": "1", "uploadId": upload_id2})
    st, _, _ = client.request("PUT", f"/{bucket}/{abort_key}", q, body=part1)
    expect(st, {200}, "upload abort part")
    st, _, _ = client.request("DELETE", f"/{bucket}/{abort_key}", urllib.parse.urlencode({"uploadId": upload_id2}))
    expect(st, {204}, "abort multipart")
    results["multipart_abort"] = "ok"

    results["supports_atomic_conditional_create"] = (
        results["conditional_existing_status"] in {409, 412}
        and results["conditional_concurrent_success_count"] == 1
    )

    return results


def _validate_delete_key(key: str) -> None:
    if not key or key == "/" or key.startswith("/"):
        raise ValueError("delete key must be a non-empty object key (not bucket root)")
    if "*" in key or "?" in key:
        raise ValueError("delete key must be an exact object key (wildcards are not allowed)")
    if key.endswith("/"):
        raise ValueError("delete key ending with '/' looks like a prefix; exact object key required")
    first_segment = key.split("/", 1)[0]
    if not (first_segment.startswith("smoke_") or first_segment.startswith("live-http-")):
        raise ValueError(
            "delete key first path segment must start with 'smoke_' or 'live-http-'"
        )


def delete_test_object(
    endpoint: str,
    bucket: str,
    region: str,
    access: str,
    secret: str,
    key: str,
) -> None:
    _validate_delete_key(key)
    client = SigV4S3Client(endpoint=endpoint, region=region, access_key=access, secret_key=secret)
    status, _, _ = client.request("DELETE", f"/{bucket}/{key}")
    expect(status, {204}, f"delete object '{key}'")


def load_env_file(path: Path):
    out = {}
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("export "):
            line = line[len("export "):]
        if "=" not in line:
            continue
        k, v = line.split("=", 1)
        out[k] = v.strip().strip("'\"")
    return out


def main():
    p = argparse.ArgumentParser(description="Probe local S3 compatibility with stdlib SigV4 requests")
    p.add_argument("--env-file", help="Path to local_s3.env generated by local_s3.sh")
    p.add_argument("--endpoint")
    p.add_argument("--region")
    p.add_argument("--access-key")
    p.add_argument("--secret-key")
    p.add_argument("--bucket")
    p.add_argument(
        "--delete-test-object",
        metavar="KEY",
        help="Delete exactly one test object key; first segment must start with smoke_ or live-http-",
    )
    p.add_argument("--output-json", help="Optional output path for probe result JSON")
    args = p.parse_args()

    values = {}
    if args.env_file:
        values.update(load_env_file(Path(args.env_file)))

    endpoint = args.endpoint or values.get("LOCAL_S3_ENDPOINT") or os.environ.get("LOCAL_S3_ENDPOINT") or os.environ.get("TEST_S3_ENDPOINT")
    region = args.region or values.get("AWS_REGION") or values.get("AWS_DEFAULT_REGION") or os.environ.get("AWS_REGION") or os.environ.get("AWS_DEFAULT_REGION")
    access_key = args.access_key or values.get("AWS_ACCESS_KEY_ID") or os.environ.get("AWS_ACCESS_KEY_ID")
    secret_key = args.secret_key or values.get("AWS_SECRET_ACCESS_KEY") or os.environ.get("AWS_SECRET_ACCESS_KEY")
    bucket = args.bucket or values.get("LOCAL_S3_BUCKET") or os.environ.get("LOCAL_S3_BUCKET") or os.environ.get("TEST_S3_BUCKET") or "test-bucket"

    missing = [
        name
        for name, val in {
            "endpoint": endpoint,
            "region": region,
            "access-key": access_key,
            "secret-key": secret_key,
            "bucket": bucket,
        }.items()
        if not val
    ]
    if missing:
        raise SystemExit(f"Missing required args: {', '.join(missing)}")

    if args.delete_test_object:
        delete_test_object(
            endpoint=endpoint,
            bucket=bucket,
            region=region,
            access=access_key,
            secret=secret_key,
            key=args.delete_test_object,
        )
        result = {"deleted_key": args.delete_test_object, "status": "ok"}
        print(json.dumps(result, indent=2))
        return

    client = SigV4S3Client(endpoint=endpoint, region=region, access_key=access_key, secret_key=secret_key)
    results = run_probe(client, bucket)

    if args.output_json:
        Path(args.output_json).write_text(json.dumps(results, indent=2))
    print(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
