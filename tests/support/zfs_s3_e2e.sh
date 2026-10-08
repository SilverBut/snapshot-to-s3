#!/usr/bin/env bash
set -euo pipefail
umask 077

: "${TEST_S3_ENDPOINT:?set an isolated local S3 endpoint}"
: "${TEST_S3_BUCKET:?set a dedicated test bucket}"
: "${AWS_ACCESS_KEY_ID:?set test credentials}"
: "${AWS_SECRET_ACCESS_KEY:?set test credentials}"
binary="$(realpath "${SNAPSHOT_TO_S3_BIN:-target/debug/snapshot-to-s3}")"
# Read only the OpenZFS JSON machine interface; never parse display tables.
json_read() {
    python3 -c '
import json, re, sys

try:
    tool, _command, mode, *args = sys.argv[1:]
    document = json.load(sys.stdin)
    objects = document["pools" if tool == "zpool" else "datasets"]
    if not isinstance(objects, dict):
        raise ValueError("expected a named object map")
    for name, item in objects.items():
        if not name or not isinstance(item, dict) or item.get("name") != name:
            raise ValueError("object map key/name mismatch")
        allowed = ("POOL",) if tool == "zpool" else ("FILESYSTEM", "SNAPSHOT", "VOLUME")
        if item.get("type") not in allowed:
            raise ValueError("invalid object type")
    def value(name, prop):
        raw = objects[name]["properties"][prop]["value"]
        if not isinstance(raw, str) or not raw or "\n" in raw or "\r" in raw:
            raise ValueError("missing, empty or malformed property " + prop)
        if prop == "guid" and (not re.fullmatch(r"[1-9][0-9]*", raw)
                              or int(raw) > 18446744073709551615):
            raise ValueError("invalid decimal GUID")
        return raw
    if mode == "online":
        for name, item in objects.items():
            health = value(name, "health")
            if not isinstance(item.get("state"), str) or not item["state"]:
                raise ValueError("missing pool state")
            if health == "ONLINE" and item["state"] == "ONLINE":
                print(name)
    elif mode == "absent":
        if args[0] in objects:
            raise ValueError("test namespace already exists: " + args[0])
    elif mode == "value":
        if tool == "zfs":
            expected = "SNAPSHOT" if "@" in args[0] else "FILESYSTEM"
            if objects[args[0]]["type"] != expected:
                raise ValueError("unexpected requested dataset type")
        print(value(*args))
    else:
        raise ValueError("unknown JSON reader mode")
except (ValueError, KeyError, TypeError, IndexError) as error:
    sys.exit("invalid OpenZFS JSON: " + str(error))
' "$@"
}
property_value() {
    local tool="$1" name="$2" property="$3" output
    output="$("$tool" get -j -p "$property" "$name")" || return
    json_read "$tool" get value "$name" "$property" <<<"$output"
}
sudo -n true
pool_json="$(zpool list -j -p -o name,health)"
online_pools="$(json_read zpool list online <<<"$pool_json")"
pool=""
while IFS= read -r name; do
    [[ -n "$name" ]] || continue
    label="$(property_value zpool "$name" user:isdev)"
    if [[ "$label" == yes ]]; then
        pool="$name"
        break
    fi
