#!/usr/bin/env bash
# ZFS + S3 end-to-end scenarios against a labelled development pool and an
# isolated S3 endpoint (normally tests/e2e/local_s3.sh).
#
# usage: run.sh [--list] [--case NAME]...
#
# Cases live in cases/NN_name.sh and run in NN order in one shared namespace.
# `# requires:` names the cases whose state a case builds on; --case runs the
# named cases plus everything they require. E2E_TRACE=1 logs each command.
set -euo pipefail
umask 077

E2E_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=tests/e2e/lib/common.sh
source "$E2E_DIR/lib/common.sh"
# shellcheck source=tests/e2e/lib/zfs.sh
source "$E2E_DIR/lib/zfs.sh"
# shellcheck source=tests/e2e/lib/gpg.sh
source "$E2E_DIR/lib/gpg.sh"

declare -a case_names=()
declare -A case_file=() case_requires=() selected=()
for file in "$E2E_DIR"/cases/[0-9][0-9]_*.sh; do
    name="$(basename "$file" .sh)"
    name="${name#[0-9][0-9]_}"
    case_names+=("$name")
    case_file[$name]="$file"
    case_requires[$name]="$(sed -n 's/^# requires:[[:space:]]*//p' "$file")"
done

usage() {
    echo "usage: run.sh [--list] [--case NAME]..." >&2
    exit 2
}

select_case() {
    local name="$1" required
    [[ -n "${case_file[$name]:-}" ]] || die "unknown case: $name (see --list)"
    [[ -z "${selected[$name]:-}" ]] || return 0
    selected[$name]=1
    for required in ${case_requires[$name]}; do
        select_case "$required"
    done
}

while (($#)); do
    case "$1" in
        --list)
            for name in "${case_names[@]}"; do
                printf '%-16s requires: %s\n' "$name" "${case_requires[$name]:-(none)}"
            done
            exit 0
            ;;
        --case)
            [[ $# -ge 2 ]] || usage
            select_case "$2"
            shift 2
            ;;
        *) usage ;;
    esac
done
if ((${#selected[@]} == 0)); then
    for name in "${case_names[@]}"; do
        selected[$name]=1
    done
fi

: "${TEST_S3_ENDPOINT:?set an isolated local S3 endpoint}"
: "${TEST_S3_BUCKET:?set a dedicated test bucket}"
: "${AWS_ACCESS_KEY_ID:?set test credentials}"
: "${AWS_SECRET_ACCESS_KEY:?set test credentials}"
binary="$(realpath "${SNAPSHOT_TO_S3_BIN:-target/debug/snapshot-to-s3}")"

# --- isolated namespace ----------------------------------------------------------

sudo -n true
select_dev_pool
id="smoke_$(date +%s)_${RANDOM}_${RANDOM}"
namespace="$pool/$id"
require_absent "$pool" "$namespace"
runtime="$(realpath -m "${E2E_RUNTIME_DIR:-target/test-artifacts/zfs_s3_e2e_$id}")"
mkdir -p -- "$(dirname "$runtime")"
mkdir -m 700 -- "$runtime"
created=false

# Destroy only the namespace this run created. Diagnostics are retained when
# anything failed or a mount is left behind.
cleanup() {
    local status=$?
    trap - EXIT
    if "$created"; then
        if ! sudo -n zfs destroy -r "$namespace"; then
            echo "cleanup failed: namespace $namespace, runtime $runtime and GPG runtime $gpg_runtime retained" >&2
            exit 1
        fi
    fi
    if findmnt -rn -o TARGET | grep -F -- "$runtime/" >/dev/null; then
        echo "test mount remains; retaining $runtime" >&2
        exit 1
    fi
    gpg_stop_agents || status=1
    if [[ "$status" -eq 0 ]]; then
        rm -r -- "$runtime"
        if [[ -n "$gpg_runtime" ]]; then
            rm -rf -- "$gpg_runtime"
        fi
    else
        echo "test failed; diagnostic runtime retained: $runtime; GPG runtime: $gpg_runtime" >&2
    fi
    exit "$status"
}
trap cleanup EXIT

gpg_setup
free="$(df -B1 --output=avail "$runtime" | tail -1 | tr -d ' ')"
min_free="${E2E_MIN_FREE_BYTES:-21474836480}"
[[ "$min_free" =~ ^[1-9][0-9]*$ ]] || die "invalid E2E_MIN_FREE_BYTES"
[[ "$free" -ge "$min_free" ]] || die "requires at least $min_free bytes free"
as_root zfs create -o mountpoint=none -o canmount=off -o atime=off "$namespace"
created=true
mkdir "$runtime/source"
as_root zfs create -o canmount=on -o atime=off -o mountpoint="$runtime/source" "$namespace/source"
assert_zfs_mount "$runtime/source"
as_root dd if=/dev/urandom of="$runtime/source/data" bs=1M count=10 status=none
as_root zfs snapshot "$namespace/source@s1"
prefix="s3://$TEST_S3_BUCKET/$id"

# --- cases -----------------------------------------------------------------------

ran=()
for name in "${case_names[@]}"; do
    [[ -n "${selected[$name]:-}" ]] || continue
    echo "=== case: $name" >&2
    # shellcheck source=/dev/null # Cases are listed above.
    source "${case_file[$name]}"
    ran+=("$name")
done
echo "PASS: ${ran[*]}" >&2
echo "remote test objects retained under $prefix; use the dedicated local service cleanup" >&2
