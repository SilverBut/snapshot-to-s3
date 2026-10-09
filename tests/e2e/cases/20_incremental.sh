# shellcheck shell=bash disable=SC2154 # Shared state is set by run.sh.
# Incremental backups s2 and s3, then a full-chain restore that must preserve
# snapshot GUIDs and file contents.
# requires: full

# Make s2 a materially cheaper incremental base for s3 than s1.
as_root dd if=/dev/urandom of="$runtime/source/second" bs=1M count=2 status=none
as_root zfs snapshot "$namespace/source@s2"
cli backup "zfs:$namespace/source@s2" "$prefix" --gpg-key-id "$fingerprint"
printf 'third snapshot\n' | as_root tee "$runtime/source/third" >/dev/null
as_root zfs snapshot "$namespace/source@s3"
cli backup "zfs:$namespace/source@s3" "$prefix" --gpg-key-id "$fingerprint"
cli restore "$prefix" "zfs:$namespace/source@s3" --target-dataset "$id/recovered" --gpg-key-id "$fingerprint"
for snapshot in s1 s2 s3; do
    assert_same_guid "$namespace/source@$snapshot" "$namespace/recovered@$snapshot"
done
mount_restored "$namespace/recovered" "$runtime/recovered"
for file in data second third; do
    [[ "$(file_sha256 "$runtime/source/$file")" == "$(file_sha256 "$runtime/recovered/$file")" ]] ||
        die "restored $file differs from the source"
done
