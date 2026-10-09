# shellcheck shell=bash
# ZFS helpers. Pool and dataset state is read only through `-j` JSON output
# validated by zfs_json.py.

json_read() {
    python3 "$E2E_DIR/lib/zfs_json.py" "$@"
}

# property_value TOOL NAME PROPERTY
property_value() {
    local tool="$1" name="$2" property="$3" output
    output="$("$tool" get -j -p "$property" "$name")" || return
    json_read "$tool" get value "$name" "$property" <<<"$output"
}

# Set `pool` to the first ONLINE pool labelled user:isdev=yes, then recheck
# the label immediately before use.
select_dev_pool() {
    local pool_json online_pools name label
    pool_json="$(zpool list -j -p -o name,health)"
    online_pools="$(json_read zpool list online <<<"$pool_json")"
    pool=""
    while IFS= read -r name; do
        [[ -n "$name" ]] || continue
        label="$(property_value zpool "$name" user:isdev)"
        if [[ "$label" == yes ]]; then
            pool="$name"
            break
        fi
    done <<<"$online_pools"
    [[ -n "$pool" ]] || die "no ONLINE user:isdev=yes pool; provide a development pool"
    label="$(property_value zpool "$pool" user:isdev)"
    [[ "$label" == yes ]] || die "selected pool is no longer user:isdev=yes: $pool"
}

require_absent() {
    local namespace_json
    namespace_json="$(zfs list -j -p -r -o name "$1")"
    json_read zfs list absent "$2" <<<"$namespace_json"
}

assert_zfs_mount() {
    [[ "$(findmnt -n -o FSTYPE -T "$1")" == zfs ]] || die "not a ZFS mount: $1"
}

# mount_restored DATASET MOUNTPOINT: mount a restored filesystem for reading.
# atime=off keeps reads from dirtying it before a later incremental receive.
mount_restored() {
    local dataset="$1" mountpoint="$2" mounted
    mkdir "$mountpoint"
    as_root zfs set atime=off canmount=on mountpoint="$mountpoint" "$dataset"
    mounted="$(property_value zfs "$dataset" mounted)"
    if [[ "$mounted" != yes ]]; then
        as_root zfs mount "$dataset"
    fi
    assert_zfs_mount "$mountpoint"
}

assert_same_guid() {
    local left right
    left="$(property_value zfs "$1" guid)"
    right="$(property_value zfs "$2" guid)"
    [[ "$left" == "$right" ]] || die "GUID mismatch: $1=$left $2=$right"
}