done <<<"$online_pools"
[[ -n "$pool" ]] || { echo "no ONLINE user:isdev=yes pool; provide a development pool" >&2; exit 1; }
label="$(property_value zpool "$pool" user:isdev)"
[[ "$label" == yes ]] || { echo "selected pool is no longer user:isdev=yes: $pool" >&2; exit 1; }
id="smoke_$(date +%s)_${RANDOM}_${RANDOM}"
namespace="$pool/$id"
namespace_json="$(zfs list -j -p -r -o name "$pool")"
json_read zfs list absent "$namespace" <<<"$namespace_json"
runtime="$(realpath -m "${E2E_RUNTIME_DIR:-target/test-artifacts/zfs_s3_e2e_$id}")"
mkdir -p -- "$(dirname "$runtime")"
mkdir -m 700 -- "$runtime"
gpg_runtime=""
gpg_home=""
gpg_public_home=""
created=false
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
    for keyhome in "$gpg_home" "$gpg_public_home"; do
        if [[ -d "$keyhome" ]]; then
            agent_pid="$(gpg-connect-agent --no-autostart --homedir "$keyhome" 'GETINFO pid' /bye 2>/dev/null | awk '$1=="D" && $2 ~ /^[0-9]+$/ {print $2}')" || {
                echo "could not discover test GPG agent for $keyhome" >&2
                status=1
                continue
            }
            if [[ "$agent_pid" =~ ^[0-9]+$ ]]; then
                kill "$agent_pid" || { echo "GPG PID $agent_pid cleanup failed" >&2; status=1; }
            fi
        fi
    done
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
gpg_runtime="$(realpath -m "${E2E_GPG_DIR:-.gpg_${RANDOM}_${RANDOM}}")"
socket_path="$gpg_runtime/gpg-public/S.gpg-agent.browser"
[[ "$(LC_ALL=C printf %s "$socket_path" | wc -c)" -lt 108 ]] || {
    echo "GPG socket path is too long; set E2E_GPG_DIR to a short dedicated directory" >&2
    exit 1
}
mkdir -p -- "$(dirname "$gpg_runtime")"
mkdir -m 700 -- "$gpg_runtime"
gpg_home="$gpg_runtime/gpg"
gpg_public_home="$gpg_runtime/gpg-public"
mkdir -m 700 "$gpg_home"
export GNUPGHOME="$gpg_home"
gpg --batch --pinentry-mode loopback --passphrase "" --quick-generate-key "snapshot-to-s3 isolated test" default default never
fingerprint="$(gpg --batch --with-colons --list-keys | awk -F: '$1=="fpr" {print $10; exit}')"
[[ -n "$fingerprint" ]]
mkdir -m 700 "$gpg_public_home"
gpg --batch --export "$fingerprint" | gpg --batch --homedir "$gpg_public_home" --import
printf '%s:6:\n' "$fingerprint" | gpg --batch --homedir "$gpg_public_home" --import-ownertrust
free="$(df -B1 --output=avail "$runtime" | tail -1 | tr -d ' ')"
[[ "$free" -ge 21474836480 ]] || { echo "requires at least 20GiB free" >&2; exit 1; }
sudo -n zfs create -o mountpoint=none -o canmount=off -o atime=off "$namespace"
created=true
mkdir "$runtime/source"
sudo -n zfs create -o canmount=on -o atime=off -o mountpoint="$runtime/source" "$namespace/source"
[[ "$(findmnt -n -o FSTYPE -T "$runtime/source")" == zfs ]]
sudo -n dd if=/dev/urandom of="$runtime/source/data" bs=1M count=10 status=none
sudo -n zfs snapshot "$namespace/source@s1"
prefix="s3://$TEST_S3_BUCKET/$id"
cli() {
    local GNUPGHOME="$GNUPGHOME"
    if [[ "$1" == backup ]]; then
        GNUPGHOME="$gpg_public_home"
    fi
    export GNUPGHOME
    sudo -n --preserve-env=GNUPGHOME,AWS_ACCESS_KEY_ID,AWS_SECRET_ACCESS_KEY,AWS_SESSION_TOKEN \
        "$binary" "$@" --endpoint "$TEST_S3_ENDPOINT" --region us-east-1
}
cli backup "zfs:$namespace/source@s1" "$prefix" --gpg-key-id "$fingerprint" --force-full-snapshot
if cli backup "zfs:$namespace/source@s1" "$prefix" --gpg-key-id "$fingerprint"; then
    echo "duplicate backup unexpectedly succeeded" >&2
    exit 1
fi
# Make s2 a materially cheaper incremental base for s3 than s1.
sudo -n dd if=/dev/urandom of="$runtime/source/second" bs=1M count=2 status=none
sudo -n zfs snapshot "$namespace/source@s2"
cli backup "zfs:$namespace/source@s2" "$prefix" --gpg-key-id "$fingerprint"
printf 'third snapshot\n' | sudo -n tee "$runtime/source/third" >/dev/null
sudo -n zfs snapshot "$namespace/source@s3"
cli backup "zfs:$namespace/source@s3" "$prefix" --gpg-key-id "$fingerprint"
cli restore "$prefix" "zfs:$namespace/source@s3" --target-dataset "$id/recovered" --gpg-key-id "$fingerprint"
for snapshot in s1 s2 s3; do
    source_guid="$(property_value zfs "$namespace/source@$snapshot" guid)"
    recovered_guid="$(property_value zfs "$namespace/recovered@$snapshot" guid)"
    [[ "$source_guid" == "$recovered_guid" ]]
