#!/usr/bin/env bash
set -euo pipefail
umask 077

# shellcheck source=tests/support/hosted_environment.sh
source "$(dirname "$0")/hosted_environment.sh"
cd "$(dirname "$0")/../.."

if [[ "${1:-}" == --verify ]]; then
    [[ -f target/copilot-dev/env.sh ]] || { echo "Copilot setup is not ready" >&2; exit 1; }
    # shellcheck disable=SC1091
    source target/copilot-dev/env.sh
    [[ -f "$COPILOT_DEV_RUNTIME/ready" ]] || { echo "Copilot setup is incomplete" >&2; exit 1; }
    verify_pool_ownership "$COPILOT_DEV_RUNTIME" "$COPILOT_ZFS_POOL" "$COPILOT_ZFS_POOL_GUID"
    python3 - "$COPILOT_DEV_RUNTIME/logs/pool-ownership.json" "$COPILOT_ZFS_POOL" <<'PY'
import json
import sys

with open(sys.argv[1]) as source:
    pool = json.load(source)["pools"][sys.argv[2]]
if pool["state"] != "ONLINE":
    sys.exit("prepared development pool is not ONLINE")
PY
    verify="$(mktemp -d "$COPILOT_DEV_RUNTIME/verify.XXXXXXXX")"
    service_pid=""
    # shellcheck disable=SC2317,SC2329 # Invoked by verification's exit/signal traps.
    cleanup_verify() {
        local status=$?
        trap - EXIT INT TERM
        if [[ -n "$service_pid" ]]; then
            if kill -0 "$service_pid" 2>/dev/null; then
                kill -TERM "$service_pid" || status=1
                wait "$service_pid" || true
            else
                echo "verification service exited unexpectedly" >&2
                wait "$service_pid" || true
                status=1
            fi
        fi
        if [[ -f "$verify/s3/weed.pid" || -f "$verify/s3/disk-budget-failure.txt" ]]; then
            echo "verification service cleanup/budget failed; retaining diagnostics" >&2
            status=1
        fi
        if ! verify_pool_ownership "$COPILOT_DEV_RUNTIME" "$COPILOT_ZFS_POOL" "$COPILOT_ZFS_POOL_GUID"; then
            echo "development pool was not retained for the agent" >&2
            status=1
        fi
        printf 'verification_status=%s\nretained_pool=%s\n' \
            "$status" "$COPILOT_ZFS_POOL" > "$verify/result.txt"
        exit "$status"
    }
    trap cleanup_verify EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM
    "$TINK_PYTHON" tests/crypto_tink_interop.py
    TINK_PYTHON="$TINK_PYTHON" cargo test --locked --test crypto_stream official_tink_runtime_bidirectional -- --ignored
    cargo build --locked
    bash tests/support/local_s3.sh "$verify/s3" > "$verify/s3.log" 2>&1 &
    service_pid=$!
    ready=false
    for ((attempt = 0; attempt < 120; attempt++)); do
        if ! kill -0 "$service_pid" 2>/dev/null; then
            cat "$verify/s3.log" >&2
            echo "verification service exited before readiness" >&2
            exit 1
        fi
        if grep -q '^ready endpoint=' "$verify/s3.log"; then
            ready=true
            break
        fi
        sleep 1
    done
    "$ready" || { echo "verification service readiness timed out" >&2; exit 1; }
    # shellcheck disable=SC1091
    source "$verify/s3/local_s3.env"
    export TEST_S3_ENDPOINT="$LOCAL_S3_ENDPOINT" TEST_S3_BUCKET="$LOCAL_S3_BUCKET"
    export E2E_RUNTIME_DIR="$verify/zfs" E2E_GPG_DIR="$verify/g"
    python3 tests/support/s3_probe.py --region us-east-1
    cargo test --locked --test live_http -- --ignored
    bash tests/support/zfs_s3_e2e.sh 2>&1 | tee "$verify/zfs-s3-e2e.log"
    exit 0
fi

[[ "$#" == 0 ]] || { echo "usage: copilot_setup.sh [--verify]" >&2; exit 1; }
require_hosted_environment
state="$PWD/target/copilot-dev/env.sh"
mkdir -p "$(dirname "$state")"
rm -f -- "$state"
root="$(realpath "$RUNNER_TEMP")/sts-copilot-${GITHUB_RUN_ID}-${GITHUB_RUN_ATTEMPT}"
mkdir -m 700 -- "$root"
mkdir "$root/logs" "$root/download" "$root/bin"
pool="sts_dev_${GITHUB_RUN_ID}_${GITHUB_RUN_ATTEMPT}"
pool_created=false
pool_guid=""
backing=""
backing_identity=""
cleanup_failed_setup() {
    local status=$?
    trap - EXIT
    if [[ "$status" != 0 ]]; then
        rm -f -- "$state"
        if "$pool_created"; then
            if verify_pool_ownership "$root" "$pool" "$pool_guid" && sudo -n zpool destroy "$pool"; then
                if [[ "$(stat -c '%d:%i' "$backing")" == "$backing_identity" ]]; then
                    rm -- "$backing"
                fi
            else
                echo "failed setup pool cleanup is unresolved; retaining $pool and $root" >&2
            fi
        fi
        echo "Copilot development setup failed; no ready environment was published" >&2
    fi
    exit "$status"
}
trap cleanup_failed_setup EXIT
install_test_tools "$root"
"$root/venv/bin/python" tests/crypto_tink_interop.py
create_hosted_pool "$root" "$pool" 4G 4294967296
cargo test --locked --no-run 2>&1 | tee "$root/logs/test-compile.log"
{
    printf 'export COPILOT_DEV_RUNTIME=%q\n' "$root"
    printf 'export COPILOT_ZFS_POOL=%q\n' "$pool"
    printf 'export COPILOT_ZFS_POOL_GUID=%q\n' "$pool_guid"
    printf 'export WEED_BIN=%q\n' "$WEED_BIN"
    printf 'export TINK_PYTHON=%q\n' "$root/venv/bin/python"
    # shellcheck disable=SC2016 # Expand the agent's current PATH when it loads the handoff.
    printf 'export PATH=%q:"$PATH"\n' "$root/bin"
    printf 'export EXPECTED_WEED_VERSION=4.48\n'
    printf 'export E2E_MIN_FREE_BYTES=4294967296\n'
    printf 'export LOCAL_S3_MIN_FREE_BYTES=4294967296\n'
    printf 'export LOCAL_S3_MAX_INCREMENT_BYTES=4294967296\n'
} > "$state.tmp"
printf 'ready\n' > "$root/ready"
mv "$state.tmp" "$state"
if [[ -n "${GITHUB_PATH:-}" ]]; then
    printf '%s\n' "$root/bin" >> "$GITHUB_PATH"
fi
if [[ -n "${GITHUB_ENV:-}" ]]; then
    printf 'TINK_PYTHON=%s\nCOPILOT_DEV_RUNTIME=%s\n' "$root/venv/bin/python" "$root" >> "$GITHUB_ENV"
fi
echo "Copilot environment ready: $pool (GUID $pool_guid); source target/copilot-dev/env.sh"
echo "The pool is intentionally retained until the ephemeral agent VM is discarded."
