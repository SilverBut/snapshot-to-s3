# shellcheck shell=bash disable=SC2154 # Shared state is set by run.sh.
# Restore only s1 into `continued`, the existing base for the continue case.
# requires: full

cli restore "$prefix" "zfs:$namespace/source@s1" --target-dataset "$id/continued"
mount_restored "$namespace/continued" "$runtime/continued"
