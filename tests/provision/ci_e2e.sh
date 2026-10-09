#!/usr/bin/env bash
set -euo pipefail
umask 077

# shellcheck source=tests/provision/lib/hosted.sh
source "$(dirname "$0")/lib/hosted.sh"
require_hosted_environment

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
           ! verify_pool_ownership "$root" "$pool" "$pool_guid"
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
            for path in "$root/s3/data" "$root/zfs" "$root/g" "$root/download" \
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

create_hosted_pool "$root" "$pool" 2G "$E2E_MIN_FREE_BYTES"
install_test_tools "$root"
"$root/venv/bin/python" tests/tooling/tink_interop.py \
    2>&1 | tee "$root/logs/tink-interop.log"
TINK_PYTHON="$root/venv/bin/python" \
    cargo test --locked --test crypto_stream official_tink_runtime_bidirectional -- --ignored \
    2>&1 | tee "$root/logs/tink-bidirectional.log"
cargo test --locked --test http_store healthy_get_exceeds_former_120_second_transfer_cap -- --ignored \
    2>&1 | tee "$root/logs/http-long-get.log"
cargo build --locked 2>&1 | tee "$root/logs/build.log"

export DOWNLOAD_DIR="$root/download"
export BUILD_ARTIFACT_DIR="$PWD/target"
bash tests/e2e/local_s3.sh "$root/s3" > "$root/logs/local-s3.log" 2>&1 &
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
python3 tests/tooling/s3_probe.py --region us-east-1 \
    2>&1 | tee "$root/logs/s3-probe.log"
cargo test --locked --test live_http -- --ignored --nocapture \
    2>&1 | tee "$root/logs/live-http.log"
bash tests/e2e/run.sh 2>&1 | tee "$root/logs/zfs-s3-e2e.log"
