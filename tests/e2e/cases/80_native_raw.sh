# shellcheck shell=bash disable=SC2154 # Shared state is set by run.sh.
# A natively encrypted dataset is sent raw and restored still encrypted and
# unmounted (its key is not available to the restore).
# requires:

mkdir "$runtime/native"
# This is a throwaway native ZFS key, not an application backup key.
head -c 32 /dev/urandom | od -An -v -tx1 | tr -d ' \n' > "$runtime/native.hex"
printf '\n' >> "$runtime/native.hex"
as_root zfs create -o encryption=aes-256-gcm -o keyformat=hex -o keylocation="file://$runtime/native.hex" \
    -o atime=off -o canmount=on -o mountpoint="$runtime/native" "$namespace/native"
assert_zfs_mount "$runtime/native"
printf 'native raw encrypted source\n' | as_root tee "$runtime/native/data" >/dev/null
as_root zfs snapshot "$namespace/native@n1"
cli backup "zfs:$namespace/native@n1" "$prefix" --gpg-key-id "$fingerprint"
cli restore "$prefix" "zfs:$namespace/native@n1" --target-dataset "$id/native-recovered"
assert_same_guid "$namespace/native@n1" "$namespace/native-recovered@n1"
encryption="$(property_value zfs "$namespace/native-recovered" encryption)"
mounted="$(property_value zfs "$namespace/native-recovered" mounted)"
[[ "$encryption" == aes-256-gcm ]] || die "native restore lost encryption: $encryption"
[[ "$mounted" == no ]] || die "native restore was mounted without its key"
