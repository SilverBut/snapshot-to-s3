# shellcheck shell=bash
# Throwaway GnuPG homes: `gpg_home` holds the private key used for restores,
# `gpg_public_home` only the trusted public key used for backups.

gpg_runtime=""
gpg_home=""
gpg_public_home=""
fingerprint=""

gpg_setup() {
    local socket_path
    gpg_runtime="$(realpath -m "${E2E_GPG_DIR:-.gpg_${RANDOM}_${RANDOM}}")"
    socket_path="$gpg_runtime/gpg-public/S.gpg-agent.browser"
    [[ "$(LC_ALL=C printf %s "$socket_path" | wc -c)" -lt 108 ]] ||
        die "GPG socket path is too long; set E2E_GPG_DIR to a short dedicated directory"
    mkdir -p -- "$(dirname "$gpg_runtime")"
    mkdir -m 700 -- "$gpg_runtime"
    gpg_home="$gpg_runtime/gpg"
    gpg_public_home="$gpg_runtime/gpg-public"
    mkdir -m 700 "$gpg_home"
    export GNUPGHOME="$gpg_home"
    gpg --batch --pinentry-mode loopback --passphrase "" \
        --quick-generate-key "snapshot-to-s3 isolated test" default default never
    fingerprint="$(gpg --batch --with-colons --list-keys | awk -F: '$1=="fpr" {print $10; exit}')"
    [[ -n "$fingerprint" ]] || die "test GPG key was not created"
    mkdir -m 700 "$gpg_public_home"
    gpg --batch --export "$fingerprint" | gpg --batch --homedir "$gpg_public_home" --import
    printf '%s:6:\n' "$fingerprint" | gpg --batch --homedir "$gpg_public_home" --import-ownertrust
}

# Stop the agents of both homes; returns 1 if any could not be stopped.
gpg_stop_agents() {
    local status=0 keyhome agent_pid
    for keyhome in "$gpg_home" "$gpg_public_home"; do
        [[ -d "$keyhome" ]] || continue
        agent_pid="$(gpg-connect-agent --no-autostart --homedir "$keyhome" 'GETINFO pid' /bye 2>/dev/null |
            awk '$1=="D" && $2 ~ /^[0-9]+$/ {print $2}')" || {
            echo "could not discover test GPG agent for $keyhome" >&2
            status=1
            continue
        }
        if [[ "$agent_pid" =~ ^[0-9]+$ ]]; then
            kill "$agent_pid" || { echo "GPG PID $agent_pid cleanup failed" >&2; status=1; }
        fi
    done
    return "$status"
}