done
mkdir "$runtime/recovered"
sudo -n zfs set atime=off canmount=on mountpoint="$runtime/recovered" "$namespace/recovered"
mounted="$(property_value zfs "$namespace/recovered" mounted)"
if [[ "$mounted" != yes ]]; then
    sudo -n zfs mount "$namespace/recovered"
fi
[[ "$(findmnt -n -o FSTYPE -T "$runtime/recovered")" == zfs ]]
for file in data second third; do
    [[ "$(sudo -n sha256sum "$runtime/source/$file" | cut -d ' ' -f 1)" == "$(sudo -n sha256sum "$runtime/recovered/$file" | cut -d ' ' -f 1)" ]]
done
cli restore "$prefix" "zfs:$namespace/source@s1" --target-dataset "$id/continued"
mkdir "$runtime/continued"
sudo -n zfs set atime=off canmount=on mountpoint="$runtime/continued" "$namespace/continued"
mounted="$(property_value zfs "$namespace/continued" mounted)"
if [[ "$mounted" != yes ]]; then
    sudo -n zfs mount "$namespace/continued"
fi
[[ "$(findmnt -n -o FSTYPE -T "$runtime/continued")" == zfs ]]
cli restore "$prefix" "stdout:$namespace/source@s1" | sudo -n zfs receive -u "$namespace/exported"
exported_guid="$(property_value zfs "$namespace/exported@s1" guid)"
source_guid="$(property_value zfs "$namespace/source@s1" guid)"
[[ "$exported_guid" == "$source_guid" ]]
python3 tests/support/s3_probe.py --region us-east-1 --delete-test-object "$id/$namespace/source/s1/stream.encrypted"
if cli restore "$prefix" "zfs:$namespace/source@s3" --target-dataset "$id/incomplete"; then
    echo "incomplete remote chain unexpectedly recovered an empty target" >&2
    exit 1
fi
cli restore "$prefix" "zfs:$namespace/source@s3" --target-dataset "$id/continued"
cli restore "$prefix" "zfs:$namespace/source@s3" --target-dataset "$id/continued"
printf 'dirty\n' | sudo -n tee "$runtime/continued/uncommitted" >/dev/null
if cli restore "$prefix" "zfs:$namespace/source@s3" --target-dataset "$id/continued"; then
    echo "dirty target unexpectedly accepted" >&2
    exit 1
fi
mkdir "$runtime/native"
# This is a throwaway native ZFS key, not an application backup key.
head -c 32 /dev/urandom | od -An -v -tx1 | tr -d ' \n' > "$runtime/native.hex"
printf '\n' >> "$runtime/native.hex"
sudo -n zfs create -o encryption=aes-256-gcm -o keyformat=hex -o keylocation="file://$runtime/native.hex" \
    -o atime=off -o canmount=on -o mountpoint="$runtime/native" "$namespace/native"
[[ "$(findmnt -n -o FSTYPE -T "$runtime/native")" == zfs ]]
printf 'native raw encrypted source\n' | sudo -n tee "$runtime/native/data" >/dev/null
sudo -n zfs snapshot "$namespace/native@n1"
cli backup "zfs:$namespace/native@n1" "$prefix" --gpg-key-id "$fingerprint"
cli restore "$prefix" "zfs:$namespace/native@n1" --target-dataset "$id/native-recovered"
native_guid="$(property_value zfs "$namespace/native@n1" guid)"
recovered_guid="$(property_value zfs "$namespace/native-recovered@n1" guid)"
[[ "$native_guid" == "$recovered_guid" ]]
encryption="$(property_value zfs "$namespace/native-recovered" encryption)"
mounted="$(property_value zfs "$namespace/native-recovered" mounted)"
[[ "$encryption" == aes-256-gcm ]]
[[ "$mounted" == no ]]
echo "PASS: full + incrementals + GUID/data + existing base + no-op + dirty rejection + single stdout export + native raw encryption" >&2
echo "remote test objects retained under $prefix; use the dedicated local service cleanup" >&2
