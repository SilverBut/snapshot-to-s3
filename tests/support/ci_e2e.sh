#!/usr/bin/env bash
# Requires Linux x86-64, OpenZFS >= 2.3 (userspace and loaded module), an
# existing ONLINE user:isdev=yes pool, passwordless sudo, Python >= 3.11 with
# venv, Rust, curl, sha256sum, tar, findmnt and GnuPG. Provision these outside
# CI; never create, import, export, destroy or relabel a pool here.
# Use a short runner work root: GnuPG socket paths must fit below 108 bytes.
set -euo pipefail
umask 077

cd "$(dirname "$0")/../.."
root="$PWD/.ci-e2e"
[[ ! -e "$root" ]] || { echo "refusing to reuse CI runtime: $root" >&2; exit 1; }
mkdir -p "$root/logs" "$root/download" "$root/bin" "$root/scratch"
export TMPDIR="$root/scratch"
export PIP_CACHE_DIR="$root/scratch/pip-cache"
export AWS_EC2_METADATA_DISABLED=true
export E2E_GPG_DIR="${E2E_GPG_DIR:-$PWD/.gpg-ci}"
unset AWS_SESSION_TOKEN AWS_PROFILE AWS_DEFAULT_PROFILE
service_pid=""
weed_pid=""

cleanup() {
    local status=$?
    trap - EXIT INT TERM
    if [[ -n "$service_pid" ]]; then
        if kill -0 "$service_pid" 2>/dev/null; then
            # The local_s3 wrapper owns and terminates only its weed/monitor PIDs.
            kill -TERM "$service_pid" || status=1
            wait "$service_pid" || true
        else
            # An unexpected service exit must fail even after successful tests.
            wait "$service_pid" || true
            status=1
        fi
    fi
    if [[ -f "$root/s3/weed.pid" ]]; then
        echo "ERROR: local S3 cleanup left a PID file; retaining runtime" >&2
        status=1
    fi
    if [[ -n "$weed_pid" ]] && kill -0 "$weed_pid" 2>/dev/null; then
        echo "ERROR: weed PID $weed_pid survived wrapper cleanup" >&2
        kill -TERM "$weed_pid" || true
        status=1
    fi
    if [[ -f "$root/s3/disk-budget-failure.txt" ]]; then
        cat "$root/s3/disk-budget-failure.txt" >&2
        status=1
    fi
    if [[ -n "$service_pid" && ! -f "$root/s3/weed.pid" ]] &&
       { [[ -z "$weed_pid" ]] || ! kill -0 "$weed_pid" 2>/dev/null; }; then
        # Only SeaweedFS owns this data directory; ZFS mounts live separately
        # under target/test-artifacts. Do not collect throwaway credentials.
        rm -rf -- "$root/s3/data"
        rm -f -- "$root/s3/local_s3.env" "$root/s3/creds.json" "$root/s3/s3_config.json"
    fi
    rm -rf -- "${root:?}/download" "${root:?}/bin" "${root:?}/venv" "${root:?}/scratch"
    printf 'exit_status=%s\n' "$status" > "$root/logs/result.txt"
    # Keep failure/mount diagnostics for an administrator. Never recursively
    # remove a checkout: failed ZFS cleanup may have left a mounted dataset.
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

[[ "$(uname -s)" == Linux && "$(uname -m)" == x86_64 ]]
sudo -n true
zfs version | tee "$root/logs/zfs-version.txt"
python3 - "$root/logs/zfs-version.txt" <<'PY'
import re
import os
import sys
from pathlib import Path

text = Path(sys.argv[1]).read_text()
for component in ("zfs", "zfs-kmod"):
    match = re.search(rf"^{component}-(\d+)\.(\d+)\.(\d+)", text, re.MULTILINE)
    if not match or tuple(map(int, match.groups())) < (2, 3, 0):
        sys.exit(f"{component} must be OpenZFS >= 2.3: {text}")
if sys.version_info < (3, 11):
    sys.exit("Python >= 3.11 is required by the S3 probe")
socket = Path(os.environ["E2E_GPG_DIR"]).resolve() / "gpg-public/S.gpg-agent.browser"
if len(os.fsencode(socket)) >= 108:
    sys.exit("GnuPG socket path exceeds Linux's limit; provision a shorter runner "
             "work root or a unique short E2E_GPG_DIR: " + str(socket))
PY
# Fail before downloads if JSON support or an eligible pool is unavailable.
# The existing E2E harness independently discovers and re-verifies the pool
# immediately before any dataset mutations.
zpool list -j -p -o name,health > "$root/logs/zpool-list.json"
zpool get -j -p user:isdev > "$root/logs/zpool-labels.json"
zpool status -P > "$root/logs/zpool-status.txt"
python3 - "$root/logs/zpool-list.json" "$root/logs/zpool-labels.json" <<'PY'
import json
import sys

try:
    documents = []
    for path in sys.argv[1:]:
        with open(path) as source:
            pools = json.load(source)["pools"]
        if not isinstance(pools, dict):
            raise ValueError("expected a named pool map")
        for name, pool in pools.items():
            if pool.get("name") != name or pool.get("type") != "POOL":
                raise ValueError("pool identity/type mismatch")
        documents.append(pools)
    online, labels = documents
    eligible = [
        name for name, pool in online.items()
        if pool["state"] == "ONLINE"
        and pool["properties"]["health"]["value"] == "ONLINE"
        and name in labels
        and labels[name]["properties"]["user:isdev"]["value"] == "yes"
    ]
except (KeyError, TypeError, ValueError) as error:
    sys.exit(f"invalid OpenZFS JSON: {error}")
if not eligible:
    sys.exit("no ONLINE user:isdev=yes pool; provision a development pool outside CI")
print("eligible existing development pools: " + ", ".join(eligible))
PY

# SHA256 is the upstream release API digest for the exact 4.48 amd64 asset:
# https://api.github.com/repos/seaweedfs/seaweedfs/releases/tags/4.48
curl --fail --location --retry 3 \
    https://github.com/seaweedfs/seaweedfs/releases/download/4.48/linux_amd64.tar.gz \
    --output "$root/download/seaweedfs-4.48-linux-amd64.tar.gz"
printf '%s  %s\n' \
    4a7d108384d044d95212d1342cdda9533fa55842c1c9b41f606ca3c8a9561124 \
    "$root/download/seaweedfs-4.48-linux-amd64.tar.gz" | sha256sum --check
tar -xzf "$root/download/seaweedfs-4.48-linux-amd64.tar.gz" -C "$root/bin" weed
export WEED_BIN="$root/bin/weed"
export EXPECTED_WEED_VERSION=4.48
"$WEED_BIN" version | tee "$root/logs/seaweedfs-version.txt"

python3 -m venv "$root/venv"
"$root/venv/bin/python" -m pip install --disable-pip-version-check 'tink==1.16.1' \
    2>&1 | tee "$root/logs/tink-install.log"
"$root/venv/bin/python" tests/crypto_tink_interop.py \
    2>&1 | tee "$root/logs/tink-interop.log"
TINK_PYTHON="$root/venv/bin/python" \
    cargo test --locked --test crypto_stream official_tink_runtime_bidirectional -- --ignored \
    2>&1 | tee "$root/logs/tink-bidirectional.log"
cargo build --locked 2>&1 | tee "$root/logs/build.log"

export DOWNLOAD_DIR="$root/download"
export BUILD_ARTIFACT_DIR="$PWD/target"
bash tests/support/local_s3.sh "$root/s3" > "$root/logs/local-s3.log" 2>&1 &
service_pid=$!
ready=false
for ((attempt = 0; attempt < 120; attempt++)); do
    if ! kill -0 "$service_pid" 2>/dev/null; then
        cat "$root/logs/local-s3.log" >&2
        echo "local S3 wrapper exited before readiness" >&2
        exit 1
    fi
    if grep -q '^ready endpoint=' "$root/logs/local-s3.log"; then
        ready=true
        break
    fi
    sleep 1
done
"$ready" || { echo "local S3 readiness timed out" >&2; exit 1; }
weed_pid="$(cat "$root/s3/weed.pid")"
[[ "$weed_pid" =~ ^[1-9][0-9]*$ ]]
# shellcheck disable=SC1091
source "$root/s3/local_s3.env"
export TEST_S3_ENDPOINT="$LOCAL_S3_ENDPOINT"
export TEST_S3_BUCKET="$LOCAL_S3_BUCKET"
curl --silent --show-error --max-time 10 "$TEST_S3_ENDPOINT/" \
    --output /dev/null
python3 tests/support/s3_probe.py --region us-east-1 \
    2>&1 | tee "$root/logs/s3-probe.log"
cargo test --locked --test live_http -- --ignored --nocapture \
    2>&1 | tee "$root/logs/live-http.log"
bash tests/support/zfs_s3_e2e.sh 2>&1 | tee "$root/logs/zfs-s3-e2e.log"
