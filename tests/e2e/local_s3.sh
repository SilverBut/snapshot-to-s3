#!/usr/bin/env bash
# Run a single-node SeaweedFS S3 endpoint on loopback in the foreground.
set -euo pipefail

EXPECTED_WEED_VERSION="${EXPECTED_WEED_VERSION:-4.48}"
EXPECTED_WEED_COMMIT="${EXPECTED_WEED_COMMIT:-}"
export LOCAL_S3_MAX_INCREMENT_GIB="${LOCAL_S3_MAX_INCREMENT_GIB:-40}"
export LOCAL_S3_MIN_FREE_GIB="${LOCAL_S3_MIN_FREE_GIB:-20}"
export LOCAL_S3_MONITOR_INTERVAL_SEC="${LOCAL_S3_MONITOR_INTERVAL_SEC:-10}"
export LOCAL_S3_MAX_INCREMENT_BYTES="${LOCAL_S3_MAX_INCREMENT_BYTES:-}"
export LOCAL_S3_MIN_FREE_BYTES="${LOCAL_S3_MIN_FREE_BYTES:-}"
READY_TIMEOUT_SEC=90

usage() {
  cat <<'USAGE'
Usage: tests/e2e/local_s3.sh RUNTIME_DIR

Starts a single-node SeaweedFS S3 endpoint on loopback in attached foreground mode.
Prints "ready endpoint=..." once S3 answers; source RUNTIME_DIR/local_s3.env to use it.

Required:
  RUNTIME_DIR              Directory for runtime state/config/logs.

Optional env:
  WEED_BIN                 Absolute path to weed binary. If unset, resolves from PATH.
  S3_REGION                Region for SigV4 (default: us-east-1)
  S3_BUCKET                Default test bucket name (default: test-bucket)
  S3_ACCESS_KEY            Fixed access key (default: random test key)
  S3_SECRET_KEY            Fixed secret key (default: random test secret)
  EXPECTED_WEED_VERSION    Required weed version (default: 4.48)
  EXPECTED_WEED_COMMIT     Optional exact commit hash to require
  DOWNLOAD_DIR             Optional dir to include in disk budget tracking.
  BUILD_ARTIFACT_DIR       Optional dir to include in disk budget tracking.
  LOCAL_S3_MAX_INCREMENT_GIB
                           Max incremental bytes across tracked dirs (default: 40 GiB)
  LOCAL_S3_MIN_FREE_GIB    Min free bytes required on each tracked filesystem (default: 20 GiB)
  LOCAL_S3_MAX_INCREMENT_BYTES
                           Overrides GiB limit with explicit bytes.
  LOCAL_S3_MIN_FREE_BYTES  Overrides GiB minimum free space with explicit bytes.
  LOCAL_S3_MONITOR_INTERVAL_SEC
                           Budget monitor interval in seconds (default: 10)
  KEEP_RUNTIME_FILES       If set to 1, keep generated runtime files on exit (default: 1)

Outputs:
  Writes generated config into RUNTIME_DIR, including:
    local_s3.env, ports.json, creds.json, s3_config.json, weed.pid

Disk budget:
  Growth of RUNTIME_DIR, DOWNLOAD_DIR and BUILD_ARTIFACT_DIR and free space on their
  filesystems are checked before start and every interval; a violation stops weed,
  writes disk-budget-failure.txt and exits 2.

Cleanup behavior:
  Kills only the weed child PID started by this script and removes weed.pid.
USAGE
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" || "$#" -ne 1 ]]; then
  usage
  [[ "$#" -eq 1 ]] && exit 0
  exit 2
fi

RUNTIME_DIR="$1"
mkdir -p "$RUNTIME_DIR" "$RUNTIME_DIR/data" "$RUNTIME_DIR/logs"
BUDGET_REPORT_FILE="$RUNTIME_DIR/disk-budget-report.txt"
BUDGET_BASELINE_FILE="$RUNTIME_DIR/disk-budget-baseline.tsv"
BUDGET_FAILURE_FILE="$RUNTIME_DIR/disk-budget-failure.txt"
WEED_LOG="$RUNTIME_DIR/logs/weed.log"

# --- weed binary -------------------------------------------------------------

resolve_weed() {
  if [[ -n "${WEED_BIN:-}" ]]; then
    [[ -x "$WEED_BIN" ]] || { echo "ERROR: WEED_BIN is not executable: $WEED_BIN" >&2; exit 1; }
  else
    WEED_BIN="$(command -v weed || true)"
    [[ -n "$WEED_BIN" ]] || {
      echo "ERROR: weed not found in PATH. Set WEED_BIN=/path/to/weed" >&2
      exit 1
    }
  fi
  local output
  output="$("$WEED_BIN" version)"
  weed_version="$(awk '{print $3}' <<<"$output")"
  weed_commit="$(awk '{print $4}' <<<"$output")"
  if [[ "$weed_version" != "$EXPECTED_WEED_VERSION" ]]; then
    echo "ERROR: weed version $weed_version != required $EXPECTED_WEED_VERSION" >&2
    echo "weed version output: $output" >&2
    exit 1
  fi
  if [[ -n "$EXPECTED_WEED_COMMIT" && "$weed_commit" != "$EXPECTED_WEED_COMMIT" ]]; then
    echo "ERROR: weed commit $weed_commit != required $EXPECTED_WEED_COMMIT" >&2
    exit 1
  fi
}

# --- disk budget ---------------------------------------------------------------

MAX_INCREMENT_BYTES="${LOCAL_S3_MAX_INCREMENT_BYTES:-$((LOCAL_S3_MAX_INCREMENT_GIB * 1024 ** 3))}"
MIN_FREE_BYTES="${LOCAL_S3_MIN_FREE_BYTES:-$((LOCAL_S3_MIN_FREE_GIB * 1024 ** 3))}"

dir_size_bytes() {
  if [[ -e "$1" ]]; then
    du -sb "$1" 2>/dev/null | awk '{print $1}'
  else
    echo 0
  fi
}

# Nearest existing ancestor, so df works before a tracked directory exists.
existing_probe_path() {
  local path="$1"
  while [[ ! -e "$path" && "$path" != "/" ]]; do
    path="$(dirname "$path")"
  done
  echo "$path"
}

record_tracked_dir() {
  local label="$1" path="$2"
  printf '%s\t%s\t%s\t%s\n' "$label" "$path" "$(dir_size_bytes "$path")" \
    "$(existing_probe_path "$path")" >> "$BUDGET_BASELINE_FILE"
}

budget_failure() {
  printf 'ERROR: %s\n' "$1" | tee "$BUDGET_FAILURE_FILE" >&2
  return 1
}

check_disk_budget() {
  local total=0 label path baseline probe current increment avail seen="|"
  : > "$BUDGET_REPORT_FILE"
  while IFS=$'\t' read -r label path baseline probe; do
    [[ -n "$label" ]] || continue
    current="$(dir_size_bytes "$path")"
    increment=$((current > baseline ? current - baseline : 0))
    total=$((total + increment))
    printf 'dir=%s path=%s baseline_bytes=%s current_bytes=%s increment_bytes=%s\n' \
      "$label" "$path" "$baseline" "$current" "$increment" >> "$BUDGET_REPORT_FILE"
  done < "$BUDGET_BASELINE_FILE"
  printf 'total_increment_bytes=%s max_increment_bytes=%s\n' \
    "$total" "$MAX_INCREMENT_BYTES" >> "$BUDGET_REPORT_FILE"
  if (( total > MAX_INCREMENT_BYTES )); then
    budget_failure "disk budget exceeded ($total > $MAX_INCREMENT_BYTES bytes)"
    return 1
  fi
  while IFS=$'\t' read -r _ _ _ probe; do
    [[ -n "$probe" && "$seen" != *"|$probe|"* ]] || continue
    seen+="$probe|"
    avail="$(df -B1 --output=avail "$probe" | tail -n1 | tr -d '[:space:]')"
    printf 'filesystem_probe=%s avail_bytes=%s min_free_bytes=%s\n' \
      "$probe" "$avail" "$MIN_FREE_BYTES" >> "$BUDGET_REPORT_FILE"
    if (( avail < MIN_FREE_BYTES )); then
      budget_failure "free space below minimum on $probe ($avail < $MIN_FREE_BYTES bytes)"
      return 1
    fi
  done < "$BUDGET_BASELINE_FILE"
  rm -f "$BUDGET_FAILURE_FILE"
}

start_budget() {
  : > "$BUDGET_BASELINE_FILE"
  record_tracked_dir runtime "$RUNTIME_DIR"
  [[ -z "${DOWNLOAD_DIR:-}" ]] || record_tracked_dir download "$DOWNLOAD_DIR"
  [[ -z "${BUILD_ARTIFACT_DIR:-}" ]] || record_tracked_dir build_artifact "$BUILD_ARTIFACT_DIR"
  check_disk_budget
}

# --- service -------------------------------------------------------------------

refuse_running_instance() {
  local stale_pid
  [[ -f "$RUNTIME_DIR/weed.pid" ]] || return 0
  stale_pid="$(cat "$RUNTIME_DIR/weed.pid" || true)"
  if [[ -n "$stale_pid" ]] && kill -0 "$stale_pid" 2>/dev/null; then
    echo "ERROR: runtime already has running weed pid $stale_pid" >&2
    exit 1
  fi
  rm -f "$RUNTIME_DIR/weed.pid"
}

weed_pid=""
monitor_pid=""
stop_child() {
  if [[ -n "$1" ]] && kill -0 "$1" 2>/dev/null; then
    kill "$1" 2>/dev/null || true
    wait "$1" 2>/dev/null || true
  fi
}

# shellcheck disable=SC2317,SC2329 # Invoked by the EXIT, INT and TERM traps.
cleanup() {
  local rc=$?
  stop_child "$monitor_pid"
  stop_child "$weed_pid"
  rm -f "$RUNTIME_DIR/weed.pid"
  if [[ "${KEEP_RUNTIME_FILES:-1}" != "1" ]]; then
    rm -f "$RUNTIME_DIR"/{local_s3.env,s3_config.json,ports.json,creds.json}
  fi
  exit "$rc"
}

start_weed() {
  "$WEED_BIN" server \
    -ip=127.0.0.1 \
    -ip.bind=127.0.0.1 \
    -dir="$RUNTIME_DIR/data" \
    -master.port="$LOCAL_S3_MASTER_PORT" \
    -master.port.grpc="$LOCAL_S3_MASTER_GRPC_PORT" \
    -master.peers="127.0.0.1:$LOCAL_S3_MASTER_PORT" \
    -master.raftBootstrap \
    -volume.port="$LOCAL_S3_VOLUME_PORT" \
    -volume.port.grpc="$LOCAL_S3_VOLUME_GRPC_PORT" \
    -filer \
    -filer.port="$LOCAL_S3_FILER_PORT" \
    -filer.port.grpc="$LOCAL_S3_FILER_GRPC_PORT" \
    -s3 \
    -s3.port="$LOCAL_S3_S3_PORT" \
    -s3.port.grpc="$LOCAL_S3_S3_GRPC_PORT" \
    -s3.port.iceberg="$LOCAL_S3_S3_ICEBERG_PORT" \
    -s3.port.lance="$LOCAL_S3_S3_LANCE_PORT" \
    -s3.ip.bind=127.0.0.1 \
    -s3.config="$RUNTIME_DIR/s3_config.json" \
    > "$WEED_LOG" 2>&1 &
  weed_pid="$!"
  echo "$weed_pid" > "$RUNTIME_DIR/weed.pid"
  (
    while kill -0 "$weed_pid" 2>/dev/null; do
      if ! check_disk_budget; then
        echo "disk budget monitor stopping weed pid=$weed_pid" >&2
        kill "$weed_pid" 2>/dev/null || true
        exit 1
      fi
      sleep "$LOCAL_S3_MONITOR_INTERVAL_SEC"
    done
  ) &
  monitor_pid="$!"
}

wait_until_ready() {
  local code
  for _ in $(seq 1 "$READY_TIMEOUT_SEC"); do
    if ! kill -0 "$weed_pid" 2>/dev/null; then
      echo "ERROR: weed exited before readiness. last logs:" >&2
      tail -n 120 "$WEED_LOG" >&2 || true
      exit 1
    fi
    code="$(curl -s -o /dev/null -w '%{http_code}' "$LOCAL_S3_ENDPOINT/" || true)"
    [[ "$code" == 200 || "$code" == 403 ]] && return 0
    sleep 1
  done
  echo "ERROR: S3 endpoint not ready on $LOCAL_S3_ENDPOINT" >&2
  tail -n 120 "$WEED_LOG" >&2 || true
  exit 1
}

resolve_weed
start_budget
refuse_running_instance
python3 "$(dirname "$0")/lib/seaweedfs_config.py" "$RUNTIME_DIR" \
  --region "${S3_REGION:-us-east-1}" --bucket "${S3_BUCKET:-test-bucket}" \
  --weed-bin "$WEED_BIN" --weed-version "$weed_version" --weed-commit "$weed_commit" \
  --budget-report "$BUDGET_REPORT_FILE"
# shellcheck source=/dev/null # Generated above.
source "$RUNTIME_DIR/local_s3.env"
trap cleanup EXIT INT TERM

echo "[$(date -Is)] starting SeaweedFS with weed=$WEED_BIN"
echo "runtime=$RUNTIME_DIR"
echo "endpoint=$LOCAL_S3_ENDPOINT region=$AWS_REGION bucket=$LOCAL_S3_BUCKET"
echo "disk-budget max_increment_bytes=$MAX_INCREMENT_BYTES min_free_bytes=$MIN_FREE_BYTES" \
  "interval_sec=$LOCAL_S3_MONITOR_INTERVAL_SEC"
start_weed
wait_until_ready
echo "ready endpoint=$LOCAL_S3_ENDPOINT"
echo "env file: $RUNTIME_DIR/local_s3.env"
echo "logs: $WEED_LOG"
echo "PID: $weed_pid"
tail -n 20 "$WEED_LOG" || true

set +e
wait "$weed_pid"
weed_rc=$?
set -e
stop_child "$monitor_pid"
if [[ -f "$BUDGET_FAILURE_FILE" ]]; then
  echo "ERROR: disk budget violation. See $BUDGET_FAILURE_FILE and $BUDGET_REPORT_FILE" >&2
  exit 2
fi
exit "$weed_rc"
