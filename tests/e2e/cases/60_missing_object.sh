# shellcheck shell=bash disable=SC2154 # Shared state is set by run.sh.
# With the s1 stream object deleted, restoring s3 into an empty target must
# fail instead of recovering a partial chain.
# requires: full incremental

python3 "$E2E_DIR/../tooling/s3_probe.py" --region us-east-1 \
    --delete-test-object "$id/$namespace/source/s1/stream.encrypted"
expect_failure "incomplete remote chain unexpectedly recovered an empty target" \
    cli restore "$prefix" "zfs:$namespace/source@s3" --target-dataset "$id/incomplete"
