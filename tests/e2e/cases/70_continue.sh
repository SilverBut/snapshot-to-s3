# shellcheck shell=bash disable=SC2154 # Shared state is set by run.sh.
# Continue `continued` from its existing s1 base to s3. This deliberately runs
# after missing_object: the deleted s1 object must not be needed. A repeat is
# a no-op, and a target modified after its last snapshot is refused.
# requires: incremental restore_base missing_object

cli restore "$prefix" "zfs:$namespace/source@s3" --target-dataset "$id/continued"
cli restore "$prefix" "zfs:$namespace/source@s3" --target-dataset "$id/continued"
printf 'dirty\n' | as_root tee "$runtime/continued/uncommitted" >/dev/null
expect_failure "dirty target unexpectedly accepted" \
    cli restore "$prefix" "zfs:$namespace/source@s3" --target-dataset "$id/continued"
