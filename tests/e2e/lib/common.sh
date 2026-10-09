# shellcheck shell=bash disable=SC2154 # Shared state is set by run.sh.
# Shared helpers for the ZFS + S3 end-to-end scenarios. Set E2E_TRACE=1 to log
# every privileged and snapshot-to-s3 command to stderr.

die() {
    echo "$*" >&2
    exit 1
}

e2e_trace() {
    [[ "${E2E_TRACE:-0}" == 1 ]] || return 0
    printf '+' >&2
    printf ' %q' "$@" >&2
    printf '\n' >&2
}

as_root() {
    e2e_trace sudo "$@"
    sudo -n "$@"
}

# Run snapshot-to-s3 against the isolated endpoint. Backups only get the
# public keyring, proving that encryption never needs the private key.
cli() {
    local GNUPGHOME="$GNUPGHOME"
    if [[ "$1" == backup ]]; then
        GNUPGHOME="$gpg_public_home"
    fi
    export GNUPGHOME
    e2e_trace snapshot-to-s3 "$@"
    sudo -n --preserve-env=GNUPGHOME,AWS_ACCESS_KEY_ID,AWS_SECRET_ACCESS_KEY,AWS_SESSION_TOKEN \
        "$binary" "$@" --endpoint "$TEST_S3_ENDPOINT" --region us-east-1
}

# expect_failure MESSAGE COMMAND...: fail the run with MESSAGE if COMMAND succeeds.
expect_failure() {
    local message="$1"
    shift
    if "$@"; then
        die "$message"
    fi
}

file_sha256() {
    as_root sha256sum "$1" | cut -d ' ' -f 1
}
