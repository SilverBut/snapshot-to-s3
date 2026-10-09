#!/usr/bin/env python3
"""Write the configuration for one loopback SeaweedFS S3 instance.

Allocates free loopback ports and test credentials, then writes ports.json,
creds.json, s3_config.json and the sourceable local_s3.env into RUNTIME_DIR.
Explicit S3_ACCESS_KEY/S3_SECRET_KEY replace the random credentials.
"""

import argparse
import json
import os
import secrets
import shlex
import socket
from pathlib import Path

PORTS = (
    "master",
    "master_grpc",
    "volume",
    "volume_grpc",
    "filer",
    "filer_grpc",
    "s3",
    "s3_grpc",
    "s3_iceberg",
    "s3_lance",
)
ACTIONS = ["Admin", "Read", "Write", "List", "Tagging"]
# Passed through from local_s3.sh so the env file records the effective limits.
BUDGET_VARIABLES = (
    "LOCAL_S3_MAX_INCREMENT_GIB",
    "LOCAL_S3_MIN_FREE_GIB",
    "LOCAL_S3_MAX_INCREMENT_BYTES",
    "LOCAL_S3_MIN_FREE_BYTES",
    "LOCAL_S3_MONITOR_INTERVAL_SEC",
)


def free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def write_json(path: Path, value: object) -> None:
    path.write_text(json.dumps(value, indent=2))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("runtime", type=Path)
    parser.add_argument("--region", required=True)
    parser.add_argument("--bucket", required=True)
    parser.add_argument("--weed-bin", required=True)
    parser.add_argument("--weed-version", required=True)
    parser.add_argument("--weed-commit", default="")
    parser.add_argument("--budget-report", required=True)
    args = parser.parse_args()

    ports = {name: free_port() for name in PORTS}
    access_key = os.environ.get("S3_ACCESS_KEY") or "AKIA" + secrets.token_hex(8).upper()
    secret_key = os.environ.get("S3_SECRET_KEY") or secrets.token_urlsafe(30)
    write_json(args.runtime / "ports.json", ports)
    write_json(
        args.runtime / "creds.json",
        {
            "access_key": access_key,
            "secret_key": secret_key,
            "region": args.region,
            "bucket": args.bucket,
        },
    )
    write_json(
        args.runtime / "s3_config.json",
        {
            "identities": [
                {
                    "name": "local-test",
                    "credentials": [{"accessKey": access_key, "secretKey": secret_key}],
                    "actions": ACTIONS,
                }
            ]
        },
    )
    exports = {
        "WEED_BIN": args.weed_bin,
        "WEED_VERSION": args.weed_version,
        "WEED_COMMIT": args.weed_commit,
        "LOCAL_S3_RUNTIME_DIR": str(args.runtime),
        "LOCAL_S3_ENDPOINT": f"http://127.0.0.1:{ports['s3']}",
        "AWS_REGION": args.region,
        "AWS_DEFAULT_REGION": args.region,
        "AWS_ACCESS_KEY_ID": access_key,
        "AWS_SECRET_ACCESS_KEY": secret_key,
        "LOCAL_S3_BUCKET": args.bucket,
        **{f"LOCAL_S3_{name.upper()}_PORT": str(port) for name, port in ports.items()},
        **{name: os.environ.get(name, "") for name in BUDGET_VARIABLES},
        "LOCAL_S3_DISK_BUDGET_REPORT": args.budget_report,
    }
    lines = ["# Source this file to use the running local S3 test service."]
    lines += [f"export {name}={shlex.quote(value)}" for name, value in exports.items()]
    (args.runtime / "local_s3.env").write_text("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
