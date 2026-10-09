# shellcheck shell=bash disable=SC2154 # Shared state is set by run.sh.
# Full backup of s1; a second backup of the same snapshot must be refused.
# requires:

cli backup "zfs:$namespace/source@s1" "$prefix" --gpg-key-id "$fingerprint" --force-full-snapshot
expect_failure "duplicate backup unexpectedly succeeded" \
    cli backup "zfs:$namespace/source@s1" "$prefix" --gpg-key-id "$fingerprint"
