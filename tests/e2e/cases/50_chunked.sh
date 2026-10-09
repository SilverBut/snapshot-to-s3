# shellcheck shell=bash disable=SC2154 # Shared state is set by run.sh.
# A stream larger than --max-object-size continues in further objects.
# requires:

chunked="$prefix/chunked"
if ! cli backup "zfs:$namespace/source@s1" "$chunked" --gpg-key-id "$fingerprint" --force-full-snapshot \
    --max-object-size 8388608 --part-buffer-size 8388608 2>"$runtime/chunked.log" ||
    ! grep -q ' in 2 objects)' "$runtime/chunked.log"; then
    cat "$runtime/chunked.log" >&2
    exit 1
fi
cli restore "$chunked" "stdout:$namespace/source@s1" | as_root zfs receive -u "$namespace/chunked"
assert_same_guid "$namespace/chunked@s1" "$namespace/source@s1"
