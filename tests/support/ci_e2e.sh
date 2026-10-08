#!/usr/bin/env bash
set -euo pipefail
umask 077

[[ "${GITHUB_ACTIONS:-}" == true && "${RUNNER_ENVIRONMENT:-}" == github-hosted &&
   "${RUNNER_OS:-}" == Linux ]] || {
    echo "refusing pool provisioning outside a GitHub-hosted Linux job" >&2
    exit 1
}
[[ "${GITHUB_RUN_ID:-}" =~ ^[1-9][0-9]*$ &&
   "${GITHUB_RUN_ATTEMPT:-}" =~ ^[1-9][0-9]*$ ]] || {
    echo "invalid GitHub run identity" >&2
    exit 1
}
[[ "$(uname -m)" == x86_64 && "${RUNNER_TEMP:-}" == /* && "$RUNNER_TEMP" != / ]] || {
    echo "requires an x86-64 hosted VM and a dedicated runner temporary directory" >&2
    exit 1
}

cd "$(dirname "$0")/../.."
root="$(realpath "$RUNNER_TEMP")/sts-${GITHUB_RUN_ID}-${GITHUB_RUN_ATTEMPT}"
mkdir -m 700 -- "$root"
mkdir "$root/logs" "$root/download" "$root/bin" "$root/scratch"
export TMPDIR="$root/scratch"
export PIP_CACHE_DIR="$root/scratch/pip-cache"
export AWS_EC2_METADATA_DISABLED=true
export E2E_RUNTIME_DIR="$root/zfs"
export E2E_GPG_DIR="$root/g"
export E2E_MIN_FREE_BYTES=4294967296
export LOCAL_S3_MIN_FREE_BYTES="$E2E_MIN_FREE_BYTES"
export LOCAL_S3_MAX_INCREMENT_BYTES=4294967296
unset AWS_SESSION_TOKEN AWS_PROFILE AWS_DEFAULT_PROFILE
pool="sts_ci_${GITHUB_RUN_ID}_${GITHUB_RUN_ATTEMPT}"
backing="$root/pool.vdev"
backing_identity=""
pool_guid=""
pool_created=false
service_pid=""
weed_pid=""
service_stopped=true

cleanup() {
    local status=$?
    trap - EXIT INT TERM
    if [[ -n "$service_pid" ]]; then
        if kill -0 "$service_pid" 2>/dev/null; then
            kill -TERM "$service_pid" || status=1
            wait "$service_pid" || true
        else
            echo "local S3 wrapper exited unexpectedly" >&2
            wait "$service_pid" || true
            status=1
        fi
        if [[ -f "$root/s3/weed.pid" ]] ||
           { [[ -n "$weed_pid" ]] && kill -0 "$weed_pid" 2>/dev/null; }; then
            echo "local S3 cleanup did not stop its owned service; retaining runtime" >&2
            status=1
        else
            service_stopped=true
        fi
    fi
    if [[ -f "$root/s3/disk-budget-failure.txt" ]]; then
        cat "$root/s3/disk-budget-failure.txt" >&2
        status=1
    fi
    if "$pool_created"; then
        if [[ -z "$pool_guid" ]] ||
           ! zpool get -j -p user:isdev "$pool" > "$root/logs/pool-cleanup.json" ||
           ! python3 - "$root/logs/pool-cleanup.json" "$pool" "$pool_guid" <<'PY'
import json
import sys

with open(sys.argv[1]) as source:
    pool = json.load(source)["pools"][sys.argv[2]]
if (pool["name"] != sys.argv[2] or pool["type"] != "POOL"
        or pool["pool_guid"] != sys.argv[3]
        or pool["properties"]["user:isdev"]["value"] != "yes"):
    sys.exit("refusing cleanup of a pool with different ownership")
PY
        then
            echo "pool ownership verification failed; retaining $pool and $backing" >&2
            status=1
        elif sudo -n zpool destroy "$pool"; then
            pool_created=false
            if [[ "$(stat -c '%d:%i' "$backing")" == "$backing_identity" ]]; then
                rm -- "$backing"
                printf 'destroyed_owned_pool=%s\n' "$pool" >> "$root/logs/cleanup.log"
            else
                echo "backing file identity changed; refusing to remove it" >&2
                status=1
            fi
        else
            echo "owned pool cleanup failed; retaining $pool and $backing" >&2
            status=1
        fi
    fi
    if ! "$pool_created" && "$service_stopped"; then
        local mounts
        if ! mounts="$(findmnt -rn -o TARGET)"; then
            echo "cannot verify remaining mounts; retaining runtime" >&2
            status=1
        elif grep -F -- "$root/" <<<"$mounts" >/dev/null; then
            echo "mounts remain in test runtime; refusing directory cleanup" >&2
            status=1
        else
            for path in "$root/s3/data" "$root/zfs" "$root/zfs-build" "$root/g" "$root/download" \
                        "$root/bin" "$root/venv" "$root/scratch"; do
                if [[ -d "$path" ]]; then
                    rm -r -- "$path"
                fi
            done
            rm -f -- "$root/s3/local_s3.env" "$root/s3/creds.json" "$root/s3/s3_config.json"
        fi
    fi
    printf 'exit_status=%s\npool_remaining=%s\nservice_stopped=%s\n' \
        "$status" "$pool_created" "$service_stopped" > "$root/logs/result.txt"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

sudo -n true
uname -a | tee "$root/logs/kernel.txt"
zfs version | tee "$root/logs/zfs-version.txt"
python3 - "$root/logs/zfs-version.txt" <<'PY'
import re
import sys

with open(sys.argv[1]) as source:
    text = source.read()
for component in ("zfs", "zfs-kmod"):
    match = re.search(rf"^{component}-(\d+)\.(\d+)\.(\d+)", text, re.MULTILINE)
    if not match or tuple(map(int, match.groups())) < (2, 3, 0):
        sys.exit(f"{component} must support OpenZFS JSON (>= 2.3): {text}")
if sys.version_info < (3, 11):
    sys.exit("Python >= 3.11 is required")
PY
zpool list -j -p > "$root/logs/pools-before.json"
python3 - "$root/logs/pools-before.json" <<'PY'
import json
import sys

with open(sys.argv[1]) as source:
    raw = source.read()
if raw == "":
    print("successful zpool list returned no imported pool records")
    pools = {}
else:
    pools = json.loads(raw)["pools"]
if pools != {}:
    sys.exit("refusing CI pool provisioning on a VM with existing pools")
PY
free="$(df -B1 --output=avail "$root" | tail -n 1 | tr -d ' ')"
[[ "$free" -ge "$E2E_MIN_FREE_BYTES" ]] || {
    echo "hosted job needs at least 4 GiB free for its bounded test fixture" >&2
    exit 1
}
curl --fail --location --retry 3 \
    https://github.com/openzfs/zfs/releases/download/zfs-2.4.4/zfs-2.4.4.tar.gz \
    --output "$root/download/zfs-2.4.4.tar.gz"
printf '%s  %s\n' \
    2a3c70d55a37cc71618a95a60e81ad66530201eb118d37741dc92efcf848c8b1 \
    "$root/download/zfs-2.4.4.tar.gz" | sha256sum --check
mkdir "$root/zfs-build"
tar -xzf "$root/download/zfs-2.4.4.tar.gz" --strip-components=1 -C "$root/zfs-build"
(
    cd "$root/zfs-build"
    ./configure --prefix=/usr/local --disable-pyzfs --with-linux="/lib/modules/$(uname -r)/build"
    make -j2
    sudo -n make install
) 2>&1 | tee "$root/logs/zfs-build.log"
sudo -n ldconfig
sudo -n modprobe -r zfs spl
sudo -n insmod "$root/zfs-build/module/spl.ko"
sudo -n insmod "$root/zfs-build/module/zfs.ko"
export PATH="/usr/local/sbin:/usr/local/bin:$PATH"
hash -r
zfs version | tee "$root/logs/zfs-version.txt"
sudo -n zfs version | tee "$root/logs/zfs-root-version.txt"
python3 - "$root/logs/zfs-version.txt" "$root/logs/zfs-root-version.txt" <<'PY'
import re
import sys

for path in sys.argv[1:]:
    with open(path) as source:
        text = source.read()
    for component in ("zfs", "zfs-kmod"):
        if not re.search(rf"^{component}-2\.4\.4(?:\s|$|-)", text, re.MULTILINE):
            sys.exit(f"expected matching OpenZFS 2.4.4 tools/module: {text}")
PY
truncate -s 2G "$backing"
backing_identity="$(stat -c '%d:%i' "$backing")"
sudo -n zpool create -o cachefile=none -o user:isdev=yes \
    -O mountpoint=none -O canmount=off -O atime=off "$pool" "$backing"
pool_created=true
zpool get -j -p user:isdev "$pool" > "$root/logs/pool-created.json"
pool_guid="$(python3 - "$root/logs/pool-created.json" "$pool" <<'PY'
import json
import sys

with open(sys.argv[1]) as source:
    pool = json.load(source)["pools"][sys.argv[2]]
guid = pool["pool_guid"]
if (pool["name"] != sys.argv[2] or pool["type"] != "POOL"
        or pool["state"] != "ONLINE" or not isinstance(guid, str)
        or not guid.isdecimal() or int(guid) == 0
        or pool["properties"]["user:isdev"]["value"] != "yes"):
    sys.exit("invalid newly created CI pool")
print(guid)
PY
)"
zpool status -P "$pool" | tee "$root/logs/pool-status.txt"

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
cargo test --locked --test http_store healthy_get_exceeds_former_120_second_transfer_cap -- --ignored \
    2>&1 | tee "$root/logs/http-long-get.log"
cargo build --locked 2>&1 | tee "$root/logs/build.log"

export DOWNLOAD_DIR="$root/download"
export BUILD_ARTIFACT_DIR="$PWD/target"
bash tests/support/local_s3.sh "$root/s3" > "$root/logs/local-s3.log" 2>&1 &
service_pid=$!
service_stopped=false
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
curl --silent --show-error --max-time 10 "$TEST_S3_ENDPOINT/" --output /dev/null
python3 tests/support/s3_probe.py --region us-east-1 \
    2>&1 | tee "$root/logs/s3-probe.log"
cargo test --locked --test live_http -- --ignored --nocapture \
    2>&1 | tee "$root/logs/live-http.log"
bash tests/support/zfs_s3_e2e.sh 2>&1 | tee "$root/logs/zfs-s3-e2e.log"
