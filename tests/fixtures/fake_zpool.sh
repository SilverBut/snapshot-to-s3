#!/usr/bin/env bash
# Fake `zpool` for tests/zfs_backend: accepts only `list -j -p -o name POOL`,
# records arguments in $base/commands and prints $base/pool.json if present.
set -euo pipefail
base="__BASE__"
printf 'zpool %s\n' "$*" >> "$base/commands"
if [[ "$#" != 6 || "$1" != "list" || "$2" != "-j" || "$3" != "-p" || "$4" != "-o" || "$5" != "name" ]]; then
  echo "unexpected zpool arguments: $*" >&2
  exit 99
fi
if [[ -f "$base/pool.json" ]]; then
  cat "$base/pool.json"
  exit 0
fi
if [[ "$6" == "pool" ]]; then
  printf '{"output_version":{"command":"zpool list","vers_major":0,"vers_minor":1},"pools":{"pool":{"name":"pool","type":"POOL","state":"ONLINE","pool_guid":"22","properties":{}}}}\n'
  exit 0
fi
echo "no such pool" >&2
exit 1
