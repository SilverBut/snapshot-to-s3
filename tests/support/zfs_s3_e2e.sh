#!/usr/bin/env bash
set -euo pipefail
umask 077

: "${TEST_S3_ENDPOINT:?set an isolated local S3 endpoint}"
: "${TEST_S3_BUCKET:?set a dedicated test bucket}"
: "${AWS_ACCESS_KEY_ID:?set test credentials}"
: "${AWS_SECRET_ACCESS_KEY:?set test credentials}"
binary="$(realpath "${SNAPSHOT_TO_S3_BIN:-target/debug/snapshot-to-s3}")"
sudo -n true
pool=""
while IFS=$'\t' read -r name health; do
    if [[ "$health" == ONLINE ]] && [[ "$(zpool get -H -o value user:isdev "$name")" == yes ]]; then
        pool="$name"
        break
    fi
done < <(zpool list -H -o name,health)
[[ -n "$pool" ]] || { echo "no ONLINE user:isdev=yes pool; provide a development pool" >&2; exit 1; }
[[ "$(zpool get -H -o value user:isdev "$pool")" == yes ]]
id="smoke_$(date +%s)_${RANDOM}_${RANDOM}"
namespace="$pool/$id"
if zfs list -H "$namespace" >/dev/null 2>&1; then
    echo "test namespace already exists: $namespace" >&2
    exit 1
fi
runtime="$(mktemp -d /tmp/snapshot-to-s3-e2e.XXXXXXXX)"
created=false
cleanup() {
    local status=$?
    trap - EXIT
    if "$created"; then
        if ! sudo -n zfs destroy -r "$namespace"; then
            echo "cleanup failed: namespace $namespace and runtime $runtime retained" >&2
            exit 1
        fi
    fi
    if findmnt -rn -o TARGET | grep -F -- "$runtime/" >/dev/null; then
        echo "test mount remains; retaining $runtime" >&2
        exit 1
    fi
    for keyhome in "$runtime/gpg" "$runtime/gpg-public"; do
        if [[ -d "$keyhome" ]]; then
            agent_pid="$(gpg-connect-agent --no-autostart --homedir "$keyhome" 'GETINFO pid' /bye 2>/dev/null | awk '$1=="D" && $2 ~ /^[0-9]+$/ {print $2}')" || {
                echo "could not discover test GPG agent for $keyhome" >&2
                continue
            }
            if [[ "$agent_pid" =~ ^[0-9]+$ ]]; then
                kill "$agent_pid" || { echo "GPG PID $agent_pid cleanup failed" >&2; status=1; }
            fi
        fi
    done
    if [[ "$status" -eq 0 ]]; then
        rm -r -- "$runtime"
    else
        echo "test failed; diagnostic runtime retained: $runtime" >&2
    fi
    exit "$status"
}
trap cleanup EXIT
export GNUPGHOME="$runtime/gpg"
mkdir -m 700 "$GNUPGHOME"
gpg --batch --pinentry-mode loopback --passphrase "" --quick-generate-key "snapshot-to-s3 isolated test" default default never
fingerprint="$(gpg --batch --with-colons --list-keys | awk -F: '$1=="fpr" {print $10; exit}')"
[[ -n "$fingerprint" ]]
mkdir -m 700 "$runtime/gpg-public"
gpg --batch --export "$fingerprint" | gpg --batch --homedir "$runtime/gpg-public" --import
printf '%s:6:\n' "$fingerprint" | gpg --batch --homedir "$runtime/gpg-public" --import-ownertrust
free="$(df -B1 --output=avail /tmp | tail -1 | tr -d ' ')"
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
        GNUPGHOME="$runtime/gpg-public"
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
printf 'second snapshot\n' | sudo -n tee "$runtime/source/second" >/dev/null
sudo -n zfs snapshot "$namespace/source@s2"
cli backup "zfs:$namespace/source@s2" "$prefix" --gpg-key-id "$fingerprint"
printf 'third snapshot\n' | sudo -n tee "$runtime/source/third" >/dev/null
sudo -n zfs snapshot "$namespace/source@s3"
cli backup "zfs:$namespace/source@s3" "$prefix" --gpg-key-id "$fingerprint"
cli restore "$prefix" "zfs:$namespace/source@s3" --target-dataset "$id/recovered" --gpg-key-id "$fingerprint"
for snapshot in s1 s2 s3; do
    [[ "$(zfs get -Hp -o value guid "$namespace/source@$snapshot")" == "$(zfs get -Hp -o value guid "$namespace/recovered@$snapshot")" ]]
done
mkdir "$runtime/recovered"
sudo -n zfs set atime=off canmount=on mountpoint="$runtime/recovered" "$namespace/recovered"
if [[ "$(zfs get -H -o value mounted "$namespace/recovered")" != yes ]]; then
    sudo -n zfs mount "$namespace/recovered"
fi
[[ "$(findmnt -n -o FSTYPE -T "$runtime/recovered")" == zfs ]]
for file in data second third; do
    [[ "$(sudo -n sha256sum "$runtime/source/$file" | cut -d ' ' -f 1)" == "$(sudo -n sha256sum "$runtime/recovered/$file" | cut -d ' ' -f 1)" ]]
done
cli restore "$prefix" "zfs:$namespace/source@s1" --target-dataset "$id/continued"
mkdir "$runtime/continued"
sudo -n zfs set atime=off canmount=on mountpoint="$runtime/continued" "$namespace/continued"
if [[ "$(zfs get -H -o value mounted "$namespace/continued")" != yes ]]; then
    sudo -n zfs mount "$namespace/continued"
fi
[[ "$(findmnt -n -o FSTYPE -T "$runtime/continued")" == zfs ]]
cli restore "$prefix" "stdout:$namespace/source@s1" | sudo -n zfs receive -u "$namespace/exported"
[[ "$(zfs get -Hp -o value guid "$namespace/exported@s1")" == "$(zfs get -Hp -o value guid "$namespace/source@s1")" ]]
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
[[ "$(zfs get -Hp -o value guid "$namespace/native@n1")" == "$(zfs get -Hp -o value guid "$namespace/native-recovered@n1")" ]]
[[ "$(zfs get -H -o value encryption "$namespace/native-recovered")" == aes-256-gcm ]]
[[ "$(zfs get -H -o value mounted "$namespace/native-recovered")" == no ]]
echo "PASS: full + incrementals + GUID/data + existing base + no-op + dirty rejection + single stdout export + native raw encryption" >&2
echo "remote test objects retained under $prefix; use the dedicated local service cleanup" >&2
