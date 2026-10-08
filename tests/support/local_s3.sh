#!/usr/bin/env bash
set -euo pipefail

EXPECTED_WEED_VERSION="${EXPECTED_WEED_VERSION:-4.48}"
EXPECTED_WEED_COMMIT="${EXPECTED_WEED_COMMIT:-}"
LOCAL_S3_MAX_INCREMENT_GIB="${LOCAL_S3_MAX_INCREMENT_GIB:-40}"
LOCAL_S3_MIN_FREE_GIB="${LOCAL_S3_MIN_FREE_GIB:-20}"
LOCAL_S3_MONITOR_INTERVAL_SEC="${LOCAL_S3_MONITOR_INTERVAL_SEC:-10}"
LOCAL_S3_MAX_INCREMENT_BYTES="${LOCAL_S3_MAX_INCREMENT_BYTES:-}"
LOCAL_S3_MIN_FREE_BYTES="${LOCAL_S3_MIN_FREE_BYTES:-}"

usage() {
  cat <<'EOF'
Usage: tests/support/local_s3.sh RUNTIME_DIR

Starts a single-node SeaweedFS S3 endpoint on loopback in attached foreground mode.

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

Cleanup behavior:
  Kills only the weed child PID started by this script and removes weed.pid.
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" || "$#" -ne 1 ]]; then
  usage
  if [[ "$#" -eq 1 ]]; then
    exit 0
  fi
  exit 2
fi

RUNTIME_DIR="$1"
mkdir -p "$RUNTIME_DIR" "$RUNTIME_DIR/data" "$RUNTIME_DIR/logs"
BUDGET_REPORT_FILE="$RUNTIME_DIR/disk-budget-report.txt"
BUDGET_BASELINE_FILE="$RUNTIME_DIR/disk-budget-baseline.tsv"
BUDGET_FAILURE_FILE="$RUNTIME_DIR/disk-budget-failure.txt"

if [[ -n "${WEED_BIN:-}" ]]; then
  if [[ ! -x "$WEED_BIN" ]]; then
    echo "ERROR: WEED_BIN is not executable: $WEED_BIN" >&2
    exit 1
  fi

  DOWNLOAD_DIR="${DOWNLOAD_DIR:-}"
  BUILD_ARTIFACT_DIR="${BUILD_ARTIFACT_DIR:-}"
  MAX_INCREMENT_BYTES=$((LOCAL_S3_MAX_INCREMENT_GIB * 1024 * 1024 * 1024))
  MIN_FREE_BYTES=$((LOCAL_S3_MIN_FREE_GIB * 1024 * 1024 * 1024))
  if [[ -n "$LOCAL_S3_MAX_INCREMENT_BYTES" ]]; then
    MAX_INCREMENT_BYTES="$LOCAL_S3_MAX_INCREMENT_BYTES"
  fi
  if [[ -n "$LOCAL_S3_MIN_FREE_BYTES" ]]; then
    MIN_FREE_BYTES="$LOCAL_S3_MIN_FREE_BYTES"
  fi

  dir_size_bytes() {
    local p="$1"
    if [[ -e "$p" ]]; then
      du -sb "$p" 2>/dev/null | awk '{print $1}'
    else
      echo 0
    fi
  }

  existing_probe_path() {
    local p="$1"
    while [[ ! -e "$p" && "$p" != "/" ]]; do
      p="$(dirname "$p")"
    done
    echo "$p"
  }

  record_tracked_dir() {
    local label="$1"
    local p="$2"
    local probe base
    probe="$(existing_probe_path "$p")"
    base="$(dir_size_bytes "$p")"
    printf '%s\t%s\t%s\t%s\n' "$label" "$p" "$base" "$probe" >> "$BUDGET_BASELINE_FILE"
  }

  check_disk_budget() {
    local total_increment=0
    : > "$BUDGET_REPORT_FILE"
    while IFS=$'\t' read -r label p baseline probe; do
      [[ -n "$label" ]] || continue
      local current increment
      current="$(dir_size_bytes "$p")"
      increment=$((current - baseline))
      if (( increment < 0 )); then
        increment=0
      fi
      total_increment=$((total_increment + increment))
      printf 'dir=%s path=%s baseline_bytes=%s current_bytes=%s increment_bytes=%s\n' \
        "$label" "$p" "$baseline" "$current" "$increment" >> "$BUDGET_REPORT_FILE"
    done < "$BUDGET_BASELINE_FILE"
    printf 'total_increment_bytes=%s max_increment_bytes=%s\n' "$total_increment" "$MAX_INCREMENT_BYTES" >> "$BUDGET_REPORT_FILE"

    if (( total_increment > MAX_INCREMENT_BYTES )); then
      printf 'ERROR: disk budget exceeded (%s > %s bytes)\n' "$total_increment" "$MAX_INCREMENT_BYTES" | tee "$BUDGET_FAILURE_FILE" >&2
      return 1
    fi

    local seen_probes="|"
    while IFS=$'\t' read -r _ _ _ probe; do
      [[ -n "$probe" ]] || continue
      if [[ "$seen_probes" == *"|$probe|"* ]]; then
        continue
      fi
      seen_probes="${seen_probes}${probe}|"
      local avail
      avail="$(df -B1 --output=avail "$probe" | tail -n1 | tr -d '[:space:]')"
      printf 'filesystem_probe=%s avail_bytes=%s min_free_bytes=%s\n' "$probe" "$avail" "$MIN_FREE_BYTES" >> "$BUDGET_REPORT_FILE"
      if (( avail < MIN_FREE_BYTES )); then
        printf 'ERROR: free space below minimum on %s (%s < %s bytes)\n' "$probe" "$avail" "$MIN_FREE_BYTES" | tee "$BUDGET_FAILURE_FILE" >&2
        return 1
      fi
    done < "$BUDGET_BASELINE_FILE"

    rm -f "$BUDGET_FAILURE_FILE"
  }

  : > "$BUDGET_BASELINE_FILE"
  record_tracked_dir "runtime" "$RUNTIME_DIR"
  if [[ -n "$DOWNLOAD_DIR" ]]; then
    record_tracked_dir "download" "$DOWNLOAD_DIR"
  fi
  if [[ -n "$BUILD_ARTIFACT_DIR" ]]; then
    record_tracked_dir "build_artifact" "$BUILD_ARTIFACT_DIR"
  fi
  check_disk_budget
else
  WEED_BIN="$(command -v weed || true)"
  if [[ -z "$WEED_BIN" ]]; then
    echo "ERROR: weed not found in PATH. Set WEED_BIN=/path/to/weed" >&2
    exit 1
  fi
fi

weed_version_out="$($WEED_BIN version)"
weed_version="$(awk '{print $3}' <<<"$weed_version_out")"
weed_commit="$(awk '{print $4}' <<<"$weed_version_out")"

if [[ "$weed_version" != "$EXPECTED_WEED_VERSION" ]]; then
  echo "ERROR: weed version $weed_version != required $EXPECTED_WEED_VERSION" >&2
  echo "weed version output: $weed_version_out" >&2
  exit 1
fi
if [[ -n "$EXPECTED_WEED_COMMIT" && "$weed_commit" != "$EXPECTED_WEED_COMMIT" ]]; then
  echo "ERROR: weed commit $weed_commit != required $EXPECTED_WEED_COMMIT" >&2
  exit 1
fi

if [[ -f "$RUNTIME_DIR/weed.pid" ]]; then
  stale_pid="$(cat "$RUNTIME_DIR/weed.pid" || true)"
  if [[ -n "$stale_pid" ]] && kill -0 "$stale_pid" 2>/dev/null; then
    echo "ERROR: runtime already has running weed pid $stale_pid" >&2
    exit 1
  fi
  rm -f "$RUNTIME_DIR/weed.pid"
fi

python3 - "$RUNTIME_DIR" <<'PY'
import json,secrets,socket,sys
from pathlib import Path
rt=Path(sys.argv[1])
ports={}
for name in ['master','master_grpc','volume','volume_grpc','filer','filer_grpc','s3']:
    s=socket.socket()
    s.bind(('127.0.0.1',0))
    ports[name]=s.getsockname()[1]
    s.close()
for name in ['s3_grpc','s3_iceberg','s3_lance']:
    s=socket.socket()
    s.bind(('127.0.0.1',0))
    ports[name]=s.getsockname()[1]
    s.close()
access_key=rt.joinpath('access_key.txt').read_text().strip() if rt.joinpath('access_key.txt').exists() else ('AKIA'+secrets.token_hex(8).upper()[:16])
secret_key=rt.joinpath('secret_key.txt').read_text().strip() if rt.joinpath('secret_key.txt').exists() else secrets.token_urlsafe(30)
creds={'access_key':access_key,'secret_key':secret_key,'region':'__REGION__','bucket':'__BUCKET__'}
(rt/'ports.json').write_text(json.dumps(ports,indent=2))
(rt/'creds.json').write_text(json.dumps(creds,indent=2))
s3cfg={'identities':[{'name':'local-test','credentials':[{'accessKey':access_key,'secretKey':secret_key}],'actions':['Admin','Read','Write','List','Tagging']}]} 
(rt/'s3_config.json').write_text(json.dumps(s3cfg,indent=2))
PY

S3_REGION="${S3_REGION:-us-east-1}"
S3_BUCKET="${S3_BUCKET:-test-bucket}"
S3_ACCESS_KEY="${S3_ACCESS_KEY:-}"
S3_SECRET_KEY="${S3_SECRET_KEY:-}"

# Override generated secrets if caller supplied explicit values.
python3 - "$RUNTIME_DIR" "$S3_REGION" "$S3_BUCKET" "$S3_ACCESS_KEY" "$S3_SECRET_KEY" <<'PY'
import json,sys
from pathlib import Path
rt=Path(sys.argv[1])
region,bucket,ak,sk=sys.argv[2],sys.argv[3],sys.argv[4],sys.argv[5]
creds=json.loads((rt/'creds.json').read_text())
if ak: creds['access_key']=ak
if sk: creds['secret_key']=sk
creds['region']=region
creds['bucket']=bucket
(rt/'creds.json').write_text(json.dumps(creds,indent=2))
cfg={'identities':[{'name':'local-test','credentials':[{'accessKey':creds['access_key'],'secretKey':creds['secret_key']}],'actions':['Admin','Read','Write','List','Tagging']}]} 
(rt/'s3_config.json').write_text(json.dumps(cfg,indent=2))
PY

MASTER_PORT="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["master"])' "$RUNTIME_DIR/ports.json")"
MASTER_GRPC_PORT="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["master_grpc"])' "$RUNTIME_DIR/ports.json")"
VOLUME_PORT="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["volume"])' "$RUNTIME_DIR/ports.json")"
VOLUME_GRPC_PORT="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["volume_grpc"])' "$RUNTIME_DIR/ports.json")"
FILER_PORT="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["filer"])' "$RUNTIME_DIR/ports.json")"
FILER_GRPC_PORT="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["filer_grpc"])' "$RUNTIME_DIR/ports.json")"
S3_PORT="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["s3"])' "$RUNTIME_DIR/ports.json")"
S3_GRPC_PORT="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["s3_grpc"])' "$RUNTIME_DIR/ports.json")"
S3_ICEBERG_PORT="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["s3_iceberg"])' "$RUNTIME_DIR/ports.json")"
S3_LANCE_PORT="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["s3_lance"])' "$RUNTIME_DIR/ports.json")"

S3_ACCESS_KEY="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["access_key"])' "$RUNTIME_DIR/creds.json")"
S3_SECRET_KEY="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["secret_key"])' "$RUNTIME_DIR/creds.json")"
S3_REGION="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["region"])' "$RUNTIME_DIR/creds.json")"
S3_BUCKET="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["bucket"])' "$RUNTIME_DIR/creds.json")"

cat > "$RUNTIME_DIR/local_s3.env" <<EOF
# Source this file to use the running local S3 test service.
export WEED_BIN='$WEED_BIN'
export WEED_VERSION='$weed_version'
export WEED_COMMIT='$weed_commit'
export LOCAL_S3_RUNTIME_DIR='$RUNTIME_DIR'
export LOCAL_S3_ENDPOINT='http://127.0.0.1:$S3_PORT'
export AWS_REGION='$S3_REGION'
export AWS_DEFAULT_REGION='$S3_REGION'
export AWS_ACCESS_KEY_ID='$S3_ACCESS_KEY'
export AWS_SECRET_ACCESS_KEY='$S3_SECRET_KEY'
export LOCAL_S3_BUCKET='$S3_BUCKET'
export LOCAL_S3_MASTER_PORT='$MASTER_PORT'
export LOCAL_S3_VOLUME_PORT='$VOLUME_PORT'
export LOCAL_S3_FILER_PORT='$FILER_PORT'
export LOCAL_S3_S3_PORT='$S3_PORT'
export LOCAL_S3_S3_GRPC_PORT='$S3_GRPC_PORT'
export LOCAL_S3_S3_ICEBERG_PORT='$S3_ICEBERG_PORT'
export LOCAL_S3_S3_LANCE_PORT='$S3_LANCE_PORT'
export LOCAL_S3_MAX_INCREMENT_GIB='$LOCAL_S3_MAX_INCREMENT_GIB'
export LOCAL_S3_MIN_FREE_GIB='$LOCAL_S3_MIN_FREE_GIB'
export LOCAL_S3_MAX_INCREMENT_BYTES='$LOCAL_S3_MAX_INCREMENT_BYTES'
export LOCAL_S3_MIN_FREE_BYTES='$LOCAL_S3_MIN_FREE_BYTES'
export LOCAL_S3_MONITOR_INTERVAL_SEC='$LOCAL_S3_MONITOR_INTERVAL_SEC'
export LOCAL_S3_DISK_BUDGET_REPORT='$BUDGET_REPORT_FILE'
EOF

weed_pid=""
monitor_pid=""
# shellcheck disable=SC2317,SC2329 # Invoked by the EXIT, INT and TERM traps.
cleanup() {
  local rc=$?
  if [[ -n "$monitor_pid" ]] && kill -0 "$monitor_pid" 2>/dev/null; then
    kill "$monitor_pid" 2>/dev/null || true
    wait "$monitor_pid" 2>/dev/null || true
  fi
  if [[ -n "$weed_pid" ]] && kill -0 "$weed_pid" 2>/dev/null; then
    kill "$weed_pid" 2>/dev/null || true
    wait "$weed_pid" 2>/dev/null || true
  fi
  rm -f "$RUNTIME_DIR/weed.pid"
  if [[ "${KEEP_RUNTIME_FILES:-1}" != "1" ]]; then
    rm -f "$RUNTIME_DIR"/local_s3.env "$RUNTIME_DIR"/s3_config.json "$RUNTIME_DIR"/ports.json "$RUNTIME_DIR"/creds.json
  fi
  exit "$rc"
}
trap cleanup EXIT INT TERM

echo "[$(date -Is)] starting SeaweedFS with weed=$WEED_BIN"
echo "runtime=$RUNTIME_DIR"
echo "endpoint=http://127.0.0.1:$S3_PORT region=$S3_REGION bucket=$S3_BUCKET"
echo "disk-budget max_increment_gib=$LOCAL_S3_MAX_INCREMENT_GIB min_free_gib=$LOCAL_S3_MIN_FREE_GIB interval_sec=$LOCAL_S3_MONITOR_INTERVAL_SEC"

"$WEED_BIN" server \
  -ip=127.0.0.1 \
  -ip.bind=127.0.0.1 \
  -dir="$RUNTIME_DIR/data" \
  -master.port="$MASTER_PORT" \
  -master.port.grpc="$MASTER_GRPC_PORT" \
  -master.peers="127.0.0.1:$MASTER_PORT" \
  -master.raftBootstrap \
  -volume.port="$VOLUME_PORT" \
  -volume.port.grpc="$VOLUME_GRPC_PORT" \
  -filer \
  -filer.port="$FILER_PORT" \
  -filer.port.grpc="$FILER_GRPC_PORT" \
  -s3 \
  -s3.port="$S3_PORT" \
  -s3.port.grpc="$S3_GRPC_PORT" \
  -s3.port.iceberg="$S3_ICEBERG_PORT" \
  -s3.port.lance="$S3_LANCE_PORT" \
  -s3.ip.bind=127.0.0.1 \
  -s3.config="$RUNTIME_DIR/s3_config.json" \
  > "$RUNTIME_DIR/logs/weed.log" 2>&1 &
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

ready="0"
for _ in $(seq 1 90); do
  if ! kill -0 "$weed_pid" 2>/dev/null; then
    echo "ERROR: weed exited before readiness. last logs:" >&2
    tail -n 120 "$RUNTIME_DIR/logs/weed.log" >&2 || true
    exit 1
  fi
  code="$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$S3_PORT/" || true)"
  if [[ "$code" == "200" || "$code" == "403" ]]; then
    ready="1"
    break
  fi
  sleep 1
done

if [[ "$ready" != "1" ]]; then
  echo "ERROR: S3 endpoint not ready on 127.0.0.1:$S3_PORT" >&2
  tail -n 120 "$RUNTIME_DIR/logs/weed.log" >&2 || true
  exit 1
fi

echo "ready endpoint=http://127.0.0.1:$S3_PORT"
echo "env file: $RUNTIME_DIR/local_s3.env"
echo "logs: $RUNTIME_DIR/logs/weed.log"
echo "PID: $weed_pid"

tail -n 20 "$RUNTIME_DIR/logs/weed.log" || true
set +e
wait "$weed_pid"
weed_rc=$?
set -e
if [[ -n "$monitor_pid" ]] && kill -0 "$monitor_pid" 2>/dev/null; then
  kill "$monitor_pid" 2>/dev/null || true
  wait "$monitor_pid" 2>/dev/null || true
fi
if [[ -f "$BUDGET_FAILURE_FILE" ]]; then
  echo "ERROR: disk budget violation. See $BUDGET_FAILURE_FILE and $BUDGET_REPORT_FILE" >&2
  exit 2
fi
exit "$weed_rc"
