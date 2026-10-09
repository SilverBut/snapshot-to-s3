#!/usr/bin/env bash

require_hosted_environment() {
    [[ "${GITHUB_ACTIONS:-}" == true && "${RUNNER_ENVIRONMENT:-}" == github-hosted &&
       "${RUNNER_OS:-}" == Linux ]] || {
        echo "refusing pool provisioning outside a GitHub-hosted Linux job" >&2
        return 1
    }
    [[ "${GITHUB_RUN_ID:-}" =~ ^[1-9][0-9]*$ &&
       "${GITHUB_RUN_ATTEMPT:-}" =~ ^[1-9][0-9]*$ ]] || {
        echo "invalid GitHub run identity" >&2
        return 1
    }
    [[ "$(uname -m)" == x86_64 && "${RUNNER_TEMP:-}" == /* && "$RUNNER_TEMP" != / ]] || {
        echo "requires an x86-64 hosted VM and a dedicated runner temporary directory" >&2
        return 1
    }
}

# shellcheck disable=SC2034 # Lifecycle state is consumed by the caller's exit trap.
create_hosted_pool() {
    local root="$1" pool="$2" size="$3" min_free="$4" free
    require_hosted_environment
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
    sys.exit("refusing hosted pool provisioning on a VM with existing pools")
PY
    free="$(df -B1 --output=avail "$root" | tail -n 1 | tr -d ' ')"
    [[ "$free" -ge "$min_free" ]] || {
        echo "insufficient free space for the bounded hosted test fixture" >&2
        return 1
    }
    backing="$root/pool.vdev"
    truncate -s "$size" "$backing"
    backing_identity="$(stat -c '%d:%i' "$backing")"
    sudo -n zpool create -o cachefile=none \
        -O mountpoint=none -O canmount=off -O atime=off "$pool" "$backing"
    pool_created=true
    zpool get -j -p guid "$pool" > "$root/logs/pool-created.json"
    pool_guid="$(python3 - "$root/logs/pool-created.json" "$pool" <<'PY'
import json
import sys

with open(sys.argv[1]) as source:
    pool = json.load(source)["pools"][sys.argv[2]]
guid = pool["pool_guid"]
if (pool["name"] != sys.argv[2] or pool["type"] != "POOL"
        or pool["state"] != "ONLINE" or not isinstance(guid, str)
        or not guid.isdecimal() or int(guid) == 0):
    sys.exit("invalid newly created hosted pool")
print(guid)
PY
)"
    sudo -n zpool set user:isdev=yes "$pool"
    verify_pool_ownership "$root" "$pool" "$pool_guid"
    zpool status -P "$pool" | tee "$root/logs/pool-status.txt"
}

verify_pool_ownership() {
    local root="$1" pool="$2" guid="$3"
    zpool get -j -p user:isdev "$pool" > "$root/logs/pool-ownership.json"
    python3 - "$root/logs/pool-ownership.json" "$pool" "$guid" <<'PY'
import json
import sys

with open(sys.argv[1]) as source:
    pool = json.load(source)["pools"][sys.argv[2]]
if (pool["name"] != sys.argv[2] or pool["type"] != "POOL"
        or pool["pool_guid"] != sys.argv[3]
        or pool["properties"]["user:isdev"]["value"] != "yes"):
    sys.exit("refusing a pool with different ownership, label or health")
PY
}

install_test_tools() {
    local root="$1"
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
}
