# shellcheck shell=bash disable=SC2154 # Shared state is set by run.sh.
# Export a restored stream to stdout and receive it with plain `zfs receive`.
# requires: full

cli restore "$prefix" "stdout:$namespace/source@s1" | as_root zfs receive -u "$namespace/exported"
assert_same_guid "$namespace/exported@s1" "$namespace/source@s1"
